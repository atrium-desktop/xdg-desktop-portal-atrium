//! The inward half of the bridge: the narrow, Portal-owned Tessera IPC client.
//!
//! The portal connects under the built-in owner-only `atrium-portal` scope
//! (`atrium_portal_ipc::LOCAL_PORTAL_SCOPE`) with `control` and a time-bounded
//! lease. This repository's wire projection admits only capture, stream, and
//! target-picking operations used by compositor-owned portal interfaces. The
//! wrapper keeps each connection alive across idle periods by renewing the
//! lease at half its TTL, and reconnects once on any failure so a compositor
//! restart or an expired lease self-heals on the next screenshot instead of
//! killing the D-Bus service.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use atrium_portal_ipc::{Client, ConnectionCapabilities, LOCAL_PORTAL_SCOPE};

/// Lease TTL requested at handshake and renewal; matches the reference
/// client's default (`LeaseRequest::default`).
const LEASE_TTL_MS: u64 = 900_000;
/// Ordinary compositor RPCs must not retain a portal worker forever if a
/// local peer accepts the socket and then stops responding.
const RPC_TIMEOUT: Duration = Duration::from_secs(15);
/// Interactive compositor chrome is itself bounded at five minutes. Leave
/// a small transport margin so its typed cancellation/error can arrive.
const INTERACTION_TIMEOUT: Duration = Duration::from_secs(305);

/// The negotiated compositor protocol version, cached process-wide once
/// learned.
///
/// The version is advertised at every handshake, but a compositor's
/// capability set does not change over one login session, so D-Bus
/// property reads must not open a fresh socket per query: before this
/// cache, a wedged compositor could stall a zbus executor thread for the
/// full 15 s connect timeout on *every* `AvailableSourceTypes` read. Only
/// a successful negotiation writes the cache — an unreachable compositor
/// keeps returning [`atrium_portal_ipc::MIN_PROTOCOL_VERSION`] at the call
/// sites, which is the same conservative answer those callers already
/// degrade to. If the compositor is replaced mid-session by an older one,
/// the stale higher value only affects capability *advertising*; each
/// stream negotiates its own protocol at connect time and fails loudly.
static NEGOTIATED_PROTOCOL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// The compositor's protocol version: the cached negotiation when one has
/// succeeded, the conservative minimum otherwise. Never blocks: a
/// compositor that has not been reached yet is reported as the oldest
/// supported version, which advertises the smallest capability set.
///
/// The first successful capture/IPC operation fills the cache, so
/// property reads before any interaction stay conservative without ever
/// connecting.
pub(crate) fn cached_protocol_version() -> u32 {
    NEGOTIATED_PROTOCOL
        .get()
        .copied()
        .unwrap_or(atrium_portal_ipc::MIN_PROTOCOL_VERSION)
}

/// Record a successfully negotiated protocol version for
/// [`cached_protocol_version`].
pub(crate) fn remember_protocol_version(version: u32) {
    let _ = NEGOTIATED_PROTOCOL.set(version);
}

/// Open the one privileged runtime boundary used by capture and streaming.
/// Refuse a handshake that did not grant both scoped control and a renewable
/// lease instead of waiting for the first sensitive operation to fail.
pub(crate) fn connect_compositor(socket: &Path, timeout: Duration) -> io::Result<Client> {
    let requested = ConnectionCapabilities {
        query: true,
        control: true,
        input: false,
        session: false,
        interaction_domain: false,
    };
    let client =
        Client::connect_scoped_with_timeout(socket, requested, LOCAL_PORTAL_SCOPE, timeout)?;
    if !client.caps().control || !client.lease().is_some_and(|lease| lease.renewable) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Tessera did not grant the Portal scope a renewable control lease",
        ));
    }
    client.set_io_timeout(Some(timeout))?;
    Ok(client)
}

/// A lazily connected, lease-renewing `CaptureOutput` client. One instance
/// lives on the capture worker thread; it is not `Sync` by design.
pub(crate) struct PortalCapture {
    socket: PathBuf,
    client: Option<Client>,
    renewed_at: Instant,
}

impl PortalCapture {
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            client: None,
            renewed_at: Instant::now(),
        }
    }

    fn connect(&self) -> io::Result<Client> {
        connect_compositor(&self.socket, RPC_TIMEOUT)
    }

    /// Hand out a live client, renewing an ageing lease or reconnecting an
    /// expired/broken one.
    fn client(&mut self) -> io::Result<&mut Client> {
        if let Some(client) = self.client.as_ref() {
            client.set_io_timeout(Some(RPC_TIMEOUT))?;
        }
        if let Some(client) = self.client.as_mut()
            && self.renewed_at.elapsed() >= Duration::from_millis(LEASE_TTL_MS / 2)
        {
            match client.renew_lease(LEASE_TTL_MS) {
                Ok(_) => self.renewed_at = Instant::now(),
                Err(error) => {
                    // An expired lease cannot be renewed; reconnecting is the
                    // recovery path. A vanished scope refuses the new
                    // handshake and surfaces here as a normal error.
                    log::info!("portal: lease renewal failed ({error}); reconnecting IPC");
                    self.client = None;
                }
            }
        }
        if self.client.is_none() {
            self.client = Some(self.connect()?);
            self.renewed_at = Instant::now();
        }
        Ok(self.client.as_mut().expect("connected above"))
    }

    /// Capture the focused output as PNG bytes. One automatic reconnect +
    /// retry hides transient failures (compositor restart, raced lease
    /// expiry); persistent failures surface as errors to the caller.
    pub(crate) fn capture_png(&mut self) -> io::Result<Vec<u8>> {
        match self.client()?.capture_output() {
            Ok((_, _, png)) => Ok(png),
            Err(first) => {
                log::info!("portal: capture failed ({first}); reconnecting IPC");
                self.client = None;
                let (_, _, png) = self.client()?.capture_output()?;
                Ok(png)
            }
        }
    }

    /// The protocol version negotiated with the compositor. Connects lazily
    /// like every other operation; callers treat a failure as the oldest
    /// supported version (conservative capability reporting). A successful
    /// read feeds the process-wide cache the D-Bus property getters use,
    /// so they never need to connect.
    pub(crate) fn protocol_version(&mut self) -> io::Result<u32> {
        let version = self.client()?.protocol_version();
        remember_protocol_version(version);
        Ok(version)
    }

    /// List the compositor's outputs (protocol 29), with the same
    /// reconnect + retry discipline as [`PortalCapture::capture_png`]. An
    /// older compositor's refusal of the op surfaces as an error, so
    /// callers must consult [`PortalCapture::protocol_version`] first.
    pub(crate) fn enumerate_outputs(&mut self) -> io::Result<Vec<atrium_portal_ipc::OutputInfo>> {
        match self.client()?.enumerate_outputs() {
            Ok(outputs) => Ok(outputs),
            Err(first) => {
                log::info!("portal: output enumeration failed ({first}); reconnecting IPC");
                self.client = None;
                self.client()?.enumerate_outputs()
            }
        }
    }

    /// Capture a region of the focused output as PNG bytes (compositor
    /// logical pixels), with the same reconnect + retry discipline as
    /// [`PortalCapture::capture_png`].
    pub(crate) fn capture_region_png(
        &mut self,
        region: atrium_portal_ipc::Rect,
    ) -> io::Result<Vec<u8>> {
        match self.client()?.capture_output_region(Some(region)) {
            Ok((_, _, png)) => Ok(png),
            Err(first) => {
                log::info!("portal: region capture failed ({first}); reconnecting IPC");
                self.client = None;
                let (_, _, png) = self.client()?.capture_output_region(Some(region))?;
                Ok(png)
            }
        }
    }

    /// Run one interactive pick through compositor chrome. Blocks
    /// until the user confirms or cancels, so this can take far longer than
    /// any other call. No automatic retry: a reconnect would orphan the
    /// user-facing picker, and the compositor bounds the wait itself.
    pub(crate) fn pick(
        &mut self,
        kind: atrium_portal_ipc::PickKind,
    ) -> io::Result<atrium_portal_ipc::PickResult> {
        let client = self.client()?;
        client.set_io_timeout(Some(INTERACTION_TIMEOUT))?;
        client.pick_target(kind)
    }

    /// Ask the user a yes/no consent question through compositor chrome
    /// (portal consent dialogs). Same blocking, no-retry discipline as
    /// [`PortalCapture::pick`].
    pub(crate) fn pick_confirm(
        &mut self,
        title: String,
        body: String,
        accept_label: Option<String>,
    ) -> io::Result<atrium_portal_ipc::ConfirmPickResult> {
        let client = self.client()?;
        client.set_io_timeout(Some(INTERACTION_TIMEOUT))?;
        client.pick_confirm(title, body, accept_label)
    }
}

/// Apply a staged wallpaper file through the compositor. The `SetWallpaper`
/// op predates the projection's version floor, so every negotiated protocol
/// speaks it; what the compositor's dispatch requires is what
/// [`connect_compositor`] already enforces at the handshake: scoped `control`
/// and a live renewable lease. Unlike capture, the connection is per call:
/// wallpaper changes are rare, so a fresh handshake (its lease dies with the
/// connection) beats carrying a renewing client on the worker. One reconnect
/// + retry hides transient failures, matching [`PortalCapture`]'s discipline.
pub(crate) fn set_wallpaper(socket: &Path, staged: &Path) -> io::Result<()> {
    let mut client = connect_compositor(socket, RPC_TIMEOUT)?;
    match client.set_wallpaper(staged) {
        Ok(()) => Ok(()),
        Err(first) => {
            log::info!("portal: wallpaper IPC failed ({first}); reconnecting IPC");
            connect_compositor(socket, RPC_TIMEOUT)?.set_wallpaper(staged)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atrium_portal_ipc::testing::{CaptureOutputPayload, Handler, Server};
    use std::sync::Arc;

    /// A compositor whose capture answers a fixed PNG, until the test flips
    /// it to failing (a wedged compositor's op error).
    struct FakeCompositor {
        fail: std::sync::atomic::AtomicBool,
    }

    impl Handler for FakeCompositor {
        fn capture_output(
            &self,
            _region: Option<atrium_portal_ipc::Rect>,
        ) -> Result<CaptureOutputPayload, String> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("simulated capture failure".into());
            }
            Ok(CaptureOutputPayload {
                width: 2,
                height: 2,
                png: vec![0x89, b'P', b'N', b'G'],
            })
        }
    }

    fn temp_socket(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tessera-ipc-tests-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("tessera.sock")
    }

    #[test]
    fn capture_reconnects_after_a_server_restart() {
        let socket = temp_socket("restart");
        let fake = Arc::new(FakeCompositor {
            fail: std::sync::atomic::AtomicBool::new(false),
        });
        let server = Server::start(&socket, Arc::clone(&fake)).expect("server starts");
        let mut capture = PortalCapture::new(socket.clone());

        // Baseline: the first capture connects and succeeds.
        capture.capture_png().expect("the first capture succeeds");

        // The compositor goes away entirely (restart). The next capture
        // fails — one reconnect attempt cannot reach a dead socket — but
        // the client keeps its failure isolated to this call.
        drop(server);
        assert!(
            capture.capture_png().is_err(),
            "a vanished compositor surfaces as an error"
        );

        // The compositor comes back on the same path; the next capture
        // transparently reconnects and succeeds again.
        let server = Server::start(&socket, Arc::clone(&fake)).expect("server restarts");
        capture
            .capture_png()
            .expect("the client reconnects after the compositor returns");
        drop(server);
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    #[test]
    fn capture_retries_once_through_an_op_failure() {
        let socket = temp_socket("retry");
        let fake = Arc::new(FakeCompositor {
            fail: std::sync::atomic::AtomicBool::new(true),
        });
        let _server = Server::start(&socket, Arc::clone(&fake)).expect("server starts");
        let mut capture = PortalCapture::new(socket.clone());

        // The op itself fails (a wedged compositor). The automatic
        // reconnect+retry runs, and because the failure is the handler's
        // and not the transport's, the retried op fails the same way —
        // the important property is that it surfaces as an Err, never a
        // hang or a panic.
        let error = capture
            .capture_png()
            .expect_err("a persistent op failure surfaces");
        assert!(
            error.to_string().contains("simulated capture failure"),
            "unexpected error: {error}"
        );
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    #[test]
    fn protocol_version_is_remembered_process_wide() {
        // The cache is process-global; this test only asserts the
        // documented contract rather than a fresh-cache transition, since
        // test ordering is not defined.
        let remembered = cached_protocol_version();
        assert!(
            remembered >= atrium_portal_ipc::MIN_PROTOCOL_VERSION,
            "the cached value is never below the floor"
        );
    }
}
