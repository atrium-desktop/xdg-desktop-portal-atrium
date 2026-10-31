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
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::{ObjectPath, OwnedFd, Variant};

use atrium_portal_request::{RequestTracker, sync};

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

/// A boxed inhibition future, kept dynamic because the seam is used as `dyn`
/// behind the interface object.
/// How long the logind inhibit call may take before it is abandoned.
///
/// A wedged logind (a real shutdown-time failure mode) would otherwise leave
/// the `Inhibit` method task hanging on the connection's runtime indefinitely,
/// holding its exported request object open. This bounds the wait to a
/// reasonable D-Bus round trip and turns the hang into a truthful error.
const LOGIND_CALL_TIMEOUT: Duration = Duration::from_secs(10);

type InhibitFuture<'a> = Pin<Box<dyn Future<Output = Result<OwnedFd, String>> + Send + 'a>>;

/// The logind client seam. Async so the served method can await the logind
/// round trip instead of blocking the connection's runtime (see the module
/// docs).
pub(crate) trait SystemInhibitor: Send + Sync {
    /// Take an inhibition lock; the returned descriptor inhibits while
    /// held and releases on close.
    fn inhibit<'a>(
        &'a self,
        what: &'a str,
        who: &'a str,
        why: &'a str,
        mode: &'a str,
    ) -> InhibitFuture<'a>;
}

/// The production inhibitor: `org.freedesktop.login1.Manager.Inhibit` on
/// the system bus. The connection is established lazily so the portal
/// starts on machines without a system bus (calls then fail honestly).
pub(crate) struct Logind {
    conn: Mutex<Option<zbus::Connection>>,
}

impl Logind {
    pub(crate) fn new() -> Self {
        Self {
            conn: Mutex::new(None),
        }
    }

    /// The cached system-bus connection, established on first use.
    async fn connection(&self) -> zbus::Result<zbus::Connection> {
        // The lock is taken only to read or store the connection and never held
        // across the await: `MutexGuard` is not `Send`, so holding it would keep
        // this future off the connection's runtime.
        if let Some(conn) = sync::lock(&self.conn, "logind connection").clone() {
            return Ok(conn);
        }
        // Bound every method call on this connection: logind can wed (especially
        // during machine shutdown), and without a timeout this method task
        // would hang forever on the connection runtime while holding its
        // exported request object open.
        let conn = zbus::connection::Builder::system()
            .method_timeout(LOGIND_CALL_TIMEOUT)
            .build()
            .await?;
        *sync::lock(&self.conn, "logind connection") = Some(conn.clone());
        Ok(conn)
    }
}

impl SystemInhibitor for Logind {
    fn inhibit<'a>(
        &'a self,
        what: &'a str,
        who: &'a str,
        why: &'a str,
        mode: &'a str,
    ) -> InhibitFuture<'a> {
        Box::pin(async move {
            let conn = self
                .connection()
                .await
                .map_err(|error| format!("system bus unavailable: {error}"))?;
            match conn
                .call_method(
                    Some("org.freedesktop.login1"),
                    "/org/freedesktop/login1",
                    Some("org.freedesktop.login1.Manager"),
                    "Inhibit",
                    &(what, who, why, mode),
                )
                .await
            {
                Ok(reply) => reply
                    .body()
                    .deserialize::<OwnedFd>()
                    .map_err(|error| format!("logind returned a malformed lock: {error}")),
                Err(error) => {
                    // Drop the cached connection so the next call reconnects.
                    *sync::lock(&self.conn, "logind connection") = None;
                    Err(format!("logind Inhibit({what}) failed: {error}"))
                }
            }
        })
    }
}

/// One accepted Inhibit call's bookkeeping.
struct InhibitRecord {
    app_id: String,
    /// The D-Bus unique name of the caller, so a dead client's logind lock can
    /// be released instead of inhibiting the session until the portal exits.
    owner: Option<String>,
    flags: u32,
    /// The held logind lock; `None` for logout/user-switch-only requests
    /// (the tracked no-op).
    lock: Option<OwnedFd>,
    /// Whether the logind lock has been attached. False while the logind call
    /// is in flight, so a `Close` in that window can mark the request cancelled
    /// rather than releasing a lock that does not exist yet.
    active: bool,
}

/// Live inhibit requests keyed by their Request object path, plus the
/// monitor session paths.
#[derive(Default)]
pub(crate) struct InhibitRegistry {
    requests: HashMap<String, InhibitRecord>,
    monitors: HashSet<String>,
    /// Request paths closed after the object was exported but before the
    /// inhibition was stored. Bounded by `MAX_INHIBIT_OBJECTS`.
    early_closes: HashSet<String>,
}

impl InhibitRegistry {
    fn live(&self) -> usize {
        self.requests.len() + self.monitors.len()
    }

    /// Reserve a slot before any external work, so the cap bounds the logind
    /// descriptors taken rather than only those kept.
    fn reserve_request(
        &mut self,
        path: &str,
        app_id: &str,
        owner: Option<String>,
        flags: u32,
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
                owner,
                flags,
                lock: None,
                active: false,
            },
        );
        Ok(())
    }

    /// Attach the logind descriptor and report whether the caller had already
    /// closed the request. One lock acquisition, so no interleaving can leave a
    /// stored inhibition with a marker still pending.
    fn attach_lock(&mut self, path: &str, lock: Option<OwnedFd>) -> bool {
        let closed_early = self.early_closes.remove(path);
        if closed_early {
            drop(lock);
            self.requests.remove(path);
            return true;
        }
        match self.requests.get_mut(path) {
            Some(record) => {
                record.lock = lock;
                record.active = true;
                false
            }
            None => {
                drop(lock);
                true
            }
        }
    }

    /// Record that `path` was closed before its inhibition was stored.
    fn note_early_close(&mut self, path: &str) {
        if self.early_closes.len() >= MAX_INHIBIT_OBJECTS
            && let Some(oldest) = self.early_closes.iter().next().cloned()
        {
            self.early_closes.remove(&oldest);
        }
        self.early_closes.insert(path.to_string());
    }

    /// Release every inhibition taken by `owner`, returning how many were
    /// released. Dropping each record closes its logind descriptor.
    pub(crate) fn release_owned_by(&mut self, owner: &str) -> usize {
        let paths: Vec<String> = self
            .requests
            .iter()
            .filter(|(_, record)| record.owner.as_deref() == Some(owner))
            .map(|(path, _)| path.clone())
            .collect();
        let released = paths.len();
        for path in paths {
            self.requests.remove(&path);
            self.early_closes.remove(&path);
        }
        released
    }

    /// Forget all state for `path`.
    fn forget(&mut self, path: &str) {
        self.requests.remove(path);
        self.early_closes.remove(path);
    }

    fn remove_request(&mut self, path: &str) -> Option<InhibitRecord> {
        let record = self.requests.remove(path)?;
        if !record.active {
            self.note_early_close(path);
        }
        Some(record)
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
/// Validate, reserve a registry slot, and take the inhibition.
///
/// The **reservation comes first**, before the logind call, on purpose: the cap
/// exists to bound how many logind inhibit descriptors this process can hold at
/// once, and checking it after the call would let N concurrent callers each take
/// a descriptor before learning that only `MAX_INHIBIT_OBJECTS` may be kept.
///
/// The registry lock is held only for the check-and-insert, never across the
/// await, so it cannot serialize the logind round trips nor keep a non-`Send`
/// guard inside this future.
async fn apply_inhibit(
    inhibitor: &dyn SystemInhibitor,
    registry: &Mutex<InhibitRegistry>,
    path: &str,
    app_id: &str,
    owner: Option<String>,
    flags: u32,
    reason: Option<String>,
) -> Result<(), String> {
    if flags == 0 || flags & !KNOWN_FLAGS != 0 {
        return Err(format!("unsupported or empty inhibit flags {flags:#x}"));
    }

    // A `Close` that raced ahead of this call is honoured before any logind
    // work: nothing was stored yet, so the marker is all there is.
    if sync::lock(registry, "inhibit registry")
        .early_closes
        .remove(path)
    {
        return Err(format!(
            "request {path} was closed before the inhibition began"
        ));
    }
    sync::lock(registry, "inhibit registry").reserve_request(path, app_id, owner, flags)?;

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
        match inhibitor.inhibit(&what, who, why, "block").await {
            Ok(lock) => Some(lock),
            Err(error) => {
                // Give the reservation back: a refused inhibition must leave no
                // bookkeeping and hold no slot.
                sync::lock(registry, "inhibit registry").forget(path);
                return Err(error);
            }
        }
    };

    // Store the descriptor and settle a `Close` that arrived while logind was
    // answering, under one lock acquisition.
    if sync::lock(registry, "inhibit registry").attach_lock(path, lock) {
        sync::lock(registry, "inhibit registry").forget(path);
        return Err(format!(
            "request {path} was closed before the inhibition completed"
        ));
    }
    Ok(())
}

/// The `reason` option, bounded and NUL-free.
fn parse_reason(options: &HashMap<String, Variant<'_>>) -> Result<Option<String>, String> {
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
    /// Admission: only the portal frontend may call a private backend
    /// (see `atrium_portal_request::caller`).
    pub(crate) callers: Arc<atrium_portal_request::CallerGuard>,
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) registry: Arc<Mutex<InhibitRegistry>>,
    pub(crate) inhibitor: Arc<dyn SystemInhibitor>,
}

impl InhibitIface {
    /// `registry` is shared with the bus-name GC, which releases the logind
    /// locks of a client that leaves the bus; `callers` is the admission guard.
    pub(crate) fn new(
        conn: zbus::Connection,
        tracker: Arc<Mutex<RequestTracker>>,
        registry: Arc<Mutex<InhibitRegistry>>,
        callers: Arc<atrium_portal_request::CallerGuard>,
    ) -> Self {
        Self {
            conn,
            tracker,
            registry,
            callers,
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
        options: HashMap<String, Variant<'_>>,

        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        // Admission first: `app_id` is caller-supplied and unverifiable, so
        // the only sound boundary is "is the sender the portal frontend?".
        // Nothing below may run for any other peer.
        self.callers
            .admit(header.sender().map(|sender| sender.as_str()))
            .map_err(zbus::fdo::Error::from)?;
        let path = handle.as_str().to_string();
        log::info!("portal: Inhibit for '{app_id}' at {path} (flags {flags:#x})");

        let reason = parse_reason(&options).map_err(zbus::fdo::Error::InvalidArgs)?;
        let owner: Option<String> = header.sender().map(|sender| sender.as_str().to_string());
        if let Err(error) = apply_inhibit(
            self.inhibitor.as_ref(),
            &self.registry,
            &path,
            app_id,
            owner,
            flags,
            reason,
        )
        .await
        {
            // Nothing is stored on any failure path, so the object serves
            // nothing: remove it rather than leave a `Close` that releases
            // nothing.
            sync::lock(&self.registry, "inhibit registry").forget(&path);
            if let Err(remove_error) = self
                .conn
                .object_server()
                .remove::<InhibitRequestIface, _>(path.as_str())
                .await
            {
                log::warn!(
                    "portal: could not remove refused inhibit request {path}: {remove_error}"
                );
            }
            return Err(zbus::fdo::Error::Failed(error));
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
                    callers: Arc::clone(&self.callers),
                },
            )
            .await
            .map_err(zbus::fdo::Error::from)?;
        if inserted.is_some() {
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

        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<u32> {
        // Admission first: `app_id` is caller-supplied and unverifiable, so
        // the only sound boundary is "is the sender the portal frontend?".
        // Nothing below may run for any other peer.
        self.callers
            .admit(header.sender().map(|sender| sender.as_str()))
            .map_err(zbus::fdo::Error::from)?;
        let path = handle.as_str().to_string();
        let session_path = session_handle.as_str().to_string();
        log::info!("portal: CreateMonitor for '{app_id}' at {session_path}");

        atrium_portal_request::register(&self.conn, &self.tracker, &path).await?;
        let result = self.create_monitor_inner(&session_path).await;
        atrium_portal_request::finish(&self.conn, &self.tracker, &path).await;
        result
    }

    /// No session manager means no Query End ever arrives; acknowledge the
    /// call for live monitor sessions and reject unknown ones.
    async fn query_end_response(
        &self,
        session_handle: ObjectPath<'_>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        // Admission first: `app_id` is caller-supplied and unverifiable, so
        // the only sound boundary is "is the sender the portal frontend?".
        // Nothing below may run for any other peer.
        self.callers
            .admit(header.sender().map(|sender| sender.as_str()))
            .map_err(zbus::fdo::Error::from)?;
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
                    callers: Arc::clone(&self.callers),
                },
            )
            .await;
        let inserted = match inserted {
            Ok(inserted) => inserted,
            Err(error) => {
                // Release the reserved slot: an export failure must not leave a
                // phantom monitor consuming the cap and answering
                // `QueryEndResponse` for a session that does not exist.
                sync::lock(&self.registry, "inhibit registry").remove_monitor(session_path);
                return Err(zbus::fdo::Error::from(error));
            }
        };
        if inserted.is_some() {
            sync::lock(&self.registry, "inhibit registry").remove_monitor(session_path);
            return Ok(2);
        }
        // The one truthful state this stack can report: Running. Nothing
        // else ever follows (no session manager).
        let state = HashMap::from([("session-state".to_owned(), Variant::from(1u32))]);
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
    /// Admission: only the portal frontend may call a private backend.
    callers: Arc<atrium_portal_request::CallerGuard>,

    path: String,
    conn: zbus::Connection,
    registry: Arc<Mutex<InhibitRegistry>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl InhibitRequestIface {
    async fn close(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        // Admission first: `app_id` is caller-supplied and unverifiable, so
        // the only sound boundary is "is the sender the portal frontend?".
        // Nothing below may run for any other peer.
        self.callers
            .admit(header.sender().map(|sender| sender.as_str()))
            .map_err(zbus::fdo::Error::from)?;
        let released = sync::lock(&self.registry, "inhibit registry").remove_request(&self.path);
        if released.is_none() {
            // Either the request is already gone, or `Inhibit` is still in
            // flight. Marking the close lets that call release the inhibition it
            // is about to store instead of leaking it.
            sync::lock(&self.registry, "inhibit registry").note_early_close(&self.path);
        }
        if let Some(record) = released
            && record.active
        {
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
    /// Admission: only the portal frontend may call a private backend.
    callers: Arc<atrium_portal_request::CallerGuard>,

    path: String,
    conn: zbus::Connection,
    registry: Arc<Mutex<InhibitRegistry>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl InhibitSessionIface {
    async fn close(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        // Admission first: `app_id` is caller-supplied and unverifiable, so
        // the only sound boundary is "is the sender the portal frontend?".
        // Nothing below may run for any other peer.
        self.callers
            .admit(header.sender().map(|sender| sender.as_str()))
            .map_err(zbus::fdo::Error::from)?;
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
        fn inhibit<'a>(
            &'a self,
            what: &'a str,
            who: &'a str,
            why: &'a str,
            mode: &'a str,
        ) -> InhibitFuture<'a> {
            self.calls.lock().unwrap().push((
                what.to_owned(),
                who.to_owned(),
                why.to_owned(),
                mode.to_owned(),
            ));
            let failure = self.fail;
            Box::pin(async move {
                if failure {
                    return Err("fake logind failure".to_owned());
                }
                // A real descriptor, standing in for logind's lock.
                let fd = std::fs::File::open("/dev/null").unwrap();
                Ok(std::os::fd::OwnedFd::from(fd).into())
            })
        }
    }

    /// Drive one `apply_inhibit` future to completion.
    fn apply(
        inhibitor: &dyn SystemInhibitor,
        registry: &Mutex<InhibitRegistry>,
        path: &str,
        app_id: &str,
        flags: u32,
        reason: Option<String>,
    ) -> Result<(), String> {
        atrium_portal_request::block_on(apply_inhibit(
            inhibitor, registry, path, app_id, None, flags, reason,
        ))
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
        let registry = Mutex::new(InhibitRegistry::default());
        assert!(apply(&inhibitor, &registry, "/r/0", "app", 0, None).is_err());
        assert!(apply(&inhibitor, &registry, "/r/x", "app", 0x10, None).is_err());
        assert!(
            sync::lock(&registry, "inhibit registry")
                .requests
                .is_empty()
        );
    }

    #[test]
    fn tracked_no_op_makes_no_logind_call() {
        let inhibitor = fake(false);
        let registry = Mutex::new(InhibitRegistry::default());
        apply(
            &inhibitor,
            &registry,
            "/r/1",
            "org.example.App",
            INHIBIT_LOGOUT | INHIBIT_USER_SWITCH,
            None,
        )
        .unwrap();
        assert!(inhibitor.calls.lock().unwrap().is_empty());
        let record = sync::lock(&registry, "inhibit registry")
            .remove_request("/r/1")
            .unwrap();
        assert!(record.lock.is_none());
        assert_eq!(record.flags, INHIBIT_LOGOUT | INHIBIT_USER_SWITCH);
    }

    #[test]
    fn logind_call_carries_who_why_and_block_mode() {
        let inhibitor = fake(false);
        let registry = Mutex::new(InhibitRegistry::default());
        apply(
            &inhibitor,
            &registry,
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
        assert!(
            sync::lock(&registry, "inhibit registry")
                .remove_request("/r/1")
                .unwrap()
                .lock
                .is_some()
        );
    }

    #[test]
    fn logind_failure_leaves_no_bookkeeping() {
        let inhibitor = fake(true);
        let registry = Mutex::new(InhibitRegistry::default());
        assert!(apply(&inhibitor, &registry, "/r/1", "app", INHIBIT_IDLE, None).is_err());
        assert!(
            sync::lock(&registry, "inhibit registry")
                .requests
                .is_empty()
        );
    }

    #[test]
    fn registry_caps_and_deduplicates_objects() {
        // The registry type itself, without the `Mutex` the server wraps it in.
        let mut registry = InhibitRegistry::default();
        registry
            .reserve_request("/r/dup", "app", None, INHIBIT_IDLE)
            .unwrap();
        assert!(
            registry
                .reserve_request("/r/dup", "app", None, INHIBIT_IDLE)
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
                .reserve_request(&format!("/r/{index}"), "app", None, INHIBIT_IDLE)
                .unwrap();
        }
        assert!(
            registry
                .reserve_request("/r/overflow", "app", None, INHIBIT_IDLE)
                .is_err()
        );
        assert!(registry.add_monitor("/m/overflow").is_err());
    }

    #[test]
    fn logind_connection_configures_method_timeout() {
        let logind = Logind::new();
        // Only run if system bus is available
        if let Ok(conn) = atrium_portal_request::block_on(logind.connection()) {
            assert_eq!(conn.method_timeout(), Some(LOGIND_CALL_TIMEOUT));
        }
    }

    #[test]
    fn reason_option_is_bounded() {
        let options = HashMap::from([("reason".to_owned(), Variant::from("Because"))]);
        assert_eq!(parse_reason(&options).unwrap().as_deref(), Some("Because"));
        assert!(parse_reason(&HashMap::new()).unwrap().is_none());
        let oversized = HashMap::from([(
            "reason".to_owned(),
            Variant::from("x".repeat(MAX_REASON_BYTES + 1)),
        )]);
        assert!(parse_reason(&oversized).is_err());
    }
}
