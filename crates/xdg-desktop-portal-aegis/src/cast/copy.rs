//! The mmap-copy transport: each frame descriptor is memory-mapped and
//! copied into a shared-pool buffer exactly once, and the pool's backing
//! memfds live here. Memory-mapping is only defined for CPU-typed pixels;
//! the callers keep tiled dmabufs out of this path.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr::NonNull;

use pipewire as pw;

use super::frame::MAX_FRAME_BYTES;
use super::state::StreamData;

/// Portal-owned backing for one copy-path pool buffer: a memfd the
/// consumer maps, plus our own mapping of it.
pub(crate) struct PoolMem {
    pub(crate) file: File,
    pub(crate) map: *mut u8,
    pub(crate) len: usize,
}

impl PoolMem {
    pub(crate) fn new(len: usize) -> io::Result<PoolMem> {
        // SAFETY: the name is static and NUL-terminated; the fd is checked
        // before ownership is constructed.
        let fd = unsafe { libc::memfd_create(c"aegis-portal-pool".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a new owned descriptor from memfd_create.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(len as u64)?;
        // SAFETY: the file is `len` bytes and outlives the mapping (owned by
        // the returned value).
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(PoolMem {
            file,
            map: map.cast::<u8>(),
            len,
        })
    }
}

impl Drop for PoolMem {
    fn drop(&mut self) {
        // SAFETY: `map`/`len` name the live mapping created in `new`.
        unsafe { libc::munmap(self.map.cast(), self.len) };
    }
}

/// Map a frame descriptor and copy the pixels into a shared-pool buffer.
/// Sealed memfds and mappable dmabufs take the same path. `width`/`height`
/// come from the caller's transport borrow (the publish path already holds
/// it); `damage` rides to the queued buffer's VideoDamage metadata.
///
/// Returns `false` only when the pool had no free buffer, so the caller
/// keeps the frame pending and a later cycle retries it. Every other
/// outcome — published, or dropped for a permanent reason — returns `true`.
pub(crate) fn copy_into_pool(
    stream: &pw::stream::Stream,
    data: &StreamData,
    file: &File,
    stride: u32,
    width: u32,
    height: u32,
    damage: &[aegis_portal_ipc::Rect],
) -> bool {
    let height = height as usize;
    let row_bytes = width as usize * 4;
    let stride = stride as usize;

    let Ok(src_len) = file.metadata().map(|meta| meta.len() as usize) else {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        log::debug!("portal: could not stat the frame descriptor");
        return true;
    };
    let needed = stride * (height - 1) + row_bytes;
    if src_len < needed || needed > MAX_FRAME_BYTES {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        log::warn!(
            "portal: frame payload of {src_len} bytes cannot hold {height} rows of stride {stride}"
        );
        return true;
    }
    // SAFETY: the descriptor outlives the mapping; the caller owns it.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            src_len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        if data.warned_unmappable.replace(true) {
            log::debug!(
                "portal: frame descriptor is not mappable: {}",
                io::Error::last_os_error()
            );
        } else {
            log::warn!(
                "portal: frame descriptor is not mappable ({}); frames are dropped because \
                 this capture cannot be delivered as shared memory",
                io::Error::last_os_error()
            );
        }
        return true;
    }

    let Some(pool_raw) = data
        .pool
        .borrow_mut()
        .pop_front()
        .or_else(|| NonNull::new(unsafe { stream.dequeue_raw_buffer() }).map(NonNull::as_ptr))
    else {
        // Pool starvation is transient: the consumer returns buffers on
        // later cycles, and the caller keeps the frame pending for a retry.
        // SAFETY: `map`/`src_len` name the live mapping created above.
        unsafe { libc::munmap(map, src_len) };
        log::debug!("portal: no free PipeWire buffer; holding frame for the next cycle");
        return false;
    };
    let published = unsafe {
        // SAFETY: `pool_raw` is a live pool buffer of this stream,
        // dequeued on this thread; its first spa_data is a mapped memory
        // block of `maxsize` bytes.
        let spa_buffer = (*pool_raw).buffer;
        let spa_data = (*spa_buffer).datas;
        let dest_ptr = (*spa_data).data.cast::<u8>();
        let dest_cap = (*spa_data).maxsize as usize;
        if dest_ptr.is_null() || dest_cap < height * row_bytes {
            data.pool.borrow_mut().push_back(pool_raw);
            false
        } else {
            let dest = std::slice::from_raw_parts_mut(dest_ptr, dest_cap);
            let src = std::slice::from_raw_parts(map.cast::<u8>(), src_len);
            copy_rows(src, stride, dest, row_bytes, height);
            let chunk = (*spa_data).chunk;
            (*chunk).offset = 0;
            (*chunk).size = (height * row_bytes) as u32;
            (*chunk).stride = row_bytes as i32;
            let seq = data.sequence.get();
            data.sequence.set(seq + 1);
            let pts = super::meta::monotonic_pts_nanos();
            // The metas map is keyed per buffer; a missing entry (no
            // add_buffer snapshot) simply skips meta attachment.
            let buffer_metas = data.buffer_metas.borrow();
            let metas = buffer_metas
                .get(&(pool_raw as usize))
                .cloned()
                .unwrap_or_default();
            drop(buffer_metas);
            super::meta::attach_header(pool_raw, &metas, seq, pts);
            super::meta::attach_damage(pool_raw, &metas, damage, width, height as u32);
            log::debug!(
                "portal: published copy frame seq {seq} (pool free: {})",
                data.pool.borrow().len()
            );
            stream.queue_raw_buffer(pool_raw);
            true
        }
    };
    if !published {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        log::debug!("portal: pool buffer could not hold the frame; dropping frame");
    }
    // SAFETY: `map`/`src_len` name the live mapping created above.
    unsafe { libc::munmap(map, src_len) };
    true
}

/// Copy `height` rows of `row_bytes` from a source with the given row
/// stride into a tightly packed destination.
pub(crate) fn copy_rows(
    src: &[u8],
    src_stride: usize,
    dest: &mut [u8],
    row_bytes: usize,
    height: usize,
) {
    if src_stride == row_bytes {
        dest[..height * row_bytes].copy_from_slice(&src[..height * row_bytes]);
        return;
    }
    for row in 0..height {
        let src_start = row * src_stride;
        let dest_start = row * row_bytes;
        dest[dest_start..dest_start + row_bytes]
            .copy_from_slice(&src[src_start..src_start + row_bytes]);
    }
}
