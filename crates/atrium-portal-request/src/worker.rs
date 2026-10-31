//! A supervised, bounded job queue for one interface worker.
//!
//! Every backend interface runs its work on a dedicated thread fed by a
//! bounded channel, because the D-Bus methods must answer promptly while a
//! prompt or a capture may block for as long as the user takes.
//!
//! ## Supervision
//!
//! A panic in a worker must not brick its interface. Without supervision the
//! worker's `SyncSender` disconnects when the thread unwinds, and every later
//! request on that interface answers `"<worker> worker is gone"` until the
//! process restarts — which for a D-Bus-activated resident daemon means until
//! the session ends. The supervisor catches the panic, logs it, and runs a
//! fresh worker over a fresh queue, so the next request is served normally.
//!
//! The request that was in flight is *not* retried: its caller has already
//! been told the worker died, and replaying a prompt or a capture could
//! double-publish a stream or a screenshot. Recovery is forward-only.
//!
//! ## Bounded queues
//!
//! Each queue holds at most [`MAX_QUEUED_REQUESTS`] jobs. A full queue is a
//! refusal (`(2, {})` at the portal boundary), not unbounded growth: a caller
//! that spawns-and-forgets saturates its own interface instead of accumulating
//! jobs without end.
//!
//! ## Worker shape
//!
//! A worker is a closure over its own queue: it keeps whatever per-interface
//! state it needs (concurrency counters, worker handles) across jobs, exactly
//! as a hand-written loop did. The supervisor only owns the restart, so
//! converting an interface is mechanical — replace `while let Ok(job) =
//! rx.recv()` with `Supervisor::serve`.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

/// Jobs one worker may hold before new requests are refused.
pub const MAX_QUEUED_REQUESTS: usize = 64;

/// A bounded queue whose worker is restarted if it panics.
///
/// Construct one per interface with [`Supervisor::new`], hand
/// [`Supervisor::sender`] to the assembly code (and to `dispatch`), and call
/// [`Supervisor::serve`] with a closure holding the worker's loop.
#[derive(Debug)]
pub struct Supervisor<J: Send + 'static> {
    name: &'static str,
    jobs: SyncSender<J>,
    queue: Option<Receiver<J>>,
}

impl<J: Send + 'static> Supervisor<J> {
    /// Create a bounded queue for the worker named `name`.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        let (jobs, queue) = sync_channel(MAX_QUEUED_REQUESTS);
        Self {
            name,
            jobs,
            queue: Some(queue),
        }
    }

    /// The sending half to hand to `dispatch` and to the interface struct.
    #[must_use]
    pub fn sender(&self) -> SyncSender<J> {
        self.jobs.clone()
    }

    /// Run `worker` over the queue, restarting it after a panic.
    ///
    /// A worker is called with the receiving half and owns its own job loop,
    /// so it keeps whatever per-interface state it needs (concurrency
    /// counters, worker handles) across jobs — exactly as a hand-written loop
    /// did before supervision existed. `Receiver::recv` needs only `&self`,
    /// which is what lets the supervisor hand the *same* queue back to a
    /// restarted worker instead of losing the channel with the panicked
    /// thread.
    ///
    /// When `worker` returns — every sender is gone — supervision stops.
    ///
    /// # Panics
    ///
    /// Panics only if called twice on the same supervisor, which is a
    /// programming error rather than a runtime condition.
    pub fn serve<F>(self, mut worker: F)
    where
        F: FnMut(&Receiver<J>),
    {
        // Release this supervisor's sender first: holding it would keep the
        // channel open forever, so the worker could never observe the
        // disconnect that ends the loop at shutdown.
        let Self { name, jobs, queue } = self;
        drop(jobs);
        let queue = queue.expect("Supervisor::serve may only be called once");
        loop {
            // `AssertUnwindSafe` is sound because the closure only borrows the
            // receiver and the supervisor's own state is not observed after an
            // unwind — the restart path deliberately starts from scratch.
            let panicked = catch_unwind(AssertUnwindSafe(|| worker(&queue))).is_err();
            if !panicked {
                // The worker drained the queue and every sender is gone.
                return;
            }
            log::error!("portal: the {name} worker panicked; restarting it for the next request");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The queue is bounded: filling it refuses rather than grows.
    #[test]
    fn the_queue_refuses_beyond_its_bound() {
        let supervisor = Supervisor::<()>::new("bounded");
        let sender = supervisor.sender();
        for _ in 0..MAX_QUEUED_REQUESTS {
            sender.try_send(()).expect("within the bound");
        }
        assert!(
            sender.try_send(()).is_err(),
            "the bound must refuse the next job"
        );
    }

    /// Dropping every sender ends the worker loop instead of spinning.
    #[test]
    fn the_worker_returns_when_every_sender_is_dropped() {
        let supervisor = Supervisor::<()>::new("drain");
        let sender = supervisor.sender();
        let worker = std::thread::spawn(move || {
            supervisor.serve(|queue| for () in queue {});
        });
        drop(sender);
        worker.join().expect("the worker loop exits on disconnect");
    }

    /// The property the daemon depends on: after a panic, the *same* queue is
    /// served again, so the request that follows is answered normally instead
    /// of hitting "worker is gone".
    #[test]
    fn a_restarted_worker_serves_the_request_after_the_panic() {
        let supervisor = Supervisor::<usize>::new("restart");
        let sender = supervisor.sender();
        let served = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&served);
        let worker = std::thread::spawn(move || {
            supervisor.serve(move |queue| {
                for job in queue {
                    if job == 0 {
                        panic!("simulated worker panic");
                    }
                    counter.fetch_add(job, Ordering::SeqCst);
                }
            });
        });
        sender.send(0).expect("the panicking job is queued");
        sender.send(7).expect("the following job is queued");
        drop(sender);
        worker
            .join()
            .expect("the supervisor must absorb the worker's panic");
        assert_eq!(
            served.load(Ordering::SeqCst),
            7,
            "the request after the panic must still be served"
        );
    }
}
