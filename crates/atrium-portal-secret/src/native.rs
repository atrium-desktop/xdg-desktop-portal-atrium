//! Portal-owned projection of the sigil daemon's native IPC wire protocol.
//!
//! The portal integrates with sigil at runtime through the narrow Unix
//! socket contract only — no sigil crate is imported, mirroring the
//! Tessera boundary (ADR-0004): each side owns its own implementation, so
//! a matching bug cannot pass a shared test. The wire format is a u32
//! big-endian length prefix followed by a JSON payload, with externally
//! tagged request and response enums. A locked or absent daemon surfaces
//! as an error value; the D-Bus adapter maps it to the portal response
//! codes (ADR-0020).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The default socket subpath under `$XDG_RUNTIME_DIR`, matching the sigil
/// daemon's own default.
const SOCKET_SUBPATH: &str = "sigil/native.sock";

/// Mirrors sigil's frame-size guard so a runaway peer fails closed the
/// same way on both sides.
const MAX_FRAME_SIZE: usize = 64 * 1024;

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

/// The one operation the Secret portal needs, serialized exactly as
/// sigil's externally tagged `IpcRequest::GetApplicationSecret`.
#[derive(Serialize)]
enum NativeRequest<'a> {
    GetApplicationSecret {
        namespace: &'a str,
        subject: &'a str,
        purpose: &'a str,
    },
}

/// Every response variant sigil's server can answer `GetApplicationSecret`
/// with, serialized exactly as its externally tagged `IpcResponse`.
#[derive(Deserialize)]
enum NativeResponse {
    Secret(Vec<u8>),
    Locked,
    Cancelled,
    AccessDenied(String),
    Error(String),
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
        let request = serde_json::to_vec(&NativeRequest::GetApplicationSecret {
            namespace,
            subject,
            purpose,
        })
        .map_err(|error| {
            NativeError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })?;
        write_frame(&mut stream, &request)?;
        let payload = read_frame(&mut stream)?;
        let response: NativeResponse = serde_json::from_slice(&payload).map_err(|error| {
            NativeError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })?;
        match response {
            NativeResponse::Secret(bytes) => Ok(Zeroizing::new(bytes)),
            NativeResponse::Locked => Err(NativeError::Locked),
            NativeResponse::Cancelled => Err(NativeError::Cancelled),
            NativeResponse::AccessDenied(reason) => Err(NativeError::AccessDenied(reason)),
            NativeResponse::Error(message) => Err(NativeError::Daemon(message)),
        }
    }
}

fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> Result<(), NativeError> {
    if payload.len() > MAX_FRAME_SIZE {
        return Err(NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "request frame exceeds the sigil frame limit",
        )));
    }
    stream.write_all(&(payload.len() as u32).to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

fn read_frame(stream: &mut UnixStream) -> Result<Zeroizing<Vec<u8>>, NativeError> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response frame exceeds the sigil frame limit",
        )));
    }
    let mut payload = Zeroizing::new(vec![0u8; len]);
    stream.read_exact(&mut payload)?;
    Ok(payload)
}
