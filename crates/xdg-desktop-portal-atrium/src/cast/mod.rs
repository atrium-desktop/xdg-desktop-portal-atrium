//! One PipeWire producer stream per started ScreenCast session.
//!
//! Each cast runs on its own thread: a scoped IPC connection receives the
//! compositor's output-frame stream and a PipeWire `Output` stream
//! republishes every new frame as raw video at the compositor's cadence.
//! The PipeWire main loop is also the IPC event loop — the IPC socket, the
//! stop socket, and the lease-renewal timer are ordinary loop sources, so
//! the thread never blocks anywhere but in `poll`.
//!
//! Two frame transports exist (see ADR-0005):
//!
//! - **Slot streaming (protocol 25)**: the compositor transfers a fixed set
//!   of dmabuf slot descriptors once at start, and each frame references a
//!   slot by index. The stream connects with `ALLOC_BUFFERS`, so every pool
//!   buffer's data slot is filled in the `add_buffer` event — bound to a
//!   compositor slot (`SPA_DATA_DmaBuf`) when the consumer negotiated the
//!   modifier-bearing format, or given a Portal-owned memfd otherwise.
//!   PipeWire fixes buffer descriptors at registration, which is why this
//!   is the only zero-copy shape that works; per-frame descriptors cannot
//!   be forwarded. A slot comes back when the consumer returns its buffer,
//!   and only then is its release reported to the compositor.
//! - **Per-frame descriptors**: every frame carries a sealed memfd or a
//!   single-plane dmabuf behind its header. The frame is memory-mapped and
//!   copied into a pool buffer exactly once. Memory-mapping is only defined
//!   for CPU-typed pixels, so this path serves SHM frames and LINEAR
//!   dmabufs; a tiled dmabuf would copy tile-swizzled bytes.
//!
//! The compositor's dmabuf slots normally carry a device-native tiled
//! modifier, which only GPU consumers can import. When the fixated
//! PipeWire format cannot be served by the current transport — a
//! modifier-ignorant consumer facing a tiled slot stream, or a GPU
//! consumer facing an SHM stream — the compositor stream is restarted on
//! the matching transport (the `dmabuf` flag at `StreamOutputStart`)
//! underneath the live PipeWire connection. The offered format is the same
//! on both transports, so the consumer never observes the switch.
//!
//! The stream is a PipeWire DRIVER fed by `pw_stream_trigger_process` when
//! a compositor frame arrives, so graph cycles normally run only when there
//! is a new frame. The latest frame is republished on the Streaming
//! transition: an early trigger fails with EIO before the link is up, and a
//! pause flushes queued buffers back to the producer.
//!
//! Frame arrival cannot be the only cycle source: `process` is what
//! reclaims consumer-returned pool buffers, and reclaiming is what releases
//! compositor dmabuf slots — while the compositor stops sending frames once
//! every slot is consumer-owned. A consumer that holds the whole pool (an
//! encoder's reorder lookahead, a slow reader) would wedge the stream in
//! that circular wait: no frames, so no reclaim, so no slots, so no frames.
//! A keepalive timer therefore triggers a cycle at a fixed cadence while
//! the stream is Streaming, so reclaim and slot releases always run and a
//! frame that failed to publish against a starved pool is retried.
//!
//! Teardown is single-path: closing the write end of the stop socket (or a
//! compositor-side `StreamEnded`, or any read error) quits the loop, after
//! which dropping the IPC client disconnects it — and the compositor's
//! disconnect cleanup stops the stream with no extra round-trip.
//!
//! A protocol-29 `StreamGeometryChanged` event (an output mode change, a
//! hotplug on a whole-desktop stream, or a window resize) freezes the
//! compositor stream until restarted. The cast loop restarts it with the
//! same target, cursor mode, and dmabuf opt-in, requires the restarted
//! stream's geometry to match the event, swaps the transport, and then
//! re-offers the PipeWire format at the new geometry so the consumer
//! re-fixates (the Buffers offer follows in the `Format` param callback,
//! which derives it from the swapped transport). A mismatched or failed
//! restart fails the stream cleanly.
//!
//! Buffers may carry `SPA_META_VideoDamage` metadata: the producer offers
//! it alongside the Buffers param and attaches each frame's compositor
//! damage rects to the buffer it publishes, so consumers can re-read only
//! what changed (see `cast::meta`).

mod copy;
mod format;
mod frame;
mod meta;
mod publish;
mod state;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use atrium_portal_ipc::{Client, StreamMessage};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use pw::spa::sys as spa_sys;
use pw::spa::utils::Direction;
use pw::stream::{StreamFlags, StreamState};

use copy::PoolMem;
use format::{
    SHM_POOL_BUFFERS, buffers_pod, format_pods, parse_buffers_data_types, parse_format_param,
};
use frame::{frame_len, validate_frame};
use meta::{damage_meta_pod, header_meta_pod};
use publish::process_frame;
use state::{
    DeliveryMode, LatestFrame, Negotiation, StreamData, Transport, replace_latest,
    restart_stream_geometry,
};

use crate::ipc;
use crate::screencast::CastJob;
use crate::session::CastSource;

/// Frame-rate ceiling requested from the compositor. The compositor paces
/// frames itself: while a stream is live, its due frames drive presentation
/// at the negotiated cadence (bounded by the output's vertical sync), so
/// frames flow even on a static screen; this is only a cap, and the server
/// clamps it to its own supported range.
pub(crate) const STREAM_MAX_FPS: u32 = 60;
/// A screencast publishes frames for PipeWire capture consumers.
const STREAM_DIRECTION: Direction = Direction::Output;
/// Lease TTL requested at handshake and renewal; renewed at half TTL.
const LEASE_TTL_MS: u64 = 900_000;
const IPC_TIMEOUT: Duration = Duration::from_secs(15);
/// Keepalive cadence of DRIVER cycles while the stream is Streaming.
/// Compositor frames trigger cycles directly; this timer exists for the
/// cycles no frame can ever trigger — buffer reclaim releases compositor
/// dmabuf slots, and the compositor stops sending frames while every slot
/// is consumer-owned, so without it a consumer holding the whole pool
/// wedges the stream in a circular wait (frozen picture until some external
/// event renegotiates the stream). The interval matches the stream's
/// maximum frame cadence, so recovery adds at most one frame of latency.
/// A responsive keepalive interval (4ms) ensures that consumer-returned
/// buffers are reclaimed and released back to the compositor with sub-frame
/// latency, preventing distributed feedback deadlocks.
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(4);

/// Negotiated parameters of a running cast, handed back to the worker once
/// the stream reaches `Paused` (the first state where the node id exists).
pub(crate) struct CastStarted {
    pub(crate) node_id: u32,
    pub(crate) serial: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// The source's origin in compositor (physical, desktop) coordinates:
    /// the selected connector's rect origin for per-output captures, or
    /// `(0, 0)` for whole-desktop and window captures (the wire carries no
    /// window position).
    pub(crate) position: (i32, i32),
}

/// Live cast handle owned by the session registry: drop `stop` to end the
/// cast, then `join` the thread before reporting the session closed.
pub(crate) struct CastHandle {
    pub(crate) stop: UnixStream,
    pub(crate) started: mpsc::Receiver<Result<CastStarted, String>>,
    pub(crate) thread: std::thread::JoinHandle<()>,
}

/// Spawn the cast thread. Returns immediately; the PipeWire negotiation
/// result arrives on `handle.started` exactly once. `source` and `cursor`
/// are the session's armed selection; every compositor stream the cast
/// starts (including transport and geometry restarts) reuses them.
pub(crate) fn spawn(
    socket: PathBuf,
    session_path: String,
    jobs: mpsc::SyncSender<CastJob>,
    source: CastSource,
    cursor: atrium_portal_ipc::StreamCursorMode,
) -> io::Result<CastHandle> {
    let (stop_read, stop_write) = UnixStream::pair()?;
    let (started_tx, started_rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("atrium-portal-cast".to_string())
        .spawn(move || {
            cast_thread(
                socket,
                session_path,
                jobs,
                source,
                cursor,
                stop_read,
                started_tx,
            )
        })?;
    Ok(CastHandle {
        stop: stop_write,
        started: started_rx,
        thread,
    })
}

/// `AsRawFd` wrapper so the shared IPC client can be a PipeWire loop source.
/// The timer and the IO callback both reach the client through the `Rc`;
/// PipeWire's `Fn` callbacks require interior mutability.
struct IpcFd(Rc<RefCell<Client>>);

impl AsRawFd for IpcFd {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.borrow().as_raw_fd()
    }
}

/// PipeWire reports the node id through the stream state callback and the
/// stable object serial through registry properties. Either event may arrive
/// first, so Start completes only after both have been observed.
pub(crate) struct StartState {
    started: Option<mpsc::Sender<Result<CastStarted, String>>>,
    paused: Option<(u32, u32, u32)>,
    serials: std::collections::HashMap<u32, u64>,
    position: (i32, i32),
    completed: bool,
}

impl StartState {
    fn try_complete(&mut self) {
        let Some((node_id, width, height)) = self.paused else {
            return;
        };
        let Some(serial) = self.serials.get(&node_id).copied() else {
            return;
        };
        if let Some(started) = self.started.take() {
            self.completed = true;
            let _ = started.send(Ok(CastStarted {
                node_id,
                serial,
                width,
                height,
                position: self.position,
            }));
        }
    }
}

fn cast_thread(
    socket: PathBuf,
    session_path: String,
    jobs: mpsc::SyncSender<CastJob>,
    source: CastSource,
    cursor: atrium_portal_ipc::StreamCursorMode,
    stop_read: UnixStream,
    started: mpsc::Sender<Result<CastStarted, String>>,
) {
    if let Err(error) = run_cast(
        &socket,
        &session_path,
        &jobs,
        &source,
        cursor,
        stop_read,
        &started,
    ) {
        log::warn!("portal: cast for {session_path} failed: {error}");
        let _ = started.send(Err(error));
    }
    // Dropping here closes the IPC connection; the compositor's disconnect
    // cleanup stops the output stream.
}

/// The whole cast, as a fallible sequence. `started` is consumed by the
/// stream listener on success, so on failure the caller still owns it.
fn run_cast(
    socket: &std::path::Path,
    session_path: &str,
    jobs: &mpsc::SyncSender<CastJob>,
    source: &CastSource,
    cursor: atrium_portal_ipc::StreamCursorMode,
    stop_read: UnixStream,
    started: &mpsc::Sender<Result<CastStarted, String>>,
) -> Result<(), String> {
    // Inward half first: without frames there is nothing to publish.
    let mut client = ipc::connect_compositor(socket, IPC_TIMEOUT)
        .map_err(|e| format!("compositor IPC connect: {e}"))?;
    let target = match source {
        CastSource::Monitor { output } => atrium_portal_ipc::StreamTarget::Output {
            output: output.clone(),
        },
        CastSource::Window { window } => {
            atrium_portal_ipc::StreamTarget::Window { window: *window }
        }
    };
    let stream_info = client
        .start_output_stream(Some(STREAM_MAX_FPS), target.clone(), true, Some(cursor))
        .map_err(|e| format!("start output stream: {e}"))?;
    let (width, height) = (stream_info.width, stream_info.height);
    // The Start result names the selected connector's origin in desktop
    // coordinates. Informational only: a failed lookup must not fail the
    // cast, and whole-desktop/window captures report (0, 0).
    let position = match source {
        CastSource::Monitor {
            output: Some(connector),
        } => client
            .enumerate_outputs()
            .ok()
            .and_then(|outputs| {
                outputs
                    .into_iter()
                    .find(|info| info.connector == *connector)
                    .map(|info| (info.rect.origin.x, info.rect.origin.y))
            })
            .unwrap_or((0, 0)),
        _ => (0, 0),
    };
    frame_len(width, height)?;
    let slot_count = stream_info.slots.as_ref().map_or(0, Vec::len);
    let (initial_transport, announced) = Transport::new(stream_info, target, cursor, true)?;
    let transport = Rc::new(RefCell::new(initial_transport));
    let teardown_transport = Rc::clone(&transport);
    let teardown_pool_mem: Rc<RefCell<HashMap<usize, PoolMem>>> =
        Rc::new(RefCell::new(HashMap::new()));
    {
        let stream_id = transport.borrow().stream_id;
        log::info!(
            "portal: compositor stream {stream_id} for {session_path}: {width}x{height}, format {announced:?}, {slot_count} slots",
        );
    }
    let client = Rc::new(RefCell::new(client));
    let teardown_client = Rc::clone(&client);

    // Outward half: PipeWire producer.
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context.connect_rc(None).map_err(|e| e.to_string())?;
    let registry = core.get_registry_rc().map_err(|e| e.to_string())?;
    // Use a reference-counted stream so the IPC source can wake the PipeWire
    // side with trigger_process when a new compositor frame arrives.
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "xdg-desktop-portal-atrium-screencast",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| e.to_string())?;

    let latest: LatestFrame = Rc::new(RefCell::new(None));
    let format_bytes = format_pods(width, height, announced, slot_count > 0);
    let pending = Rc::new(Cell::new(false));
    let start_state = Rc::new(RefCell::new(StartState {
        started: Some(started.clone()),
        paused: None,
        serials: std::collections::HashMap::new(),
        position,
        completed: false,
    }));
    let exit_start_state = Rc::clone(&start_state);
    let registry_start_state = Rc::clone(&start_state);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |object| {
            let Some(serial) = object
                .props
                .and_then(|properties| properties.get("object.serial"))
                .and_then(|serial| serial.parse::<u64>().ok())
            else {
                return;
            };
            let mut state = registry_start_state.borrow_mut();
            state.serials.insert(object.id, serial);
            state.try_complete();
        })
        .register();

    let state_loop_weak = mainloop.downgrade();
    // Set while the stream is Streaming; the keepalive timer only triggers
    // cycles in that state (a trigger before the link is up fails with EIO).
    let streaming = Rc::new(Cell::new(false));
    let state_streaming = Rc::clone(&streaming);
    let _listener = stream
        .add_local_listener_with_user_data(StreamData {
            latest: Rc::clone(&latest),
            pending: Rc::clone(&pending),
            transport: Rc::clone(&transport),
            negotiation: RefCell::new(Negotiation {
                mode: DeliveryMode::Shm,
                consumer_data_types: None,
            }),
            pool: RefCell::new(VecDeque::new()),
            pool_mem: Rc::clone(&teardown_pool_mem),
            client: Rc::clone(&client),
            mainloop: mainloop.downgrade(),
            start_state,
            sequence: Cell::new(0),
            buffer_metas: RefCell::new(HashMap::new()),
            dropped_frames: Cell::new(0),
            warned_unmappable: Cell::new(false),
        })
        .state_changed(move |stream, data, _old, new| {
            log::debug!("portal: pipewire stream {new:?}");
            state_streaming.set(new == StreamState::Streaming);
            if let StreamState::Error(message) = new {
                if let Some(started) = data.start_state.borrow_mut().started.take() {
                    let _ = started.send(Err(format!("PipeWire stream error: {message}")));
                }
                if let Some(mainloop) = state_loop_weak.upgrade() {
                    mainloop.quit();
                }
                return;
            }
            // Paused is the first state with a valid node id. The registry
            // normally announced its object.serial already; also consult the
            // stream properties in case this PipeWire version exposes it
            // there first.
            if new == StreamState::Paused {
                let node_id = stream.node_id();
                let mut state = data.start_state.borrow_mut();
                if let Some(serial) = stream
                    .properties()
                    .get("object.serial")
                    .and_then(|serial| serial.parse::<u64>().ok())
                {
                    state.serials.insert(node_id, serial);
                }
                let (width, height) = {
                    let transport = data.transport.borrow();
                    (transport.width, transport.height)
                };
                state.paused = Some((node_id, width, height));
                state.try_complete();
            }
            // A DRIVER stream only runs cycles when triggered, and a
            // trigger before the link is up fails with EIO. Once the stream
            // (re)reaches Streaming, republish the latest frame: early
            // frames were never delivered, and a pause flushed queued
            // buffers back to us.
            if new == StreamState::Streaming && data.latest.borrow().is_some() {
                data.pending.set(true);
                if let Err(error) = stream.trigger_process() {
                    log::debug!("portal: trigger_process failed: {error}");
                }
            }
        })
        .param_changed(|stream, data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id == spa::param::ParamType::Format.as_raw() {
                match parse_format_param(param) {
                    Some(fixated) => data.apply_fixated_format(&fixated),
                    None => log::warn!("portal: could not parse the fixated PipeWire format"),
                }
                // Advertise the layout delivery actually uses: the slot's
                // stride and size for zero-copy dmabuf, tightly packed for
                // the shared-memory copy path. The damage and header metadata
                // offers ride along (see `cast::meta`); a consumer that ignores
                // them simply gets buffers without the meta blocks.
                let (buffers, header_meta, damage_meta) = {
                    let transport = data.transport.borrow();
                    let mode = data.negotiation.borrow().mode;
                    let slot = if mode == DeliveryMode::Dmabuf {
                        transport.slot_files.first()
                    } else {
                        None
                    };
                    let buffers = match slot {
                        Some(slot) => buffers_pod(
                            transport.slot_files.len(),
                            slot.stride as i32,
                            slot.byte_len as i32,
                        ),
                        None => buffers_pod(
                            SHM_POOL_BUFFERS,
                            (transport.width * 4) as i32,
                            (transport.width * transport.height * 4) as i32,
                        ),
                    };
                    (buffers, header_meta_pod(), damage_meta_pod())
                };
                // Header first: PipeWire 1.0.x merges ParamMeta offers in
                // enumeration order and drops metas past an early
                // fixate/limit, so the order of this array decides whether
                // SPA_META_Header survives into the negotiated buffer
                // layout on the Ubuntu 24.04 baseline (PipeWire 1.0.5).
                // Without it every frame ships a zeroed header and
                // consumers read sequence 0 / PTS 0 forever.
                let mut params = [
                    Pod::from_bytes(&header_meta).expect("header meta pod"),
                    Pod::from_bytes(&damage_meta).expect("damage meta pod"),
                    Pod::from_bytes(&buffers).expect("buffers pod"),
                ];
                if let Err(error) = stream.update_params(&mut params) {
                    log::warn!("portal: pipewire update_params failed: {error}");
                }
            } else if id == spa::param::ParamType::Buffers.as_raw()
                && let Some(mask) = parse_buffers_data_types(param)
            {
                log::debug!("portal: consumer accepts buffer data types {mask:#05b}");
                data.negotiation.borrow_mut().consumer_data_types = Some(mask);
            }
        })
        .add_buffer(|_stream, data, buffer| {
            // Snapshot the intact meta array: PipeWire 1.0.x zeroes the
            // `type` fields after the first consumer return, and the
            // publish path resolves metas through this snapshot then.
            data.buffer_metas
                .borrow_mut()
                .insert(buffer as usize, unsafe { meta::snapshot_metas(buffer) });
            // With ALLOC_BUFFERS the producer fills each pool buffer's data
            // slot here, before the buffer registers with the consumer. A
            // dmabuf-negotiated slot stream binds the next compositor slot;
            // everything else gets a Portal-owned memfd for the copy path.
            let bind = if data.negotiation.borrow().forwarding_eligible() {
                let mut transport = data.transport.borrow_mut();
                transport
                    .slot_bindings
                    .iter_mut()
                    .enumerate()
                    .find(|(_, binding)| binding.pool.is_none())
                    .map(|(index, binding)| {
                        binding.pool = Some(buffer);
                        index
                    })
            } else {
                None
            };
            let Some(datas) = (unsafe {
                // SAFETY: `buffer` is a live pool buffer handed to this
                // stream by the add_buffer event; its spa_buffer is valid.
                let datas = (*(*buffer).buffer).datas;
                if datas.is_null() || (*datas).chunk.is_null() {
                    None
                } else {
                    Some(datas)
                }
            }) else {
                log::warn!("portal: pool buffer without data or chunk slots");
                return;
            };
            if let Some(index) = bind {
                let (fd, byte_len, stride) = {
                    let transport = data.transport.borrow();
                    let slot = &transport.slot_files[index];
                    (slot.file.as_raw_fd() as i64, slot.byte_len, slot.stride)
                };
                // SAFETY: `datas`/`chunk` are live pool slots patched here
                // before any use; they stay patched until remove_buffer.
                unsafe {
                    let chunk = (*datas).chunk;
                    (*datas).type_ = spa_sys::SPA_DATA_DmaBuf;
                    (*datas).flags = spa::buffer::DataFlags::READABLE.bits();
                    (*datas).fd = fd;
                    (*datas).mapoffset = 0;
                    (*datas).maxsize = byte_len as u32;
                    (*datas).data = std::ptr::null_mut();
                    (*chunk).offset = 0;
                    (*chunk).size = byte_len as u32;
                    (*chunk).stride = stride as i32;
                    (*chunk).flags = spa_sys::SPA_CHUNK_FLAG_NONE as i32;
                }
                log::debug!("portal: bound compositor slot {index} to a pool buffer");
                return;
            }
            let frame_bytes = {
                let transport = data.transport.borrow();
                transport.width as usize * transport.height as usize * 4
            };
            match PoolMem::new(frame_bytes) {
                Ok(mem) => {
                    let stride = data.transport.borrow().width * 4;
                    // SAFETY: as above; the memfd stays owned by
                    // `pool_mem` until remove_buffer/teardown, and `map`
                    // points at its pages.
                    unsafe {
                        let chunk = (*datas).chunk;
                        (*datas).type_ = spa_sys::SPA_DATA_MemFd;
                        (*datas).flags =
                            spa_sys::SPA_DATA_FLAG_READABLE | spa_sys::SPA_DATA_FLAG_MAPPABLE;
                        (*datas).fd = mem.file.as_raw_fd() as i64;
                        (*datas).mapoffset = 0;
                        (*datas).maxsize = mem.len as u32;
                        (*datas).data = mem.map.cast();
                        (*chunk).offset = 0;
                        (*chunk).size = 0;
                        (*chunk).stride = stride as i32;
                        (*chunk).flags = spa_sys::SPA_CHUNK_FLAG_NONE as i32;
                    }
                    data.pool_mem.borrow_mut().insert(buffer as usize, mem);
                }
                Err(error) => {
                    log::warn!("portal: could not allocate a pool buffer: {error}");
                }
            }
        })
        .remove_buffer(|_stream, data, buffer| {
            // The pool is being torn down (renegotiation or stop): drop the
            // binding and release any in-flight slot the consumer abandoned.
            data.buffer_metas.borrow_mut().remove(&(buffer as usize));
            data.pool_mem.borrow_mut().remove(&(buffer as usize));
            data.pool.borrow_mut().retain(|pooled| *pooled != buffer);
            let mut transport = data.transport.borrow_mut();
            let Some((slot, binding)) = transport
                .slot_bindings
                .iter_mut()
                .enumerate()
                .find(|(_, binding)| binding.pool == Some(buffer))
            else {
                return;
            };
            binding.pool = None;
            let was_in_flight = std::mem::replace(&mut binding.in_flight, false);
            let stream_id = transport.stream_id;
            let has_slots = !transport.slot_files.is_empty();
            let client = Rc::clone(&data.client);
            drop(transport);
            if was_in_flight
                && has_slots
                && let Err(error) = client
                    .borrow_mut()
                    .release_stream_buffer(stream_id, slot as u32)
            {
                log::debug!("portal: slot release failed: {error}");
            }
        })
        .process(process_frame)
        .register()
        .map_err(|e| e.to_string())?;

    // IPC frames arrive as a loop source; the lease timer keeps the scoped
    // connection alive across quiet periods.
    let loop_weak = mainloop.downgrade();
    let stream_weak = stream.downgrade();
    let compositor_dropped = Rc::new(Cell::new(0_u64));
    let _ipc_source = mainloop.loop_().add_io(
        IpcFd(Rc::clone(&client)),
        spa::support::system::IoFlags::IN | spa::support::system::IoFlags::ERR,
        {
            let jobs = jobs.clone();
            let session_path = session_path.to_string();
            let compositor_dropped = Rc::clone(&compositor_dropped);
            let transport = Rc::clone(&transport);
            let latest = Rc::clone(&latest);
            let pending = Rc::clone(&pending);
            move |io| {
                let message = io.0.borrow_mut().next_stream_message();
                match message {
                    Ok(StreamMessage::Frame(frame)) => {
                        if frame.dropped != compositor_dropped.get() {
                            log::debug!(
                                "portal: compositor reports {} backpressure-dropped stream frames",
                                frame.dropped
                            );
                            compositor_dropped.set(frame.dropped);
                        }
                        let state = transport.borrow();
                        if frame.stream_id != state.stream_id {
                            // A transport switch left frames of the
                            // superseded stream on the wire.
                            log::debug!(
                                "portal: ignoring frame for superseded stream {}",
                                frame.stream_id
                            );
                            return;
                        }
                        match validate_frame(
                            frame,
                            state.width,
                            state.height,
                            state.announced,
                            state.slot_count(),
                        ) {
                            Ok(payload) => {
                                drop(state);
                                replace_latest(&latest, &pending, &transport, &io.0, payload);
                                if let Some(stream) = stream_weak.upgrade()
                                    && let Err(error) = stream.trigger_process()
                                {
                                    log::debug!("portal: trigger_process failed: {error}");
                                }
                            }
                            Err(reason) => {
                                log::debug!("portal: ignoring stream frame: {reason}");
                            }
                        }
                    }
                    Ok(StreamMessage::LeaseRenewed) => {}
                    Ok(StreamMessage::GeometryChanged {
                        stream_id,
                        width,
                        height,
                    }) => {
                        // The compositor sends no further frames until the
                        // stream is restarted: restart it with the same
                        // target, cursor mode, and dmabuf opt-in, then
                        // re-offer the PipeWire format at the new geometry
                        // so the consumer re-fixates. The Buffers offer
                        // follows in `param_changed`, which derives it from
                        // the swapped transport.
                        if stream_id != transport.borrow().stream_id {
                            log::debug!(
                                "portal: ignoring geometry change for superseded stream {stream_id}"
                            );
                            return;
                        }
                        match restart_stream_geometry(
                            &transport,
                            &io.0,
                            &latest,
                            &pending,
                            stream_id,
                            width,
                            height,
                        ) {
                            Ok((announced, slot_count)) => {
                                log::info!(
                                    "portal: stream {stream_id} geometry changed to {width}x{height}; renegotiating"
                                );
                                if let Some(stream) = stream_weak.upgrade() {
                                    let format_bytes =
                                        format_pods(width, height, announced, slot_count > 0);
                                    let mut params: Vec<&Pod> = format_bytes
                                        .iter()
                                        .map(|bytes| Pod::from_bytes(bytes).expect("format pod"))
                                        .collect();
                                    if let Err(error) = stream.update_params(&mut params) {
                                        log::warn!(
                                            "portal: pipewire re-offer after the geometry change failed: {error}"
                                        );
                                    }
                                }
                            }
                            Err(error) => {
                                // A mismatched or failed restart cannot be
                                // served honestly; fail the stream cleanly.
                                log::warn!(
                                    "portal: geometry restart for {session_path} failed: {error}"
                                );
                                end_cast(&jobs, &session_path, &loop_weak);
                            }
                        }
                    }
                    Ok(StreamMessage::Ended { reason, .. }) => {
                        log::info!("portal: compositor ended stream for {session_path}: {reason}");
                        end_cast(&jobs, &session_path, &loop_weak);
                    }
                    Err(error) => {
                        log::warn!("portal: stream read for {session_path} failed: {error}");
                        end_cast(&jobs, &session_path, &loop_weak);
                    }
                }
            }
        },
    );

    let stopped_by_owner = Rc::new(Cell::new(false));
    let stop_flag = Rc::clone(&stopped_by_owner);
    let _stop_source = mainloop.loop_().add_io(
        stop_read,
        spa::support::system::IoFlags::IN | spa::support::system::IoFlags::HUP,
        {
            let loop_weak = mainloop.downgrade();
            move |socket| {
                stop_flag.set(true);
                let mut byte = [0u8; 1];
                use std::io::Read;
                // EOF (write end dropped) is the only expected readability.
                let _ = socket.read(&mut byte);
                if let Some(mainloop) = loop_weak.upgrade() {
                    mainloop.quit();
                }
            }
        },
    );

    let lease_timer = mainloop.loop_().add_timer(move |_| {
        if let Err(error) = client.borrow_mut().request_lease_renewal(LEASE_TTL_MS) {
            log::warn!("portal: cast lease renewal failed: {error}");
        }
    });
    let half_ttl = Duration::from_millis(LEASE_TTL_MS / 2);
    lease_timer.update_timer(Some(half_ttl), Some(half_ttl));

    // The keepalive is the cycle source of last resort: while Streaming, it
    // runs `process` only when work is pending (a frame held back by pool
    // starvation or in-flight slot buffers needing reclaim upon consumer return).
    // On a static screen with all buffers reclaimed, it remains idle without
    // triggering redundant PipeWire graph cycles.
    let keepalive_stream = stream.downgrade();
    let keepalive_pending = Rc::clone(&pending);
    let keepalive_transport = Rc::clone(&transport);
    let keepalive_timer = mainloop.loop_().add_timer(move |_| {
        if !streaming.get() {
            return;
        }
        let needs_cycle = {
            let is_pending = keepalive_pending.get();
            let transport = keepalive_transport.borrow();
            let has_in_flight_slots = transport.slot_bindings.iter().any(|b| b.in_flight);
            is_pending || has_in_flight_slots
        };
        if !needs_cycle {
            return;
        }
        let Some(stream) = keepalive_stream.upgrade() else {
            return;
        };
        if let Err(error) = stream.trigger_process() {
            log::debug!("portal: keepalive trigger_process failed: {error}");
        }
    });
    keepalive_timer.update_timer(Some(KEEPALIVE_INTERVAL), Some(KEEPALIVE_INTERVAL));

    // The Param:Meta offers (Header PTS/sequence, VideoDamage) are declared
    // up front with the format at connect() — not only re-offered from
    // param_changed(Format). Buffer allocation runs the link's meta
    // negotiation against whatever the port enumerates at that moment;
    // declaring the metas only after the format fixates loses the race on
    // PipeWire 1.0.x (Ubuntu 24.04 LTS), where the negotiated buffer
    // layout then carries no meta blocks at all: the producer's
    // spa_buffer_find_meta returns NULL, every published frame ships a
    // zeroed header, and consumers see sequence 0 forever.
    let header_meta_bytes = header_meta_pod();
    let damage_meta_bytes = damage_meta_pod();
    let mut format_refs: Vec<&Pod> = format_bytes
        .iter()
        .map(|bytes| Pod::from_bytes(bytes).expect("format pod"))
        .collect();
    for bytes in [&header_meta_bytes, &damage_meta_bytes] {
        format_refs.push(Pod::from_bytes(bytes).expect("meta pod"));
    }
    stream
        .connect(
            // This stream publishes compositor frames. `Input` describes a
            // capture consumer (for example a camera reader) and leaves OBS
            // with no producer port to link to; a screencast source must
            // expose an output port.
            STREAM_DIRECTION,
            None,
            // Portal consumers explicitly target the node returned by
            // Start/OpenPipeWireRemote. AUTOCONNECT asks session-manager
            // policy to route this source to an unrelated default target and
            // can tear the stream down with "no target node available".
            // ALLOC_BUFFERS lets the producer attach each pool buffer's
            // memory in the add_buffer event (the only way a consumer ever
            // sees a dmabuf descriptor: buffer contents are fixed at
            // registration), and DRIVER pairs it with trigger_process so
            // cycles run only when a compositor frame arrives.
            StreamFlags::MAP_BUFFERS | StreamFlags::ALLOC_BUFFERS | StreamFlags::DRIVER,
            &mut format_refs,
        )
        .map_err(|e| e.to_string())?;

    mainloop.run();
    // Teardown: unmap pool backing, then release slots the consumer still
    // holds so the compositor can reuse them without waiting for the
    // disconnect cleanup.
    teardown_pool_mem.borrow_mut().clear();
    let transport_state = teardown_transport.borrow();
    for (slot, binding) in transport_state.slot_bindings.iter().enumerate() {
        if binding.in_flight
            && let Err(error) = teardown_client
                .borrow_mut()
                .release_stream_buffer(transport_state.stream_id, slot as u32)
        {
            log::debug!("portal: teardown slot release failed: {error}");
        }
    }
    drop(transport_state);
    if exit_start_state.borrow().completed && !stopped_by_owner.get() {
        let _ = jobs.send(CastJob::SessionEnded {
            session_path: session_path.to_owned(),
        });
    }
    Ok(())
}

/// Report a compositor-side stream end to the screencast worker and stop
/// the cast thread's loop.
fn end_cast(
    jobs: &mpsc::SyncSender<CastJob>,
    session_path: &str,
    loop_weak: &pw::main_loop::MainLoopWeak,
) {
    let _ = jobs.send(CastJob::SessionEnded {
        session_path: session_path.to_string(),
    });
    if let Some(mainloop) = loop_weak.upgrade() {
        mainloop.quit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::FromRawFd;

    use atrium_portal_ipc::{StreamFrame, StreamPayload};
    use pipewire::spa::pod;
    use pipewire::spa::utils::{Choice, ChoiceEnum};

    use super::copy::copy_rows;
    use super::format::{
        ALL_DATA_TYPES, DRM_FORMAT_MOD_LINEAR, FRAMERATE_DEFAULT, FRAMERATE_MAX, FRAMERATE_MIN,
        FixatedFormat, announced_format, buffers_pod, choice_default, format_pod, fourcc,
        parse_format_param, spa_format_for_drm,
    };
    use super::frame::{FramePayload, validate_frame};
    use super::meta::{MAX_DAMAGE_REGIONS, damage_meta_pod, parse_meta_pod};

    fn pod_words(bytes: &[u8]) -> &[u32] {
        let (head, words, tail) = unsafe { bytes.align_to::<u32>() };
        assert!(head.is_empty() && tail.is_empty());
        words
    }

    fn parse_pod(bytes: &[u8]) -> pod::Object {
        let parsed = pod::deserialize::PodDeserializer::deserialize_from::<pod::Value>(bytes)
            .expect("deserialize")
            .1;
        let pod::Value::Object(object) = parsed else {
            panic!("expected an object pod");
        };
        object
    }

    fn unsealed_memfd(bytes: &[u8]) -> File {
        // SAFETY: the name is static and NUL-terminated; the returned fd is
        // checked before ownership is constructed.
        let fd = unsafe { libc::memfd_create(c"tessera-cast-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create: {}", io::Error::last_os_error());
        // SAFETY: `fd` is a new owned descriptor from memfd_create.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(bytes).unwrap();
        file
    }

    fn frame_for_test(format: atrium_portal_ipc::StreamPixelFormat) -> StreamFrame {
        StreamFrame {
            stream_id: 1,
            sequence: 1,
            width: 2,
            height: 2,
            stride: 8,
            format,
            damage: Vec::new(),
            dropped: 0,
            slot: None,
            payload: StreamPayload::Memfd(unsealed_memfd(&[0; 16])),
        }
    }

    #[test]
    fn screencast_stream_is_a_pipewire_producer() {
        assert!(matches!(STREAM_DIRECTION, Direction::Output));
    }

    #[test]
    fn drm_fourccs_map_to_spa_formats_in_memory_order() {
        use spa::param::video::VideoFormat;
        let cases = [
            (fourcc(b'X', b'R', b'2', b'4'), VideoFormat::BGRx),
            (fourcc(b'A', b'R', b'2', b'4'), VideoFormat::BGRA),
            (fourcc(b'X', b'B', b'2', b'4'), VideoFormat::RGBx),
            (fourcc(b'A', b'B', b'2', b'4'), VideoFormat::RGBA),
            (fourcc(b'R', b'X', b'2', b'4'), VideoFormat::xBGR),
            (fourcc(b'R', b'A', b'2', b'4'), VideoFormat::ABGR),
            (fourcc(b'B', b'X', b'2', b'4'), VideoFormat::xRGB),
            (fourcc(b'B', b'A', b'2', b'4'), VideoFormat::ARGB),
        ];
        for (drm, spa_format) in cases {
            assert_eq!(spa_format_for_drm(drm), Some(spa_format), "{drm:#010x}");
        }
        assert_eq!(spa_format_for_drm(fourcc(b'N', b'V', b'1', b'2')), None);
    }

    #[test]
    fn format_pod_is_a_parseable_video_format_with_a_framerate_range() {
        let bytes = format_pod(1920, 1080, spa::param::video::VideoFormat::BGRx, None);
        assert!(Pod::from_bytes(&bytes).is_some(), "pod parses");
        let words = pod_words(&bytes);
        // Pod header: body size, then SPA_TYPE_Object; the object body
        // carries type (ObjectParamFormat) and id (EnumFormat).
        assert_eq!(words[0] as usize, bytes.len() - 8);
        assert_eq!(words[2], spa::utils::SpaTypes::ObjectParamFormat.as_raw());
        assert_eq!(words[3], spa::param::ParamType::EnumFormat.as_raw());
        let object = parse_pod(&bytes);
        assert_eq!(object.properties.len(), 5);
        // The framerate is a range so each consumer paces against its own
        // clock; frames arrive at the compositor's actual cadence.
        let framerate = object
            .properties
            .iter()
            .find(|p| p.key == spa::param::format::FormatProperties::VideoFramerate.as_raw())
            .and_then(|p| match &p.value {
                pod::Value::Choice(pod::ChoiceValue::Fraction(choice)) => Some(choice),
                _ => None,
            })
            .expect("framerate property");
        assert_eq!(choice_default(framerate), FRAMERATE_DEFAULT);
        let Choice(_, ChoiceEnum::Range { min, max, .. }) = framerate else {
            panic!("framerate must be a range");
        };
        assert_eq!(*min, FRAMERATE_MIN);
        assert_eq!(*max, FRAMERATE_MAX);
    }

    #[test]
    fn buffers_pod_covers_one_frame_and_every_data_type() {
        let bytes = buffers_pod(0, 640 * 4, 640 * 480 * 4);
        let object = parse_pod(&bytes);
        assert_eq!(
            object.type_,
            spa::utils::SpaTypes::ObjectParamBuffers.as_raw()
        );
        let size = object
            .properties
            .iter()
            .find(|p| p.key == 3)
            .and_then(|p| match p.value {
                pod::Value::Int(size) => Some(size),
                _ => None,
            })
            .expect("size property");
        assert_eq!(size, 640 * 480 * 4);
        let data_types = object
            .properties
            .iter()
            .find(|property| property.key == 6)
            .and_then(|property| match &property.value {
                pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                    _,
                    ChoiceEnum::Flags { default, flags },
                ))) => Some((*default, flags.as_slice())),
                _ => None,
            })
            .expect("dataType flags property");
        assert_eq!(data_types, (ALL_DATA_TYPES as i32, &[][..]));
    }

    #[test]
    fn format_param_parsing_tolerates_plain_and_choice_values() {
        let fixated = parse_format_param(
            Pod::from_bytes(&format_pod(
                640,
                480,
                spa::param::video::VideoFormat::BGRx,
                None,
            ))
            .expect("pod"),
        )
        .expect("parseable format pod");
        assert_eq!(
            fixated,
            FixatedFormat {
                spa_format: spa::param::video::VideoFormat::BGRx.as_raw(),
                width: 640,
                height: 480,
                modifier: None,
            }
        );
    }

    #[test]
    fn shm_frames_validate_against_the_announced_geometry() {
        let announced = announced_format(atrium_portal_ipc::StreamPixelFormat::Bgra8).unwrap();
        let frame = frame_for_test(atrium_portal_ipc::StreamPixelFormat::Bgra8);
        assert!(validate_frame(frame, 2, 2, announced, 0).is_ok());

        let wrong_geometry = StreamFrame {
            width: 4,
            payload: StreamPayload::Memfd(unsealed_memfd(&[0; 32])),
            ..frame_for_test(atrium_portal_ipc::StreamPixelFormat::Bgra8)
        };
        assert!(validate_frame(wrong_geometry, 2, 2, announced, 0).is_err());
    }

    #[test]
    fn dmabuf_frames_allow_padded_strides_but_pin_the_modifier() {
        let announced = announced_format(atrium_portal_ipc::StreamPixelFormat::Dmabuf {
            drm_format: fourcc(b'X', b'R', b'2', b'4'),
            modifier: DRM_FORMAT_MOD_LINEAR,
        })
        .unwrap();
        let frame = StreamFrame {
            stride: 16, // padded: 2 pixels wide would only need 8
            payload: StreamPayload::Dmabuf(unsealed_memfd(&[0; 32])),
            ..frame_for_test(atrium_portal_ipc::StreamPixelFormat::Dmabuf {
                drm_format: fourcc(b'X', b'R', b'2', b'4'),
                modifier: DRM_FORMAT_MOD_LINEAR,
            })
        };
        assert!(matches!(
            validate_frame(frame, 2, 2, announced, 0),
            Ok(FramePayload::Descriptor { stride: 16, .. })
        ));

        let wrong_modifier = frame_for_test(atrium_portal_ipc::StreamPixelFormat::Dmabuf {
            drm_format: fourcc(b'X', b'R', b'2', b'4'),
            modifier: 4,
        });
        assert!(validate_frame(wrong_modifier, 2, 2, announced, 0).is_err());
    }

    /// The copy path memory-maps the frame descriptor, so a tiled dmabuf
    /// can never enter it: mapping it would copy tile-swizzled bytes into
    /// the consumer's buffers. Tiled streams are served by the
    /// compositor's SHM readback transport instead.
    #[test]
    fn non_linear_dmabuf_frames_never_enter_the_copy_path() {
        let announced = announced_format(atrium_portal_ipc::StreamPixelFormat::Dmabuf {
            drm_format: fourcc(b'X', b'R', b'2', b'4'),
            modifier: 0x0100_0000_0000_0001, // DRM_FORMAT_MOD_I915_X_TILED
        })
        .unwrap();
        let frame = StreamFrame {
            stride: 16,
            payload: StreamPayload::Dmabuf(unsealed_memfd(&[0; 32])),
            ..frame_for_test(atrium_portal_ipc::StreamPixelFormat::Dmabuf {
                drm_format: fourcc(b'X', b'R', b'2', b'4'),
                modifier: 0x0100_0000_0000_0001,
            })
        };
        let Err(reason) = validate_frame(frame, 2, 2, announced, 0) else {
            panic!("a tiled dmabuf frame must not validate for the copy path");
        };
        assert!(reason.contains("non-LINEAR"), "{reason}");
    }

    #[test]
    fn copy_rows_handles_tight_and_padded_strides() {
        let src: Vec<u8> = (0..64).collect();
        let mut dest = vec![0_u8; 16];
        copy_rows(&src, 8, &mut dest, 8, 2);
        assert_eq!(dest, src[..16]);

        let mut dest = vec![0_u8; 8];
        copy_rows(&src, 16, &mut dest, 4, 2);
        assert_eq!(dest, [0, 1, 2, 3, 16, 17, 18, 19]);
    }

    #[test]
    fn damage_meta_offer_is_video_damage_with_region_capacity() {
        let bytes = damage_meta_pod();
        let (meta_type, size) = parse_meta_pod(&bytes).expect("parseable meta pod");
        assert_eq!(meta_type, spa_sys::SPA_META_VideoDamage);
        assert_eq!(
            size as usize,
            MAX_DAMAGE_REGIONS * std::mem::size_of::<spa_sys::spa_meta_region>()
        );
    }
}
