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

use crate::error::Result;

pub(crate) struct ShmPool {
    fd: OwnedFd,
    pool: wl_shm_pool::WlShmPool,
    map_ptr: NonNull<u8>,
    map_size: usize,
    pub(crate) buffers: [ShmBuffer; 2],
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: usize,
}

pub(crate) struct ShmBuffer {
    pub(crate) wl: wl_buffer::WlBuffer,
    pub(crate) offset: usize,
    pub(crate) in_use: bool,
}

// The mmap is process-local memory; we hand &mut [u8] slices out only while
// `self` is borrowed mutably.
unsafe impl Send for ShmPool {}

impl ShmPool {
    pub(crate) fn new<D>(
        shm: &wl_shm::WlShm,
        width: u32,
        height: u32,
        qh: &QueueHandle<D>,
    ) -> Result<Self>
    where
        D: Dispatch<wl_shm_pool::WlShmPool, ()> + Dispatch<wl_buffer::WlBuffer, ()> + 'static,
    {
        let width = width.max(1);
        let height = height.max(1);
        let stride = width as usize * 4;
        let per_buffer = stride * height as usize;
        let total = per_buffer * 2;

        let fd = memfd_create("meander-shm", MemfdFlags::CLOEXEC)?;
        ftruncate(&fd, total as u64)?;

        let map_ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                NonZeroUsize::new(total).unwrap().get(),
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )?
        };
        let map_ptr = NonNull::new(map_ptr as *mut u8).expect("mmap returned non-null");

        let pool = shm.create_pool(fd.as_fd(), total as i32, qh, ());
        let buffers = [
            ShmBuffer {
                wl: pool.create_buffer(
                    0,
                    width as i32,
                    height as i32,
                    stride as i32,
                    wl_shm::Format::Argb8888,
                    qh,
                    (),
                ),
                offset: 0,
                in_use: false,
            },
            ShmBuffer {
                wl: pool.create_buffer(
                    per_buffer as i32,
                    width as i32,
                    height as i32,
                    stride as i32,
                    wl_shm::Format::Argb8888,
                    qh,
                    (),
                ),
                offset: per_buffer,
                in_use: false,
            },
        ];

        Ok(Self {
            fd,
            pool,
            map_ptr,
            map_size: total,
            buffers,
            width,
            height,
            stride,
        })
    }

    /// Borrow the pixel slice for buffer `idx` (0 or 1). Premultiplied RGBA.
    pub(crate) fn pixels_mut(&mut self, idx: usize) -> &mut [u8] {
        let off = self.buffers[idx].offset;
        let len = self.stride * self.height as usize;
        // Safety: we own the mapping for self.map_size bytes, off+len is in
        // range by construction, and the &mut self borrow excludes aliasing.
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
        let _ = &self.fd;
    }
}
