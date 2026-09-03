//! `org.freedesktop.impl.portal.Notification` v2, rendered by the
//! Portal-owned notification daemon.
//!
//! Notifications are asynchronous and long-lived, so they do not fit the
//! one-shot prompter: the first `AddNotification` lazily spawns
//! `atrium-portal-prompter --notification-daemon`, which speaks the
//! newline-delimited stream protocol in `atrium_portal_prompter::notify`
//! (commands in, events out). A pump thread maps the daemon's
//! `ActionInvoked` events to the interface's signal and its `Closed`
//! events to bookkeeping. No `Request` objects and no worker thread: both
//! methods are validation plus a channel write (the email/lockdown
//! precedent).
//!
//! Lifecycle: the daemon is spawned on demand and reused. Its death is
//! detected lazily (`try_wait` before the next write, broken pipes on
//! write); state is dropped and the next `AddNotification` respawns.
//! `RemoveNotification` for an unknown id is a no-op success (the spec
//! allows withdrawing never-shown notifications).
//!
//! Bounds and mapping decisions (all logged at debug level when ignored):
//!
//! - `title` ≤ 256 chars, `body` ≤ 4 KiB; `markup-body` is accepted with
//!   the markup stripped (only when `body` is absent) since the dialog
//!   renders plain text.
//! - `icon`/`sound` are accepted and ignored: the daemon renders text and
//!   buttons only.
//! - `default-action-target` and button `target` are not transported over
//!   the stream, so the emitted `ActionInvoked` carries an empty parameter
//!   array (the spec fills it only "if one was specified").
//! - Button `purpose` is not interpreted; a purposed button with a label
//!   shows as a normal button (per spec), one without a label is dropped.
//! - `display-hint`: `transient` forces a short timeout, `persistent`
//!   forces none; other hints are ignored.
//! - Expiry policy (computed here, executed by the daemon): low → 5 s,
//!   normal → 10 s, high/urgent persist.
//! - Caps: 64 live ids per application (over-cap evicts the app's oldest,
//!   which the daemon reports as `Closed`) and 256 total (over-cap rejects
//!   with a warn log — AddNotification has no error return).

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use atrium_portal_prompter::notify::{
    CommandFrame, EventFrame, Notification, NotifyButton, NotifyCommand, NotifyEvent, Priority,
    read_line_bounded,
};
use atrium_portal_runtime::sync;
use zbus::zvariant::Value;

use crate::prompter;

const MAX_ID_BYTES: usize = 255;
const MAX_TITLE_CHARS: usize = 256;
const MAX_BODY_BYTES: usize = 4 * 1024;
const MAX_BUTTONS: usize = 8;
const MAX_LABEL_CHARS: usize = 256;
const MAX_IDS_PER_APP: usize = 64;
const MAX_TOTAL_IDS: usize = 256;

const NOTIFICATION_IFACE: &str = "org.freedesktop.impl.portal.Notification";

/// The served notification interface.
pub(crate) struct NotificationIface {
    /// Blocking handle for the pump thread's signal emission.
    conn: zbus::blocking::Connection,
    /// Shared with the settings watcher, which re-skins the daemon's
    /// cards when desktop preferences change.
    daemon: Arc<Mutex<DaemonManager>>,
}

impl NotificationIface {
    pub(crate) fn new(conn: zbus::blocking::Connection, daemon: Arc<Mutex<DaemonManager>>) -> Self {
        Self { conn, daemon }
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Notification")]
impl NotificationIface {
    async fn add_notification(
        &self,
        app_id: &str,
        id: &str,
        notification: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<()> {
        log::info!("portal: AddNotification for '{app_id}' id '{id}'");
        let notification =
            parse_notification(app_id, id, &notification).map_err(zbus::fdo::Error::InvalidArgs)?;
        sync::lock(&self.daemon, "notification daemon").notify(&self.conn, notification);
        Ok(())
    }

    async fn remove_notification(&self, app_id: &str, id: &str) -> zbus::fdo::Result<()> {
        log::info!("portal: RemoveNotification for '{app_id}' id '{id}'");
        if valid_id(app_id).is_err() || valid_id(id).is_err() {
            // Withdrawing a malformed or unknown id is a no-op success.
            return Ok(());
        }
        sync::lock(&self.daemon, "notification daemon").close(app_id, id);
        Ok(())
    }

    /// No special categories or button purposes are understood.
    #[zbus(property, name = "SupportedOptions")]
    fn supported_options(&self) -> HashMap<String, Value<'static>> {
        HashMap::new()
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

/// Portal ids become stream keys: bounded and control-free.
fn valid_id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_ID_BYTES || value.chars().any(char::is_control) {
        return Err("id is empty, oversized, or contains control characters".to_string());
    }
    Ok(())
}

/// Translate the spec's notification vardict into the stream notification.
/// Unknown keys are ignored with a debug log.
fn parse_notification(
    app_id: &str,
    id: &str,
    map: &HashMap<String, Value<'_>>,
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
        return Err("notification has neither a title nor a body".to_string());
    }

    let priority = match get_string("priority").as_deref() {
        None | Some("normal") => Priority::Normal,
        Some("low") => Priority::Low,
        Some("high") => Priority::High,
        Some("urgent") => Priority::Urgent,
        Some(other) => {
            log::debug!("portal: unknown notification priority {other:?}; using normal");
            Priority::Normal
        }
    };

    let default_action = get_string("default-action").filter(|action| !action.is_empty());
    if let Some(action) = &default_action {
        valid_id(action)?;
    }
    if map.contains_key("default-action-target") {
        log::debug!("portal: ignoring 'default-action-target' (targets are not transported)");
    }

    let mut buttons = Vec::new();
    if let Some(value) = map.get("buttons") {
        let cloned = value
            .try_clone()
            .map_err(|error| format!("buttons value cannot be cloned: {error}"))?;
        let wire = Vec::<HashMap<String, Value>>::try_from(cloned)
            .map_err(|_| "buttons is not an array of vardicts".to_string())?;
        if wire.len() > MAX_BUTTONS {
            return Err(format!("more than {MAX_BUTTONS} buttons"));
        }
        for button in wire {
            let get = |key: &str| button.get(key).and_then(|v| String::try_from(v).ok());
            let Some(action) = get("action").filter(|action| !action.is_empty()) else {
                log::debug!("portal: dropping a button without an action");
                continue;
            };
            valid_id(&action)?;
            let purpose = get("purpose");
            if purpose.is_some() {
                log::debug!("portal: button purpose is not interpreted; showing a normal button");
            }
            let Some(label) = get("label").filter(|label| !label.is_empty()) else {
                if purpose.is_none() {
                    log::debug!("portal: dropping a button without a label");
                }
                continue;
            };
            if label.chars().count() > MAX_LABEL_CHARS || label.contains('\0') {
                return Err("button label is oversized or contains NUL".to_string());
            }
            buttons.push(NotifyButton { action, label });
        }
    }

    let mut hints: Vec<String> = Vec::new();
    if let Some(value) = map.get("display-hint") {
        hints = value
            .try_clone()
            .ok()
            .and_then(|value| Vec::<String>::try_from(value).ok())
            .unwrap_or_default();
    }
    for ignored in ["icon", "sound", "category"] {
        if map.contains_key(ignored) {
            log::debug!("portal: ignoring notification key '{ignored}' (not rendered)");
        }
    }

    let mut expire_hint = match priority {
        Priority::Low => Some(5),
        Priority::Normal => Some(10),
        Priority::High | Priority::Urgent => None,
    };
    for hint in &hints {
        match hint.as_str() {
            "transient" => expire_hint = Some(5),
            "persistent" => expire_hint = None,
            other => log::debug!("portal: ignoring display hint {other:?}"),
        }
    }

    Ok(Notification {
        app_id: app_id.to_owned(),
        id: id.to_owned(),
        title,
        body,
        priority,
        default_action,
        buttons,
        expire_hint,
    })
}

/// Minimal de-markup for `markup-body`: drop every `<...>` span. The
/// dialog renders plain text; the supported tags (`b`, `i`, `a`) carry no
/// text of their own beyond the href, which plain text cannot use.
fn strip_markup(markup: &str) -> String {
    let mut out = String::with_capacity(markup.len());
    let mut in_tag = false;
    for c in markup.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Live-id bookkeeping shared with the pump thread (which forgets cards
/// the daemon reports closed).
#[derive(Default)]
struct LiveNotifications {
    /// app_id → live ids, oldest first.
    per_app: HashMap<String, VecDeque<String>>,
}

impl LiveNotifications {
    fn total(&self) -> usize {
        self.per_app.values().map(VecDeque::len).sum()
    }

    /// Track a notification (id reuse keeps its position); returns the
    /// app's oldest id when the per-app cap forced an eviction.
    fn track(&mut self, app_id: &str, id: &str) -> Option<String> {
        let ids = self.per_app.entry(app_id.to_owned()).or_default();
        if !ids.iter().any(|existing| existing == id) {
            ids.push_back(id.to_owned());
        }
        if ids.len() > MAX_IDS_PER_APP {
            return ids.pop_front();
        }
        None
    }

    fn remove(&mut self, app_id: &str, id: &str) -> bool {
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

    /// Forget every tracked id, returning how many were live. Called when
    /// the daemon they were shown by dies: its cards are gone with it.
    fn clear(&mut self) -> usize {
        let forgotten = self.total();
        self.per_app.clear();
        forgotten
    }
}

/// One running daemon: its stdin (command sink), the child handle (liveness
/// and reaping), and the event pump thread.
struct DaemonHandle {
    child: Child,
    stdin: ChildStdin,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

/// The daemon lifecycle plus the live-id bookkeeping.
#[derive(Default)]
pub(crate) struct DaemonManager {
    daemon: Option<DaemonHandle>,
    live: Arc<Mutex<LiveNotifications>>,
    /// The latest appearance snapshot; re-pushed after every respawn.
    appearance: Option<atrium_portal_prompter::PromptAppearance>,
}

impl DaemonManager {
    /// Push a new appearance snapshot to a live daemon (no spawn): the
    /// settings watcher calls this when desktop preferences change. The
    /// snapshot is remembered so a later respawn re-primes with it.
    pub(crate) fn push_appearance(
        &mut self,
        conn: &zbus::blocking::Connection,
        settings: &crate::settings::SettingsStore,
    ) {
        let appearance = crate::settings::prompt_appearance_of(settings);
        if self.appearance == Some(appearance) {
            return;
        }
        self.appearance = Some(appearance);
        if self.daemon.is_some() {
            self.send(
                conn,
                &CommandFrame::new(NotifyCommand::SetAppearance { appearance }),
            );
        }
    }
}

impl DaemonManager {
    /// Deliver one notification, spawning or respawning the daemon as
    /// needed. AddNotification has no error return, so failures are logged
    /// and the notification dropped.
    fn notify(&mut self, conn: &zbus::blocking::Connection, notification: Notification) {
        if sync::lock(&self.live, "live notifications").total() >= MAX_TOTAL_IDS {
            log::warn!(
                "portal: notification limit of {MAX_TOTAL_IDS} reached; dropping id '{}'",
                notification.id
            );
            return;
        }
        let evicted = sync::lock(&self.live, "live notifications")
            .track(&notification.app_id, &notification.id);
        if let Some(evicted) = evicted {
            log::info!("portal: per-app notification cap; evicting oldest id '{evicted}'");
            self.send(
                conn,
                &CommandFrame::new(NotifyCommand::Close {
                    app_id: notification.app_id.clone(),
                    id: evicted.clone(),
                }),
            );
            sync::lock(&self.live, "live notifications").remove(&notification.app_id, &evicted);
        }
        let app_id = notification.app_id.clone();
        let id = notification.id.clone();
        if !self.send(
            conn,
            &CommandFrame::new(NotifyCommand::Notify(notification)),
        ) {
            sync::lock(&self.live, "live notifications").remove(&app_id, &id);
        }
    }

    /// Withdraw one notification; unknown ids are a no-op.
    fn close(&mut self, app_id: &str, id: &str) {
        if !sync::lock(&self.live, "live notifications").remove(app_id, id) {
            return;
        }
        // Best effort: a dead daemon has nothing showing anyway.
        let _ = self.daemon.as_mut().map(|daemon| {
            daemon
                .stdin
                .write_all(
                    &CommandFrame::new(NotifyCommand::Close {
                        app_id: app_id.to_owned(),
                        id: id.to_owned(),
                    })
                    .encode()
                    .unwrap_or_default(),
                )
                .and_then(|()| daemon.stdin.flush())
        });
    }

    /// Write one frame, respawning the daemon once on failure. Returns
    /// whether the frame was delivered.
    fn send(&mut self, conn: &zbus::blocking::Connection, frame: &CommandFrame) -> bool {
        let Ok(line) = frame.encode() else {
            return false;
        };
        for attempt in 0..2 {
            if !self.ensure_daemon(conn) {
                return false;
            }
            // `ensure_daemon` returned true, so a live daemon exists; the
            // let-else keeps a future refactor of that invariant from
            // panicking a D-Bus method instead of failing the send.
            let Some(daemon) = self.daemon.as_mut() else {
                log::error!("portal: notification daemon missing after ensure");
                return false;
            };
            if daemon
                .stdin
                .write_all(&line)
                .and_then(|()| daemon.stdin.flush())
                .is_ok()
            {
                return true;
            }
            log::warn!("portal: notification daemon write failed; respawning");
            self.daemon = None;
            if attempt == 1 {
                return false;
            }
        }
        false
    }

    /// Ensure a live daemon, spawning on first use or after a death.
    /// A remembered appearance (from [`DaemonManager::push_appearance`])
    /// is re-pushed after the spawn; a first-run daemon is primed by the
    /// first notify anyway.
    fn ensure_daemon(&mut self, conn: &zbus::blocking::Connection) -> bool {
        if let Some(daemon) = &mut self.daemon {
            match daemon.child.try_wait() {
                Ok(None) => return true,
                Ok(Some(status)) => {
                    log::warn!("portal: notification daemon exited with {status}");
                    // The cards died with the daemon: every live-id it
                    // tracked is gone. Forget them here or the map only
                    // ever grows (a dead daemon reports no `Closed`
                    // events), and once it reaches `MAX_TOTAL_IDS` the
                    // portal refuses notifications forever — a
                    // functional outage long outliving the crash.
                    let forgotten = sync::lock(&self.live, "live notifications").clear();
                    log::info!(
                        "portal: forgot {forgotten} notification ids that died with the daemon"
                    );
                    self.daemon = None;
                }
                Err(error) => {
                    log::warn!("portal: could not query notification daemon: {error}");
                    self.daemon = None;
                }
            }
        }
        match spawn_daemon(conn, Arc::clone(&self.live)) {
            Ok(daemon) => {
                log::info!("portal: notification daemon spawned");
                self.daemon = Some(daemon);
                // Re-prime a remembered appearance so respawned cards
                // never flash the fallback palette (stream v2).
                if let Some(appearance) = self.appearance {
                    self.send(
                        conn,
                        &CommandFrame::new(NotifyCommand::SetAppearance { appearance }),
                    );
                }
                true
            }
            Err(error) => {
                log::error!("portal: could not spawn the notification daemon: {error}");
                false
            }
        }
    }
}

/// Spawn the daemon and its event pump thread.
fn spawn_daemon(
    conn: &zbus::blocking::Connection,
    live: Arc<Mutex<LiveNotifications>>,
) -> Result<DaemonHandle, String> {
    let executable = prompter::executable()?;
    let mut child = Command::new(&executable)
        .arg("--notification-daemon")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", executable.display()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "daemon stdin was not piped".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "daemon stdout was not piped".to_owned())?;
    let pump_conn = conn.clone();
    let pump = std::thread::Builder::new()
        .name("atrium-portal-notify-pump".to_owned())
        .spawn(move || event_pump(stdout, pump_conn, live))
        .map_err(|error| format!("could not spawn the event pump: {error}"))?;
    Ok(DaemonHandle {
        child,
        stdin,
        pump: Some(pump),
    })
}

/// Map daemon events to D-Bus signals and bookkeeping until the daemon's
/// stdout closes (its death is detected lazily by the manager).
fn event_pump(
    stdout: ChildStdout,
    conn: zbus::blocking::Connection,
    live: Arc<Mutex<LiveNotifications>>,
) {
    let mut reader = std::io::BufReader::new(stdout);
    loop {
        let line = match read_line_bounded(&mut reader) {
            Ok(Some(Ok(line))) => line,
            Ok(Some(Err(error))) => {
                log::warn!("portal: {error}");
                continue;
            }
            Ok(None) => {
                log::info!("portal: notification daemon's stream ended");
                return;
            }
            Err(error) => {
                log::warn!("portal: notification daemon's stream failed: {error}");
                return;
            }
        };
        let event = match EventFrame::decode(&line) {
            Ok(frame) => frame.event,
            Err(error) => {
                log::warn!("portal: rejecting daemon event: {error}");
                continue;
            }
        };
        match event {
            NotifyEvent::ActionInvoked { app_id, id, action } => {
                emit_action_invoked(&conn, &app_id, &id, &action);
                sync::lock(&live, "live notifications").remove(&app_id, &id);
            }
            NotifyEvent::Closed { app_id, id } => {
                sync::lock(&live, "live notifications").remove(&app_id, &id);
            }
        }
    }
}

/// Emit the interface's `ActionInvoked` signal. The parameter array is
/// empty: action targets are not transported (see the module docs) and no
/// activation token exists.
fn emit_action_invoked(conn: &zbus::blocking::Connection, app_id: &str, id: &str, action: &str) {
    let parameters: Vec<Value<'static>> = Vec::new();
    if let Err(error) = conn.emit_signal(
        None::<&str>,
        crate::DESKTOP_PATH,
        NOTIFICATION_IFACE,
        "ActionInvoked",
        &(app_id, id, action, parameters),
    ) {
        log::warn!("portal: could not emit ActionInvoked for '{id}': {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(pairs: &[(&str, Value<'static>)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn ids_are_bounded() {
        let map = notification(&[("title", Value::from("Hi"))]);
        assert!(parse_notification("app", "id", &map).is_ok());
        assert!(parse_notification("", "id", &map).is_err());
        assert!(parse_notification("app", "", &map).is_err());
        assert!(parse_notification("app", "a\0b", &map).is_err());
        assert!(parse_notification("app", &"x".repeat(256), &map).is_err());
    }

    #[test]
    fn title_body_and_markup_rules() {
        let map = notification(&[]);
        assert!(parse_notification("app", "id", &map).is_err());

        let map = notification(&[("body", Value::from("Just a body"))]);
        let parsed = parse_notification("app", "id", &map).unwrap();
        assert_eq!(parsed.body, "Just a body");

        // markup-body is a stripped fallback for body.
        let map = notification(&[(
            "markup-body",
            Value::from("<b>Bold</b> and <a href=\"https://x\">linked</a>"),
        )]);
        let parsed = parse_notification("app", "id", &map).unwrap();
        assert_eq!(parsed.body, "Bold and linked");

        // An explicit body wins over markup.
        let map = notification(&[
            ("body", Value::from("Plain")),
            ("markup-body", Value::from("<b>Markup</b>")),
        ]);
        assert_eq!(parse_notification("app", "id", &map).unwrap().body, "Plain");

        let oversized = "x".repeat(MAX_TITLE_CHARS + 1);
        let map = notification(&[("title", Value::from(oversized))]);
        assert!(parse_notification("app", "id", &map).is_err());
    }

    #[test]
    fn priority_hints_and_expiry_policy() {
        let map = notification(&[
            ("title", Value::from("Hi")),
            ("priority", Value::from("low")),
        ]);
        assert_eq!(
            parse_notification("app", "id", &map).unwrap().expire_hint,
            Some(5)
        );

        let map = notification(&[("title", Value::from("Hi"))]);
        assert_eq!(
            parse_notification("app", "id", &map).unwrap().expire_hint,
            Some(10)
        );

        let map = notification(&[
            ("title", Value::from("Hi")),
            ("priority", Value::from("urgent")),
        ]);
        assert_eq!(
            parse_notification("app", "id", &map).unwrap().expire_hint,
            None
        );

        // transient forces a short timeout even for urgent; persistent
        // forces none even for low.
        let map = notification(&[
            ("title", Value::from("Hi")),
            ("priority", Value::from("urgent")),
            ("display-hint", Value::from(vec!["transient".to_owned()])),
        ]);
        assert_eq!(
            parse_notification("app", "id", &map).unwrap().expire_hint,
            Some(5)
        );
        let map = notification(&[
            ("title", Value::from("Hi")),
            ("priority", Value::from("low")),
            ("display-hint", Value::from(vec!["persistent".to_owned()])),
        ]);
        assert_eq!(
            parse_notification("app", "id", &map).unwrap().expire_hint,
            None
        );
    }

    #[test]
    fn buttons_follow_the_spec_rules() {
        let button = |pairs: Vec<(&str, Value<'static>)>| -> HashMap<String, Value<'static>> {
            pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
        };
        let map = notification(&[
            ("title", Value::from("Hi")),
            (
                "buttons",
                Value::from(vec![
                    button(vec![
                        ("label", Value::from("Reply")),
                        ("action", Value::from("reply")),
                    ]),
                    // No action: dropped.
                    button(vec![("label", Value::from("Broken"))]),
                    // Unknown purpose but labelled: shown as a normal button.
                    button(vec![
                        ("label", Value::from("Snooze")),
                        ("action", Value::from("snooze")),
                        ("purpose", Value::from("x-vendor.thing")),
                    ]),
                    // Purpose without a label: dropped.
                    button(vec![
                        ("action", Value::from("alert")),
                        ("purpose", Value::from("system.custom-alert")),
                    ]),
                ]),
            ),
        ]);
        let parsed = parse_notification("app", "id", &map).unwrap();
        let labels: Vec<&str> = parsed.buttons.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["Reply", "Snooze"]);

        // More than eight buttons fail the whole notification.
        let many = vec![
            button(vec![
                ("label", Value::from("L")),
                ("action", Value::from("a")),
            ]);
            MAX_BUTTONS + 1
        ];
        let map = notification(&[("title", Value::from("Hi")), ("buttons", Value::from(many))]);
        assert!(parse_notification("app", "id", &map).is_err());
    }

    #[test]
    fn live_ids_track_evict_and_remove() {
        let mut live = LiveNotifications::default();
        assert!(live.track("app", "a").is_none());
        // Id reuse keeps position and does not grow the count.
        assert!(live.track("app", "a").is_none());
        assert_eq!(live.total(), 1);
        // Fill to exactly the per-app cap: "a" plus MAX-1 more.
        for index in 0..MAX_IDS_PER_APP - 1 {
            live.track("app", &format!("n{index}"));
        }
        assert_eq!(live.total(), MAX_IDS_PER_APP);
        // Over the per-app cap the oldest id is evicted.
        assert_eq!(live.track("app", "new"), Some("a".to_owned()));
        assert!(live.remove("app", "n0"));
        assert!(!live.remove("app", "n0"));
        assert!(!live.remove("ghost", "n0"));
    }

    #[test]
    fn a_dead_daemon_forgets_all_live_ids() {
        let mut live = LiveNotifications::default();
        live.track("app-a", "1");
        live.track("app-a", "2");
        live.track("app-b", "3");
        assert_eq!(live.total(), 3);
        // The daemon's death retires every card it was showing.
        assert_eq!(live.clear(), 3);
        assert_eq!(live.total(), 0);
        // Clearing an empty map is a no-op, and tracking works again
        // afterwards (the saturation outage is not permanent).
        assert_eq!(live.clear(), 0);
        assert!(live.track("app-a", "4").is_none());
        assert_eq!(live.total(), 1);
    }

    #[test]
    fn markup_stripping_drops_tag_spans() {
        assert_eq!(strip_markup("<b>hi</b>"), "hi");
        assert_eq!(strip_markup("no markup"), "no markup");
        assert_eq!(strip_markup("<i>a</i> <b>b</b>"), "a b");
        assert_eq!(strip_markup("unclosed <b tag"), "unclosed ");
    }
}
