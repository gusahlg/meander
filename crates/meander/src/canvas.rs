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
use crate::font::Font;

pub struct Canvas<'a> {
    pub(crate) pixmap: PixmapMut<'a>,
    pub(crate) scale: i32,
}

impl<'a> Canvas<'a> {
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
    pub fn fill(&mut self, color: Color) {
        self.pixmap.fill(color.to_tiny());
    }

    /// Fill an axis-aligned rectangle. Coordinates are in physical pixels.
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        let Some(r) = Rect::from_xywh(x, y, w, h) else { return };
        let mut paint = Paint::default();
        paint.set_color(color.to_tiny());
        paint.anti_alias = true;
        self.pixmap.fill_rect(r, &paint, Transform::identity(), None);
    }

    /// Stroke the outline of an axis-aligned rectangle.
    pub fn stroke_rect(&mut self, x: f32, y: f32, w: f32, h: f32, line_width: f32, color: Color) {
        let Some(r) = Rect::from_xywh(x, y, w, h) else { return };
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
        self.pixmap
            .fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
    }

    /// Draw a left-to-right line of text on `baseline`.
    ///
    /// Returns the x coordinate where the pen ended up (use it to chain runs
    /// at different colours).
    pub fn text(
        &mut self,
        text: &str,
        x: f32,
        baseline: f32,
        size_px: f32,
        color: Color,
        font: &Font,
    ) -> f32 {
        let raw = font.raw();
        let mut pen = x;
        let w = self.width() as i32;
        let h = self.height() as i32;
        for ch in text.chars() {
            let (metrics, bitmap) = raw.rasterize(ch, size_px);
            for row in 0..metrics.height {
                let py = baseline as i32 - (metrics.height as i32 + metrics.ymin) + row as i32;
                if py < 0 || py >= h {
                    continue;
                }
                for col in 0..metrics.width {
                    let coverage = bitmap[row * metrics.width + col];
                    if coverage == 0 {
                        continue;
                    }
                    let px = pen as i32 + metrics.xmin + col as i32;
                    if px < 0 || px >= w {
                        continue;
                    }
                    let a = (color.a as u32 * coverage as u32) / 255;
                    if a == 0 {
                        continue;
                    }
                    let src_r = ((color.r as u32 * a) / 255) as u8;
                    let src_g = ((color.g as u32 * a) / 255) as u8;
                    let src_b = ((color.b as u32 * a) / 255) as u8;
                    let src_a = a as u8;
                    let inv = 255u32 - a;
                    let off = (py as usize * w as usize + px as usize) * 4;
                    let data = self.pixmap.data_mut();
                    data[off] = src_r + ((data[off] as u32 * inv) / 255) as u8;
                    data[off + 1] = src_g + ((data[off + 1] as u32 * inv) / 255) as u8;
                    data[off + 2] = src_b + ((data[off + 2] as u32 * inv) / 255) as u8;
                    data[off + 3] = src_a + ((data[off + 3] as u32 * inv) / 255) as u8;
                }
            }
            pen += metrics.advance_width;
        }
        pen
    }

    /// Measure the advance width of `text` at `size_px`, in physical pixels.
    pub fn text_width(&self, text: &str, size_px: f32, font: &Font) -> f32 {
        let raw = font.raw();
        text.chars()
            .map(|ch| raw.metrics(ch, size_px).advance_width)
            .sum()
    }
}
