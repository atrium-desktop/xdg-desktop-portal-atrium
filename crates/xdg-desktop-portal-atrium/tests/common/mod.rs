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
    /// The sandbox home and runtime directory activated services inherit.
    sandbox: PathBuf,
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
        // An activated `xdg-document-portal` mounts its FUSE tree at
        // `$XDG_RUNTIME_DIR/doc`. It normally unmounts when the bus goes
        // away; if it died first, the stale mount would make the sandbox
        // undeletable, so detach it lazily.
        let doc = self.sandbox.join("runtime/doc");
        if is_mount_point(&doc) {
            for tool in ["fusermount3", "fusermount"] {
                let unmounted = Command::new(tool)
                    .arg("-u")
                    .arg("-z")
                    .arg(&doc)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success());
                if unmounted {
                    break;
                }
            }
        }
    }
}

/// Whether `path` is a mount point of this process's mount namespace.
fn is_mount_point(path: &Path) -> bool {
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let target = path.to_string_lossy();
    // Field 5 is the mount point; spaces in it are octal-escaped.
    mountinfo.lines().any(|line| {
        line.split(' ')
            .nth(4)
            .is_some_and(|mount| mount.replace("\\040", " ") == target)
    })
}

/// Spawn a private `dbus-daemon --session`; `None` when dbus-daemon is not
/// installed (tests skip).
///
/// The bus passes its own environment to every service it activates, so it
/// runs with a private home and runtime directory: an activated
/// `xdg-permission-store` or `xdg-document-portal` must write its database and
/// mount its FUSE tree inside the test's sandbox, never into the user's real
/// `~/.local/share` or `$XDG_RUNTIME_DIR/doc`. `XDG_DATA_DIRS` and `PATH` are
/// inherited so the system's `.service` files and binaries stay reachable.
pub fn private_bus() -> Option<PrivateBus> {
    let root = temp_dir("bus");
    let mut command = Command::new("dbus-daemon");
    for (key, sub) in [
        ("HOME", "home"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("XDG_DATA_HOME", "home/.local/share"),
        ("XDG_CONFIG_HOME", "home/.config"),
        ("XDG_CACHE_HOME", "home/.cache"),
        ("XDG_STATE_HOME", "home/.local/state"),
    ] {
        let dir = root.join(sub);
        std::fs::create_dir_all(&dir).expect("create private bus sandbox dir");
        command.env(key, dir);
    }
    std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
        .expect("make private bus runtime dir private");
    command
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
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
    Some(PrivateBus {
        address,
        child,
        sandbox: root,
    })
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

/// Impersonate the portal frontend on a private bus.
///
/// The backend admits only the peer that owns `org.freedesktop.portal.Desktop`
/// (see `atrium_portal_request::caller`), so a test client that wants to call
/// the private interfaces must hold that name first. A real frontend resolves
/// each sandboxed application's identity itself and forwards the resulting
/// `app_id`; the fixtures here stand in for it, which is exactly the trust
/// delegation the daemon relies on.
///
/// Requesting the name also makes the backend's `NameOwnerChanged` watcher
/// learn this connection as the admitted peer. The call is synchronous, so
/// once it returns the boundary is in place.
pub fn claim_frontend(conn: &zbus::Connection) {
    let reply: Result<u32, zbus::Error> = bus::try_call::<_, u32>(
        &bus::proxy(
            conn,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        ),
        "RequestName",
        &("org.freedesktop.portal.Desktop", 0u32),
    );
    // 1 = primary owner; 4 = already owned by this connection.
    let code = reply.expect("the fixture must own the portal frontend name");
    assert!(
        code == 1 || code == 4,
        "the fixture could not become the frontend (RequestName returned {code})"
    );
    await_admission(conn);
}

/// Stop impersonating the portal frontend while keeping `conn` on the bus.
///
/// PipeWire's `module-portal` resolves the owner of
/// `org.freedesktop.portal.Desktop` to a PID and applies portal access rules
/// to every PipeWire client from that process. In production the frontend
/// and the application consuming a ScreenCast stream are different
/// processes; a test that also runs an in-process PipeWire consumer must
/// therefore drop the name before the consumer connects, or the session
/// manager never links it. Sessions survive: the backend reclaims them only
/// when their owner's unique name leaves the bus, and `conn` stays.
pub fn release_frontend(conn: &zbus::Connection) {
    let reply: Result<u32, zbus::Error> = bus::try_call::<_, u32>(
        &bus::proxy(
            conn,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        ),
        "ReleaseName",
        &("org.freedesktop.portal.Desktop",),
    );
    // 1 = released.
    let code = reply.expect("release the portal frontend name");
    assert_eq!(
        code, 1,
        "the fixture did not own the frontend name (ReleaseName returned {code})"
    );
}

/// Wait until the backend actually admits this connection.
///
/// The daemon learns the new owner from `NameOwnerChanged`, delivered
/// asynchronously, so claiming the name does not mean the boundary has moved
/// yet.
///
/// The probe must be an admission-checked **method**. A property will not do:
/// zbus answers `Properties.Get` without entering a method, so a property read
/// succeeds before admission is in effect and the fixture would race every
/// subsequent call. `RemoveNotification` is the right probe — it is
/// admission-checked first, and for an unknown id it is a side-effect-free
/// success.
fn await_admission(conn: &zbus::Connection) {
    let notifications = bus::proxy(
        conn,
        "org.freedesktop.impl.portal.desktop.atrium",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.impl.portal.Notification",
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let probe: Result<(), zbus::Error> =
            bus::try_call(&notifications, "RemoveNotification", &("", ""));
        match probe {
            Ok(()) => return,
            Err(error) => {
                let text = error.to_string();
                assert!(
                    text.contains("admits no caller") || text.contains("private backend"),
                    "the backend rejected the fixture for a reason unrelated to admission: {text}"
                );
                assert!(
                    Instant::now() < deadline,
                    "the backend never admitted the fixture after it claimed the frontend name"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Kill a daemon child on drop.
pub struct KillOnDrop(pub Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Directories created by [`temp_dir`] in this test process, removed by
/// [`remove_temp_dirs`] when the process exits.
static TEMP_DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// A unique, private temp directory tagged by test name.
///
/// `tempfile` creates the directory atomically with an unpredictable name and
/// mode 0700. It is not tied to a guard's scope: the daemon under test may
/// still hold files in it while the test's locals drop. Instead the directory
/// is removed when the test process exits — normally or after a failed
/// assertion, since nextest runs each test in its own process — unless
/// `ATRIUM_PORTAL_E2E_KEEP` is set to leave the logs of a failed run
/// inspectable.
pub fn temp_dir(tag: &str) -> PathBuf {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| {
        // SAFETY: `atexit` only requires a valid function pointer with static
        // lifetime. `remove_temp_dirs` is a plain `extern "C" fn()`; a panic
        // inside it aborts instead of unwinding across the C boundary.
        let registered = unsafe { libc::atexit(remove_temp_dirs) };
        assert_eq!(registered, 0, "register temp-dir cleanup");
    });
    let dir = tempfile::Builder::new()
        .prefix(&format!("xdg-desktop-portal-atrium-e2e-{tag}-"))
        .tempdir()
        .expect("create temp dir")
        .keep();
    TEMP_DIRS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(dir.clone());
    dir
}

extern "C" fn remove_temp_dirs() {
    if std::env::var_os("ATRIUM_PORTAL_E2E_KEEP").is_some() {
        return;
    }
    // `try_lock`: a thread still holding the lock at exit must not deadlock
    // the process on its way out; leaking one run's directories is the lesser
    // failure.
    let dirs = match TEMP_DIRS.try_lock() {
        Ok(dirs) => dirs,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    for dir in dirs.iter() {
        let _ = std::fs::remove_dir_all(dir);
    }
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
/// and `org.freedesktop.portal.Documents`. On a bus that cannot activate them
/// — no `.service` file, or an activation request that is accepted and never
/// answered — the frontend sits for its 25 s timeout and then aborts with
/// "No document portal".
///
/// This asks the bus to activate exactly those two services, bounded, so
/// callers can skip with a truthful reason in seconds instead of timing out.
/// Any error *reply* is a definite answer (the service is not installed);
/// only the absence of a reply means the bus cannot activate at all.
pub fn frontend_prerequisites(bus: &PrivateBus) -> Result<(), String> {
    const ACTIVATION_BOUND: Duration = Duration::from_secs(5);

    if frontend_binary().is_none() {
        return Err("xdg-desktop-portal frontend is not installed".into());
    }
    if document_portal_binary().is_none() {
        return Err("xdg-document-portal is not installed".into());
    }
    let conn = bus.connect();
    for service in [
        "org.freedesktop.impl.portal.PermissionStore",
        "org.freedesktop.portal.Documents",
    ] {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let probe_conn = conn.clone();
        std::thread::spawn(move || {
            let fdo = bus::proxy(
                &probe_conn,
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
            );
            let reply: zbus::Result<(u32,)> =
                bus::try_call(&fdo, "StartServiceByName", &(service, 0u32));
            let _ = reply_tx.send(reply.map(|_| ()).map_err(|error| error.to_string()));
        });
        match reply_rx.recv_timeout(ACTIVATION_BOUND) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(format!("the bus cannot activate {service}: {error}")),
            Err(_) => {
                return Err(format!(
                    "the session bus never answered activation of {service}"
                ));
            }
        }
    }
    Ok(())
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
/// The wire format mirrors `sigil-wire-v2` literally — an 8-byte binary header
/// (`b"SIGL"`, version `2`, opcode/status byte, `u16_be` payload length) with
/// length-prefixed UTF-8 TLV fields — defined here as local fixtures rather
/// than imported from the sigil workspace. A shared implementation could make a
/// matching protocol bug pass on both sides; these literals cannot.
pub struct FakeSigil {
    socket_path: PathBuf,
    cleanup_dir: Option<PathBuf>,
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
    Desynced,
    Cancelled,
    Error(String),
    /// Read the request, then hold the connection open without ever
    /// replying — a wedged sigil whose unlock prompt is never answered.
    /// The portal must bound this rather than wait forever.
    Stall,
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
        Self::bind_internal(socket_path, Some(socket_dir), response)
    }

    /// Bind at an explicit custom socket path (for testing `SIGIL_SOCKET_PATH`).
    pub fn bind_at(socket_path: &Path, response: FakeSigilResponse) -> Self {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).expect("create custom sigil socket dir");
        }
        Self::bind_internal(socket_path.to_path_buf(), None, response)
    }

    fn bind_internal(
        socket_path: PathBuf,
        cleanup_dir: Option<PathBuf>,
        response: FakeSigilResponse,
    ) -> Self {
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
            socket_path,
            cleanup_dir,
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
        let _ = std::fs::remove_file(&self.socket_path);
        if let Some(dir) = &self.cleanup_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
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
    const STATUS_DESYNCED: u8 = 0x04;
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
            // Hold the socket without answering. The thread parks here for
            // the life of the test, which is exactly the state the portal's
            // socket timeout must survive.
            FakeSigilResponse::Stall => {
                std::thread::sleep(std::time::Duration::from_secs(3600));
                continue;
            }
            FakeSigilResponse::Secret(bytes) => (STATUS_SECRET, bytes.clone()),
            FakeSigilResponse::Locked => (STATUS_LOCKED, Vec::new()),
            FakeSigilResponse::Desynced => (STATUS_DESYNCED, Vec::new()),
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
