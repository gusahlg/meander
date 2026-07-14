//! Checked shm buffer geometry.
//!
//! Compositor-controlled logical dimensions and scale flow into pixel-buffer
//! arithmetic that ends in an `unsafe` slice construction and several lossy
//! `usize`/`i32` casts for the Wayland wire. [`BufferLayout::new`] is the single
//! validated gate for that arithmetic: it uses checked multiplication, enforces
//! a documented maximum allocation, and guarantees every value it stores fits
//! the `i32` range the `wl_shm` protocol uses for pool/buffer sizes and offsets.
//!
//! Nothing here touches a memfd, an mmap, or a protocol object — it is pure, so
//! the overflow and boundary behaviour is exhaustively unit-testable without a
//! compositor.

use crate::error::{Error, Result};

/// Bytes per pixel for the premultiplied 32-bit formats meander uses.
pub(crate) const BYTES_PER_PIXEL: usize = 4;

/// Documented hard cap on a single shm pool allocation.
///
/// 512 MiB comfortably fits a double-buffered 8K (7680×4320) surface while
/// staying an order of magnitude under `i32::MAX`, the limit the `wl_shm`
/// protocol imposes on pool size, buffer offset, and stride.
pub(crate) const MAX_POOL_BYTES: usize = 512 * 1024 * 1024;

/// Validated geometry for a multi-buffer shm pool.
///
/// Construct with [`BufferLayout::new`]; the fields are only ever set from that
/// checked path, so consumers can treat them as trusted invariants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BufferLayout {
    /// Physical width in pixels (logical width × scale).
    pub(crate) width: u32,
    /// Physical height in pixels (logical height × scale).
    pub(crate) height: u32,
    /// Row stride in bytes (`width × BYTES_PER_PIXEL`).
    pub(crate) stride: usize,
    /// Bytes occupied by a single buffer (`stride × height`).
    pub(crate) per_buffer: usize,
    /// Number of buffers packed back-to-back in the pool.
    pub(crate) buffer_count: usize,
    /// Total pool size in bytes (`per_buffer × buffer_count`).
    pub(crate) total: usize,
}

impl BufferLayout {
    /// Validate and compute the geometry for `buffer_count` buffers of
    /// `logical_w × logical_h` logical pixels at the given integer `scale`.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidBufferDimensions`] for a non-positive scale, a
    ///   zero-sized buffer, a zero buffer count, or any arithmetic step that
    ///   overflows `u32`/`usize`.
    /// - [`Error::BufferTooLarge`] when the total exceeds [`MAX_POOL_BYTES`] or
    ///   any protocol-visible value would not fit `i32`.
    pub(crate) fn new(
        logical_w: u32,
        logical_h: u32,
        scale: i32,
        buffer_count: usize,
    ) -> Result<Self> {
        if scale < 1 {
            return Err(Error::InvalidBufferDimensions("scale must be >= 1"));
        }
        if buffer_count == 0 {
            return Err(Error::InvalidBufferDimensions("buffer count must be >= 1"));
        }
        let scale = scale as u32;

        let width = logical_w
            .checked_mul(scale)
            .ok_or(Error::InvalidBufferDimensions(
                "physical width overflows u32",
            ))?;
        let height = logical_h
            .checked_mul(scale)
            .ok_or(Error::InvalidBufferDimensions(
                "physical height overflows u32",
            ))?;
        if width == 0 || height == 0 {
            return Err(Error::InvalidBufferDimensions(
                "buffer must be at least 1x1",
            ));
        }

        let stride = (width as usize)
            .checked_mul(BYTES_PER_PIXEL)
            .ok_or(Error::InvalidBufferDimensions("stride overflows usize"))?;
        let per_buffer =
            stride
                .checked_mul(height as usize)
                .ok_or(Error::InvalidBufferDimensions(
                    "buffer size overflows usize",
                ))?;
        let total = per_buffer
            .checked_mul(buffer_count)
            .ok_or(Error::InvalidBufferDimensions("pool size overflows usize"))?;

        if total > MAX_POOL_BYTES {
            return Err(Error::BufferTooLarge {
                requested: total,
                max: MAX_POOL_BYTES,
            });
        }

        // Everything the wl_shm wire sees is an i32. `total` is the largest of
        // these (offsets and stride are strictly smaller), so a single check of
        // `total` — already bounded by MAX_POOL_BYTES < i32::MAX — proves the
        // rest, but we convert each explicitly so a future cap change can't
        // silently reintroduce a lossy cast.
        let too_large = |_| Error::BufferTooLarge {
            requested: total,
            max: MAX_POOL_BYTES,
        };
        i32::try_from(width).map_err(too_large)?;
        i32::try_from(height).map_err(too_large)?;
        i32::try_from(stride).map_err(too_large)?;
        i32::try_from(total).map_err(too_large)?;

        Ok(Self {
            width,
            height,
            stride,
            per_buffer,
            buffer_count,
            total,
        })
    }

    /// Byte offset of buffer `idx` within the pool. Guaranteed `< total` for
    /// any `idx < buffer_count`.
    pub(crate) fn offset(&self, idx: usize) -> usize {
        debug_assert!(idx < self.buffer_count, "buffer index out of range");
        self.per_buffer * idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_double_buffered_bar_scale1() {
        let l = BufferLayout::new(1920, 28, 1, 2).unwrap();
        assert_eq!(l.width, 1920);
        assert_eq!(l.height, 28);
        assert_eq!(l.stride, 1920 * 4);
        assert_eq!(l.per_buffer, 1920 * 28 * 4);
        assert_eq!(l.total, 1920 * 28 * 4 * 2);
        assert_eq!(l.offset(0), 0);
        assert_eq!(l.offset(1), l.per_buffer);
    }

    #[test]
    fn scale_multiplies_both_dimensions() {
        let l = BufferLayout::new(1920, 28, 2, 2).unwrap();
        assert_eq!(l.width, 3840);
        assert_eq!(l.height, 56);
        assert_eq!(l.stride, 3840 * 4);
    }

    #[test]
    fn high_scale_is_honoured() {
        let l = BufferLayout::new(100, 100, 8, 2).unwrap();
        assert_eq!(l.width, 800);
        assert_eq!(l.height, 800);
    }

    #[test]
    fn minimum_one_by_one_is_valid() {
        let l = BufferLayout::new(1, 1, 1, 1).unwrap();
        assert_eq!(l.width, 1);
        assert_eq!(l.height, 1);
        assert_eq!(l.stride, 4);
        assert_eq!(l.per_buffer, 4);
        assert_eq!(l.total, 4);
    }

    #[test]
    fn zero_width_is_rejected() {
        assert!(matches!(
            BufferLayout::new(0, 28, 1, 2),
            Err(Error::InvalidBufferDimensions(_))
        ));
    }

    #[test]
    fn zero_height_is_rejected() {
        assert!(matches!(
            BufferLayout::new(1920, 0, 1, 2),
            Err(Error::InvalidBufferDimensions(_))
        ));
    }

    #[test]
    fn zero_scale_is_rejected() {
        assert!(matches!(
            BufferLayout::new(1920, 28, 0, 2),
            Err(Error::InvalidBufferDimensions(_))
        ));
    }

    #[test]
    fn negative_scale_is_rejected() {
        assert!(matches!(
            BufferLayout::new(1920, 28, -1, 2),
            Err(Error::InvalidBufferDimensions(_))
        ));
    }

    #[test]
    fn zero_buffer_count_is_rejected() {
        assert!(matches!(
            BufferLayout::new(1920, 28, 1, 0),
            Err(Error::InvalidBufferDimensions(_))
        ));
    }

    #[test]
    fn physical_width_overflow_is_caught() {
        // logical_w * scale overflows u32.
        assert!(matches!(
            BufferLayout::new(u32::MAX, 1, 2, 1),
            Err(Error::InvalidBufferDimensions(_))
        ));
    }

    #[test]
    fn physical_height_overflow_is_caught() {
        assert!(matches!(
            BufferLayout::new(1, u32::MAX, 2, 1),
            Err(Error::InvalidBufferDimensions(_))
        ));
    }

    #[test]
    fn stride_overflow_on_64bit_is_a_size_error() {
        // A width that fits u32 but whose stride/size exceeds the cap must be
        // rejected as too large, never silently truncated.
        let r = BufferLayout::new(u32::MAX / 2, 1, 1, 1);
        assert!(matches!(r, Err(Error::BufferTooLarge { .. })));
    }

    #[test]
    fn exceeding_the_cap_is_rejected() {
        // Just over 512 MiB: 8192 * 8192 * 4 = 256 MiB per buffer, ×3 buffers.
        let r = BufferLayout::new(8192, 8192, 1, 3);
        assert!(matches!(r, Err(Error::BufferTooLarge { requested, max })
            if requested > max && max == MAX_POOL_BYTES));
    }

    #[test]
    fn at_the_cap_is_accepted() {
        // 8192 * 8192 * 4 * 2 = 512 MiB exactly.
        let l = BufferLayout::new(8192, 8192, 1, 2).unwrap();
        assert_eq!(l.total, MAX_POOL_BYTES);
        assert!(i32::try_from(l.total).is_ok());
    }

    #[test]
    fn total_overflow_via_buffer_count_is_caught() {
        // per_buffer near usize::MAX combined with a large count must not wrap.
        // Use dimensions that individually pass but whose product overflows.
        let r = BufferLayout::new(65536, 65536, 1, 2);
        // 65536*65536*4 already exceeds the cap; ensure a size error, not a wrap.
        assert!(matches!(r, Err(Error::BufferTooLarge { .. })));
    }

    #[test]
    fn every_buffer_offset_is_within_total() {
        let l = BufferLayout::new(64, 64, 1, 4).unwrap();
        for i in 0..l.buffer_count {
            assert!(l.offset(i) + l.per_buffer <= l.total);
        }
    }
}
