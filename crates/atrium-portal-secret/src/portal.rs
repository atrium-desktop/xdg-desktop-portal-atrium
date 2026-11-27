//! `org.freedesktop.impl.portal.Secret` v1: the native, permanent secret
//! portal interface.
//!
//! Sandboxed applications retrieve a master secret through this interface;
//! the portal frontend then encrypts per-application secrets with it. The
//! served secret is derived from the sigil daemon via native IPC so
//! applications receive stable, mutually isolated keys. It is written to the
//! caller-supplied file descriptor rather than returned over D-Bus.
//!
//! Response codes follow the portal specification: 0 success, 1 cancelled, 2 other error.
//!
//! ## Admission: the sharpest edge of the private-backend boundary
//!
//! `app_id` arrives as a plain string and nothing in the protocol proves it.
//! For this interface that is not cosmetic: `app_id` is the *subject* handed to
//! sigil, so whoever can call this method chooses **which application's master
//! secret** it receives. Reading another application's master secret means
//! reading every credential that application stored.
//!
//! The boundary is therefore enforced before any work happens: the sender's
//! D-Bus unique name must be the portal frontend, which resolves the sandboxed
//! application's identity itself. See `atrium_portal_request::caller` for the
//! full argument.
//!
//! ## Why the work runs on a worker, not in the served method
//!
//! Retrieving a secret is unbounded in wall-clock time: when the vault is
//! locked, sigil raises an unlock prompt and answers only once the user types
//! a password. zbus 6 dispatches interface methods as tasks on the
//! connection's runtime, so any indefinite wait made *inside* the served
//! method pins that runtime and takes every other interface down with it —
//! Settings, ScreenCast, FileChooser, all of them. Blocking there is equally
//! wrong (`block_on` from a task panics; see the Inhibit interface).
//!
//! The work therefore follows the same shape as every other interface that
//! can block (ADR-0028): the method exports the `Request` object, dispatches a
//! job onto a supervised bounded queue, and awaits a one-slot reply. The
//! waiting thread is the worker's, which by construction owes nothing to the
//! connection runtime.
//!
//! ## Cancellation
//!
//! `Request.Close` marks the request closed in the shared tracker. The worker
//! observes that before starting, and again after sigil answers, so a caller
//! that gave up answers 1 (cancelled) instead of being handed a secret it no
//! longer wants. The descriptor is closed by drop on every path, including
//! the cancelled one.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, IntoRawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use crate::native::{NativeError, SigilConnection};
use atrium_portal_request::{PortalResponse, RequestTracker, ResponseSender, sync};
use zbus::{Fd, ObjectPath, Variant};

/// The served interface version.
pub(crate) const SECRET_VERSION: u32 = 1;

/// Secrets are 32 bytes plus a small amount of framing slack. The bound
/// exists so a broken or hostile daemon cannot make this process allocate
/// arbitrarily; it is not a secret-length policy (sigil owns that).
const MAX_SECRET_BYTES: usize = 4096;

/// One dispatched retrieve request.
pub(crate) enum SecretJob {
    Retrieve {
        request_path: String,
        app_id: String,
        /// The caller's descriptor, already duplicated into this process.
        fd: std::os::fd::OwnedFd,
        reply: ResponseSender,
    },
}

/// The served secret interface.
pub(crate) struct SecretIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<SecretJob>,
    /// Admission: only the portal frontend may call this private backend.
    pub(crate) callers: Arc<atrium_portal_request::CallerGuard>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Secret")]
impl SecretIface {
    /// `RetrieveSecret(o handle, s app_id, h fd, a{sv} options)` with the
    /// spec-conformant `(response, results)` reply.
    async fn retrieve_secret(
        &self,
        handle: ObjectPath<'_>,
        app_id: String,
        fd: Fd<'_>,
        _options: HashMap<String, Variant<'_>>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, Variant<'static>>)> {
        // Admission first: nothing below may run for any other peer. This
        // must come before the descriptor is touched and before sigil is
        // asked for a secret.
        self.callers
            .admit(header.sender().map(|sender| sender.as_str()))
            .map_err(zbus::fdo::Error::from)?;
        let path = handle.as_str().to_string();
        log::info!("portal: RetrieveSecret for '{app_id}' at {path}");

        // Duplicate the caller's descriptor *before* dispatching: the `Fd`
        // borrows the message and cannot outlive this call, and a clone
        // failure is a caller error, not something to hand to the worker.
        let owned_fd = match fd.as_fd().try_clone_to_owned() {
            Ok(owned) => owned,
            Err(error) => {
                log::warn!("portal: could not clone the RetrieveSecret fd: {error}");
                return Ok((2, HashMap::new()));
            }
        };

        atrium_portal_request::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "secret",
            &self.jobs,
            |reply| SecretJob::Retrieve {
                request_path: path.clone(),
                app_id: app_id.clone(),
                fd: owned_fd,
                reply,
            },
        )
        .await
    }

    /// The backend contract level.
    ///
    /// A property cannot apply admission itself: zbus refuses to register an
    /// accessor that takes a message header, so `Properties.Get` is answered
    /// without consulting the caller. That is harmless here and everywhere
    /// else in this backend because a property value is a protocol constant,
    /// and every interface's *methods* are admission-checked. The rule that
    /// follows is a standing constraint: anything sensitive must be a method,
    /// never a property.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        SECRET_VERSION
    }
}

/// The supervised worker: fan each request out to its own task thread.
///
/// A worker that served jobs serially would let one caller take the whole
/// interface down. `write_all` to a caller-supplied descriptor has no timeout —
/// a pipe already at capacity with no reader draining it blocks indefinitely —
/// and supervision only restarts a worker that *panics*, so a blocked worker
/// stays blocked for the rest of the session. Every later `RetrieveSecret`
/// would queue behind it and then be refused once the bounded queue filled,
/// for every application at once.
///
/// A thread per request, bounded by the concurrency cap, keeps one wedged
/// caller to one leaked thread. This is the shape the wallpaper and print
/// workers use, for the same reason.
pub(crate) fn secret_worker(rx: &mpsc::Receiver<SecretJob>, tracker: Arc<Mutex<RequestTracker>>) {
    /// Concurrent retrievals. Unlock prompting is user interaction, so this
    /// is a ceiling on leaked threads rather than a throughput target.
    const MAX_ACTIVE_RETRIEVALS: usize = 8;

    struct ActiveGuard(Arc<AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    let active = Arc::new(AtomicUsize::new(0));
    while let Ok(SecretJob::Retrieve {
        request_path,
        app_id,
        fd,
        reply,
    }) = rx.recv()
    {
        if active.fetch_add(1, Ordering::AcqRel) >= MAX_ACTIVE_RETRIEVALS {
            active.fetch_sub(1, Ordering::AcqRel);
            log::warn!("portal: refusing RetrieveSecret: concurrency limit reached");
            let _ = reply.send_blocking((2, HashMap::new()));
            continue;
        }
        let task_tracker = Arc::clone(&tracker);
        let guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("atrium-portal-secret-task".to_owned())
            .spawn(move || {
                let _active = guard;
                let response = run_retrieve(&task_tracker, &request_path, &app_id, fd);
                // A dropped receiver means the caller's D-Bus call is gone;
                // the task must not treat that as a reason to stop serving.
                let _ = reply.send_blocking(response);
            })
        {
            log::error!("portal: could not spawn the secret task: {error}");
            let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
        }
    }
}

/// Resolve one request to a portal response tuple.
fn run_retrieve(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    app_id: &str,
    owned_fd: std::os::fd::OwnedFd,
) -> PortalResponse {
    if sync::lock(tracker, "secret tracker").was_closed(request_path) {
        log::info!("portal: RetrieveSecret at {request_path} cancelled before the vault was asked");
        return (1, HashMap::new());
    }

    let connection = match SigilConnection::connect_default() {
        Ok(connection) => connection,
        Err(error) => {
            log::error!("portal: failed to locate the sigil socket: {error}");
            return (2, HashMap::new());
        }
    };

    let is_cancelled = || sync::lock(tracker, "secret tracker").was_closed(request_path);
    let secret = match connection.get_application_secret_cancellable(
        "atrium.portal.Secret/v1",
        app_id,
        "master-secret",
        Some(&is_cancelled),
    ) {
        Ok(secret) => secret,
        Err(NativeError::Locked) => {
            log::info!("portal: sigil is locked for '{app_id}'");
            return (1, HashMap::new());
        }
        Err(NativeError::Desynced) => {
            log::error!(
                "portal: sigil vault credentials are desynchronized for '{app_id}'; \
                     run 'sigil recover' to re-synchronize"
            );
            return (2, HashMap::new());
        }
        Err(NativeError::Cancelled) => {
            log::info!("portal: unlock cancelled for '{app_id}'");
            return (1, HashMap::new());
        }
        Err(NativeError::TimedOut) => {
            log::error!(
                "portal: sigil did not answer RetrieveSecret for '{app_id}' within the socket \
                 timeout; treating the vault as unavailable"
            );
            return (2, HashMap::new());
        }
        Err(error) => {
            log::error!("portal: error retrieving secret for '{app_id}': {error}");
            return (2, HashMap::new());
        }
    };

    // The user may have given up while sigil was prompting. Answering 1 keeps
    // the contract truthful, and dropping `secret` wipes it (ADR-0022).
    if sync::lock(tracker, "secret tracker").was_closed(request_path) {
        log::info!(
            "portal: RetrieveSecret at {request_path} cancelled while the vault was unlocking"
        );
        return (1, HashMap::new());
    }

    if secret.len() > MAX_SECRET_BYTES {
        log::error!(
            "portal: sigil returned a {}-byte secret for '{app_id}', above the {MAX_SECRET_BYTES}-byte bound",
            secret.len()
        );
        return (2, HashMap::new());
    }

    match write_secret(owned_fd, secret) {
        Ok(()) => (0, HashMap::new()),
        Err(error) => {
            log::warn!("portal: could not deliver the secret for '{app_id}': {error}");
            (2, HashMap::new())
        }
    }
}

/// Write `secret` into the caller's descriptor and flush it if it is a file.
///
/// Takes ownership of `fd` so it is closed on every path, including the error
/// ones: a leaked descriptor here would be visible to the caller as a
/// permanently open pipe.
///
/// Bounded on readiness: writing to a caller-supplied pipe or socket must not
/// block this worker thread indefinitely if the client fails to drain it.
fn write_secret(
    owned_fd: std::os::fd::OwnedFd,
    secret: zeroize::Zeroizing<Vec<u8>>,
) -> std::io::Result<()> {
    let raw = owned_fd.into_raw_fd();
    // SAFETY: `raw` came from `into_raw_fd` on the line above, which transfers
    // ownership of exactly one descriptor to us; `File` takes it back and
    // closes it on drop. No other code holds or closes it.
    let mut file = unsafe { File::from_raw_fd(raw) };

    // Bound wait for descriptor readiness: 5 seconds is generous for a local
    // pipe or file to accept at most 4 KiB of secret material. If the client
    // passes a full pipe and never reads it, fail rather than parking the task.
    let mut pfd = libc::pollfd {
        fd: raw,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: poll reads one pollfd struct and does not retain the pointer.
    let ready = unsafe { libc::poll(&mut pfd, 1, 5000) };
    if ready < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if ready == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out waiting for client descriptor to accept secret write",
        ));
    }

    file.write_all(secret.as_slice())?;
    // Explicit drop so the plaintext leaves process memory as early as
    // possible (ADR-0022), before the (potentially slow) flush below.
    drop(secret);
    // Only regular files can fsync; the caller-supplied fd may be a pipe,
    // which rejects fsync with EINVAL.
    match file.metadata() {
        Ok(metadata) if metadata.is_file() => file.sync_all(),
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_secret_delivers_to_pipe_and_closes_it() {
        let (reader, writer) = std::io::pipe().unwrap();
        let secret = zeroize::Zeroizing::new(vec![42u8; 32]);
        write_secret(writer.into(), secret).unwrap();

        let mut buf = Vec::new();
        let mut reader = reader;
        std::io::Read::read_to_end(&mut reader, &mut buf).unwrap();
        assert_eq!(buf, vec![42u8; 32]);
    }

    #[test]
    fn write_secret_handles_broken_pipe_without_hanging() {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader); // Broken pipe
        let secret = zeroize::Zeroizing::new(vec![42u8; 32]);
        let err = write_secret(writer.into(), secret).expect_err("broken pipe must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
