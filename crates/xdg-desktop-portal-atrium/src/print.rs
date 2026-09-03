//! `org.freedesktop.impl.portal.Print`: printing through the system `lp`
//! client.
//!
//! The lens stack has no print UI and building a printer dialog is out of
//! proportion, so this backend is deliberately minimal but real (the
//! Email/`xdg-email` hand-off precedent):
//!
//! - `PreparePrint` validates the supplied `settings`/`page_setup` maps
//!   and echoes them back unchanged with a fresh nonzero `token` — the
//!   spec-legal "backend accepts the settings as-is" behavior. No printer
//!   selection or page-setup editing is presented; the frontend passes
//!   through what the application supplied. `modal`/`accept_label` are
//!   accepted and ignored (debug log).
//! - `Print` reads the supplied descriptor into a bounded, 0600 temp file
//!   under `$XDG_RUNTIME_DIR` and submits it to the default printer as
//!   `lp -t <title> <file>` (direct spawn, no shell; `ATRIUM_PORTAL_LP`
//!   overrides the command for tests). `lp`'s exit status decides the
//!   response: 0 on successful queueing, 2 on any failure. A missing `lp`
//!   therefore means `Print` always answers 2 while `PreparePrint` keeps
//!   working — the interface stays served either way.
//!
//! Tokens are opaque correlation handles: because no dialog ever edits
//! settings, a `token` in `Print`'s options is accepted whether or not
//! this backend issued it (the spec leaves token validity to the
//! implementation) and echoed back in the results; a fresh one is
//! returned when absent.
//!
//! Response codes: 0 printed/accepted, 1 cancelled (`Request.Close` raced
//! in), 2 error.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use zbus::zvariant::{ObjectPath, OwnedFd, Value};

use atrium_portal_runtime::{PortalResponse, RequestTracker, ResponseSender, sync};

/// Bounds for the settings/page-setup maps and the print payload.
const MAX_MAP_ENTRIES: usize = 256;
const MAX_KEY_BYTES: usize = 255;
const MAX_STRING_VALUE_BYTES: usize = 4 * 1024;
const MAX_PRINT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TITLE_CHARS: usize = 128;

/// One print request handed from the bus method to the worker.
pub(crate) enum PrintJob {
    Print {
        request_path: String,
        app_id: String,
        title: String,
        fd: OwnedFd,
        token: u32,
        reply: ResponseSender,
    },
}

/// The served print interface.
pub(crate) struct PrintIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<PrintJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Print")]
impl PrintIface {
    #[allow(clippy::too_many_arguments)]
    async fn prepare_print(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        title: &str,
        settings: HashMap<String, Value<'_>>,
        page_setup: HashMap<String, Value<'_>>,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let path = handle.as_str().to_string();
        log::info!("portal: PreparePrint for '{app_id}' at {path}");
        for ignored in ["modal", "accept_label"] {
            if options.contains_key(ignored) {
                log::debug!("portal: ignoring PreparePrint option '{ignored}' (no print dialog)");
            }
        }
        if title.len() > MAX_TITLE_CHARS * 4 {
            log::warn!("portal: refusing PreparePrint: title is oversized");
            return Ok((2, HashMap::new()));
        }

        atrium_portal_runtime::register(&self.conn, &self.tracker, &path).await?;
        let response = prepare(app_id, settings, page_setup);
        atrium_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
        response
    }

    async fn print(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        title: &str,
        fd: OwnedFd,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let path = handle.as_str().to_string();
        log::info!("portal: Print for '{app_id}' at {path}");
        if options.contains_key("modal") {
            log::debug!("portal: ignoring Print option 'modal' (no print dialog)");
        }
        // The token correlates with a PreparePrint call; any or none is
        // accepted (see the module docs).
        let token = options
            .get("token")
            .and_then(|value| u32::try_from(value).ok())
            .filter(|token| *token != 0)
            .unwrap_or_else(fresh_token);

        atrium_portal_runtime::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "print",
            &self.jobs,
            |reply| PrintJob::Print {
                request_path: path.clone(),
                app_id: app_id.to_string(),
                title: title.to_string(),
                fd,
                token,
                reply,
            },
        )
        .await
    }

    /// The local frontend contract level this backend implements. The impl
    /// interface XML defines no version property; 3 matches the documented
    /// frontend contract (upstream's v4 added frontend-only options).
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        3
    }
}

/// A fresh nonzero token.
fn fresh_token() -> u32 {
    rand::random::<u32>().max(1)
}

/// Validate and echo the settings, attaching a fresh token. Instant, so it
/// runs inside the served method (the email precedent).
fn prepare(
    app_id: &str,
    settings: HashMap<String, Value<'_>>,
    page_setup: HashMap<String, Value<'_>>,
) -> zbus::fdo::Result<PortalResponse> {
    for (name, map) in [("settings", &settings), ("page_setup", &page_setup)] {
        if let Err(error) = validate_map(name, map) {
            log::warn!("portal: refusing PreparePrint for '{app_id}': {error}");
            return Ok((2, HashMap::new()));
        }
    }
    let to_static = |map: HashMap<String, Value<'_>>| -> HashMap<String, Value<'static>> {
        map.into_iter()
            .filter_map(|(key, value)| value.try_to_owned().ok().map(|v| (key, v)))
            .map(|(key, value)| (key, Value::from(value)))
            .collect()
    };
    let token = fresh_token();
    let results: HashMap<String, Value<'static>> = HashMap::from([
        ("settings".to_owned(), Value::from(to_static(settings))),
        ("page-setup".to_owned(), Value::from(to_static(page_setup))),
        ("token".to_owned(), Value::from(token)),
    ]);
    log::info!("portal: PreparePrint for '{app_id}' accepted as-is (token {token})");
    Ok((0, results))
}

/// Bound a settings/page-setup map: entry count, key shape, and string
/// value sizes. Non-string values ride the bus already allocated and are
/// echoed verbatim.
fn validate_map(name: &str, map: &HashMap<String, Value<'_>>) -> Result<(), String> {
    if map.len() > MAX_MAP_ENTRIES {
        return Err(format!("{name} exceeds the {MAX_MAP_ENTRIES}-entry limit"));
    }
    for (key, value) in map {
        if key.is_empty() || key.len() > MAX_KEY_BYTES || key.contains('\0') {
            return Err(format!("{name} key is empty, oversized, or contains NUL"));
        }
        if let Ok(text) = String::try_from(value)
            && (text.len() > MAX_STRING_VALUE_BYTES || text.contains('\0'))
        {
            return Err(format!(
                "{name} value for {key:?} is oversized or contains NUL"
            ));
        }
    }
    Ok(())
}

/// The job title argv-safe: control characters dropped, length capped.
fn sanitize_title(title: &str) -> String {
    let clean: String = title
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TITLE_CHARS)
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        "Portal print job".to_owned()
    } else {
        clean.to_owned()
    }
}

/// The `lp` argument vector (no shell anywhere in the path).
fn lp_argv(title: &str, file: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "-t".into(),
        sanitize_title(title).into(),
        file.as_os_str().to_owned(),
    ]
}

/// The print command: `lp` unless overridden (tests point this at
/// `/bin/true`/`/bin/false`).
fn lp_command() -> String {
    std::env::var("ATRIUM_PORTAL_LP").unwrap_or_else(|_| "lp".to_string())
}

/// The directory print spools land in: `$XDG_RUNTIME_DIR` (private, tmpfs)
/// with a temp-dir fallback.
fn spool_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Read the print descriptor into a bounded 0600 temp file. The caller
/// deletes the file after submission.
fn spool_fd(fd: OwnedFd, dir: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let source = std::fs::File::from(std::os::fd::OwnedFd::from(fd));
    let path = dir.join(format!(
        "tessera-print-{}-{}.ps",
        std::process::id(),
        fresh_token()
    ));
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("could not create the spool file: {error}"))?;
    let mut limited = source.take(MAX_PRINT_BYTES + 1);
    let result = std::io::copy(&mut limited, &mut target);
    target
        .flush()
        .and_then(|()| result.map(|_| ()))
        .map_err(|error| {
            let _ = std::fs::remove_file(&path);
            format!("could not spool the print data: {error}")
        })?;
    let length = target.metadata().map_err(|error| error.to_string())?.len();
    if length == 0 || length > MAX_PRINT_BYTES {
        let _ = std::fs::remove_file(&path);
        return Err(format!(
            "print data length {length} is outside 1..={MAX_PRINT_BYTES}"
        ));
    }
    Ok(path)
}

/// Submit the spool file to the default printer and wait for the queue
/// submission. `lp` exits once the job is queued, so the wait is short.
fn submit(lp: &str, title: &str, file: &Path) -> Result<(), String> {
    let status = std::process::Command::new(lp)
        .args(lp_argv(title, file))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|error| format!("could not run {lp}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{lp} exited with {status}"))
    }
}

/// Dispatch print jobs independently so one slow spool cannot
/// head-of-line block another application's request.
pub(crate) fn print_worker(rx: mpsc::Receiver<PrintJob>, tracker: Arc<Mutex<RequestTracker>>) {
    const MAX_ACTIVE_PRINT_REQUESTS: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(PrintJob::Print {
        request_path,
        app_id,
        title,
        fd,
        token,
        reply,
    }) = rx.recv()
    {
        if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_ACTIVE_PRINT_REQUESTS {
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            log::warn!("portal: refusing Print request: concurrency limit reached");
            let _ = reply.send_blocking((2, HashMap::new()));
            continue;
        }
        let task_tracker = Arc::clone(&tracker);
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("atrium-portal-print-task".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let result = run_print(&task_tracker, &request_path, &app_id, &title, fd, token);
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn Print task: {error}");
            let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
        }
    }
}

/// Execute one request: spool the descriptor, submit to `lp`, clean up.
fn run_print(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    app_id: &str,
    title: &str,
    fd: OwnedFd,
    token: u32,
) -> (u32, HashMap<String, Value<'static>>) {
    if sync::lock(tracker, "print tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }
    let spool = match spool_fd(fd, &spool_dir()) {
        Ok(path) => path,
        Err(error) => {
            log::warn!("portal: Print for '{app_id}' failed: {error}");
            return (2, HashMap::new());
        }
    };
    let submission = submit(&lp_command(), title, &spool);
    if let Err(error) = std::fs::remove_file(&spool) {
        log::warn!("portal: could not remove the print spool file: {error}");
    }
    match submission {
        Ok(()) => {
            log::info!("portal: Print for '{app_id}' queued via lp (token {token})");
            (0, HashMap::from([("token".to_owned(), Value::from(token))]))
        }
        Err(error) => {
            log::warn!("portal: Print for '{app_id}' failed: {error}");
            (2, HashMap::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_are_bounded() {
        let mut settings = HashMap::new();
        settings.insert("copies".to_owned(), Value::from("2"));
        assert!(validate_map("settings", &settings).is_ok());

        let flooded: HashMap<String, Value<'static>> = (0..=MAX_MAP_ENTRIES)
            .map(|index| (format!("key-{index}"), Value::from("x")))
            .collect();
        assert!(validate_map("settings", &flooded).is_err());

        let oversized: HashMap<String, Value<'static>> = HashMap::from([(
            "job-name".to_owned(),
            Value::from("x".repeat(MAX_STRING_VALUE_BYTES + 1)),
        )]);
        assert!(validate_map("settings", &oversized).is_err());

        let bad_key: HashMap<String, Value<'static>> =
            HashMap::from([("".to_owned(), Value::from("x"))]);
        assert!(validate_map("settings", &bad_key).is_err());
    }

    #[test]
    fn prepare_echoes_maps_with_a_fresh_token() {
        let settings = HashMap::from([("copies".to_owned(), Value::from("2"))]);
        let page_setup = HashMap::from([("media".to_owned(), Value::from("A4"))]);
        let (response, results) = prepare("org.example.App", settings, page_setup).unwrap();
        assert_eq!(response, 0);
        let token = u32::try_from(&results["token"]).unwrap();
        assert_ne!(token, 0);
        // The maps come back unchanged (settings key, page-setup key).
        assert!(results.contains_key("settings"));
        assert!(results.contains_key("page-setup"));

        let oversized: HashMap<String, Value<'static>> = (0..=MAX_MAP_ENTRIES)
            .map(|index| (format!("key-{index}"), Value::from("x")))
            .collect();
        let (response, _) = prepare("org.example.App", oversized, HashMap::new()).unwrap();
        assert_eq!(response, 2);
    }

    #[test]
    fn titles_are_sanitized_for_argv() {
        assert_eq!(sanitize_title("Quarterly report"), "Quarterly report");
        assert_eq!(sanitize_title(""), "Portal print job");
        assert_eq!(sanitize_title("  "), "Portal print job");
        assert_eq!(sanitize_title("a\u{7}b\nc"), "abc");
        assert_eq!(
            sanitize_title(&"x".repeat(MAX_TITLE_CHARS + 50))
                .chars()
                .count(),
            MAX_TITLE_CHARS
        );
    }

    #[test]
    fn lp_argv_is_direct_and_shell_free() {
        let argv = lp_argv("My Document", Path::new("/tmp/spool.ps"));
        assert_eq!(
            argv,
            [
                std::ffi::OsString::from("-t"),
                std::ffi::OsString::from("My Document"),
                std::ffi::OsString::from("/tmp/spool.ps"),
            ]
        );
    }

    #[test]
    fn spooling_caps_and_secures_the_temp_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!(
            "tessera-print-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let source = dir.join("source.ps");
        std::fs::write(&source, b"%PDF fake print data").unwrap();
        let fd: OwnedFd = std::os::fd::OwnedFd::from(std::fs::File::open(&source).unwrap()).into();
        let spool = spool_fd(fd, &dir).unwrap();
        let metadata = spool.metadata().unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&spool).unwrap(), b"%PDF fake print data");
        std::fs::remove_file(&spool).unwrap();

        // An empty descriptor is rejected.
        let empty = dir.join("empty.ps");
        std::fs::write(&empty, b"").unwrap();
        let fd: OwnedFd = std::os::fd::OwnedFd::from(std::fs::File::open(&empty).unwrap()).into();
        assert!(spool_fd(fd, &dir).is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn submission_reports_the_exit_status() {
        let dir = std::env::temp_dir();
        let spool = dir.join(format!("tessera-print-submit-{}.ps", std::process::id()));
        std::fs::write(&spool, b"data").unwrap();
        assert!(submit("/bin/true", "title", &spool).is_ok());
        assert!(submit("/bin/false", "title", &spool).is_err());
        assert!(submit("/definitely/missing/lp", "title", &spool).is_err());
        std::fs::remove_file(&spool).unwrap();
    }
}
