//! Memfd-backed wl_shm buffer pool with double buffering.

use std::num::NonZeroUsize;
use std::os::fd::{AsFd, OwnedFd};
use std::ptr::NonNull;

use rustix::fs::{ftruncate, memfd_create, MemfdFlags};
use rustix::mm::{mmap, munmap, MapFlags, ProtFlags};
use wayland_client::{
    protocol::{wl_buffer, wl_shm, wl_shm_pool},
    Dispatch, QueueHandle,
};

use crate::buffer_layout::BufferLayout;
use crate::error::Result;
use crate::shm_format::PixelFormat;

/// Number of buffers meander double-buffers a surface with.
pub(crate) const BUFFER_COUNT: usize = 2;

pub(crate) struct ShmPool {
    // Owned for the lifetime of the pool — wayland-client maps it server-side
    // when we create_pool, and we munmap our own view in Drop. Dropping the
    // OwnedFd after munmap releases the underlying memfd.
    _fd: OwnedFd,
    pool: wl_shm_pool::WlShmPool,
    map_ptr: NonNull<u8>,
    map_size: usize,
    pub(crate) buffers: [ShmBuffer; BUFFER_COUNT],
    pub(crate) layout: BufferLayout,
    /// Wire format the buffers were created with. Determines whether the draw
    /// path has to swap R/B before commit.
    pub(crate) format: PixelFormat,
}

pub(crate) struct ShmBuffer {
    pub(crate) wl: wl_buffer::WlBuffer,
    pub(crate) offset: usize,
    pub(crate) in_use: bool,
}

impl ShmPool {
    pub(crate) fn new<D>(
        shm: &wl_shm::WlShm,
        layout: BufferLayout,
        format: PixelFormat,
        qh: &QueueHandle<D>,
    ) -> Result<Self>
    where
        D: Dispatch<wl_shm_pool::WlShmPool, ()> + Dispatch<wl_buffer::WlBuffer, ()> + 'static,
    {
        // `layout` is already validated: every cast below is guaranteed lossless
        // and `total` fits i32, so no arbitrary input reaches memfd/mmap or the
        // wire without passing BufferLayout::new.
        debug_assert_eq!(layout.buffer_count, BUFFER_COUNT);
        let total = layout.total;
        let width = layout.width as i32;
        let height = layout.height as i32;
        let stride = layout.stride as i32;
        let wl_format = format.wl_format();

        let fd = memfd_create("meander-shm", MemfdFlags::CLOEXEC)?;
        ftruncate(&fd, total as u64)?;

        let map_ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                NonZeroUsize::new(total)
                    .expect("layout.total is non-zero")
                    .get(),
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )?
        };
        let map_ptr = NonNull::new(map_ptr as *mut u8).expect("mmap returned non-null");

        let pool = shm.create_pool(fd.as_fd(), total as i32, qh, ());
        let make = |idx: usize| ShmBuffer {
            wl: pool.create_buffer(
                layout.offset(idx) as i32,
                width,
                height,
                stride,
                wl_format,
                qh,
                (),
            ),
            offset: layout.offset(idx),
            in_use: false,
        };
        let buffers = [make(0), make(1)];

        Ok(Self {
            _fd: fd,
            pool,
            map_ptr,
            map_size: total,
            buffers,
            layout,
            format,
        })
    }

    /// Borrow the pixel slice for buffer `idx`. Premultiplied RGBA.
    pub(crate) fn pixels_mut(&mut self, idx: usize) -> &mut [u8] {
        assert!(idx < self.buffers.len(), "shm buffer index out of range");
        let off = self.buffers[idx].offset;
        let len = self.layout.per_buffer;
        // Invariant, checked in release (not just debug): the validated layout
        // guarantees `off + len <= total == map_size`. Keeping this as a real
        // assert means a future refactor that broke the invariant would panic
        // rather than hand out a slice past the mapping.
        assert!(
            off.checked_add(len).is_some_and(|end| end <= self.map_size),
            "shm buffer slice would run past the mapped region"
        );
        // Safety: we own the mapping for self.map_size bytes, off+len is in
        // range (asserted above), and the &mut self borrow excludes aliasing
        // for the lifetime of the returned slice.
        unsafe { std::slice::from_raw_parts_mut(self.map_ptr.as_ptr().add(off), len) }
    }

    pub(crate) fn pick_free(&self) -> Option<usize> {
        self.buffers.iter().position(|b| !b.in_use)
    }

    /// Convert premultiplied RGBA (tiny-skia's native layout) to
    /// premultiplied BGRA (the byte order Wayland's `Argb8888` expects on
    /// little-endian hosts).
    pub(crate) fn rgba_to_bgra(buf: &mut [u8]) {
        for chunk in buf.chunks_exact_mut(4) {
            chunk.swap(0, 2);
        }
    }

    /// Mark a buffer free if it matches `released`. Returns true when found.
    pub(crate) fn release(&mut self, released: &wl_buffer::WlBuffer) -> bool {
        for b in self.buffers.iter_mut() {
            if &b.wl == released {
                b.in_use = false;
                return true;
            }
        }
        false
    }
}

impl Drop for ShmPool {
    fn drop(&mut self) {
        for b in &self.buffers {
            b.wl.destroy();
        }
        self.pool.destroy();
        unsafe {
            let _ = munmap(self.map_ptr.as_ptr() as *mut _, self.map_size);
        }
        // `_fd` is dropped here, releasing the memfd.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_to_bgra(buf: &mut [u8]) {
        ShmPool::rgba_to_bgra(buf);
    }

    #[test]
    fn rgba_to_bgra_swaps_red_and_blue_keeps_green_and_alpha() {
        let mut buf = [10, 20, 30, 40];
        rgba_to_bgra(&mut buf);
        assert_eq!(buf, [30, 20, 10, 40]);
    }

    #[test]
    fn rgba_to_bgra_is_an_involution() {
        let original = [11u8, 22, 33, 44, 55, 66, 77, 88];
        let mut buf = original;
        rgba_to_bgra(&mut buf);
        rgba_to_bgra(&mut buf);
        assert_eq!(buf, original);
    }

    #[test]
    fn rgba_to_bgra_handles_empty_buffer() {
        let mut buf: [u8; 0] = [];
        rgba_to_bgra(&mut buf);
        // No panic, no work.
    }

    #[test]
    fn rgba_to_bgra_ignores_trailing_partial_pixel() {
        // chunks_exact(4) skips any remainder; we use that to guarantee
        // alignment to whole pixels. Verify behaviour.
        let mut buf = [1, 2, 3, 4, 5, 6];
        rgba_to_bgra(&mut buf);
        assert_eq!(buf, [3, 2, 1, 4, 5, 6]);
    }

    #[test]
    fn rgba_to_bgra_does_not_change_grey_pixels() {
        // Grey pixels have R == B by construction, so the swap is invisible.
        let mut buf = [128, 64, 128, 255, 200, 50, 200, 128];
        let copy = buf;
        rgba_to_bgra(&mut buf);
        assert_eq!(buf, copy);
    }
}
