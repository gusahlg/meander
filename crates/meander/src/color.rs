//! Straight (non-premultiplied) sRGB colours.

/// 8-bit sRGB colour with straight alpha.
///
/// Conversions to `tiny_skia::Color` premultiply for you. Pixel buffers handed
/// to Wayland are premultiplied ARGB8888 — meander handles the conversion when
/// the canvas is flushed to the surface buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);
    pub const BLACK: Self = Self::rgb(0, 0, 0);
    pub const WHITE: Self = Self::rgb(255, 255, 255);
    pub const RED: Self = Self::rgb(255, 0, 0);
    pub const GREEN: Self = Self::rgb(0, 255, 0);
    pub const BLUE: Self = Self::rgb(0, 0, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// 0xRRGGBBAA hex literal (matches the gharialctl border-colour format).
    pub const fn hex(rgba: u32) -> Self {
        Self::rgba(
            ((rgba >> 24) & 0xff) as u8,
            ((rgba >> 16) & 0xff) as u8,
            ((rgba >> 8) & 0xff) as u8,
            (rgba & 0xff) as u8,
        )
    }

    pub fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    pub(crate) fn to_tiny(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba8(self.r, self.g, self.b, self.a)
    }
}
