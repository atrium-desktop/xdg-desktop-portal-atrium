//! `org.freedesktop.impl.portal.Background` v1: consent for background
//! activity and login autostart.
//!
//! Every request is answered by the Portal-owned, one-shot confirmation
//! dialog (the same prompter surface Access uses): the body names the
//! application id, quotes the supplied reason, and notes an autostart
//! request. Only the allow button grants (`background: true`); denial,
//! dismissal, or a racing `Request.Close` answers 1, and prompter failures
//! answer 2. There is deliberately no permission-store persistence — the
//! prompt appears on every request, so an application cannot silently
//! re-acquire background rights after the user has lost track of the
//! grant.
//!
//! Autostart is granted only when the request both sets the `autostart`
//! option and supplies a non-empty `commandline`: granting writes
//! `$XDG_CONFIG_HOME/autostart/<app_id>.desktop` (atomic temp+rename,
//! 0644, overwriting any previous entry so re-grants are idempotent) and
//! reports `autostart: true`. A write failure degrades to
//! `autostart: false` with a warn log — the background grant itself still
//! stands. Requests without the option (or without a command) report
//! `autostart: false`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use atrium_portal_prompter::{ConfirmRequest, ConfirmResponse, PromptResult, PrompterRequest};
use zbus::zvariant::{ObjectPath, Value};

use crate::prompter::{self, InvokeError};
use atrium_portal_runtime::{PortalResponse, RequestTracker, ResponseSender, sync};

/// The app id becomes a filename, so it is bounded and separator-free.
const MAX_APP_ID_BYTES: usize = 255;
/// The supplied reason; the composed dialog body is further bounded by the
/// prompter contract's 16 KiB text cap.
const MAX_REASON_BYTES: usize = 16 * 1024;
const MAX_COMMANDLINE_ARGS: usize = 64;
const MAX_ARG_BYTES: usize = 4 * 1024;
/// The prompter contract's per-text cap; the composed body must fit.
const MAX_BODY_BYTES: usize = 16 * 1024;

/// The autostart half of a request, present only when the application
/// asked for autostart *and* supplied a command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutostartSpec {
    commandline: Vec<String>,
    dbus_activatable: bool,
}

/// One background request handed from the bus method to the worker.
pub(crate) enum BackgroundJob {
    Request {
        request_path: String,
        app_id: String,
        prompt: ConfirmRequest,
        autostart: Option<AutostartSpec>,
        reply: ResponseSender,
    },
}

/// The served background interface. The method only validates, registers
/// the request object, and enqueues; the consent dialog and the autostart
/// write happen on the worker.
pub(crate) struct BackgroundIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<BackgroundJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Background")]
impl BackgroundIface {
    async fn request_background(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        reason: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let path = handle.as_str().to_string();
        log::info!("portal: RequestBackground for '{app_id}' at {path}");

        let (prompt, autostart) = match parse_request(app_id, parent_window, reason, &options) {
            Ok(parsed) => parsed,
            Err(error) => {
                log::warn!("portal: refusing Background request: {error}");
                return Ok((2, HashMap::new()));
            }
        };

        atrium_portal_runtime::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "request",
            &self.jobs,
            |reply| BackgroundJob::Request {
                request_path: path.clone(),
                app_id: app_id.to_string(),
                prompt,
                autostart,
                reply,
            },
        )
        .await
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

/// Validate the request and translate it into the prompter's confirmation
/// plus the autostart spec (when the application actually asked for one).
fn parse_request(
    app_id: &str,
    parent_window: &str,
    reason: &str,
    options: &HashMap<String, Value<'_>>,
) -> Result<(ConfirmRequest, Option<AutostartSpec>), String> {
    if app_id.is_empty()
        || app_id.len() > MAX_APP_ID_BYTES
        || app_id
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
    {
        return Err("app id is empty, oversized, or contains separators/NUL".to_string());
    }
    if reason.len() > MAX_REASON_BYTES || reason.contains('\0') {
        return Err("reason is oversized or contains NUL".to_string());
    }
    let get_bool = |key: &str| {
        options
            .get(key)
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(false)
    };
    let wants_autostart = get_bool("autostart");
    let dbus_activatable = get_bool("dbus-activatable");
    let commandline: Vec<String> = options
        .get("commandline")
        .and_then(|value| Vec::<String>::try_from(value.try_clone().ok()?).ok())
        .unwrap_or_default();
    if commandline.len() > MAX_COMMANDLINE_ARGS
        || commandline
            .iter()
            .any(|arg| arg.is_empty() || arg.len() > MAX_ARG_BYTES || arg.contains('\0'))
    {
        return Err("commandline is empty-arg, oversized, or contains NUL".to_string());
    }
    // Autostart without a command is meaningless; grant background only.
    let autostart = (wants_autostart && !commandline.is_empty()).then_some(AutostartSpec {
        commandline,
        dbus_activatable,
    });

    let prompt = ConfirmRequest {
        title: "Run in Background".to_owned(),
        body: consent_body(app_id, reason, wants_autostart),
        accept_label: Some("_Allow".to_owned()),
        deny_label: None,
        modal: true,
        parent_window: (!parent_window.is_empty()).then(|| parent_window.to_owned()),
    };
    prompt
        .validate()
        .map_err(|error| format!("composed prompt is invalid: {error}"))?;
    Ok((prompt, autostart))
}

/// The dialog body: the application id, the verbatim reason, and the
/// autostart note, truncated on a char boundary to fit the prompter
/// contract's text cap.
fn consent_body(app_id: &str, reason: &str, autostart: bool) -> String {
    let mut body = format!("The application '{app_id}' wants to run in the background.");
    if !reason.trim().is_empty() {
        body.push_str("\n\n");
        body.push_str(reason);
    }
    if autostart {
        body.push_str("\n\nThe application also wants to start automatically when you log in.");
    }
    if body.len() > MAX_BODY_BYTES {
        // Leave room for the ellipsis (three UTF-8 bytes).
        let mut end = MAX_BODY_BYTES - 3;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push('…');
    }
    body
}

/// The autostart entry's path under the given config root.
fn autostart_entry_path(config_dir: &Path, app_id: &str) -> PathBuf {
    config_dir
        .join("autostart")
        .join(format!("{app_id}.desktop"))
}

/// Render one argument for the `Exec=` line. Arguments made entirely of
/// unreserved characters pass through; anything else is double-quoted with
/// the spec's reserved characters (`"`, `` ` ``, `$`, `\`)
/// backslash-escaped. Literal percent signs are always doubled so the
/// Exec parser never sees a field code.
fn quote_exec_arg(arg: &str) -> String {
    let safe = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '-' | '_' | '.' | '/' | ':' | '@' | '%' | '=' | ',' | '~' | '+'
            )
    };
    let rendered = if !arg.is_empty() && arg.chars().all(safe) {
        arg.to_owned()
    } else {
        let mut quoted = String::with_capacity(arg.len() + 2);
        quoted.push('"');
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('"');
        quoted
    };
    rendered.replace('%', "%%")
}

/// The autostart desktop entry's content.
fn render_desktop_entry(app_id: &str, spec: &AutostartSpec) -> String {
    let exec = spec
        .commandline
        .iter()
        .map(|arg| quote_exec_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let mut entry = format!("[Desktop Entry]\nType=Application\nName={app_id}\nExec={exec}\n");
    if spec.dbus_activatable {
        entry.push_str("DBusActivatable=true\n");
    }
    // Marker so tooling can tell portal-granted entries from hand-made ones.
    entry.push_str("X-Tessera-Portal=background\n");
    entry
}

/// Write the autostart entry under `config_dir`, atomically (temp file plus
/// rename) with 0644 permissions, overwriting any previous grant.
fn write_autostart_entry(
    config_dir: &Path,
    app_id: &str,
    spec: &AutostartSpec,
) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = autostart_entry_path(config_dir, app_id);
    let directory = path.parent().expect("autostart path has a parent");
    std::fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(".{app_id}.desktop.atrium-{}", std::process::id()));
    std::fs::write(&temporary, render_desktop_entry(app_id, spec))?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&temporary, &path)?;
    Ok(path)
}

/// The results dictionary for a grant.
fn results(autostart: bool) -> HashMap<String, Value<'static>> {
    HashMap::from([
        ("background".to_owned(), Value::from(true)),
        ("autostart".to_owned(), Value::from(autostart)),
    ])
}

/// Dispatch consent prompts independently so one application leaving a
/// prompt open cannot head-of-line block every other Background request.
pub(crate) fn background_worker(
    rx: mpsc::Receiver<BackgroundJob>,
    tracker: Arc<Mutex<RequestTracker>>,
    settings: crate::settings::SettingsStore,
) {
    const MAX_ACTIVE_BACKGROUND_REQUESTS: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(BackgroundJob::Request {
        request_path,
        app_id,
        prompt,
        autostart,
        reply,
    }) = rx.recv()
    {
        if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            >= MAX_ACTIVE_BACKGROUND_REQUESTS
        {
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            log::warn!("portal: refusing Background request: concurrency limit reached");
            let _ = reply.send_blocking((2, HashMap::new()));
            continue;
        }
        let task_tracker = Arc::clone(&tracker);
        let task_settings = settings.clone();
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("atrium-portal-background-task".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let result = run_request(
                    &task_tracker,
                    &request_path,
                    &app_id,
                    prompt,
                    autostart,
                    Some(&task_settings),
                );
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn Background task: {error}");
            let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
        }
    }
}

/// Execute one request: ask for consent, then honour the answer.
fn run_request(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    app_id: &str,
    prompt: ConfirmRequest,
    autostart: Option<AutostartSpec>,
    settings: Option<&crate::settings::SettingsStore>,
) -> (u32, HashMap<String, Value<'static>>) {
    if sync::lock(tracker, "background tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }

    let cancelled = || sync::lock(tracker, "background tracker").was_closed(request_path);
    let confirmed = prompter::invoke(PrompterRequest::confirm(prompt), settings, Some(&cancelled));
    match confirmed {
        Ok(PromptResult::Confirm(ConfirmResponse::Confirmed)) => {
            // Request.Close wins a race with a completed child response.
            if sync::lock(tracker, "background tracker").was_closed(request_path) {
                return (1, HashMap::new());
            }
            let autostart_granted = match autostart {
                Some(spec) => write_autostart(app_id, &spec),
                None => false,
            };
            log::info!(
                "portal: RequestBackground for '{app_id}' granted (autostart: {autostart_granted})"
            );
            (0, results(autostart_granted))
        }
        Ok(PromptResult::Confirm(ConfirmResponse::Cancelled)) | Err(InvokeError::Cancelled) => {
            (1, HashMap::new())
        }
        Ok(_) => {
            log::warn!("portal: Background prompter returned the wrong response kind");
            (2, HashMap::new())
        }
        Err(InvokeError::Failed(error)) => {
            log::warn!("portal: RequestBackground for '{app_id}' failed: {error}");
            (2, HashMap::new())
        }
    }
}

/// Attempt the autostart write; a failure degrades to `autostart: false`
/// while the background grant stands.
fn write_autostart(app_id: &str, spec: &AutostartSpec) -> bool {
    let Some(config_dir) = dirs::config_dir() else {
        log::warn!("portal: no XDG config directory; cannot write the autostart entry");
        return false;
    };
    match write_autostart_entry(&config_dir, app_id, spec) {
        Ok(path) => {
            log::info!("portal: wrote autostart entry {}", path.display());
            true
        }
        Err(error) => {
            log::warn!("portal: could not write the autostart entry for '{app_id}': {error}");
            false
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

    #[test]
    fn app_id_and_reason_limits_are_bounded() {
        assert!(parse_request("", "", "", &HashMap::new()).is_err());
        assert!(parse_request("org.foo.Bar", "", "", &HashMap::new()).is_ok());
        assert!(parse_request("org/foo", "", "", &HashMap::new()).is_err());
        assert!(parse_request("org.foo\nBar", "", "", &HashMap::new()).is_err());
        assert!(parse_request(&"a".repeat(256), "", "", &HashMap::new()).is_err());
        assert!(
            parse_request(
                "org.foo.Bar",
                "",
                &"x".repeat(MAX_REASON_BYTES + 1),
                &HashMap::new()
            )
            .is_err()
        );
    }

    #[test]
    fn commandline_limits_and_autostart_semantics() {
        // Autostart without a commandline grants background only.
        let (_, autostart) = parse_request(
            "org.foo.Bar",
            "",
            "",
            &options(&[("autostart", Value::from(true))]),
        )
        .unwrap();
        assert!(autostart.is_none());

        let (_, autostart) = parse_request(
            "org.foo.Bar",
            "",
            "",
            &options(&[
                ("autostart", Value::from(true)),
                ("dbus-activatable", Value::from(true)),
                (
                    "commandline",
                    Value::from(vec!["foo".to_owned(), "--flag".to_owned()]),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(
            autostart,
            Some(AutostartSpec {
                commandline: vec!["foo".to_owned(), "--flag".to_owned()],
                dbus_activatable: true,
            })
        );

        let oversized = options(&[(
            "commandline",
            Value::from(vec!["x".repeat(MAX_ARG_BYTES + 1)]),
        )]);
        assert!(parse_request("org.foo.Bar", "", "", &oversized).is_err());
        let empty_arg = options(&[("commandline", Value::from(vec![String::new()]))]);
        assert!(parse_request("org.foo.Bar", "", "", &empty_arg).is_err());
        let too_many = options(&[(
            "commandline",
            Value::from(vec!["x".to_owned(); MAX_COMMANDLINE_ARGS + 1]),
        )]);
        assert!(parse_request("org.foo.Bar", "", "", &too_many).is_err());
    }

    #[test]
    fn consent_body_names_the_app_reason_and_autostart() {
        let body = consent_body("org.foo.Bar", "To keep music playing.", true);
        assert!(body.contains("'org.foo.Bar'"));
        assert!(body.contains("To keep music playing."));
        assert!(body.contains("start automatically"));
        let body = consent_body("org.foo.Bar", "", false);
        assert!(!body.contains("start automatically"));
        // The composed body always fits the prompter contract's text cap.
        let body = consent_body("org.foo.Bar", &"x".repeat(MAX_REASON_BYTES), false);
        assert!(body.len() <= MAX_BODY_BYTES);
    }

    #[test]
    fn exec_quoting_follows_the_desktop_entry_spec() {
        assert_eq!(quote_exec_arg("foo"), "foo");
        assert_eq!(quote_exec_arg("--flag=value"), "--flag=value");
        assert_eq!(quote_exec_arg("two words"), "\"two words\"");
        assert_eq!(quote_exec_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote_exec_arg(""), "\"\"");
        // Literal percents double so the Exec parser sees no field codes.
        assert_eq!(quote_exec_arg("100%"), "100%%");
        assert_eq!(quote_exec_arg("a %u b"), "\"a %%u b\"");
    }

    #[test]
    fn desktop_entry_rendering_is_complete_and_conditional() {
        let spec = AutostartSpec {
            commandline: vec!["foo".to_owned(), "two words".to_owned()],
            dbus_activatable: true,
        };
        let entry = render_desktop_entry("org.foo.Bar", &spec);
        assert!(entry.starts_with("[Desktop Entry]\nType=Application\n"));
        assert!(entry.contains("Name=org.foo.Bar\n"));
        assert!(entry.contains("Exec=foo \"two words\"\n"));
        assert!(entry.contains("DBusActivatable=true\n"));
        assert!(entry.contains("X-Tessera-Portal=background\n"));

        let spec = AutostartSpec {
            commandline: vec!["foo".to_owned()],
            dbus_activatable: false,
        };
        assert!(!render_desktop_entry("org.foo.Bar", &spec).contains("DBusActivatable"));
    }

    #[test]
    fn autostart_write_is_atomic_and_idempotent() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = std::env::temp_dir().join(format!(
            "tessera-background-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let spec = AutostartSpec {
            commandline: vec!["foo".to_owned(), "--bar".to_owned()],
            dbus_activatable: false,
        };
        let path = write_autostart_entry(&root, "org.foo.Bar", &spec).unwrap();
        assert_eq!(path, root.join("autostart/org.foo.Bar.desktop"));
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o644);
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.contains("Exec=foo --bar\n"));

        // Re-granting overwrites in place.
        let second_spec = AutostartSpec {
            commandline: vec!["foo".to_owned(), "--baz".to_owned()],
            dbus_activatable: true,
        };
        write_autostart_entry(&root, "org.foo.Bar", &second_spec).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert!(second.contains("Exec=foo --baz\n"));
        assert!(second.contains("DBusActivatable=true\n"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn grant_results_report_both_flags() {
        let granted = results(true);
        assert_eq!(granted["background"], Value::from(true));
        assert_eq!(granted["autostart"], Value::from(true));
        let background_only = results(false);
        assert_eq!(background_only["autostart"], Value::from(false));
    }
}
