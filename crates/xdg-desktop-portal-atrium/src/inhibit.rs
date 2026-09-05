//! `org.freedesktop.impl.portal.Inhibit` v3, backed by logind.
//!
//! The honest mechanism available to a Tessera compositor session is
//! `org.freedesktop.login1.Manager.Inhibit`: holding the returned
//! descriptor inhibits, closing it releases. Portal flags map as follows:
//!
//! - **8 (idle)** → logind `idle`
//! - **4 (suspend)** → logind `sleep`
//! - **1 (logout)** and **2 (user switch)** → no logind or session-manager
//!   equivalent exists in this stack. The flags are accepted and tracked
//!   in the session record so introspection stays truthful, but they
//!   inhibit nothing — a documented no-op.
//!
//! The logind mode is always `block`: with no session manager there is no
//! end-session signal to drive a `delay`-plus-`QueryEndResponse` flow, and
//! `idle` only supports `block` anyway.
//!
//! Error surfacing: `Inhibit` returns no response tuple, so a failed
//! logind call fails the D-Bus method itself — callers get a truthful
//! error instead of a silently unbacked inhibition. Logout/user-switch-only
//! requests need no logind call and always succeed (as no-ops). If the
//! system bus or logind is absent, the same rule applies: error, not a
//! fake grant.
//!
//! `CreateMonitor` exports a real
//! `org.freedesktop.impl.portal.Session` object and emits one initial
//! `StateChanged` with `session-state` 1 (Running). With no session
//! manager no Query End/Ending states ever follow, so `QueryEndResponse`
//! is an acknowledged no-op for live monitor sessions and an error for
//! unknown ones.
//!
//! The logind call is a single fast local D-Bus round trip made in the
//! served method — no worker thread (the lockdown/email precedent). The
//! client sits behind the [`SystemInhibitor`] seam so unit tests fake it
//! without a system bus.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use zbus::zvariant::{ObjectPath, OwnedFd, Value};

use atrium_portal_runtime::{RequestTracker, sync};

/// The freedesktop Inhibit flag values.
pub(crate) const INHIBIT_LOGOUT: u32 = 1;
pub(crate) const INHIBIT_USER_SWITCH: u32 = 2;
pub(crate) const INHIBIT_SUSPEND: u32 = 4;
pub(crate) const INHIBIT_IDLE: u32 = 8;
const KNOWN_FLAGS: u32 = INHIBIT_LOGOUT | INHIBIT_USER_SWITCH | INHIBIT_SUSPEND | INHIBIT_IDLE;

/// Live inhibit requests plus monitor sessions, combined cap.
const MAX_INHIBIT_OBJECTS: usize = 64;
const MAX_REASON_BYTES: usize = 16 * 1024;

const INHIBIT_IFACE: &str = "org.freedesktop.impl.portal.Inhibit";

/// The logind client seam (see the module docs).
pub(crate) trait SystemInhibitor: Send + Sync {
    /// Take an inhibition lock; the returned descriptor inhibits while
    /// held and releases on close.
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> Result<OwnedFd, String>;
}

/// The production inhibitor: `org.freedesktop.login1.Manager.Inhibit` on
/// the system bus. The connection is established lazily so the portal
/// starts on machines without a system bus (calls then fail honestly).
pub(crate) struct Logind {
    conn: Mutex<Option<zbus::blocking::Connection>>,
}

impl Logind {
    pub(crate) fn new() -> Self {
        Self {
            conn: Mutex::new(None),
        }
    }
}

impl SystemInhibitor for Logind {
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> Result<OwnedFd, String> {
        let mut guard = sync::lock(&self.conn, "logind connection");
        if guard.is_none() {
            let conn = zbus::blocking::Connection::system()
                .map_err(|error| format!("system bus unavailable: {error}"))?;
            *guard = Some(conn);
        }
        // Invariant, not a fallible step: the branch above stored the
        // connection, and the lock is still held.
        let conn = guard
            .as_ref()
            .expect("logind connection stored under the same lock");
        let reply = conn.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Inhibit",
            &(what, who, why, mode),
        );
        match reply {
            Ok(reply) => reply
                .body()
                .deserialize::<OwnedFd>()
                .map_err(|error| format!("logind returned a malformed lock: {error}")),
            Err(error) => {
                // Drop the cached connection so the next call reconnects.
                *guard = None;
                Err(format!("logind Inhibit({what}) failed: {error}"))
            }
        }
    }
}

/// One accepted Inhibit call's bookkeeping.
struct InhibitRecord {
    app_id: String,
    flags: u32,
    /// The held logind lock; `None` for logout/user-switch-only requests
    /// (the tracked no-op).
    lock: Option<OwnedFd>,
}

/// Live inhibit requests keyed by their Request object path, plus the
/// monitor session paths.
#[derive(Default)]
pub(crate) struct InhibitRegistry {
    requests: HashMap<String, InhibitRecord>,
    monitors: HashSet<String>,
}

impl InhibitRegistry {
    fn live(&self) -> usize {
        self.requests.len() + self.monitors.len()
    }

    fn add_request(
        &mut self,
        path: &str,
        app_id: &str,
        flags: u32,
        lock: Option<OwnedFd>,
    ) -> Result<(), String> {
        if self.requests.contains_key(path) || self.monitors.contains(path) {
            return Err(format!("inhibit object {path} is already active"));
        }
        if self.live() >= MAX_INHIBIT_OBJECTS {
            return Err(format!("inhibit limit of {MAX_INHIBIT_OBJECTS} reached"));
        }
        self.requests.insert(
            path.to_string(),
            InhibitRecord {
                app_id: app_id.to_string(),
                flags,
                lock,
            },
        );
        Ok(())
    }

    fn remove_request(&mut self, path: &str) -> Option<InhibitRecord> {
        self.requests.remove(path)
    }

    fn add_monitor(&mut self, path: &str) -> Result<(), String> {
        if self.requests.contains_key(path) || self.monitors.contains(path) {
            return Err(format!("inhibit object {path} is already active"));
        }
        if self.live() >= MAX_INHIBIT_OBJECTS {
            return Err(format!("inhibit limit of {MAX_INHIBIT_OBJECTS} reached"));
        }
        self.monitors.insert(path.to_string());
        Ok(())
    }

    fn remove_monitor(&mut self, path: &str) -> bool {
        self.monitors.remove(path)
    }

    fn is_monitor(&self, path: &str) -> bool {
        self.monitors.contains(path)
    }
}

/// The logind `what` string for a flag set: the flags this stack can
/// genuinely back. Empty means the request is a tracked no-op (logout
/// and/or user switch only).
fn logind_what(flags: u32) -> String {
    let mut what = Vec::new();
    if flags & INHIBIT_IDLE != 0 {
        what.push("idle");
    }
    if flags & INHIBIT_SUSPEND != 0 {
        what.push("sleep");
    }
    what.join(":")
}

/// Validate and take the inhibition: the logind lock first (so a failure
/// leaves no bookkeeping behind), then the registry record. A registry
/// refusal drops the just-taken lock, releasing it immediately.
fn apply_inhibit(
    inhibitor: &dyn SystemInhibitor,
    registry: &mut InhibitRegistry,
    path: &str,
    app_id: &str,
    flags: u32,
    reason: Option<String>,
) -> Result<(), String> {
    if flags == 0 || flags & !KNOWN_FLAGS != 0 {
        return Err(format!("unsupported or empty inhibit flags {flags:#x}"));
    }
    let what = logind_what(flags);
    let lock = if what.is_empty() {
        None
    } else {
        let who = if app_id.is_empty() {
            "A sandboxed application"
        } else {
            app_id
        };
        let why = reason
            .as_deref()
            .filter(|reason| !reason.is_empty())
            .unwrap_or("Portal inhibition request");
        Some(inhibitor.inhibit(&what, who, why, "block")?)
    };
    registry.add_request(path, app_id, flags, lock)
}

/// The `reason` option, bounded and NUL-free.
fn parse_reason(options: &HashMap<String, Value<'_>>) -> Result<Option<String>, String> {
    match options
        .get("reason")
        .and_then(|value| String::try_from(value).ok())
    {
        Some(reason) if reason.len() > MAX_REASON_BYTES || reason.contains('\0') => {
            Err("reason is oversized or contains NUL".to_string())
        }
        other => Ok(other),
    }
}

/// The served inhibit interface. No worker: the logind call happens in the
/// method, so failures surface as D-Bus errors (see the module docs).
pub(crate) struct InhibitIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) registry: Arc<Mutex<InhibitRegistry>>,
    pub(crate) inhibitor: Arc<dyn SystemInhibitor>,
}

impl InhibitIface {
    pub(crate) fn new(conn: zbus::Connection, tracker: Arc<Mutex<RequestTracker>>) -> Self {
        Self {
            conn,
            tracker,
            registry: Arc::new(Mutex::new(InhibitRegistry::default())),
            inhibitor: Arc::new(Logind::new()),
        }
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Inhibit")]
impl InhibitIface {
    async fn inhibit(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        _window: &str,
        flags: u32,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<()> {
        let path = handle.as_str().to_string();
        log::info!("portal: Inhibit for '{app_id}' at {path} (flags {flags:#x})");

        let reason = parse_reason(&options).map_err(zbus::fdo::Error::InvalidArgs)?;
        {
            let mut registry = sync::lock(&self.registry, "inhibit registry");
            apply_inhibit(
                self.inhibitor.as_ref(),
                &mut registry,
                &path,
                app_id,
                flags,
                reason,
            )
            .map_err(zbus::fdo::Error::Failed)?;
        }
        let inserted = self
            .conn
            .object_server()
            .at(
                path.as_str(),
                InhibitRequestIface {
                    path: path.clone(),
                    conn: self.conn.clone(),
                    registry: Arc::clone(&self.registry),
                },
            )
            .await
            .map_err(zbus::fdo::Error::from)?;
        if !inserted {
            sync::lock(&self.registry, "inhibit registry").remove_request(&path);
            return Err(zbus::fdo::Error::Failed(format!(
                "request handle {path} is already active"
            )));
        }
        if flags & (INHIBIT_LOGOUT | INHIBIT_USER_SWITCH) != 0 {
            log::info!(
                "portal: Inhibit for '{app_id}' tracks logout/user-switch as a no-op (see module docs)"
            );
        }
        Ok(())
    }

    async fn create_monitor(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _window: &str,
    ) -> zbus::fdo::Result<u32> {
        let path = handle.as_str().to_string();
        let session_path = session_handle.as_str().to_string();
        log::info!("portal: CreateMonitor for '{app_id}' at {session_path}");

        atrium_portal_runtime::register(&self.conn, &self.tracker, &path).await?;
        let result = self.create_monitor_inner(&session_path).await;
        atrium_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
        result
    }

    /// No session manager means no Query End ever arrives; acknowledge the
    /// call for live monitor sessions and reject unknown ones.
    async fn query_end_response(&self, session_handle: ObjectPath<'_>) -> zbus::fdo::Result<()> {
        let path = session_handle.as_str();
        if sync::lock(&self.registry, "inhibit registry").is_monitor(path) {
            log::debug!("portal: QueryEndResponse for {path} acknowledged (no-op)");
            Ok(())
        } else {
            Err(zbus::fdo::Error::InvalidArgs(format!(
                "unknown inhibit monitor session {path}"
            )))
        }
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        3
    }
}

impl InhibitIface {
    async fn create_monitor_inner(&self, session_path: &str) -> zbus::fdo::Result<u32> {
        if let Err(error) = sync::lock(&self.registry, "inhibit registry").add_monitor(session_path)
        {
            log::warn!("portal: refusing monitor session: {error}");
            return Ok(2);
        }
        let inserted = self
            .conn
            .object_server()
            .at(
                session_path,
                InhibitSessionIface {
                    path: session_path.to_owned(),
                    conn: self.conn.clone(),
                    registry: Arc::clone(&self.registry),
                },
            )
            .await
            .map_err(zbus::fdo::Error::from)?;
        if !inserted {
            sync::lock(&self.registry, "inhibit registry").remove_monitor(session_path);
            return Ok(2);
        }
        // The one truthful state this stack can report: Running. Nothing
        // else ever follows (no session manager).
        let state = HashMap::from([("session-state".to_owned(), Value::from(1u32))]);
        let session = ObjectPath::try_from(session_path)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        if let Err(error) = self
            .conn
            .emit_signal(
                None::<&str>,
                crate::DESKTOP_PATH,
                INHIBIT_IFACE,
                "StateChanged",
                &(session, state),
            )
            .await
        {
            log::warn!("portal: could not emit the initial StateChanged: {error}");
        }
        Ok(0)
    }
}

/// The Request object owned by one accepted Inhibit call. `Close` releases
/// the inhibition (dropping the logind lock) and removes the object.
struct InhibitRequestIface {
    path: String,
    conn: zbus::Connection,
    registry: Arc<Mutex<InhibitRegistry>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl InhibitRequestIface {
    async fn close(&self) -> zbus::fdo::Result<()> {
        let released = sync::lock(&self.registry, "inhibit registry").remove_request(&self.path);
        if let Some(record) = released {
            log::info!(
                "portal: released inhibition of '{}' at {} (flags {:#x}, logind lock: {})",
                record.app_id,
                self.path,
                record.flags,
                // Dropping the record closes the descriptor, releasing
                // the logind inhibition.
                record.lock.is_some()
            );
        }
        if let Err(error) = self
            .conn
            .object_server()
            .remove::<InhibitRequestIface, _>(self.path.as_str())
            .await
        {
            log::warn!(
                "portal: could not remove inhibit request {}: {error}",
                self.path
            );
        }
        Ok(())
    }
}

/// The monitor Session object. There is no teardown beyond unregistration:
/// monitors hold no resources.
struct InhibitSessionIface {
    path: String,
    conn: zbus::Connection,
    registry: Arc<Mutex<InhibitRegistry>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl InhibitSessionIface {
    async fn close(&self) -> zbus::fdo::Result<()> {
        log::info!("portal: monitor session {} closed by client", self.path);
        sync::lock(&self.registry, "inhibit registry").remove_monitor(&self.path);
        if let Err(error) = self
            .conn
            .object_server()
            .remove::<InhibitSessionIface, _>(self.path.as_str())
            .await
        {
            log::warn!(
                "portal: could not remove monitor session {}: {error}",
                self.path
            );
        }
        Ok(())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records calls; can be told to fail.
    struct FakeInhibitor {
        calls: Mutex<Vec<(String, String, String, String)>>,
        fail: bool,
    }

    impl SystemInhibitor for FakeInhibitor {
        fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> Result<OwnedFd, String> {
            self.calls.lock().unwrap().push((
                what.to_owned(),
                who.to_owned(),
                why.to_owned(),
                mode.to_owned(),
            ));
            if self.fail {
                return Err("fake logind failure".to_owned());
            }
            // A real descriptor, standing in for logind's lock.
            let fd = std::fs::File::open("/dev/null").unwrap();
            Ok(std::os::fd::OwnedFd::from(fd).into())
        }
    }

    fn fake(fail: bool) -> FakeInhibitor {
        FakeInhibitor {
            calls: Mutex::new(Vec::new()),
            fail,
        }
    }

    #[test]
    fn flags_map_to_logind_subjects() {
        assert_eq!(logind_what(INHIBIT_IDLE), "idle");
        assert_eq!(logind_what(INHIBIT_SUSPEND), "sleep");
        assert_eq!(logind_what(INHIBIT_IDLE | INHIBIT_SUSPEND), "idle:sleep");
        // Logout and user switch have no logind subject: the tracked no-op.
        assert_eq!(logind_what(INHIBIT_LOGOUT | INHIBIT_USER_SWITCH), "");
        assert_eq!(logind_what(INHIBIT_LOGOUT | INHIBIT_IDLE), "idle");
    }

    #[test]
    fn flag_validation_rejects_zero_and_unknown_bits() {
        let inhibitor = fake(false);
        let mut registry = InhibitRegistry::default();
        assert!(apply_inhibit(&inhibitor, &mut registry, "/r/0", "app", 0, None).is_err());
        assert!(apply_inhibit(&inhibitor, &mut registry, "/r/x", "app", 0x10, None).is_err());
        assert!(registry.requests.is_empty());
    }

    #[test]
    fn tracked_no_op_makes_no_logind_call() {
        let inhibitor = fake(false);
        let mut registry = InhibitRegistry::default();
        apply_inhibit(
            &inhibitor,
            &mut registry,
            "/r/1",
            "org.example.App",
            INHIBIT_LOGOUT | INHIBIT_USER_SWITCH,
            None,
        )
        .unwrap();
        assert!(inhibitor.calls.lock().unwrap().is_empty());
        let record = registry.remove_request("/r/1").unwrap();
        assert!(record.lock.is_none());
        assert_eq!(record.flags, INHIBIT_LOGOUT | INHIBIT_USER_SWITCH);
    }

    #[test]
    fn logind_call_carries_who_why_and_block_mode() {
        let inhibitor = fake(false);
        let mut registry = InhibitRegistry::default();
        apply_inhibit(
            &inhibitor,
            &mut registry,
            "/r/1",
            "org.example.Player",
            INHIBIT_IDLE | INHIBIT_SUSPEND,
            Some("Playing a film".to_owned()),
        )
        .unwrap();
        let calls = inhibitor.calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[(
                "idle:sleep".to_owned(),
                "org.example.Player".to_owned(),
                "Playing a film".to_owned(),
                "block".to_owned()
            )]
        );
        drop(calls);
        assert!(registry.remove_request("/r/1").unwrap().lock.is_some());
    }

    #[test]
    fn logind_failure_leaves_no_bookkeeping() {
        let inhibitor = fake(true);
        let mut registry = InhibitRegistry::default();
        assert!(
            apply_inhibit(&inhibitor, &mut registry, "/r/1", "app", INHIBIT_IDLE, None).is_err()
        );
        assert!(registry.requests.is_empty());
    }

    #[test]
    fn registry_caps_and_deduplicates_objects() {
        let mut registry = InhibitRegistry::default();
        registry
            .add_request("/r/dup", "app", INHIBIT_IDLE, None)
            .unwrap();
        assert!(
            registry
                .add_request("/r/dup", "app", INHIBIT_IDLE, None)
                .is_err()
        );
        // A monitor path collides with a request path too.
        assert!(registry.add_monitor("/r/dup").is_err());
        registry.add_monitor("/m/1").unwrap();
        assert!(registry.is_monitor("/m/1"));
        assert!(registry.remove_monitor("/m/1"));
        assert!(!registry.is_monitor("/m/1"));

        registry.remove_request("/r/dup");
        for index in 0..MAX_INHIBIT_OBJECTS {
            registry
                .add_request(&format!("/r/{index}"), "app", INHIBIT_IDLE, None)
                .unwrap();
        }
        assert!(
            registry
                .add_request("/r/overflow", "app", INHIBIT_IDLE, None)
                .is_err()
        );
        assert!(registry.add_monitor("/m/overflow").is_err());
    }

    #[test]
    fn reason_option_is_bounded() {
        let options = HashMap::from([("reason".to_owned(), Value::from("Because"))]);
        assert_eq!(parse_reason(&options).unwrap().as_deref(), Some("Because"));
        assert!(parse_reason(&HashMap::new()).unwrap().is_none());
        let oversized = HashMap::from([(
            "reason".to_owned(),
            Value::from("x".repeat(MAX_REASON_BYTES + 1)),
        )]);
        assert!(parse_reason(&oversized).is_err());
    }
}
