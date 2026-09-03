//! `org.freedesktop.impl.portal.Email`: compose-email requests.
//!
//! The portal frontend resolves public-API attachment fds into local
//! `file://` URIs before calling this backend. Stable frontend releases also
//! pass absolute local paths for host callers when the document portal is
//! unavailable, so both safe local representations are accepted.
//! `ComposeEmail` validates and decodes them, then hands the message to the session's preferred
//! mail client through `xdg-email`
//! (`--cc`/`--bcc`/`--subject`/`--body`/`--attach`; the recipient list goes
//! as a `mailto:` URI). The hand-off is fire-and-forget: the mail client
//! owns the compose window from there. `ATRIUM_PORTAL_MAILER` overrides the
//! mailer command (tests, sessions without xdg-utils).
//!
//! The request is not interactive on our side, so no worker thread: the
//! `Request` object is exported for spec shape, the mailer is spawned, and
//! the method answers immediately. Response codes follow the portal
//! specification: 0 handed off, 1 cancelled (`Request.Close` raced in),
//! 2 other error.

use std::collections::HashMap;
use std::process::Child;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::zvariant::{ObjectPath, Value};

use crate::files;
use atrium_portal_runtime::{PortalResponse, RequestTracker, sync};

const MAX_ACTIVE_MAILERS: usize = 32;
const MAX_RECIPIENTS: usize = 512;
const MAX_ATTACHMENTS: usize = 128;
const MAX_MAILER_ARGUMENT_BYTES: usize = 256 * 1024;
static ACTIVE_MAILERS: AtomicUsize = AtomicUsize::new(0);

/// The served email interface.
pub(crate) struct EmailIface {
    /// Async handle onto the same connection; only used inside served
    /// methods, which already run on zbus's executor (screenshot precedent).
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Email")]
impl EmailIface {
    async fn compose_email(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let path = handle.as_str().to_string();
        log::info!("portal: ComposeEmail for '{app_id}' at {path}");

        atrium_portal_runtime::register(&self.conn, &self.tracker, &path).await?;
        let response = compose(app_id, &path, &options, &self.tracker);
        atrium_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
        response
    }
}

/// Build the mailer invocation and spawn it. Split from the zbus method so
/// the whole flow is testable without a bus.
fn compose(
    app_id: &str,
    request_path: &str,
    options: &HashMap<String, Value<'_>>,
    tracker: &Arc<Mutex<RequestTracker>>,
) -> zbus::fdo::Result<PortalResponse> {
    if sync::lock(tracker, "email tracker").was_closed(request_path) {
        return Ok((1, HashMap::new()));
    }

    let parsed = match ParsedOptions::from(options) {
        Ok(parsed) => parsed,
        Err(error) => {
            log::warn!("portal: rejecting ComposeEmail for '{app_id}': {error}");
            return Ok((2, HashMap::new()));
        }
    };
    let argv = mailer_argv(&parsed);

    let program = mailer_command();
    log::info!(
        "portal: ComposeEmail for '{app_id}' → {program} ({} attachment(s), {} arg(s))",
        parsed.attachments.len(),
        argv.len()
    );
    let mut command = std::process::Command::new(&program);
    command.args(&argv);
    if let Some(token) = &parsed.activation_token {
        command.env("XDG_ACTIVATION_TOKEN", token);
    }
    if ACTIVE_MAILERS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < MAX_ACTIVE_MAILERS).then_some(active + 1)
        })
        .is_err()
    {
        log::warn!("portal: refusing ComposeEmail request: mailer limit reached");
        return Ok((2, HashMap::new()));
    }
    match command.spawn() {
        Ok(child) => match hand_to_reaper(child) {
            Ok(()) => Ok((0, HashMap::new())),
            Err(error) => {
                log::warn!("portal: could not supervise {program}: {error}");
                Ok((2, HashMap::new()))
            }
        },
        Err(error) => {
            release_mailer_slot();
            log::warn!("portal: could not spawn {program}: {error}");
            Ok((2, HashMap::new()))
        }
    }
}

/// Keep a single process-reaper thread for all mailer launches. Dropping a
/// live `Child` does not reap it; a long-lived D-Bus backend would otherwise
/// accumulate zombies after repeated ComposeEmail calls.
fn hand_to_reaper(child: Child) -> Result<(), String> {
    use std::sync::OnceLock;

    static REAPER: OnceLock<Option<std::sync::mpsc::Sender<Child>>> = OnceLock::new();
    let sender = REAPER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<Child>();
        match std::thread::Builder::new()
            .name("tessera-mailer-reaper".to_owned())
            .spawn(move || {
                let mut children = Vec::new();
                loop {
                    match receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(child) => children.push(child),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            for mut child in children {
                                let _ = child.kill();
                                let _ = child.wait();
                                release_mailer_slot();
                            }
                            return;
                        }
                    }
                    let mut index = 0;
                    while index < children.len() {
                        match children[index].try_wait() {
                            Ok(Some(_)) => {
                                let mut child = children.swap_remove(index);
                                // `try_wait` above reaped and cached the
                                // status; `wait` consumes that cached result
                                // and makes the lifecycle explicit.
                                if let Err(error) = child.wait() {
                                    log::warn!("portal: could not finalize mailer: {error}");
                                }
                                release_mailer_slot();
                            }
                            Ok(None) => index += 1,
                            Err(error) => {
                                log::warn!("portal: could not reap mailer process: {error}");
                                let mut child = children.swap_remove(index);
                                let _ = child.kill();
                                let _ = child.wait();
                                release_mailer_slot();
                            }
                        }
                    }
                }
            }) {
            Ok(_) => Some(sender),
            Err(error) => {
                log::error!("portal: could not start mailer reaper: {error}");
                None
            }
        }
    });

    let Some(sender) = sender else {
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        release_mailer_slot();
        return Err("mailer reaper is unavailable".to_owned());
    };
    sender.send(child).map_err(|error| {
        let mut child = error.0;
        let _ = child.kill();
        let _ = child.wait();
        release_mailer_slot();
        "mailer reaper stopped unexpectedly".to_owned()
    })
}

fn release_mailer_slot() {
    ACTIVE_MAILERS.fetch_sub(1, Ordering::AcqRel);
}

/// The mailer command: `xdg-email` unless overridden (tests point this at a
/// recorder script).
fn mailer_command() -> String {
    std::env::var("ATRIUM_PORTAL_MAILER").unwrap_or_else(|_| "xdg-email".to_string())
}

/// Options parsed out of the `a{sv}` argument.
#[derive(Debug)]
struct ParsedOptions {
    addresses: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: Option<String>,
    body: Option<String>,
    attachments: Vec<std::path::PathBuf>,
    activation_token: Option<String>,
}

impl ParsedOptions {
    fn from(options: &HashMap<String, Value<'_>>) -> Result<Self, String> {
        let string_list = |key: &str| {
            options
                .get(key)
                .and_then(|value| Vec::<String>::try_from(value.clone()).ok())
                .unwrap_or_default()
        };
        let mut addresses = string_list("addresses");
        if let Some(address) = options
            .get("address")
            .and_then(|value| String::try_from(value).ok())
        {
            addresses.push(address);
        }
        let attachment_uris = match options.get("attachments") {
            Some(value) => Vec::<String>::try_from(value.clone())
                .map_err(|_| "the attachments option must be an array of URI strings")?,
            None => Vec::new(),
        };
        let attachments = attachment_uris
            .into_iter()
            .map(|attachment| {
                attachment_path(&attachment).ok_or_else(|| {
                    format!(
                        "attachment is not a valid local file URI or absolute path: {attachment}"
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let parsed = ParsedOptions {
            addresses,
            cc: string_list("cc"),
            bcc: string_list("bcc"),
            subject: options
                .get("subject")
                .and_then(|value| String::try_from(value).ok()),
            body: options
                .get("body")
                .and_then(|value| String::try_from(value).ok()),
            attachments,
            activation_token: options
                .get("activation_token")
                .and_then(|value| String::try_from(value).ok()),
        };
        parsed.validate_limits()?;
        Ok(parsed)
    }

    fn validate_limits(&self) -> Result<(), String> {
        let recipients = self
            .addresses
            .len()
            .checked_add(self.cc.len())
            .and_then(|count| count.checked_add(self.bcc.len()))
            .ok_or_else(|| "recipient count overflow".to_owned())?;
        if recipients > MAX_RECIPIENTS {
            return Err(format!(
                "recipient count exceeds the {MAX_RECIPIENTS}-entry limit"
            ));
        }
        if self.attachments.len() > MAX_ATTACHMENTS {
            return Err(format!(
                "attachment count exceeds the {MAX_ATTACHMENTS}-entry limit"
            ));
        }

        let mut total = 0_usize;
        let mut add = |length: usize| -> Result<(), String> {
            total = total
                .checked_add(length)
                .ok_or_else(|| "mailer argument size overflow".to_owned())?;
            if total > MAX_MAILER_ARGUMENT_BYTES {
                return Err(format!(
                    "mailer arguments exceed the {MAX_MAILER_ARGUMENT_BYTES}-byte limit"
                ));
            }
            Ok(())
        };
        for value in self.addresses.iter().chain(&self.cc).chain(&self.bcc) {
            add(value.len())?;
        }
        for value in [&self.subject, &self.body, &self.activation_token]
            .into_iter()
            .flatten()
        {
            add(value.len())?;
        }
        use std::os::unix::ffi::OsStrExt;
        for attachment in &self.attachments {
            add(attachment.as_os_str().as_bytes().len())?;
        }
        Ok(())
    }
}

/// Decode the documented `file://` backend representation while accepting
/// the absolute-path compatibility form emitted by xdg-desktop-portal 1.20
/// for unsandboxed callers without a document portal mount.
fn attachment_path(attachment: &str) -> Option<std::path::PathBuf> {
    if attachment.starts_with("file://") {
        return files::path_from_file_uri(attachment);
    }
    let path = std::path::PathBuf::from(attachment);
    path.is_absolute().then_some(path)
}

/// The mailer argument vector: recipients as a `mailto:` URI, everything
/// else as xdg-email flags.
fn mailer_argv(parsed: &ParsedOptions) -> Vec<std::ffi::OsString> {
    let mut argv = Vec::new();
    for address in &parsed.cc {
        argv.push("--cc".into());
        argv.push(address.as_str().into());
    }
    for address in &parsed.bcc {
        argv.push("--bcc".into());
        argv.push(address.as_str().into());
    }
    if let Some(subject) = &parsed.subject {
        argv.push("--subject".into());
        argv.push(subject.as_str().into());
    }
    if let Some(body) = &parsed.body {
        argv.push("--body".into());
        argv.push(body.as_str().into());
    }
    for path in &parsed.attachments {
        argv.push("--attach".into());
        argv.push(path.as_os_str().to_owned());
    }
    argv.push(format!("mailto:{}", parsed.addresses.join(",")).into());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, Value<'static>)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn address_and_addresses_merge_into_the_mailto_uri() {
        let parsed = ParsedOptions::from(&options(&[
            ("address", Value::from("first@example.com")),
            (
                "addresses",
                Value::from(vec!["second@example.com".to_string()]),
            ),
        ]))
        .unwrap();
        let argv = mailer_argv(&parsed);
        assert_eq!(
            argv.last().unwrap(),
            std::ffi::OsStr::new("mailto:second@example.com,first@example.com")
        );
    }

    #[test]
    fn flags_cover_cc_bcc_subject_body_and_attachments() {
        let parsed = ParsedOptions::from(&options(&[
            ("cc", Value::from(vec!["carbon@example.com".to_string()])),
            ("bcc", Value::from(vec!["blind@example.com".to_string()])),
            ("subject", Value::from("a subject")),
            ("body", Value::from("the body")),
            (
                "attachments",
                Value::from(vec!["file:///documents/attachment%200".to_string()]),
            ),
        ]))
        .unwrap();
        let argv = mailer_argv(&parsed);
        let flag_value = |flag: &str| {
            let at = argv.iter().position(|arg| arg == flag).unwrap();
            argv[at + 1].clone()
        };
        assert_eq!(flag_value("--cc"), "carbon@example.com");
        assert_eq!(flag_value("--bcc"), "blind@example.com");
        assert_eq!(flag_value("--subject"), "a subject");
        assert_eq!(flag_value("--body"), "the body");
        assert_eq!(flag_value("--attach"), "/documents/attachment 0");
    }

    #[test]
    fn empty_options_still_produce_a_mailto_uri() {
        let parsed = ParsedOptions::from(&HashMap::new()).unwrap();
        let argv = mailer_argv(&parsed);
        assert_eq!(argv.as_slice(), [std::ffi::OsString::from("mailto:")]);
    }

    #[test]
    fn invalid_or_remote_attachment_uri_is_rejected() {
        for uri in [
            "https://example.com/file",
            "file://server/share/file",
            "relative",
        ] {
            let error = ParsedOptions::from(&options(&[(
                "attachments",
                Value::from(vec![uri.to_string()]),
            )]))
            .unwrap_err();
            assert!(error.contains("valid local file URI or absolute path"));
        }
    }

    #[test]
    fn frontend_compatibility_absolute_attachment_path_is_accepted() {
        let parsed = ParsedOptions::from(&options(&[(
            "attachments",
            Value::from(vec!["/tmp/frontend attachment.bin".to_string()]),
        )]))
        .unwrap();
        assert_eq!(
            parsed.attachments,
            [std::path::PathBuf::from("/tmp/frontend attachment.bin")]
        );
    }

    #[test]
    fn oversized_mailer_payload_and_attachment_flood_are_rejected() {
        let oversized = ParsedOptions::from(&options(&[(
            "body",
            Value::from("x".repeat(MAX_MAILER_ARGUMENT_BYTES + 1)),
        )]));
        assert!(oversized.is_err());

        let attachments: Vec<String> = (0..=MAX_ATTACHMENTS)
            .map(|index| format!("file:///tmp/attachment-{index}"))
            .collect();
        let flooded = ParsedOptions::from(&options(&[("attachments", Value::from(attachments))]));
        assert!(flooded.is_err());
    }
}
