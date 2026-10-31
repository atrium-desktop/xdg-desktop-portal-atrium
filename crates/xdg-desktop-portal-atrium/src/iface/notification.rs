//! `org.freedesktop.impl.portal.Notification` v2, rendered by the compositor.
//!
//! Notifications are asynchronous and long-lived, so they do not fit the
//! one-shot prompt path. ADR-0026 gives them to the component that already
//! owns a notification queue and renders it: the compositor. Each
//! `AddNotification` becomes one `Command::Notify` on the `atrium-portal`
//! scope, and `RemoveNotification` becomes `Command::DismissNotification`.
//!
//! ## What survives the move, and what cannot
//!
//! The portal's notification surface is richer than the compositor queue's, so
//! several features degrade instead of being silently faked:
//!
//! - **Supported**: title (rendered as the queue entry's summary), body, and
//!   application id. A `markup-body` is accepted only when `body` is absent,
//!   and the markup is stripped (the queue renders plain text).
//! - **Dropped**: the action list, button labels, `icon`, `sound`,
//!   `category`, `desktop-entry`, and `display-hint`. The queue's entry is
//!   summary/body/app-id only; there is no surface for any of them, so they
//!   are logged at debug and ignored, and no `ActionInvoked` signal is ever
//!   emitted because the compositor reports no action back.
//! - **Cannot be withdrawn**: the queue addresses an entry by its compositor
//!   id, and the portal scope is granted `Notify` and `DismissNotification`
//!   but *not* `ObserveNotifications`, so the backend can never learn the id
//!   that `Command::Notify` created. `RemoveNotification` therefore stops
//!   tracking the id locally (so the per-app cap behaves as the spec expects)
//!   and logs that the card remains visible until the compositor's TTL expires.
//!   Every notification is retracted by the tape, not by the application.
//!
//! Bounds are enforced here so a malformed request never reaches IPC: app id
//! and notification id ≤ 255 bytes and control-free, title ≤ 256 characters,
//! body ≤ 4 KiB, and the spec's per-application and global caps.
//!
//! No `Request` objects and no worker thread: both methods are validation plus
//! one compositor round-trip (the lockdown/email precedent).

use std::collections::{HashMap, VecDeque};

use zbus::Variant;

use crate::ipc;

const MAX_ID_BYTES: usize = 255;
const MAX_TITLE_CHARS: usize = 256;
const MAX_BODY_BYTES: usize = 4 * 1024;
const MAX_IDS_PER_APP: usize = 64;
const MAX_TOTAL_IDS: usize = 256;

/// The served notification interface.
pub(crate) struct NotificationIface {
    /// Live-id bookkeeping for the spec's per-application and global caps.
    live: std::sync::Arc<std::sync::Mutex<LiveNotifications>>,
}

impl NotificationIface {
    pub(crate) fn new(live: std::sync::Arc<std::sync::Mutex<LiveNotifications>>) -> Self {
        Self { live }
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Notification")]
impl NotificationIface {
    async fn add_notification(
        &self,
        app_id: &str,
        id: &str,
        notification: HashMap<String, Variant<'_>>,
    ) -> zbus::fdo::Result<()> {
        log::info!("portal: AddNotification for '{app_id}' id '{id}'");
        let parsed =
            parse_notification(app_id, id, &notification).map_err(zbus::fdo::Error::InvalidArgs)?;
        let live = std::sync::Arc::clone(&self.live);
        // AddNotification has no error return, so the compositor round-trip
        // runs on a short-lived thread: a wedged compositor must not stall a
        // zbus executor thread that other interfaces share.
        std::thread::Builder::new()
            .name("atrium-portal-notify".to_owned())
            .spawn(move || deliver(&live, parsed))
            .map(|_| ())
            .map_err(|error| zbus::fdo::Error::Failed(format!("notify thread: {error}")))
    }

    async fn remove_notification(&self, app_id: &str, id: &str) -> zbus::fdo::Result<()> {
        log::info!("portal: RemoveNotification for '{app_id}' id '{id}'");
        if valid_id(app_id).is_err() || valid_id(id).is_err() {
            // Withdrawing a malformed or unknown id is a no-op success.
            return Ok(());
        }
        let tracked = crate::notification::lock_live(&self.live).remove(app_id, id);
        if tracked {
            log::info!(
                "portal: notification '{id}' is withdrawn from the portal but remains visible \
                 until the compositor's retention window expires (the portal scope cannot \
                 address a queue entry it did not create)"
            );
        }
        Ok(())
    }

    /// No special categories or button purposes are understood.
    #[zbus(property, name = "SupportedOptions")]
    fn supported_options(&self) -> HashMap<String, Variant<'static>> {
        HashMap::new()
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

/// One validated notification ready for IPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Notification {
    pub(crate) summary: String,
    pub(crate) body: String,
    pub(crate) app_id: Option<String>,
    /// The portal's per-application id, so the compositor can match the card.
    pub(crate) external_id: String,
}

/// Deliver one notification and record it against the caps.
fn deliver(live: &std::sync::Arc<std::sync::Mutex<LiveNotifications>>, notification: Notification) {
    if crate::notification::lock_live(live).total() >= MAX_TOTAL_IDS {
        log::warn!(
            "portal: notification limit of {MAX_TOTAL_IDS} reached; dropping id '{}'",
            notification.external_id
        );
        return;
    }
    let app_id = notification.app_id.clone().unwrap_or_default();
    let evicted = crate::notification::lock_live(live).track(&app_id, &notification.external_id);
    if let Some(evicted) = evicted {
        log::info!("portal: per-app notification cap; forgetting oldest id '{evicted}'");
        crate::notification::lock_live(live).remove(&app_id, &evicted);
    }
    let socket = match ipc::compositor_socket() {
        Ok(socket) => socket,
        Err(error) => {
            log::warn!("portal: cannot deliver notification: {error}");
            crate::notification::lock_live(live).remove(&app_id, &notification.external_id);
            return;
        }
    };
    if let Err(error) = ipc::post_notification(
        &socket,
        &notification.summary,
        &notification.body,
        notification.app_id.clone(),
        Some(notification.external_id.clone()),
    ) {
        log::warn!(
            "portal: could not deliver notification '{}': {error}",
            notification.external_id
        );
        crate::notification::lock_live(live).remove(&app_id, &notification.external_id);
    }
}

/// Lock the shared live-id map, recovering from a poisoned mutex: the map is
/// pure bookkeeping, and a panicking delivery thread must not turn every later
/// notification into a crash (the [`crate::ipc`] worker policy).
pub(crate) fn lock_live(
    live: &std::sync::Arc<std::sync::Mutex<LiveNotifications>>,
) -> std::sync::MutexGuard<'_, LiveNotifications> {
    live.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Portal ids become compositor keys: bounded and control-free.
fn valid_id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_ID_BYTES || value.chars().any(char::is_control) {
        return Err("id is empty, oversized, or contains control characters".to_string());
    }
    Ok(())
}

/// Translate the spec's notification vardict. Unknown keys are ignored with a
/// debug log.
fn parse_notification(
    app_id: &str,
    id: &str,
    map: &HashMap<String, Variant<'_>>,
) -> Result<Notification, String> {
    valid_id(app_id)?;
    valid_id(id)?;

    let get_string = |key: &str| map.get(key).and_then(|value| String::try_from(value).ok());
    let title = get_string("title").unwrap_or_default();
    if title.chars().count() > MAX_TITLE_CHARS || title.contains('\0') {
        return Err("title is oversized or contains NUL".to_string());
    }
    let mut body = get_string("body").unwrap_or_default();
    if body.is_empty()
        && let Some(markup) = get_string("markup-body")
    {
        body = strip_markup(&markup);
    }
    if body.len() > MAX_BODY_BYTES || body.contains('\0') {
        return Err("body is oversized or contains NUL".to_string());
    }
    if title.trim().is_empty() && body.trim().is_empty() {
        return Err("a notification needs a title or a body".to_string());
    }
    for key in [
        "buttons",
        "icon",
        "sound",
        "category",
        "desktop-entry",
        "display-hint",
    ] {
        if map.contains_key(key) {
            log::debug!("portal: dropping unsupported notification key {key:?}");
        }
    }

    Ok(Notification {
        summary: title,
        body,
        app_id: (!app_id.is_empty()).then(|| app_id.to_owned()),
        external_id: id.to_owned(),
    })
}

/// Strip a minimal Pango-ish markup subset to plain text.
fn strip_markup(markup: &str) -> String {
    let mut out = String::with_capacity(markup.len());
    let mut chars = markup.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '<' => {
                for next in chars.by_ref() {
                    if next == '>' {
                        break;
                    }
                }
            }
            '&' => {
                let mut entity = String::new();
                while let Some(&next) = chars.peek() {
                    if next == ';' || entity.len() > 8 {
                        break;
                    }
                    entity.push(next);
                    chars.next();
                }
                if chars.peek() == Some(&';') {
                    chars.next();
                }
                match entity.as_str() {
                    "amp" => out.push('&'),
                    "lt" => out.push('<'),
                    "gt" => out.push('>'),
                    "quot" => out.push('"'),
                    "apos" => out.push('\''),
                    _ => {
                        out.push('&');
                        out.push_str(&entity);
                        out.push(';');
                    }
                }
                let _ = character;
            }
            other => out.push(other),
        }
    }
    out
}

/// Live-id bookkeeping: the ids this application believes are showing.
///
/// The compositor owns the real cards. This map exists only so the spec's
/// caps behave (one application cannot displace another's notifications) and
/// so a repeat `AddNotification` never double-counts.
#[derive(Debug, Default)]
pub(crate) struct LiveNotifications {
    /// app_id → live ids, oldest first.
    per_app: HashMap<String, VecDeque<String>>,
}

impl LiveNotifications {
    pub(crate) fn total(&self) -> usize {
        self.per_app.values().map(VecDeque::len).sum()
    }

    /// Track a notification (id reuse keeps its position); returns the app's
    /// oldest id when the per-app cap forced an eviction.
    pub(crate) fn track(&mut self, app_id: &str, id: &str) -> Option<String> {
        let ids = self.per_app.entry(app_id.to_owned()).or_default();
        if !ids.iter().any(|existing| existing == id) {
            ids.push_back(id.to_owned());
        }
        if ids.len() > MAX_IDS_PER_APP {
            return ids.pop_front();
        }
        None
    }

    pub(crate) fn remove(&mut self, app_id: &str, id: &str) -> bool {
        let Some(ids) = self.per_app.get_mut(app_id) else {
            return false;
        };
        let removed = ids
            .iter()
            .position(|existing| existing == id)
            .map(|index| ids.remove(index));
        if ids.is_empty() {
            self.per_app.remove(app_id);
        }
        removed.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, Variant<'static>)]) -> HashMap<String, Variant<'static>> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn notification_requires_a_title_or_body() {
        assert!(parse_notification("app", "1", &map(&[])).is_err());
        let parsed =
            parse_notification("app", "1", &map(&[("body", Variant::from("hello"))])).unwrap();
        assert_eq!(parsed.summary, "");
        assert_eq!(parsed.body, "hello");
    }

    #[test]
    fn ids_and_text_are_bounded_and_control_free() {
        assert!(parse_notification("app", "", &map(&[("title", Variant::from("t"))])).is_err());
        assert!(
            parse_notification(
                &"a".repeat(256),
                "1",
                &map(&[("title", Variant::from("t"))])
            )
            .is_err()
        );
        assert!(parse_notification("app", "1", &map(&[("title", Variant::from("a\0b"))])).is_err());
        let long = "x".repeat(MAX_TITLE_CHARS + 1);
        assert!(parse_notification("app", "1", &map(&[("title", Variant::from(long))])).is_err());
    }

    #[test]
    fn markup_body_is_stripped_only_when_body_is_absent() {
        let parsed = parse_notification(
            "app",
            "1",
            &map(&[("markup-body", Variant::from("<b>bold</b> &amp; plain"))]),
        )
        .unwrap();
        assert_eq!(parsed.body, "bold & plain");

        let parsed = parse_notification(
            "app",
            "1",
            &map(&[
                ("body", Variant::from("plain")),
                ("markup-body", Variant::from("<i>ignored</i>")),
            ]),
        )
        .unwrap();
        assert_eq!(parsed.body, "plain");
    }

    #[test]
    fn per_app_cap_evicts_the_oldest_id() {
        let mut live = LiveNotifications::default();
        for index in 0..MAX_IDS_PER_APP {
            assert!(live.track("app", &index.to_string()).is_none());
        }
        assert_eq!(live.track("app", "new"), Some("0".to_owned()));
        assert_eq!(live.total(), MAX_IDS_PER_APP);
        assert!(live.remove("app", "new"));
        assert!(!live.remove("app", "new"));
    }

    #[test]
    fn tracking_is_idempotent_per_id() {
        let mut live = LiveNotifications::default();
        assert!(live.track("app", "1").is_none());
        assert!(live.track("app", "1").is_none());
        assert_eq!(live.total(), 1);
    }
}
