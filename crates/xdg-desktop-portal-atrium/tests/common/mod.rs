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
//! free to use only what it needs.
#![allow(dead_code)]

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
    pub fn connect(&self) -> zbus::blocking::Connection {
        zbus::blocking::connection::Builder::address(self.address.as_str())
            .expect("valid private bus address")
            .build()
            .expect("connect to the private bus")
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
    let mut child = match Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
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
    command
        .env("DBUS_SESSION_BUS_ADDRESS", bus.address())
        .env("XDG_DATA_HOME", data)
        .env("XDG_CACHE_HOME", data.join("cache"))
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

/// Write one scripted response consumed by [`fake_prompter`].
pub fn write_prompter_response(
    directory: &std::path::Path,
    index: u32,
    response: &atrium_portal_prompter::PrompterResponse,
) {
    std::fs::write(
        directory.join(format!("response-{index}.json")),
        serde_json::to_vec(response).expect("serialize prompter response"),
    )
    .expect("write prompter response");
}

/// Read and validate one request recorded by [`fake_prompter`].
pub fn read_prompter_request(
    directory: &std::path::Path,
    index: u32,
) -> atrium_portal_prompter::PromptRequest {
    let request: atrium_portal_prompter::PrompterRequest = serde_json::from_slice(
        &std::fs::read(directory.join(format!("request-{index}.json")))
            .expect("read recorded prompter request"),
    )
    .expect("decode recorded prompter request");
    request.into_prompt().expect("validate prompter request")
}

/// Read one recorded request's appearance snapshot (contract v6), decoded
/// from the raw JSON so the wire shape — not just the typed projection —
/// is what the assertion sees. `None` when the request omitted it.
#[must_use]
pub fn read_prompter_appearance(
    directory: &std::path::Path,
    index: u32,
) -> Option<atrium_portal_ipc::DesktopPreferences> {
    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.join(format!("request-{index}.json")))
            .expect("read recorded prompter request"),
    )
    .expect("decode recorded prompter request JSON");
    value.get("appearance").map(|appearance| {
        let color_scheme = match appearance["color_scheme"].as_str() {
            Some("dark") => atrium_portal_ipc::ColorScheme::Dark,
            Some("light") => atrium_portal_ipc::ColorScheme::Light,
            _ => atrium_portal_ipc::ColorScheme::System,
        };
        let accent_color =
            appearance["accent_color"]
                .as_object()
                .map(|accent| atrium_portal_ipc::AccentColor {
                    red: accent["red"].as_u64().unwrap_or_default() as u8,
                    green: accent["green"].as_u64().unwrap_or_default() as u8,
                    blue: accent["blue"].as_u64().unwrap_or_default() as u8,
                });
        atrium_portal_ipc::DesktopPreferences {
            color_scheme,
            accent_color,
            contrast: if appearance["high_contrast"].as_bool().unwrap_or(false) {
                atrium_portal_ipc::Contrast::High
            } else {
                atrium_portal_ipc::Contrast::Normal
            },
            reduced_motion: appearance["reduced_motion"].as_bool().unwrap_or(false),
            ..atrium_portal_ipc::DesktopPreferences::default()
        }
    })
}

/// Create a pipe-compatible, one-shot prompter fixture. Each invocation
/// records its request before returning the correspondingly numbered reply.
pub fn fake_prompter(directory: &std::path::Path) -> PathBuf {
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

/// Wait until the daemon owns `name` (10 s bound).
pub fn wait_for_name(conn: &zbus::blocking::Connection, name: &str) {
    let fdo = zbus::blocking::Proxy::new(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .expect("fdo proxy");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let owned: bool = fdo
            .call("NameHasOwner", &(name,))
            .expect("NameHasOwner call");
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
