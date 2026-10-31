//! `org.freedesktop.impl.portal.Access` v1: the generic consent dialog.
//!
//! `AccessDialog` is the portal's last-resort permission prompt: the
//! frontend supplies the exact title, subtitle, body, and button labels and
//! the compositor renders the choice (ADR-0026). The request parks on a
//! worker while `PickConfirm` presents the frontend's text verbatim in
//! compositor chrome. Only the grant button answers 0; dismissal or a racing
//! `Request.Close` answers 1, and a failure to reach the compositor answers 2.
//!
//! The compositor's consent surface is a two-text prompt with one affirmative
//! label. The frontend's `deny_label` and `modality` hints therefore have no
//! rendering home: dismiss is the deny path, and the compositor owns modality
//! for its own chrome. Both are accepted and logged rather than rejected, so
//! a frontend that sends them is not failed over cosmetics.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};

use zbus::{ObjectPath, Variant};

use crate::ipc::{self, Consent};
use atrium_portal_request::{PortalResponse, RequestTracker, ResponseSender, sync};

const MAX_DIALOG_TEXT_BYTES: usize = 16 * 1024;
const MAX_LABEL_BYTES: usize = 256;

/// One access request handed from the bus method to the worker.
pub(crate) enum AccessJob {
    Dialog {
        request_path: String,
        app_id: String,
        prompt: ipc::ConsentPrompt,
        reply: ResponseSender,
    },
}

/// The served access interface. The method only registers the request
/// object and enqueues; the consent prompt happens on the worker.
pub(crate) struct AccessIface {
    /// Admission: only the portal frontend may call a private backend
    /// (see `atrium_portal_request::caller`).
    pub(crate) callers: Arc<atrium_portal_request::CallerGuard>,
    /// Async handle onto the same connection; only used inside served
    /// methods, which already run on zbus's executor (account precedent).
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<AccessJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Access")]
impl AccessIface {
    #[allow(clippy::too_many_arguments)]
    async fn access_dialog(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        window: &str,
        title: &str,
        subtitle: &str,
        body: &str,
        options: HashMap<String, Variant<'_>>,

        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<PortalResponse> {
        // Admission first: `app_id` is caller-supplied and unverifiable, so
        // the only sound boundary is "is the sender the portal frontend?".
        // Nothing below may run for any other peer.
        self.callers
            .admit(header.sender().map(|sender| sender.as_str()))
            .map_err(zbus::fdo::Error::from)?;
        let path = handle.as_str().to_string();
        log::info!("portal: AccessDialog for '{app_id}' at {path}");

        let prompt = match dialog_request(window, title, subtitle, body, &options) {
            Ok(prompt) => prompt,
            Err(error) => {
                log::warn!("portal: refusing Access request: {error}");
                return Ok((2, HashMap::new()));
            }
        };

        atrium_portal_request::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "access",
            &self.jobs,
            |reply| AccessJob::Dialog {
                request_path: path.clone(),
                app_id: app_id.to_string(),
                prompt,
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

/// Translate the portal arguments into the prompter's confirmation request.
/// The subtitle leads the body so both survive the two-text dialog surface;
/// the `icon` hint is accepted but not rendered.
fn dialog_request(
    window: &str,
    title: &str,
    subtitle: &str,
    body: &str,
    options: &HashMap<String, Variant<'_>>,
) -> Result<ipc::ConsentPrompt, String> {
    for (name, text) in [("title", title), ("subtitle", subtitle), ("body", body)] {
        if text.len() > MAX_DIALOG_TEXT_BYTES || text.contains('\0') {
            return Err(format!("{name} is oversized or contains NUL"));
        }
    }
    if title.trim().is_empty() && subtitle.trim().is_empty() && body.trim().is_empty() {
        return Err("title, subtitle, and body are all empty".to_string());
    }

    let text = |name: &str, limit: usize| -> Result<Option<String>, String> {
        let value = options
            .get(name)
            .and_then(|value| String::try_from(value).ok());
        match value {
            Some(value) if value.len() > limit || value.contains('\0') => {
                Err(format!("{name} label is oversized or contains NUL"))
            }
            other => Ok(other),
        }
    };
    let deny_label = text("deny_label", MAX_LABEL_BYTES)?;
    let grant_label = text("grant_label", MAX_LABEL_BYTES)?;
    if deny_label.is_some() {
        log::debug!("portal: Access deny_label has no compositor consent surface");
    }
    // A non-empty parent window is the frontend naming the surface that asked;
    // the compositor parents its own chrome, so the value carries no routing.
    if !window.is_empty() {
        log::debug!("portal: Access parent window is compositor-owned");
    }

    let (title, body) = if title.trim().is_empty() {
        (subtitle.to_string(), body.to_string())
    } else if subtitle.trim().is_empty() {
        (title.to_string(), body.to_string())
    } else if body.trim().is_empty() {
        (title.to_string(), subtitle.to_string())
    } else {
        (title.to_string(), format!("{subtitle}\n\n{body}"))
    };

    Ok(ipc::ConsentPrompt {
        title,
        body,
        accept_label: grant_label,
    })
}

/// Dispatch consent prompts independently so one application leaving a
/// prompt open cannot head-of-line block every other Access request.
pub(crate) fn access_worker(rx: &mpsc::Receiver<AccessJob>, tracker: Arc<Mutex<RequestTracker>>) {
    const MAX_ACTIVE_ACCESS_REQUESTS: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(AccessJob::Dialog {
        request_path,
        app_id,
        prompt,
        reply,
    }) = rx.recv()
    {
        if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_ACTIVE_ACCESS_REQUESTS {
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            log::warn!("portal: refusing Access request: concurrency limit reached");
            let _ = reply.send_blocking((2, HashMap::new()));
            continue;
        }
        let task_tracker = Arc::clone(&tracker);
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("atrium-portal-access-task".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let result = run_dialog(&task_tracker, &request_path, &app_id, prompt);
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn Access task: {error}");
            let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
        }
    }
}

/// Execute one request: show the frontend's dialog, then relay the choice.
fn run_dialog(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    app_id: &str,
    prompt: ipc::ConsentPrompt,
) -> (u32, HashMap<String, Variant<'static>>) {
    if sync::lock(tracker, "access tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }

    let socket = match crate::ipc::compositor_socket() {
        Ok(socket) => socket,
        Err(error) => {
            log::warn!("portal: AccessDialog for '{app_id}' cannot locate the compositor: {error}");
            return (2, HashMap::new());
        }
    };
    let answered = ipc::request_consent(
        &socket,
        &prompt.title,
        &prompt.body,
        prompt.accept_label.as_deref(),
    );
    match answered {
        Ok(Consent::Granted) => {
            // Request.Close wins a race with a completed answer.
            if sync::lock(tracker, "access tracker").was_closed(request_path) {
                log::info!("portal: AccessDialog for '{app_id}' closed after the user accepted");
                return (1, HashMap::new());
            }
            log::info!("portal: AccessDialog for '{app_id}' granted");
            (0, HashMap::new())
        }
        Ok(Consent::Cancelled) => (1, HashMap::new()),
        Err(error) => {
            log::warn!("portal: AccessDialog for '{app_id}' failed: {error}");
            (2, HashMap::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_text_limits_are_bounded() {
        let oversized = "x".repeat(MAX_DIALOG_TEXT_BYTES + 1);
        assert!(dialog_request("", &oversized, "", "", &HashMap::new()).is_err());
        assert!(dialog_request("", "", "", "", &HashMap::new()).is_err());
        assert!(dialog_request("", "Title", "", "", &HashMap::new()).is_ok());
    }

    #[test]
    fn subtitle_and_body_are_joined() {
        let prompt =
            dialog_request("wayland:parent", "Title", "Sub", "Body", &HashMap::new()).unwrap();
        assert_eq!(prompt.title, "Title");
        assert_eq!(prompt.body, "Sub\n\nBody");

        let prompt = dialog_request("", "", "Sub", "", &HashMap::new()).unwrap();
        assert_eq!(prompt.title, "Sub");
    }

    #[test]
    fn the_grant_label_comes_from_options() {
        // The compositor's consent surface has one affirmative label; a
        // `deny_label` or `modality` hint is accepted but has no rendering
        // home (ADR-0026), so only the grant label reaches the prompt.
        let options = HashMap::from([
            ("deny_label".to_owned(), Variant::from("_Deny")),
            ("grant_label".to_owned(), Variant::from("_Allow")),
            ("modality".to_owned(), Variant::from(false)),
        ]);
        let prompt = dialog_request("", "Title", "", "", &options).unwrap();
        assert_eq!(prompt.accept_label.as_deref(), Some("_Allow"));

        let oversized = HashMap::from([(
            "grant_label".to_owned(),
            Variant::from("x".repeat(MAX_LABEL_BYTES + 1)),
        )]);
        assert!(dialog_request("", "Title", "", "", &oversized).is_err());
    }
}
