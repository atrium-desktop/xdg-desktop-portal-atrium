//! `xdg-desktop-portal-atrium`: the portal backend for the Tessera compositor.
//!
//! A standalone D-Bus-activated process that bridges the freedesktop portal
//! backend interfaces to the compositor's scoped IPC. Outward it
//! serves `org.freedesktop.impl.portal.Settings` v1,
//! `org.freedesktop.impl.portal.Screenshot` v3,
//! `org.freedesktop.impl.portal.ScreenCast` v6,
//! `org.freedesktop.impl.portal.Secret` v1,
//! `org.freedesktop.impl.portal.Lockdown`,
//! `org.freedesktop.impl.portal.FileChooser`,
//! `org.freedesktop.impl.portal.Email`,
//! `org.freedesktop.impl.portal.Account`,
//! `org.freedesktop.impl.portal.Access` v1,
//! and `org.freedesktop.impl.portal.AppChooser` v4,
//! and `org.freedesktop.impl.portal.OpenURI` v3,
//! and `org.freedesktop.impl.portal.Background` v1,
//! and `org.freedesktop.impl.portal.Inhibit` v3,
//! and `org.freedesktop.impl.portal.Notification` v2,
//! and `org.freedesktop.impl.portal.Wallpaper` v1,
//! and `org.freedesktop.impl.portal.Print` at
//! `/org/freedesktop/portal/desktop` under the well-known name
//! `org.freedesktop.impl.portal.desktop.atrium`. Secret is delegated to the
//! sigil daemon (ADR-0020). FileChooser launches the arca companion prompt
//! binary; no file data crosses compositor IPC. For
//! compositor-owned resources the backend is an ordinary scoped IPC client:
//! pixels come from `CaptureOutput` under the built-in
//! `atrium-portal` named scope with a sealed-memfd blob transfer
//! transport, screencast frames arrive through the same scope's output-frame
//! stream and are republished as a PipeWire producer stream. Account consent,
//! password-mode vault unlock, and target picking are delegated to companion
//! services (sigil, tessera) per ADR-0020 and ADR-0021.
//! No Wayland capture protocol is added anywhere.
//!
//! The process uses zbus's blocking API on the session bus and plain
//! `std::thread` workers without an async runtime. Method dispatch runs on
//! zbus's internal
//! executor; the compositor IPC round-trip (which blocks for up to one frame)
//! happens on a dedicated capture worker so a slow capture never stalls the
//! bus, and every screencast runs its own PipeWire main loop on a dedicated
//! cast thread.

mod companion;
mod fsx;
mod iface;

// The assembly below names each interface by its own module; the `iface`
// parent exists to keep the interface files in one place (ADR-0025 §3).
pub(crate) use iface::{
    access, account, app_chooser, background, email, file_chooser, inhibit, lockdown, notification,
    open_uri, print, screenshot, settings, wallpaper,
};

pub(crate) use fsx::atomic;
mod ipc;
mod screencast;
mod uri;

use std::sync::{Arc, Mutex};

use atrium_portal_request::{CallerGuard, RequestTracker, Supervisor};
use atrium_portal_secret::{SecretError, SecretService};
use screencast::{CastJob, RestoreStore, ScreenCastIface, SessionRegistry};
use screenshot::{CaptureJob, ScreenshotIface};
use settings::{SettingsIface, SettingsStore};

/// The well-known bus name the portal frontend resolves through the
/// `atrium.portal` file's `DBusName`.
pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.atrium";

/// Seed the caller guard with the frontend's current unique name.
///
/// `GetNameOwner` is the authoritative answer at startup, before any
/// `NameOwnerChanged` can be observed. A failure leaves the guard unseeded,
/// which admits nobody: the daemon would rather refuse every call than admit an
/// unverified one.
async fn seed_caller_guard(conn: &zbus::Connection, callers: &CallerGuard) {
    let owner = match zbus::fdo::DBusProxy::new(conn).await {
        Ok(bus) => match atrium_portal_request::FRONTEND_BUS_NAME.try_into().ok() {
            Some(name) => atrium_portal_request::block_on(bus.get_name_owner(name))
                .ok()
                .map(|owner| owner.to_string()),
            None => None,
        },
        Err(error) => {
            log::warn!("portal: system bus unavailable while seeding admission: {error}");
            None
        }
    };
    if owner.is_none() {
        log::warn!(
            "portal: {} is not on the bus yet; admitting no caller until it appears",
            atrium_portal_request::FRONTEND_BUS_NAME
        );
    }
    callers.observe_owner(owner.as_deref());
}
/// The object path every portal backend serves its interfaces at.
pub const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";

/// Errors that prevent the backend from coming up at all.
#[derive(Debug, thiserror::Error)]
pub enum PortalError {
    /// No `$XDG_RUNTIME_DIR`, so the compositor IPC socket cannot be located.
    #[error("$XDG_RUNTIME_DIR is unset; cannot locate tessera.sock")]
    NoRuntimeDir,
    /// Session-bus or object-server setup failed.
    #[error("D-Bus setup failed: {0}")]
    Bus(#[from] zbus::Error),
    /// The advertised Secret interface cannot be backed safely. Refuse a
    /// misleading partial startup so D-Bus activation reports the fault.
    #[error("secret vault setup failed: {0}")]
    Secret(#[from] SecretError),
    /// An essential long-lived worker could not be created. Starting under
    /// the advertised name would otherwise expose a permanently stale or
    /// non-responsive interface.
    #[error("worker setup failed: {0}")]
    Worker(#[source] std::io::Error),
}

/// Spawn one named long-lived worker thread; a spawn failure aborts
/// startup before the bus name is claimed, so D-Bus activation reports
/// the fault instead of exposing a silently unbacked interface. The join
/// handle is dropped deliberately: workers are detached and live as long
/// as the bus connection.
fn spawn_worker(
    name: &'static str,
    task: impl FnOnce() + Send + 'static,
) -> Result<(), PortalError> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(task)
        .map_err(PortalError::Worker)?;
    Ok(())
}

/// Watch `NameOwnerChanged` and reclaim what a vanished client owned: its
/// mode-2 screencast restore tokens, and its ScreenCast sessions.
///
/// Sessions must be reclaimed here and not only through `Session.Close`: a
/// client that crashes never sends `Close`, and its session would otherwise
/// keep a compositor capture running — and one of `MAX_LIVE_CASTS` — until the
/// portal itself restarts.
///
/// A lag on the match stream must not stop the GC: losing it would silently
/// disable both reclamations for the rest of the session, so the stream is
/// re-subscribed instead of abandoned.
fn spawn_restore_watcher(
    conn: zbus::Connection,
    store: Arc<Mutex<RestoreStore>>,
    sessions: Arc<Mutex<SessionRegistry>>,
    jobs: std::sync::mpsc::SyncSender<CastJob>,
    inhibit_registry: Arc<Mutex<inhibit::InhibitRegistry>>,
    callers: Arc<CallerGuard>,
) -> Result<(), PortalError> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .interface("org.freedesktop.DBus")
        .member("NameOwnerChanged")
        .build()?;
    spawn_worker("atrium-portal-restore-gc", move || {
        use futures_util::StreamExt as _;
        loop {
            let stream = atrium_portal_request::block_on(async {
                zbus::MessageStream::for_match_rule(
                    rule.clone(),
                    &conn,
                    std::num::NonZeroUsize::new(64),
                )
                .await
            });
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    // The connection itself is unusable; the process exits for
                    // reactivation through the keep-alive path.
                    log::error!("portal: NameOwnerChanged watch could not start: {error}");
                    return;
                }
            };
            loop {
                let next = atrium_portal_request::block_on(stream.next());
                let Some(Ok(message)) = next else {
                    // A lagged or closed stream: drop it and re-subscribe, so
                    // one burst of bus traffic cannot disable the GC forever.
                    log::warn!("portal: NameOwnerChanged watch restarted after an error");
                    break;
                };
                let Some(signal) = zbus::fdo::NameOwnerChanged::from_message(message) else {
                    continue;
                };
                let Ok(args) = signal.args() else {
                    continue;
                };
                // Keep admission current first: a frontend that gained, lost,
                // or changed its unique name moves the boundary for every
                // interface, including calls already in flight behind this
                // signal.
                if args.name().as_str() == atrium_portal_request::FRONTEND_BUS_NAME {
                    let owner: Option<String> =
                        args.new_owner().as_ref().map(|owner| owner.to_string());
                    callers.observe_owner(owner.as_deref());
                }
                // Only unique-name departures matter below: the name is the
                // owner key, and a well-known name's owner change keeps it.
                if args.new_owner().is_some() || !args.name().as_str().starts_with(':') {
                    continue;
                }
                let owner = args.name().as_str();
                atrium_portal_request::sync::lock(&store, "screencast restore store")
                    .drop_owner(owner);
                // Enqueue the teardown on the screencast worker, which owns
                // the cast threads; `try_send` because this thread must never
                // block on a full queue.
                let orphaned = atrium_portal_request::sync::lock(&sessions, "screencast sessions")
                    .remove_owned_by(owner);
                // Inhibitions hold logind descriptors: a dead client must not
                // keep the session awake or un-suspendable.
                let released =
                    atrium_portal_request::sync::lock(&inhibit_registry, "inhibit registry")
                        .release_owned_by(owner);
                if released > 0 {
                    log::info!(
                        "portal: released {released} inhibition(s) held by departed {owner}"
                    );
                }
                for session_path in orphaned {
                    log::info!(
                        "portal: reclaiming session {session_path}; its owner {owner} left the bus"
                    );
                    if let Err(error) = jobs.try_send(CastJob::SessionEnded {
                        session_path: session_path.clone(),
                    }) {
                        log::error!(
                            "portal: could not enqueue teardown for orphaned {session_path}: {error}"
                        );
                    }
                }
            }
        }
    })
}

/// Run the backend: serve all interfaces on the session bus and spawn the
/// capture and screencast workers. The process is D-Bus-activated,
/// stays resident while the bus is connected, and exits for reactivation
/// when that connection fails.
pub fn run() -> Result<(), PortalError> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or(PortalError::NoRuntimeDir)?;
    let socket = std::path::PathBuf::from(runtime_dir).join("tessera.sock");

    // Global PipeWire setup for every future cast thread (idempotent).
    atrium_portal_cast::init();

    let conn = atrium_portal_request::block_on(async {
        zbus::connection::Builder::session().build().await
    })
    .map_err(PortalError::Bus)?;
    let driver = atrium_portal_request::driver::spawn(conn.clone(), "atrium-portal-bus")
        .map_err(PortalError::Worker)?;

    let tracker = Arc::new(Mutex::new(RequestTracker::default()));
    let sessions = Arc::new(Mutex::new(SessionRegistry::default()));
    let restore_store = Arc::new(Mutex::new(RestoreStore::load(screencast::data_dir())));
    // Every interface worker runs on a supervised, bounded queue (ADR-0028):
    // a panic in one worker restarts it instead of bricking its interface for
    // the life of this D-Bus-activated process.
    let capture_supervisor = Supervisor::<CaptureJob>::new("capture");
    let jobs = capture_supervisor.sender();
    let cast_supervisor = Supervisor::<CastJob>::new("screencast");
    let cast_jobs = cast_supervisor.sender();
    let file_chooser_supervisor = Supervisor::<file_chooser::FileChooserJob>::new("file chooser");
    let file_chooser_jobs = file_chooser_supervisor.sender();
    let account_supervisor = Supervisor::<account::AccountJob>::new("account");
    let account_jobs = account_supervisor.sender();
    let access_supervisor = Supervisor::<access::AccessJob>::new("access");
    let access_jobs = access_supervisor.sender();
    let app_chooser_supervisor = Supervisor::<app_chooser::AppChooserJob>::new("app chooser");
    let app_chooser_jobs = app_chooser_supervisor.sender();
    let open_uri_supervisor = Supervisor::<open_uri::OpenUriJob>::new("open uri");
    let open_uri_jobs = open_uri_supervisor.sender();
    let background_supervisor = Supervisor::<background::BackgroundJob>::new("background");
    let background_jobs = background_supervisor.sender();
    let wallpaper_supervisor = Supervisor::<wallpaper::WallpaperJob>::new("wallpaper");
    let wallpaper_jobs = wallpaper_supervisor.sender();
    let print_supervisor = Supervisor::<print::PrintJob>::new("print");
    let print_jobs = print_supervisor.sender();
    let settings_store = SettingsStore::default();
    settings::prime_store(&socket, &settings_store);

    // Admission: only the portal frontend may call this private backend, since
    // `app_id` is caller-supplied and unverifiable. One guard is shared by
    // every interface, seeded from the bus now and kept current by the name
    // watcher below. Until it knows the frontend it admits nobody (fail closed).
    let callers = CallerGuard::new();
    atrium_portal_request::block_on(seed_caller_guard(&conn, callers.as_ref()));

    // Secret is declared in atrium.portal, so its storage is part of the
    // service's startup contract. Never acquire the bus name with that
    // advertised interface missing. Storage, unlock, and lock state are
    // delegated to the sigil daemon (ADR-0020); this adapter only projects
    // the portal interface onto sigil's native IPC client.
    let secret_service = SecretService::new(Arc::clone(&callers));

    // Serve before requesting the name so no call can arrive at a name we own
    // but do not serve yet (same ordering as the SNI tray watcher).
    {
        let settings_store = settings_store.clone();
        driver.call(move |conn| {
            atrium_portal_request::block_on(async move {
                conn.object_server()
                    .at(DESKTOP_PATH, SettingsIface::new(settings_store))
                    .await
            })
        })?;
    }
    let tracker__ = tracker.clone();
    let jobs__ = jobs.clone();
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    ScreenshotIface {
                        conn: conn.clone(),
                        tracker: tracker__,
                        jobs: jobs__,
                        callers: Arc::clone(&callers__),
                    },
                )
                .await
        })
    })?;
    let tracker__ = tracker.clone();
    let sessions__ = sessions.clone();
    let cast_jobs__ = cast_jobs.clone();
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    ScreenCastIface {
                        conn: conn.clone(),
                        tracker: tracker__,
                        sessions: sessions__,
                        jobs: cast_jobs__,
                        callers: Arc::clone(&callers__),
                    },
                )
                .await
        })
    })?;
    // Stateless sandbox-policy query surface; no worker, no IPC. It serves only
    // properties, which zbus answers without a message header, so admission
    // cannot apply here; the values are sandbox-policy booleans, not secrets.
    driver.call(|conn| {
        atrium_portal_request::block_on(async {
            conn.object_server()
                .at(DESKTOP_PATH, lockdown::LockdownIface::default())
                .await
        })
    })?;
    let tracker__ = tracker.clone();
    let file_chooser_jobs__ = file_chooser_jobs.clone();
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    file_chooser::FileChooserIface {
                        conn: conn.clone(),
                        tracker: tracker__,
                        jobs: file_chooser_jobs__,
                        callers: Arc::clone(&callers__),
                    },
                )
                .await
        })
    })?;
    // Email hand-off is fire-and-forget (no worker, no IPC).
    let tracker__ = tracker.clone();
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    email::EmailIface {
                        conn: conn.clone(),
                        tracker: tracker__,
                        callers: Arc::clone(&callers__),
                    },
                )
                .await
        })
    })?;
    let tracker__ = tracker.clone();
    let account_jobs__ = account_jobs.clone();
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    account::AccountIface {
                        conn: conn.clone(),
                        tracker: tracker__,
                        jobs: account_jobs__,
                        callers: Arc::clone(&callers__),
                    },
                )
                .await
        })
    })?;
    let tracker__ = tracker.clone();
    let access_jobs__ = access_jobs.clone();
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    access::AccessIface {
                        conn: conn.clone(),
                        tracker: tracker__,
                        jobs: access_jobs__,
                        callers: Arc::clone(&callers__),
                    },
                )
                .await
        })
    })?;
    let tracker__ = tracker.clone();
    let app_chooser_jobs__ = app_chooser_jobs.clone();
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    app_chooser::AppChooserIface {
                        conn: conn.clone(),
                        tracker: tracker__,
                        jobs: app_chooser_jobs__,
                        callers: Arc::clone(&callers__),
                    },
                )
                .await
        })
    })?;
    let tracker__ = tracker.clone();
    let open_uri_jobs__ = open_uri_jobs.clone();
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    open_uri::OpenUriIface {
                        conn: conn.clone(),
                        tracker: tracker__,
                        jobs: open_uri_jobs__,
                        callers: Arc::clone(&callers__),
                    },
                )
                .await
        })
    })?;
    let tracker__ = tracker.clone();
    let background_jobs__ = background_jobs.clone();
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    background::BackgroundIface {
                        conn: conn.clone(),
                        tracker: tracker__,
                        jobs: background_jobs__,
                        callers: Arc::clone(&callers__),
                    },
                )
                .await
        })
    })?;
    // Inhibit awaits logind in-method (one fast system-bus round trip); no
    // worker, no compositor IPC. Its registry is shared with the bus-name GC
    // so a client that dies releases the logind locks it took.
    let inhibit_registry = Arc::new(Mutex::new(inhibit::InhibitRegistry::default()));
    let inhibit_registry__ = Arc::clone(&inhibit_registry);
    let callers__ = Arc::clone(&callers);
    let tracker__ = tracker.clone();
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    inhibit::InhibitIface::new(
                        conn.clone(),
                        tracker__,
                        inhibit_registry__,
                        callers__,
                    ),
                )
                .await
        })
    })?;
    // Notification forwards to the compositor's queue (ADR-0026): no worker,
    // no Request objects, and no appearance plumbing -- the compositor skins
    // its own cards from the desktop preferences it already owns.
    let notify_live = Arc::new(std::sync::Mutex::new(
        notification::LiveNotifications::default(),
    ));
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    notification::NotificationIface::new(
                        Arc::clone(&notify_live),
                        Arc::clone(&callers__),
                    ),
                )
                .await
        })
    })?;
    let tracker__ = tracker.clone();
    let wallpaper_jobs__ = wallpaper_jobs.clone();
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    wallpaper::WallpaperIface {
                        conn: conn.clone(),
                        tracker: tracker__,
                        jobs: wallpaper_jobs__,
                        callers: Arc::clone(&callers__),
                    },
                )
                .await
        })
    })?;
    let tracker__ = tracker.clone();
    let print_jobs__ = print_jobs.clone();
    let callers__ = Arc::clone(&callers);
    driver.call(move |conn| {
        atrium_portal_request::block_on(async move {
            conn.object_server()
                .at(
                    DESKTOP_PATH,
                    print::PrintIface {
                        conn: conn.clone(),
                        tracker: tracker__,
                        jobs: print_jobs__,
                        callers: Arc::clone(&callers__),
                    },
                )
                .await
        })
    })?;

    secret_service.register_portal(&driver, &conn, Arc::clone(&tracker), DESKTOP_PATH)?;
    // The sigil round trip can wait for the user to unlock the vault, so it
    // runs on a supervised worker rather than inside the served method (see
    // the `atrium_portal_secret::portal` module docs).
    let secret_tracker = Arc::clone(&tracker);
    spawn_worker("atrium-portal-secret", move || {
        secret_service.serve(Arc::clone(&secret_tracker));
    })?;

    let worker_tracker = Arc::clone(&tracker);
    let worker_socket = socket.clone();
    spawn_worker("atrium-portal-capture", move || {
        capture_supervisor.serve(|queue| {
            screenshot::capture_worker(queue, Arc::clone(&worker_tracker), worker_socket.clone())
        });
    })?;

    let cast_worker_conn = conn.clone();
    let cast_worker_tracker = Arc::clone(&tracker);
    let cast_worker_socket = socket.clone();
    let cast_worker_store = Arc::clone(&restore_store);
    let gc_sessions = Arc::clone(&sessions);
    let gc_jobs = cast_jobs.clone();
    spawn_worker("atrium-portal-screencast", move || {
        cast_supervisor.serve(|cast_rx| {
            screencast::cast_worker(
                cast_rx,
                cast_jobs.clone(),
                cast_worker_conn.clone(),
                Arc::clone(&cast_worker_tracker),
                Arc::clone(&sessions),
                Arc::clone(&cast_worker_store),
                cast_worker_socket.clone(),
            )
        })
    })?;

    // Mode-2 restore tokens live exactly as long as the caller's bus
    // connection, and a ScreenCast session must not outlive it either: reclaim
    // both when the owning unique name vanishes.
    spawn_restore_watcher(
        conn.clone(),
        restore_store,
        gc_sessions,
        gc_jobs,
        inhibit_registry,
        callers,
    )?;

    // FileChooser dispatches one supervised UI task/process per request and
    // never shares the compositor capture worker.
    let file_chooser_tracker = Arc::clone(&tracker);
    spawn_worker("atrium-portal-file-chooser", move || {
        file_chooser_supervisor
            .serve(|rx| file_chooser::file_chooser_worker(rx, Arc::clone(&file_chooser_tracker)))
    })?;

    let account_tracker = Arc::clone(&tracker);
    spawn_worker("atrium-portal-account", move || {
        account_supervisor.serve(|rx| account::account_worker(rx, Arc::clone(&account_tracker)))
    })?;

    let access_tracker = Arc::clone(&tracker);
    spawn_worker("atrium-portal-access", move || {
        access_supervisor.serve(|rx| access::access_worker(rx, Arc::clone(&access_tracker)))
    })?;

    // AppChooser uses the same per-request supervised prompter pattern as
    // Access; candidate resolution happens in the served method.
    let app_chooser_tracker = Arc::clone(&tracker);
    spawn_worker("atrium-portal-app-chooser", move || {
        app_chooser_supervisor
            .serve(|rx| app_chooser::app_chooser_worker(rx, Arc::clone(&app_chooser_tracker)))
    })?;

    // OpenURI launches resolved applications itself and reuses the
    // AppChooser dialog when the user must pick one.
    let open_uri_tracker = Arc::clone(&tracker);
    spawn_worker("atrium-portal-open-uri", move || {
        open_uri_supervisor.serve(|rx| open_uri::open_uri_worker(rx, Arc::clone(&open_uri_tracker)))
    })?;

    // Background consent uses the same supervised confirmation prompt as
    // Access; the autostart entry write happens on the worker task.
    let background_tracker = Arc::clone(&tracker);
    spawn_worker("atrium-portal-background", move || {
        background_supervisor
            .serve(|rx| background::background_worker(rx, Arc::clone(&background_tracker)))
    })?;

    // Wallpaper crosses the compositor IPC and does slow image I/O, so it
    // gets the standard per-request task worker. Staged wallpapers are
    // session artifacts: wipe last session's staging directory now.
    wallpaper::clean_staging();
    let wallpaper_tracker = Arc::clone(&tracker);
    let wallpaper_socket = socket.clone();
    spawn_worker("atrium-portal-wallpaper", move || {
        wallpaper_supervisor.serve(|rx| {
            wallpaper::wallpaper_worker(
                rx,
                Arc::clone(&wallpaper_tracker),
                wallpaper_socket.clone(),
            )
        })
    })?;

    // Print spools the document and hands it to the system `lp` client
    // (the email/xdg-email hand-off precedent).
    let print_tracker = Arc::clone(&tracker);
    spawn_worker("atrium-portal-print", move || {
        print_supervisor.serve(|rx| print::print_worker(rx, Arc::clone(&print_tracker)))
    })?;

    settings::spawn_watcher(conn.clone(), socket, settings_store).map_err(PortalError::Worker)?;

    driver.call(|conn| {
        atrium_portal_request::block_on(async { conn.request_name(BUS_NAME).await })
    })?;

    log::info!(
        "portal: serving Settings+Screenshot+ScreenCast+Secret+Lockdown+FileChooser+Email+Account+Access+AppChooser+OpenURI+Background+Inhibit+Notification+Wallpaper+Print as {BUS_NAME}"
    );

    // Keep the main thread tied to the bus connection. A disconnected
    // backend must exit so D-Bus activation can start a fresh process.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        driver.call(|conn| {
            atrium_portal_request::block_on(async {
                conn.call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus.Peer"),
                    "Ping",
                    &(),
                )
                .await
            })
        })?;
    }
}

#[cfg(test)]
mod integration_metadata_tests {
    const PORTAL_FILE: &str =
        include_str!("../../../contrib/xdg-desktop-portal/portals/atrium.portal");
    const TESSERA_PORTALS_CONF: &str =
        include_str!("../../../contrib/xdg-desktop-portal/tessera-portals.conf");
    const ATRIUM_PORTALS_CONF: &str =
        include_str!("../../../contrib/xdg-desktop-portal/atrium-portals.conf");
    const DBUS_SERVICE: &str = include_str!(
        "../../../contrib/dbus-1/services/org.freedesktop.impl.portal.desktop.atrium.service.in"
    );

    #[test]
    fn portal_metadata_declares_use_in_tessera_and_atrium() {
        let use_in = PORTAL_FILE
            .lines()
            .find_map(|line| line.strip_prefix("UseIn="))
            .expect("portal metadata must declare UseIn");
        let desktops: Vec<_> = use_in
            .split(';')
            .filter(|entry| !entry.is_empty())
            .collect();
        assert!(desktops.contains(&"tessera"), "UseIn must declare tessera");
        assert!(desktops.contains(&"atrium"), "UseIn must declare atrium");
    }

    #[test]
    fn capability_file_advertises_exactly_the_served_interfaces() {
        let interfaces = PORTAL_FILE
            .lines()
            .find_map(|line| line.strip_prefix("Interfaces="))
            .expect("portal metadata must declare Interfaces");
        let advertised: Vec<_> = interfaces
            .split(';')
            .filter(|entry| !entry.is_empty())
            .collect();
        assert_eq!(
            advertised,
            [
                "org.freedesktop.impl.portal.Settings",
                "org.freedesktop.impl.portal.Screenshot",
                "org.freedesktop.impl.portal.ScreenCast",
                "org.freedesktop.impl.portal.Secret",
                "org.freedesktop.impl.portal.Lockdown",
                "org.freedesktop.impl.portal.FileChooser",
                "org.freedesktop.impl.portal.Email",
                "org.freedesktop.impl.portal.Account",
                "org.freedesktop.impl.portal.Access",
                "org.freedesktop.impl.portal.AppChooser",
                "org.freedesktop.impl.portal.OpenURI",
                "org.freedesktop.impl.portal.Background",
                "org.freedesktop.impl.portal.Inhibit",
                "org.freedesktop.impl.portal.Notification",
                "org.freedesktop.impl.portal.Wallpaper",
                "org.freedesktop.impl.portal.Print",
            ]
        );
    }

    #[test]
    fn atrium_is_the_sole_backend_without_a_gtk_fallback() {
        for conf in [TESSERA_PORTALS_CONF, ATRIUM_PORTALS_CONF] {
            assert!(
                conf.lines().any(|line| line == "default=atrium"),
                "the routing default is Tessera alone"
            );
            assert!(
                !conf.to_lowercase().contains("gtk"),
                "no GTK fallback route or comment remains in the routing config"
            );
            for interface in [
                "Settings",
                "Screenshot",
                "ScreenCast",
                "Secret",
                "Lockdown",
                "FileChooser",
                "Email",
                "Account",
                "Access",
                "AppChooser",
                "OpenURI",
                "Background",
                "Inhibit",
                "Notification",
                "Wallpaper",
                "Print",
            ] {
                let route = format!("org.freedesktop.impl.portal.{interface}=atrium");
                assert!(
                    conf.lines().any(|line| line.starts_with(&route)),
                    "missing explicit Tessera route for {interface}"
                );
            }
            // Every interface routes to Tessera alone; no fallback backend exists.
            for line in conf.lines() {
                if line.starts_with("org.freedesktop.") {
                    assert!(
                        line.ends_with("=atrium"),
                        "unexpected non-Tessera route: {line}"
                    );
                }
            }
        }
    }

    #[test]
    fn activation_uses_the_private_packaged_executable() {
        assert!(
            DBUS_SERVICE
                .lines()
                .any(|line| line == "Exec=@libexecdir@/xdg-desktop-portal-atrium")
        );
    }
}
