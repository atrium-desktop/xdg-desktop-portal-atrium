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

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, IntoRawFd};
use std::sync::{Arc, Mutex};

use crate::native::{NativeError, SigilConnection};
use atrium_portal_runtime::RequestTracker;
use atrium_portal_runtime::sync;
use zbus::zvariant::{Fd, ObjectPath, Value};

/// The served interface version.
pub(crate) const SECRET_VERSION: u32 = 1;

/// The served secret interface.
pub(crate) struct SecretIface {
    pub(crate) conn: zbus::blocking::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
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
        _options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, Value<'static>>)> {
        let path = handle.as_str().to_string();
        log::info!("portal: RetrieveSecret for '{app_id}' at {path}");

        atrium_portal_runtime::register(self.conn.inner(), &self.tracker, &path).await?;
        let response = self.run(&path, &app_id, fd).await;
        atrium_portal_runtime::finish(self.conn.inner(), &self.tracker, &path).await;
        response
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        SECRET_VERSION
    }
}

impl SecretIface {
    async fn run(
        &self,
        path: &str,
        app_id: &str,
        fd: Fd<'_>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, Value<'static>>)> {
        if sync::lock(&self.tracker, "secret tracker").was_closed(path) {
            log::info!("portal: RetrieveSecret at {path} cancelled by Request.Close");
            return Ok((1, HashMap::new()));
        }

        let owned_fd = match fd.as_fd().try_clone_to_owned() {
            Ok(owned) => owned,
            Err(error) => {
                log::warn!("portal: could not clone the RetrieveSecret fd: {error}");
                return Ok((2, HashMap::new()));
            }
        };

        let connection = match SigilConnection::connect_default() {
            Ok(c) => c,
            Err(e) => {
                log::error!("portal: failed to locate the sigil socket: {e}");
                return Ok((2, HashMap::new()));
            }
        };

        match connection.get_application_secret("atrium.portal.Secret/v1", app_id, "master-secret")
        {
            Ok(secret) => {
                let raw = owned_fd.into_raw_fd();
                let mut file = unsafe { File::from_raw_fd(raw) };
                let write_res = file.write_all(secret.as_slice());
                // Explicitly drop and zeroize the transient in-memory secret bytes
                // immediately after writing to the destination fd (ADR-0022).
                drop(secret);

                // Only regular files can fsync; the caller-supplied fd may
                // be a pipe, which rejects fsync with EINVAL.
                let sync_res = match file.metadata() {
                    Ok(metadata) if metadata.is_file() => file.sync_all(),
                    Ok(_) => Ok(()),
                    Err(error) => Err(error),
                };

                if let Err(e) = write_res {
                    log::warn!("portal: could not write the secret into the client fd: {e}");
                    return Ok((2, HashMap::new()));
                }
                if let Err(e) = sync_res {
                    log::warn!("portal: could not sync the client fd: {e}");
                    return Ok((2, HashMap::new()));
                }

                Ok((0, HashMap::new()))
            }
            Err(NativeError::Locked) => {
                log::info!("portal: sigil is locked for '{app_id}'");
                Ok((1, HashMap::new()))
            }
            Err(NativeError::Cancelled) => {
                log::info!("portal: unlock cancelled for '{app_id}'");
                Ok((1, HashMap::new()))
            }
            Err(e) => {
                log::error!("portal: error retrieving secret for '{app_id}': {e}");
                Ok((2, HashMap::new()))
            }
        }
    }
}
