//! Native `org.freedesktop.impl.portal.Secret` adapter for Tessera.
//!
//! Sandboxed applications retrieve their per-application master secret
//! through this portal backend interface, which delegates storage, unlock,
//! and lock-state authority to the sigil daemon (ADR-0020) via the native
//! IPC client. The adapter owns only the D-Bus projection; a locked or
//! unavailable sigil daemon surfaces at call time as the portal response
//! codes, never as a missing advertised interface.

pub mod native;
pub mod portal;

use std::sync::{Arc, Mutex};

use atrium_portal_request::{RequestTracker, Supervisor};

/// Errors occurring in the portal secret adapter.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// The Secret service adapter instance registered with the portal runtime.
///
/// Owns the supervised worker queue the served interface dispatches onto
/// (see `portal` for why the retrieval cannot run in the served method).
pub struct SecretService {
    supervisor: Supervisor<portal::SecretJob>,
    jobs: std::sync::mpsc::SyncSender<portal::SecretJob>,
    callers: Arc<atrium_portal_request::CallerGuard>,
}

impl SecretService {
    /// `callers` is the shared admission guard: only the portal frontend may
    /// reach this private backend (see `atrium_portal_request::caller`).
    pub fn new(callers: Arc<atrium_portal_request::CallerGuard>) -> Self {
        let supervisor = Supervisor::new("secret");
        let jobs = supervisor.sender();
        Self {
            supervisor,
            jobs,
            callers,
        }
    }

    /// Export the Secret interface at `path` and start its worker.
    ///
    /// Takes the async `zbus::Connection` (zbus 6 has no blocking one) and an
    /// [`atrium_portal_request::Driver`] to install the object on the thread
    /// that drives that connection: the caller is a synchronous assembly path
    /// and must not drive the connection itself.
    ///
    /// The worker is spawned through `spawn_worker` so the assembly code keeps
    /// one accounting of the threads this process runs.
    pub fn register_portal(
        &self,
        driver: &atrium_portal_request::Driver,
        conn: &zbus::Connection,
        tracker: Arc<Mutex<RequestTracker>>,
        path: &'static str,
    ) -> Result<(), SecretError> {
        let iface = portal::SecretIface {
            conn: conn.clone(),
            tracker: tracker.clone(),
            jobs: self.jobs.clone(),
            callers: Arc::clone(&self.callers),
        };
        driver
            .call(move |conn| {
                atrium_portal_request::block_on(async {
                    conn.object_server().at(path, iface).await
                })
            })
            .map_err(|e| SecretError::Io(std::io::Error::other(e.to_string())))?;
        Ok(())
    }

    /// Run the worker loop. Blocking; call it on a dedicated thread via
    /// `spawn_worker`.
    ///
    /// Consumes the supervisor, so this can only be called once.
    pub fn serve(self, tracker: Arc<Mutex<RequestTracker>>) {
        self.supervisor
            .serve(|rx| portal::secret_worker(rx, Arc::clone(&tracker)));
    }
}
