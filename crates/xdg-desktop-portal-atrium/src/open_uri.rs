//! `org.freedesktop.impl.portal.OpenURI` v3: open a URI or file with the
//! user's application.
//!
//! Resolution is entirely in-process: `file://` URIs percent-decode to a
//! path and take their content type from the shared-mime-info glob
//! databases ([`crate::apps`]); every other well-formed scheme maps to
//! `x-scheme-handler/<scheme>`. With `ask=false` (the default) a configured
//! default application launches directly; otherwise the same one-shot
//! `ChooseApp` prompter dialog AppChooser uses offers every registered
//! application, with the backend's "Remember this choice" checkbox
//! recording the selection as the content type's default. Answering 0
//! means launched, 1 means the user cancelled the chooser, 2 means the
//! URI was malformed, nothing can open the content type, or the launch
//! failed.
//!
//! Deliberate limitations, matching the rest of the backend:
//!
//! - Desktop entries with `Terminal=true` are refused (response 2, warn
//!   log): the portal does not pick a terminal emulator, and such entries
//!   are filtered out of chooser candidates since they can never launch.
//! - `writable` is accepted and ignored (debug log): the portal always
//!   opens the real file, never a read-only copy.
//! - `activation_token` is accepted and ignored (debug log): compositor
//!   activation tokens do not exist yet.
//! - `OpenFile` resolves the passed descriptor through `/proc/self/fd/`
//!   and treats it as the corresponding `file://` URI; the descriptor is
//!   closed immediately after resolving.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use atrium_portal_prompter::{
    AppChoice, Choice, ChooseAppRequest, ChooseAppResponse, PromptResult, PrompterRequest,
};
use zbus::zvariant::{ObjectPath, OwnedFd, Value};

use crate::apps::{self, AppDirs, AppInfo};
use crate::prompter::{self, InvokeError};
use crate::{app_chooser, files};
use atrium_portal_runtime::{PortalResponse, RequestTracker, ResponseSender, sync};

/// URIs past this size are rejected outright.
const MAX_URI_BYTES: usize = 8 * 1024;

/// One open request handed from the bus methods to the worker.
pub(crate) enum OpenUriJob {
    Open {
        request_path: String,
        app_id: String,
        /// Exactly what the launcher's field codes receive.
        uri: String,
        content_type: String,
        parent_window: Option<String>,
        ask: bool,
        reply: ResponseSender,
    },
}

/// The served OpenURI interface. The methods only classify, register the
/// request object, and enqueue; the decision and the chooser dialog run on
/// the worker.
pub(crate) struct OpenUriIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<OpenUriJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.OpenURI")]
impl OpenUriIface {
    async fn open_uri(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        uri: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        self.open(handle, app_id, parent_window, uri.to_owned(), &options)
            .await
    }

    async fn open_file(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        fd: OwnedFd,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        // Resolve the descriptor to its path, then drop it: the fd must
        // not outlive the method call.
        let uri = fd_path(&fd).map(|path| files::file_uri(&path));
        drop(fd);
        match uri {
            Ok(uri) => {
                self.open(handle, app_id, parent_window, uri, &options)
                    .await
            }
            Err(error) => {
                log::warn!("portal: refusing OpenFile for '{app_id}': {error}");
                Ok((2, HashMap::new()))
            }
        }
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        3
    }
}

impl OpenUriIface {
    async fn open(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        uri: String,
        options: &HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let path = handle.as_str().to_string();
        log::info!("portal: OpenURI for '{app_id}' at {path}: {uri}");

        let content_type = match content_type_for(&AppDirs::from_env(), &uri) {
            Ok(content_type) => content_type,
            Err(error) => {
                log::warn!("portal: refusing OpenURI request: {error}");
                return Ok((2, HashMap::new()));
            }
        };
        let ask = options
            .get("ask")
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(false);
        if options.contains_key("writable") {
            log::debug!("portal: ignoring OpenURI 'writable'; the real file is always opened");
        }
        if options.contains_key("activation_token") {
            log::debug!(
                "portal: ignoring OpenURI 'activation_token'; compositor activation tokens do not exist yet"
            );
        }

        atrium_portal_runtime::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "open",
            &self.jobs,
            |reply| OpenUriJob::Open {
                request_path: path.clone(),
                app_id: app_id.to_string(),
                uri,
                content_type,
                parent_window: (!parent_window.is_empty()).then(|| parent_window.to_owned()),
                ask,
                reply,
            },
        )
        .await
    }
}

/// A classified URI: a decoded local path, or a lowercased scheme name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    File(PathBuf),
    Scheme(String),
}

/// Validate and classify a URI. Rejects empty/oversized URIs, control
/// characters (including NUL), and missing or malformed schemes.
fn classify(uri: &str) -> Result<Target, String> {
    if uri.is_empty() || uri.len() > MAX_URI_BYTES {
        return Err("URI is empty or oversized".to_string());
    }
    if uri.chars().any(char::is_control) {
        return Err("URI contains control characters".to_string());
    }
    let Some(colon) = uri.find(':') else {
        return Err("URI has no scheme".to_string());
    };
    let scheme = &uri[..colon];
    let mut characters = scheme.chars();
    let valid = characters.next().is_some_and(|c| c.is_ascii_alphabetic())
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !valid {
        return Err(format!("URI scheme {scheme:?} is malformed"));
    }
    if scheme.eq_ignore_ascii_case("file") {
        // path_from_file_uri expects the canonical lowercase scheme;
        // `uri[colon..]` starts at the ':'.
        let canonical = format!("file{}", &uri[colon..]);
        return files::path_from_file_uri(&canonical)
            .map(Target::File)
            .ok_or_else(|| "file URI does not name an absolute local path".to_string());
    }
    Ok(Target::Scheme(scheme.to_ascii_lowercase()))
}

/// The content type an open request resolves to: glob-based for local
/// files (`application/octet-stream` when no glob matches), the
/// `x-scheme-handler` pseudo-type otherwise.
fn content_type_for(dirs: &AppDirs, uri: &str) -> Result<String, String> {
    match classify(uri)? {
        Target::File(path) => {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            Ok(dirs
                .content_type_for_filename(name)
                .unwrap_or_else(|| "application/octet-stream".to_owned()))
        }
        Target::Scheme(scheme) => Ok(format!("x-scheme-handler/{scheme}")),
    }
}

/// The worker's decision before any dialog: launch the default directly,
/// or offer the chooser these candidates.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Launch(AppInfo),
    Choose(Vec<AppChoice>),
    Nothing,
}

/// Apply the `ask` semantics and the terminal limitation: with `ask=false`
/// a configured default launches directly; otherwise every registered,
/// launchable application is offered with the default leading (the dialog
/// pre-selects the first row). `Terminal=true` entries are filtered out of
/// the chooser because [`apps::launch`] cannot run them.
fn decide(dirs: &AppDirs, content_type: &str, ask: bool) -> Decision {
    let default = dirs.default_app(content_type);
    if !ask && let Some(app) = default {
        return Decision::Launch(app);
    }
    let mut candidates: Vec<AppChoice> = dirs
        .apps_for_content_type(content_type)
        .into_iter()
        .filter(|app| {
            if app.terminal {
                log::debug!(
                    "portal: hiding terminal application '{}' from the OpenURI chooser",
                    app.id
                );
            }
            !app.terminal
        })
        .map(|app| AppChoice {
            id: app.id,
            name: app.name,
            icon: app.icon,
        })
        .collect();
    if candidates.is_empty() {
        return Decision::Nothing;
    }
    candidates =
        app_chooser::order_candidates(candidates, default.as_ref().map(|app| app.id.as_str()));
    Decision::Choose(candidates)
}

/// Launch one resolved application, honouring the terminal limitation.
fn launch_app(app: &AppInfo, uri: &str) -> Result<(), String> {
    if app.terminal {
        return Err(format!(
            "default application '{}' needs a terminal, which the portal does not launch",
            app.id
        ));
    }
    apps::launch(&app.exec, &[uri.to_owned()])
        .map_err(|error| format!("could not launch '{}': {error}", app.id))
}

/// Dispatch open requests independently so one application leaving the
/// chooser open cannot head-of-line block every other OpenURI request.
pub(crate) fn open_uri_worker(
    rx: mpsc::Receiver<OpenUriJob>,
    tracker: Arc<Mutex<RequestTracker>>,
    settings: crate::settings::SettingsStore,
) {
    const MAX_ACTIVE_OPEN_REQUESTS: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(OpenUriJob::Open {
        request_path,
        app_id,
        uri,
        content_type,
        parent_window,
        ask,
        reply,
    }) = rx.recv()
    {
        if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_ACTIVE_OPEN_REQUESTS {
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            log::warn!("portal: refusing OpenURI request: concurrency limit reached");
            let _ = reply.send_blocking((2, HashMap::new()));
            continue;
        }
        let task_tracker = Arc::clone(&tracker);
        let task_settings = settings.clone();
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("atrium-portal-open-uri-task".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let result = run_open(
                    &task_tracker,
                    Some(&task_settings),
                    &request_path,
                    &app_id,
                    &uri,
                    &content_type,
                    parent_window,
                    ask,
                );
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn OpenURI task: {error}");
            let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
        }
    }
}

/// Execute one request: launch the default, or show the chooser and launch
/// the selection (recording it when the remember checkbox was ticked).
#[allow(clippy::too_many_arguments)]
fn run_open(
    tracker: &Arc<Mutex<RequestTracker>>,
    settings: Option<&crate::settings::SettingsStore>,
    request_path: &str,
    app_id: &str,
    uri: &str,
    content_type: &str,
    parent_window: Option<String>,
    ask: bool,
) -> (u32, HashMap<String, Value<'static>>) {
    if sync::lock(tracker, "open uri tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }
    let dirs = AppDirs::from_env();
    match decide(&dirs, content_type, ask) {
        Decision::Launch(app) => match launch_app(&app, uri) {
            Ok(()) => {
                log::info!("portal: OpenURI for '{app_id}' launched '{}'", app.id);
                (0, HashMap::new())
            }
            Err(error) => {
                log::warn!("portal: OpenURI for '{app_id}' failed: {error}");
                (2, HashMap::new())
            }
        },
        Decision::Nothing => {
            log::warn!("portal: OpenURI for '{app_id}': nothing opens {content_type}");
            (2, HashMap::new())
        }
        Decision::Choose(candidates) => run_chooser(
            tracker,
            settings,
            request_path,
            app_id,
            uri,
            content_type,
            parent_window,
            candidates,
        ),
    }
}

/// Show the chooser dialog and act on its answer.
#[allow(clippy::too_many_arguments)]
fn run_chooser(
    tracker: &Arc<Mutex<RequestTracker>>,
    settings: Option<&crate::settings::SettingsStore>,
    request_path: &str,
    app_id: &str,
    uri: &str,
    content_type: &str,
    parent_window: Option<String>,
    candidates: Vec<AppChoice>,
) -> (u32, HashMap<String, Value<'static>>) {
    let request = ChooseAppRequest {
        app_id: app_id.to_owned(),
        title: "Open With".to_owned(),
        content_type: content_type.to_owned(),
        parent_window,
        apps: candidates,
        choices: vec![Choice {
            id: "remember".to_owned(),
            label: "Remember this choice".to_owned(),
            options: Vec::new(),
            selected: "false".to_owned(),
        }],
    };
    if let Err(error) = request.validate() {
        log::warn!("portal: invalid OpenURI chooser request: {error}");
        return (2, HashMap::new());
    }

    let cancelled = || sync::lock(tracker, "open uri tracker").was_closed(request_path);
    let answered = prompter::invoke(
        PrompterRequest::choose_app(request.clone()),
        settings,
        Some(&cancelled),
    );
    match answered {
        Ok(PromptResult::ChooseApp(response)) => {
            // Request.Close wins a race with a completed child response.
            if sync::lock(tracker, "open uri tracker").was_closed(request_path) {
                return (1, HashMap::new());
            }
            if let Err(error) = response.validate_for(&request) {
                log::warn!("portal: invalid OpenURI chooser response for '{app_id}': {error}");
                return (2, HashMap::new());
            }
            let ChooseAppResponse::Selected { app, choices } = response else {
                return (1, HashMap::new());
            };
            let Some(app_info) = AppDirs::from_env().app_by_id(&app) else {
                log::warn!("portal: chooser returned an unresolvable app {app:?}");
                return (2, HashMap::new());
            };
            if let Err(error) = launch_app(&app_info, uri) {
                log::warn!("portal: OpenURI for '{app_id}' failed: {error}");
                return (2, HashMap::new());
            }
            if app_chooser::remembers(&choices)
                && let Err(error) = AppDirs::from_env().set_default_app(content_type, &app)
            {
                log::warn!(
                    "portal: could not record the default application for '{content_type}': {error}"
                );
            }
            log::info!("portal: OpenURI for '{app_id}' launched '{app}'");
            (0, HashMap::new())
        }
        Err(InvokeError::Cancelled) => (1, HashMap::new()),
        Ok(_) => {
            log::warn!("portal: OpenURI prompter returned the wrong response kind");
            (2, HashMap::new())
        }
        Err(InvokeError::Failed(error)) => {
            log::warn!("portal: OpenURI chooser for '{app_id}' failed: {error}");
            (2, HashMap::new())
        }
    }
}

/// Resolve an `OpenFile` descriptor to its on-disk path through
/// `/proc/self/fd/`.
fn fd_path(fd: &OwnedFd) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// A fixture XDG tree with a text editor and a browser handler.
    struct Fixture {
        root: PathBuf,
        dirs: AppDirs,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "tessera-open-uri-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let dirs = AppDirs::fixture(
                root.join("data-home"),
                vec![root.join("data-a")],
                root.join("config-home"),
                vec![],
            );
            let applications = root.join("data-home/applications");
            std::fs::create_dir_all(&applications).unwrap();
            std::fs::write(
                applications.join("editor.desktop"),
                "[Desktop Entry]\nName=Foo Editor\nExec=foo-edit %U\nMimeType=text/plain;\n",
            )
            .unwrap();
            std::fs::write(
                applications.join("browser.desktop"),
                "[Desktop Entry]\nName=Bar Browser\nExec=bar-browse %u\nMimeType=x-scheme-handler/http;\n",
            )
            .unwrap();
            Self { root, dirs }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn file_uris_decode_to_paths_including_non_ascii() {
        assert_eq!(
            classify("file:///tmp/plain.txt"),
            Ok(Target::File(PathBuf::from("/tmp/plain.txt")))
        );
        assert_eq!(
            classify("file:///tmp/caf%C3%A9%20au%20lait.md"),
            Ok(Target::File(PathBuf::from("/tmp/caf\u{e9} au lait.md")))
        );
        assert_eq!(
            classify("FILE:///tmp/upper.txt"),
            Ok(Target::File(PathBuf::from("/tmp/upper.txt")))
        );
        assert!(classify("file://remote.example/share").is_err());
        assert!(classify("file://relative").is_err());
    }

    #[test]
    fn schemes_map_to_scheme_handlers() {
        assert_eq!(
            classify("https://example.com/page?a=b"),
            Ok(Target::Scheme("https".to_owned()))
        );
        assert_eq!(
            classify("MAILTO:user@example.com"),
            Ok(Target::Scheme("mailto".to_owned()))
        );
    }

    #[test]
    fn malformed_uris_are_rejected() {
        assert!(classify("").is_err());
        assert!(classify("no-scheme-here").is_err());
        assert!(classify("1http://example.com").is_err());
        assert!(classify("bad scheme://x").is_err());
        assert!(classify("https://example.com/a\u{7}b").is_err());
        assert!(classify(&format!("https://{}", "x".repeat(MAX_URI_BYTES))).is_err());
    }

    #[test]
    fn content_types_come_from_globs_or_the_scheme() {
        let fixture = Fixture::new("types");
        fixture_root_globs(&fixture.root);
        let dirs = &fixture.dirs;
        assert_eq!(
            content_type_for(dirs, "file:///tmp/notes.txt").unwrap(),
            "text/plain"
        );
        assert_eq!(
            content_type_for(dirs, "file:///tmp/blob").unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            content_type_for(dirs, "https://example.com").unwrap(),
            "x-scheme-handler/https"
        );
    }

    fn fixture_root_globs(root: &Path) {
        let mime = root.join("data-home/mime");
        std::fs::create_dir_all(&mime).unwrap();
        std::fs::write(mime.join("globs2"), "50:*.txt:text/plain\n").unwrap();
    }

    #[test]
    fn decision_launches_the_default_unless_asked() {
        let fixture = Fixture::new("decide");
        fixture_root_globs(&fixture.root);
        let dirs = &fixture.dirs;

        // No default configured: the chooser offers the registered editor.
        match decide(dirs, "text/plain", false) {
            Decision::Choose(candidates) => {
                assert_eq!(candidates.len(), 1);
                assert_eq!(candidates[0].id, "editor.desktop");
            }
            other => panic!("expected the chooser, got {other:?}"),
        }
        assert_eq!(decide(dirs, "image/png", false), Decision::Nothing);

        // A configured default launches directly, but ask=true still shows
        // the chooser with the default leading.
        dirs.set_default_app("text/plain", "editor.desktop")
            .unwrap();
        match decide(dirs, "text/plain", false) {
            Decision::Launch(app) => assert_eq!(app.id, "editor.desktop"),
            other => panic!("expected a direct launch, got {other:?}"),
        }
        match decide(dirs, "text/plain", true) {
            Decision::Choose(candidates) => {
                assert_eq!(candidates[0].id, "editor.desktop")
            }
            other => panic!("expected the chooser, got {other:?}"),
        }
    }

    #[test]
    fn terminal_applications_are_filtered_from_the_chooser() {
        let fixture = Fixture::new("terminal");
        fixture_root_globs(&fixture.root);
        std::fs::write(
            fixture.root.join("data-home/applications/shell-view.desktop"),
            "[Desktop Entry]\nName=Shell View\nExec=shell-view %f\nTerminal=true\nMimeType=text/plain;\n",
        )
        .unwrap();
        let Decision::Choose(candidates) = decide(&fixture.dirs, "text/plain", false) else {
            panic!("expected the chooser");
        };
        assert!(candidates.iter().all(|app| app.id != "shell-view.desktop"));
    }

    #[test]
    fn fds_resolve_to_their_paths() {
        let file = std::fs::File::open("/etc/hostname").unwrap();
        let owned: OwnedFd = std::os::fd::OwnedFd::from(file).into();
        let path = fd_path(&owned).unwrap();
        drop(owned);
        assert_eq!(path, PathBuf::from("/etc/hostname"));
        assert_eq!(files::file_uri(&path), "file:///etc/hostname");
    }
}
