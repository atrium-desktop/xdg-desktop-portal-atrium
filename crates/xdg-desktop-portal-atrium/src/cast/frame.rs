//! Received compositor frames: the payload a frame carries and its
//! validation against the stream's announced format and geometry.

use std::fs::File;

use atrium_portal_ipc::{StreamFrame, StreamPayload};

use super::format::{AnnouncedFormat, DRM_FORMAT_MOD_LINEAR};

pub(crate) const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// A received compositor frame.
pub(crate) enum FramePayload {
    /// A frame carrying its own descriptor (sealed memfd or dmabuf blob),
    /// plus the plane stride and the damage rects from the frame header.
    Descriptor {
        file: File,
        stride: u32,
        damage: Vec<atrium_portal_ipc::Rect>,
    },
    /// A protocol-25 frame referencing a slot transferred at start, plus
    /// the damage rects from the frame header.
    Slot {
        slot: u32,
        damage: Vec<atrium_portal_ipc::Rect>,
    },
}

/// Check one received frame against the stream's announced format and
/// geometry, returning the storable payload.
pub(crate) fn validate_frame(
    frame: StreamFrame,
    width: u32,
    height: u32,
    announced: AnnouncedFormat,
    slot_count: usize,
) -> Result<FramePayload, String> {
    if frame.width != width || frame.height != height {
        return Err(format!(
            "frame geometry {}x{} differs from the announced {width}x{height}",
            frame.width, frame.height
        ));
    }
    let row_bytes = width as u64 * 4;
    if u64::from(frame.stride) < row_bytes || frame.stride > i32::MAX as u32 {
        return Err(format!("invalid frame stride {}", frame.stride));
    }
    if let Some(slot) = frame.slot {
        if slot_count == 0 {
            return Err("slot frame on a stream without a slot table".to_string());
        }
        if slot as usize >= slot_count {
            return Err(format!(
                "slot {slot} is outside the {slot_count}-slot table"
            ));
        }
        if !matches!(frame.payload, StreamPayload::Slot) {
            return Err("slot frame carried a descriptor".to_string());
        }
        return Ok(FramePayload::Slot {
            slot,
            damage: frame.damage,
        });
    }
    match (announced, frame.format, frame.payload) {
        (
            AnnouncedFormat::Shm(_),
            atrium_portal_ipc::StreamPixelFormat::Bgra8
            | atrium_portal_ipc::StreamPixelFormat::Rgba8,
            StreamPayload::Memfd(file),
        ) => {
            // The compositor's SHM readback is tightly packed.
            if u64::from(frame.stride) != row_bytes {
                return Err(format!(
                    "SHM frame stride {} is not tightly packed",
                    frame.stride
                ));
            }
            Ok(FramePayload::Descriptor {
                file,
                stride: frame.stride,
                damage: frame.damage,
            })
        }
        (
            AnnouncedFormat::Dmabuf {
                drm_format,
                modifier,
                ..
            },
            atrium_portal_ipc::StreamPixelFormat::Dmabuf {
                drm_format: frame_drm,
                modifier: frame_modifier,
            },
            StreamPayload::Dmabuf(file),
        ) if frame_drm == drm_format && frame_modifier == modifier => {
            // The copy path memory-maps the descriptor, which is only
            // defined for CPU-typed pixels: a tiled dmabuf would copy
            // tile-swizzled bytes. Tiled streams belong to the
            // compositor's SHM readback (see `sync_transport`).
            if modifier != DRM_FORMAT_MOD_LINEAR {
                return Err(format!(
                    "dmabuf frame with non-LINEAR modifier {modifier:#x} cannot be memory-mapped"
                ));
            }
            Ok(FramePayload::Descriptor {
                file,
                stride: frame.stride,
                damage: frame.damage,
            })
        }
        (announced, wire, _) => Err(format!(
            "frame format {wire:?} does not match the announced {announced:?}"
        )),
    }
}

pub(crate) fn frame_len(width: u32, height: u32) -> Result<usize, String> {
    let bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(height as usize))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| format!("invalid compositor stream geometry {width}x{height}"))?;
    if width == 0 || height == 0 || bytes > MAX_FRAME_BYTES {
        return Err(format!(
            "compositor stream geometry {width}x{height} exceeds the 256 MiB frame limit"
        ));
    }
    Ok(bytes)
}
