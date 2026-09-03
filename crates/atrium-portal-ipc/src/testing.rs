//! Minimal independent Tessera IPC server for Portal integration tests.

use std::collections::HashMap;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::blob::SealedBlob;
use crate::codec::{read_msg, write_msg};
use crate::schema::{Request, Response, valid_wallpaper_path};
use crate::{
    ConfirmPickResult, ConnectionCapabilities, Event, LOCAL_PORTAL_SCOPE, LeaseGrant, OutputInfo,
    PROTOCOL_VERSION, PickKind, PickResult, Rect, SettingsSnapshot, StreamCursorMode,
    StreamPixelFormat, StreamTarget,
};

pub struct CaptureOutputPayload {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

#[derive(Debug)]
pub struct StreamInfo {
    pub stream_id: u64,
    pub width: u32,
    pub height: u32,
    pub format: StreamPixelFormat,
    /// dmabuf slot table (protocol 25): each fd is sent to the client after
    /// the reply, in slot order. A memfd stands in for a GPU buffer in
    /// tests.
    pub slots: Option<Vec<StreamSlotInfo>>,
}

#[derive(Debug)]
pub struct StreamSlotInfo {
    pub fd: std::os::fd::RawFd,
    pub stride: u32,
    pub byte_len: u64,
}

pub struct StreamFramePayload {
    pub stream_id: u64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: StreamPixelFormat,
    pub damage: Vec<Rect>,
    pub dropped: u64,
    pub pixels: Arc<[u8]>,
}

/// A frame announced together with a caller-supplied descriptor. This
/// mirrors the compositor's dmabuf stream transport: the same `StreamFrame`
/// header, but the descriptor is sent unsealed because dmabufs cannot carry
/// memfd seals. Tests supply the descriptor (a plain memfd stands in for a
/// GPU buffer on machines without a render node).
pub struct StreamFrameFdPayload {
    pub stream_id: u64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: StreamPixelFormat,
    pub damage: Vec<Rect>,
    pub dropped: u64,
    pub byte_len: u64,
}

pub trait Handler: Send + Sync + 'static {
    fn settings(&self) -> SettingsSnapshot {
        SettingsSnapshot::default()
    }

    fn capture_output(&self, _region: Option<Rect>) -> Result<CaptureOutputPayload, String> {
        Err("capture is not implemented by this test server".into())
    }

    /// Protocol-29 output enumeration.
    fn enumerate_outputs(&self) -> Result<Vec<OutputInfo>, String> {
        Err("output enumeration is not implemented by this test server".into())
    }

    fn pick_target(&self, _connection: u64, _kind: PickKind) -> Result<PickResult, String> {
        Err("target picking is not implemented by this test server".into())
    }

    fn pick_confirm(
        &self,
        _connection: u64,
        _title: String,
        _body: String,
        _accept_label: Option<String>,
    ) -> Result<ConfirmPickResult, String> {
        Err("confirmation is not implemented by this test server".into())
    }

    fn stream_output_start(
        &self,
        _connection: u64,
        _max_fps: Option<u32>,
        _target: StreamTarget,
        _dmabuf: Option<bool>,
        _cursor: Option<StreamCursorMode>,
    ) -> Result<StreamInfo, String> {
        Err("streaming is not implemented by this test server".into())
    }

    /// Frames written to the connection BEFORE the `StreamOutputStarted`
    /// reply. The real compositor publishes the stream lane before it queues
    /// the reply, so an already-produced frame can legitimately precede it;
    /// tests use this hook to reproduce that race deterministically.
    fn frames_before_started(&self, _stream_id: u64) -> Vec<StreamFramePayload> {
        Vec::new()
    }

    fn stream_output_stop(&self, _stream_id: u64) {}

    /// Protocol-25 slot release from the client.
    fn stream_buffer_release(&self, _stream_id: u64, _slot: u32) {}

    /// Lease policy: whether a `Hello` lease request is granted. Mirrors the
    /// compositor's policy decision; returning `false` leaves the connection
    /// leaseless, so privileged ops fail their live-lease gate.
    fn grant_lease(&self) -> bool {
        true
    }

    /// Extra scope names the server recognizes beyond the portal scope. They
    /// handshake cleanly but carry no operations, so tests can exercise the
    /// explicit-scope-op gates (the real dispatch's "out of scope" path).
    fn known_scopes(&self) -> &'static [&'static str] {
        &[]
    }

    /// Session lock state for mutation gates, mirroring the compositor's
    /// lock/VT check. Default: the session is active.
    fn session_active(&self) -> bool {
        true
    }

    /// Wallpaper swap (the `SetWallpaper` op, gated like the real dispatch:
    /// control, a live lease, an explicit scope op, a valid path, and an
    /// active session). `path` names the staged image file; the reply is the
    /// decode-and-swap receipt.
    fn set_wallpaper(&self, _connection: u64, _path: PathBuf) -> Result<(), String> {
        Err("wallpaper is not implemented by this test server".into())
    }

    fn streams_disconnected(&self, _connection: u64) {}
}

type SharedWriter = Arc<Mutex<UnixStream>>;
type Streams = Arc<Mutex<HashMap<u64, (u64, SharedWriter)>>>;

pub struct Server {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<UnixStream>>>,
    streams: Streams,
    accept_thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Start a server that speaks the newest protocol and negotiates older
    /// clients down, like the current compositor.
    pub fn start<H>(path: &Path, handler: Arc<H>) -> io::Result<Self>
    where
        H: Handler,
    {
        Self::start_inner(path, handler, None)
    }

    /// Start a server that only accepts exactly `version`, like a
    /// compositor from before protocol down-negotiation.
    pub fn start_legacy<H>(path: &Path, handler: Arc<H>, version: u32) -> io::Result<Self>
    where
        H: Handler,
    {
        Self::start_inner(path, handler, Some(version))
    }

    fn start_inner<H>(path: &Path, handler: Arc<H>, exact_version: Option<u32>) -> io::Result<Self>
    where
        H: Handler,
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let next_connection = Arc::new(AtomicU64::new(1));

        let thread_stop = Arc::clone(&stop);
        let thread_connections = Arc::clone(&connections);
        let thread_streams = Arc::clone(&streams);
        let accept_thread = std::thread::Builder::new()
            .name("atrium-portal-ipc-test-listener".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Ok(guard) = stream.try_clone() {
                                thread_connections.lock().unwrap().push(guard);
                            }
                            let connection = next_connection.fetch_add(1, Ordering::Relaxed);
                            let connection_handler: Arc<dyn Handler> = handler.clone();
                            let connection_streams = Arc::clone(&thread_streams);
                            let _ = std::thread::Builder::new()
                                .name(format!("atrium-portal-ipc-test-{connection}"))
                                .spawn(move || {
                                    serve_connection(
                                        stream,
                                        connection,
                                        connection_handler,
                                        connection_streams,
                                        exact_version,
                                    );
                                });
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })?;

        Ok(Self {
            path: path.to_path_buf(),
            stop,
            connections,
            streams,
            accept_thread: Some(accept_thread),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn push_stream_frame(&self, frame: StreamFramePayload) -> bool {
        let writer = self
            .streams
            .lock()
            .unwrap()
            .get(&frame.stream_id)
            .map(|(_, writer)| Arc::clone(writer));
        let Some(writer) = writer else {
            return false;
        };
        let Ok(blob) = SealedBlob::new(&frame.pixels) else {
            return false;
        };
        let mut writer = writer.lock().unwrap();
        write_msg(
            &mut *writer,
            &Event::StreamFrame {
                stream_id: frame.stream_id,
                sequence: frame.sequence,
                width: frame.width,
                height: frame.height,
                stride: frame.stride,
                format: frame.format,
                damage: frame.damage,
                dropped: frame.dropped,
                byte_len: blob.len(),
                slot: None,
            },
        )
        .and_then(|()| blob.send(&writer))
        .is_ok()
    }

    /// Push a protocol-25 slot frame: the header carries `slot` and no
    /// descriptor follows.
    #[must_use]
    pub fn push_stream_frame_slot(&self, frame: StreamFrameFdPayload, slot: u32) -> bool {
        let writer = self
            .streams
            .lock()
            .unwrap()
            .get(&frame.stream_id)
            .map(|(_, writer)| Arc::clone(writer));
        let Some(writer) = writer else {
            return false;
        };
        let mut writer = writer.lock().unwrap();
        write_msg(
            &mut *writer,
            &Event::StreamFrame {
                stream_id: frame.stream_id,
                sequence: frame.sequence,
                width: frame.width,
                height: frame.height,
                stride: frame.stride,
                format: frame.format,
                damage: frame.damage,
                dropped: frame.dropped,
                byte_len: frame.byte_len,
                slot: Some(slot),
            },
        )
        .is_ok()
    }

    /// Push a `StreamFrame` header followed by an unsealed descriptor. Used
    /// for dmabuf stream tests; `fd` must be `byte_len` bytes long.
    #[must_use]
    pub fn push_stream_frame_fd(
        &self,
        frame: StreamFrameFdPayload,
        fd: std::os::fd::RawFd,
    ) -> bool {
        let writer = self
            .streams
            .lock()
            .unwrap()
            .get(&frame.stream_id)
            .map(|(_, writer)| Arc::clone(writer));
        let Some(writer) = writer else {
            return false;
        };
        let mut writer = writer.lock().unwrap();
        write_msg(
            &mut *writer,
            &Event::StreamFrame {
                stream_id: frame.stream_id,
                sequence: frame.sequence,
                width: frame.width,
                height: frame.height,
                stride: frame.stride,
                format: frame.format,
                damage: frame.damage,
                dropped: frame.dropped,
                byte_len: frame.byte_len,
                slot: None,
            },
        )
        .and_then(|()| crate::blob::send_fd(&writer, fd))
        .is_ok()
    }

    /// Push a protocol-29 `StreamGeometryChanged` event: after it, the
    /// compositor produces no further frames for the stream until the
    /// client restarts it.
    #[must_use]
    pub fn push_stream_geometry_changed(&self, stream_id: u64, width: u32, height: u32) -> bool {
        let writer = self
            .streams
            .lock()
            .unwrap()
            .get(&stream_id)
            .map(|(_, writer)| Arc::clone(writer));
        let Some(writer) = writer else {
            return false;
        };
        let mut writer = writer.lock().unwrap();
        write_msg(
            &mut *writer,
            &Event::StreamGeometryChanged {
                stream_id,
                width,
                height,
            },
        )
        .is_ok()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for stream in self.connections.lock().unwrap().drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn serve_connection(
    stream: UnixStream,
    connection: u64,
    handler: Arc<dyn Handler>,
    streams: Streams,
    exact_version: Option<u32>,
) {
    let Ok(mut reader) = stream.try_clone() else {
        return;
    };
    let writer = Arc::new(Mutex::new(stream));
    let mut handshaken = false;
    // Per-connection authorization state, mirroring what the real dispatch
    // consults: the granted capabilities, whether a live lease is held, and
    // the connection's scope. The portal scope is the only one carrying the
    // SetWallpaper op; handler-declared scopes and unscoped connections
    // carry no ops.
    let mut granted = ConnectionCapabilities::QUERY;
    let mut lease_alive = false;
    let mut scope: Option<String> = None;
    while let Ok(request) = read_msg::<_, Request>(&mut reader) {
        let result = match request {
            Request::Hello {
                version,
                caps,
                scope: requested_scope,
                lease,
            } if !handshaken => {
                let accepted = match exact_version {
                    Some(exact) => version == exact,
                    None => version <= PROTOCOL_VERSION,
                };
                if !accepted {
                    send_error(&writer, format!("unsupported protocol version {version}"))
                } else if requested_scope.as_deref().is_some_and(|name| {
                    name != LOCAL_PORTAL_SCOPE && !handler.known_scopes().contains(&name)
                }) {
                    send_error(&writer, "unknown test scope".into())
                } else {
                    handshaken = true;
                    granted = ConnectionCapabilities {
                        query: true,
                        ..caps
                    };
                    let grant = if handler.grant_lease() {
                        lease.map(|request| LeaseGrant {
                            id: connection,
                            ttl_ms: request.ttl_ms,
                            renewable: true,
                        })
                    } else {
                        None
                    };
                    lease_alive = grant.is_some();
                    scope = requested_scope;
                    send(
                        &writer,
                        &Response::Hello {
                            version: version.min(PROTOCOL_VERSION),
                            caps: granted,
                            lease: grant,
                        },
                    )
                }
            }
            _ if !handshaken => send_error(&writer, "Hello must be the first request".into()),
            Request::Hello { .. } => send_error(&writer, "duplicate Hello".into()),
            Request::GetSettings => send(
                &writer,
                &Response::Settings {
                    snapshot: handler.settings(),
                },
            ),
            Request::Subscribe => send(&writer, &Response::Subscribed),
            Request::RenewLease { ttl_ms } => {
                // A lease cannot be created out of nothing: only a
                // connection that already holds one renews it.
                if lease_alive {
                    send(
                        &writer,
                        &Response::LeaseRenewed {
                            lease: LeaseGrant {
                                id: connection,
                                ttl_ms,
                                renewable: true,
                            },
                        },
                    )
                } else {
                    send_error(&writer, "no active lease to renew".into())
                }
            }
            Request::EnumerateOutputs => match handler.enumerate_outputs() {
                Ok(outputs) => send(&writer, &Response::Outputs { outputs }),
                Err(message) => send_error(&writer, message),
            },
            Request::CaptureOutput { region } => match handler.capture_output(region) {
                Ok(capture) => match SealedBlob::new(&capture.png) {
                    Ok(blob) => {
                        let mut writer = writer.lock().unwrap();
                        write_msg(
                            &mut *writer,
                            &Response::CaptureOutput {
                                width: capture.width,
                                height: capture.height,
                                png_bytes: blob.len(),
                            },
                        )
                        .and_then(|()| blob.send(&writer))
                    }
                    Err(error) => Err(error),
                },
                Err(message) => send_error(&writer, message),
            },
            Request::PickTarget { kind } => match handler.pick_target(connection, kind) {
                Ok(result) => send(&writer, &Response::Picked { result }),
                Err(message) => send_error(&writer, message),
            },
            Request::PickConfirm {
                title,
                body,
                accept_label,
            } => match handler.pick_confirm(connection, title, body, accept_label) {
                Ok(result) => send(&writer, &Response::ConfirmPicked { result }),
                Err(message) => send_error(&writer, message),
            },
            Request::StreamOutputStart {
                max_fps,
                target,
                dmabuf,
                cursor,
            } => {
                match handler.stream_output_start(connection, max_fps, target, dmabuf, cursor) {
                    Ok(info) => {
                        let (slots, slot_stride, slot_bytes) = match info.slots.as_ref() {
                            Some(table) if !table.is_empty() => (
                                Some(table.len() as u32),
                                Some(table[0].stride),
                                Some(table[0].byte_len),
                            ),
                            _ => (None, None, None),
                        };
                        // Reply and slot descriptors must land contiguously:
                        // the writer is held for the whole sequence. The
                        // delivery lane is registered BEFORE the reply, so
                        // a concurrently pushed frame finds the lane and
                        // queues on the writer mutex instead of being lost
                        // to a lookup that ran before registration.
                        // `frames_before_started` is the deliberate
                        // exception: it reproduces the real compositor's
                        // race, where a produced frame precedes the reply.
                        streams
                            .lock()
                            .unwrap()
                            .insert(info.stream_id, (connection, Arc::clone(&writer)));
                        let result = {
                            let mut guard = writer.lock().unwrap();
                            let mut wrote = Ok(());
                            for frame in handler.frames_before_started(info.stream_id) {
                                wrote = SealedBlob::new(&frame.pixels).and_then(|blob| {
                                    write_msg(
                                        &mut *guard,
                                        &Event::StreamFrame {
                                            stream_id: frame.stream_id,
                                            sequence: frame.sequence,
                                            width: frame.width,
                                            height: frame.height,
                                            stride: frame.stride,
                                            format: frame.format,
                                            damage: frame.damage,
                                            dropped: frame.dropped,
                                            byte_len: blob.len(),
                                            slot: None,
                                        },
                                    )
                                    .and_then(|()| blob.send(&guard))
                                });
                                if wrote.is_err() {
                                    break;
                                }
                            }
                            wrote
                                .and_then(|()| {
                                    write_msg(
                                        &mut *guard,
                                        &Response::StreamOutputStarted {
                                            stream_id: info.stream_id,
                                            width: info.width,
                                            height: info.height,
                                            format: info.format,
                                            slots,
                                            slot_stride,
                                            slot_bytes,
                                        },
                                    )
                                })
                                .and_then(|()| {
                                    if let Some(table) = info.slots {
                                        for slot in table {
                                            crate::blob::send_fd(&guard, slot.fd)?;
                                        }
                                    }
                                    Ok(())
                                })
                        };
                        if result.is_err() {
                            // The reply never landed; do not leave a dead
                            // delivery lane registered.
                            streams.lock().unwrap().remove(&info.stream_id);
                        }
                        result
                    }
                    Err(message) => send_error(&writer, message),
                }
            }
            Request::StreamBufferRelease { stream_id, slot } => {
                handler.stream_buffer_release(stream_id, slot);
                send(&writer, &Response::StreamBufferReleased { stream_id, slot })
            }
            Request::StreamOutputStop { stream_id } => {
                streams.lock().unwrap().remove(&stream_id);
                handler.stream_output_stop(stream_id);
                send(&writer, &Response::StreamOutputStopped { stream_id })
            }
            Request::SetWallpaper { path } => {
                // The real dispatch's gate order: control, a live lease, an
                // explicit SetWallpaper op in the connection's scope (never
                // inherited), a valid path, then the lock/VT gate. The reply
                // is the decode-and-swap receipt.
                let op_allowed = scope.as_deref() == Some(LOCAL_PORTAL_SCOPE);
                if !granted.control {
                    send_error(
                        &writer,
                        "SetWallpaper requires the control capability".into(),
                    )
                } else if !lease_alive {
                    send_error(&writer, "privileged capability lease expired".into())
                } else if !op_allowed {
                    send_error(&writer, "out of scope".into())
                } else if !valid_wallpaper_path(&path) {
                    send_error(
                        &writer,
                        "wallpaper path must be bounded, absolute, and lexically normalized".into(),
                    )
                } else if !handler.session_active() {
                    send_error(&writer, "session is locked or inactive".into())
                } else {
                    match handler.set_wallpaper(connection, path) {
                        Ok(()) => send(&writer, &Response::WallpaperSet {}),
                        Err(message) => send_error(&writer, message),
                    }
                }
            }
        };
        if result.is_err() {
            break;
        }
    }
    streams
        .lock()
        .unwrap()
        .retain(|_, (owner, _)| *owner != connection);
    handler.streams_disconnected(connection);
}

fn send(writer: &SharedWriter, response: &Response) -> io::Result<()> {
    write_msg(&mut *writer.lock().unwrap(), response)
}

fn send_error(writer: &SharedWriter, message: String) -> io::Result<()> {
    send(writer, &Response::Error { message })
}
