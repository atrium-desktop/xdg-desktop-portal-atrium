//! `org.freedesktop.impl.portal.AppChooser` v4: the "open with" dialog.
//!
//! The frontend names a content type (and optionally an explicit candidate
//! list); the backend resolves real desktop entries through
//! [`atrium_portal_apps`] and parks the request on a worker while the
//! compositor presents the candidates through its `PickApp` chrome
//! (ADR-0026). Answering reports `{"choice": desktop_id}` with response 0;
//! dismissal or a racing `Request.Close` answers 1, and any resolution or
//! compositor failure answers 2.
//!
//! Three spec features are deliberately degraded, because the compositor's
//! picker is a candidate list and nothing else:
//!
//! - `UpdateChoices` is accepted and shape-checked but not rendered: the
//!   picker is a compositor prompt that cannot be mutated mid-flight, so the
//!   frontend's live updates are logged and acknowledged. Any choices the
//!   frontend needs on screen must ride the initial `options["choices"]`.
//! - `options["choices"]` is parsed in the FileChooser `(ssas)s` wire shape
//!   and validated, but the compositor picker renders no embedded controls,
//!   so no checkbox is shown and the default-application write that the
//!   legacy remember checkbox drove does not happen. The frontend still
//!   applies its own default through the app's own configuration.
//! - `last_choice` is forwarded to the compositor, which pre-highlights the
//!   row; the backend no longer reorders the candidate list for it.
//!
//! Icons are resolved from the desktop entry to keep the candidate list
//! faithful to the installed application; the compositor renders names.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};

use zbus::zvariant::{ObjectPath, Value};

use crate::ipc;
use atrium_portal_apps::{AppDirs, AppInfo};
use atrium_portal_request::{PortalResponse, RequestTracker, ResponseSender, sync};

/// One application offered to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppChoice {
    /// The desktop file id; also the value the portal reports as the choice.
    pub(crate) id: String,
    /// The user-visible application name.
    pub(crate) name: String,
    /// The themed icon name from the desktop entry, if any.
    pub(crate) icon: Option<String>,
}

/// One resolved AppChooser request: the candidates plus the context the
/// compositor's picker needs. This is the backend's own model, not a wire
/// contract: the compositor receives only desktop ids (ADR-0026).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChooseAppRequest {
    pub(crate) app_id: String,
    pub(crate) title: String,
    pub(crate) content_type: String,
    pub(crate) parent_window: Option<String>,
    pub(crate) apps: Vec<AppChoice>,
    /// The frontend's previously used application, forwarded to the
    /// compositor so it can pre-highlight that row.
    pub(crate) last_choice: Option<String>,
}

impl ChooseAppRequest {
    /// Reject malformed values before the compositor is asked anything, and
    /// bound the candidate list it will be handed.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.title.trim().is_empty() || self.content_type.trim().is_empty() {
            return Err("app chooser title and content type must not be empty".into());
        }
        if self.apps.is_empty() || self.apps.len() > MAX_CANDIDATES {
            return Err(format!("app chooser needs 1..={MAX_CANDIDATES} candidates"));
        }
        let mut ids = std::collections::BTreeSet::new();
        for app in &self.apps {
            if app.id.is_empty() || app.id.contains('\0') {
                return Err("app ids must be non-empty and NUL-free".into());
            }
            if !ids.insert(app.id.as_str()) {
                return Err(format!("duplicate app candidate {:?}", app.id));
            }
        }
        Ok(())
    }
}

/// Candidate and embedded-choice caps.
const MAX_CANDIDATES: usize = 64;
const MAX_CHOICES: usize = 16;

/// One choose-application request handed from the bus method to the worker.
pub(crate) enum AppChooserJob {
    Choose {
        request_path: String,
        app_id: String,
        request: ChooseAppRequest,
        reply: ResponseSender,
    },
}

/// The served app-chooser interface. The method only resolves candidates,
/// registers the request object, and enqueues; the dialog runs on the
/// worker.
pub(crate) struct AppChooserIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<AppChooserJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.AppChooser")]
impl AppChooserIface {
    #[allow(clippy::too_many_arguments)]
    async fn choose_application(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        choices: Vec<String>,
        content_type: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let path = handle.as_str().to_string();
        log::info!("portal: ChooseApplication for '{app_id}' ({content_type}) at {path}");

        let dirs = AppDirs::from_env();
        let request = match build_request(
            &dirs,
            app_id,
            parent_window,
            &choices,
            content_type,
            &options,
        ) {
            Ok(request) => request,
            Err(error) => {
                log::warn!("portal: refusing AppChooser request: {error}");
                return Ok((2, HashMap::new()));
            }
        };

        atrium_portal_request::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "choose",
            &self.jobs,
            |reply| AppChooserJob::Choose {
                request_path: path.clone(),
                app_id: app_id.to_string(),
                request,
                reply,
            },
        )
        .await
    }

    /// The frontend's mid-dialog choice updates. The one-shot prompter
    /// cannot re-render, so the update is validated, logged, and
    /// acknowledged; see the module docs.
    async fn update_choices(
        &self,
        handle: ObjectPath<'_>,
        choices: Vec<(String, HashMap<String, Value<'_>>)>,
    ) -> zbus::fdo::Result<()> {
        if choices.len() > MAX_CHOICES
            || choices
                .iter()
                .any(|(id, _)| id.is_empty() || id.len() > 256 || id.contains('\0'))
        {
            return Err(zbus::fdo::Error::InvalidArgs(
                "malformed AppChooser choices update".to_string(),
            ));
        }
        log::info!(
            "portal: UpdateChoices for {} ({} choice(s)) is not rendered by the one-shot prompter",
            handle.as_str(),
            choices.len()
        );
        Ok(())
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        4
    }
}

/// Resolve the portal arguments into the prompter's request: real desktop
/// entries, the preferred candidate first, and the embedded choices
/// (including the backend's own remember checkbox).
fn build_request(
    dirs: &AppDirs,
    app_id: &str,
    parent_window: &str,
    choices: &[String],
    content_type: &str,
    options: &HashMap<String, Value<'_>>,
) -> Result<ChooseAppRequest, String> {
    let apps = resolve_candidates(dirs, choices, content_type);
    if apps.is_empty() {
        return Err(format!("no application can open {content_type:?}"));
    }

    // The `last_choice` hint leads the list (the dialog pre-selects the
    // first row); without it the configured default leads.
    let last_choice = options
        .get("last_choice")
        .and_then(|value| String::try_from(value).ok());
    let preferred = last_choice
        .as_deref()
        .filter(|id| apps.iter().any(|app| &app.id == id))
        .map(str::to_owned)
        .or_else(|| {
            dirs.default_app(content_type)
                .map(|default| default.id)
                .filter(|id| apps.iter().any(|app| &app.id == id))
        });
    note_embedded_choices(options);

    let request = ChooseAppRequest {
        app_id: app_id.to_owned(),
        title: "Open with".to_owned(),
        content_type: content_type.to_owned(),
        parent_window: (!parent_window.is_empty()).then(|| parent_window.to_owned()),
        apps,
        last_choice: preferred,
    };
    request.validate()?;
    Ok(request)
}

/// The candidate list: the frontend's explicit desktop ids resolved
/// through the desktop-file database (unknown or unlaunchable ids are
/// skipped), or every application registered for the content type.
fn resolve_candidates(dirs: &AppDirs, supplied: &[String], content_type: &str) -> Vec<AppChoice> {
    let mut seen = std::collections::HashSet::new();
    let resolved: Vec<AppInfo> = if supplied.is_empty() {
        dirs.apps_for_content_type(content_type)
    } else {
        supplied
            .iter()
            .filter_map(|id| dirs.app_by_id(id))
            .filter(|app| !app.exec.is_empty())
            .collect()
    };
    resolved
        .into_iter()
        .filter(|app| seen.insert(app.id.clone()))
        .take(MAX_CANDIDATES)
        .map(|app| AppChoice {
            id: app.id,
            name: app.name,
            icon: app.icon,
        })
        .collect()
}

/// Log the embedded choices the frontend offered. The compositor picker
/// renders a candidate list and no embedded controls (ADR-0026), so these
/// are shape-checked and acknowledged rather than shown.
fn note_embedded_choices(options: &HashMap<String, Value<'_>>) {
    let Some(value) = options.get("choices") else {
        return;
    };
    let count = value
        .try_clone()
        .ok()
        .and_then(|value| {
            Vec::<(String, String, Vec<(String, String)>, String)>::try_from(value).ok()
        })
        .map(|wire| wire.len());
    match count {
        Some(count) => log::info!(
            "portal: AppChooser offered {count} embedded choice(s); the compositor picker renders none"
        ),
        None => {
            log::info!("portal: ignoring AppChooser options[\"choices\"] with an unexpected shape")
        }
    }
}

/// Dispatch chooser dialogs independently so one application leaving the
/// dialog open cannot head-of-line block every other AppChooser request.
pub(crate) fn app_chooser_worker(
    rx: mpsc::Receiver<AppChooserJob>,
    tracker: Arc<Mutex<RequestTracker>>,
) {
    const MAX_ACTIVE_APP_CHOOSERS: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(AppChooserJob::Choose {
        request_path,
        app_id,
        request,
        reply,
    }) = rx.recv()
    {
        if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_ACTIVE_APP_CHOOSERS {
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            log::warn!("portal: refusing AppChooser request: concurrency limit reached");
            let _ = reply.send_blocking((2, HashMap::new()));
            continue;
        }
        let task_tracker = Arc::clone(&tracker);
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("atrium-portal-app-chooser-task".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let result = run_dialog(&task_tracker, &request_path, &app_id, request);
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn AppChooser task: {error}");
            let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
        }
    }
}

/// Execute one request: show the chooser dialog, then relay the selection
/// (recording it as the default when the remember checkbox was ticked).
fn run_dialog(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    app_id: &str,
    request: ChooseAppRequest,
) -> (u32, HashMap<String, Value<'static>>) {
    if sync::lock(tracker, "app chooser tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }

    // The compositor renders the picker (ADR-0026). A non-empty parent
    // window names the surface that asked; the compositor parents its own
    // chrome.
    if request.parent_window.is_some() {
        log::debug!("portal: AppChooser parent window is compositor-owned");
    }
    let socket = match ipc::compositor_socket() {
        Ok(socket) => socket,
        Err(error) => {
            log::warn!(
                "portal: ChooseApplication for '{app_id}' cannot locate the compositor: {error}"
            );
            return (2, HashMap::new());
        }
    };
    let choices: Vec<String> = request.apps.iter().map(|app| app.id.clone()).collect();
    let answered = ipc::request_app_choice(
        &socket,
        choices,
        Some(format!("Open {}", request.content_type)),
        request.last_choice.clone(),
    );
    match answered {
        Ok(atrium_portal_ipc::AppPickResult::App { id }) => {
            // Request.Close wins a race with a completed answer.
            if sync::lock(tracker, "app chooser tracker").was_closed(request_path) {
                return (1, HashMap::new());
            }
            if !request.apps.iter().any(|app| app.id == id) {
                log::warn!("portal: compositor chose an unoffered application {id:?}");
                return (2, HashMap::new());
            }
            log::info!("portal: ChooseApplication for '{app_id}' -> {id}");
            (0, HashMap::from([("choice".to_owned(), Value::from(id))]))
        }
        Ok(atrium_portal_ipc::AppPickResult::Cancelled) => (1, HashMap::new()),
        Err(error) => {
            log::warn!("portal: ChooseApplication for '{app_id}' failed: {error}");
            (2, HashMap::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A fixture XDG tree with two text/plain handlers.
    struct Fixture {
        root: PathBuf,
        dirs: AppDirs,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "tessera-app-chooser-{name}-{}-{}",
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
                "[Desktop Entry]\nName=Foo Editor\nExec=foo-edit %U\nIcon=foo-edit\nMimeType=text/plain;\n",
            )
            .unwrap();
            std::fs::write(
                applications.join("viewer.desktop"),
                "[Desktop Entry]\nName=Bar Viewer\nExec=bar-view %u\nMimeType=text/plain;\n",
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

    fn options(pairs: &[(&str, Value<'static>)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn explicit_candidates_resolve_and_unknown_ids_drop_out() {
        let fixture = Fixture::new("explicit");
        let apps = resolve_candidates(
            &fixture.dirs,
            &[
                "viewer.desktop".to_owned(),
                "missing.desktop".to_owned(),
                "../evil.desktop".to_owned(),
                "editor.desktop".to_owned(),
            ],
            "text/plain",
        );
        let ids: Vec<&str> = apps.iter().map(|app| app.id.as_str()).collect();
        assert_eq!(ids, ["viewer.desktop", "editor.desktop"]);
        assert_eq!(apps[0].name, "Bar Viewer");
        assert_eq!(apps[1].icon.as_deref(), Some("foo-edit"));
    }

    #[test]
    fn enumeration_falls_back_to_the_content_type_index() {
        let fixture = Fixture::new("enumerate");
        let apps = resolve_candidates(&fixture.dirs, &[], "text/plain");
        assert_eq!(apps.len(), 2);
        assert!(resolve_candidates(&fixture.dirs, &[], "image/png").is_empty());
    }

    #[test]
    fn the_preferred_candidate_is_forwarded_as_last_choice() {
        let fixture = Fixture::new("preferred");
        let request = build_request(
            &fixture.dirs,
            "app",
            "",
            &[],
            "text/plain",
            &options(&[("last_choice", Value::from("viewer.desktop"))]),
        )
        .expect("a resolvable choice");
        assert_eq!(request.last_choice.as_deref(), Some("viewer.desktop"));
        // An unknown preference is not forwarded.
        let request = build_request(
            &fixture.dirs,
            "app",
            "",
            &[],
            "text/plain",
            &options(&[("last_choice", Value::from("missing.desktop"))]),
        )
        .expect("a resolvable choice");
        assert!(
            request.last_choice.is_none()
                || request.last_choice.as_deref() != Some("missing.desktop")
        );
    }

    #[test]
    fn the_compositor_receives_only_desktop_ids() {
        let fixture = Fixture::new("ids");
        let request = build_request(
            &fixture.dirs,
            "app",
            "",
            &["viewer.desktop".to_owned()],
            "text/plain",
            &HashMap::new(),
        )
        .expect("a resolvable choice");
        let ids: Vec<_> = request.apps.iter().map(|app| app.id.as_str()).collect();
        assert_eq!(ids, ["viewer.desktop"]);
        // The embedded controls the frontend offered do not reach the picker.
        let with_choices = options(&[(
            "choices",
            Value::from(vec![(
                "encoding".to_owned(),
                "Encoding".to_owned(),
                vec![("utf8".to_owned(), "UTF-8".to_owned())],
                "utf8".to_owned(),
            )]),
        )]);
        let request = build_request(
            &fixture.dirs,
            "app",
            "",
            &["viewer.desktop".to_owned()],
            "text/plain",
            &with_choices,
        )
        .expect("a resolvable choice");
        assert_eq!(request.apps.len(), 1);
    }

    #[test]
    fn duplicate_candidates_are_rejected() {
        let request = ChooseAppRequest {
            app_id: "app".into(),
            title: "Open with".into(),
            content_type: "text/plain".into(),
            parent_window: None,
            apps: vec![
                AppChoice {
                    id: "a.desktop".into(),
                    name: "A".into(),
                    icon: None,
                },
                AppChoice {
                    id: "a.desktop".into(),
                    name: "A again".into(),
                    icon: None,
                },
            ],
            last_choice: None,
        };
        assert!(request.validate().is_err());
    }
}
