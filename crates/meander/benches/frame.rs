//! Small, dependency-free rendering benchmarks.
//!
//! Run with `cargo bench -p meander`. This is a hand-rolled harness (no
//! criterion) so it builds on stable with no extra dependencies; it prints
//! nanoseconds-per-iteration for each case. Keep the before/after numbers in a
//! PR description when touching the renderer — per the improvement plan, do not
//! add SIMD or a new container unless a representative case here improves
//! materially.
//!
//! Cases mirror the plan's list: canvas primitives, text, buffer clear, the
//! RGBA→BGRA swap, a complete example-bar frame, and hot-cache text
//! measurement + drawing. Each runs at 1920×28 scale-1 and scale-2.
//!
//! Text cases need a font; set `$MEANDER_BAR_FONT` (or install DejaVuSans at the
//! default path) to include them, otherwise they are skipped with a note.

use std::hint::black_box;
use std::time::Instant;

use meander::{Canvas, Color, Font};
use tiny_skia::Pixmap;

/// One buffer configuration to benchmark.
struct Config {
    label: &'static str,
    logical_w: u32,
    logical_h: u32,
    scale: i32,
}

impl Config {
    fn phys_w(&self) -> u32 {
        self.logical_w * self.scale as u32
    }
    fn phys_h(&self) -> u32 {
        self.logical_h * self.scale as u32
    }
    fn pixmap(&self) -> Pixmap {
        Pixmap::new(self.phys_w(), self.phys_h()).expect("pixmap")
    }
}

/// Time `f` over `iters` iterations after a short warmup; print ns/iter.
fn bench(name: &str, iters: u32, mut f: impl FnMut()) {
    for _ in 0..(iters / 10).max(1) {
        f();
    }
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let elapsed = start.elapsed();
    let per = elapsed.as_nanos() as f64 / iters as f64;
    println!("  {name:<34} {per:>12.1} ns/iter  ({iters} iters)");
}

fn find_font() -> Option<Font> {
    let candidates = [
        std::env::var("MEANDER_BAR_FONT").ok(),
        Some("/usr/share/fonts/TTF/DejaVuSans.ttf".into()),
        Some("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".into()),
        Some("/usr/share/fonts/dejavu/DejaVuSans.ttf".into()),
    ];
    candidates
        .into_iter()
        .flatten()
        .find_map(|p| Font::from_file(&p).ok())
}

/// Reference RGBA→BGRA swap (the operation ShmPool performs before commit).
fn rgba_to_bgra(buf: &mut [u8]) {
    for chunk in buf.chunks_exact_mut(4) {
        chunk.swap(0, 2);
    }
}

fn bench_config(cfg: &Config, font: Option<&Font>) {
    println!(
        "{} ({}x{} scale {}, physical {}x{}):",
        cfg.label,
        cfg.logical_w,
        cfg.logical_h,
        cfg.scale,
        cfg.phys_w(),
        cfg.phys_h()
    );
    let scale = cfg.scale;
    let fg = Color::hex_rgb(0xCFD2E0);
    let bg = Color::hex_rgb(0x14141C);

    // Buffer clear (transparent).
    {
        let mut pm = cfg.pixmap();
        bench("buffer clear (fill 0)", 2000, || {
            let data = pm.data_mut();
            data.fill(0);
            black_box(data);
        });
    }

    // RGBA -> BGRA swap over the whole buffer.
    {
        let mut buf = vec![0u8; (cfg.phys_w() * cfg.phys_h() * 4) as usize];
        bench("rgba->bgra swap", 2000, || {
            rgba_to_bgra(&mut buf);
            black_box(&buf);
        });
    }

    // Full-buffer fill primitive.
    {
        let mut pm = cfg.pixmap();
        bench("canvas fill", 2000, || {
            let mut c = Canvas::new(pm.as_mut(), scale);
            c.fill(bg);
            black_box(pm.data());
        });
    }

    // A batch of rectangles + rounded rects + a line (the non-text primitives).
    {
        let mut pm = cfg.pixmap();
        bench("primitives (rect/rounded/line)", 2000, || {
            let mut c = Canvas::new(pm.as_mut(), scale);
            c.fill(bg);
            for i in 0..9 {
                let x = 40.0 + i as f32 * 24.0 * scale as f32;
                c.rounded_rect(x, 4.0, 22.0 * scale as f32, 20.0 * scale as f32, 3.0, fg);
                c.stroke_rect(x, 4.0, 22.0 * scale as f32, 20.0 * scale as f32, 1.0, fg);
            }
            c.line(0.0, 1.0, cfg.phys_w() as f32, 1.0, 1.0, fg);
        });
    }

    if let Some(font) = font {
        let size = 14.0 * scale as f32;
        let sample = "meander  ratio 0.55  gaps 8  border 2";

        // Cold-ish text: cache warmed by warmup, measures layout+blit.
        {
            let mut pm = cfg.pixmap();
            bench("text (warm cache)", 2000, || {
                let mut c = Canvas::new(pm.as_mut(), scale);
                c.text(sample, 8.0, 20.0, size, fg, font);
            });
        }

        // Hot-cache measure-then-draw of the same run (prepare once, reuse).
        {
            let mut pm = cfg.pixmap();
            bench("text_width + text (hot)", 4000, || {
                let run = font.prepare(sample, size);
                let _w = run.width();
                let mut c = Canvas::new(pm.as_mut(), scale);
                c.draw_text_run(&run, 8.0, 20.0, fg);
            });
        }

        // A complete example-bar-style frame.
        {
            let mut pm = cfg.pixmap();
            bench("full bar frame", 1000, || {
                let mut c = Canvas::new(pm.as_mut(), scale);
                draw_bar_frame(&mut c, font, size);
            });
        }
    } else {
        println!("  (text cases skipped: no font — set MEANDER_BAR_FONT)");
    }
    println!();
}

/// A representative bar frame: background, mode pill, nine tag boxes with
/// labels, and a right-aligned summary — the shape the example draws.
fn draw_bar_frame(c: &mut Canvas<'_>, font: &Font, size: f32) {
    let w = c.width() as f32;
    let h = c.height() as f32;
    let bg = Color::hex_rgb(0x14141C);
    let fg = Color::hex_rgb(0xCFD2E0);
    let accent = Color::hex_rgb(0xC8324B);
    c.fill(bg);
    c.rounded_rect(8.0, 4.0, 60.0, h - 8.0, 4.0, accent);
    c.text(" default ", 8.0, h - 8.0, size, fg, font);
    let mut x = 90.0;
    for n in 1..=9 {
        c.rounded_rect(x, 4.0, 22.0, h - 8.0, 3.0, accent.with_alpha(0xAA));
        c.text(&n.to_string(), x + 8.0, h - 8.0, size, fg, font);
        x += 24.0;
    }
    let summary = "ratio 0.55  gaps 8  border 2";
    let sw = c.text_width(summary, size, font);
    c.text(summary, w - sw - 8.0, h - 8.0, size, fg, font);
}

fn main() {
    println!("meander rendering benchmarks\n");
    let font = find_font();
    if font.is_none() {
        println!("note: no font found; text cases will be skipped.\n");
    }
    let configs = [
        Config {
            label: "bar scale-1",
            logical_w: 1920,
            logical_h: 28,
            scale: 1,
        },
        Config {
            label: "bar scale-2",
            logical_w: 1920,
            logical_h: 28,
            scale: 2,
        },
    ];
    for cfg in &configs {
        bench_config(cfg, font.as_ref());
    }
}
