//! Portal-owned projection of the sigil daemon's native IPC wire protocol (sigil-wire-v2).
//!
//! The portal integrates with sigil at runtime through the narrow Unix
//! socket contract only — no sigil crate is imported, mirroring the
//! Tessera boundary (ADR-0004): each side owns its own implementation, so
//! a matching bug cannot pass a shared test.
//!
//! Under ADR-0023 and Sigil ADR-0009, the wire protocol is a strict
//! binary framing format (`sigil-wire-v2`):
//! - Header: 8 bytes `[b'S', b'I', b'G', b'L', version=2, opcode/status, u16_be payload_len]`
//! - Request: Compact TLV fields `[u16_be len][utf-8 bytes]` without JSON heap overhead.
//! - Response: Direct 32-byte secret payload wrapped in `Zeroizing` or typed status code.
//!
//! ## Every read and write is bounded
//!
//! sigil is an interactive daemon: when the vault is locked it raises an
//! unlock prompt and answers only after the user types a password. A socket
//! with no read timeout would park the calling thread for as long as that
//! takes — minutes, or forever if the prompt is never dismissed and the
//! daemon never writes. `SOCK_STREAM` offers no other progress signal (no
//! EOF, no partial-delivery event), so the only defence is a bound.
//!
//! The bound is deliberately generous: it is not a latency budget for the
//! user's typing, it is a liveness floor that turns "sigil is wedged" into a
//! typed [`NativeError::TimedOut`] the caller can report, instead of a hang
//! that propagates into the portal's own interfaces.
//!
//! ## Cancellation closes the socket
//!
//! The response is read in short `poll(2)` slices, and the caller's
//! cancellation predicate is checked between slices. When the portal's
//! `Request.Close` arrives, the read returns [`NativeError::Cancelled`] and
//! the stream is dropped, so sigil observes EOF and can abandon a pending
//! unlock prompt instead of answering a caller that has gone.
//!
//! ## Responses are decoded strictly
//!
//! Every status has one payload shape (ADR-0030): a non-empty secret for
//! `STATUS_SECRET`, no bytes for `STATUS_LOCKED`, `STATUS_DESYNCED`, and
//! `STATUS_CANCELLED`, and exactly one length-prefixed UTF-8 string for
//! `STATUS_ACCESS_DENIED` and `STATUS_ERROR`. Any other shape fails closed.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// The environment variable overriding the sigil native socket path, matching
/// `sigil-ipc`'s `SIGIL_SOCKET_PATH`.
const SOCKET_PATH_ENV: &str = "SIGIL_SOCKET_PATH";
/// The default socket subpath under `$XDG_RUNTIME_DIR`, matching the sigil
/// daemon's own default.
const SOCKET_SUBPATH: &str = "sigil/native.sock";

/// Wire protocol constants matching sigil-wire-v2
const WIRE_MAGIC: [u8; 4] = *b"SIGL";
const WIRE_VERSION: u8 = 2;
const HEADER_LEN: usize = 8;
const MAX_PAYLOAD_SIZE: usize = 4096;

const OP_GET_APPLICATION_SECRET: u8 = 0x01;

/// Liveness floor for the whole response, measured from the moment the
/// request is written (see the module docs).
///
/// Chosen to outlast a slow unlock prompt the user is actively answering
/// while still guaranteeing a wedged sigil eventually fails the request:
/// five minutes is well past any plausible interaction, and the portal's own
/// `Request.Close` lets a caller abandon the wait sooner.
pub const SOCKET_TIMEOUT: Duration = Duration::from_secs(300);

/// A secret must be written before the caller's read times out; this is the
/// far shorter budget for the descriptor hand-off (local socket, no prompt).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

const STATUS_SECRET: u8 = 0x01;
const STATUS_LOCKED: u8 = 0x03;
const STATUS_DESYNCED: u8 = 0x04;
const STATUS_CANCELLED: u8 = 0x05;
const STATUS_ACCESS_DENIED: u8 = 0x06;
const STATUS_ERROR: u8 = 0x07;

/// Errors occurring in the native IPC projection.
#[derive(Debug, thiserror::Error)]
pub enum NativeError {
    /// The daemon reported the vault is locked and could not be unlocked
    /// interactively (headless session or prompter unavailable).
    #[error("sigil is locked")]
    Locked,
    /// The vault's primary slot is out of sync with the OS password and needs
    /// recovery (`sigil recover`).
    #[error("sigil vault credentials are desynchronized")]
    Desynced,
    /// The unlock prompt was dismissed or timed out.
    #[error("sigil unlock was cancelled")]
    Cancelled,
    /// The daemon denied the caller.
    #[error("sigil denied access: {0}")]
    AccessDenied(String),
    /// The daemon reported an internal failure.
    #[error("sigil daemon error: {0}")]
    Daemon(String),
    /// The socket or the framing failed.
    #[error("sigil IPC error: {0}")]
    Io(#[from] std::io::Error),
    /// The daemon did not answer a framed read within
    /// [`SOCKET_TIMEOUT`] (a wedged sigil, not a slow unlock).
    #[error("sigil did not answer within {SOCKET_TIMEOUT:?}")]
    TimedOut,
    /// The daemon answered with a variant this projection does not decode.
    #[error("unexpected sigil IPC response")]
    Unexpected,
}

/// A blocking connection factory for the sigil daemon's native socket.
#[derive(Clone, Debug)]
pub struct SigilConnection {
    socket_path: PathBuf,
}

impl SigilConnection {
    /// Target an explicit native IPC socket path.
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// Connect to `$SIGIL_SOCKET_PATH` if set and non-empty, otherwise
    /// `$XDG_RUNTIME_DIR/sigil/native.sock`. Fails closed if neither is set.
    pub fn connect_default() -> Result<Self, NativeError> {
        let socket_path = resolve_socket_path(
            std::env::var_os(SOCKET_PATH_ENV),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )?;
        Ok(Self { socket_path })
    }

    /// The socket path this connection targets.
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    /// Retrieve/derive the application secret for `(namespace, subject,
    /// purpose)`. Each call opens its own connection, matching the sigil
    /// client's connect-per-request pattern and the daemon's framing
    /// state machine.
    ///
    /// The returned secret is wrapped in [`Zeroizing`] so it is deterministically
    /// wiped from process memory when dropped (ADR-0022).
    pub fn get_application_secret(
        &self,
        namespace: &str,
        subject: &str,
        purpose: &str,
    ) -> Result<Zeroizing<Vec<u8>>, NativeError> {
        self.get_application_secret_cancellable(namespace, subject, purpose, None)
    }

    /// Retrieve/derive the application secret while polling `cancelled` between
    /// short socket-read slices. When `cancelled` returns `true`, the Unix
    /// socket is closed immediately (signaling peer EOF to `sigil` so any
    /// in-flight interactive unlock wait can abort) and [`NativeError::Cancelled`]
    /// is returned.
    pub fn get_application_secret_cancellable(
        &self,
        namespace: &str,
        subject: &str,
        purpose: &str,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<Zeroizing<Vec<u8>>, NativeError> {
        if cancelled.is_some_and(|is_cancelled| is_cancelled()) {
            return Err(NativeError::Cancelled);
        }

        let mut stream = UnixStream::connect(&self.socket_path)?;
        // Bound both directions *before* the first byte: an unbounded read on
        // an interactive daemon is a potential permanent hang (module docs).
        stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;

        // Encode binary request frame: [Header 8B][Payload]
        let mut frame = Zeroizing::new(Vec::with_capacity(
            namespace.len() + subject.len() + purpose.len() + 14,
        ));
        frame.extend_from_slice(&WIRE_MAGIC);
        frame.push(WIRE_VERSION);
        frame.push(OP_GET_APPLICATION_SECRET);

        let mut payload = Zeroizing::new(Vec::new());
        append_str(&mut payload, namespace)?;
        append_str(&mut payload, subject)?;
        append_str(&mut payload, purpose)?;

        if payload.len() > MAX_PAYLOAD_SIZE {
            return Err(NativeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Request payload exceeds max frame size",
            )));
        }

        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        frame.extend_from_slice(&payload);

        stream.write_all(&frame)?;
        stream.flush()?;

        let deadline = Instant::now() + SOCKET_TIMEOUT;

        // Read 8-byte response header
        let mut header = [0u8; HEADER_LEN];
        read_exact_cancellable(&mut stream, &mut header, deadline, cancelled)?;

        if header[0..4] != WIRE_MAGIC {
            return Err(NativeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid sigil wire magic",
            )));
        }
        if header[4] != WIRE_VERSION {
            return Err(NativeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Unsupported sigil wire version: {}", header[4]),
            )));
        }

        let status = header[5];
        let payload_len = u16::from_be_bytes([header[6], header[7]]) as usize;
        if payload_len > MAX_PAYLOAD_SIZE {
            return Err(NativeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Response payload exceeds max frame size",
            )));
        }

        let mut payload_buf = Zeroizing::new(vec![0u8; payload_len]);
        read_exact_cancellable(&mut stream, &mut payload_buf, deadline, cancelled)?;

        match status {
            STATUS_SECRET => {
                if payload_buf.is_empty() {
                    return Err(NativeError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Empty secret payload in sigil response",
                    )));
                }
                Ok(payload_buf)
            }
            STATUS_LOCKED => {
                ensure_empty_payload(&payload_buf, "STATUS_LOCKED")?;
                Err(NativeError::Locked)
            }
            STATUS_DESYNCED => {
                ensure_empty_payload(&payload_buf, "STATUS_DESYNCED")?;
                Err(NativeError::Desynced)
            }
            STATUS_CANCELLED => {
                ensure_empty_payload(&payload_buf, "STATUS_CANCELLED")?;
                Err(NativeError::Cancelled)
            }
            STATUS_ACCESS_DENIED => {
                let msg = decode_string(&payload_buf)?;
                Err(NativeError::AccessDenied(msg))
            }
            STATUS_ERROR => {
                let msg = decode_string(&payload_buf)?;
                Err(NativeError::Daemon(msg))
            }
            _ => Err(NativeError::Unexpected),
        }
    }
}

/// Resolve the socket from `$SIGIL_SOCKET_PATH`, then `$XDG_RUNTIME_DIR`.
///
/// Both must be absolute. The portal is D-Bus-activated, so its working
/// directory carries no meaning: a relative override is a misconfiguration
/// and fails closed rather than resolving against wherever the process
/// happens to run, and a relative runtime dir is ignored as the XDG base
/// directory specification requires.
fn resolve_socket_path(
    override_path: Option<std::ffi::OsString>,
    runtime_dir: Option<std::ffi::OsString>,
) -> Result<PathBuf, NativeError> {
    if let Some(path) = override_path.filter(|p| !p.is_empty()).map(PathBuf::from) {
        if path.is_absolute() {
            return Ok(path);
        }
        return Err(NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("SIGIL_SOCKET_PATH must be absolute, got {path:?}"),
        )));
    }
    let runtime_dir = runtime_dir
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .ok_or_else(|| {
            NativeError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "neither SIGIL_SOCKET_PATH nor an absolute XDG_RUNTIME_DIR is set",
            ))
        })?;
    Ok(runtime_dir.join(SOCKET_SUBPATH))
}

/// Read exactly `buf.len()` bytes before `deadline`, observing `cancelled`
/// every 50 ms via `poll(2)`.
///
/// Checking cancellation inside the wait loop ensures D-Bus `Request.Close`
/// immediately drops the `UnixStream` (triggering peer EOF on `sigil` so any
/// in-flight unlock prompt can abort) instead of parking the worker thread
/// until `SOCKET_TIMEOUT`.
fn read_exact_cancellable(
    stream: &mut UnixStream,
    buf: &mut [u8],
    deadline: Instant,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<(), NativeError> {
    const POLL_SLICE_MS: i32 = 50;

    let mut filled = 0;
    while filled < buf.len() {
        if cancelled.is_some_and(|is_cancelled| is_cancelled()) {
            return Err(NativeError::Cancelled);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(NativeError::TimedOut);
        }
        let remaining_ms = deadline
            .saturating_duration_since(now)
            .as_millis()
            .min(i32::MAX as u128) as i32;
        let slice_ms = remaining_ms.clamp(1, POLL_SLICE_MS);

        let mut pfd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a valid stack struct passed to `poll(2)` for one fd.
        let ready = unsafe { libc::poll(&mut pfd, 1, slice_ms) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(NativeError::Io(err));
        }
        if ready == 0 {
            continue;
        }

        match stream.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(NativeError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "sigil closed the native socket before completing the frame",
                )));
            }
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(NativeError::TimedOut);
            }
            Err(error) => return Err(NativeError::Io(error)),
        }
    }
    Ok(())
}

fn append_str(out: &mut Vec<u8>, s: &str) -> Result<(), NativeError> {
    let bytes = s.as_bytes();
    if bytes.len() > u16::MAX as usize {
        return Err(NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "String exceeds max u16 length",
        )));
    }
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

fn ensure_empty_payload(buf: &[u8], label: &str) -> Result<(), NativeError> {
    if !buf.is_empty() {
        return Err(NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Unexpected trailing bytes in {label} response payload"),
        )));
    }
    Ok(())
}

fn decode_string(buf: &[u8]) -> Result<String, NativeError> {
    if buf.len() < 2 {
        return Err(NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Truncated string length prefix in response payload",
        )));
    }
    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    if buf.len() < 2 + len {
        return Err(NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Truncated string in response payload",
        )));
    }
    if buf.len() != 2 + len {
        return Err(NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Trailing bytes after string in response payload",
        )));
    }
    String::from_utf8(buf[2..2 + len].to_vec()).map_err(|e| {
        NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Bounds every blocking server-side read so a client-side regression
    /// fails the test instead of hanging it.
    const SERVER_READ_TIMEOUT: Duration = Duration::from_secs(5);

    /// Read one request frame off `stream` and return its raw bytes.
    fn read_request(stream: &mut UnixStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(SERVER_READ_TIMEOUT))
            .expect("bound server read");
        let mut header = [0u8; HEADER_LEN];
        stream.read_exact(&mut header).expect("request header");
        let len = u16::from_be_bytes([header[6], header[7]]) as usize;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).expect("request body");
        let mut received = header.to_vec();
        received.extend_from_slice(&body);
        received
    }

    /// Serve one connection that answers with `reply`, run one request
    /// against it, and return the client's result plus the request bytes.
    fn exchange(reply: Vec<u8>) -> (Result<Zeroizing<Vec<u8>>, NativeError>, Vec<u8>) {
        let dir = tempfile::tempdir().expect("socket tempdir");
        let sock = dir.path().join("native.sock");
        let listener = UnixListener::bind(&sock).expect("bind fake sigil");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            let received = read_request(&mut stream);
            let _ = stream.write_all(&reply);
            received
        });
        let result = SigilConnection::new(&sock).get_application_secret(
            "atrium.portal.Secret/v1",
            "org.test.App",
            "master-secret",
        );
        let received = server.join().expect("fake sigil thread");
        (result, received)
    }

    fn reply(status: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
        frame.extend_from_slice(&WIRE_MAGIC);
        frame.push(WIRE_VERSION);
        frame.push(status);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    fn tlv(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        append_str(&mut out, text).expect("encode string");
        out
    }

    #[test]
    fn resolve_socket_path_honors_override_before_runtime_dir() {
        let runtime = Some(OsString::from("/run/user/1000"));
        assert_eq!(
            resolve_socket_path(Some(OsString::from("/custom/sigil.sock")), runtime.clone())
                .unwrap(),
            PathBuf::from("/custom/sigil.sock")
        );
        assert_eq!(
            resolve_socket_path(None, runtime.clone()).unwrap(),
            PathBuf::from("/run/user/1000/sigil/native.sock")
        );
        assert_eq!(
            resolve_socket_path(Some(OsString::new()), runtime.clone()).unwrap(),
            PathBuf::from("/run/user/1000/sigil/native.sock"),
            "an empty override falls through to the runtime dir"
        );
        assert!(
            resolve_socket_path(None, Some(OsString::new())).is_err(),
            "an empty runtime dir must not resolve to a relative path"
        );
        assert!(
            resolve_socket_path(Some(OsString::from("sigil.sock")), runtime).is_err(),
            "a relative override fails closed instead of falling through"
        );
        assert!(
            resolve_socket_path(None, Some(OsString::from("run/user/1000"))).is_err(),
            "a relative runtime dir is ignored, per the XDG base directory spec"
        );
        assert!(resolve_socket_path(None, None).is_err());
    }

    #[test]
    fn request_frame_matches_the_literal_wire_v2_layout() {
        let secret = vec![0xABu8; 32];
        let (result, wire) = exchange(reply(STATUS_SECRET, &secret));
        assert_eq!(result.unwrap().as_slice(), secret.as_slice());

        let mut expected = b"SIGL\x02\x01".to_vec();
        let mut payload = Vec::new();
        for field in ["atrium.portal.Secret/v1", "org.test.App", "master-secret"] {
            payload.extend_from_slice(&(field.len() as u16).to_be_bytes());
            payload.extend_from_slice(field.as_bytes());
        }
        expected.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        expected.extend_from_slice(&payload);
        assert_eq!(wire, expected);
    }

    #[test]
    fn well_formed_statuses_decode_to_typed_errors() {
        assert!(matches!(
            exchange(reply(STATUS_LOCKED, &[])).0,
            Err(NativeError::Locked)
        ));
        assert!(matches!(
            exchange(reply(STATUS_DESYNCED, &[])).0,
            Err(NativeError::Desynced)
        ));
        assert!(matches!(
            exchange(reply(STATUS_CANCELLED, &[])).0,
            Err(NativeError::Cancelled)
        ));
        assert!(matches!(
            exchange(reply(STATUS_ACCESS_DENIED, &tlv("uid mismatch"))).0,
            Err(NativeError::AccessDenied(ref text)) if text == "uid mismatch"
        ));
        assert!(matches!(
            exchange(reply(STATUS_ERROR, &tlv("boom"))).0,
            Err(NativeError::Daemon(ref text)) if text == "boom"
        ));
        assert!(matches!(
            exchange(reply(0x7F, &[])).0,
            Err(NativeError::Unexpected)
        ));
    }

    #[test]
    fn malformed_responses_fail_closed() {
        let mut bad_magic = reply(STATUS_LOCKED, &[]);
        bad_magic[0] = b'X';
        let mut bad_version = reply(STATUS_LOCKED, &[]);
        bad_version[4] = 1;
        let mut trailing_string = tlv("boom");
        trailing_string.push(0xFF);
        let mut oversized = reply(STATUS_SECRET, &[]);
        oversized[6..8].copy_from_slice(&((MAX_PAYLOAD_SIZE + 1) as u16).to_be_bytes());

        let cases: [(&str, Vec<u8>); 10] = [
            ("bad magic", bad_magic),
            ("bad version", bad_version),
            ("oversized payload length", oversized),
            ("empty secret", reply(STATUS_SECRET, &[])),
            ("padded locked", reply(STATUS_LOCKED, &[0x01])),
            ("padded desynced", reply(STATUS_DESYNCED, &[0x01])),
            ("padded cancelled", reply(STATUS_CANCELLED, &[0x01])),
            ("empty access denied", reply(STATUS_ACCESS_DENIED, &[])),
            (
                "trailing error bytes",
                reply(STATUS_ERROR, &trailing_string),
            ),
            (
                "truncated frame",
                reply(STATUS_SECRET, &[0xAB; 32])[..HEADER_LEN + 8].to_vec(),
            ),
        ];
        for (label, bytes) in cases {
            let (result, _) = exchange(bytes);
            assert!(
                matches!(result, Err(NativeError::Io(_))),
                "{label}: expected an Io error, got {result:?}"
            );
        }
    }

    #[test]
    fn cancellation_before_connect_never_touches_the_socket() {
        let conn = SigilConnection::new("/nonexistent/sigil.sock");
        let result = conn.get_application_secret_cancellable("ns", "app", "p", Some(&|| true));
        assert!(matches!(result, Err(NativeError::Cancelled)));
    }

    #[test]
    fn cancellation_aborts_a_stalled_peer_and_delivers_eof() {
        let dir = tempfile::tempdir().expect("socket tempdir");
        let sock = dir.path().join("native.sock");
        let listener = UnixListener::bind(&sock).expect("bind fake sigil");

        let flag = Arc::new(AtomicBool::new(false));
        let server_flag = Arc::clone(&flag);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            read_request(&mut stream);
            // Never answer: cancel the client, then expect its EOF.
            server_flag.store(true, Ordering::SeqCst);
            let mut probe = [0u8; 1];
            matches!(stream.read(&mut probe), Ok(0))
        });

        let started = Instant::now();
        let is_cancelled = || flag.load(Ordering::SeqCst);
        let result = SigilConnection::new(&sock).get_application_secret_cancellable(
            "atrium.portal.Secret/v1",
            "org.test.Cancel",
            "master-secret",
            Some(&is_cancelled),
        );
        assert!(matches!(result, Err(NativeError::Cancelled)), "{result:?}");
        assert!(
            started.elapsed() < SERVER_READ_TIMEOUT,
            "cancellation must not wait for SOCKET_TIMEOUT"
        );
        assert!(
            server.join().expect("fake sigil thread"),
            "dropping the client stream must deliver EOF to sigil"
        );
    }
}
