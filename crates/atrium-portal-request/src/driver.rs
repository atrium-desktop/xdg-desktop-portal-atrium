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

/// How long a caller waits for the driver thread before declaring it wedged.
///
/// The driver runs connection work that zbus does not bound (`request_name`,
/// name lookups, signal emission). Without a ceiling, one wedged job would
/// pin the caller — and, for the assembly path, the whole process — while the
/// daemon still owned its bus name, so clients would hang instead of being
/// told the portal is unavailable.
const DRIVER_JOB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl Driver {
    /// Run `job` on the driver thread and wait for its result.
    ///
    /// # Panics
    ///
    /// Panics when called from the driver thread itself: that would wait for
    /// the very thread it is on, which can never answer. zbus reports the same
    /// situation (calling `block_on` inside an interface method) the same way.
    /// Also panics when the driver does not answer within its job timeout: the
    /// connection thread is wedged, and continuing as if it worked would place
    /// a caller's work behind a queue that never drains.
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
        // Enqueue on a bounded budget too. The driver is wedged precisely when
        // its queue is not draining, and `send_blocking` on a full queue would
        // then block here forever -- making the timeout below unreachable and
        // hanging the caller (and, on the assembly path, the whole process)
        // while it still owned its bus name.
        //
        // `async_channel` has no timed send, so the bound is expressed by
        // splitting the budget: wait for room for up to `enqueue`, then wait
        // for the answer for whatever remains.
        let started = std::time::Instant::now();
        let mut enqueue_failed = wrapped;
        loop {
            match self.jobs.try_send(enqueue_failed) {
                Ok(()) => break,
                Err(async_channel::TrySendError::Closed(_)) => {
                    panic!("the connection driver has stopped")
                }
                Err(async_channel::TrySendError::Full(job)) => {
                    enqueue_failed = job;
                    if started.elapsed() >= DRIVER_JOB_TIMEOUT {
                        panic!(
                            "the connection driver queue did not accept a job within \
                             {DRIVER_JOB_TIMEOUT:?}; its connection work is wedged"
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        let remaining = DRIVER_JOB_TIMEOUT.saturating_sub(started.elapsed());
        response.recv_timeout(remaining).unwrap_or_else(|_| {
            panic!(
                "the connection driver thread did not answer within {DRIVER_JOB_TIMEOUT:?}; \
                 its connection work is wedged"
            )
        })
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
                    // A job that panics must not take the driver thread with
                    // it: the thread is the connection's only pump, so losing
                    // it would strand every interface at once. Contain the
                    // unwind and keep pumping; the caller whose job panicked
                    // observes a dropped reply on its own side.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        job(&connection);
                    }))
                    .is_err()
                    {
                        log::error!("the connection driver job panicked; the driver continues");
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A panicking job must not kill the driver thread: it is the connection's
    /// only pump, so losing it strands every interface sharing the connection.
    #[test]
    fn a_panicking_job_leaves_the_driver_usable() {
        let Some(bus) = private_bus() else {
            eprintln!("skipping: dbus-daemon is not installed");
            return;
        };
        let connection = block_on(zbus::connection::Builder::address(bus.address.as_str()).build())
            .expect("connect to the private bus");
        let driver = spawn(connection, "driver-panic-test").expect("spawn the driver");

        // A job that panics. `Driver::call` must still return afterwards.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            driver.call(|_| panic!("job panics on purpose"));
        }));
        assert!(panicked.is_err(), "the caller observes the panic");

        // The driver is expected to still serve jobs. Give it a moment: the
        // catch_unwind above happens on this thread, but the driver's own
        // isolation runs on its thread.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let value = driver.call(|_| 42_u32);
        assert_eq!(value, 42, "the driver must keep pumping after a job panic");
    }

    /// A private `dbus-daemon` that dies with the test binary.
    ///
    /// The child is explicitly killed and reaped on drop (the tidy path), and
    /// carries `PR_SET_PDEATHSIG` so the kernel still reaps it when this
    /// binary is killed without unwinding — a timeout or a `SIGKILL` from CI.
    /// Both halves are required by the repository's process-hygiene rule.
    struct PrivateBus {
        address: String,
        child: std::process::Child,
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Start a private `dbus-daemon` and return it, or `None` when the
    /// binary is not installed.
    fn private_bus() -> Option<PrivateBus> {
        use std::io::{BufRead as _, BufReader};
        use std::os::unix::process::CommandExt as _;
        use std::process::{Command, Stdio};

        let mut command = Command::new("dbus-daemon");
        command
            .args(["--session", "--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: the closure runs in the forked child before `exec` and
        // calls only async-signal-safe functions (`prctl`, `getppid`,
        // `_exit`), touching no memory shared with the parent beyond the
        // return value.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() == 1 {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let mut child = command.spawn().ok()?;
        let stdout = child.stdout.take()?;
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).ok()?;
        let address = line.trim().to_owned();
        if address.is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        Some(PrivateBus { address, child })
    }
}
