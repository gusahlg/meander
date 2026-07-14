//! A minimal river/gharial status bar built on meander.
//!
//! Run after starting gharial:
//!
//!     meander-bar [PATH_TO_FONT.ttf]
//!
//! If no font path is given, the bar reads `$MEANDER_BAR_FONT` and falls back
//! to `/usr/share/fonts/TTF/DejaVuSans.ttf`. Adjust to taste — meander does
//! not embed a default font.
//!
//! The loop is change-driven: it blocks in `poll` on both the Wayland
//! connection fd and the status poller's notification fd, so it only wakes when
//! the compositor has an event or gharial's state actually moved — not on a
//! fixed timer.

use std::os::fd::BorrowedFd;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use meander::{
    Anchor, App, Color, Event, Font, Layer, PointerButton, PointerEvent, PointerEventKind,
};
use meander_gharial::{Gharial, Status, StatusPoller};
use rustix::event::{poll, PollFd, PollFlags, Timespec};

const BAR_HEIGHT: u32 = 28;
const FONT_SIZE: f32 = 14.0;
const PAD: f32 = 8.0;
const TAG_BOX: f32 = 22.0;

const BG: Color = Color::hex_rgb(0x14141C);
const FG: Color = Color::hex_rgb(0xCFD2E0);
const FG_DIM: Color = Color::hex_rgb(0x5F6478);
const ACCENT: Color = Color::hex_rgb(0xC8324B);
const OCCUPIED: Color = Color::hex_rgb(0x00C896);
const MODE_BG: Color = Color::hex_rgb(0x2A1C2E);

/// A tag box's clickable rectangle, in physical pixels (matching the pointer
/// coordinates meander delivers). Recorded while drawing so click handling uses
/// the exact geometry that was painted, at the right scale.
#[derive(Clone, Copy)]
struct TagHit {
    tag: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl TagHit {
    fn contains(&self, px: f64, py: f64) -> bool {
        let (px, py) = (px as f32, py as f32);
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("meander-bar: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let font_path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("MEANDER_BAR_FONT").ok())
        .unwrap_or_else(|| "/usr/share/fonts/TTF/DejaVuSans.ttf".to_string());
    let font = Font::from_file(&font_path).map_err(|e| {
        format!("could not load font {font_path}: {e} (set MEANDER_BAR_FONT or pass a path)")
    })?;

    let mut app = App::connect()?;
    let bar = app
        .layer_surface()
        .namespace("meander.bar")
        .layer(Layer::Top)
        .anchor(Anchor::TOP_STRIP)
        .size(0, BAR_HEIGHT)
        .exclusive_zone(BAR_HEIGHT as i32)
        .build()?;

    let gharial = Gharial::connect()
        .map_err(|e| format!("could not reach gharial: {e} (is the daemon running?)"))?;
    // `try_start_polling` surfaces a bad interval or an OS failure instead of
    // panicking deep in a thread.
    let poller: StatusPoller = gharial.try_start_polling(Duration::from_millis(100))?;

    let mut needs_draw = true;
    // Retain the shared snapshot Arc rather than cloning a fresh Status each
    // frame; only re-read when the poller's revision advances.
    let mut status: Arc<Status> = Arc::new(Status::default());
    let mut last_revision = 0u64;
    let mut last_poll_error: Option<String> = None;
    // Hit rectangles from the most recent frame, used by click handling.
    let mut hit_rects: Vec<TagHit> = Vec::new();

    loop {
        while let Some(ev) = app.next_event() {
            match ev {
                Event::Configure { surface, .. } if surface == bar => needs_draw = true,
                Event::Frame { surface, .. } if surface == bar => needs_draw = true,
                Event::Closed { surface, .. } if surface == bar => return Ok(()),
                Event::GlobalLost(iface) => {
                    return Err(format!("compositor withdrew {iface}; exiting").into());
                }
                Event::Pointer(pe) => {
                    if let Some(tag) = tag_under_pointer(&pe, &hit_rects) {
                        // Report the click command's error instead of dropping it.
                        if let Err(e) = gharial.tag_focus(tag) {
                            eprintln!("meander-bar: tag focus {tag} failed: {e}");
                        }
                    }
                }
                _ => {}
            }
        }

        // Pull the poller's latest coherent state; only react when it changed.
        let snap = poller.poll();
        if snap.revision != last_revision {
            last_revision = snap.revision;
            if let Some(s) = snap.status {
                if *s != *status {
                    status = s;
                    needs_draw = true;
                }
            }
            // Surface poller errors (daemon gone, malformed status, ...) once.
            if snap.last_error != last_poll_error {
                if let Some(err) = &snap.last_error {
                    eprintln!("meander-bar: status poll error: {err}");
                }
                last_poll_error = snap.last_error;
            }
        }

        if needs_draw {
            let mut new_hits = Vec::new();
            match app
                .surface(bar)
                .draw(|c| draw_bar(c, &font, &status, &mut new_hits))
            {
                Ok(()) => {
                    hit_rects = new_hits;
                    needs_draw = false;
                }
                // Both buffers still owned by the compositor; retry on the next
                // Frame / buffer Release.
                Err(meander::Error::BuffersBusy) => {}
                Err(e) => return Err(e.into()),
            }
        }

        app.flush()?;
        // Block until either fd is ready (or a heartbeat), then let meander read
        // and dispatch whatever arrived on the Wayland socket, non-blocking.
        wait_for_activity(&app, &poller)?;
        app.dispatch(Some(Duration::ZERO))?;
    }
}

/// Block until the Wayland connection or the poller's notification fd is ready,
/// or a one-second heartbeat elapses so a missed edge can't wedge the bar.
fn wait_for_activity(app: &App, poller: &StatusPoller) -> Result<(), Box<dyn std::error::Error>> {
    let wl_fd = app.connection_fd();
    let poll_fd = poller.notify_fd();
    // SAFETY: both fds are owned by `app` / `poller` for the duration of this
    // borrow; we only poll them and never close them.
    let wl = unsafe { BorrowedFd::borrow_raw(wl_fd) };
    let pl = unsafe { BorrowedFd::borrow_raw(poll_fd) };
    let mut fds = [
        PollFd::new(&wl, PollFlags::IN),
        PollFd::new(&pl, PollFlags::IN),
    ];
    let timeout = Timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    match poll(&mut fds, Some(&timeout)) {
        Ok(_) | Err(rustix::io::Errno::INTR) => {}
        Err(e) => return Err(e.into()),
    }
    if fds[1].revents().contains(PollFlags::IN) {
        // Drain the eventfd so it doesn't immediately re-fire.
        poller.drain();
    }
    Ok(())
}

fn draw_bar(c: &mut meander::Canvas<'_>, font: &Font, s: &Status, hits: &mut Vec<TagHit>) {
    let w = c.width() as f32;
    let h = c.height() as f32;
    let scale = c.scale() as f32;
    let size = FONT_SIZE * scale;
    // Centre text vertically: top edge sits at (bar height − cap height) / 2,
    // using the font's ascent as a good-enough cap-height proxy.
    let (ascent, _, _) = font.metrics(size);
    let text_top = ((h - ascent) / 2.0).max(0.0);

    c.fill(BG);

    // Mode pill (left).
    let mode_text = format!(" {} ", s.mode);
    let mw = c.text_width(&mode_text, size, font);
    c.rounded_rect(
        PAD * scale,
        4.0 * scale,
        mw,
        h - 8.0 * scale,
        4.0 * scale,
        MODE_BG,
    );
    let mode_color = if s.mode == "default" { FG } else { ACCENT };
    c.text_top(&mode_text, PAD * scale, text_top, size, mode_color, font);

    // Tag row (after the mode pill).
    let mut x = PAD * scale + mw + PAD * scale;
    let box_w = TAG_BOX * scale;
    let box_h = h - 8.0 * scale;
    let y = 4.0 * scale;
    for n in 1u32..=9 {
        let active = s.tag_active(n);
        let occupied = s.tag_occupied(n);
        if active {
            c.rounded_rect(x, y, box_w, box_h, 3.0 * scale, ACCENT.with_alpha(0xAA));
        } else if occupied {
            c.stroke_rect(x + 1.0, y + 1.0, box_w - 2.0, box_h - 2.0, scale, OCCUPIED);
        }
        let label = n.to_string();
        let lw = c.text_width(&label, size, font);
        let lc = if active {
            FG
        } else if occupied {
            OCCUPIED
        } else {
            FG_DIM
        };
        c.text_top(&label, x + (box_w - lw) / 2.0, text_top, size, lc, font);
        // Record the exact painted rectangle (physical pixels) for click hits.
        hits.push(TagHit {
            tag: n,
            x,
            y,
            w: box_w,
            h: box_h,
        });
        x += box_w + 2.0 * scale;
    }

    // Right-aligned: layout summary.
    let summary = format!(
        "ratio {:.2}  gaps {}  border {}",
        s.main_ratio.unwrap_or(0.0),
        s.gaps.unwrap_or(0),
        s.border_width.unwrap_or(0),
    );
    let sw = c.text_width(&summary, size, font);
    c.text_top(&summary, w - sw - PAD * scale, text_top, size, FG_DIM, font);
}

/// Map a left-button press to the tag whose recorded rectangle it landed in.
fn tag_under_pointer(p: &PointerEvent, hits: &[TagHit]) -> Option<u32> {
    if let PointerEventKind::Press(PointerButton::Left) = p.kind {
        hits.iter()
            .find(|hit| hit.contains(p.x, p.y))
            .map(|hit| hit.tag)
    } else {
        None
    }
}
