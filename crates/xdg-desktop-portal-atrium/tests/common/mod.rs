//! Shared helpers for the xdg-desktop-portal-atrium end-to-end tests.
//!
//! Every test gets its OWN private session bus (a spawned `dbus-daemon`)
//! and spawns the daemon with `DBUS_SESSION_BUS_ADDRESS` pointing at it.
//! The ambient environment of a developer machine points at the live
//! session bus; tests must never claim well-known names there, so nothing
//! here reads the ambient bus address.
//!
//! Cargo compiles this module into every test binary, so helpers not used
//! by one binary warn as dead code; the blanket allow keeps each test file
//! free to use only what it needs. `unreachable_pub` is allowed for the same
//! reason: this module is not a library root, but every test binary links it
//! as one, so its items are legitimately `pub` within each binary.
#![allow(dead_code, unreachable_pub)]

pub mod bus;
pub mod fake_compositor;

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// CI/release validation requests hard failures instead of optional skips.
pub fn e2e_required() -> bool {
    std::env::var_os("ATRIUM_PORTAL_REQUIRE_E2E").is_some()
}

fn unavailable(message: &str) -> Option<PrivateBus> {
    assert!(
        !e2e_required(),
        "required E2E prerequisite failed: {message}"
    );
    None
}

/// Arm a `PR_SET_PDEATHSIG` on the child, so the kernel kills it when this
/// process dies, however it dies.
///
/// Normal teardown runs [`PrivateBus`]'s `Drop`. That is not enough: a test
/// killed for exceeding its timeout, or a test binary that aborts, leaves the
/// `dbus-daemon` behind, and a full run then accumulates one orphan per
/// failure. `PR_SET_PDEATHSIG` is the only teardown that survives `SIGKILL`,
/// because the kernel — not the dead parent — delivers it.
///
/// The signal is set *in the child*, between `fork` and `exec`, which is what
/// `CommandExt::pre_exec` exists for; calling `prctl` in the parent would
/// instead arm the parent itself.
pub fn die_with_parent(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: the closure runs in the forked child before `exec`. It calls
    // only async-signal-safe functions (`prctl`, `getppid`, `_exit`) and
    // touches no memory shared with the parent beyond the return value, which
    // is the documented contract for `pre_exec`.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // If the parent died between `fork` and the `prctl` above, the
            // death signal has already fired for nobody and will never
            // arrive again: exit instead of lingering as an orphan.
            if libc::getppid() == 1 {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

/// A private session bus; killed on drop.
pub struct PrivateBus {
    address: String,
    child: Child,
}

impl PrivateBus {
    /// The daemon and every client must connect here, never to the ambient
    /// session bus.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// A blocking zbus connection to this bus.
    pub fn connect(&self) -> zbus::Connection {
        bus::connect(self.address.as_str())
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn a private `dbus-daemon --session`; `None` when dbus-daemon is not
/// installed (tests skip).
pub fn private_bus() -> Option<PrivateBus> {
    let mut command = Command::new("dbus-daemon");
    command
        .args(["--session", "--nofork", "--print-address=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    die_with_parent(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return unavailable(&format!("could not spawn dbus-daemon: {error}")),
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return unavailable("dbus-daemon stdout was not piped");
    };
    let mut line = String::new();
    if let Err(error) = BufReader::new(stdout).read_line(&mut line) {
        let _ = child.kill();
        let _ = child.wait();
        return unavailable(&format!("could not read dbus-daemon address: {error}"));
    }
    let address = line.trim().to_string();
    if address.is_empty() {
        let _ = child.kill();
        let _ = child.wait();
        return unavailable("dbus-daemon returned an empty address");
    }
    Some(PrivateBus { address, child })
}

/// Spawn the portal daemon bound to the private bus and hermetic XDG dirs.
/// `ATRIUM_PORTAL_E2E_DAEMON_LOG=<path>` captures the daemon's stderr into
/// that file for debugging hung flows.
pub fn spawn_daemon(bus: &PrivateBus, data: &PathBuf, runtime: &PathBuf) -> Child {
    daemon_command(bus, data, runtime)
        .spawn()
        .expect("spawn xdg-desktop-portal-atrium")
}

/// Construct the hermetic daemon command so a test can add interface-specific
/// environment before spawning it.
pub fn daemon_command(bus: &PrivateBus, data: &PathBuf, runtime: &PathBuf) -> Command {
    let stderr: Stdio = match std::env::var_os("ATRIUM_PORTAL_E2E_DAEMON_LOG") {
        Some(path) => Stdio::from(std::fs::File::create(path).expect("create daemon log file")),
        None => Stdio::null(),
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_xdg-desktop-portal-atrium"));
    die_with_parent(&mut command);
    command
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_DATA_HOME", data)
        .env("XDG_CACHE_HOME", data.join("cache"))
        // Keep resolution hermetic: without this, `XDG_DATA_DIRS` falls back to
        // `/usr/local/share:/usr/share` and the daemon resolves the host's
        // installed applications instead of the test fixture.
        .env("XDG_DATA_DIRS", data.join("data-dirs"))
        .env("XDG_CONFIG_HOME", data.join("config"))
        .env("XDG_RUNTIME_DIR", runtime)
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        )
        .stdout(Stdio::null())
        .stderr(stderr);
    command
}

/// Kill a daemon child on drop.
pub struct KillOnDrop(pub Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A unique temp directory tagged by test name and pid.
pub fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "xdg-desktop-portal-atrium-e2e-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Create a pipe-compatible, one-shot chooser fixture (`arca
/// --chooser-prompt`, ADR-0027). Each invocation records its request before
/// returning the correspondingly numbered reply.
pub fn fake_chooser(directory: &std::path::Path) -> PathBuf {
    let path = directory.join("fake-prompter");
    std::fs::write(
        &path,
        r#"#!/bin/sh
set -eu
fixture="${ATRIUM_PROMPTER_FIXTURE:?}"
count_file="$fixture/count"
while ! mkdir "$fixture/count-lock" 2>/dev/null; do :; done
if test -f "$count_file"; then
    index=$(cat "$count_file")
else
    index=0
fi
index=$((index + 1))
printf '%s\n' "$index" > "$count_file"
rmdir "$fixture/count-lock"
cat > "$fixture/request-$index.json"
if test -f "$fixture/response-$index.json"; then
    cat "$fixture/response-$index.json"
else
    exec sleep 30
fi
"#,
    )
    .expect("write fake prompter");
    let mut permissions = std::fs::metadata(&path)
        .expect("fake prompter metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions).expect("make fake prompter executable");
    path
}

/// Locate the real `xdg-desktop-portal` frontend binary.
pub fn frontend_binary() -> Option<&'static std::path::Path> {
    [
        "/usr/libexec/xdg-desktop-portal",
        "/usr/lib/xdg-desktop-portal",
    ]
    .into_iter()
    .map(std::path::Path::new)
    .find(|path| path.is_file())
}

/// Locate `xdg-document-portal`, which the frontend insists on activating
/// before it claims `org.freedesktop.portal.Desktop`.
fn document_portal_binary() -> Option<&'static std::path::Path> {
    [
        "/usr/libexec/xdg-document-portal",
        "/usr/lib/xdg-document-portal",
    ]
    .into_iter()
    .map(std::path::Path::new)
    .find(|path| path.is_file())
}

/// Whether the frontend can actually start against `bus`.
///
/// The frontend claims `org.freedesktop.portal.Desktop` only after
/// `StartServiceByName` succeeds for `org.freedesktop.impl.portal.PermissionStore`
/// and `org.freedesktop.portal.Documents`. A private `dbus-daemon` without a
/// systemd session manager has no activation helper, so those calls are
/// accepted and never answered: the frontend sits for its 25 s timeout and
/// then aborts with "No document portal".
///
/// The decisive question is therefore not "is systemd installed" but "did the
/// bus answer an activation request". This probes the bus directly, in
/// milliseconds, so callers can skip with a truthful reason.
pub fn frontend_prerequisites(bus: &PrivateBus) -> Result<(), &'static str> {
    if frontend_binary().is_none() {
        return Err("xdg-desktop-portal frontend is not installed");
    }
    if document_portal_binary().is_none() {
        return Err("xdg-document-portal is not installed");
    }
    let conn = bus.connect();
    let fdo = bus::proxy(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    );
    // A service that never answers distinguishes an inert bus from a live one.
    // An activation request for a name nobody serves returns promptly on a bus
    // with an activation helper, and hangs without one, so bound it.
    let activated: Result<(u32,), _> = {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match bus::try_call(
                &fdo,
                "StartServiceByName",
                &("org.freedesktop.portal.ProbeNobodyServes", 0u32),
            ) {
                Ok(reply) => break Ok(reply),
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => break Err(error),
            }
        }
    };
    match activated {
        // `2` means "already running", which is impossible here; anything the
        // bus answers at all proves an activation helper exists.
        Ok((_,)) => Ok(()),
        Err(_) => Err("the session bus has no service-activation helper"),
    }
}

/// Wait up to `bound` for `name`; returns whether it appeared.
fn name_appears(conn: &zbus::Connection, name: &str, bound: Duration) -> bool {
    let fdo = bus::proxy(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    );
    let deadline = Instant::now() + bound;
    loop {
        if bus::call::<_, bool>(&fdo, "NameHasOwner", &(name,)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Wait for the frontend to claim its well-known name.
///
/// Returns `false` rather than panicking when it never appears: the frontend
/// can legitimately be unable to start (see [`frontend_prerequisites`]), and a
/// caller should report "skipped" instead of an opaque timeout. The bound is
/// short on purpose — the prerequisite check above is what decides whether the
/// frontend *can* start, and this only covers the residual race.
pub fn wait_for_frontend(conn: &zbus::Connection) -> bool {
    name_appears(
        conn,
        "org.freedesktop.portal.Desktop",
        Duration::from_secs(10),
    )
}

/// Wait until the daemon owns `name` (10 s bound).
pub fn wait_for_name(conn: &zbus::Connection, name: &str) {
    let fdo = bus::proxy(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let owned: bool = bus::call(&fdo, "NameHasOwner", &(name,));
        if owned {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the daemon to own {name}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A plain pipe pair — the fd shape real `RetrieveSecret` clients (Chrome,
/// libportal) pass, NOT a socket. Guards the regression where the backend
/// used the socket-only `shutdown(2)` and failed ENOTSOCK on pipes.
pub fn pipe_pair() -> (std::fs::File, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let mut fds = [-1; 2];
    // SAFETY: fds is a valid out-array; on success both ends are owned fds.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe(2)");
    // SAFETY: each raw fd is wrapped exactly once.
    unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
    }
}

/// Read a pipe to EOF with a timeout guard (the daemon closes its write
/// end after delivering, or drops it on failure).
pub fn read_all_with_timeout(mut file: std::fs::File, timeout: Duration) -> Vec<u8> {
    use std::io::Read;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = file.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx.recv_timeout(timeout)
        .expect("the pipe must reach EOF within the timeout")
}

// ---- fake sigil daemon ------------------------------------------------------

/// A Portal-owned stand-in for the sigil daemon's native IPC socket.
///
/// The wire format mirrors sigil's protocol literally — a u32 big-endian
/// length prefix followed by JSON, with externally tagged request and
/// response enums — defined here as local fixtures rather than imported
/// from the sigil workspace. A shared implementation could make a matching
/// protocol bug pass on both sides; these literals cannot.
pub struct FakeSigil {
    runtime_dir: PathBuf,
    listener: Option<std::os::unix::net::UnixListener>,
    observed: std::sync::Arc<std::sync::Mutex<Vec<ObservedSecretRequest>>>,
    /// Set when the listener could not be created (e.g. a leftover socket).
    bind_error: Option<String>,
}

/// The GetApplicationSecret request the portal sent, captured verbatim.
#[derive(Clone, Debug)]
pub struct ObservedSecretRequest {
    pub namespace: String,
    pub subject: String,
    pub purpose: String,
}

/// What the fake sigil daemon answers with.
pub enum FakeSigilResponse {
    Secret(Vec<u8>),
    Locked,
    Cancelled,
    Error(String),
}

impl FakeSigil {
    /// Bind `$runtime_dir/sigil/native.sock` and serve until dropped. One
    /// worker thread per connection; each connection carries exactly one
    /// request, like the real client's connect-per-call pattern.
    pub fn bind(runtime_dir: &Path, response: FakeSigilResponse) -> Self {
        let socket_dir = runtime_dir.join("sigil");
        std::fs::create_dir_all(&socket_dir).expect("create sigil runtime dir");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))
            .expect("make sigil runtime dir private");
        let socket_path = socket_dir.join("native.sock");
        let (listener, bind_error) = match std::os::unix::net::UnixListener::bind(&socket_path) {
            Ok(listener) => (Some(listener), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        if let Some(listener) = &listener {
            let observed = std::sync::Arc::clone(&observed);
            let listener = listener.try_clone().expect("clone listener");
            std::thread::spawn(move || serve(listener, observed, response));
        }
        Self {
            runtime_dir: runtime_dir.to_path_buf(),
            listener,
            observed,
            bind_error,
        }
    }

    pub fn bind_error(&self) -> Option<&str> {
        self.bind_error.as_deref()
    }

    /// Every GetApplicationSecret request seen so far.
    pub fn observed(&self) -> Vec<ObservedSecretRequest> {
        self.observed.lock().expect("observed lock").clone()
    }
}

impl Drop for FakeSigil {
    fn drop(&mut self) {
        drop(self.listener.take());
        let _ = std::fs::remove_dir_all(self.runtime_dir.join("sigil"));
    }
}

fn serve(
    listener: std::os::unix::net::UnixListener,
    observed: std::sync::Arc<std::sync::Mutex<Vec<ObservedSecretRequest>>>,
    response: FakeSigilResponse,
) {
    const WIRE_MAGIC: [u8; 4] = *b"SIGL";
    const WIRE_VERSION: u8 = 2;
    const OP_GET_APPLICATION_SECRET: u8 = 0x01;
    const STATUS_SECRET: u8 = 0x01;
    const STATUS_LOCKED: u8 = 0x03;
    const STATUS_CANCELLED: u8 = 0x05;
    const STATUS_ERROR: u8 = 0x07;

    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            break;
        };
        use std::io::{Read, Write};
        let mut header = [0u8; 8];
        if stream.read_exact(&mut header).is_err() {
            continue;
        }
        if header[0..4] != WIRE_MAGIC
            || header[4] != WIRE_VERSION
            || header[5] != OP_GET_APPLICATION_SECRET
        {
            continue;
        }
        let payload_len = u16::from_be_bytes([header[6], header[7]]) as usize;
        let mut payload = vec![0u8; payload_len];
        if stream.read_exact(&mut payload).is_err() {
            continue;
        }

        let mut offset = 0;
        let Ok((ns, subj, purp)) = (|| -> Result<(String, String, String), String> {
            fn read_str(p: &[u8], o: &mut usize) -> Result<String, String> {
                if *o + 2 > p.len() {
                    return Err("truncated length".into());
                }
                let l = u16::from_be_bytes([p[*o], p[*o + 1]]) as usize;
                *o += 2;
                if *o + l > p.len() {
                    return Err("truncated data".into());
                }
                let s = String::from_utf8(p[*o..*o + l].to_vec()).map_err(|e| e.to_string())?;
                *o += l;
                Ok(s)
            }
            Ok((
                read_str(&payload, &mut offset)?,
                read_str(&payload, &mut offset)?,
                read_str(&payload, &mut offset)?,
            ))
        })() else {
            continue;
        };

        observed
            .lock()
            .expect("observed lock")
            .push(ObservedSecretRequest {
                namespace: ns,
                subject: subj,
                purpose: purp,
            });

        let (status, resp_body) = match &response {
            FakeSigilResponse::Secret(bytes) => (STATUS_SECRET, bytes.clone()),
            FakeSigilResponse::Locked => (STATUS_LOCKED, Vec::new()),
            FakeSigilResponse::Cancelled => (STATUS_CANCELLED, Vec::new()),
            FakeSigilResponse::Error(msg) => {
                let mut b = Vec::new();
                let bytes = msg.as_bytes();
                b.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
                b.extend_from_slice(bytes);
                (STATUS_ERROR, b)
            }
        };

        let mut out = Vec::new();
        out.extend_from_slice(&WIRE_MAGIC);
        out.push(WIRE_VERSION);
        out.push(status);
        out.extend_from_slice(&(resp_body.len() as u16).to_be_bytes());
        out.extend_from_slice(&resp_body);

        let _ = stream.write_all(&out);
        let _ = stream.flush();
    }
}
