//! Caller admission: which D-Bus peer may invoke these interfaces at all.
//!
//! ## Why this exists
//!
//! Every backend method takes `app_id` as a plain string argument. Nothing in
//! the protocol proves it, so an untrusted caller can name any application it
//! likes. That is not cosmetic: `app_id` is rendered into compositor consent
//! text ("Allow org.mozilla.firefox to record the entire desktop?"), it keys
//! `Background`'s autostart file, it binds ScreenCast restore tokens, and — the
//! sharp end — it is the subject the Secret portal derives a per-application
//! master secret for. A same-user process that could call this backend
//! directly could ask for another application's master secret by typing its
//! name, and read every credential that application stored.
//!
//! ## The boundary
//!
//! `org.freedesktop.impl.portal.desktop.atrium` is a **private backend**. Its
//! only legitimate client is the portal frontend, `org.freedesktop.portal.Desktop`,
//! which resolves the sandboxed application's identity itself (it has the
//! sandbox metadata this process does not) and forwards the resulting `app_id`.
//!
//! Admission is therefore expressed as: *the sender's unique bus name must be
//! the one currently owned by the frontend.* Applications reach the portal
//! through the frontend's public interface, never by calling the backend.
//!
//! This holds without trusting `app_id` at all, which is what makes it sound:
//! the decision depends only on the message header's sender, and the header is
//! set by the bus daemon, not by the caller.
//!
//! ## Why it is a set lookup and not a bus round trip
//!
//! The check runs inside interface methods, which zbus 6 dispatches as tasks on
//! the connection's own runtime (see ADR-0029). Any bus call there would have
//! to be awaited, and a blocking one would panic. Admission is therefore a pure
//! in-memory comparison against a cache the daemon maintains from
//! `NameOwnerChanged`, so it can never block and never re-enters the connection.
//!
//! ## Failing closed
//!
//! Before the frontend has appeared, or after it has gone, there is no owner to
//! compare against and **every** call is refused. A backend that admitted
//! callers while the frontend was absent would be exploitable precisely in the
//! window where nothing is watching it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The well-known name of the only legitimate client of a private backend.
pub const FRONTEND_BUS_NAME: &str = "org.freedesktop.portal.Desktop";

/// Why a call was refused. Rendered into the D-Bus error so the failure is
/// diagnosable from a client log rather than presenting as a timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The frontend is not on the bus, so there is nothing to admit against.
    NoFrontend,
    /// The sender is not the frontend's current unique name.
    NotTheFrontend,
    /// The message carried no sender at all, which a real method call cannot.
    NoSender,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFrontend => write!(
                formatter,
                "{FRONTEND_BUS_NAME} does not own its name; the private backend admits no caller \
                 until the portal frontend is present"
            ),
            Self::NotTheFrontend => write!(
                formatter,
                "this is a private backend; only {FRONTEND_BUS_NAME} may call it. Applications \
                 must use the public org.freedesktop.portal.Desktop interface, which resolves \
                 their identity before forwarding the request"
            ),
            Self::NoSender => write!(formatter, "the call carried no sender bus name"),
        }
    }
}

impl From<Refusal> for zbus::fdo::Error {
    fn from(refusal: Refusal) -> Self {
        zbus::fdo::Error::AccessDenied(refusal.to_string())
    }
}

/// The frontend's current unique bus name, maintained from `NameOwnerChanged`.
///
/// One `CallerGuard` is shared by every interface, so they cannot disagree
/// about who the frontend is.
#[derive(Debug, Default)]
pub struct CallerGuard {
    frontend: Mutex<Option<String>>,
    /// Set once the guard has been seeded from the bus, so an early refusal is
    /// distinguishable from "not yet asked".
    seeded: AtomicBool,
}

impl CallerGuard {
    /// A guard that admits nobody until [`Self::observe_owner`] says otherwise.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record the frontend's current unique name (`None` when it has none).
    ///
    /// Called by the name watcher: at startup with the answer to
    /// `GetNameOwner`, and afterwards for every `NameOwnerChanged`.
    pub fn observe_owner(&self, owner: Option<&str>) {
        let mut slot = crate::sync::lock(&self.frontend, "caller guard");
        *slot = owner.map(str::to_owned);
        self.seeded.store(true, Ordering::Release);
        if let Some(owner) = owner {
            log::info!("portal: admitting calls from {FRONTEND_BUS_NAME} ({owner}) only");
        } else {
            log::info!("portal: {FRONTEND_BUS_NAME} left the bus; refusing every caller");
        }
    }

    /// Whether the guard has been seeded at least once.
    #[must_use]
    pub fn is_seeded(&self) -> bool {
        self.seeded.load(Ordering::Acquire)
    }

    /// Admit a call from `sender`, or explain the refusal.
    ///
    /// `sender` is the message header's sender: the bus daemon sets it, so it
    /// cannot be forged by the caller.
    pub fn admit(&self, sender: Option<&str>) -> Result<(), Refusal> {
        let Some(sender) = sender else {
            return Err(Refusal::NoSender);
        };
        let frontend = crate::sync::lock(&self.frontend, "caller guard");
        match frontend.as_deref() {
            None => Err(Refusal::NoFrontend),
            Some(owner) if owner == sender => Ok(()),
            Some(_) => Err(Refusal::NotTheFrontend),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_admitted_before_the_frontend_appears() {
        let guard = CallerGuard::new();
        assert!(!guard.is_seeded());
        assert_eq!(guard.admit(Some(":1.5")), Err(Refusal::NoFrontend));
        assert_eq!(guard.admit(None), Err(Refusal::NoSender));
    }

    #[test]
    fn only_the_frontends_current_unique_name_is_admitted() {
        let guard = CallerGuard::new();
        guard.observe_owner(Some(":1.5"));
        assert!(guard.admit(Some(":1.5")).is_ok());
        // An application reaching the private backend directly is refused,
        // which is the whole point: it would otherwise choose its own app_id.
        assert_eq!(guard.admit(Some(":1.9")), Err(Refusal::NotTheFrontend));
    }

    /// A frontend restart invalidates the old unique name: the guard must
    /// follow the name, not remember the first owner forever.
    #[test]
    fn a_frontend_restart_moves_the_boundary() {
        let guard = CallerGuard::new();
        guard.observe_owner(Some(":1.5"));
        guard.observe_owner(Some(":1.11"));
        assert_eq!(guard.admit(Some(":1.5")), Err(Refusal::NotTheFrontend));
        assert!(guard.admit(Some(":1.11")).is_ok());
    }

    /// The window after the frontend leaves must admit nobody.
    #[test]
    fn a_departed_frontend_admits_nobody() {
        let guard = CallerGuard::new();
        guard.observe_owner(Some(":1.5"));
        guard.observe_owner(None);
        assert_eq!(guard.admit(Some(":1.5")), Err(Refusal::NoFrontend));
    }

    #[test]
    fn the_refusal_explains_how_to_call_correctly() {
        let text = Refusal::NotTheFrontend.to_string();
        assert!(text.contains(FRONTEND_BUS_NAME));
        assert!(text.contains("public"));
    }
}
