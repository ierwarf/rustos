//! Blocking `wait4` rendezvous: a parent parks until a child changes state.
//!
//! - **Owner:** `kernel-ps` owns the waiter table; the wait policy (which child,
//!   which options, `EINTR`) stays with the `wait4` caller and `procd`.
//! - **Boundary:** An entry is a wake hint keyed by parent PID. It grants no
//!   authority; the woken task re-runs `wait_for_child`, which alone consumes a
//!   child's status.
//! - **Lifecycle:** register (after arming the block) -> re-check the child table
//!   -> commit the block -> unregister. State producers wake after publishing.
//! - **Concurrency:** Lock-free fixed table. Both sides separate their store and
//!   their load with a SeqCst fence, so either the producer sees the registered
//!   waiter or the waiter's re-check sees the published state.
//! - **Failure:** A full table makes `register` fail and the caller falls back to
//!   yield polling; a spurious or stale wake costs one extra re-check.
//! - **Forbidden:** Consuming child status here, blocking, allocating, or
//!   taking a lock: producers run under the process-table leaf's caller context.
//! - **Evidence:** `core_lifecycle` fork+wait latency; `wait4` blocking witness.
use core::sync::atomic::{fence, AtomicU64, Ordering};

const CAPACITY: usize = 64;

struct Slot {
    task: AtomicU64,
    parent: AtomicU64,
}

static WAITERS: [Slot; CAPACITY] = [const {
    Slot {
        task: AtomicU64::new(0),
        parent: AtomicU64::new(0),
    }
}; CAPACITY];

/// Publishes `task_id` as waiting for a child of `parent_pid`. The caller must
/// have armed its block already and must re-check the child table afterwards.
pub fn register_child_waiter(parent_pid: u64, task_id: u64) -> bool {
    if task_id == 0 {
        return false;
    }
    for slot in &WAITERS {
        if slot
            .task
            .compare_exchange(0, task_id, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            slot.parent.store(parent_pid, Ordering::Release);
            // Pairs with the fence in `wake_child_waiters`.
            fence(Ordering::SeqCst);
            return true;
        }
    }
    false
}

/// Withdraws every entry of `task_id`.
pub fn unregister_child_waiter(task_id: u64) {
    for slot in &WAITERS {
        if slot.task.load(Ordering::Relaxed) == task_id {
            let _ = slot
                .task
                .compare_exchange(task_id, 0, Ordering::Release, Ordering::Relaxed);
        }
    }
}

/// Wakes parked parents after a child's status or job-control state changed.
pub fn wake_child_waiters(parent_pid: u64) {
    if parent_pid == 0 {
        return;
    }
    fence(Ordering::SeqCst);
    for slot in &WAITERS {
        let task = slot.task.load(Ordering::Acquire);
        if task != 0 && slot.parent.load(Ordering::Acquire) == parent_pid {
            let _ = super::wake_task(task);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn register_unregister_round_trip_frees_the_slot() {
        let _guard = TEST_LOCK.lock().unwrap();
        assert!(register_child_waiter(7, 0xfeed_0001));
        assert!(WAITERS
            .iter()
            .any(|slot| slot.task.load(Ordering::Relaxed) == 0xfeed_0001));
        unregister_child_waiter(0xfeed_0001);
        assert!(WAITERS
            .iter()
            .all(|slot| slot.task.load(Ordering::Relaxed) != 0xfeed_0001));
    }

    #[test]
    fn zero_task_is_never_registered() {
        let _guard = TEST_LOCK.lock().unwrap();
        assert!(!register_child_waiter(7, 0));
    }

    #[test]
    fn table_exhaustion_is_reported_not_overwritten() {
        let _guard = TEST_LOCK.lock().unwrap();
        let base = 0xfeed_1000;
        let mut taken = 0;
        for i in 0..CAPACITY as u64 {
            if register_child_waiter(9, base + i) {
                taken += 1;
            }
        }
        assert!(taken > 0);
        let overflow_ok = register_child_waiter(9, base + CAPACITY as u64 + 1);
        for i in 0..=CAPACITY as u64 + 1 {
            unregister_child_waiter(base + i);
        }
        if taken == CAPACITY {
            assert!(!overflow_ok);
        }
    }
}
