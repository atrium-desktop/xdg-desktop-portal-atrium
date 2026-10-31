//! Driving zbus 6's async API from this daemon's thread model.
//!
//! zbus 6 removed the blocking API (`zbus::blocking`, `*ProxyBlocking`, the
//! `blocking-api` feature); the replacement is `zbus::block_on`, whose contract
//! shapes the whole daemon:
//!
//! - A connection's work — reading the socket, dispatching method calls,
//!   polling timers — runs **on the thread that is inside `zbus::block_on`**,
//!   between polls of the future. A connection therefore makes progress only
//!   while some thread drives it.
//! - `zbus::block_on` must not be called from inside a task zbus is running (an
//!   interface method, or a future inside another `block_on`): it panics.
//!
//! This daemon is thread-per-interface with synchronous methods, so an
//! interface method cannot drive the connection itself. The shape that follows:
//!
//! - one **driver thread** per connection sits in `block_on` for the life of
//!   the process, which is what keeps the object server and signals moving;
//! - interface methods hand work to that thread through [`Driver::call`] and
//!   block until it answers.
//!
//! Calling `block_on` per method instead would not work: the future that waits
//! for a reply also has to poll the connection that would deliver it, and the
//! work can only run between polls of the outermost future.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use zbus::Connection;

/// Work to run against a live connection on the driver thread.
type Job = Box<dyn FnOnce(&Connection) + Send + 'static>;

/// A handle that runs closures on the thread holding the connection.
#[derive(Clone)]
pub struct Driver {
    jobs: async_channel::Sender<Job>,
    thread: Arc<AtomicU64>,
}

impl std::fmt::Debug for Driver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Driver").finish_non_exhaustive()
    }
}

/// Two threads never share a `ThreadId` value, and `u64::MAX` means "no thread
/// yet": the driver publishes its id before it starts serving jobs.
const NO_THREAD: u64 = u64::MAX;

fn thread_id() -> u64 {
    // `ThreadId::as_u64` is unstable, so derive a stable id from the OS thread
    // handle instead. `gettid` is what the kernel uses and is stable for the
    // thread's life.
    // SAFETY: `gettid` takes no arguments and only reads the caller's own id.
    let raw = unsafe { libc::gettid() };
    u64::try_from(raw).unwrap_or(u64::MAX - 1)
}

impl Driver {
    /// Run `job` on the driver thread and wait for its result.
    ///
    /// # Panics
    ///
    /// Panics when called from the driver thread itself: that would wait for
    /// the very thread it is on, which can never answer. zbus reports the same
    /// situation (calling `block_on` inside an interface method) the same way.
    pub fn call<R, F>(&self, job: F) -> R
    where
        F: FnOnce(&Connection) -> R + Send + 'static,
        R: Send + 'static,
    {
        assert_ne!(
            self.thread.load(Ordering::Acquire),
            thread_id(),
            "Driver::call must not be invoked from the connection's own driver thread; \
             an interface method cannot synchronously wait for the connection that runs it"
        );
        let (reply, response) = std::sync::mpsc::sync_channel(1);
        let wrapped: Job = Box::new(move |connection| {
            let _ = reply.send(job(connection));
        });
        self.jobs
            .send_blocking(wrapped)
            .expect("the connection driver has stopped");
        response
            .recv()
            .expect("the connection driver dropped a job without answering")
    }

    /// Whether the caller is already on the driver thread.
    #[must_use]
    pub fn is_self(&self) -> bool {
        self.thread.load(Ordering::Acquire) == thread_id()
    }
}

/// Spawn the thread that drives `connection`, returning a handle to it.
///
/// The driver runs until every [`Driver`] handle is dropped.
pub fn spawn(connection: Connection, name: &str) -> std::io::Result<Driver> {
    let (jobs, queue) = async_channel::bounded::<Job>(64);
    let thread = Arc::new(AtomicU64::new(NO_THREAD));
    let published = Arc::clone(&thread);
    let ready = std::sync::mpsc::sync_channel(0);
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            published.store(thread_id(), Ordering::Release);
            let _ = ready.0.send(());
            // One `block_on` for the thread's whole life: the connection's I/O
            // reactor runs between polls of this future.
            zbus::block_on(async {
                while let Ok(job) = queue.recv().await {
                    job(&connection);
                }
            });
        })?;
    // Do not hand out a handle whose thread id is not published yet.
    let _ = ready.1.recv();
    Ok(Driver { jobs, thread })
}

/// Drive `future` to completion on the calling thread.
///
/// The caller must not already be on a driver thread; see the module docs.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    zbus::block_on(future)
}
