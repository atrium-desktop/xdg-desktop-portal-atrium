//! `org.freedesktop.impl.portal.Account` v1: user identity sharing.
//!
//! `GetUserInformation` never answers silently: the request parks on a
//! worker while a Portal-owned, one-shot confirmation dialog asks the
//! user whether to share their name and avatar
//! with the calling application. Only an affirmative answer releases the
//! identity: the account name and GECOS real name from `getpwuid`, plus the
//! first existing avatar from the canonical candidates
//! (`$XDG_DATA_HOME/tessera/avatars/face.*`, then the freedesktop `~/.face`
//! conventions — the same precedence `tessera-avatar` resolves).
//!
//! Response codes follow the portal specification: 0 shared, 1 declined
//! (or `Request.Close` raced in), 2 other error.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};

use atrium_portal_prompter::{ConfirmRequest, ConfirmResponse, PromptResult, PrompterRequest};
use zbus::zvariant::{ObjectPath, Value};

use crate::files;
use crate::prompter::{self, InvokeError};
use atrium_portal_runtime::{PortalResponse, RequestTracker, ResponseSender, sync};

const MAX_REASON_BYTES: usize = 16 * 1024;

/// One account request handed from the bus method to the worker.
pub(crate) enum AccountJob {
    GetUserInformation {
        request_path: String,
        app_id: String,
        parent_window: Option<String>,
        reason: Option<String>,
        reply: ResponseSender,
    },
}

/// The served account interface. The method only registers the request
/// object and enqueues; the consent prompt happens on the worker.
pub(crate) struct AccountIface {
    /// Async handle onto the same connection; only used inside served
    /// methods, which already run on zbus's executor (screenshot precedent).
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<AccountJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Account")]
impl AccountIface {
    async fn get_user_information(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        window: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let path = handle.as_str().to_string();
        log::info!("portal: GetUserInformation for '{app_id}' at {path}");

        let reason = match account_reason(&options) {
            Ok(reason) => reason,
            Err(error) => {
                log::warn!("portal: refusing Account request: {error}");
                return Ok((2, HashMap::new()));
            }
        };

        atrium_portal_runtime::dispatch(
            &self.conn,
            &self.tracker,
            &path,
            "get_user_information",
            &self.jobs,
            |reply| AccountJob::GetUserInformation {
                request_path: path.clone(),
                app_id: app_id.to_string(),
                parent_window: (!window.is_empty()).then(|| window.to_owned()),
                reason,
                reply,
            },
        )
        .await
    }
}

fn account_reason(options: &HashMap<String, Value<'_>>) -> Result<Option<String>, String> {
    let reason = options
        .get("reason")
        .and_then(|value| String::try_from(value).ok());
    if reason
        .as_ref()
        .is_some_and(|reason| reason.len() > MAX_REASON_BYTES)
    {
        return Err(format!("reason exceeds the {MAX_REASON_BYTES}-byte limit"));
    }
    Ok(reason)
}

/// Dispatch consent prompts independently so one application leaving a
/// prompt open cannot head-of-line block every other Account request.
pub(crate) fn account_worker(
    rx: mpsc::Receiver<AccountJob>,
    tracker: Arc<Mutex<RequestTracker>>,
    settings: crate::settings::SettingsStore,
) {
    const MAX_ACTIVE_ACCOUNT_REQUESTS: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(AccountJob::GetUserInformation {
        request_path,
        app_id,
        parent_window,
        reason,
        reply,
    }) = rx.recv()
    {
        if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_ACTIVE_ACCOUNT_REQUESTS {
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            log::warn!("portal: refusing Account request: concurrency limit reached");
            let _ = reply.send_blocking((2, HashMap::new()));
            continue;
        }
        let task_tracker = Arc::clone(&tracker);
        let task_settings = settings.clone();
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("atrium-portal-account-task".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let result = run_request(
                    &task_tracker,
                    &request_path,
                    &app_id,
                    parent_window,
                    reason,
                    Some(&task_settings),
                );
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn Account task: {error}");
            let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
        }
    }
}

/// Execute one request: prompt for consent, then release the identity.
fn run_request(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    app_id: &str,
    parent_window: Option<String>,
    reason: Option<String>,
    settings: Option<&crate::settings::SettingsStore>,
) -> (u32, HashMap<String, Value<'static>>) {
    if sync::lock(tracker, "account tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }

    let mut body = format!(
        "The application '{app_id}' requests access to your personal information \
         (name and avatar photo)."
    );
    if let Some(reason) = reason {
        body.push(' ');
        body.push_str(&reason);
    }
    let cancelled = || sync::lock(tracker, "account tracker").was_closed(request_path);
    let confirmed = prompter::invoke(
        PrompterRequest::confirm(ConfirmRequest {
            title: "Share Personal Information".to_string(),
            body,
            accept_label: Some("_Share".to_string()),
            deny_label: None,
            modal: true,
            parent_window,
        }),
        settings,
        Some(&cancelled),
    );
    match confirmed {
        Ok(PromptResult::Confirm(ConfirmResponse::Confirmed)) => {}
        Ok(PromptResult::Confirm(ConfirmResponse::Cancelled)) | Err(InvokeError::Cancelled) => {
            return (1, HashMap::new());
        }
        Ok(_) => {
            log::warn!("portal: Account prompter returned the wrong response kind");
            return (2, HashMap::new());
        }
        Err(InvokeError::Failed(error)) => {
            log::warn!("portal: GetUserInformation consent for '{app_id}' failed: {error}");
            return (2, HashMap::new());
        }
    }
    if sync::lock(tracker, "account tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }

    let identity = Identity::current();
    log::info!("portal: GetUserInformation for '{app_id}' shared (consent given)");
    let mut results = HashMap::from([
        ("id".to_string(), Value::from(identity.id)),
        ("name".to_string(), Value::from(identity.name)),
    ]);
    if let Some(image) = identity.avatar_uri {
        results.insert("image".to_string(), Value::from(image));
    }
    (0, results)
}

/// The local user's account id, real name, and avatar URI.
struct Identity {
    id: String,
    name: String,
    avatar_uri: Option<String>,
}

impl Identity {
    fn current() -> Identity {
        let (id, name) = passwd_identity();
        Identity {
            id,
            name,
            avatar_uri: avatar_path().map(|path| files::file_uri(&path)),
        }
    }
}

/// `(account id, real name)` from `getpwuid`: the GECOS full name up to
/// the first comma, falling back to the account name.
fn passwd_identity() -> (String, String) {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let Some((id, gecos)) = passwd_fields(uid) else {
        let fallback = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        return (fallback.clone(), fallback);
    };
    let name = gecos
        .split(',')
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| id.clone());
    (id, name)
}

/// Thread-safe passwd lookup. The daemon has D-Bus, capture, chooser, and
/// PipeWire threads, so the process-global buffer returned by `getpwuid`
/// would be unsafe here.
fn passwd_fields(uid: libc::uid_t) -> Option<(String, String)> {
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut size = if suggested > 0 {
        usize::try_from(suggested).unwrap_or(16 * 1024)
    } else {
        16 * 1024
    }
    .clamp(1024, 1024 * 1024);

    loop {
        // SAFETY: `passwd` is a plain C output struct initialized before use.
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; size];
        // SAFETY: the output pointers and buffer are valid for this call;
        // strings are copied before `buffer` is dropped.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                &mut passwd,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && size < 1024 * 1024 {
            size = (size * 2).min(1024 * 1024);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let read = |field: *const std::ffi::c_char| {
            if field.is_null() {
                String::new()
            } else {
                // SAFETY: successful getpwuid_r fields point into its
                // NUL-terminated caller-owned buffer.
                unsafe { std::ffi::CStr::from_ptr(field) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        return Some((read(passwd.pw_name), read(passwd.pw_gecos)));
    }
}

/// The first existing avatar candidate, in `tessera-avatar`'s precedence:
/// the canonical Tessera data location, then the freedesktop `~/.face`
/// conventions.
fn avatar_path() -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(data) = dirs::data_dir() {
        let dir = data.join("tessera").join("avatars");
        for name in ["face.png", "face.jpg", "face.webp", "face"] {
            candidates.push(dir.join(name));
        }
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".face"));
        candidates.push(home.join(".face.icon"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_never_empty() {
        let (id, name) = passwd_identity();
        assert!(!id.is_empty());
        assert!(!name.is_empty());
    }

    #[test]
    fn account_reason_limit_is_bounded() {
        let accepted = HashMap::from([(
            "reason".to_owned(),
            Value::from("x".repeat(MAX_REASON_BYTES)),
        )]);
        assert!(account_reason(&accepted).is_ok());

        let refused = HashMap::from([(
            "reason".to_owned(),
            Value::from("x".repeat(MAX_REASON_BYTES + 1)),
        )]);
        assert!(account_reason(&refused).is_err());
    }
}
