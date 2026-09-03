//! Poison-tolerant `Mutex`/`RwLock` acquisition for the daemon's shared
//! bookkeeping state.
//!
//! Policy: these locks guard pure bookkeeping (request-cancellation
//! markers, session registries, cached settings, lazy connections) whose
//! mutations are short, total operations — insert, remove, replace — that
//! leave the state self-consistent for the next caller even when a holder
//! panics mid-task. A panicking worker must not take down a
//! D-Bus-activated daemon: the process is meant to stay resident for the
//! next request, and `std::sync`'s default poison-and-cascade behavior
//! would turn one faulting task into a permanently crashing service once
//! any later `.lock().unwrap()` re-panicked on the poisoned lock. The
//! helpers below therefore recover the inner state from the `PoisonError`
//! and log one warning naming the lock per recovery, so the incident is
//! visible without killing the process.
//!
//! Do not use these helpers where a partially applied mutation would be
//! corrupting (multi-step invariants spanning several fields); keep the
//! crash (or restructure the state) there instead.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock `mutex`, recovering the inner state if a previous holder panicked
/// (see the module docs for the policy). `name` identifies the lock in the
/// recovery warning.
pub fn lock<'a, T>(mutex: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| recover(name, poisoned))
}

/// Read-lock `lock`, recovering the inner state if a previous writer
/// panicked (see the module docs for the policy). `name` identifies the
/// lock in the recovery warning.
pub fn read_lock<'a, T>(lock: &'a RwLock<T>, name: &'static str) -> RwLockReadGuard<'a, T> {
    lock.read()
        .unwrap_or_else(|poisoned| recover(name, poisoned))
}

/// Write-lock `lock`, recovering the inner state if a previous holder
/// panicked (see the module docs for the policy). `name` identifies the
/// lock in the recovery warning.
pub fn write_lock<'a, T>(lock: &'a RwLock<T>, name: &'static str) -> RwLockWriteGuard<'a, T> {
    lock.write()
        .unwrap_or_else(|poisoned| recover(name, poisoned))
}

/// Extract the guard from a poison error, logging once per recovery. The
/// lock stays poisoned, so every later acquisition through these helpers
/// re-warns rather than hiding a recurring fault.
fn recover<T>(name: &'static str, poisoned: PoisonError<T>) -> T {
    log::warn!("sync: lock '{name}' was poisoned by a panicking holder; recovered its inner state");
    poisoned.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;

    #[test]
    fn poisoned_mutex_still_locks_through_the_helper_and_recovers_state() {
        let mutex = Arc::new(Mutex::new(vec![1, 2, 3]));
        let panicking = Arc::clone(&mutex);
        let panicked = catch_unwind(AssertUnwindSafe(move || {
            let mut guard = panicking.lock().unwrap();
            guard.push(4);
            panic!("simulated worker fault");
        }));
        assert!(panicked.is_err());
        assert!(mutex.is_poisoned());

        // The helper recovers: the mutation that completed before the
        // panic is visible, and the guard is fully usable afterwards.
        let mut guard = lock(&mutex, "test mutex");
        guard.push(5);
        assert_eq!(guard.as_slice(), [1, 2, 3, 4, 5]);
        drop(guard);

        // Recovery does not clear the poison flag; later acquisitions
        // still go through the helper (and re-warn).
        assert!(mutex.is_poisoned());
        assert_eq!(lock(&mutex, "test mutex").as_slice(), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn poisoned_rwlock_recovers_through_read_and_write_helpers() {
        let lock = Arc::new(RwLock::new(1_u32));
        let panicking = Arc::clone(&lock);
        let panicked = catch_unwind(AssertUnwindSafe(move || {
            let mut guard = panicking.write().unwrap();
            *guard = 2;
            panic!("simulated worker fault");
        }));
        assert!(panicked.is_err());
        assert!(lock.is_poisoned());

        assert_eq!(*read_lock(&lock, "test rwlock"), 2);
        *write_lock(&lock, "test rwlock") = 3;
        assert_eq!(*read_lock(&lock, "test rwlock"), 3);
    }
}
