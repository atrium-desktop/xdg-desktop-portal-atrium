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

use atrium_portal_request::RequestTracker;

/// Errors occurring in the portal secret adapter.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// The Secret service adapter instance registered with the portal runtime.
#[derive(Clone, Default)]
pub struct SecretService;

impl SecretService {
    pub fn new() -> Self {
        Self
    }

    /// Export the Secret interface at `path`.
    ///
    /// Takes the async `zbus::Connection` (zbus 6 has no blocking one) and an
    /// [`atrium_portal_request::Driver`] to install the object on the thread
    /// that drives that connection: the caller is a synchronous assembly path
    /// and must not drive the connection itself.
    pub fn register_portal(
        &self,
        driver: &atrium_portal_request::Driver,
        conn: &zbus::Connection,
        tracker: Arc<Mutex<RequestTracker>>,
        path: &'static str,
    ) -> Result<(), SecretError> {
        let iface = portal::SecretIface {
            conn: conn.clone(),
            tracker,
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
}
