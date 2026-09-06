//! Portal-owned projection of the sigil daemon's native IPC wire protocol (sigil-wire-v2).
//!
//! The portal integrates with sigil at runtime through the narrow Unix
//! socket contract only — no sigil crate is imported, mirroring the
//! Tessera boundary (ADR-0004): each side owns its own implementation, so
//! a matching bug cannot pass a shared test.
//!
//! Under ADR-0022 and Sigil ADR-0004, the wire protocol is an uncompromising,
//! zero-allocation binary framing format (`sigil-wire-v2`):
//! - Header: 8 bytes `[b'S', b'I', b'G', b'L', version=2, opcode/status, u16_be payload_len]`
//! - Request: Compact TLV fields `[u16_be len][utf-8 bytes]` without JSON heap overhead.
//! - Response: Direct 32-byte secret payload wrapped in `Zeroizing` or typed status code.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use zeroize::Zeroizing;

/// The default socket subpath under `$XDG_RUNTIME_DIR`, matching the sigil
/// daemon's own default.
const SOCKET_SUBPATH: &str = "sigil/native.sock";

/// Wire protocol constants matching sigil-wire-v2
const WIRE_MAGIC: [u8; 4] = *b"SIGL";
const WIRE_VERSION: u8 = 2;
const HEADER_LEN: usize = 8;
const MAX_PAYLOAD_SIZE: usize = 4096;

const OP_GET_APPLICATION_SECRET: u8 = 0x01;

const STATUS_SECRET: u8 = 0x01;
const STATUS_LOCKED: u8 = 0x03;
const STATUS_CANCELLED: u8 = 0x05;
const STATUS_ACCESS_DENIED: u8 = 0x06;
const STATUS_ERROR: u8 = 0x07;

/// Errors occurring in the native IPC projection.
#[derive(Debug, thiserror::Error)]
pub enum NativeError {
    /// The daemon reported the vault is locked.
    #[error("sigil is locked")]
    Locked,
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
    /// The daemon answered with a variant this projection does not decode.
    #[error("unexpected sigil IPC response")]
    Unexpected,
}

/// A blocking connection factory for the sigil daemon's native socket.
#[derive(Clone)]
pub struct SigilConnection {
    socket_path: PathBuf,
}

impl SigilConnection {
    /// Connect to `$XDG_RUNTIME_DIR/sigil/native.sock`.
    pub fn connect_default() -> Result<Self, NativeError> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|dir| !dir.is_empty())
            .ok_or_else(|| {
                NativeError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "XDG_RUNTIME_DIR is unset",
                ))
            })?;
        Ok(Self {
            socket_path: PathBuf::from(runtime_dir).join(SOCKET_SUBPATH),
        })
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
        let mut stream = UnixStream::connect(&self.socket_path)?;

        // Encode binary request frame: [Header 8B][Payload]
        let mut frame = Vec::with_capacity(namespace.len() + subject.len() + purpose.len() + 14);
        frame.extend_from_slice(&WIRE_MAGIC);
        frame.push(WIRE_VERSION);
        frame.push(OP_GET_APPLICATION_SECRET);

        let mut payload = Vec::new();
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

        // Read 8-byte response header
        let mut header = [0u8; HEADER_LEN];
        stream.read_exact(&mut header)?;

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
        stream.read_exact(&mut payload_buf)?;

        match status {
            STATUS_SECRET => Ok(payload_buf),
            STATUS_LOCKED => Err(NativeError::Locked),
            STATUS_CANCELLED => Err(NativeError::Cancelled),
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

fn decode_string(buf: &[u8]) -> Result<String, NativeError> {
    if buf.len() < 2 {
        return Ok(String::new());
    }
    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    if buf.len() < 2 + len {
        return Err(NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Truncated string in response payload",
        )));
    }
    String::from_utf8(buf[2..2 + len].to_vec()).map_err(|e| {
        NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    })
}
