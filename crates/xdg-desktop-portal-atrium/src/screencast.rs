//! `org.freedesktop.impl.portal.ScreenCast` v6.
//!
//! The portal frontend supplies Request and Session object paths to the
//! backend. `CreateSession` exports the Session object, `SelectSources`
//! runs the source selection (chooser prompter, optional window pick, and
//! a compositor consent confirmation), and `Start` spawns the cast thread
//! (compositor frame stream → PipeWire producer). Blocking picker and
//! PipeWire work stays on the screencast worker; D-Bus methods
//! asynchronously await the backend `(response, results)` tuple.
//!
//! Capabilities key off the negotiated compositor protocol: against
//! protocol 29 the backend advertises monitor and window sources plus
//! Hidden and Embedded cursor modes; older compositors get monitor-only
//! and Hidden-only. Client source-type masks that offer more than the
//! backend can serve are accepted and served as their supported subset,
//! per the `types`-as-acceptable-set contract. Selection always requires
//! an explicit compositor confirmation identifying the requesting
//! application. A window selection goes through the compositor's
//! interactive toplevel pick; a per-output selection names the connector
//! (protocol 29 fails a connector target closed against older peers, so
//! the option only exists where it can be honored).
//!
//! Persistence follows the v4 contract: with `persist_mode` 1 or 2 and the
//! user's "remember" tick, a monitor selection yields an opaque
//! 128-bit restore token (mode 1 on disk under
//! `$XDG_DATA_HOME/atrium-portal`, mode 2 in memory until the caller's bus
//! name vanishes). A later `SelectSources` presenting a valid token
//! restores the stored selection without any UI; invalid or unservable
//! tokens silently degrade to the normal flow. Window selections are never
//! persisted (window ids are not stable) and report `persist_mode` 0.
//! Version 5's `mapping_id` stream property is optional and omitted
//! because no RemoteDesktop coordinate mapping exists. Version 6's stable
//! PipeWire `object.serial` is resolved from the registry and returned as
//! `pipewire-serial`; Start fails rather than claim v6 without that stable
//! identifier.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use zbus::zvariant::{Array, Dict, ObjectPath, Structure, StructureBuilder, Value};

use crate::cast;
use crate::persist::{RestoreStore, StoredSelection, StoredSource};
use crate::session::{CastSource, SessionIface, SessionRegistry};
use atrium_portal_runtime::{PortalResponse, RequestTracker, ResponseSender, sync};

const SESSION_IFACE: &str = "org.freedesktop.impl.portal.Session";

/// Version 6 adds the stable `pipewire-serial` stream property.
pub(crate) const SCREENCAST_VERSION: u32 = 6;
/// `AvailableSourceTypes` bit: monitor.
const SOURCE_TYPE_MONITOR: u32 = 1;
/// `AvailableSourceTypes` bit: window (protocol 29).
const SOURCE_TYPE_WINDOW: u32 = 2;
/// `AvailableCursorModes` bit: Hidden.
const CURSOR_MODE_HIDDEN: u32 = 1;
/// `AvailableCursorModes` bit: Embedded (protocol 29).
const CURSOR_MODE_EMBEDDED: u32 = 2;
/// The compositor protocol that adds output enumeration, per-output
/// stream targets, window streams, and cursor modes.
const PER_SOURCE_PROTOCOL: u32 = 29;
/// Waiting for the PipeWire negotiation longer than this is a failure.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// One job handed from the bus methods to the screencast worker.
pub(crate) enum CastJob {
    SelectSources {
        request_path: String,
        session_path: String,
        app_id: String,
        source_types: u32,
        cursor_mode: u32,
        persist_mode: u32,
        restore_token: Option<String>,
        /// The caller's D-Bus unique name; mode-2 tokens are keyed by it.
        sender: Option<String>,
        reply: ResponseSender,
    },
    Start {
        request_path: String,
        session_path: String,
        app_id: String,
        reply: ResponseSender,
    },
    /// The client called `Session.Close`.
    CloseSession { session_path: String },
    /// The compositor ended the stream (scope revoked, lease lapsed,
    /// disconnect); reported by the cast thread.
    SessionEnded { session_path: String },
}

/// Options parsed out of the `SelectSources` argument.
pub(crate) struct SelectOptions {
    /// Requested source-type mask; must intersect what we offer.
    pub(crate) source_types: u32,
    pub(crate) cursor_mode: u32,
    pub(crate) persist_mode: u32,
    pub(crate) restore_token: Option<String>,
}

/// Parse `SelectSources` options. Unknown keys are ignored per spec.
pub(crate) fn parse_select_options(options: &HashMap<String, Value<'_>>) -> SelectOptions {
    let source_types = options
        .get("types")
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(SOURCE_TYPE_MONITOR);
    let cursor_mode = options
        .get("cursor_mode")
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(1);
    let persist_mode = options
        .get("persist_mode")
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    let restore_token = options
        .get("restore_token")
        .and_then(|value| String::try_from(value).ok())
        .filter(|token| !token.is_empty());
    SelectOptions {
        source_types,
        cursor_mode,
        persist_mode,
        restore_token,
    }
}

/// Why a `SelectSources` option set cannot be served, as a D-Bus message.
/// `types` is the set the client accepts; the backend may serve any subset,
/// so the mask only needs to intersect what the compositor's negotiated
/// protocol can serve. OBS's unified screen-capture source always offers
/// monitor|window and breaks on a strict equality check. The window bit and
/// the Embedded cursor mode need protocol 29.
/// The protocol-independent shape checks an interface method can run
/// without touching compositor IPC: no undefined source-type bits and a
/// defined persist mode. The capability-dependent half (servable subset,
/// cursor mode) lives in [`validate_select`] and runs on the worker, where
/// the live negotiated protocol is known — see `ipc::cached_protocol_version`.
fn validate_select_shape(options: &SelectOptions) -> Result<(), String> {
    if options.source_types & !(SOURCE_TYPE_MONITOR | SOURCE_TYPE_WINDOW) != 0 {
        return Err(format!(
            "source types {:#b} name undefined bits",
            options.source_types
        ));
    }
    if options.persist_mode > 2 {
        return Err(format!(
            "persist_mode {} is not defined by the ScreenCast contract",
            options.persist_mode
        ));
    }
    Ok(())
}

fn validate_select(options: &SelectOptions, protocol: u32) -> Result<(), String> {
    validate_select_shape(options)?;
    // The mask must intersect what the compositor can serve. A window bit
    // alongside monitor is accepted everywhere and served as monitor on
    // pre-29 compositors; only a mask with no servable subset fails.
    let available = if protocol >= PER_SOURCE_PROTOCOL {
        SOURCE_TYPE_MONITOR | SOURCE_TYPE_WINDOW
    } else {
        SOURCE_TYPE_MONITOR
    };
    if options.source_types & available == 0 {
        return Err(if protocol >= PER_SOURCE_PROTOCOL {
            "only monitor and window sources are supported".to_string()
        } else {
            "only monitor sources are supported".to_string()
        });
    }
    match options.cursor_mode {
        CURSOR_MODE_HIDDEN => {}
        CURSOR_MODE_EMBEDDED if protocol >= PER_SOURCE_PROTOCOL => {}
        other => {
            return Err(format!(
                "cursor_mode {other} is not supported (Hidden{})",
                if protocol >= PER_SOURCE_PROTOCOL {
                    " or Embedded"
                } else {
                    " only"
                }
            ));
        }
    }
    Ok(())
}

/// The served ScreenCast interface. Methods register request/session
/// objects and enqueue; the worker does everything blocking.
pub(crate) struct ScreenCastIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) sessions: Arc<Mutex<SessionRegistry>>,
    pub(crate) jobs: mpsc::SyncSender<CastJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCastIface {
    async fn create_session(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let request_path = handle.as_str().to_string();
        let session_path = session_handle.as_str().to_string();
        atrium_portal_runtime::register(&self.conn, &self.tracker, &request_path).await?;

        if sync::lock(&self.tracker, "screencast tracker").was_closed(&request_path) {
            atrium_portal_runtime::finish(&self.conn, &self.tracker, &request_path).await;
            return Ok((1, HashMap::new()));
        }
        let insert_result =
            { sync::lock(&self.sessions, "screencast sessions").insert(&session_path, app_id) };
        if let Err(error) = insert_result {
            log::warn!("portal: CreateSession refused: {error}");
            atrium_portal_runtime::finish(&self.conn, &self.tracker, &request_path).await;
            return Ok((2, HashMap::new()));
        }
        let inserted = self
            .conn
            .object_server()
            .at(
                session_path.as_str(),
                SessionIface {
                    path: session_path.clone(),
                    jobs: self.jobs.clone(),
                },
            )
            .await;
        let inserted = match inserted {
            Ok(inserted) => inserted,
            Err(error) => {
                let _ = sync::lock(&self.sessions, "screencast sessions").remove(&session_path);
                atrium_portal_runtime::finish(&self.conn, &self.tracker, &request_path).await;
                return Err(zbus::fdo::Error::from(error));
            }
        };
        if !inserted {
            let _ = sync::lock(&self.sessions, "screencast sessions").remove(&session_path);
            atrium_portal_runtime::finish(&self.conn, &self.tracker, &request_path).await;
            return Ok((2, HashMap::new()));
        }
        atrium_portal_runtime::finish(&self.conn, &self.tracker, &request_path).await;
        log::info!("portal: screencast session {session_path} created for '{app_id}'");
        Ok((0, HashMap::new()))
    }

    async fn select_sources(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, Value<'_>>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let session_path = session_handle.as_str().to_string();
        if !sync::lock(&self.sessions, "screencast sessions").contains(&session_path) {
            return Err(zbus::fdo::Error::Failed(format!(
                "unknown session {session_path}"
            )));
        }
        let options = parse_select_options(&options);
        // Only the protocol-independent shape here: the capability half of
        // the validation runs on the worker against the live negotiated
        // protocol (an interface method must not open compositor sockets).
        validate_select_shape(&options).map_err(zbus::fdo::Error::InvalidArgs)?;

        let path = handle.as_str().to_string();
        log::debug!("portal: SelectSources for '{app_id}' on {session_path} at {path}");

        atrium_portal_runtime::register(&self.conn, &self.tracker, &path).await?;
        let (reply, response) = async_channel::bounded(1);
        let queued = self.enqueue(CastJob::SelectSources {
            request_path: path.clone(),
            session_path,
            app_id: app_id.to_string(),
            source_types: options.source_types,
            cursor_mode: options.cursor_mode,
            persist_mode: options.persist_mode,
            restore_token: options.restore_token,
            sender: header.sender().map(|sender| sender.as_str().to_string()),
            reply,
        });
        match queued {
            Ok(true) => {}
            Ok(false) => {
                atrium_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
                return Ok((2, HashMap::new()));
            }
            Err(error) => {
                atrium_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
                return Err(error);
            }
        }
        let result = response.recv().await.map_err(|_| {
            zbus::fdo::Error::Failed("screencast worker dropped its response".to_string())
        });
        atrium_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
        result
    }

    async fn start(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        _options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let session_path = session_handle.as_str().to_string();
        if !sync::lock(&self.sessions, "screencast sessions").contains(&session_path) {
            return Err(zbus::fdo::Error::Failed(format!(
                "unknown session {session_path}"
            )));
        }
        let path = handle.as_str().to_string();
        log::debug!("portal: Start for '{app_id}' on {session_path} at {path}");

        atrium_portal_runtime::register(&self.conn, &self.tracker, &path).await?;
        let (reply, response) = async_channel::bounded(1);
        let queued = self.enqueue(CastJob::Start {
            request_path: path.clone(),
            session_path,
            app_id: app_id.to_string(),
            reply,
        });
        match queued {
            Ok(true) => {}
            Ok(false) => {
                atrium_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
                return Ok((2, HashMap::new()));
            }
            Err(error) => {
                atrium_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
                return Err(error);
            }
        }
        let result = response.recv().await.map_err(|_| {
            zbus::fdo::Error::Failed("screencast worker dropped its response".to_string())
        });
        atrium_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
        result
    }

    #[zbus(property, name = "AvailableSourceTypes")]
    fn available_source_types(&self) -> u32 {
        if self.compositor_version() >= PER_SOURCE_PROTOCOL {
            SOURCE_TYPE_MONITOR | SOURCE_TYPE_WINDOW
        } else {
            SOURCE_TYPE_MONITOR
        }
    }

    #[zbus(property, name = "AvailableCursorModes")]
    fn available_cursor_modes(&self) -> u32 {
        if self.compositor_version() >= PER_SOURCE_PROTOCOL {
            CURSOR_MODE_HIDDEN | CURSOR_MODE_EMBEDDED
        } else {
            CURSOR_MODE_HIDDEN
        }
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        SCREENCAST_VERSION
    }
}

impl ScreenCastIface {
    /// The live negotiated compositor protocol; a compositor that has not
    /// been reached yet reports the conservative minimum, which advertises
    /// monitor sources and the Hidden cursor mode only. Served from the
    /// process-wide cache so property reads never open a socket (see
    /// `ipc::cached_protocol_version`).
    fn compositor_version(&self) -> u32 {
        crate::ipc::cached_protocol_version()
    }

    /// `Ok(false)` is bounded backpressure, reported as portal response 2;
    /// disconnection remains a D-Bus service failure.
    fn enqueue(&self, job: CastJob) -> zbus::fdo::Result<bool> {
        match self.jobs.try_send(job) {
            Ok(()) => Ok(true),
            Err(mpsc::TrySendError::Full(_)) => {
                log::warn!("portal: refusing ScreenCast request: worker queue is full");
                Ok(false)
            }
            Err(mpsc::TrySendError::Disconnected(_)) => Err(zbus::fdo::Error::Failed(
                "screencast worker is gone".to_string(),
            )),
        }
    }
}

/// Dispatch blocking selections and PipeWire negotiations independently.
/// Session close/end events stay on this dispatcher and therefore remain
/// responsive even while another application has a confirmation open.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cast_worker(
    rx: mpsc::Receiver<CastJob>,
    jobs: mpsc::SyncSender<CastJob>,
    conn: zbus::blocking::Connection,
    tracker: Arc<Mutex<RequestTracker>>,
    sessions: Arc<Mutex<SessionRegistry>>,
    restore_store: Arc<Mutex<RestoreStore>>,
    socket: PathBuf,
    settings: crate::settings::SettingsStore,
) {
    const MAX_ACTIVE_CAST_REQUESTS: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(job) = rx.recv() {
        match job {
            CastJob::SelectSources {
                request_path,
                session_path,
                app_id,
                source_types,
                cursor_mode,
                persist_mode,
                restore_token,
                sender,
                reply,
            } => {
                if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                    >= MAX_ACTIVE_CAST_REQUESTS
                {
                    active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                    let _ = reply.send_blocking((2, HashMap::new()));
                    continue;
                }
                let task_tracker = Arc::clone(&tracker);
                let task_sessions = Arc::clone(&sessions);
                let task_store = Arc::clone(&restore_store);
                let task_socket = socket.clone();
                let task_settings = settings.clone();
                let active_guard = ActiveGuard(Arc::clone(&active));
                let spawn_failure_reply = reply.clone();
                if let Err(error) = std::thread::Builder::new()
                    .name("atrium-portal-select-sources".to_owned())
                    .spawn(move || {
                        let _active = active_guard;
                        let mut picker = crate::ipc::PortalCapture::new(task_socket);
                        let code = select_sources(
                            &task_tracker,
                            &task_sessions,
                            &task_store,
                            Some(&task_settings),
                            &mut picker,
                            &request_path,
                            &session_path,
                            &app_id,
                            SelectOptions {
                                source_types,
                                cursor_mode,
                                persist_mode,
                                restore_token,
                            },
                            sender.as_deref(),
                        );
                        log::debug!("portal: SelectSources for '{app_id}' → response {code}");
                        let _ = reply.send_blocking((code, HashMap::new()));
                    })
                {
                    log::error!("portal: could not spawn SelectSources task: {error}");
                    let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
                }
            }
            CastJob::Start {
                request_path,
                session_path,
                app_id,
                reply,
            } => {
                if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                    >= MAX_ACTIVE_CAST_REQUESTS
                {
                    active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                    let _ = reply.send_blocking((2, HashMap::new()));
                    continue;
                }
                let task_tracker = Arc::clone(&tracker);
                let task_sessions = Arc::clone(&sessions);
                let task_jobs = jobs.clone();
                let task_socket = socket.clone();
                let active_guard = ActiveGuard(Arc::clone(&active));
                let spawn_failure_reply = reply.clone();
                if let Err(error) = std::thread::Builder::new()
                    .name("atrium-portal-start-cast".to_owned())
                    .spawn(move || {
                        let _active = active_guard;
                        let response = start_cast(
                            &task_tracker,
                            &task_sessions,
                            &task_jobs,
                            &task_socket,
                            &request_path,
                            &session_path,
                            &app_id,
                        );
                        log::debug!("portal: Start for '{app_id}' → response {}", response.0);
                        let _ = reply.send_blocking(response);
                    })
                {
                    log::error!("portal: could not spawn Start task: {error}");
                    let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
                }
            }
            CastJob::CloseSession { session_path } | CastJob::SessionEnded { session_path } => {
                close_session(&conn, &sessions, &session_path);
            }
        }
    }
}

/// Arm the session. Response codes: 0 ok, 1 cancelled, 2 refused.
#[allow(clippy::too_many_arguments)]
fn select_sources(
    tracker: &Arc<Mutex<RequestTracker>>,
    sessions: &Arc<Mutex<SessionRegistry>>,
    store: &Arc<Mutex<RestoreStore>>,
    settings: Option<&crate::settings::SettingsStore>,
    picker: &mut crate::ipc::PortalCapture,
    request_path: &str,
    session_path: &str,
    app_id: &str,
    options: SelectOptions,
    sender: Option<&str>,
) -> u32 {
    if sync::lock(tracker, "screencast tracker").was_closed(request_path) {
        return 1;
    }
    let protocol = picker
        .protocol_version()
        .unwrap_or(atrium_portal_ipc::MIN_PROTOCOL_VERSION);
    // The capability half of SelectSources validation, against the live
    // negotiated protocol (the interface method checked only the shape).
    if let Err(error) = validate_select(&options, protocol) {
        log::warn!("portal: refusing SelectSources: {error}");
        return 2;
    }

    // A valid restore token skips every dialog: the stored selection is
    // restored as-is and the token stays valid. Anything else — an unknown,
    // mismatched, or unservable token — silently degrades to the normal
    // interactive flow, as the contract permits.
    if options.persist_mode > 0
        && let Some(token) = options.restore_token.as_deref()
    {
        let outputs = enumerate_outputs(picker, protocol);
        let servable = |source: &StoredSource| servable_source(source, protocol, &outputs);
        let restored =
            sync::lock(store, "screencast restore store").validate(token, app_id, &servable);
        sync::lock(store, "screencast restore store").prune(&servable);
        if let Some((mode, selection)) = restored {
            let source = match &selection.source {
                StoredSource::Desktop => CastSource::Monitor { output: None },
                StoredSource::Output { connector } => CastSource::Monitor {
                    output: Some(connector.clone()),
                },
            };
            log::info!("portal: SelectSources for '{app_id}' restored a persisted selection");
            return mark_selected(
                tracker,
                sessions,
                request_path,
                session_path,
                app_id,
                source,
                selection.cursor_mode,
                mode,
                Some((mode, token.to_string())),
            );
        }
    }

    // The chooser's option list: the whole desktop always; one entry per
    // connector when the compositor has several; the interactive window
    // pick when the client accepts window sources. A single resulting
    // option skips the chooser entirely — the common single-monitor case
    // keeps the historical one-dialog flow.
    let mut choices = vec![atrium_portal_prompter::SourceChoice {
        id: "desktop".to_string(),
        label: "Entire desktop".to_string(),
        description: None,
    }];
    let mut window_offered = false;
    if protocol >= PER_SOURCE_PROTOCOL {
        let outputs = enumerate_outputs(picker, protocol);
        if outputs.len() > 1 {
            for output in &outputs {
                choices.push(atrium_portal_prompter::SourceChoice {
                    id: format!("output:{}", output.connector),
                    label: output.connector.clone(),
                    description: Some(format!(
                        "{}×{}{}",
                        output.rect.size.w,
                        output.rect.size.h,
                        if output.primary { ", primary" } else { "" }
                    )),
                });
            }
        }
        if options.source_types & SOURCE_TYPE_WINDOW != 0 {
            window_offered = true;
            choices.push(atrium_portal_prompter::SourceChoice {
                id: "window".to_string(),
                label: "Window…".to_string(),
                description: None,
            });
        }
    }

    let (selected, remember) = if choices.len() == 1 {
        ("desktop".to_string(), false)
    } else {
        match choose_source(
            tracker,
            settings,
            request_path,
            app_id,
            choices,
            options.persist_mode > 0,
        ) {
            Ok(answer) => answer,
            Err(code) => return code,
        }
    };
    if sync::lock(tracker, "screencast tracker").was_closed(request_path) {
        return 1;
    }

    let source = match selected.as_str() {
        "desktop" => CastSource::Monitor { output: None },
        "window" if window_offered => match pick_window(picker) {
            Ok(window) => CastSource::Window { window },
            Err(code) => return code,
        },
        id if id.starts_with("output:") && protocol >= PER_SOURCE_PROTOCOL => CastSource::Monitor {
            output: Some(id.trim_start_matches("output:").to_string()),
        },
        other => {
            log::warn!("portal: source chooser answered an unoffered option {other:?}");
            return 2;
        }
    };

    // Explicit compositor consent naming the concrete target.
    let body = match &source {
        CastSource::Monitor { output: None } => {
            format!("Allow {app_id} to record the entire desktop?")
        }
        CastSource::Monitor {
            output: Some(connector),
        } => format!("Allow {app_id} to record output {connector}?"),
        CastSource::Window { .. } => format!("Allow {app_id} to record the selected window?"),
    };
    match picker.pick_confirm(
        "Share Your Screen".to_string(),
        body,
        Some("Share".to_string()),
    ) {
        Ok(atrium_portal_ipc::ConfirmPickResult::Confirmed) => {}
        Ok(atrium_portal_ipc::ConfirmPickResult::Cancelled) => return 1,
        Err(error) => {
            log::warn!("portal: screen sharing confirmation failed: {error}");
            return 2;
        }
    }
    if sync::lock(tracker, "screencast tracker").was_closed(request_path) {
        return 1;
    }

    // Persistence: only monitor selections are restorable (a window id is
    // not stable), so a remembered window selection never yields a token
    // and Start reports the reduction to persist_mode 0.
    let grant = if remember && options.persist_mode > 0 {
        match &source {
            CastSource::Monitor { output } => {
                let stored = StoredSelection {
                    app_id: app_id.to_string(),
                    source: match output {
                        None => StoredSource::Desktop,
                        Some(connector) => StoredSource::Output {
                            connector: connector.clone(),
                        },
                    },
                    cursor_mode: options.cursor_mode,
                };
                sync::lock(store, "screencast restore store")
                    .issue(options.persist_mode, sender.unwrap_or(""), stored)
                    .map(|token| (options.persist_mode, token))
            }
            CastSource::Window { .. } => None,
        }
    } else {
        None
    };

    mark_selected(
        tracker,
        sessions,
        request_path,
        session_path,
        app_id,
        source,
        options.cursor_mode,
        options.persist_mode,
        grant,
    )
}

/// Record the armed selection on the session. Response codes as above.
#[allow(clippy::too_many_arguments)]
fn mark_selected(
    tracker: &Arc<Mutex<RequestTracker>>,
    sessions: &Arc<Mutex<SessionRegistry>>,
    request_path: &str,
    session_path: &str,
    app_id: &str,
    source: CastSource,
    cursor_mode: u32,
    persist_mode: u32,
    persist_grant: Option<(u32, String)>,
) -> u32 {
    if sync::lock(tracker, "screencast tracker").was_closed(request_path) {
        return 1;
    }
    match sync::lock(sessions, "screencast sessions").mark_sources_selected(
        session_path,
        app_id,
        source,
        cursor_mode,
        persist_mode,
        persist_grant,
    ) {
        Ok(()) => 0,
        Err(error) => {
            log::warn!("portal: SelectSources refused: {error}");
            2
        }
    }
}

/// List the compositor's outputs when the protocol speaks enumeration;
/// any failure (including an older compositor's refusal) is an empty list,
/// which simply yields no per-output chooser entries.
fn enumerate_outputs(
    picker: &mut crate::ipc::PortalCapture,
    protocol: u32,
) -> Vec<atrium_portal_ipc::OutputInfo> {
    if protocol < PER_SOURCE_PROTOCOL {
        return Vec::new();
    }
    picker.enumerate_outputs().unwrap_or_else(|error| {
        log::info!("portal: output enumeration unavailable: {error}");
        Vec::new()
    })
}

/// Whether a stored selection can still be captured: the whole desktop
/// always; a connector only against protocol 29 with the connector present.
fn servable_source(
    source: &StoredSource,
    protocol: u32,
    outputs: &[atrium_portal_ipc::OutputInfo],
) -> bool {
    match source {
        StoredSource::Desktop => true,
        StoredSource::Output { connector } => {
            protocol >= PER_SOURCE_PROTOCOL
                && outputs.iter().any(|output| &output.connector == connector)
        }
    }
}

/// Run the source chooser prompter. Response codes: `Ok` carries the
/// selected option id and the remember tick; `Err(1)` cancels, `Err(2)`
/// is a prompter failure.
fn choose_source(
    tracker: &Arc<Mutex<RequestTracker>>,
    settings: Option<&crate::settings::SettingsStore>,
    request_path: &str,
    app_id: &str,
    options: Vec<atrium_portal_prompter::SourceChoice>,
    remember_offered: bool,
) -> Result<(String, bool), u32> {
    let request = atrium_portal_prompter::ChooseSourceRequest {
        app_id: app_id.to_string(),
        title: "Share Your Screen".to_string(),
        options,
        remember_offered,
        parent_window: None,
    };
    if let Err(error) = request.validate() {
        log::warn!("portal: invalid source chooser request: {error}");
        return Err(2);
    }
    let cancelled = || sync::lock(tracker, "screencast tracker").was_closed(request_path);
    let answered = crate::prompter::invoke(
        atrium_portal_prompter::PrompterRequest::choose_source(request.clone()),
        settings,
        Some(&cancelled),
    );
    match answered {
        Ok(atrium_portal_prompter::PromptResult::ChooseSource(response)) => {
            if cancelled() {
                return Err(1);
            }
            if let Err(error) = response.validate_for(&request) {
                log::warn!("portal: invalid source chooser response: {error}");
                return Err(2);
            }
            match response {
                atrium_portal_prompter::ChooseSourceResponse::Selected { source, remember } => {
                    Ok((source, remember))
                }
                atrium_portal_prompter::ChooseSourceResponse::Cancelled => Err(1),
            }
        }
        Err(crate::prompter::InvokeError::Cancelled) => Err(1),
        Ok(_) => {
            log::warn!("portal: source chooser returned the wrong response kind");
            Err(2)
        }
        Err(crate::prompter::InvokeError::Failed(error)) => {
            log::warn!("portal: source chooser failed: {error}");
            Err(2)
        }
    }
}

/// Pick one toplevel through compositor chrome.
fn pick_window(picker: &mut crate::ipc::PortalCapture) -> Result<atrium_portal_ipc::WindowId, u32> {
    match picker.pick(atrium_portal_ipc::PickKind::Window) {
        Ok(atrium_portal_ipc::PickResult::Window { id }) => Ok(id),
        Ok(atrium_portal_ipc::PickResult::Cancelled) => Err(1),
        Ok(other) => {
            log::warn!("portal: window pick answered an unexpected result: {other:?}");
            Err(2)
        }
        Err(error) => {
            log::warn!("portal: window pick failed: {error}");
            Err(2)
        }
    }
}

/// Spawn the cast and report the negotiated stream. Response codes follow
/// the portal spec (0 ok, 1 cancelled, 2 error).
#[allow(clippy::too_many_arguments)]
fn start_cast(
    tracker: &Arc<Mutex<RequestTracker>>,
    sessions: &Arc<Mutex<SessionRegistry>>,
    jobs: &mpsc::SyncSender<CastJob>,
    socket: &std::path::Path,
    request_path: &str,
    session_path: &str,
    app_id: &str,
) -> (u32, HashMap<String, Value<'static>>) {
    if sync::lock(tracker, "screencast tracker").was_closed(request_path) {
        return (1, HashMap::new());
    }
    let selection = {
        let mut sessions = sync::lock(sessions, "screencast sessions");
        match sessions.reserve_start(session_path, app_id) {
            Ok(selection) => selection,
            Err(error) => {
                log::warn!("portal: Start refused: {error}");
                return (2, HashMap::new());
            }
        }
    };
    let cursor = match selection.cursor_mode {
        CURSOR_MODE_EMBEDDED => atrium_portal_ipc::StreamCursorMode::Embedded,
        _ => atrium_portal_ipc::StreamCursorMode::Hidden,
    };
    // The cast thread reports compositor-side stream ends back to this
    // worker through a clone of the worker's own job channel.
    let handle = match cast::spawn(
        socket.to_path_buf(),
        session_path.to_string(),
        jobs.clone(),
        selection.source.clone(),
        cursor,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            sync::lock(sessions, "screencast sessions").clear_start(session_path);
            log::warn!("portal: could not spawn cast for {session_path}: {error}");
            return (2, HashMap::new());
        }
    };
    match handle.started.recv_timeout(START_TIMEOUT) {
        Ok(Ok(started)) => {
            // A Close racing the negotiation wins over a started cast.
            if sync::lock(tracker, "screencast tracker").was_closed(request_path) {
                drop(handle.stop);
                let _ = handle.thread.join();
                sync::lock(sessions, "screencast sessions").clear_start(session_path);
                return (1, HashMap::new());
            }
            if let Err((stop, thread)) = sync::lock(sessions, "screencast sessions").mark_started(
                session_path,
                handle.stop,
                handle.thread,
            ) {
                drop(stop);
                let _ = thread.join();
                return (1, HashMap::new());
            }
            log::info!(
                "portal: cast for {session_path} live as pipewire node {} serial {} ({}x{}, {:?})",
                started.node_id,
                started.serial,
                started.width,
                started.height,
                selection.source
            );
            let source_type = match &selection.source {
                CastSource::Monitor { .. } => SOURCE_TYPE_MONITOR,
                CastSource::Window { .. } => SOURCE_TYPE_WINDOW,
            };
            let mut results = HashMap::from([(
                "streams".to_string(),
                streams_value(
                    started.node_id,
                    started.serial,
                    source_type,
                    started.position,
                    (started.width as i32, started.height as i32),
                ),
            )]);
            match &selection.persist_grant {
                // A granted or restored token is reported with its mode.
                Some((mode, token)) => {
                    results.insert("persist_mode".to_string(), Value::from(*mode));
                    results.insert("restore_token".to_string(), Value::from(token.clone()));
                }
                // Omitting this would make the frontend assume the
                // requested nonzero mode was granted. Report the safe
                // reduction (no remember tick, or an unpersistable
                // selection such as a window).
                None if selection.requested_persist_mode != 0 => {
                    results.insert("persist_mode".to_string(), Value::from(0_u32));
                }
                None => {}
            }
            (0, results)
        }
        Ok(Err(error)) => {
            log::warn!("portal: cast for {session_path} failed: {error}");
            drop(handle.stop);
            let _ = handle.thread.join();
            sync::lock(sessions, "screencast sessions").clear_start(session_path);
            (2, HashMap::new())
        }
        Err(_) => {
            log::warn!("portal: cast for {session_path} timed out during negotiation");
            drop(handle.stop);
            let _ = handle.thread.join();
            sync::lock(sessions, "screencast sessions").clear_start(session_path);
            (2, HashMap::new())
        }
    }
}

/// Stop the cast (if any), emit `Closed`, and remove the session object.
fn close_session(
    conn: &zbus::blocking::Connection,
    sessions: &Arc<Mutex<SessionRegistry>>,
    session_path: &str,
) {
    let Some(session) = sync::lock(sessions, "screencast sessions").remove(session_path) else {
        return;
    };
    crate::session::stop_cast(session);
    log::debug!("portal: screencast session {session_path} closed");
    if let Err(error) = conn.emit_signal(None::<&str>, session_path, SESSION_IFACE, "Closed", &()) {
        log::warn!("portal: could not emit Closed for {session_path}: {error}");
    }
    if let Err(error) = conn.object_server().remove::<SessionIface, _>(session_path) {
        log::warn!("portal: could not remove {session_path}: {error}");
    }
}

/// The `streams` result: `a(ua{sv})` with the PipeWire node id, the
/// source's position and size in compositor coordinates, and its source
/// type (monitor or window).
fn streams_value(
    node_id: u32,
    pipewire_serial: u64,
    source_type: u32,
    position: (i32, i32),
    size: (i32, i32),
) -> Value<'static> {
    let properties: HashMap<String, Value> = HashMap::from([
        (
            "position".to_string(),
            Value::Structure(Structure::from(position)),
        ),
        ("size".to_string(), Value::Structure(Structure::from(size))),
        ("source_type".to_string(), Value::U32(source_type)),
        ("pipewire-serial".to_string(), Value::U64(pipewire_serial)),
    ]);
    // `append_field` keeps each field's dynamic signature, so the structure
    // types as `(ua{sv})`; `Structure::from` would route the fields through
    // `Value::new` and wrap them as variants (`(vv)`).
    let stream = StructureBuilder::new()
        .append_field(Value::U32(node_id))
        .append_field(Value::Dict(Dict::from(properties)))
        .build()
        .expect("non-empty structure");
    // The array must carry the element signature `(ua{sv})` — building it
    // from a `Vec<Value>` would type it as `av`, which the frontend cannot
    // deserialize as the spec's `a(ua{sv})`.
    let mut streams =
        Array::new(&zbus::zvariant::Signature::try_from("(ua{sv})").expect("valid signature"));
    streams
        .append(Value::Structure(stream))
        .expect("stream element matches");
    Value::Array(streams)
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
    fn select_options_default_to_monitor_with_hidden_cursor() {
        let parsed = parse_select_options(&HashMap::new());
        assert_eq!(parsed.source_types, SOURCE_TYPE_MONITOR);
        assert_eq!(parsed.cursor_mode, 1);
        assert_eq!(parsed.persist_mode, 0);
        assert_eq!(parsed.restore_token, None);
        assert!(validate_select(&parsed, 24).is_ok());
        assert!(validate_select(&parsed, 29).is_ok());
    }

    #[test]
    fn select_shape_rejects_protocol_independent_garbage() {
        // Undefined source-type bits are malformed on every protocol.
        let mut garbage = parse_select_options(&HashMap::new());
        garbage.source_types |= 1 << 9;
        assert!(validate_select_shape(&garbage).is_err());
        assert!(validate_select(&garbage, 29).is_err());
        // An undefined persist mode likewise.
        let mut persist = parse_select_options(&HashMap::new());
        persist.persist_mode = 3;
        assert!(validate_select_shape(&persist).is_err());
        assert!(validate_select(&persist, 29).is_err());
        // The shape check is protocol-free: a monitor-only mask passes it
        // even where the capability half would still reject other masks.
        let monitor_only = parse_select_options(&HashMap::new());
        assert!(validate_select_shape(&monitor_only).is_ok());
    }

    #[test]
    fn select_options_accept_monitor_and_window_mix() {
        // Clients such as OBS's unified screen capture offer every type they
        // can take; serving the supported subset is the contract.
        let parsed = parse_select_options(&options(&[("types", Value::from(0b11u32))]));
        assert!(validate_select(&parsed, 24).is_ok());
        assert!(validate_select(&parsed, 29).is_ok());
    }

    #[test]
    fn embedded_cursor_mode_needs_protocol_29() {
        for unsupported in [0u32, 3, 4, 5] {
            let parsed =
                parse_select_options(&options(&[("cursor_mode", Value::from(unsupported))]));
            assert!(validate_select(&parsed, 29).is_err());
        }
        let parsed = parse_select_options(&options(&[("cursor_mode", Value::from(2u32))]));
        assert!(validate_select(&parsed, 28).is_err());
        assert!(validate_select(&parsed, 29).is_ok());
    }

    #[test]
    fn window_only_sources_need_protocol_29() {
        let window_only = parse_select_options(&options(&[("types", Value::from(0b10u32))]));
        assert_eq!(window_only.source_types, 2);
        assert!(validate_select(&window_only, 28).is_err());
        assert!(validate_select(&window_only, 29).is_ok());
    }

    #[test]
    fn select_options_refuse_unknown_source_bits() {
        let unknown = parse_select_options(&options(&[("types", Value::from(0b100u32))]));
        assert!(validate_select(&unknown, 29).is_err());
        let empty = parse_select_options(&options(&[("types", Value::from(0u32))]));
        assert!(validate_select(&empty, 29).is_err());
    }

    #[test]
    fn restore_token_is_parsed_and_empty_tokens_ignored() {
        let parsed = parse_select_options(&options(&[(
            "restore_token",
            Value::from("0123456789abcdef0123456789abcdef"),
        )]));
        assert_eq!(
            parsed.restore_token.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        let parsed = parse_select_options(&options(&[("restore_token", Value::from(""))]));
        assert_eq!(parsed.restore_token, None);
        let parsed = parse_select_options(&options(&[("restore_token", Value::from(7u32))]));
        assert_eq!(parsed.restore_token, None);
    }

    #[test]
    fn screencast_version_is_6() {
        assert_eq!(SCREENCAST_VERSION, 6);
    }

    #[test]
    fn persist_modes_are_parsed_and_unknown_values_refused() {
        for mode in 0_u32..=2 {
            let parsed = parse_select_options(&options(&[("persist_mode", Value::from(mode))]));
            assert_eq!(parsed.persist_mode, mode);
            assert!(validate_select(&parsed, 29).is_ok());
        }
        let parsed = parse_select_options(&options(&[("persist_mode", Value::from(3_u32))]));
        assert!(validate_select(&parsed, 29).is_err());
    }

    #[test]
    fn select_options_ignore_wrong_types() {
        let parsed = parse_select_options(&options(&[("types", Value::from("monitor"))]));
        assert!(validate_select(&parsed, 29).is_ok());
    }

    #[test]
    fn streams_value_has_portal_shape() {
        let value = streams_value(42, 9001, SOURCE_TYPE_MONITOR, (0, 0), (1920, 1080));
        let Value::Array(array) = &value else {
            panic!("streams must be an array");
        };
        assert_eq!(array.len(), 1);
        // Signature: a(ua{sv}) — the frontend's deserialize expects exactly
        // this shape, so pin it.
        assert_eq!(
            value.value_signature().to_string(),
            "a(ua{sv})",
            "streams signature"
        );
        let stream: Value = array.get(0).expect("read").expect("one stream");
        let Value::Structure(stream) = stream else {
            panic!("stream element must be a structure");
        };
        let Value::Dict(properties) = &stream.fields()[1] else {
            panic!("stream properties must be a dict");
        };
        assert!(properties.iter().any(|(key, value)| {
            matches!(key, Value::Str(key) if key.as_str() == "pipewire-serial")
                && matches!(value, Value::Value(value) if **value == Value::U64(9001))
        }));
    }
}
