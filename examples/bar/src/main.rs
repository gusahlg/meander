//! A minimal river/gharial status bar built on meander.
//!
//! Run after starting gharial:
//!
//!     meander-bar [PATH_TO_FONT.ttf]
//!
//! If no font path is given, the bar reads `$MEANDER_BAR_FONT` and falls back
//! to `/usr/share/fonts/TTF/DejaVuSans.ttf`. Adjust to taste — meander does
//! not embed a default font.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use meander::{
    Anchor, App, Color, Event, Font, Layer, PointerEvent, PointerEventKind, PointerButton,
};
use meander_gharial::{Gharial, Status, StatusPoller};

const BAR_HEIGHT: u32 = 28;
const FONT_SIZE: f32 = 14.0;
const PAD: f32 = 8.0;
const TAG_BOX: f32 = 22.0;

const BG: Color = Color::hex(0x14141Cff);
const FG: Color = Color::hex(0xCFD2E0ff);
const FG_DIM: Color = Color::hex(0x5F6478ff);
const ACCENT: Color = Color::hex(0xC8324Bff);
const OCCUPIED: Color = Color::hex(0x00C896ff);
const MODE_BG: Color = Color::hex(0x2A1C2Eff);

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
    let font_path = std::env::args().nth(1).or_else(|| std::env::var("MEANDER_BAR_FONT").ok())
        .unwrap_or_else(|| "/usr/share/fonts/TTF/DejaVuSans.ttf".to_string());
    let font = Font::from_file(&font_path).map_err(|e| {
        format!("could not load font {font_path}: {e} (set MEANDER_BAR_FONT or pass a path)")
    })?;

    let mut app = App::connect()?;
    let bar = app
        .layer_surface()
        .namespace("meander.bar")
        .layer(Layer::Top)
        .anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT)
        .size(0, BAR_HEIGHT)
        .exclusive_zone(BAR_HEIGHT as i32)
        .build()?;

    let gharial = Gharial::connect()
        .map_err(|e| format!("could not reach gharial: {e} (is the daemon running?)"))?;
    let poller: StatusPoller = gharial.start_polling(Duration::from_millis(100));

    let mut needs_draw = true;
    let mut last_status: Status = Status::default();
    let mut last_redraw = Instant::now();

    loop {
        while let Some(ev) = app.next_event() {
            match ev {
                Event::Configure { surface, .. } if surface == bar => {
                    needs_draw = true;
                }
                Event::Frame { surface, .. } if surface == bar => {
                    needs_draw = true;
                }
                Event::Closed { surface, .. } if surface == bar => return Ok(()),
                Event::Pointer(pe) => handle_pointer(&pe, &gharial),
                _ => {}
            }
        }

        // Redraw if either the compositor asked us or gharial state moved.
        let new_status = poller.latest_or_default();
        if new_status != last_status {
            last_status = new_status.clone();
            needs_draw = true;
        }

        if needs_draw {
            let _ = app.surface(bar).draw(|c| draw_bar(c, &font, &last_status));
            needs_draw = false;
            last_redraw = Instant::now();
        }

        app.flush()?;
        // Wake up on either Wayland events or roughly every 100ms to recheck
        // the poller's snapshot. (Real apps that want pixel-perfect timing
        // would integrate the poller's fd; this is fine for a bar.)
        app.dispatch(Some(Duration::from_millis(100)))?;
        let _ = last_redraw;
    }
}

fn draw_bar(c: &mut meander::Canvas<'_>, font: &Font, s: &Status) {
    let w = c.width() as f32;
    let h = c.height() as f32;
    let scale = c.scale() as f32;

    c.fill(BG);

    // Mode pill (left).
    let mode_text = format!(" {} ", s.mode);
    let mw = c.text_width(&mode_text, FONT_SIZE * scale, font);
    c.rounded_rect(PAD * scale, 4.0 * scale, mw, h - 8.0 * scale, 4.0 * scale, MODE_BG);
    let mode_color = if s.mode == "default" { FG } else { ACCENT };
    c.text(
        &mode_text,
        PAD * scale,
        (h + FONT_SIZE * scale) / 2.0 - 2.0 * scale,
        FONT_SIZE * scale,
        mode_color,
        font,
    );

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
            c.stroke_rect(x + 1.0, y + 1.0, box_w - 2.0, box_h - 2.0, 1.0 * scale, OCCUPIED);
        }
        let label = n.to_string();
        let lw = c.text_width(&label, FONT_SIZE * scale, font);
        let lc = if active { FG } else if occupied { OCCUPIED } else { FG_DIM };
        c.text(
            &label,
            x + (box_w - lw) / 2.0,
            (h + FONT_SIZE * scale) / 2.0 - 2.0 * scale,
            FONT_SIZE * scale,
            lc,
            font,
        );
        x += box_w + 2.0 * scale;
    }

    // Right-aligned: layout summary.
    let summary = format!(
        "ratio {:.2}  gaps {}  border {}",
        s.main_ratio.unwrap_or(0.0),
        s.gaps.unwrap_or(0),
        s.border_width.unwrap_or(0),
    );
    let sw = c.text_width(&summary, FONT_SIZE * scale, font);
    c.text(
        &summary,
        w - sw - PAD * scale,
        (h + FONT_SIZE * scale) / 2.0 - 2.0 * scale,
        FONT_SIZE * scale,
        FG_DIM,
        font,
    );
}

fn handle_pointer(p: &PointerEvent, g: &Gharial) {
    if let PointerEventKind::Press(PointerButton::Left) = p.kind {
        // Click in the tag row triggers `tag focus N`.
        // Rough geometry: skip the mode pill, then 9 boxes at TAG_BOX spacing.
        // We don't have the canvas scale here, so we approximate with the
        // physical box width — good enough for a v0 example.
        // A real bar would remember per-tag hit rects from the last draw.
        let tag_row_start = 80.0;
        let box_w = 24.0;
        if p.x >= tag_row_start {
            let idx = ((p.x - tag_row_start) / box_w) as u32 + 1;
            if (1..=9).contains(&idx) {
                let _ = g.tag_focus(idx);
            }
        }
    }
}
