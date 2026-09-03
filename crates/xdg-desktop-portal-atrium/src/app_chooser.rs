//! `org.freedesktop.impl.portal.AppChooser` v4: the "open with" dialog.
//!
//! The frontend names a content type (and optionally an explicit candidate
//! list); the backend resolves real desktop entries through [`crate::apps`]
//! and parks the request on a worker while a Portal-owned, one-shot
//! prompter dialog presents the candidates. Answering reports
//! `{"choice": desktop_id}` with response 0; dismissal or a racing
//! `Request.Close` answers 1, and any resolution or prompter failure
//! answers 2. When the user ticks the remember checkbox (choice id
//! `remember`/`always`), the selection is also recorded as the
//! content type's default in `$XDG_CONFIG_HOME/mimeapps.list`.
//!
//! Two spec features are deliberately degraded, matching the trade-off the
//! routing config has always documented for this interface:
//!
//! - `UpdateChoices` is accepted and shape-checked but not rendered: the
//!   prompter is a one-shot process whose dialog cannot be mutated
//!   mid-flight, so the frontend's live updates are logged at info level
//!   and acknowledged. Any choices the frontend needs on screen must ride
//!   the initial `options["choices"]` instead.
//! - `options["choices"]` is parsed in the FileChooser `(ssas)s` wire
//!   shape; a mismatched value is logged and ignored rather than failing
//!   the request, since the spec's encoding for this key has varied
//!   across frontend versions.
//!
//! Icons are resolved into the prompter contract but rendered as names
//! only; see the prompter's `choose_app` dialog docs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};

use atrium_portal_prompter::{
    AppChoice, Choice, ChooseAppRequest, ChooseAppResponse, PromptResult, PrompterRequest,
};
use zbus::zvariant::{ObjectPath, Value};

use crate::apps::{AppDirs, AppInfo};
use crate::prompter::{self, InvokeError};
use atrium_portal_runtime::{PortalResponse, RequestTracker, ResponseSender, sync};

/// Candidate and embedded-choice caps, mirroring the prompter contract.
const MAX_CANDIDATES: usize = 64;
const MAX_CHOICES: usize = 16;
/// The choice ids treated as "make this the default application".
const REMEMBER_IDS: [&str; 2] = ["remember", "always"];

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

        let request = match build_request(app_id, parent_window, &choices, content_type, &options) {
            Ok(request) => request,
            Err(error) => {
                log::warn!("portal: refusing AppChooser request: {error}");
                return Ok((2, HashMap::new()));
            }
        };

        atrium_portal_runtime::dispatch(
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
    app_id: &str,
    parent_window: &str,
    choices: &[String],
    content_type: &str,
    options: &HashMap<String, Value<'_>>,
) -> Result<ChooseAppRequest, String> {
    let dirs = AppDirs::from_env();
    let apps = resolve_candidates(&dirs, choices, content_type);
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
    let apps = order_candidates(apps, preferred.as_deref());

    let request = ChooseAppRequest {
        app_id: app_id.to_owned(),
        title: "Open with".to_owned(),
        content_type: content_type.to_owned(),
        parent_window: (!parent_window.is_empty()).then(|| parent_window.to_owned()),
        apps,
        choices: prompter_choices(options),
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

/// Move the preferred candidate to the front; the dialog pre-selects row 0.
/// Shared with OpenURI, which pre-selects the configured default the same
/// way.
pub(crate) fn order_candidates(
    mut apps: Vec<AppChoice>,
    preferred: Option<&str>,
) -> Vec<AppChoice> {
    if let Some(preferred) = preferred
        && let Some(index) = apps.iter().position(|app| app.id == preferred)
    {
        let app = apps.remove(index);
        apps.insert(0, app);
    }
    apps
}

/// The embedded dialog choices: the frontend's `options["choices"]` in the
/// FileChooser wire shape (parse failures are logged and ignored — the
/// frontend can still drive `UpdateChoices`, which we acknowledge), plus
/// the backend's remember checkbox when the frontend supplied none.
fn prompter_choices(options: &HashMap<String, Value<'_>>) -> Vec<Choice> {
    let mut choices: Vec<Choice> = match options.get("choices") {
        None => Vec::new(),
        Some(value) => match value.try_clone().ok().and_then(|value| {
            Vec::<(String, String, Vec<(String, String)>, String)>::try_from(value).ok()
        }) {
            Some(wire) => wire
                .into_iter()
                .take(MAX_CHOICES)
                .map(|(id, label, options, selected)| Choice {
                    id,
                    label,
                    options,
                    selected,
                })
                .collect(),
            None => {
                log::info!(
                    "portal: ignoring AppChooser options[\"choices\"] with an unexpected shape"
                );
                Vec::new()
            }
        },
    };
    if !choices
        .iter()
        .any(|choice| REMEMBER_IDS.contains(&choice.id.as_str()))
    {
        choices.push(Choice {
            id: "remember".to_owned(),
            label: "Remember this choice".to_owned(),
            options: Vec::new(),
            selected: "false".to_owned(),
        });
    }
    choices
}

/// Whether the answered choices tick a remember checkbox. Shared with
/// OpenURI, which appends the same checkbox to its chooser dialog.
pub(crate) fn remembers(choices: &[(String, String)]) -> bool {
    choices
        .iter()
        .any(|(id, value)| REMEMBER_IDS.contains(&id.as_str()) && value == "true")
}

/// Dispatch chooser dialogs independently so one application leaving the
/// dialog open cannot head-of-line block every other AppChooser request.
pub(crate) fn app_chooser_worker(
    rx: mpsc::Receiver<AppChooserJob>,
    tracker: Arc<Mutex<RequestTracker>>,
    settings: crate::settings::SettingsStore,
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
        let task_settings = settings.clone();
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("atrium-portal-app-chooser-task".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let result = run_dialog(
                    &task_tracker,
                    &request_path,
                    &app_id,
                    request,
                    Some(&task_settings),
                );
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
    settings: Option<&crate::settings::SettingsStore>,
) -> (u32, HashMap<String, Value<'static>>) {
    if sync::lock(tracker, "app chooser tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }

    let cancelled = || sync::lock(tracker, "app chooser tracker").was_closed(request_path);
    let answered = prompter::invoke(
        PrompterRequest::choose_app(request.clone()),
        settings,
        Some(&cancelled),
    );
    match answered {
        Ok(PromptResult::ChooseApp(response)) => {
            // Request.Close wins a race with a completed child response.
            if sync::lock(tracker, "app chooser tracker").was_closed(request_path) {
                return (1, HashMap::new());
            }
            if let Err(error) = response.validate_for(&request) {
                log::warn!("portal: invalid AppChooser response for '{app_id}': {error}");
                return (2, HashMap::new());
            }
            match response {
                ChooseAppResponse::Selected { app, choices } => {
                    if remembers(&choices)
                        && let Err(error) =
                            AppDirs::from_env().set_default_app(&request.content_type, &app)
                    {
                        log::warn!(
                            "portal: could not record the default application for '{}': {error}",
                            request.content_type
                        );
                    }
                    log::info!("portal: ChooseApplication for '{app_id}' -> {app}");
                    (0, HashMap::from([("choice".to_owned(), Value::from(app))]))
                }
                ChooseAppResponse::Cancelled => (1, HashMap::new()),
            }
        }
        Err(InvokeError::Cancelled) => (1, HashMap::new()),
        Ok(_) => {
            log::warn!("portal: AppChooser prompter returned the wrong response kind");
            (2, HashMap::new())
        }
        Err(InvokeError::Failed(error)) => {
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
    fn the_preferred_candidate_leads() {
        let fixture = Fixture::new("preferred");
        let apps = resolve_candidates(&fixture.dirs, &[], "text/plain");
        let ordered = order_candidates(apps, Some("viewer.desktop"));
        assert_eq!(ordered[0].id, "viewer.desktop");
        // An unknown preference changes nothing.
        let apps = resolve_candidates(&fixture.dirs, &[], "text/plain");
        let before: Vec<_> = apps.iter().map(|app| app.id.clone()).collect();
        let after: Vec<_> = order_candidates(apps, Some("missing.desktop"))
            .iter()
            .map(|app| app.id.clone())
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn wire_choices_parse_and_the_remember_checkbox_is_added() {
        let choices = prompter_choices(&options(&[(
            "choices",
            Value::from(vec![(
                "encoding".to_owned(),
                "Encoding".to_owned(),
                vec![("utf8".to_owned(), "UTF-8".to_owned())],
                "utf8".to_owned(),
            )]),
        )]));
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].id, "encoding");
        assert_eq!(choices[1].id, "remember");
        assert!(choices[1].options.is_empty());

        // A frontend-supplied remember choice is not duplicated.
        let choices = prompter_choices(&options(&[(
            "choices",
            Value::from(vec![(
                "always".to_owned(),
                "Always use this app".to_owned(),
                Vec::<(String, String)>::new(),
                "true".to_owned(),
            )]),
        )]));
        assert_eq!(choices.len(), 1);

        // A wrongly-shaped value is ignored, not fatal.
        let choices = prompter_choices(&options(&[("choices", Value::from(42u32))]));
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].id, "remember");
    }

    #[test]
    fn remember_detection_matches_the_documented_ids() {
        assert!(remembers(&[("remember".to_owned(), "true".to_owned())]));
        assert!(remembers(&[("always".to_owned(), "true".to_owned())]));
        assert!(!remembers(&[("remember".to_owned(), "false".to_owned())]));
        assert!(!remembers(&[("encoding".to_owned(), "true".to_owned())]));
    }
}
