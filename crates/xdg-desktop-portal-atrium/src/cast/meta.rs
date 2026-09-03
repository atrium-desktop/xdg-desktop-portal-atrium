//! `SPA_META_VideoDamage`: the producer offers the metadata and attaches
//! the compositor's per-frame damage rects to every published buffer, so
//! consumers (OBS's PipeWire source, encoders) can re-read only what
//! changed. The bindings reach the raw spa meta symbols through
//! [`spa_sys`]; the unsafe island below mirrors the `add_buffer` patching
//! in `cast::mod`.

use std::collections::HashMap;

use super::state::BufferMetaSnapshot;

use pipewire::spa::pod::{self};
use pipewire::spa::sys as spa_sys;
use pipewire::spa::{self};
use pipewire::sys as pw_sys;

/// Snapshot a pool buffer's meta array, keyed by meta type.
///
/// Called from `add_buffer`, where the layout is intact. See
/// [`crate::cast::state::StreamData::buffer_metas`] for why the live array
/// cannot be trusted after the first consumer return on PipeWire 1.0.x.
///
/// # Safety
/// `buffer` must be a live pool buffer of this stream handed to
/// `add_buffer`, whose `spa_buffer` and `metas` array are valid.
pub(crate) unsafe fn snapshot_metas(buffer: *mut pw_sys::pw_buffer) -> BufferMetaSnapshot {
    let mut map = HashMap::new();
    let spa_buffer = unsafe { (*buffer).buffer };
    if spa_buffer.is_null() {
        return map;
    }
    let (n_metas, metas) = unsafe { ((*spa_buffer).n_metas, (*spa_buffer).metas) };
    for index in 0..n_metas {
        // SAFETY: `metas` is the live meta array of length `n_metas`.
        let meta = unsafe { metas.add(index as usize) };
        let meta = unsafe { &*meta };
        map.insert(meta.type_, (meta.data as usize, meta.size));
    }
    map
}

/// Resolve a meta block on a pool buffer, preferring the live array and
/// falling back to the `add_buffer` snapshot when PipeWire 1.0.x has zeroed
/// the array's `type` fields after a consumer return. Returns the meta's
/// data pointer and size.
///
/// # Safety
/// `buffer` must be a live pool buffer of this stream whose snapshot (if
/// used) was captured from its own `add_buffer`.
pub(crate) unsafe fn find_meta(
    buffer: *mut pw_sys::pw_buffer,
    meta_type: u32,
    snapshot: &BufferMetaSnapshot,
) -> Option<(*mut std::ffi::c_void, u32)> {
    let spa_buffer = unsafe { (*buffer).buffer };
    if !spa_buffer.is_null() {
        // SAFETY: the live array is valid memory even when its `type`
        // fields were zeroed; the lookup only reads.
        let live = unsafe { spa_sys::spa_buffer_find_meta(spa_buffer, meta_type) };
        if !live.is_null() {
            // SAFETY: a live hit is a negotiated `spa_meta`.
            let meta = unsafe { &*live };
            return Some((meta.data, meta.size));
        }
    }
    // SAFETY: snapshot entries were captured from this same buffer's
    // `add_buffer` layout; `remove_buffer` clears them before the buffer
    // can be freed.
    snapshot
        .get(&meta_type)
        .map(|(data, size)| (*data as *mut std::ffi::c_void, *size))
}

/// Damage regions offered to consumers. A frame with more rects degrades
/// to a single full-frame region: damage is a read-back hint and
/// over-reporting is always safe, under-reporting is not.
pub(crate) const MAX_DAMAGE_REGIONS: usize = 16;

/// The `SPA_PARAM_Meta` offer for `SPA_META_Header`: carries timestamps (PTS),
/// sequence numbers, and buffer flags so consumers like OBS / FFmpeg can
/// pace, synchronize, and track frames accurately.
pub(crate) fn header_meta_pod() -> Vec<u8> {
    let object = pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            pod::Property {
                key: 1, // SPA_PARAM_META_type
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Id(spa::utils::Id(spa_sys::SPA_META_Header)),
            },
            pod::Property {
                key: 2, // SPA_PARAM_META_size
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(std::mem::size_of::<spa_sys::spa_meta_header>() as i32),
            },
        ],
    };
    pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pod::Value::Object(object),
    )
    .expect("pod serialization")
    .0
    .into_inner()
}

/// The `SPA_PARAM_Meta` offer: one VideoDamage metadata block per buffer
/// with room for [`MAX_DAMAGE_REGIONS`] regions. The size is the region
/// array's bytes, matching every producer that ships the metadata (the
/// `spa_meta` header itself is allocated by PipeWire, not counted here).
pub(crate) fn damage_meta_pod() -> Vec<u8> {
    let object = pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            pod::Property {
                key: 1, // SPA_PARAM_META_type
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Id(spa::utils::Id(spa_sys::SPA_META_VideoDamage)),
            },
            pod::Property {
                key: 2, // SPA_PARAM_META_size
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(
                    (MAX_DAMAGE_REGIONS * std::mem::size_of::<spa_sys::spa_meta_region>()) as i32,
                ),
            },
        ],
    };
    pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pod::Value::Object(object),
    )
    .expect("pod serialization")
    .0
    .into_inner()
}

/// Get monotonic clock timestamp in nanoseconds for buffer PTS.
pub(crate) fn monotonic_pts_nanos() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// Attach standard header metadata (PTS timestamp and monotonic sequence)
/// to a buffer about to be queued into PipeWire.
///
/// # Safety
/// `buffer` must be a live pool buffer of this stream.
pub(crate) unsafe fn attach_header(
    buffer: *mut pw_sys::pw_buffer,
    snapshot: &BufferMetaSnapshot,
    seq: u64,
    pts_nanos: i64,
) {
    let Some((header, _)) = (unsafe { find_meta(buffer, spa_sys::SPA_META_Header, snapshot) })
    else {
        log::warn!(
            "portal: pool buffer carries no Header meta; PTS/sequence not attached (seq {seq} lost)"
        );
        return;
    };
    // SAFETY: the Header meta's data pointer names the `spa_meta_header`
    // block in the negotiated layout.
    unsafe {
        let header = header.cast::<spa_sys::spa_meta_header>();
        (*header).flags = 0;
        (*header).offset = 0;
        (*header).pts = pts_nanos;
        (*header).dts_offset = 0;
        (*header).seq = seq;
    }
}

/// Attach the frame's damage rects to a buffer about to be queued. A
/// no-op when the buffer carries no VideoDamage meta (the consumer did
/// not negotiate it). Regions beyond the written ones are zeroed:
/// consumers iterate the whole capacity, and a reused buffer must never
/// leak a previous frame's damage. An empty damage list therefore writes
/// all-zero regions (frame unchanged), never stale rects.
///
/// # Safety
/// `buffer` must be a live pool buffer of this stream, not referenced by
/// the consumer at this instant (the publish path queues it right after).
pub(crate) unsafe fn attach_damage(
    buffer: *mut pw_sys::pw_buffer,
    snapshot: &BufferMetaSnapshot,
    damage: &[atrium_portal_ipc::Rect],
    width: u32,
    height: u32,
) {
    // SAFETY: `buffer` is live per the caller; the snapshot was captured
    // from its own `add_buffer` layout.
    let Some((data, size)) =
        (unsafe { find_meta(buffer, spa_sys::SPA_META_VideoDamage, snapshot) })
    else {
        return;
    };
    // Rebuild the `spa_meta` view `spa_meta_first`/`spa_meta_check` need:
    // the live array's entry may have been zeroed, but the data pointer
    // and size recorded at `add_buffer` still name the same block.
    let meta = spa_sys::spa_meta {
        type_: spa_sys::SPA_META_VideoDamage,
        data,
        size,
    };
    // Over-capacity damage collapses to one full-frame region (see the
    // constant's docs).
    let full_frame;
    let rects = if damage.len() > MAX_DAMAGE_REGIONS {
        full_frame = [atrium_portal_ipc::Rect::new(
            0,
            0,
            width as i32,
            height as i32,
        )];
        &full_frame[..]
    } else {
        damage
    };
    // SAFETY: `meta` names the negotiated VideoDamage block on this
    // buffer; writes are bounded by `spa_meta_check`, which guards the
    // block's real capacity regardless of what was offered.
    unsafe {
        let meta = &meta as *const spa_sys::spa_meta;
        let mut region = spa_sys::spa_meta_first(meta).cast::<spa_sys::spa_meta_region>();
        let mut index = 0;
        while spa_sys::spa_meta_check(region.cast::<std::ffi::c_void>(), meta) {
            let value = match rects.get(index) {
                Some(rect) => spa_sys::spa_meta_region {
                    region: spa_sys::spa_region {
                        position: spa_sys::spa_point {
                            x: rect.origin.x,
                            y: rect.origin.y,
                        },
                        size: spa_sys::spa_rectangle {
                            width: rect.size.w.max(0) as u32,
                            height: rect.size.h.max(0) as u32,
                        },
                    },
                },
                None => std::mem::zeroed(),
            };
            region.write(value);
            region = region.add(1);
            index += 1;
        }
    }
}

/// Parse a `SPA_PARAM_Meta` pod back into (type, size) for tests.
#[cfg(test)]
pub(crate) fn parse_meta_pod(bytes: &[u8]) -> Option<(u32, i32)> {
    use pipewire::spa::pod::Pod;
    let pod = Pod::from_bytes(bytes)?;
    let value = pod::deserialize::PodDeserializer::deserialize_from::<pod::Value>(pod.as_bytes())
        .ok()?
        .1;
    let pod::Value::Object(object) = value else {
        return None;
    };
    let mut meta_type = None;
    let mut size = None;
    for property in &object.properties {
        match property.key {
            1 => {
                if let pod::Value::Id(id) = &property.value {
                    meta_type = Some(id.0);
                }
            }
            2 => {
                if let pod::Value::Int(value) = &property.value {
                    size = Some(*value);
                }
            }
            _ => {}
        }
    }
    Some((meta_type?, size?))
}
