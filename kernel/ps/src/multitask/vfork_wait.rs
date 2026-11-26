//! Blocking `vfork` parent-thread completion rendezvous.
//!
//! - **Owner:** `kernel-ps` owns the completion registry and wakeup sequencing.
//! - **Boundary:** `vfork` suspends only the calling `parent_tid` (never the
//!   whole parent process and never inside single-threaded `procd`).
//! - **Lifecycle:** `begin_vfork_wait` (registered before child activation) ->
//!   child runs sharing parent MM -> `complete_vfork` on child `exec` commit,
//!   exit status, exiting mark, or final thread detach -> parent consumes
//!   completion (`take_vfork_done`) or cleans up (`cancel_vfork_wait`).
//! - **Concurrency:** Lock-free fixed table matching scheduler capacity with
//!   `SeqCst` fences pairing producer completion against consumer arm/recheck.

use core::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};
use super::slot_dir::AtomicDir;

const CHUNK_LEN: usize = 64;
const MAX_CHUNKS: usize = 4096;
const RESERVED_SLOT: u64 = u64::MAX;

struct Slot {
    child_pid: AtomicU64,
    parent_tid: AtomicU64,
    done: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            child_pid: AtomicU64::new(0),
            parent_tid: AtomicU64::new(0),
            done: AtomicU64::new(0),
        }
    }
}

static COMPLETIONS: AtomicDir<Slot, CHUNK_LEN, MAX_CHUNKS> = AtomicDir::new();
static NEXT_HIGH_WATER: AtomicUsize = AtomicUsize::new(0);

fn try_claim_slot(child_pid: u64, parent_tid: u64) -> bool {
    // ORDERING: Acquire observes allocated slots published prior to scan.
    let len = NEXT_HIGH_WATER.load(Ordering::Acquire);
    for idx in 0..len {
        let Some(slot) = COMPLETIONS.get(idx) else {
            continue;
        };
        if slot
            .child_pid
            .compare_exchange(0, RESERVED_SLOT, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            slot.parent_tid.store(parent_tid, Ordering::Release);
            slot.done.store(0, Ordering::Release);
            slot.child_pid.store(child_pid, Ordering::Release);
            fence(Ordering::SeqCst);
            return true;
        }
    }
    // Grow by one entry in `AtomicDir` cleanly outside any IRQ or lock.
    let idx = NEXT_HIGH_WATER
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
            (cur < CHUNK_LEN * MAX_CHUNKS).then_some(cur + 1)
        })
        .ok();
    let Some(idx) = idx else {
        return false;
    };
    if !COMPLETIONS.ensure(idx, Slot::new) {
        return false;
    }
    let Some(slot) = COMPLETIONS.get(idx) else {
        return false;
    };
    if slot
        .child_pid
        .compare_exchange(0, RESERVED_SLOT, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        slot.parent_tid.store(parent_tid, Ordering::Release);
        slot.done.store(0, Ordering::Release);
        slot.child_pid.store(child_pid, Ordering::Release);
        fence(Ordering::SeqCst);
        return true;
    }
    false
}

/// Reserves a `vfork` completion record before child activation.
pub fn begin_vfork_wait(child_pid: u64, parent_tid: u64) -> bool {
    if child_pid == 0 || child_pid == RESERVED_SLOT || parent_tid == 0 {
        return false;
    }
    if try_claim_slot(child_pid, parent_tid) {
        return true;
    }
    reclaim_orphaned();
    try_claim_slot(child_pid, parent_tid)
}

/// Frees records whose waiting parent task no longer exists (only the parent
/// consumes a record, so a dead parent can never free it). Records still
/// awaiting the child stay: `complete_vfork` must be able to find them.
fn reclaim_orphaned() {
    for slot in COMPLETIONS.iter() {
        let child_pid = slot.child_pid.load(Ordering::Acquire);
        if child_pid == 0 || child_pid == RESERVED_SLOT {
            continue;
        }
        let parent_tid = slot.parent_tid.load(Ordering::Acquire);
        if slot.done.load(Ordering::Acquire) != 0
            && parent_tid != 0
            && !parent_task_alive(parent_tid)
            && slot
                .child_pid
                .compare_exchange(child_pid, RESERVED_SLOT, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            slot.parent_tid.store(0, Ordering::Release);
            slot.done.store(0, Ordering::Release);
            slot.child_pid.store(0, Ordering::Release);
        }
    }
}

#[cfg(not(test))]
fn parent_task_alive(parent_tid: u64) -> bool {
    super::is_user_task_alive(parent_tid)
}

#[cfg(test)]
fn parent_task_alive(parent_tid: u64) -> bool {
    parent_tid != 8_999
}

/// Signals completion for `child_pid` (called after `exec` commit or child
/// exit/fault/detach) and wakes only its registered `parent_tid`.
pub fn complete_vfork(child_pid: u64) {
    if child_pid == 0 || child_pid == RESERVED_SLOT {
        return;
    }
    fence(Ordering::SeqCst);
    for slot in COMPLETIONS.iter() {
        let current = slot.child_pid.load(Ordering::Acquire);
        if current == child_pid {
            slot.done.store(1, Ordering::Release);
            fence(Ordering::SeqCst);
            if slot.child_pid.load(Ordering::Acquire) == child_pid {
                let parent_tid = slot.parent_tid.load(Ordering::Acquire);
                #[cfg(not(test))]
                if parent_tid != 0 {
                    let _ = super::wake_task(parent_tid);
                }
                #[cfg(test)]
                let _ = parent_tid;
            }
        }
    }
}

/// Returns `true` and frees the slot once `complete_vfork(child_pid)` has
/// fired (or if no matching entry exists).
pub fn take_vfork_done(child_pid: u64, parent_tid: u64) -> bool {
    if child_pid == 0 || child_pid == RESERVED_SLOT || parent_tid == 0 {
        return true;
    }
    fence(Ordering::SeqCst);
    let mut found = false;
    for slot in COMPLETIONS.iter() {
        if slot.child_pid.load(Ordering::Acquire) == child_pid
            && slot.parent_tid.load(Ordering::Acquire) == parent_tid
        {
            found = true;
            if slot.done.load(Ordering::Acquire) != 0 {
                slot.parent_tid.store(0, Ordering::Release);
                slot.done.store(0, Ordering::Release);
                slot.child_pid.store(0, Ordering::Release);
                return true;
            }
        }
    }
    !found
}

/// Removes any pending `vfork` completion record for `(child_pid, parent_tid)`
/// when the wait is abandoned (spawn rollback, fatal parent interruption).
pub fn cancel_vfork_wait(child_pid: u64, parent_tid: u64) {
    if child_pid == 0 || child_pid == RESERVED_SLOT {
        return;
    }
    for slot in COMPLETIONS.iter() {
        if slot.child_pid.load(Ordering::Acquire) == child_pid
            && (parent_tid == 0 || slot.parent_tid.load(Ordering::Acquire) == parent_tid)
        {
            slot.parent_tid.store(0, Ordering::Release);
            slot.done.store(0, Ordering::Release);
            slot.child_pid.store(0, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn early_completion_before_wait_is_latched_and_reclaimed() {
        let _guard = TEST_LOCK.lock().unwrap();
        assert!(begin_vfork_wait(9001, 8001));
        assert!(!take_vfork_done(9001, 8001));
        complete_vfork(9001);
        assert!(take_vfork_done(9001, 8001));
        // Consumed cleanly so no stale entry remains.
        assert!(take_vfork_done(9001, 8001));
    }

    #[test]
    fn completed_record_of_dead_parent_is_reclaimed_when_table_is_full() {
        let _guard = TEST_LOCK.lock().unwrap();
        assert!(begin_vfork_wait(9003, 8_999));
        complete_vfork(9003);
        let mut filled = std::vec::Vec::new();
        for i in 0..128 {
            let pid = 70_000 + i;
            assert!(begin_vfork_wait(pid, 8004));
            filled.push(pid);
        }
        reclaim_orphaned();
        for pid in &filled {
            cancel_vfork_wait(*pid, 8004);
        }
        assert!(begin_vfork_wait(9004, 8004));
        cancel_vfork_wait(9004, 8004);
        assert!(take_vfork_done(9003, 8_999), "orphan record was swept");
    }

    #[test]
    fn cancel_frees_pending_slot_for_reuse() {
        let _guard = TEST_LOCK.lock().unwrap();
        assert!(begin_vfork_wait(9002, 8002));
        assert!(!take_vfork_done(9002, 8002));
        cancel_vfork_wait(9002, 8002);
        assert!(take_vfork_done(9002, 8002));
    }
}
