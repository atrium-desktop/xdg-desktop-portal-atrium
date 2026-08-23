//! `org.freedesktop.impl.portal.Request` objects and their cancellation
//! bookkeeping.
//!
//! The portal frontend passes the exact object path in each backend method's
//! `handle` argument. The backend exports one object at that path for the
//! duration of the method call and removes it before returning the
//! `(response, results)` reply. `Close` records the path in the shared
//! tracker; workers check it before and after interactive work so a racing
//! cancellation answers with response code 1.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use zbus::zvariant::Value;

pub mod sync;

pub type PortalResults = std::collections::HashMap<String, Value<'static>>;
pub type PortalResponse = (u32, PortalResults);
pub type ResponseSender = async_channel::Sender<PortalResponse>;

/// Cancellation state shared between every served `Request` object and the
/// capture worker.
#[derive(Default)]
pub struct RequestTracker {
    /// Request paths currently served (between `register` and `finish`).
    /// The membership test is what makes a late `Close` harmless: the
    /// marker is only recorded for a request that is still in flight.
    active: HashSet<String>,
    closed: HashSet<String>,
}

impl RequestTracker {
    /// Whether `Close` arrived for this request path.
    pub fn was_closed(&self, path: &str) -> bool {
        self.closed.contains(path)
    }

    /// Record the request as served. Called by [`register`] after the
    /// object is exported.
    fn activate(&mut self, path: &str) {
        self.active.insert(path.to_owned());
    }

    /// Record a `Close`, unless the request already finished.
    ///
    /// `Close` and `finish` race in the executor: a client can dispatch
    /// `Close` in the window after the backend already replied and
    /// removed the object. Recording that marker anyway would leave a
    /// string in `closed` that nothing ever forgets (the request's
    /// `finish` has already run), and a *reused* handle would then be
    /// misreported as cancelled. Both operations take this crate's mutex,
    /// so the membership test serializes correctly against `forget`.
    fn mark_closed(&mut self, path: &str) {
        if self.active.contains(path) {
            self.closed.insert(path.to_owned());
        }
    }

    /// Drop all state for a finished request.
    fn forget(&mut self, path: &str) {
        self.active.remove(path);
        self.closed.remove(path);
    }
}

/// The served request object. The portal spec gives it only `Close`.
struct RequestIface {
    path: String,
    tracker: Arc<Mutex<RequestTracker>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestIface {
    async fn close(&self) -> zbus::fdo::Result<()> {
        log::info!("portal: request {} closed by client", self.path);
        sync::lock(&self.tracker, "request tracker").mark_closed(&self.path);
        Ok(())
    }
}

/// Export the backend request object at the exact path supplied by the portal
/// frontend. A duplicate handle is a protocol error rather than an
/// opportunity to share cancellation state between calls.
pub async fn register(
    conn: &zbus::Connection,
    tracker: &Arc<Mutex<RequestTracker>>,
    path: &str,
) -> zbus::fdo::Result<()> {
    let inserted = conn
        .object_server()
        .at(
            path,
            RequestIface {
                path: path.to_string(),
                tracker: Arc::clone(tracker),
            },
        )
        .await
        .map_err(zbus::fdo::Error::from)?;
    if !inserted {
        return Err(zbus::fdo::Error::Failed(format!(
            "request handle {path} is already active"
        )));
    }
    sync::lock(tracker, "request tracker").activate(path);
    Ok(())
}

/// Remove a finished request object and its cancellation marker.
pub async fn finish(conn: &zbus::Connection, tracker: &Arc<Mutex<RequestTracker>>, path: &str) {
    if let Err(error) = conn.object_server().remove::<RequestIface, _>(path).await {
        log::warn!("portal: could not remove request {path}: {error}");
    }
    sync::lock(tracker, "request tracker").forget(path);
}

/// Dispatch one portal request to its interface worker and await the reply.
///
/// This is the shared tail of every backend method that hands work to a
/// worker thread: it exports the `Request` object, creates the one-slot
/// reply channel, enqueues the job, and translates the three ways the
/// handoff can fail into the portal's own vocabulary —
///
/// - a full worker queue answers `(2, {})` (refused), after logging why;
/// - a gone worker surfaces as `zbus::fdo::Error::Failed("{worker} worker
///   is gone")`;
/// - a dropped reply (worker panicked before answering) surfaces as
///   `zbus::fdo::Error::Failed("{worker} worker dropped its response")`.
///
/// `job` builds the interface's job value from the reply sender, keeping
/// the payload fields at the call site. The `Request` object is always
/// removed (and its cancellation marker forgotten) before returning, on
/// every path.
///
/// The sender type is `std::sync::mpsc::SyncSender<J>`: the backend's
/// workers are plain threads fed by bounded sync channels
/// (`MAX_QUEUED_REQUESTS`), whose `try_send` backpressure is exactly the
/// refusal this helper reports.
pub async fn dispatch<J>(
    conn: &zbus::Connection,
    tracker: &Arc<Mutex<RequestTracker>>,
    path: &str,
    worker: &'static str,
    jobs: &std::sync::mpsc::SyncSender<J>,
    job: impl FnOnce(ResponseSender) -> J,
) -> zbus::fdo::Result<PortalResponse> {
    register(conn, tracker, path).await?;
    let (reply, response) = async_channel::bounded(1);
    let queued = jobs.try_send(job(reply));
    if let Err(std::sync::mpsc::TrySendError::Full(_)) = queued {
        log::warn!("portal: refusing {worker} request: worker queue is full");
        finish(conn, tracker, path).await;
        return Ok((2, std::collections::HashMap::new()));
    }
    if queued.is_err() {
        finish(conn, tracker, path).await;
        return Err(zbus::fdo::Error::Failed(format!("{worker} worker is gone")));
    }
    let result = response
        .recv()
        .await
        .map_err(|_| zbus::fdo::Error::Failed(format!("{worker} worker dropped its response")));
    finish(conn, tracker, path).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader};
    use std::process::{Child, Command, Stdio};

    #[test]
    fn tracker_records_and_forgets_closes() {
        let mut tracker = RequestTracker::default();
        assert!(!tracker.was_closed("/r/1"));
        tracker.activate("/r/1");
        tracker.mark_closed("/r/1");
        assert!(tracker.was_closed("/r/1"));
        tracker.forget("/r/1");
        assert!(!tracker.was_closed("/r/1"));
    }

    #[test]
    fn close_marks_the_request_closed() {
        let tracker = Arc::new(Mutex::new(RequestTracker::default()));
        let iface = RequestIface {
            path: "/r/1".to_string(),
            tracker: Arc::clone(&tracker),
        };
        // Close is only meaningful while the request is served.
        sync::lock(&tracker, "test tracker").activate("/r/1");
        zbus::block_on(iface.close()).expect("Close answers Ok");
        assert!(sync::lock(&tracker, "test tracker").was_closed("/r/1"));
    }

    #[test]
    fn a_close_after_finish_is_ignored_and_cannot_poison_a_reused_handle() {
        let mut tracker = RequestTracker::default();
        tracker.activate("/r/race");
        tracker.mark_closed("/r/race");
        tracker.forget("/r/race");
        // A Close dispatched in the window after `finish` removed the
        // object must not leave a marker behind …
        tracker.mark_closed("/r/race");
        assert!(
            !tracker.was_closed("/r/race"),
            "a late Close must not be recorded"
        );
        // … and a later request reusing the handle starts clean.
        tracker.activate("/r/race");
        assert!(
            !tracker.was_closed("/r/race"),
            "a reused handle must not inherit a stale marker"
        );
        // Active bookkeeping is dropped with the request, so repeated
        // handle reuse cannot grow either set.
        tracker.forget("/r/race");
        assert!(!tracker.was_closed("/r/race"));
    }

    /// A private session bus (a spawned `dbus-daemon`), mirroring the
    /// daemon's end-to-end test fixture; killed on drop. `None` when
    /// dbus-daemon is not installed (the bus-dependent tests skip).
    struct PrivateBus {
        address: String,
        child: Child,
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn private_bus() -> Option<PrivateBus> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).ok()?;
        let address = line.trim().to_string();
        (!address.is_empty()).then_some(PrivateBus { address, child })
    }

    fn connect(bus: &PrivateBus) -> zbus::Connection {
        zbus::block_on(async {
            zbus::connection::Builder::address(bus.address.as_str())?
                .build()
                .await
        })
        .expect("connect to the private bus")
    }

    #[test]
    fn duplicate_register_at_the_same_handle_is_an_error() {
        let Some(bus) = private_bus() else {
            eprintln!("skipping: dbus-daemon is not installed");
            return;
        };
        let conn = connect(&bus);
        let tracker = Arc::new(Mutex::new(RequestTracker::default()));
        zbus::block_on(register(&conn, &tracker, "/r/dup")).expect("first register succeeds");
        let error = zbus::block_on(register(&conn, &tracker, "/r/dup"))
            .expect_err("a duplicate handle is a protocol error");
        assert!(
            error.to_string().contains("already active"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn finish_removes_the_object_and_forgets_the_close_marker() {
        let Some(bus) = private_bus() else {
            eprintln!("skipping: dbus-daemon is not installed");
            return;
        };
        let conn = connect(&bus);
        let tracker = Arc::new(Mutex::new(RequestTracker::default()));
        zbus::block_on(register(&conn, &tracker, "/r/fin")).expect("register succeeds");
        sync::lock(&tracker, "test tracker").mark_closed("/r/fin");
        zbus::block_on(finish(&conn, &tracker, "/r/fin"));
        assert!(
            !sync::lock(&tracker, "test tracker").was_closed("/r/fin"),
            "finish drops the cancellation marker"
        );
        // The object is gone from the server, so the handle is free again.
        zbus::block_on(register(&conn, &tracker, "/r/fin")).expect("the handle is reusable");
    }

    /// The job every dispatch test sends: the path it came with, so the
    /// reply can be asserted against the right request.
    #[derive(Debug)]
    struct ProbeJob {
        #[allow(dead_code)]
        path: String,
        reply: ResponseSender,
    }

    fn dispatch_setup(
        tag: &str,
        bound: usize,
    ) -> Option<(
        zbus::Connection,
        Arc<Mutex<RequestTracker>>,
        std::sync::mpsc::SyncSender<ProbeJob>,
    )> {
        let bus = private_bus()?;
        let conn = connect(&bus);
        std::mem::forget(bus); // the connection outlives the daemon child here
        let tracker = Arc::new(Mutex::new(RequestTracker::default()));
        let (tx, _rx) = std::sync::mpsc::sync_channel::<ProbeJob>(bound);
        let _ = tag;
        Some((conn, tracker, tx))
    }

    #[test]
    fn dispatch_answers_the_worker_reply() {
        let Some((conn, _tracker, _tx)) = dispatch_setup("ok", 1) else {
            eprintln!("skipping: dbus-daemon is not installed");
            return;
        };
        // Rebuild the pair with a live receiver the test drains.
        let (tx, rx) = std::sync::mpsc::sync_channel::<ProbeJob>(1);
        let tracker = Arc::new(Mutex::new(RequestTracker::default()));
        let handle = std::thread::spawn(move || {
            let job = rx.recv().expect("the job arrives");
            job.reply
                .send_blocking((0, std::collections::HashMap::new()))
                .expect("the reply sends");
        });
        let response = zbus::block_on(dispatch(&conn, &tracker, "/r/ok", "probe", &tx, |reply| {
            ProbeJob {
                path: "/r/ok".to_string(),
                reply,
            }
        }))
        .expect("a answered dispatch succeeds");
        handle.join().expect("worker thread");
        assert_eq!(response.0, 0);
        // The request object is gone: the handle is reusable.
        zbus::block_on(register(&conn, &tracker, "/r/ok")).expect("the handle is reusable");
    }

    #[test]
    fn dispatch_refuses_with_code_2_when_the_queue_is_full() {
        let Some((conn, _tracker, _tx)) = dispatch_setup("full", 1) else {
            eprintln!("skipping: dbus-daemon is not installed");
            return;
        };
        // A zero-bound channel whose receiver stays alive: one try_send
        // fills it, which is exactly the production backpressure condition.
        let (tx, rx) = std::sync::mpsc::sync_channel::<ProbeJob>(0);
        std::thread::spawn(move || {
            // Hold the receiver open without draining, so sends report
            // Full rather than Disconnected.
            std::thread::park();
            drop(rx);
        });
        let tracker = Arc::new(Mutex::new(RequestTracker::default()));
        let response = zbus::block_on(dispatch(
            &conn,
            &tracker,
            "/r/full",
            "probe",
            &tx,
            |reply| ProbeJob {
                path: "/r/full".to_string(),
                reply,
            },
        ))
        .expect("a full queue is a refusal, not a D-Bus error");
        assert_eq!(response.0, 2, "portal code 2 = refused");
        assert!(response.1.is_empty());
        // The request object was removed before answering.
        zbus::block_on(register(&conn, &tracker, "/r/full")).expect("the handle is reusable");
    }

    #[test]
    fn dispatch_fails_when_the_worker_is_gone() {
        let Some((conn, _t, _tx)) = dispatch_setup("gone", 1) else {
            eprintln!("skipping: dbus-daemon is not installed");
            return;
        };
        // A dropped receiver is a worker that shut down.
        let (tx, rx) = std::sync::mpsc::sync_channel::<ProbeJob>(1);
        drop(rx);
        let tracker = Arc::new(Mutex::new(RequestTracker::default()));
        let error = zbus::block_on(dispatch(
            &conn,
            &tracker,
            "/r/gone",
            "probe",
            &tx,
            |reply| ProbeJob {
                path: "/r/gone".to_string(),
                reply,
            },
        ))
        .expect_err("a gone worker is a D-Bus failure");
        assert!(
            error.to_string().contains("probe worker is gone"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn dispatch_fails_when_the_reply_channel_is_dropped() {
        let Some((conn, _t, _tx)) = dispatch_setup("dropped", 1) else {
            eprintln!("skipping: dbus-daemon is not installed");
            return;
        };
        // A worker that panics before answering drops its reply sender.
        let (tx, rx) = std::sync::mpsc::sync_channel::<ProbeJob>(1);
        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                let _ = job; // dropped without sending: the reply channel closes
            }
        });
        let tracker = Arc::new(Mutex::new(RequestTracker::default()));
        let error = zbus::block_on(dispatch(
            &conn,
            &tracker,
            "/r/drop",
            "probe",
            &tx,
            |reply| ProbeJob {
                path: "/r/drop".to_string(),
                reply,
            },
        ))
        .expect_err("a dropped reply is a D-Bus failure");
        assert!(
            error
                .to_string()
                .contains("probe worker dropped its response"),
            "unexpected error: {error}"
        );
    }
}
