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

    /// 0xRRGGBB hex literal; alpha is implicit-opaque. Sugar over [`Color::hex`]
    /// for the common case where you don't need a transparent colour.
    pub const fn hex_rgb(rgb: u32) -> Self {
        Self::rgb(
            ((rgb >> 16) & 0xff) as u8,
            ((rgb >> 8) & 0xff) as u8,
            (rgb & 0xff) as u8,
        )
    }

    pub fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    pub(crate) fn to_tiny(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba8(self.r, self.g, self.b, self.a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_unpacks_in_rrggbbaa_order() {
        assert_eq!(
            Color::hex(0x11_22_33_44),
            Color::rgba(0x11, 0x22, 0x33, 0x44)
        );
    }

    #[test]
    fn hex_handles_alpha_zero_and_full() {
        assert_eq!(Color::hex(0xFFFFFFFF), Color::rgba(255, 255, 255, 255));
        assert_eq!(Color::hex(0x000000FF), Color::rgba(0, 0, 0, 255));
        assert_eq!(Color::hex(0xFFFFFF00), Color::rgba(255, 255, 255, 0));
    }

    #[test]
    fn rgb_defaults_alpha_to_opaque() {
        assert_eq!(Color::rgb(10, 20, 30).a, 255);
    }

    #[test]
    fn with_alpha_only_changes_alpha() {
        let c = Color::rgba(1, 2, 3, 4).with_alpha(99);
        assert_eq!(c, Color::rgba(1, 2, 3, 99));
    }

    #[test]
    fn named_constants_match_components() {
        assert_eq!(Color::BLACK, Color::rgb(0, 0, 0));
        assert_eq!(Color::WHITE, Color::rgb(255, 255, 255));
        assert_eq!(Color::TRANSPARENT.a, 0);
    }

    #[test]
    fn hex_rgb_implies_opaque_alpha() {
        assert_eq!(
            Color::hex_rgb(0x102030),
            Color::rgba(0x10, 0x20, 0x30, 0xff)
        );
    }

    #[test]
    fn hex_rgb_ignores_high_byte() {
        // Anything in the top 8 bits of the input must not leak into the
        // colour — hex_rgb takes 0xRRGGBB only.
        assert_eq!(Color::hex_rgb(0xFF_00_FF_FF), Color::hex_rgb(0x00_FF_FF));
    }

    #[test]
    fn hex_and_hex_rgb_agree_on_opaque_colours() {
        assert_eq!(Color::hex_rgb(0xc8324b), Color::hex(0xc8324bff));
    }
}
