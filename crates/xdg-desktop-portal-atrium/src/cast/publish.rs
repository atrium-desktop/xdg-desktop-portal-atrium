//! The PipeWire `process` path: reclaim the buffers PipeWire returns
//! (releasing their compositor slots), then publish the pending frame —
//! as the pool buffer bound to its slot when the consumer takes dmabufs,
//! or through the copy path otherwise.

use std::ptr::NonNull;
use std::rc::Rc;

use pipewire as pw;

use super::copy::copy_into_pool;
use super::frame::FramePayload;
use super::state::StreamData;

/// Reclaim every buffer PipeWire returns to the producer. A buffer bound to
/// a compositor slot triggers the slot's release; unbound buffers go to the
/// copy path's stash.
fn reclaim_returned_buffers(stream: &pw::stream::Stream, data: &StreamData) {
    loop {
        // SAFETY: called from the stream's own thread inside `process`.
        let raw = unsafe { stream.dequeue_raw_buffer() };
        let Some(raw) = NonNull::new(raw) else {
            break;
        };
        let mut transport = data.transport.borrow_mut();
        let bound = transport
            .slot_bindings
            .iter_mut()
            .enumerate()
            .find(|(_, binding)| binding.pool == Some(raw.as_ptr()));
        if let Some((slot, binding)) = bound {
            if binding.in_flight {
                binding.in_flight = false;
                let stream_id = transport.stream_id;
                let has_slots = !transport.slot_files.is_empty();
                let client = Rc::clone(&data.client);
                drop(transport);
                if has_slots
                    && let Err(error) = client
                        .borrow_mut()
                        .release_stream_buffer(stream_id, slot as u32)
                {
                    log::debug!("portal: slot release failed: {error}");
                }
            }
        } else {
            drop(transport);
            data.pool.borrow_mut().push_back(raw.as_ptr());
        }
    }
}

/// The PipeWire `process` callback: publish the pending frame, if any.
pub(crate) fn process_frame(stream: &pw::stream::Stream, data: &mut StreamData) {
    reclaim_returned_buffers(stream, data);

    if !data.pending.get() {
        return;
    }
    // The latest frame stays stored after publishing: a consumer that
    // (re)activates later gets it republished on the Streaming transition,
    // since queued buffers are flushed back on pause.
    let frame = data.latest.borrow();
    let Some(frame) = &*frame else {
        data.pending.set(false);
        return;
    };
    match frame {
        FramePayload::Descriptor {
            file,
            stride,
            damage,
        } => {
            // A pool-starved frame stays pending: its memfd pixels are
            // immutable, so the next cycle — frame- or keepalive-triggered —
            // safely retries it once the consumer returns a pool buffer.
            let (width, height) = {
                let transport = data.transport.borrow();
                (transport.width, transport.height)
            };
            if copy_into_pool(stream, data, file, *stride, width, height, damage) {
                data.pending.set(false);
            }
        }
        FramePayload::Slot { slot, damage } => {
            // Slot frames never retry: every failure mode is permanent for
            // that frame, and the copy fallback releases the slot
            // immediately, so the pixels a retry would read may already be
            // overwritten.
            publish_slot(stream, data, *slot, damage);
            data.pending.set(false);
        }
    }
}

/// Publish a protocol-25 slot frame: queue the pool buffer bound to the
/// slot when the consumer takes dmabufs, or copy the slot's pixels into a
/// free pool buffer (and release the slot immediately) otherwise.
fn publish_slot(
    stream: &pw::stream::Stream,
    data: &StreamData,
    slot: u32,
    damage: &[atrium_portal_ipc::Rect],
) {
    let mut transport = data.transport.borrow_mut();
    let Some(binding) = transport.slot_bindings.get_mut(slot as usize) else {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        log::warn!("portal: frame for unknown slot {slot}; dropping");
        return;
    };
    if binding.in_flight {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        log::warn!("portal: compositor reused slot {slot} before its release; dropping frame");
        return;
    }
    let forward = data.negotiation.borrow().forwarding_eligible();
    match (forward, binding.pool) {
        (true, Some(pool_raw)) => {
            binding.in_flight = true;
            let (width, height) = (transport.width, transport.height);
            drop(transport);
            // SAFETY: `pool_raw` is a live pool buffer of this stream bound
            // to this slot, dequeued earlier and not referenced elsewhere.
            let seq = data.sequence.get();
            data.sequence.set(seq + 1);
            let pts = super::meta::monotonic_pts_nanos();
            unsafe {
                let buffer_metas = data.buffer_metas.borrow();
                let metas = buffer_metas
                    .get(&(pool_raw as usize))
                    .cloned()
                    .unwrap_or_default();
                drop(buffer_metas);
                super::meta::attach_header(pool_raw, &metas, seq, pts);
                super::meta::attach_damage(pool_raw, &metas, damage, width, height);
                stream.queue_raw_buffer(pool_raw)
            };
        }
        _ => {
            if !transport.cpu_mappable() {
                // Never linear-copy tiled pixels: the transport switch to
                // the compositor's SHM readback is what serves this
                // consumer. Drop the frame and hand the slot back so the
                // compositor's ring keeps turning until then.
                data.dropped_frames.set(data.dropped_frames.get() + 1);
                drop(transport);
                data.release_slot(slot);
                return;
            }
            let file = &transport.slot_files[slot as usize].file;
            let stride = transport.slot_files[slot as usize].stride;
            let (width, height) = (transport.width, transport.height);
            copy_into_pool(stream, data, file, stride, width, height, damage);
            drop(transport);
            data.release_slot(slot);
        }
    }
}
