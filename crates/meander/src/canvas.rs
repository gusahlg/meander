//! 2D drawing primitives backed by [`tiny_skia`] plus a glyph rasteriser.
//!
//! A `Canvas` is a borrowed view of one shm buffer for one surface, valid only
//! for the duration of the closure handed to [`SurfaceHandle::draw`]. Pixels
//! are stored as premultiplied RGBA; the surface flush swaps R and B to land
//! them as the `Argb8888` byte order Wayland expects.
//!
//! [`SurfaceHandle::draw`]: crate::SurfaceHandle::draw

use tiny_skia::{Paint, PathBuilder, PixmapMut, Rect, Stroke, Transform};

use crate::color::Color;
use crate::font::{Font, TextRun};

pub struct Canvas<'a> {
    pub(crate) pixmap: PixmapMut<'a>,
    pub(crate) scale: i32,
    /// When true, the backing buffer may hold stale pixels and must be cleared
    /// to transparent before the first *partial* draw. A full [`fill`](Self::fill)
    /// clears the flag without a separate clear pass, so a frame that starts by
    /// filling writes each pixel once instead of twice. Off-screen canvases
    /// created with [`Canvas::new`] start `false` — they draw onto exactly the
    /// pixels you handed in.
    needs_clear: bool,
}

impl<'a> Canvas<'a> {
    /// Wrap a borrowed [`PixmapMut`] as a `Canvas` at the given buffer scale.
    ///
    /// meander uses this internally to hand you a canvas over a surface's shm
    /// buffer, but it is also the entry point for off-screen rendering: build a
    /// `tiny_skia::Pixmap`, take `pixmap.as_mut()`, draw, then do what you like
    /// with the pixels. `scale` is informational (see [`Canvas::scale`]); pass
    /// `1` if you are not rendering for a HiDPI surface.
    ///
    /// The canvas draws directly onto the pixels you provide with no implicit
    /// clear — the buffer starts as whatever you passed in.
    pub fn new(pixmap: PixmapMut<'a>, scale: i32) -> Self {
        Self {
            pixmap,
            scale,
            needs_clear: false,
        }
    }

    /// Internal constructor for the surface draw path, where the shm buffer is
    /// reused and may hold the previous frame. Defers the transparent clear
    /// until the first partial draw (see [`Canvas::needs_clear`]).
    pub(crate) fn new_surface(pixmap: PixmapMut<'a>, scale: i32) -> Self {
        Self {
            pixmap,
            scale,
            needs_clear: true,
        }
    }

    /// Clear to transparent if a deferred clear is still pending. Called by
    /// every partial primitive before it draws, and once after the user's draw
    /// closure so an empty frame still starts transparent.
    pub(crate) fn ensure_cleared(&mut self) {
        if self.needs_clear {
            self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
            self.needs_clear = false;
        }
    }

    /// Width of the underlying buffer, in physical pixels.
    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    /// Height of the underlying buffer, in physical pixels.
    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }

    /// Buffer scale (1 on a normal display, 2 on a HiDPI one). The buffer is
    /// `logical_size * scale` physical pixels. You can multiply your logical
    /// coordinates by this when you want sharp output.
    pub fn scale(&self) -> i32 {
        self.scale
    }

    /// Fill the entire buffer with one colour, replacing whatever was there.
    ///
    /// Because this covers every pixel, it also satisfies the surface draw
    /// path's transparent-start guarantee without a separate clear pass: a
    /// frame that begins with `fill` writes each pixel once, not twice.
    pub fn fill(&mut self, color: Color) {
        self.pixmap.fill(color.to_tiny());
        self.needs_clear = false;
    }

    /// Fill an axis-aligned rectangle. Coordinates are in physical pixels.
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        self.ensure_cleared();
        let Some(r) = Rect::from_xywh(x, y, w, h) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(color.to_tiny());
        paint.anti_alias = true;
        self.pixmap
            .fill_rect(r, &paint, Transform::identity(), None);
    }

    /// Stroke the outline of an axis-aligned rectangle.
    pub fn stroke_rect(&mut self, x: f32, y: f32, w: f32, h: f32, line_width: f32, color: Color) {
        self.ensure_cleared();
        let Some(r) = Rect::from_xywh(x, y, w, h) else {
            return;
        };
        let mut pb = PathBuilder::new();
        pb.push_rect(r);
        let Some(path) = pb.finish() else { return };
        let mut paint = Paint::default();
        paint.set_color(color.to_tiny());
        paint.anti_alias = true;
        let stroke = Stroke {
            width: line_width,
            ..Default::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    /// Filled rectangle with rounded corners. `radius` is clamped to half the
    /// shorter side.
    pub fn rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Color) {
        self.ensure_cleared();
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        if r == 0.0 {
            return self.rect(x, y, w, h, color);
        }
        let mut pb = PathBuilder::new();
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.quad_to(x + w, y, x + w, y + r);
        pb.line_to(x + w, y + h - r);
        pb.quad_to(x + w, y + h, x + w - r, y + h);
        pb.line_to(x + r, y + h);
        pb.quad_to(x, y + h, x, y + h - r);
        pb.line_to(x, y + r);
        pb.quad_to(x, y, x + r, y);
        pb.close();
        let Some(path) = pb.finish() else { return };
        let mut paint = Paint::default();
        paint.set_color(color.to_tiny());
        paint.anti_alias = true;
        self.pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    /// Draw a left-to-right line of text on `baseline`.
    ///
    /// Returns the x coordinate where the pen ended up (use it to chain runs
    /// at different colours). This lays the run out and rasterises it under a
    /// single cache lock; if you also need the width, or you draw the same
    /// string every frame, [`Font::prepare`] once and reuse the [`TextRun`]
    /// with [`Canvas::draw_text_run`] instead.
    pub fn text(
        &mut self,
        text: &str,
        x: f32,
        baseline: f32,
        size_px: f32,
        color: Color,
        font: &Font,
    ) -> f32 {
        let run = font.prepare(text, size_px);
        self.draw_text_run(&run, x, baseline, color)
    }

    /// Draw a previously [`prepare`](Font::prepare)d [`TextRun`] with its origin
    /// pen at `x` on `baseline`. Returns the x coordinate where the pen ended
    /// up. No glyph-cache lookups happen here — the run already holds its
    /// rasterised glyphs.
    pub fn draw_text_run(&mut self, run: &TextRun, x: f32, baseline: f32, color: Color) -> f32 {
        self.ensure_cleared();
        let w = self.width() as i32;
        let h = self.height() as i32;
        let stride = w as usize * 4;
        let data = self.pixmap.data_mut();
        for (entry, pen) in run.glyphs() {
            let metrics = entry.metrics;
            let bitmap = &entry.bitmap;
            if metrics.width > 0 && metrics.height > 0 {
                let top = baseline as i32 - (metrics.height as i32 + metrics.ymin);
                let left = (x + pen) as i32 + metrics.xmin;
                for row in 0..metrics.height {
                    let py = top + row as i32;
                    if py < 0 || py >= h {
                        continue;
                    }
                    let row_start = py as usize * stride;
                    let bitmap_row = row * metrics.width;
                    for col in 0..metrics.width {
                        let coverage = bitmap[bitmap_row + col];
                        if coverage == 0 {
                            continue;
                        }
                        let px = left + col as i32;
                        if px < 0 || px >= w {
                            continue;
                        }
                        let a = color.a as u32 * coverage as u32 / 255;
                        if a == 0 {
                            continue;
                        }
                        let inv = 255 - a;
                        let off = row_start + px as usize * 4;
                        blend_over(&mut data[off..off + 4], color, a, inv);
                    }
                }
            }
        }
        x + run.width()
    }

    /// Measure the advance width of `text` at `size_px`, in physical pixels.
    /// Metrics-only (no rasterisation) under a single cache lock.
    pub fn text_width(&self, text: &str, size_px: f32, font: &Font) -> f32 {
        font.measure(text, size_px)
    }

    /// Draw a left-to-right line of text whose **top edge** sits at `y_top`.
    ///
    /// This is sugar over [`Canvas::text`] for callers who think in CSS-style
    /// top-left coordinates instead of typographic baselines. Internally we
    /// just offset by the font ascent at this size.
    ///
    /// Returns the x coordinate where the pen ended up.
    pub fn text_top(
        &mut self,
        text: &str,
        x: f32,
        y_top: f32,
        size_px: f32,
        color: Color,
        font: &Font,
    ) -> f32 {
        let (ascent, _, _) = font.metrics(size_px);
        self.text(text, x, y_top + ascent, size_px, color, font)
    }

    /// Draw a 1px-or-wider line between two points.
    ///
    /// Endpoints are in physical pixels; the line is anti-aliased.
    pub fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: Color) {
        self.ensure_cleared();
        if width <= 0.0 {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(x1, y1);
        pb.line_to(x2, y2);
        let Some(path) = pb.finish() else { return };
        let mut paint = Paint::default();
        paint.set_color(color.to_tiny());
        paint.anti_alias = true;
        let stroke = Stroke {
            width,
            ..Default::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    /// Write one premultiplied pixel. Bounds-checked; out-of-range calls are
    /// silently dropped, matching the canvas-as-clipped-window contract of
    /// the other primitives.
    pub fn set_pixel(&mut self, x: i32, y: i32, color: Color) {
        self.ensure_cleared();
        let w = self.width() as i32;
        let h = self.height() as i32;
        if x < 0 || y < 0 || x >= w || y >= h {
            return;
        }
        let off = (y as usize * w as usize + x as usize) * 4;
        // Saturating "over" composite with `a = color.a` so the call has the
        // same blending semantics as text() and rect() at the edges.
        let a = color.a as u32;
        let inv = 255 - a;
        let data = self.pixmap.data_mut();
        blend_over(&mut data[off..off + 4], color, a, inv);
    }
}

/// Premultiplied "over" composite of one source pixel onto a 4-byte destination
/// slice in RGBA order. `a` is the effective source alpha (already multiplied
/// by the glyph coverage); `inv = 255 - a`. Saturating arithmetic guards
/// against the at-most-one-LSB overshoot integer rounding can produce.
#[inline]
fn blend_over(dst: &mut [u8], color: Color, a: u32, inv: u32) {
    let src_r = (color.r as u32 * a / 255) as u8;
    let src_g = (color.g as u32 * a / 255) as u8;
    let src_b = (color.b as u32 * a / 255) as u8;
    let src_a = a as u8;
    dst[0] = src_r.saturating_add((dst[0] as u32 * inv / 255) as u8);
    dst[1] = src_g.saturating_add((dst[1] as u32 * inv / 255) as u8);
    dst[2] = src_b.saturating_add((dst[2] as u32 * inv / 255) as u8);
    dst[3] = src_a.saturating_add((dst[3] as u32 * inv / 255) as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_over_with_zero_alpha_leaves_dst_unchanged() {
        let mut dst = [10, 20, 30, 40];
        blend_over(&mut dst, Color::rgba(255, 255, 255, 255), 0, 255);
        assert_eq!(dst, [10, 20, 30, 40]);
    }

    #[test]
    fn blend_over_with_full_alpha_overwrites_with_premultiplied_src() {
        let mut dst = [99, 99, 99, 99];
        blend_over(&mut dst, Color::rgba(10, 20, 30, 255), 255, 0);
        // a = 255 → src = color * 255 / 255 = color; dst contribution = 0.
        assert_eq!(dst, [10, 20, 30, 255]);
    }

    #[test]
    fn blend_over_half_alpha_mixes_correctly() {
        // a = 128 (close enough to 50%), pure red over pure blue.
        let mut dst = [0, 0, 255, 255]; // pre-multiplied opaque blue
        blend_over(&mut dst, Color::rgba(255, 0, 0, 255), 128, 127);
        // src_r = 255 * 128 / 255 = 128; r = 128 + 0 = 128
        // src_g = 0; g = 0 + 0 = 0
        // src_b = 0; b = 0 + 255 * 127 / 255 = 0 + 127 = 127
        // src_a = 128; a = 128 + 255 * 127 / 255 = 128 + 127 = 255
        assert_eq!(dst, [128, 0, 127, 255]);
    }

    #[test]
    fn blend_over_never_overflows_even_with_pathological_rounding() {
        // Construct a case where integer rounding would push the result one
        // past 255 in an un-saturating implementation. With a = 1 the source
        // contribution rounds down to 1 for any colour byte ≤ 254; the dst
        // contribution can also round up. Saturation must clamp at 255.
        let mut dst = [255, 255, 255, 255];
        blend_over(&mut dst, Color::rgba(255, 255, 255, 255), 1, 254);
        // src_X = 255 * 1 / 255 = 1; dst_X = 255 * 254 / 255 = 254
        // 1 + 254 = 255 — fits, but only just.
        assert_eq!(dst, [255, 255, 255, 255]);
    }

    #[test]
    fn blend_over_into_empty_buffer_paints_premultiplied() {
        let mut dst = [0, 0, 0, 0];
        blend_over(&mut dst, Color::rgba(200, 100, 50, 255), 200, 55);
        // src_r = 200 * 200 / 255 = 156
        // src_g = 100 * 200 / 255 = 78
        // src_b = 50 * 200 / 255 = 39
        // src_a = 200
        assert_eq!(dst, [156, 78, 39, 200]);
    }

    #[test]
    fn blend_over_is_idempotent_for_opaque_src_on_opaque_dst() {
        // Painting the same opaque colour twice should not drift.
        let mut dst = [40, 60, 80, 255];
        let color = Color::rgba(40, 60, 80, 255);
        blend_over(&mut dst, color, 255, 0);
        blend_over(&mut dst, color, 255, 0);
        assert_eq!(dst, [40, 60, 80, 255]);
    }

    // -----------------------------------------------------------------------
    // Integration-style tests using a real tiny-skia Pixmap.
    // -----------------------------------------------------------------------

    fn new_canvas(w: u32, h: u32) -> tiny_skia::Pixmap {
        tiny_skia::Pixmap::new(w, h).expect("pixmap")
    }

    fn pixel_at(pm: &tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
        let off = (y as usize * pm.width() as usize + x as usize) * 4;
        [
            pm.data()[off],
            pm.data()[off + 1],
            pm.data()[off + 2],
            pm.data()[off + 3],
        ]
    }

    #[test]
    fn fill_paints_every_pixel_with_the_given_colour() {
        let mut pm = new_canvas(3, 2);
        {
            let mut c = Canvas::new(pm.as_mut(), 1);
            c.fill(Color::rgba(10, 20, 30, 255));
        }
        for y in 0..pm.height() {
            for x in 0..pm.width() {
                assert_eq!(pixel_at(&pm, x, y), [10, 20, 30, 255]);
            }
        }
    }

    #[test]
    fn set_pixel_writes_premultiplied_at_target() {
        let mut pm = new_canvas(4, 4);
        {
            let mut c = Canvas::new(pm.as_mut(), 1);
            c.set_pixel(1, 2, Color::rgba(50, 100, 150, 255));
        }
        assert_eq!(pixel_at(&pm, 1, 2), [50, 100, 150, 255]);
        // Surrounding pixels stay zero.
        assert_eq!(pixel_at(&pm, 0, 2), [0, 0, 0, 0]);
        assert_eq!(pixel_at(&pm, 2, 2), [0, 0, 0, 0]);
        assert_eq!(pixel_at(&pm, 1, 1), [0, 0, 0, 0]);
        assert_eq!(pixel_at(&pm, 1, 3), [0, 0, 0, 0]);
    }

    #[test]
    fn set_pixel_clips_negative_and_out_of_bounds_coords() {
        let mut pm = new_canvas(4, 4);
        let untouched = pm.data().to_vec();
        {
            let mut c = Canvas::new(pm.as_mut(), 1);
            c.set_pixel(-1, 0, Color::WHITE);
            c.set_pixel(0, -1, Color::WHITE);
            c.set_pixel(4, 0, Color::WHITE);
            c.set_pixel(0, 4, Color::WHITE);
            c.set_pixel(100, 100, Color::WHITE);
        }
        assert_eq!(pm.data(), &untouched[..]);
    }

    #[test]
    fn rect_paints_inside_bounds_only() {
        let mut pm = new_canvas(8, 4);
        {
            let mut c = Canvas::new(pm.as_mut(), 1);
            c.rect(2.0, 1.0, 3.0, 2.0, Color::rgba(255, 0, 0, 255));
        }
        // Inside corners.
        assert_eq!(pixel_at(&pm, 2, 1), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&pm, 4, 2), [255, 0, 0, 255]);
        // Outside.
        assert_eq!(pixel_at(&pm, 1, 1), [0, 0, 0, 0]);
        assert_eq!(pixel_at(&pm, 5, 1), [0, 0, 0, 0]);
        assert_eq!(pixel_at(&pm, 2, 0), [0, 0, 0, 0]);
        assert_eq!(pixel_at(&pm, 2, 3), [0, 0, 0, 0]);
    }

    #[test]
    fn rect_with_non_positive_dimensions_is_a_no_op() {
        let mut pm = new_canvas(4, 4);
        let untouched = pm.data().to_vec();
        {
            let mut c = Canvas::new(pm.as_mut(), 1);
            c.rect(1.0, 1.0, 0.0, 2.0, Color::RED);
            c.rect(1.0, 1.0, 2.0, 0.0, Color::RED);
            c.rect(1.0, 1.0, -1.0, 2.0, Color::RED);
        }
        assert_eq!(pm.data(), &untouched[..]);
    }

    #[test]
    fn line_paints_at_least_one_pixel() {
        let mut pm = new_canvas(8, 8);
        {
            let mut c = Canvas::new(pm.as_mut(), 1);
            c.line(0.0, 4.0, 8.0, 4.0, 1.0, Color::rgba(255, 255, 255, 255));
        }
        let painted = pm.data().iter().any(|&b| b != 0);
        assert!(painted, "line should have written at least one pixel");
    }

    #[test]
    fn line_with_zero_width_is_a_no_op() {
        let mut pm = new_canvas(4, 4);
        let untouched = pm.data().to_vec();
        {
            let mut c = Canvas::new(pm.as_mut(), 1);
            c.line(0.0, 0.0, 4.0, 4.0, 0.0, Color::WHITE);
        }
        assert_eq!(pm.data(), &untouched[..]);
    }

    #[test]
    fn rounded_rect_with_zero_radius_matches_rect() {
        let mut pm_round = new_canvas(8, 8);
        let mut pm_rect = new_canvas(8, 8);
        {
            let mut c = Canvas::new(pm_round.as_mut(), 1);
            c.rounded_rect(1.0, 1.0, 6.0, 6.0, 0.0, Color::rgba(0, 200, 0, 255));
        }
        {
            let mut c = Canvas::new(pm_rect.as_mut(), 1);
            c.rect(1.0, 1.0, 6.0, 6.0, Color::rgba(0, 200, 0, 255));
        }
        assert_eq!(pm_round.data(), pm_rect.data());
    }

    #[test]
    fn canvas_reports_dimensions_and_scale() {
        let mut pm = new_canvas(5, 7);
        let c = Canvas::new(pm.as_mut(), 2);
        assert_eq!(c.width(), 5);
        assert_eq!(c.height(), 7);
        assert_eq!(c.scale(), 2);
    }

    // -----------------------------------------------------------------------
    // Lazy-clear semantics (surface draw path).
    // -----------------------------------------------------------------------

    /// A pixmap pre-filled with a "stale previous frame" colour.
    fn stale_canvas(w: u32, h: u32) -> tiny_skia::Pixmap {
        let mut pm = tiny_skia::Pixmap::new(w, h).unwrap();
        pm.fill(Color::rgba(9, 9, 9, 255).to_tiny());
        pm
    }

    #[test]
    fn surface_partial_draw_clears_stale_pixels_to_transparent() {
        let mut pm = stale_canvas(4, 4);
        {
            let mut c = Canvas::new_surface(pm.as_mut(), 1);
            c.rect(1.0, 1.0, 1.0, 1.0, Color::rgba(255, 0, 0, 255));
            c.ensure_cleared();
        }
        // The drawn pixel is red...
        assert_eq!(pixel_at(&pm, 1, 1), [255, 0, 0, 255]);
        // ...and the stale grey is gone everywhere else.
        assert_eq!(pixel_at(&pm, 0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel_at(&pm, 3, 3), [0, 0, 0, 0]);
    }

    #[test]
    fn surface_leading_fill_overwrites_stale_without_double_clearing() {
        let mut pm = stale_canvas(3, 2);
        {
            let mut c = Canvas::new_surface(pm.as_mut(), 1);
            c.fill(Color::rgba(10, 20, 30, 255));
            // A leading fill satisfies the clear; ensure_cleared is now a no-op.
            c.ensure_cleared();
        }
        for y in 0..pm.height() {
            for x in 0..pm.width() {
                assert_eq!(pixel_at(&pm, x, y), [10, 20, 30, 255]);
            }
        }
    }

    #[test]
    fn surface_empty_frame_clears_to_transparent() {
        let mut pm = stale_canvas(3, 3);
        {
            let mut c = Canvas::new_surface(pm.as_mut(), 1);
            // Draw nothing at all.
            c.ensure_cleared();
        }
        for y in 0..pm.height() {
            for x in 0..pm.width() {
                assert_eq!(pixel_at(&pm, x, y), [0, 0, 0, 0]);
            }
        }
    }

    #[test]
    fn off_screen_canvas_does_not_auto_clear() {
        // Canvas::new draws directly onto provided pixels; a partial draw must
        // leave the rest of the caller's buffer untouched.
        let mut pm = stale_canvas(3, 1);
        {
            let mut c = Canvas::new(pm.as_mut(), 1);
            c.rect(0.0, 0.0, 1.0, 1.0, Color::rgba(255, 0, 0, 255));
        }
        assert_eq!(pixel_at(&pm, 0, 0), [255, 0, 0, 255]);
        // Untouched pixels keep the caller's original content.
        assert_eq!(pixel_at(&pm, 2, 0), [9, 9, 9, 255]);
    }
}
