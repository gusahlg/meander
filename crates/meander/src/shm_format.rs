//! Pixel format selection for shm buffers.
//!
//! tiny-skia rasterises into premultiplied **RGBA** byte order. Wayland's
//! guaranteed-available `Argb8888` is, on a little-endian host, premultiplied
//! **BGRA** byte order — so meander has to swap R/B before commit. When the
//! compositor also advertises `Abgr8888` (little-endian RGBA byte order) we can
//! skip that swap and hand tiny-skia's bytes over directly.

use wayland_client::protocol::wl_shm;

/// A shm wire format meander knows how to render into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PixelFormat {
    /// `Argb8888` — always available per the `wl_shm` spec. Byte order on a
    /// little-endian host is BGRA, so the RGBA rasteriser output needs an R/B
    /// swap before commit.
    Argb8888,
    /// `Abgr8888` — optional. Byte order on a little-endian host is RGBA, which
    /// matches tiny-skia exactly, so no swap is needed.
    Abgr8888,
}

impl PixelFormat {
    pub(crate) fn wl_format(self) -> wl_shm::Format {
        match self {
            PixelFormat::Argb8888 => wl_shm::Format::Argb8888,
            PixelFormat::Abgr8888 => wl_shm::Format::Abgr8888,
        }
    }

    /// Whether the premultiplied-RGBA rasteriser output must have R and B
    /// swapped to land in this format's byte order on a little-endian host.
    pub(crate) fn needs_rb_swap(self) -> bool {
        match self {
            PixelFormat::Argb8888 => true,
            PixelFormat::Abgr8888 => false,
        }
    }

    /// Pick the best format meander can use given the set advertised by the
    /// compositor. Prefers the swap-free `Abgr8888`, else falls back to the
    /// mandatory `Argb8888`.
    pub(crate) fn best(abgr_advertised: bool) -> Self {
        if abgr_advertised {
            PixelFormat::Abgr8888
        } else {
            PixelFormat::Argb8888
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argb_needs_swap_abgr_does_not() {
        assert!(PixelFormat::Argb8888.needs_rb_swap());
        assert!(!PixelFormat::Abgr8888.needs_rb_swap());
    }

    #[test]
    fn best_prefers_abgr_when_available() {
        assert_eq!(PixelFormat::best(true), PixelFormat::Abgr8888);
        assert_eq!(PixelFormat::best(false), PixelFormat::Argb8888);
    }
}
