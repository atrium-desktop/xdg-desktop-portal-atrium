//! `org.freedesktop.impl.portal.DynamicLauncher` v1: the launcher-install
//! confirmation dialog.
//!
//! Scope note, verified against the interface XML
//! (`/usr/share/dbus-1/interfaces/org.freedesktop.impl.portal.DynamicLauncher.xml`)
//! and the published docs: the *backend* interface has exactly two methods,
//! `PrepareInstall` and `RequestInstallToken`. The `Install`, `Uninstall`,
//! `GetIcon`, and `Launch` methods live on the frontend-facing
//! `org.freedesktop.portal.DynamicLauncher` interface and are implemented
//! by xdg-desktop-portal itself — the .desktop file writing happens there,
//! not here. This module therefore owns only the consent dialog and never
//! touches the launchers directory.
//!
//! `PrepareInstall` presents the Portal-owned launcher editor (name field,
//! web-app URL and icon note when relevant) via the one-shot prompter.
//! Saved answers 0 with `{"name": s, "icon": v}` — the icon variant is
//! echoed verbatim because icon editing is not implemented (`editable_icon`
//! is accepted and ignored with a debug log, which the spec permits:
//! "if the implementation supports this"). Cancellation, dismissal, or a
//! racing `Request.Close` answers 1; failures answer 2.
//!
//! `RequestInstallToken` always answers 1: non-interactive installation is
//! never permitted, so every install passes the dialog. `version` is 1 and
//! `SupportedLauncherTypes` is 3 (Application | Webapp); a webapp request
//! displays its `target` URL in the dialog.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};

use atrium_portal_prompter::{
    LauncherEditRequest, LauncherEditResponse, PromptResult, PrompterRequest,
};
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use crate::prompter::{self, InvokeError};
use atrium_portal_runtime::{PortalResponse, RequestTracker, ResponseSender, sync};

/// Bounds for the text the frontend supplies.
const MAX_NAME_BYTES: usize = 1024;
const MAX_TARGET_BYTES: usize = 4 * 1024;

/// One prepare-install request handed from the bus method to the worker.
pub(crate) enum DynamicLauncherJob {
    Prepare {
        request_path: String,
        app_id: String,
        request: LauncherEditRequest,
        /// The proposed icon, echoed verbatim into the results on save.
        icon: OwnedValue,
        reply: ResponseSender,
    },
}

/// The served dynamic-launcher interface. The dialog-bearing method only
/// validates, registers the request object, and enqueues; the prompt runs
/// on the worker.
pub(crate) struct DynamicLauncherIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<DynamicLauncherJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.DynamicLauncher")]
impl DynamicLauncherIface {
    async fn prepare_install(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        name: &str,
        icon_v: Value<'_>,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let path = handle.as_str().to_string();
        log::info!("portal: PrepareInstall for '{app_id}' at {path}");

        let prepared = match prepare_request(app_id, parent_window, name, &icon_v, &options) {
            Ok(prepared) => prepared,
            Err(error) => {
                log::warn!("portal: refusing PrepareInstall request: {error}");
                return Ok((2, HashMap::new()));
            }
        };
        let (request, icon) = prepared;

        atrium_portal_runtime::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "prepare",
            &self.jobs,
            |reply| DynamicLauncherJob::Prepare {
                request_path: path.clone(),
                app_id: app_id.to_string(),
                request,
                icon,
                reply,
            },
        )
        .await
    }

    /// Non-interactive installation is never permitted: every install goes
    /// through the confirmation dialog, so no install tokens are issued.
    async fn request_install_token(
        &self,
        app_id: &str,
        _options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<u32> {
        log::info!("portal: RequestInstallToken for '{app_id}' refused (dialog is mandatory)");
        Ok(1)
    }

    /// Application (1) and Webapp (2); the dialog displays a webapp's
    /// target URL.
    #[zbus(property, name = "SupportedLauncherTypes")]
    fn supported_launcher_types(&self) -> u32 {
        3
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

/// Validate the request and translate it into the prompter's editor
/// request plus the icon to echo back.
fn prepare_request(
    app_id: &str,
    parent_window: &str,
    name: &str,
    icon_v: &Value<'_>,
    options: &HashMap<String, Value<'_>>,
) -> Result<(LauncherEditRequest, OwnedValue), String> {
    if name.len() > MAX_NAME_BYTES || name.contains('\0') {
        return Err("launcher name is oversized or contains NUL".to_string());
    }
    let get_bool = |key: &str, default| {
        options
            .get(key)
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(default)
    };
    let launcher_type = options
        .get("launcher_type")
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(1);
    if !matches!(launcher_type, 1 | 2) {
        return Err(format!("unsupported launcher type {launcher_type}"));
    }
    let target = options
        .get("target")
        .and_then(|value| String::try_from(value).ok());
    if launcher_type == 2
        && let Some(target) = &target
        && (target.is_empty() || target.len() > MAX_TARGET_BYTES || target.contains('\0'))
    {
        return Err("webapp target is empty, oversized, or contains NUL".to_string());
    }
    if get_bool("editable_icon", false) {
        log::debug!("portal: ignoring 'editable_icon'; icon editing is not supported");
    }
    let icon = icon_v
        .try_to_owned()
        .map_err(|error| format!("could not own the icon variant: {error}"))?;

    let request = LauncherEditRequest {
        app_id: app_id.to_owned(),
        title: "Install Launcher".to_owned(),
        name: name.to_owned(),
        editable_name: get_bool("editable_name", true),
        target: if launcher_type == 2 { target } else { None },
        icon_label: icon_label(icon_v),
        modal: get_bool("modal", true),
        parent_window: (!parent_window.is_empty()).then(|| parent_window.to_owned()),
    };
    request.validate()?;
    Ok((request, icon))
}

/// A short human-readable label for the serialized GIcon: the first themed
/// name for `("themed", as)`, a generic note for `("bytes", ay)` and
/// `("file", s)`. Unknown shapes yield no label; the icon itself is echoed
/// back in the results regardless.
fn icon_label(icon_v: &Value<'_>) -> Option<String> {
    let (kind, data) = match icon_v {
        Value::Structure(structure) => {
            let [kind, data] = structure.fields() else {
                return None;
            };
            (String::try_from(kind).ok()?, data.try_clone().ok()?)
        }
        _ => return None,
    };
    // The variant payload may still be wrapped in nested variant layers.
    let mut data = data;
    while let Value::Value(inner) = data {
        data = *inner;
    }
    match kind.as_str() {
        "themed" => Vec::<String>::try_from(data)
            .ok()
            .and_then(|names| names.into_iter().next()),
        "bytes" => Some("a custom image".to_owned()),
        "file" => Some("a custom image file".to_owned()),
        _ => None,
    }
}

/// Dispatch editor dialogs independently so one application leaving a
/// dialog open cannot head-of-line block every other PrepareInstall.
pub(crate) fn dynamic_launcher_worker(
    rx: mpsc::Receiver<DynamicLauncherJob>,
    tracker: Arc<Mutex<RequestTracker>>,
    settings: crate::settings::SettingsStore,
) {
    const MAX_ACTIVE_PREPARE_REQUESTS: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(DynamicLauncherJob::Prepare {
        request_path,
        app_id,
        request,
        icon,
        reply,
    }) = rx.recv()
    {
        if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_ACTIVE_PREPARE_REQUESTS {
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            log::warn!("portal: refusing PrepareInstall request: concurrency limit reached");
            let _ = reply.send_blocking((2, HashMap::new()));
            continue;
        }
        let task_tracker = Arc::clone(&tracker);
        let task_settings = settings.clone();
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("atrium-portal-dynamic-launcher-task".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let result = run_prepare(
                    &task_tracker,
                    &request_path,
                    &app_id,
                    request,
                    icon,
                    Some(&task_settings),
                );
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn PrepareInstall task: {error}");
            let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
        }
    }
}

/// Execute one request: show the editor, then relay the reviewed name and
/// the echoed icon.
fn run_prepare(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    app_id: &str,
    request: LauncherEditRequest,
    icon: OwnedValue,
    settings: Option<&crate::settings::SettingsStore>,
) -> (u32, HashMap<String, Value<'static>>) {
    if sync::lock(tracker, "dynamic launcher tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }

    let cancelled = || sync::lock(tracker, "dynamic launcher tracker").was_closed(request_path);
    let answered = prompter::invoke(
        PrompterRequest::launcher_edit(request.clone()),
        settings,
        Some(&cancelled),
    );
    match answered {
        Ok(PromptResult::LauncherEdit(response)) => {
            // Request.Close wins a race with a completed child response.
            if sync::lock(tracker, "dynamic launcher tracker").was_closed(request_path) {
                return (1, HashMap::new());
            }
            if let Err(error) = response.validate_for(&request) {
                log::warn!("portal: invalid PrepareInstall response for '{app_id}': {error}");
                return (2, HashMap::new());
            }
            match response {
                LauncherEditResponse::Saved { name } => {
                    log::info!("portal: PrepareInstall for '{app_id}' confirmed as '{name}'");
                    let results = HashMap::from([
                        ("name".to_owned(), Value::from(name)),
                        ("icon".to_owned(), Value::from(icon)),
                    ]);
                    (0, results)
                }
                LauncherEditResponse::Cancelled => (1, HashMap::new()),
            }
        }
        Err(InvokeError::Cancelled) => (1, HashMap::new()),
        Ok(_) => {
            log::warn!("portal: DynamicLauncher prompter returned the wrong response kind");
            (2, HashMap::new())
        }
        Err(InvokeError::Failed(error)) => {
            log::warn!("portal: PrepareInstall for '{app_id}' failed: {error}");
            (2, HashMap::new())
        }
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

    fn icon_bytes() -> Value<'static> {
        Value::from(("bytes", vec![0x89u8, 0x50, 0x4e, 0x47]))
    }

    #[test]
    fn name_and_type_limits_are_bounded() {
        assert!(prepare_request("app", "", "", &icon_bytes(), &HashMap::new()).is_ok());
        let oversized = "x".repeat(MAX_NAME_BYTES + 1);
        assert!(prepare_request("app", "", &oversized, &icon_bytes(), &HashMap::new()).is_err());
        assert!(prepare_request("app", "", "a\0b", &icon_bytes(), &HashMap::new()).is_err());

        let unknown_type = options(&[("launcher_type", Value::from(7u32))]);
        assert!(prepare_request("app", "", "Name", &icon_bytes(), &unknown_type).is_err());
    }

    #[test]
    fn options_default_and_webapp_target_is_carried() {
        let (request, _) =
            prepare_request("app", "", "Name", &icon_bytes(), &HashMap::new()).unwrap();
        assert!(request.modal);
        assert!(request.editable_name);
        assert!(request.target.is_none());

        // A webapp request shows its target URL.
        let webapp = options(&[
            ("launcher_type", Value::from(2u32)),
            ("target", Value::from("https://app.example")),
            ("modal", Value::from(false)),
            ("editable_name", Value::from(false)),
        ]);
        let (request, _) = prepare_request("app", "", "Name", &icon_bytes(), &webapp).unwrap();
        assert_eq!(request.target.as_deref(), Some("https://app.example"));
        assert!(!request.modal);
        assert!(!request.editable_name);

        // A target is ignored (and not validated away) for applications.
        let with_target = options(&[("target", Value::from("https://ignored.example"))]);
        let (request, _) = prepare_request("app", "", "Name", &icon_bytes(), &with_target).unwrap();
        assert!(request.target.is_none());

        // An empty editable name passes validation; a non-editable one does
        // not (the contract enforces it).
        let not_editable = options(&[("editable_name", Value::from(false))]);
        assert!(prepare_request("app", "", "", &icon_bytes(), &HashMap::new()).is_ok());
        assert!(prepare_request("app", "", "", &icon_bytes(), &not_editable).is_err());
    }

    #[test]
    fn icon_labels_decode_the_common_gicon_shapes() {
        assert_eq!(icon_label(&icon_bytes()).as_deref(), Some("a custom image"));
        let themed = Value::from(("themed", vec!["cool-app".to_owned(), "cool".to_owned()]));
        assert_eq!(icon_label(&themed).as_deref(), Some("cool-app"));
        // The wire form nests the payload in variant layers.
        let nested = Value::from((
            "themed",
            Value::Value(Box::new(Value::from(vec!["nested-app".to_owned()]))),
        ));
        assert_eq!(icon_label(&nested).as_deref(), Some("nested-app"));
        assert!(icon_label(&Value::from(42u32)).is_none());
        assert!(icon_label(&Value::from(("emblem", 1u32))).is_none());
    }

    #[test]
    fn the_icon_is_owned_for_later_echo() {
        let (_, icon) = prepare_request("app", "", "Name", &icon_bytes(), &HashMap::new()).unwrap();
        let echoed: Value<'static> = Value::from(icon);
        let (kind, bytes) = <(String, Vec<u8>)>::try_from(echoed).unwrap();
        assert_eq!(kind, "bytes");
        assert_eq!(bytes, [0x89, 0x50, 0x4e, 0x47]);
    }
}
