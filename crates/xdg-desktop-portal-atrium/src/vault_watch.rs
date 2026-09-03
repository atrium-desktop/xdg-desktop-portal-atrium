//! Session-lock watcher: bind the secret vault's lock state to the
//! desktop's authoritative lock boundary (ADR-0019).
//!
//! The backend subscribes to logind on the **system** bus:
//!
//! - `org.freedesktop.login1.Session.Lock` — emitted when the session
//!   was locked (`LockSession()` called, typically by the idle policy
//!   once the secure lock frame is confirmed). Locks the vault and
//!   zeroizes the master key.
//! - `org.freedesktop.login1.Session.Unlock` — the session returned.
//!   A keyfile-mode vault is re-unlocked from `vault.key`; a
//!   password-mode vault waits for a PAM token or the masked prompt.
//! - `org.freedesktop.login1.Manager.PrepareForSleep` — suspend keeps
//!   the same discipline as a screen lock (the machine leaves the
//!   user's possession with secrets in RAM): lock going in, re-unlock
//!   keyfile vaults coming out. Password-mode vaults come back locked
//!   and are re-opened by the PAM token a committing unlock plants.
//!
//! The signals carry no payload identifying the session, so the watcher
//! resolves its session object path once at startup — `$XDG_SESSION_ID`
//! through `GetSession` first, `GetSessionByPID(self)` as the fallback
//! — and matches every signal's path against it. Without a session
//! (nested or remote environments) the watcher disables itself and the
//! vault simply keeps its startup state, which is the documented
//! fallback for the whole logind integration.
//!
//! If the system bus is unavailable the watcher retries in the
//! background; the vault stays at its startup state until logind
//! appears, exactly like the inhibit integration's lazy connection.

use std::sync::Arc;

use atrium_portal_secret::SecretService;

const MANAGER_DESTINATION: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER_INTERFACE: &str = "org.freedesktop.login1.Manager";
const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Spawn the session-lock watcher on its own worker thread.
///
/// Never fails startup: a missing system bus or session is logged once
/// and retried; the vault keeps its startup lock state meanwhile.
pub(crate) fn spawn_session_lock_watcher(service: Arc<SecretService>) {
    let spawned = std::thread::Builder::new()
        .name("atrium-portal-session-lock".to_owned())
        .spawn(move || watch(service));
    if let Err(error) = spawned {
        log::error!("portal: could not start the session-lock watcher: {error}");
    }
}

fn watch(service: Arc<SecretService>) {
    loop {
        match run_once(&service) {
            // The stream ended (logind restarted); reconnect.
            Ok(()) => {
                log::info!("portal: logind session signals ended; reconnecting");
            }
            Err(error) => {
                log::warn!("portal: logind session watcher unavailable: {error}");
            }
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
}

type WatchError = String;

/// Resolve this process's logind session and consume lock signals until
/// the stream ends. Errors before the stream starts (no system bus, no
/// session) disable the watcher for one retry interval.
fn run_once(service: &Arc<SecretService>) -> Result<(), WatchError> {
    let connection = zbus::blocking::Connection::system().map_err(|e| e.to_string())?;

    let session_path = resolve_session_path(&connection)?;
    log::info!(
        "portal: vault lock policy follows logind session {} ({} mode)",
        session_path,
        if service.is_keyfile_mode() {
            "keyfile"
        } else {
            "password"
        }
    );

    // One broad rule on the logind sender keeps the subscription to a
    // single match rule (the blocking iterator takes exactly one); member
    // and path are filtered in the dispatch below.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(MANAGER_DESTINATION)
        .map_err(|e| e.to_string())?
        .build();

    let mut messages = zbus::blocking::MessageIterator::for_match_rule(rule, &connection, Some(16))
        .map_err(|e| e.to_string())?;

    for message in messages.by_ref() {
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                log::warn!("portal: session-lock signal stream error: {error}");
                break;
            }
        };
        // Match this session's Lock/Unlock exactly by path; sleep events
        // are manager-wide.
        let header = message.header();
        let (Some(path), Some(member)) = (header.path(), header.member()) else {
            continue;
        };
        let member = member.as_str();
        let interface = header.interface().map(|i| i.as_str().to_owned());
        let is_session = *path == *session_path
            && interface.as_deref() == Some("org.freedesktop.login1.Session");
        match member {
            "Lock" if is_session => service.lock_for_session(),
            "Unlock" if is_session => service.unlock_for_session(),
            "PrepareForSleep" => {
                let Ok(preparing) = message.body().deserialize::<bool>() else {
                    continue;
                };
                if preparing {
                    service.lock_for_session();
                } else {
                    service.unlock_for_session();
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Resolve the caller's own logind session object path.
fn resolve_session_path(
    connection: &zbus::blocking::Connection,
) -> Result<zbus::zvariant::OwnedObjectPath, WatchError> {
    // Prefer $XDG_SESSION_ID (imported into the user manager's — and thus
    // the D-Bus activation — environment by tessera-session): it names the
    // *graphical* session even for a D-Bus-activated process that lives in
    // the user manager's cgroup, where GetSessionByPID resolves to the
    // class=manager session — which logind exempts from locking
    // (CanLock=false, LockSession() not-supported). The ID is validated by
    // round-tripping through Manager.GetSession, so a stale inherited
    // variable cannot point the watcher at a foreign session.
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.is_empty()
        && let Ok(reply) = connection.call_method(
            Some(MANAGER_DESTINATION),
            MANAGER_PATH,
            Some(MANAGER_INTERFACE),
            "GetSession",
            &(&id,),
        )
        && let Ok(path) = reply
            .body()
            .deserialize::<zbus::zvariant::OwnedObjectPath>()
    {
        return Ok(path);
    }
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.is_empty()
    {
        log::warn!(
            "portal: $XDG_SESSION_ID='{id}' did not resolve; falling back to GetSessionByPID"
        );
    }
    let reply = connection
        .call_method(
            Some(MANAGER_DESTINATION),
            MANAGER_PATH,
            Some(MANAGER_INTERFACE),
            "GetSessionByPID",
            &(std::process::id()),
        )
        .map_err(|e| format!("GetSessionByPID failed: {e}"))?;
    reply
        .body()
        .deserialize::<zbus::zvariant::OwnedObjectPath>()
        .map_err(|e| format!("malformed GetSessionByPID reply: {e}"))
}
