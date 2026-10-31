//! `org.freedesktop.impl.portal.Session` objects and the cast-session
//! registry behind the ScreenCast portal.
//!
//! One Session object per `CreateSession` call, registered at the exact
//! `session_handle` supplied by the portal frontend. `Close` hands the path to the
//! screencast worker, which stops the cast (if any), emits `Closed`, and
//! removes the object. A cast that ends from the compositor side
//! (`Event::StreamEnded`, disconnect) takes the same teardown path.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

const MAX_SESSIONS: usize = 128;
const MAX_LIVE_CASTS: usize = 16;

/// The source a session is armed with. This is [`atrium_portal_cast::CastSource`]
/// re-exported: the registry and the cast thread must agree on what a source
/// is, and the cast crate owns the definition. Whole-desktop and per-connector
/// monitor captures are persistable; window captures are not (a window id is
/// not stable across sessions, so no restore token is ever issued for one and
/// Start reports `persist_mode` 0).
pub(crate) use atrium_portal_cast::CastSource;

/// The selection `Start` must apply, reserved out of the registry.
pub(crate) struct StartSelection {
    pub(crate) source: CastSource,
    pub(crate) cursor_mode: u32,
    /// Persistence the client asked for; Start reports the granted mode,
    /// which is 0 whenever `persist_grant` is `None`.
    pub(crate) requested_persist_mode: u32,
    /// The restore-token grant decided at `SelectSources`: `(mode, token)`
    /// for both freshly issued and restored tokens.
    pub(crate) persist_grant: Option<(u32, String)>,
}

/// State of one portal screencast session.
pub(crate) struct CastSession {
    /// Application identity supplied by the frontend at `CreateSession`.
    pub(crate) app_id: String,
    /// The D-Bus unique name that created the session, so a vanished owner's
    /// session can be torn down instead of holding a compositor capture.
    pub(crate) owner: Option<String>,
    pub(crate) sources_selected: bool,
    /// The armed source; meaningful once `sources_selected` holds.
    pub(crate) source: CastSource,
    /// The negotiated cursor mode (1 = Hidden, 2 = Embedded).
    pub(crate) cursor_mode: u32,
    /// Persistence requested by the frontend.
    pub(crate) requested_persist_mode: u32,
    /// The restore-token grant from `SelectSources`, if any.
    pub(crate) persist_grant: Option<(u32, String)>,
    /// Reserved before spawning PipeWire negotiation so concurrent Start
    /// calls cannot create two producers for one session.
    pub(crate) starting: bool,
    /// Closing this end makes the cast thread's stop socket readable, which
    /// quits its PipeWire main loop. `None` until `Start` succeeds.
    pub(crate) stop: Option<UnixStream>,
    pub(crate) cast_thread: Option<std::thread::JoinHandle<()>>,
}

/// Live sessions keyed by object path.
#[derive(Default)]
pub(crate) struct SessionRegistry {
    sessions: HashMap<String, CastSession>,
}

impl SessionRegistry {
    /// Register a fresh session. Duplicate paths are refused so a hostile or
    /// buggy client cannot shadow another application's session.
    pub(crate) fn insert(
        &mut self,
        path: &str,
        app_id: &str,
        owner: Option<String>,
    ) -> Result<(), String> {
        if self.sessions.contains_key(path) {
            return Err(format!("session {path} already exists"));
        }
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(format!("session limit of {MAX_SESSIONS} reached"));
        }
        self.sessions.insert(
            path.to_string(),
            CastSession {
                app_id: app_id.to_string(),
                owner,
                sources_selected: false,
                source: CastSource::Monitor { output: None },
                cursor_mode: 1,
                requested_persist_mode: 0,
                persist_grant: None,
                starting: false,
                stop: None,
                cast_thread: None,
            },
        );
        Ok(())
    }

    pub(crate) fn contains(&self, path: &str) -> bool {
        self.sessions.contains_key(path)
    }

    pub(crate) fn mark_sources_selected(
        &mut self,
        path: &str,
        app_id: &str,
        source: CastSource,
        cursor_mode: u32,
        requested_persist_mode: u32,
        persist_grant: Option<(u32, String)>,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(path)
            .ok_or_else(|| format!("unknown session {path}"))?;
        if session.sources_selected {
            return Err(format!("session {path} already selected sources"));
        }
        if session.app_id != app_id {
            return Err(format!("session {path} belongs to another application"));
        }
        session.sources_selected = true;
        session.source = source;
        session.cursor_mode = cursor_mode;
        session.requested_persist_mode = requested_persist_mode;
        session.persist_grant = persist_grant;
        Ok(())
    }

    /// Validate the application owner and return the selection `Start`
    /// must apply.
    pub(crate) fn reserve_start(
        &mut self,
        path: &str,
        app_id: &str,
    ) -> Result<StartSelection, String> {
        let session = self
            .sessions
            .get(path)
            .ok_or_else(|| format!("unknown session {path}"))?;
        if session.app_id != app_id {
            return Err(format!("session {path} belongs to another application"));
        }
        if !session.sources_selected {
            return Err(format!("session {path} has not selected sources"));
        }
        if session.starting || session.cast_thread.is_some() {
            return Err(format!("session {path} already started"));
        }
        let live = self
            .sessions
            .values()
            .filter(|session| session.starting || session.cast_thread.is_some())
            .count();
        if live >= MAX_LIVE_CASTS {
            return Err(format!("live cast limit of {MAX_LIVE_CASTS} reached"));
        }
        let session = self
            .sessions
            .get_mut(path)
            .ok_or_else(|| format!("unknown session {path}"))?;
        session.starting = true;
        Ok(StartSelection {
            source: session.source.clone(),
            cursor_mode: session.cursor_mode,
            requested_persist_mode: session.requested_persist_mode,
            persist_grant: session.persist_grant.clone(),
        })
    }

    pub(crate) fn clear_start(&mut self, path: &str) {
        if let Some(session) = self.sessions.get_mut(path) {
            session.starting = false;
        }
    }

    /// Install a negotiated cast only if the reserved session still exists.
    /// On a raced Session.Close, return ownership so the caller can stop and
    /// join the orphan producer before replying.
    pub(crate) fn mark_started(
        &mut self,
        path: &str,
        stop: UnixStream,
        cast_thread: std::thread::JoinHandle<()>,
    ) -> Result<(), (UnixStream, std::thread::JoinHandle<()>)> {
        if let Some(session) = self.sessions.get_mut(path) {
            session.starting = false;
            session.stop = Some(stop);
            session.cast_thread = Some(cast_thread);
            Ok(())
        } else {
            Err((stop, cast_thread))
        }
    }

    /// Detach a session from the registry. The caller stops/joins its cast
    /// after dropping the registry lock.
    pub(crate) fn remove(&mut self, path: &str) -> Option<CastSession> {
        self.sessions.remove(path)
    }

    /// Detach every session created by `owner`, returning their paths.
    ///
    /// The caller tears each one down after dropping the registry lock, so a
    /// client that disappears cannot hold a compositor capture open for the
    /// rest of the session.
    pub(crate) fn remove_owned_by(&mut self, owner: &str) -> Vec<String> {
        let paths: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.owner.as_deref() == Some(owner))
            .map(|(path, _)| path.clone())
            .collect();
        for path in &paths {
            self.sessions.remove(path);
        }
        paths
    }
}

/// Stop and join a detached cast without holding the session-registry lock.
pub(crate) fn stop_cast(mut session: CastSession) {
    drop(session.stop.take());
    if let Some(thread) = session.cast_thread.take() {
        let _ = thread.join();
    }
}

/// The served session object. The portal spec gives it `Close` and the
/// `Closed` signal; the worker emits the signal after teardown so the
/// frontend observes a fully stopped session.
pub(crate) struct SessionIface {
    /// Admission: only the portal frontend may call a private backend
    /// (see `atrium_portal_request::caller`).
    pub(crate) callers: Arc<atrium_portal_request::CallerGuard>,
    pub(crate) path: String,
    pub(crate) jobs: std::sync::mpsc::SyncSender<crate::screencast::CastJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionIface {
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
        log::info!("portal: session {} closed by client", self.path);
        // `try_send`, never `send`: zbus 6 runs this method as a task on the
        // connection's own runtime, so a blocking send on a full queue would
        // stall every interface sharing the connection until the screencast
        // worker drained a job.
        //
        // Teardown also cannot be dropped, so a full queue is handed to a
        // single dedicated pump instead of being refused: `Close` answers
        // immediately and the session is torn down as soon as there is room.
        let job = crate::screencast::CastJob::CloseSession {
            session_path: self.path.clone(),
        };
        match self.jobs.try_send(job) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(job)) => {
                log::warn!(
                    "portal: screencast queue is full; deferring the close of {}",
                    self.path
                );
                defer_close(self.jobs.clone(), job);
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                log::error!(
                    "portal: screencast worker is gone; cannot close {}",
                    self.path
                );
                Err(zbus::fdo::Error::Failed(
                    "screencast worker is gone".to_string(),
                ))
            }
        }
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }
}

/// Hand a `CloseSession` job to the shared deferred-close pump.
///
/// One process-wide thread blocks on the screencast queue's `send` on behalf of
/// every caller that found the queue full. Being a single thread, a burst of
/// closes cannot grow the process by a thread per call. The pump's own queue is
/// bounded and produced non-blockingly: it runs on the connection's runtime, so
/// a blocking send there would reintroduce the stall it exists to avoid.
fn defer_close(
    jobs: std::sync::mpsc::SyncSender<crate::screencast::CastJob>,
    job: crate::screencast::CastJob,
) {
    use std::sync::OnceLock;

    const PUMP_DEPTH: usize = 64;

    static PUMP: OnceLock<Option<std::sync::mpsc::SyncSender<crate::screencast::CastJob>>> =
        OnceLock::new();
    let pump = PUMP.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<crate::screencast::CastJob>(PUMP_DEPTH);
        let spawned = std::thread::Builder::new()
            .name("atrium-portal-close-pump".to_owned())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    if jobs.send(job).is_err() {
                        log::error!(
                            "portal: screencast worker vanished with deferred closes pending"
                        );
                        return;
                    }
                }
            });
        match spawned {
            Ok(_) => Some(tx),
            Err(error) => {
                log::error!("portal: could not start the deferred-close pump: {error}");
                None
            }
        }
    });
    match pump {
        Some(pump) => match pump.try_send(job) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                log::error!("portal: deferred-close pump is full; a session close was lost");
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                log::error!("portal: the deferred-close pump stopped; a session close was lost");
            }
        },
        None => log::error!("portal: no deferred-close pump; a session close was lost"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with(path: &str) -> SessionRegistry {
        let mut registry = SessionRegistry::default();
        registry.insert(path, "org.example.App", None).unwrap();
        registry
    }

    fn desktop() -> CastSource {
        CastSource::Monitor { output: None }
    }

    #[test]
    fn duplicate_session_paths_are_refused() {
        let mut registry = registry_with("/s/1");
        assert!(registry.contains("/s/1"));
        assert!(registry.insert("/s/1", "org.example.App", None).is_err());
        assert!(registry.insert("/s/2", "org.example.App", None).is_ok());
    }

    #[test]
    fn total_session_count_is_bounded() {
        let mut registry = SessionRegistry::default();
        for index in 0..MAX_SESSIONS {
            registry
                .insert(&format!("/s/{index}"), "org.example.App", None)
                .unwrap();
        }
        assert!(
            registry
                .insert("/s/overflow", "org.example.App", None)
                .is_err()
        );
    }

    #[test]
    fn live_cast_count_is_bounded() {
        let mut registry = SessionRegistry::default();
        for index in 0..=MAX_LIVE_CASTS {
            let path = format!("/s/{index}");
            registry.insert(&path, "org.example.App", None).unwrap();
            registry
                .mark_sources_selected(&path, "org.example.App", desktop(), 1, 0, None)
                .unwrap();
        }
        for index in 0..MAX_LIVE_CASTS {
            registry
                .reserve_start(&format!("/s/{index}"), "org.example.App")
                .unwrap();
        }
        assert!(
            registry
                .reserve_start(&format!("/s/{MAX_LIVE_CASTS}"), "org.example.App")
                .is_err()
        );
    }

    #[test]
    fn host_applications_with_an_empty_frontend_app_id_are_supported() {
        let mut registry = SessionRegistry::default();
        registry.insert("/s/host", "", None).unwrap();
        registry
            .mark_sources_selected("/s/host", "", desktop(), 1, 0, None)
            .unwrap();
        let selection = registry.reserve_start("/s/host", "").unwrap();
        assert_eq!(selection.source, desktop());
        assert_eq!(selection.cursor_mode, 1);
    }

    #[test]
    fn start_requires_selected_sources_and_single_use() {
        let mut registry = registry_with("/s/1");
        assert!(registry.reserve_start("/s/1", "org.example.App").is_err());
        registry
            .mark_sources_selected("/s/1", "org.example.App", desktop(), 1, 0, None)
            .unwrap();
        let selection = registry.reserve_start("/s/1", "org.example.App").unwrap();
        assert_eq!(selection.source, desktop());
        assert_eq!(selection.persist_grant, None);
        registry.clear_start("/s/1");
        assert!(registry.reserve_start("/s/1", "org.example.Other").is_err());
        assert!(
            registry
                .mark_sources_selected("/s/1", "org.example.App", desktop(), 1, 0, None)
                .is_err()
        );

        let (stop, _read) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(|| {});
        registry.reserve_start("/s/1", "org.example.App").unwrap();
        registry.mark_started("/s/1", stop, thread).unwrap();
        assert!(registry.reserve_start("/s/1", "org.example.App").is_err());
    }

    #[test]
    fn remove_stops_and_joins_the_cast() {
        let mut registry = registry_with("/s/1");
        registry
            .mark_sources_selected("/s/1", "org.example.App", desktop(), 1, 0, None)
            .unwrap();
        let (stop, read) = UnixStream::pair().unwrap();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_flag = std::sync::Arc::clone(&flag);
        let thread = std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            use std::io::Read;
            let _ = (&read).read(&mut byte);
            thread_flag.store(true, std::sync::atomic::Ordering::Release);
        });
        registry.reserve_start("/s/1", "org.example.App").unwrap();
        registry.mark_started("/s/1", stop, thread).unwrap();
        let session = registry.remove("/s/1").expect("session was live");
        stop_cast(session);
        assert!(flag.load(std::sync::atomic::Ordering::Acquire));
        assert!(!registry.contains("/s/1"));
    }
}
