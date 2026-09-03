//! Synchronous client for the Portal-owned Tessera IPC v29 projection.

use std::collections::VecDeque;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::blob;
use crate::codec::{read_msg, write_msg};
use crate::schema::{LeaseRequest, Request, Response, valid_wallpaper_path};
use crate::{
    ConfirmPickResult, ConnectionCapabilities, Event, LeaseGrant, MIN_PROTOCOL_VERSION, OutputInfo,
    PROTOCOL_VERSION, PickKind, PickResult, Rect, SettingsSnapshot, StreamCursorMode,
    StreamPixelFormat, StreamTarget,
};

#[derive(Debug)]
pub struct StreamStarted {
    pub stream_id: u64,
    pub width: u32,
    pub height: u32,
    pub format: StreamPixelFormat,
    /// dmabuf slot descriptors (protocol 25), transferred once at start.
    pub slots: Option<Vec<StreamSlot>>,
}

/// One dmabuf slot of a protocol-25 stream: a fixed-size descriptor the
/// compositor renders into until the consumer releases it.
#[derive(Debug)]
pub struct StreamSlot {
    pub file: std::fs::File,
    pub stride: u32,
    pub byte_len: u64,
}

/// Frame payload descriptor received behind a `StreamFrame` header. The
/// variant mirrors the header's pixel format: SHM formats carry a sealed
/// memfd, `Dmabuf` carries the single-plane GPU buffer descriptor.
#[derive(Debug)]
pub enum StreamPayload {
    /// Sealed memfd of `byte_len` bytes, positioned at offset 0.
    Memfd(std::fs::File),
    /// Single-plane dmabuf of `byte_len` bytes; the plane stride travels in
    /// the frame header.
    Dmabuf(std::fs::File),
    /// A frame in a dmabuf slot transferred at start (protocol 25); the
    /// frame header's `slot` names it and no descriptor follows.
    Slot,
}

#[derive(Debug)]
pub struct StreamFrame {
    pub stream_id: u64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: StreamPixelFormat,
    pub damage: Vec<Rect>,
    pub dropped: u64,
    /// The dmabuf slot this frame occupies (protocol 25); `payload` is
    /// [`StreamPayload::Slot`] then.
    pub slot: Option<u32>,
    pub payload: StreamPayload,
}

#[derive(Debug)]
pub enum StreamMessage {
    Frame(StreamFrame),
    Ended {
        stream_id: u64,
        reason: String,
    },
    LeaseRenewed,
    /// The stream's output geometry changed (protocol 29). The compositor
    /// sends no further frames for the stream until the client restarts it
    /// (`StreamOutputStop` + `StreamOutputStart`).
    GeometryChanged {
        stream_id: u64,
        width: u32,
        height: u32,
    },
}

pub struct Client {
    stream: UnixStream,
    caps: ConnectionCapabilities,
    lease: Option<LeaseGrant>,
    /// The protocol version both ends speak after the handshake.
    version: u32,
    /// Stream events that raced ahead of a request reply: the compositor
    /// publishes a stream before writing `StreamOutputStarted`, so frames
    /// can legitimately precede it. Drained before the socket is read again.
    pending: VecDeque<StreamMessage>,
}

impl Client {
    pub fn connect_with_timeout(
        path: &Path,
        requested: ConnectionCapabilities,
        timeout: Duration,
    ) -> io::Result<Self> {
        Self::connect_inner(path, requested, None, timeout)
    }

    pub fn connect_scoped_with_timeout(
        path: &Path,
        requested: ConnectionCapabilities,
        scope: impl Into<String>,
        timeout: Duration,
    ) -> io::Result<Self> {
        Self::connect_inner(path, requested, Some(scope.into()), timeout)
    }

    fn connect_inner(
        path: &Path,
        requested: ConnectionCapabilities,
        scope: Option<String>,
        timeout: Duration,
    ) -> io::Result<Self> {
        let mut version = PROTOCOL_VERSION;
        loop {
            match Self::handshake(path, requested, scope.as_deref(), version, timeout)? {
                Ok(client) => return Ok(client),
                Err(_) if version > MIN_PROTOCOL_VERSION => {
                    // A refused or failed handshake at the newest version
                    // retries one step older; the final attempt surfaces
                    // the real error.
                    version -= 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Attempt the handshake once at `version`. `Ok(Err(..))` is a clean
    /// server-side refusal (version or scope); `Err(..)` is a transport or
    /// framing failure.
    fn handshake(
        path: &Path,
        requested: ConnectionCapabilities,
        scope: Option<&str>,
        version: u32,
        timeout: Duration,
    ) -> io::Result<Result<Self, io::Error>> {
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        write_msg(
            &mut stream,
            &Request::Hello {
                version,
                caps: requested,
                scope: scope.map(str::to_string),
                lease: requested.privileged().then(LeaseRequest::default),
            },
        )?;
        Ok(match read_msg::<_, Response>(&mut stream)? {
            Response::Hello {
                version: replied,
                caps,
                lease,
            } if replied <= version => Ok(Self {
                stream,
                caps,
                lease,
                version: replied,
                pending: VecDeque::new(),
            }),
            Response::Hello { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("Tessera IPC server replied a newer protocol than offered ({version})"),
            )),
            Response::Error { message } => {
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, message))
            }
            other => Err(unexpected("Hello", &other)),
        })
    }

    #[must_use]
    pub fn caps(&self) -> ConnectionCapabilities {
        self.caps
    }

    /// The protocol version negotiated at the handshake.
    #[must_use]
    pub fn protocol_version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub fn lease(&self) -> Option<LeaseGrant> {
        self.lease
    }

    pub fn set_io_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)?;
        self.stream.set_write_timeout(timeout)
    }

    pub fn renew_lease(&mut self, ttl_ms: u64) -> io::Result<LeaseGrant> {
        write_msg(&mut self.stream, &Request::RenewLease { ttl_ms })?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::LeaseRenewed { lease } => {
                self.lease = Some(lease);
                Ok(lease)
            }
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("LeaseRenewed", &other)),
        }
    }

    pub fn settings(&mut self) -> io::Result<SettingsSnapshot> {
        write_msg(&mut self.stream, &Request::GetSettings)?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::Settings { snapshot } => Ok(snapshot),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("Settings", &other)),
        }
    }

    pub fn subscribe(&mut self) -> io::Result<()> {
        write_msg(&mut self.stream, &Request::Subscribe)?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::Subscribed => Ok(()),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("Subscribed", &other)),
        }
    }

    pub fn next_event(&mut self) -> io::Result<Event> {
        let value: serde_json::Value = read_msg(&mut self.stream)?;
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("SettingsChanged") => serde_json::from_value(value).map_err(json_error),
            Some(_) => Ok(Event::Other),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Tessera IPC event has no type",
            )),
        }
    }

    /// List the compositor's outputs (protocol 29). Older compositors do
    /// not speak the op; their refusal surfaces as the reply's `Error`.
    pub fn enumerate_outputs(&mut self) -> io::Result<Vec<OutputInfo>> {
        write_msg(&mut self.stream, &Request::EnumerateOutputs)?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::Outputs { outputs } => Ok(outputs),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("Outputs", &other)),
        }
    }

    pub fn capture_output(&mut self) -> io::Result<(u32, u32, Vec<u8>)> {
        self.capture_output_region(None)
    }

    pub fn capture_output_region(
        &mut self,
        region: Option<Rect>,
    ) -> io::Result<(u32, u32, Vec<u8>)> {
        write_msg(&mut self.stream, &Request::CaptureOutput { region })?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::CaptureOutput {
                width,
                height,
                png_bytes,
            } => Ok((width, height, blob::receive(&self.stream, png_bytes)?)),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("CaptureOutput", &other)),
        }
    }

    pub fn pick_target(&mut self, kind: PickKind) -> io::Result<PickResult> {
        write_msg(&mut self.stream, &Request::PickTarget { kind })?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::Picked { result } => Ok(result),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("Picked", &other)),
        }
    }

    pub fn pick_confirm(
        &mut self,
        title: String,
        body: String,
        accept_label: Option<String>,
    ) -> io::Result<ConfirmPickResult> {
        write_msg(
            &mut self.stream,
            &Request::PickConfirm {
                title,
                body,
                accept_label,
            },
        )?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::ConfirmPicked { result } => Ok(result),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("ConfirmPicked", &other)),
        }
    }

    /// Replace the desktop wallpaper with the image at `path`. The op
    /// predates this projection's version floor, so every negotiated
    /// protocol speaks it. The compositor decodes the file itself and
    /// answers with an authoritative receipt, so the caller must stage the
    /// image at a path that stays alive for the session. The path rule
    /// mirrors the compositor's own check (bounded, absolute, lexically
    /// normalized) so a request it would reject never crosses the socket.
    pub fn set_wallpaper(&mut self, path: &Path) -> io::Result<()> {
        if !valid_wallpaper_path(path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "wallpaper path {} must be bounded, absolute, and lexically normalized",
                    path.display()
                ),
            ));
        }
        write_msg(
            &mut self.stream,
            &Request::SetWallpaper {
                path: path.to_path_buf(),
            },
        )?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::WallpaperSet {} => Ok(()),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("WallpaperSet", &other)),
        }
    }

    pub fn start_output_stream_target(
        &mut self,
        max_fps: Option<u32>,
        target: StreamTarget,
    ) -> io::Result<StreamStarted> {
        self.start_output_stream(max_fps, target, false, None)
    }

    /// Start an output stream, optionally opting into the protocol-25
    /// dmabuf slot transport. The opt-in is honored only when the
    /// negotiated protocol is 25 or newer; an older server answers with the
    /// SHM stream as if the flag were absent. The protocol-29 parameters
    /// degrade the same way: `cursor` is sent only to a protocol-29 peer,
    /// and a connector-named `target` requires one outright (an older
    /// compositor could only stream the whole desktop, so the request
    /// fails closed instead of capturing more than asked).
    pub fn start_output_stream(
        &mut self,
        max_fps: Option<u32>,
        target: StreamTarget,
        dmabuf: bool,
        cursor: Option<StreamCursorMode>,
    ) -> io::Result<StreamStarted> {
        let dmabuf = dmabuf && self.version >= 25;
        let cursor = cursor.filter(|_| self.version >= 29);
        if self.version < 29 && matches!(target, StreamTarget::Output { output: Some(_) }) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "per-output stream targets need protocol 29",
            ));
        }
        write_msg(
            &mut self.stream,
            &Request::StreamOutputStart {
                max_fps,
                target,
                dmabuf: dmabuf.then_some(true),
                cursor,
            },
        )?;
        // The compositor publishes the stream lane before it writes the
        // reply, so frames (or a stream end) may already be on the wire.
        // Buffer them in arrival order; they surface from
        // `next_stream_message` after the start completes.
        let started = loop {
            let value: serde_json::Value = read_msg(&mut self.stream)?;
            if value.get("type").and_then(serde_json::Value::as_str) == Some("StreamOutputStarted")
            {
                break serde_json::from_value::<Response>(value).map_err(json_error)?;
            }
            if let Some(message) = self.stream_message(value)? {
                self.pending.push_back(message);
            }
        };
        match started {
            Response::StreamOutputStarted {
                stream_id,
                width,
                height,
                format,
                slots,
                slot_stride,
                slot_bytes,
            } => {
                let slots = match (format, slots, slot_stride, slot_bytes) {
                    // Protocol 25: the slot table follows the reply.
                    (StreamPixelFormat::Dmabuf { .. }, Some(count), Some(stride), Some(bytes)) => {
                        let mut table = Vec::with_capacity(count as usize);
                        for _ in 0..count {
                            let file = blob::receive_dmabuf_file(&self.stream, bytes)?;
                            table.push(StreamSlot {
                                file,
                                stride,
                                byte_len: bytes,
                            });
                        }
                        Some(table)
                    }
                    // A dmabuf announcement without slot metadata is the
                    // per-frame transport: each frame carries its own
                    // descriptor.
                    (StreamPixelFormat::Dmabuf { .. }, None, None, None) => None,
                    (StreamPixelFormat::Dmabuf { .. }, ..) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "dmabuf stream announcement with a partial slot table",
                        ));
                    }
                    (_, None, None, None) => None,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "SHM stream announcement carried partial slot metadata",
                        ));
                    }
                };
                Ok(StreamStarted {
                    stream_id,
                    width,
                    height,
                    format,
                    slots,
                })
            }
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("StreamOutputStarted", &other)),
        }
    }

    /// Release a dmabuf stream slot (protocol 25) after the PipeWire
    /// consumer returned its buffer. The reply arrives on the read side and
    /// is skipped like any interleaved response.
    pub fn release_stream_buffer(&mut self, stream_id: u64, slot: u32) -> io::Result<()> {
        write_msg(
            &mut self.stream,
            &Request::StreamBufferRelease { stream_id, slot },
        )
    }

    pub fn stop_output_stream(&mut self, stream_id: u64) -> io::Result<()> {
        write_msg(&mut self.stream, &Request::StreamOutputStop { stream_id })?;
        // Frames already in flight may precede the reply; buffer them so a
        // later reader still observes them in arrival order.
        loop {
            let value: serde_json::Value = read_msg(&mut self.stream)?;
            if value.get("type").and_then(serde_json::Value::as_str) == Some("StreamOutputStopped")
            {
                match serde_json::from_value::<Response>(value).map_err(json_error)? {
                    Response::StreamOutputStopped { stream_id: stopped }
                        if stopped == stream_id =>
                    {
                        return Ok(());
                    }
                    other => return Err(unexpected("StreamOutputStopped", &other)),
                }
            }
            if let Some(message) = self.stream_message(value)? {
                self.pending.push_back(message);
            }
        }
    }

    pub fn request_lease_renewal(&mut self, ttl_ms: u64) -> io::Result<()> {
        write_msg(&mut self.stream, &Request::RenewLease { ttl_ms })
    }

    pub fn next_stream_message(&mut self) -> io::Result<StreamMessage> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(message);
        }
        loop {
            let value: serde_json::Value = read_msg(&mut self.stream)?;
            if let Some(message) = self.stream_message(value)? {
                return Ok(message);
            }
        }
    }

    /// Parse one wire message into a stream event, consuming the descriptor
    /// that follows a frame header. Returns `None` for messages the stream
    /// reader skips (slot-release replies and unrelated traffic).
    fn stream_message(&mut self, value: serde_json::Value) -> io::Result<Option<StreamMessage>> {
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("StreamFrame") => {
                let event: Event = serde_json::from_value(value).map_err(json_error)?;
                let Event::StreamFrame {
                    stream_id,
                    sequence,
                    width,
                    height,
                    stride,
                    format,
                    damage,
                    dropped,
                    byte_len,
                    slot,
                } = event
                else {
                    unreachable!();
                };
                let payload = match slot {
                    Some(_) => StreamPayload::Slot,
                    None => match format {
                        StreamPixelFormat::Bgra8 | StreamPixelFormat::Rgba8 => {
                            StreamPayload::Memfd(blob::receive_memfd_file(&self.stream, byte_len)?)
                        }
                        StreamPixelFormat::Dmabuf { .. } => StreamPayload::Dmabuf(
                            blob::receive_dmabuf_file(&self.stream, byte_len)?,
                        ),
                    },
                };
                Ok(Some(StreamMessage::Frame(StreamFrame {
                    stream_id,
                    sequence,
                    width,
                    height,
                    stride,
                    format,
                    damage,
                    dropped,
                    slot,
                    payload,
                })))
            }
            Some("StreamEnded") => {
                let event: Event = serde_json::from_value(value).map_err(json_error)?;
                let Event::StreamEnded { stream_id, reason } = event else {
                    unreachable!();
                };
                Ok(Some(StreamMessage::Ended { stream_id, reason }))
            }
            Some("StreamGeometryChanged") => {
                let event: Event = serde_json::from_value(value).map_err(json_error)?;
                let Event::StreamGeometryChanged {
                    stream_id,
                    width,
                    height,
                } = event
                else {
                    unreachable!();
                };
                Ok(Some(StreamMessage::GeometryChanged {
                    stream_id,
                    width,
                    height,
                }))
            }
            Some("LeaseRenewed") => {
                let response: Response = serde_json::from_value(value).map_err(json_error)?;
                let Response::LeaseRenewed { lease } = response else {
                    unreachable!();
                };
                self.lease = Some(lease);
                Ok(Some(StreamMessage::LeaseRenewed))
            }
            Some("Error") => {
                let response: Response = serde_json::from_value(value).map_err(json_error)?;
                let Response::Error { message } = response else {
                    unreachable!();
                };
                Err(io::Error::other(message))
            }
            Some(_) => Ok(None),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Tessera IPC stream message has no type",
            )),
        }
    }
}

impl AsRawFd for Client {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.stream.as_raw_fd()
    }
}

fn unexpected(expected: &str, response: &Response) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("expected {expected}, got {response:?}"),
    )
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
