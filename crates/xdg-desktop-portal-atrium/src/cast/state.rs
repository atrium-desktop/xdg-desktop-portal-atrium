//! Live cast state shared between the stream listener, the IPC source, and
//! teardown: the compositor-side transport (with the protocol-25 slot
//! table), the delivery negotiation, and the latest received frame.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use atrium_portal_ipc::Client;
use pipewire as pw;
use pw::sys as pw_sys;

use super::copy::PoolMem;
use super::format::{
    AnnouncedFormat, DMABUF_DATA_TYPE_BIT, DRM_FORMAT_MOD_LINEAR, FixatedFormat, announced_format,
};
use super::frame::FramePayload;
use super::{STREAM_MAX_FPS, StartState};

/// How the fixated PipeWire format makes frames reach the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryMode {
    /// Copy every frame into the shared memory pool.
    Shm,
    /// The consumer fixated the modifier-bearing format: slot buffers may
    /// go out as `SPA_DATA_DmaBuf`.
    Dmabuf,
}

/// Live negotiation state. `param_changed` callbacks update it; the
/// `process` callback reads it for every frame.
#[derive(Debug)]
pub(crate) struct Negotiation {
    pub(crate) mode: DeliveryMode,
    /// The consumer's accepted `SPA_PARAM_BUFFERS_dataType` mask, when the
    /// peer's Buffers param has been observed. A consumer that fixates a
    /// modifier-bearing format is expected to accept DmaBuf buffers, so an
    /// unknown mask does not block forwarding; an observed mask without the
    /// DmaBuf bit does.
    pub(crate) consumer_data_types: Option<u32>,
}

impl Negotiation {
    pub(crate) fn forwarding_eligible(&self) -> bool {
        self.mode == DeliveryMode::Dmabuf
            && self
                .consumer_data_types
                .is_none_or(|mask| mask & DMABUF_DATA_TYPE_BIT != 0)
    }
}

/// One protocol-25 slot's binding to a PipeWire pool buffer.
#[derive(Debug)]
pub(crate) struct SlotBinding {
    /// The pool buffer patched onto this slot's descriptor at `add_buffer`.
    pub(crate) pool: Option<*mut pw_sys::pw_buffer>,
    /// The slot's buffer is with the consumer; the compositor must not
    /// reuse the slot until the release goes out.
    pub(crate) in_flight: bool,
}

/// The compositor-side transport behind the PipeWire stream: which
/// compositor stream frames belong to, what target and geometry they have,
/// and the protocol-25 slot table when streaming dmabuf slots. Shared
/// between the stream listener, the IPC source, and teardown so a
/// transport switch (dmabuf slots ↔ SHM readback) or a geometry restart is
/// observed everywhere without rebuilding listener state.
///
/// Invariant: the value always describes the *live* compositor stream and
/// the PipeWire shape currently offered for it. Every mutation —
/// `sync_transport`'s fixation restart or `restart_stream_geometry`'s
/// geometry restart — swaps the whole value wholesale (stream id, slot
/// table, geometry, offered modifier), so no observer can read a
/// half-updated transport. Both writers run on the cast thread's single
/// main loop, which makes the swap atomic against every reader.
pub(crate) struct Transport {
    pub(crate) stream_id: u64,
    /// The target the live stream was started with; restarts reuse it.
    pub(crate) target: atrium_portal_ipc::StreamTarget,
    /// The cursor mode the live stream was started with; restarts reuse it.
    pub(crate) cursor: atrium_portal_ipc::StreamCursorMode,
    /// The dmabuf opt-in the live stream was started with.
    pub(crate) dmabuf: bool,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) announced: AnnouncedFormat,
    /// The modifier the PipeWire offer carries for this transport: present
    /// only while a dmabuf slot stream is announced. Fixated formats are
    /// validated against it; it changes only when the offer itself changes
    /// (a geometry restart re-offers the format).
    pub(crate) offered_modifier: Option<u64>,
    pub(crate) slot_files: Vec<atrium_portal_ipc::StreamSlot>,
    pub(crate) slot_bindings: Vec<SlotBinding>,
}

impl Transport {
    pub(crate) fn new(
        started: atrium_portal_ipc::StreamStarted,
        target: atrium_portal_ipc::StreamTarget,
        cursor: atrium_portal_ipc::StreamCursorMode,
        dmabuf: bool,
    ) -> Result<(Self, AnnouncedFormat), String> {
        let announced = announced_format(started.format)?;
        let slot_files = started.slots.unwrap_or_default();
        let slot_count = slot_files.len();
        let offered_modifier = match announced {
            AnnouncedFormat::Dmabuf { modifier, .. } if slot_count > 0 => Some(modifier),
            _ => None,
        };
        let transport = Self {
            stream_id: started.stream_id,
            target,
            cursor,
            dmabuf,
            width: started.width,
            height: started.height,
            announced,
            offered_modifier,
            slot_files,
            slot_bindings: (0..slot_count)
                .map(|_| SlotBinding {
                    pool: None,
                    in_flight: false,
                })
                .collect(),
        };
        Ok((transport, announced))
    }

    pub(crate) fn slot_count(&self) -> usize {
        self.slot_files.len()
    }

    /// True when the copy path may memory-map this transport's frames:
    /// CPU-typed SHM pixels or LINEAR dmabufs. A tiled dmabuf memory-maps
    /// to tile-swizzled bytes, so those frames must come from the
    /// compositor's SHM readback transport instead.
    pub(crate) fn cpu_mappable(&self) -> bool {
        match self.announced {
            AnnouncedFormat::Shm(_) => true,
            AnnouncedFormat::Dmabuf { modifier, .. } => modifier == DRM_FORMAT_MOD_LINEAR,
        }
    }

    /// Swap in a restarted compositor stream. The stopped stream's slots
    /// die with it compositor-side, so no releases are owed for them; the
    /// PipeWire pool is renegotiated separately and `remove_buffer` simply
    /// finds no binding for the old pool buffers.
    fn swap_restarted(
        &mut self,
        started: atrium_portal_ipc::StreamStarted,
        dmabuf: bool,
    ) -> Result<AnnouncedFormat, String> {
        let announced = announced_format(started.format)?;
        let slot_files = started.slots.unwrap_or_default();
        let slot_count = slot_files.len();
        self.stream_id = started.stream_id;
        self.width = started.width;
        self.height = started.height;
        self.dmabuf = dmabuf;
        self.announced = announced;
        self.slot_files = slot_files;
        self.slot_bindings = (0..slot_count)
            .map(|_| SlotBinding {
                pool: None,
                in_flight: false,
            })
            .collect();
        Ok(announced)
    }
}

/// Latest frame shared between the IPC source (writer) and the PipeWire
/// `process` callback (reader). `None` until the first frame arrives.
pub(crate) type LatestFrame = Rc<RefCell<Option<FramePayload>>>;

/// Stream-listener user data.
pub(crate) struct StreamData {
    pub(crate) latest: LatestFrame,
    /// Set when a new IPC frame has arrived but not yet been pushed to
    /// PipeWire. Cleared by the `process` callback once the frame is
    /// published (or dropped for good); a frame the starved pool could not
    /// take stays pending and is retried on a later cycle.
    pub(crate) pending: Rc<Cell<bool>>,
    /// The live compositor transport, including the stream's geometry and
    /// the PipeWire shape offered for it; swapped by `sync_transport` and
    /// by the geometry-restart path (see the invariant on [`Transport`]).
    pub(crate) transport: Rc<RefCell<Transport>>,
    pub(crate) negotiation: RefCell<Negotiation>,
    /// Unbound pool buffers dequeued in earlier cycles; the copy path
    /// fills them. A dequeued order (FIFO): taking the OLDEST returned
    /// buffer gives the consumer the longest window to observe the frame
    /// just queued before its buffer is rewritten. LIFO reuse (a plain
    /// Vec's tail pop) rewrites the same buffer the consumer most recently
    /// returned, collapsing the pool to an effective depth of one — on
    /// PipeWire 1.0.x that visibly drops every other frame of a
    /// continuous stream (the consumer reads the buffer only after the
    /// next write already replaced its contents).
    pub(crate) pool: RefCell<VecDeque<*mut pw_sys::pw_buffer>>,
    /// Portal-owned memfd backing for copy-path pool buffers, keyed by
    /// `pw_buffer` pointer. With `ALLOC_BUFFERS` the producer supplies the
    /// pool memory; entries are unmapped at `remove_buffer` and teardown.
    pub(crate) pool_mem: Rc<RefCell<HashMap<usize, PoolMem>>>,
    /// The IPC client, for slot releases and transport restarts.
    pub(crate) client: Rc<RefCell<Client>>,
    /// Quit handle for fatal transport errors.
    pub(crate) mainloop: pw::main_loop::MainLoopWeak,
    pub(crate) start_state: Rc<RefCell<StartState>>,
    /// Monotonic frame sequence counter attached to SPA_META_Header.
    pub(crate) sequence: Cell<u64>,
    /// Portal-side frame drops (unmappable dmabuf, pool starvation),
    /// counted for the stream's lifetime.
    pub(crate) dropped_frames: Cell<u64>,
    /// Rate-limit the unmappable-dmabuf warning to once per stream.
    pub(crate) warned_unmappable: Cell<bool>,
    /// Meta blocks per pool buffer, snapshotted in `add_buffer`.
    ///
    /// PipeWire 1.0.x (the Ubuntu 24.04 baseline) zeroes the `type` field
    /// of every `struct spa_meta` in a pool buffer's meta array once the
    /// buffer has been through a consumer-return round trip, so a reused
    /// buffer's `spa_buffer_find_meta` finds nothing and every subsequent
    /// frame would ship a zeroed header (sequence 0, PTS 0). The snapshot
    /// keys the same `meta.data` pointers by their original type, letting
    /// the publish path attach Header/VideoDamage to a reused buffer
    /// exactly as to a fresh one. Cleared by `remove_buffer`.
    pub(crate) buffer_metas: RefCell<BufferMetaSnapshots>,
}

/// Meta blocks of one pool buffer, keyed by SPA meta type: `(data, size)`.
pub(crate) type BufferMetaSnapshot = HashMap<u32, (usize, u32)>;
/// Every pool buffer's meta snapshot, keyed by `pw_buffer` pointer.
type BufferMetaSnapshots = HashMap<usize, BufferMetaSnapshot>;

/// Tell the compositor a slot is reusable. Best-effort: the stream's
/// teardown cleans up regardless.
pub(crate) fn release_slot(
    transport: &Rc<RefCell<Transport>>,
    client: &Rc<RefCell<Client>>,
    slot: u32,
) {
    let (stream_id, has_slots) = {
        let transport = transport.borrow();
        (transport.stream_id, !transport.slot_files.is_empty())
    };
    if !has_slots {
        return;
    }
    if let Err(error) = client.borrow_mut().release_stream_buffer(stream_id, slot) {
        log::debug!("portal: slot release for stream {stream_id} failed: {error}");
    }
}

/// Store a newly arrived frame as the stream's latest and mark it pending.
///
/// The overwrite drops the superseded frame; when that frame was never
/// published (still pending) and referenced a compositor slot, the slot
/// has no other release path — a published slot's release is owned by its
/// pool binding (reclaim) or already went out on the copy path. Release it
/// here, or the compositor's slot ring permanently shrinks by one and the
/// zero-copy stream degrades to frame drops. Every writer of `latest` that
/// stores a frame goes through this helper so the invariant cannot be
/// bypassed.
pub(crate) fn replace_latest(
    latest: &LatestFrame,
    pending: &Cell<bool>,
    transport: &Rc<RefCell<Transport>>,
    client: &Rc<RefCell<Client>>,
    payload: FramePayload,
) {
    let superseded = latest.borrow_mut().replace(payload);
    if pending.get()
        && let Some(FramePayload::Slot { slot, .. }) = superseded
    {
        release_slot(transport, client, slot);
    }
    pending.set(true);
}

impl StreamData {
    /// Tell the compositor a slot is reusable. Best-effort: the stream's
    /// teardown cleans up regardless.
    pub(crate) fn release_slot(&self, slot: u32) {
        release_slot(&self.transport, &self.client, slot);
    }
}

impl StreamData {
    /// Record the fixated format: verify it against what was offered,
    /// switch the compositor transport when the current one cannot serve
    /// it, and derive the delivery mode. Consumers can renegotiate
    /// mid-stream (OBS removes an unimportable modifier and retries), so
    /// this runs on every `Format` param change.
    pub(crate) fn apply_fixated_format(&self, fixated: &FixatedFormat) {
        {
            let transport = self.transport.borrow();
            let offered_spa = transport.announced.spa_format();
            if fixated.spa_format != offered_spa.as_raw() {
                log::warn!(
                    "portal: consumer fixated SPA format {} but only {} was offered",
                    fixated.spa_format,
                    offered_spa.as_raw()
                );
            }
            if fixated.width != transport.width || fixated.height != transport.height {
                log::warn!(
                    "portal: consumer fixated {}x{} but the compositor streams {}x{}",
                    fixated.width,
                    fixated.height,
                    transport.width,
                    transport.height
                );
            }
        }
        if let Err(error) = self.sync_transport(fixated.modifier) {
            log::error!("portal: compositor transport switch failed: {error}");
            if let Some(mainloop) = self.mainloop.upgrade() {
                mainloop.quit();
            }
            return;
        }
        let transport = self.transport.borrow();
        let (width, height) = (transport.width, transport.height);
        let mode = match (transport.announced, fixated.modifier) {
            (AnnouncedFormat::Dmabuf { modifier, .. }, Some(fixated_modifier))
                if fixated_modifier == modifier && !transport.slot_files.is_empty() =>
            {
                DeliveryMode::Dmabuf
            }
            (AnnouncedFormat::Dmabuf { modifier, .. }, Some(fixated_modifier))
                if !transport.slot_files.is_empty() =>
            {
                log::warn!(
                    "portal: consumer fixated modifier {fixated_modifier:#x} but the compositor streams {modifier:#x}; falling back to SHM delivery"
                );
                DeliveryMode::Shm
            }
            _ => DeliveryMode::Shm,
        };
        drop(transport);
        let mut negotiation = self.negotiation.borrow_mut();
        if negotiation.mode != mode {
            match mode {
                DeliveryMode::Dmabuf => log::info!(
                    "portal: pipewire consumer negotiated zero-copy dmabuf capture ({width}x{height})"
                ),
                DeliveryMode::Shm => log::info!(
                    "portal: pipewire consumer negotiated shared-memory capture ({width}x{height})"
                ),
            }
            negotiation.mode = mode;
        }
    }

    /// Restart the compositor stream on the transport the fixated PipeWire
    /// format needs: dmabuf slots when the consumer fixated the offered
    /// modifier, the compositor's SHM readback when it did not. A no-op
    /// when the current transport already serves the fixation — crucially,
    /// a LINEAR dmabuf transport stays, because memory-mapping it is
    /// exact. A tiled dmabuf transport never serves SHM consumers: the
    /// copy path would read tile-swizzled bytes, so the readback
    /// transport (which de-tiles on the GPU) takes over. The PipeWire
    /// stream itself is untouched: the offered format is identical on
    /// both transports, so the consumer never observes the switch.
    fn sync_transport(&self, fixated_modifier: Option<u64>) -> Result<(), String> {
        let (stream_id, needs_switch, want_dmabuf) = {
            let transport = self.transport.borrow();
            let want_dmabuf = matches!(
                (transport.offered_modifier, fixated_modifier),
                (Some(offered), Some(fixated)) if fixated == offered
            );
            let is_dmabuf = matches!(transport.announced, AnnouncedFormat::Dmabuf { .. });
            let needs = if want_dmabuf {
                !is_dmabuf
            } else {
                is_dmabuf && !transport.cpu_mappable()
            };
            (transport.stream_id, needs, want_dmabuf)
        };
        if !needs_switch {
            return Ok(());
        }
        let (target, cursor) = {
            let transport = self.transport.borrow();
            (transport.target.clone(), transport.cursor)
        };
        let mut client = self.client.borrow_mut();
        client
            .stop_output_stream(stream_id)
            .map_err(|e| format!("stop compositor stream {stream_id}: {e}"))?;
        let started = client
            .start_output_stream(Some(STREAM_MAX_FPS), target, want_dmabuf, Some(cursor))
            .map_err(|e| format!("restart compositor stream (dmabuf={want_dmabuf}): {e}"))?;
        drop(client);
        {
            let transport = self.transport.borrow();
            if started.width != transport.width || started.height != transport.height {
                return Err(format!(
                    "restarted stream geometry {}x{} differs from the negotiated {}x{}",
                    started.width, started.height, transport.width, transport.height
                ));
            }
            let expected_spa = transport.announced.spa_format();
            let announced = announced_format(started.format)?;
            if announced.spa_format() != expected_spa {
                return Err(format!(
                    "restarted stream format {announced:?} differs from the negotiated {expected_spa:?}"
                ));
            }
        }
        let slot_count = started.slots.as_ref().map_or(0, Vec::len);
        let mut transport = self.transport.borrow_mut();
        let announced = transport.swap_restarted(started, want_dmabuf)?;
        log::info!(
            "portal: restarted compositor stream {} as {} ({} slots)",
            transport.stream_id,
            match announced {
                AnnouncedFormat::Dmabuf { .. } => "dmabuf slots",
                AnnouncedFormat::Shm(_) => "shared-memory readback",
            },
            slot_count
        );
        drop(transport);
        // Frames of the superseded stream must never be published. This
        // clear bypasses `replace_latest` deliberately: the stopped
        // stream's slots die with it compositor-side, so no release is
        // owed for a pending slot frame here.
        *self.latest.borrow_mut() = None;
        self.pending.set(false);
        Ok(())
    }
}

/// Restart the live compositor stream after a `StreamGeometryChanged`
/// event: stop it, start it again with the SAME target, cursor mode, and
/// dmabuf opt-in, and require the new stream's geometry to match the
/// event exactly. On success the transport describes the new stream and
/// the caller re-offers the PipeWire format so the consumer re-fixates at
/// the new geometry. On failure the stream is left stopped and the caller
/// fails the cast.
///
/// Returns the restarted transport's announced format and slot count for
/// the PipeWire re-offer.
pub(crate) fn restart_stream_geometry(
    transport: &Rc<RefCell<Transport>>,
    client: &Rc<RefCell<Client>>,
    latest: &LatestFrame,
    pending: &Cell<bool>,
    stream_id: u64,
    width: u32,
    height: u32,
) -> Result<(AnnouncedFormat, usize), String> {
    let (current_id, target, cursor, dmabuf) = {
        let transport = transport.borrow();
        (
            transport.stream_id,
            transport.target.clone(),
            transport.cursor,
            transport.dmabuf,
        )
    };
    if current_id != stream_id {
        // An event for a superseded stream: nothing to restart.
        return Err(format!(
            "geometry change for superseded stream {stream_id} (live: {current_id})"
        ));
    }
    super::frame::frame_len(width, height)?;
    let mut client = client.borrow_mut();
    client
        .stop_output_stream(current_id)
        .map_err(|e| format!("stop compositor stream {current_id} for geometry change: {e}"))?;
    let started = client
        .start_output_stream(Some(STREAM_MAX_FPS), target, dmabuf, Some(cursor))
        .map_err(|e| format!("restart compositor stream at {width}x{height}: {e}"))?;
    drop(client);
    if started.width != width || started.height != height {
        return Err(format!(
            "restarted stream geometry {}x{} does not match the announced change to {width}x{height}",
            started.width, started.height
        ));
    }
    let mut transport = transport.borrow_mut();
    let announced = transport.swap_restarted(started, dmabuf)?;
    // The offer changes with the geometry: recompute the modifier the
    // consumer may fixate from the restarted transport.
    transport.offered_modifier = match announced {
        AnnouncedFormat::Dmabuf { modifier, .. } if transport.slot_count() > 0 => Some(modifier),
        _ => None,
    };
    let slot_count = transport.slot_count();
    log::info!(
        "portal: restarted compositor stream {} for the geometry change to {width}x{height} ({} slots)",
        transport.stream_id,
        slot_count
    );
    drop(transport);
    // The frozen pre-restart frame has the old geometry and the stopped
    // stream's slots die with it; clear without releases, exactly like
    // `sync_transport`'s supersede path.
    *latest.borrow_mut() = None;
    pending.set(false);
    Ok((announced, slot_count))
}
