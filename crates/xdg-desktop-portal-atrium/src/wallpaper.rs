//! `org.freedesktop.impl.portal.Wallpaper` v1: set the desktop wallpaper.
//!
//! Wallpaper application is compositor-owned (the compositor draws the
//! outputs), so this is the one Portal interface that crosses the scoped
//! IPC boundary: the compositor's `SetWallpaper` op names an image PATH the
//! compositor decodes itself ([`atrium_portal_ipc`]). The portal therefore
//! stages the image at a private, session-stable location —
//! `$XDG_RUNTIME_DIR/atrium-portal/wallpaper/current.<ext>` — and keeps it
//! after a successful reply: the receipt means the decode-and-swap is done,
//! but the compositor may keep streaming a video wallpaper from the path.
//! The staging directory is wiped at daemon startup.
//!
//! Flow: the URI must be `file://` (a portal backend does not fetch from
//! the network — remote URIs answer 2); the image is read with a 64 MiB
//! staging sanity cap on the worker. With `show-preview=true` the existing
//! `Confirm` prompt asks to set the named file as the wallpaper (accept
//! "_Set Wallpaper"; cancellation answers 1) — a true visual preview awaits
//! image decoding in the lens stack, documented limitation. With
//! `show-preview=false`
//! the spec allows direct application, so no prompt is shown. `set-on`
//! (`background`/`lockscreen`/`both`) is accepted and validated — missing or
//! empty means `background`, unknown values answer 2 — but it is not
//! forwarded: the compositor has a single wallpaper concept and the wire op
//! carries no placement.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use atrium_portal_prompter::{ConfirmRequest, ConfirmResponse, PromptResult, PrompterRequest};
use zbus::zvariant::{ObjectPath, Value};

use crate::prompter::{self, InvokeError};
use crate::{files, ipc};
use atrium_portal_runtime::{RequestTracker, ResponseSender, sync};

/// Portal-side staging sanity cap on the image read: a wallpaper is a still
/// image, and 64 MiB covers 8K PNGs with headroom.
const MAX_WALLPAPER_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

/// One wallpaper request handed from the bus method to the worker.
pub(crate) enum WallpaperJob {
    Set {
        request_path: String,
        app_id: String,
        path: PathBuf,
        show_preview: bool,
        parent_window: Option<String>,
        reply: ResponseSender,
    },
}

/// The served wallpaper interface. The method only validates and enqueues;
/// the slow work (image read, consent prompt, IPC) runs on the worker.
pub(crate) struct WallpaperIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<WallpaperJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Wallpaper")]
impl WallpaperIface {
    async fn set_wallpaper_uri(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        uri: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<u32> {
        let path = handle.as_str().to_string();
        log::info!("portal: SetWallpaperURI for '{app_id}' at {path}: {uri}");

        let (image_path, show_preview) = match parse_request(uri, &options) {
            Ok(parsed) => parsed,
            Err(error) => {
                log::warn!("portal: refusing SetWallpaperURI: {error}");
                return Ok(2);
            }
        };

        atrium_portal_runtime::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "set_wallpaper_uri",
            &self.jobs,
            |reply| WallpaperJob::Set {
                request_path: path.clone(),
                app_id: app_id.to_string(),
                path: image_path,
                show_preview,
                parent_window: (!parent_window.is_empty()).then(|| parent_window.to_owned()),
                reply,
            },
        )
        .await
        .map(|(response, _)| response)
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

/// Validate the URI and options: a local file, the `set-on` value, the
/// preview flag.
fn parse_request(
    uri: &str,
    options: &HashMap<String, Value<'_>>,
) -> Result<(PathBuf, bool), String> {
    if uri.len() > 8 * 1024 {
        return Err("URI is oversized".to_string());
    }
    if !uri.starts_with("file://") {
        return Err(format!(
            "only file:// URIs can be wallpapers; a portal backend does not fetch {uri:?} over the network"
        ));
    }
    let path = files::path_from_file_uri(uri)
        .ok_or_else(|| "file URI does not name an absolute local path".to_string())?;

    parse_set_on(options)?;
    let show_preview = options
        .get("show-preview")
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false);
    Ok((path, show_preview))
}

/// Validate the `set-on` option. The compositor has a single wallpaper
/// concept, so the value is not forwarded — but an unknown value is still
/// refused.
fn parse_set_on(options: &HashMap<String, Value<'_>>) -> Result<(), String> {
    match options
        .get("set-on")
        .and_then(|value| String::try_from(value).ok())
        .as_deref()
    {
        None | Some("") | Some("background") | Some("lockscreen") | Some("both") => Ok(()),
        Some(other) => Err(format!("unknown set-on value {other:?}")),
    }
}

/// Read the image, bounded and non-empty.
fn read_image(path: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    std::io::Read::take(file, MAX_WALLPAPER_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if bytes.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    if bytes.len() as u64 > MAX_WALLPAPER_IMAGE_BYTES {
        return Err(format!(
            "{} exceeds the {MAX_WALLPAPER_IMAGE_BYTES}-byte wallpaper limit",
            path.display()
        ));
    }
    Ok(bytes)
}

/// The wallpaper staging directory: `$XDG_RUNTIME_DIR/atrium-portal/wallpaper`.
fn staging_dir() -> Option<PathBuf> {
    staging_dir_from(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// Split out for tests: environment variables are process-global.
fn staging_dir_from(runtime: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let base = runtime.filter(|dir| !dir.is_empty())?;
    Some(PathBuf::from(base).join("atrium-portal").join("wallpaper"))
}

/// The staged file's stable name: `current.<orig-ext>` with the extension
/// reduced to lowercase ASCII alphanumerics (anything else falls back to
/// `img`), so the name can never escape the staging directory.
fn staged_name(image_path: &Path) -> String {
    let extension = image_path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_lowercase)
        .filter(|extension| {
            !extension.is_empty() && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .unwrap_or_else(|| "img".to_owned());
    format!("current.{extension}")
}

/// Stage `image` as `dir/current.<ext>`, atomically replacing a previous
/// wallpaper staged under the same name (never truncated in place).
fn stage_image(dir: &Path, image_path: &Path, image: &[u8]) -> std::io::Result<PathBuf> {
    files::write_atomic(dir, &staged_name(image_path), image)
}

/// Drop staged files other than `keep`: after a successful swap the
/// compositor no longer references them. Best-effort — any leftover is
/// wiped at the next daemon startup.
fn prune_staging(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path != keep
            && path.is_file()
            && let Err(error) = std::fs::remove_file(&path)
        {
            log::warn!(
                "portal: could not prune staged wallpaper {}: {error}",
                path.display()
            );
        }
    }
}

/// Wipe the staging directory at daemon startup: a staged wallpaper is a
/// session-lifetime artifact, so a previous boot's files are stale by
/// definition. Best-effort — the next stage re-creates the directory.
pub(crate) fn clean_staging() {
    let Some(dir) = staging_dir() else {
        return;
    };
    if let Err(error) = files::remove_owned_dir(&dir) {
        log::warn!(
            "portal: could not clean the wallpaper staging directory {}: {error}",
            dir.display()
        );
    }
}

/// Dispatch wallpaper requests independently so one open preview cannot
/// head-of-line block another application's request.
pub(crate) fn wallpaper_worker(
    rx: mpsc::Receiver<WallpaperJob>,
    tracker: Arc<Mutex<RequestTracker>>,
    socket: PathBuf,
    settings: crate::settings::SettingsStore,
) {
    const MAX_ACTIVE_WALLPAPER_REQUESTS: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(WallpaperJob::Set {
        request_path,
        app_id,
        path,
        show_preview,
        parent_window,
        reply,
    }) = rx.recv()
    {
        if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_ACTIVE_WALLPAPER_REQUESTS
        {
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            log::warn!("portal: refusing SetWallpaperURI request: concurrency limit reached");
            let _ = reply.send_blocking((2, HashMap::new()));
            continue;
        }
        let task_tracker = Arc::clone(&tracker);
        let task_socket = socket.clone();
        let task_settings = settings.clone();
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("atrium-portal-wallpaper-task".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let result = run_set(
                    &task_tracker,
                    Some(&task_settings),
                    &request_path,
                    &app_id,
                    &task_socket,
                    &path,
                    show_preview,
                    parent_window,
                );
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn wallpaper task: {error}");
            let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
        }
    }
}

/// Execute one request: resolve the staging directory, then read the image,
/// maybe ask for consent, stage, and apply.
#[allow(clippy::too_many_arguments)]
fn run_set(
    tracker: &Arc<Mutex<RequestTracker>>,
    settings: Option<&crate::settings::SettingsStore>,
    request_path: &str,
    app_id: &str,
    socket: &Path,
    image_path: &Path,
    show_preview: bool,
    parent_window: Option<String>,
) -> (u32, HashMap<String, Value<'static>>) {
    let Some(staging) = staging_dir() else {
        log::warn!("portal: SetWallpaperURI for '{app_id}' failed: $XDG_RUNTIME_DIR is unset");
        return (2, HashMap::new());
    };
    run_set_staged(
        tracker,
        settings,
        request_path,
        app_id,
        socket,
        &staging,
        image_path,
        show_preview,
        parent_window,
    )
}

/// Execute one request against a resolved staging directory (split out so
/// tests can stage in a scratch directory).
#[allow(clippy::too_many_arguments)]
fn run_set_staged(
    tracker: &Arc<Mutex<RequestTracker>>,
    settings: Option<&crate::settings::SettingsStore>,
    request_path: &str,
    app_id: &str,
    socket: &Path,
    staging: &Path,
    image_path: &Path,
    show_preview: bool,
    parent_window: Option<String>,
) -> (u32, HashMap<String, Value<'static>>) {
    if sync::lock(tracker, "wallpaper tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }
    let image = match read_image(image_path) {
        Ok(image) => image,
        Err(error) => {
            log::warn!("portal: SetWallpaperURI for '{app_id}' failed: {error}");
            return (2, HashMap::new());
        }
    };

    if show_preview {
        match preview_consent(
            tracker,
            settings,
            request_path,
            app_id,
            image_path,
            parent_window,
        ) {
            Ok(true) => {}
            Ok(false) => return (1, HashMap::new()),
            Err(error) => {
                log::warn!("portal: wallpaper preview for '{app_id}' failed: {error}");
                return (2, HashMap::new());
            }
        }
    }
    // Request.Close wins a race with a completed prompt.
    if sync::lock(tracker, "wallpaper tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }

    // Stage before the IPC: the compositor decodes the path itself, and the
    // file must outlive a successful reply (a video wallpaper streams from
    // it for the rest of the session).
    let staged = match stage_image(staging, image_path, &image) {
        Ok(staged) => staged,
        Err(error) => {
            log::warn!("portal: SetWallpaperURI for '{app_id}' failed to stage: {error}");
            return (2, HashMap::new());
        }
    };
    match ipc::set_wallpaper(socket, &staged) {
        Ok(()) => {
            prune_staging(staging, &staged);
            log::info!(
                "portal: SetWallpaperURI for '{app_id}' applied {}",
                staged.display()
            );
            (0, HashMap::new())
        }
        Err(error) => {
            // The swap never happened: drop the just-staged file, but never
            // a previous wallpaper staged under a different name.
            let _ = std::fs::remove_file(&staged);
            log::warn!("portal: SetWallpaperURI for '{app_id}' failed: {error}");
            (2, HashMap::new())
        }
    }
}

/// The preview consent prompt: a textual stand-in until the lens stack
/// decodes images (see the module docs). `Ok(true)` confirms, `Ok(false)`
/// cancels.
fn preview_consent(
    tracker: &Arc<Mutex<RequestTracker>>,
    settings: Option<&crate::settings::SettingsStore>,
    request_path: &str,
    app_id: &str,
    image_path: &Path,
    parent_window: Option<String>,
) -> Result<bool, String> {
    let name = image_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| image_path.display().to_string());
    let prompt = ConfirmRequest {
        title: "Set Wallpaper".to_owned(),
        body: format!("The application '{app_id}' wants to set '{name}' as the wallpaper."),
        accept_label: Some("_Set Wallpaper".to_owned()),
        deny_label: None,
        modal: true,
        parent_window,
    };
    let cancelled = || sync::lock(tracker, "wallpaper tracker").was_closed(request_path);
    match prompter::invoke(PrompterRequest::confirm(prompt), settings, Some(&cancelled)) {
        Ok(PromptResult::Confirm(ConfirmResponse::Confirmed)) => Ok(true),
        Ok(PromptResult::Confirm(ConfirmResponse::Cancelled)) | Err(InvokeError::Cancelled) => {
            Ok(false)
        }
        Ok(_) => Err("wallpaper prompter returned the wrong response kind".to_owned()),
        Err(InvokeError::Failed(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, Value<'static>)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tessera-wallpaper-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_local_file_uris_are_accepted() {
        let (path, preview) =
            parse_request("file:///tmp/wall%20paper.png", &HashMap::new()).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/wall paper.png"));
        assert!(!preview);

        assert!(parse_request("https://example.com/wall.png", &HashMap::new()).is_err());
        assert!(parse_request("file://remote/share/w.png", &HashMap::new()).is_err());
        assert!(parse_request("relative.png", &HashMap::new()).is_err());
    }

    #[test]
    fn set_on_is_validated_but_not_forwarded() {
        // Every spec value passes; the compositor's single wallpaper
        // concept means the value never leaves option parsing.
        for value in ["background", "lockscreen", "both", ""] {
            let options = options(&[("set-on", Value::from(value))]);
            assert!(
                parse_request("file:///tmp/w.png", &options).is_ok(),
                "set-on={value:?}"
            );
        }
        let bad_value = options(&[("set-on", Value::from("screensaver"))]);
        assert!(parse_request("file:///tmp/w.png", &bad_value).is_err());

        let with_preview = options(&[("show-preview", Value::from(true))]);
        let (_, preview) = parse_request("file:///tmp/w.png", &with_preview).unwrap();
        assert!(preview);
    }

    #[test]
    fn image_reads_are_bounded() {
        let dir = scratch_dir("reads");
        let image = dir.join("w.png");
        std::fs::write(&image, b"\x89PNG fake").unwrap();
        assert_eq!(read_image(&image).unwrap(), b"\x89PNG fake");

        let empty = dir.join("empty.png");
        std::fs::write(&empty, b"").unwrap();
        assert!(read_image(&empty).is_err());
        assert!(read_image(&dir.join("missing.png")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn staged_names_are_stable_and_sanitized() {
        assert_eq!(staged_name(Path::new("/tmp/wall.png")), "current.png");
        assert_eq!(staged_name(Path::new("/tmp/wall.JPG")), "current.jpg");
        assert_eq!(staged_name(Path::new("/tmp/wall.avif")), "current.avif");
        // No extension, an empty one, or non-alphanumeric bytes all fall
        // back to the neutral name.
        assert_eq!(staged_name(Path::new("/tmp/wall")), "current.img");
        assert_eq!(staged_name(Path::new("/tmp/wall.p ng")), "current.img");
        assert_eq!(staged_name(Path::new("/tmp/wall.p.ng ")), "current.img");
    }

    #[test]
    fn staging_dir_requires_xdg_runtime_dir() {
        assert_eq!(
            staging_dir_from(Some(std::ffi::OsString::from("/run/user/1000"))),
            Some(PathBuf::from("/run/user/1000/atrium-portal/wallpaper"))
        );
        assert_eq!(staging_dir_from(Some("".into())), None);
        assert_eq!(staging_dir_from(None), None);
    }

    #[test]
    fn staging_writes_private_files_and_replaces_atomically() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch_dir("staging");
        let staging = dir.join("wallpaper");

        let first = stage_image(&staging, Path::new("/tmp/wall.png"), b"first").unwrap();
        assert_eq!(first, staging.join("current.png"));
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(
            std::fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&staging).unwrap().permissions().mode() & 0o777,
            0o700
        );

        // Same extension: the file is swapped, never truncated in place.
        let second = stage_image(&staging, Path::new("/tmp/other.png"), b"second").unwrap();
        assert_eq!(second, first);
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prune_drops_previous_names_and_keeps_the_applied_file() {
        let dir = scratch_dir("prune");
        let staging = dir.join("wallpaper");
        let old = stage_image(&staging, Path::new("/tmp/wall.png"), b"old").unwrap();
        let keep = stage_image(&staging, Path::new("/tmp/wall.jpg"), b"new").unwrap();

        prune_staging(&staging, &keep);
        assert!(!old.exists());
        assert_eq!(std::fs::read(&keep).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A compositor double: records applied paths, or refuses every swap.
    #[derive(Default)]
    struct Compositor {
        fail: bool,
        applied: Mutex<Vec<PathBuf>>,
    }

    impl atrium_portal_ipc::testing::Handler for Compositor {
        fn set_wallpaper(&self, _connection: u64, path: PathBuf) -> Result<(), String> {
            if self.fail {
                return Err("decode failed".into());
            }
            self.applied.lock().unwrap().push(path);
            Ok(())
        }
    }

    fn serve(
        name: &str,
        compositor: Arc<Compositor>,
    ) -> (atrium_portal_ipc::testing::Server, PathBuf) {
        let socket = scratch_dir(name).join("tessera.sock");
        let server = atrium_portal_ipc::testing::Server::start(&socket, compositor).unwrap();
        (server, socket)
    }

    fn tracker() -> Arc<Mutex<RequestTracker>> {
        Arc::new(Mutex::new(RequestTracker::default()))
    }

    #[test]
    fn successful_request_stages_applies_and_prunes() {
        let dir = scratch_dir("success");
        let staging = dir.join("wallpaper");
        let source = dir.join("vacation.png");
        std::fs::write(&source, b"png-bytes").unwrap();
        let compositor = Arc::new(Compositor::default());
        let (server, socket) = serve("success", Arc::clone(&compositor));
        // A previous wallpaper staged under a different extension.
        let previous = stage_image(&staging, Path::new("/tmp/old.jpg"), b"old").unwrap();

        let (response, _) = run_set_staged(
            &tracker(),
            None,
            "/request/1",
            "app",
            &socket,
            &staging,
            &source,
            false,
            None,
        );
        assert_eq!(response, 0);

        let staged = staging.join("current.png");
        assert_eq!(std::fs::read(&staged).unwrap(), b"png-bytes");
        // The compositor received exactly the staged path, and the previous
        // wallpaper's name was pruned after the swap.
        assert_eq!(compositor.applied.lock().unwrap().as_slice(), &[staged]);
        assert!(!previous.exists());
        drop(server);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_request_keeps_the_previous_wallpaper() {
        let dir = scratch_dir("failure");
        let staging = dir.join("wallpaper");
        // The current wallpaper, applied earlier.
        let previous = stage_image(&staging, Path::new("/tmp/current.png"), b"applied").unwrap();

        let source = dir.join("new.jpg");
        std::fs::write(&source, b"jpg-bytes").unwrap();
        let compositor = Arc::new(Compositor {
            fail: true,
            ..Compositor::default()
        });
        let (server, socket) = serve("failure", compositor);
        let (response, _) = run_set_staged(
            &tracker(),
            None,
            "/request/1",
            "app",
            &socket,
            &staging,
            &source,
            false,
            None,
        );
        assert_eq!(response, 2);

        // The previous wallpaper survived; the refused staging is gone.
        assert_eq!(std::fs::read(&previous).unwrap(), b"applied");
        assert!(!staging.join("current.jpg").exists());
        drop(server);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refusal_maps_to_response_two() {
        let dir = scratch_dir("refusal");
        let staging = dir.join("wallpaper");
        let source = dir.join("w.png");
        std::fs::write(&source, b"png-bytes").unwrap();
        let compositor = Arc::new(Compositor {
            fail: true,
            ..Compositor::default()
        });
        let (server, socket) = serve("refusal", compositor);
        let (response, _) = run_set_staged(
            &tracker(),
            None,
            "/request/1",
            "app",
            &socket,
            &staging,
            &source,
            false,
            None,
        );
        assert_eq!(response, 2);
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
        drop(server);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_protocol_24_compositor_speaks_the_wallpaper_op() {
        // The op predates the projection's version floor, so the legacy
        // handshake negotiates down and still applies.
        let dir = scratch_dir("legacy");
        let staging = dir.join("wallpaper");
        let source = dir.join("w.png");
        std::fs::write(&source, b"png-bytes").unwrap();
        let socket = scratch_dir("legacy-sock").join("tessera.sock");
        let compositor = Arc::new(Compositor::default());
        let server =
            atrium_portal_ipc::testing::Server::start_legacy(&socket, compositor, 24).unwrap();

        let (response, _) = run_set_staged(
            &tracker(),
            None,
            "/request/1",
            "app",
            &socket,
            &staging,
            &source,
            false,
            None,
        );
        assert_eq!(response, 0);
        assert_eq!(
            std::fs::read(staging.join("current.png")).unwrap(),
            b"png-bytes"
        );
        drop(server);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
