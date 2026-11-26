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

const TAG_MASK: u64 = 0b11;
const TAG_FREE: u64 = 0;
const TAG_CLAIMING: u64 = 1;
const TAG_PENDING: u64 = 2;
const TAG_DONE: u64 = 3;
const GEN_SHIFT: u32 = 2;

const fn state_word(generation: u64, tag: u64) -> u64 {
    (generation << GEN_SHIFT) | (tag & TAG_MASK)
}

const fn state_gen(word: u64) -> u64 {
    word >> GEN_SHIFT
}

const fn state_tag(word: u64) -> u64 {
    word & TAG_MASK
}

struct Slot {
    child_pid: AtomicU64,
    parent_tid: AtomicU64,
    state: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            child_pid: AtomicU64::new(0),
            parent_tid: AtomicU64::new(0),
            state: AtomicU64::new(state_word(0, TAG_FREE)),
        }
    }
}

static COMPLETIONS: AtomicDir<Slot, CHUNK_LEN, MAX_CHUNKS> = AtomicDir::new();
static NEXT_HIGH_WATER: AtomicUsize = AtomicUsize::new(0);

fn try_claim_existing_slot(slot: &Slot, child_pid: u64, parent_tid: u64) -> bool {
    let word = slot.state.load(Ordering::Acquire);
    if state_tag(word) != TAG_FREE {
        return false;
    }
    let next_gen = state_gen(word).wrapping_add(1);
    if slot
        .state
        .compare_exchange(
            word,
            state_word(next_gen, TAG_CLAIMING),
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .is_err()
    {
        return false;
    }
    slot.parent_tid.store(parent_tid, Ordering::Release);
    slot.child_pid.store(child_pid, Ordering::Release);
    slot.state
        .store(state_word(next_gen, TAG_PENDING), Ordering::Release);
    fence(Ordering::SeqCst);
    true
}

fn try_claim_slot(child_pid: u64, parent_tid: u64) -> bool {
    // ORDERING: Acquire observes allocated slots published prior to scan.
    let len = NEXT_HIGH_WATER.load(Ordering::Acquire);
    for idx in 0..len {
        let Some(slot) = COMPLETIONS.get(idx) else {
            continue;
        };
        if try_claim_existing_slot(slot, child_pid, parent_tid) {
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
    try_claim_existing_slot(slot, child_pid, parent_tid)
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

fn release_slot(slot: &Slot, generation: u64) {
    slot.child_pid.store(0, Ordering::Release);
    slot.parent_tid.store(0, Ordering::Release);
    slot.state
        .store(state_word(generation, TAG_FREE), Ordering::Release);
}

/// Frees records whose waiting parent task no longer exists (only the parent
/// consumes a record, so a dead parent can never free it). Records still
/// awaiting the child stay: `complete_vfork` must be able to find them.
fn reclaim_orphaned() {
    for slot in COMPLETIONS.iter() {
        let word = slot.state.load(Ordering::Acquire);
        if state_tag(word) != TAG_DONE {
            continue;
        }
        let child_pid = slot.child_pid.load(Ordering::Acquire);
        if child_pid == 0 || child_pid == RESERVED_SLOT {
            continue;
        }
        let parent_tid = slot.parent_tid.load(Ordering::Acquire);
        if parent_tid != 0
            && !parent_task_alive(parent_tid)
            && slot
                .state
                .compare_exchange(
                    word,
                    state_word(state_gen(word), TAG_CLAIMING),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            release_slot(slot, state_gen(word));
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
        let word = slot.state.load(Ordering::Acquire);
        if state_tag(word) != TAG_PENDING {
            continue;
        }
        if slot.child_pid.load(Ordering::Acquire) != child_pid {
            continue;
        }
        let parent_tid = slot.parent_tid.load(Ordering::Acquire);
        let generation = state_gen(word);
        if slot
            .state
            .compare_exchange(
                word,
                state_word(generation, TAG_DONE),
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            #[cfg(not(test))]
            if parent_tid != 0 {
                let _ = super::wake_task(parent_tid);
            }
            #[cfg(test)]
            let _ = parent_tid;
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
        let mut word = slot.state.load(Ordering::Acquire);
        let tag = state_tag(word);
        if tag != TAG_PENDING && tag != TAG_DONE {
            continue;
        }
        if slot.child_pid.load(Ordering::Acquire) == child_pid
            && slot.parent_tid.load(Ordering::Acquire) == parent_tid
        {
            word = slot.state.load(Ordering::Acquire);
            if state_tag(word) == TAG_DONE {
                let generation = state_gen(word);
                if slot
                    .state
                    .compare_exchange(
                        word,
                        state_word(generation, TAG_CLAIMING),
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    release_slot(slot, generation);
                }
                return true;
            }
            if state_tag(word) == TAG_PENDING {
                found = true;
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
        let mut word = slot.state.load(Ordering::Acquire);
        let generation = state_gen(word);
        while matches!(state_tag(word), TAG_PENDING | TAG_DONE) {
            if slot.child_pid.load(Ordering::Acquire) != child_pid
                || (parent_tid != 0 && slot.parent_tid.load(Ordering::Acquire) != parent_tid)
            {
                break;
            }
            match slot.state.compare_exchange(
                word,
                state_word(generation, TAG_CLAIMING),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    release_slot(slot, generation);
                    break;
                }
                Err(actual) if state_gen(actual) == generation => {
                    word = actual;
                }
                Err(_) => break,
            }
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

    #[test]
    fn stale_generation_does_not_clobber_reused_slot() {
        let _guard = TEST_LOCK.lock().unwrap();
        assert!(begin_vfork_wait(9101, 8101));
        let slot = COMPLETIONS
            .iter()
            .find(|s| s.child_pid.load(Ordering::Acquire) == 9101)
            .expect("claimed slot");
        let word1 = slot.state.load(Ordering::Acquire);
        assert_eq!(state_tag(word1), TAG_PENDING);

        // Cancel first wait and immediately reuse the same slot for a new wait.
        cancel_vfork_wait(9101, 8101);
        assert!(begin_vfork_wait(9102, 8102));
        let word2 = slot.state.load(Ordering::Acquire);
        assert_eq!(state_tag(word2), TAG_PENDING);
        assert_eq!(state_gen(word2), state_gen(word1).wrapping_add(1));

        // A delayed CAS carrying the stale generation from `9101` cannot mark
        // the reused slot `DONE`.
        assert!(slot
            .state
            .compare_exchange(
                word1,
                state_word(state_gen(word1), TAG_DONE),
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_err());
        complete_vfork(9101);
        assert!(!take_vfork_done(9102, 8102));

        // Only completing the current generation releases `9102`.
        complete_vfork(9102);
        assert!(take_vfork_done(9102, 8102));
    }
}
