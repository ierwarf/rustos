//! Per-slot affinity and locality payloads outside the scheduler catalog.
//!
//! Affinity commits are serialized by scheduler authority, yet the versioned
//! record keeps task mask, process mask, and migration intent coherent if a
//! future diagnostic or placement reader observes it without that catalog.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering, fence};

use super::{MAX_TASK, RunOwnerState, owner};

const UNRESTRICTED_MASK: u64 = u64::MAX;
const NO_LAST_CPU: u8 = u8::MAX;

struct SlotAffinity {
    version: AtomicU64,
    task_mask: AtomicU64,
    process_mask: AtomicU64,
    migration_pending: AtomicBool,
}

impl SlotAffinity {
    const fn new() -> Self {
        Self {
            version: AtomicU64::new(0),
            task_mask: AtomicU64::new(UNRESTRICTED_MASK),
            process_mask: AtomicU64::new(UNRESTRICTED_MASK),
            migration_pending: AtomicBool::new(false),
        }
    }
}

static AFFINITIES: [SlotAffinity; MAX_TASK] = [const { SlotAffinity::new() }; MAX_TASK];
static LAST_CPU: [AtomicU8; MAX_TASK] = [const { AtomicU8::new(NO_LAST_CPU) }; MAX_TASK];

pub(crate) fn reset_before_publication() {
    for (affinity, last_cpu) in AFFINITIES.iter().zip(LAST_CPU.iter()) {
        affinity.version.store(0, Ordering::Release);
        affinity
            .task_mask
            .store(UNRESTRICTED_MASK, Ordering::Release);
        affinity
            .process_mask
            .store(UNRESTRICTED_MASK, Ordering::Release);
        affinity.migration_pending.store(false, Ordering::Release);
        last_cpu.store(NO_LAST_CPU, Ordering::Release);
    }
}

pub(crate) fn initialize_affinity(slot: usize, task_mask: u64, process_mask: u64) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler affinity initialized after owner publication"
    );
    set_affinity(slot, task_mask, process_mask, false);
    LAST_CPU[slot].store(NO_LAST_CPU, Ordering::Release);
}

pub(crate) fn reset_affinity(slot: usize) {
    set_affinity(slot, UNRESTRICTED_MASK, UNRESTRICTED_MASK, false);
    LAST_CPU[slot].store(NO_LAST_CPU, Ordering::Release);
}

pub(crate) fn set_affinity(
    slot: usize,
    task_mask: u64,
    process_mask: u64,
    migration_pending: bool,
) {
    assert!(
        task_mask != 0 && process_mask != 0 && task_mask & !process_mask == 0,
        "scheduler affinity payload is invalid"
    );
    let affinity = AFFINITIES
        .get(slot)
        .expect("scheduler affinity slot exceeds capacity");
    let version = affinity.version.load(Ordering::Relaxed);
    assert_eq!(
        version & 1,
        0,
        "scheduler affinity replacement raced for slot {slot}"
    );
    affinity
        .version
        .store(version.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    affinity.task_mask.store(task_mask, Ordering::Relaxed);
    affinity.process_mask.store(process_mask, Ordering::Relaxed);
    affinity
        .migration_pending
        .store(migration_pending, Ordering::Relaxed);
    affinity
        .version
        .store(version.wrapping_add(2), Ordering::Release);
}

/// Returns a coherent affinity commit. An overlapping writer is a fail-closed
/// empty record; scheduler-serialized callers never observe that fallback.
#[inline]
pub(crate) fn affinity_snapshot(slot: usize) -> (u64, u64, bool) {
    let affinity = AFFINITIES
        .get(slot)
        .expect("scheduler affinity slot exceeds capacity");
    let version = affinity.version.load(Ordering::Acquire);
    if version & 1 != 0 {
        return (0, 0, true);
    }
    let task_mask = affinity.task_mask.load(Ordering::Relaxed);
    let process_mask = affinity.process_mask.load(Ordering::Relaxed);
    let migration_pending = affinity.migration_pending.load(Ordering::Relaxed);
    fence(Ordering::Acquire);
    if affinity.version.load(Ordering::Relaxed) != version {
        return (0, 0, true);
    }
    (task_mask, process_mask, migration_pending)
}

#[inline]
pub(crate) fn record_last_cpu(slot: usize, cpu: u8) {
    LAST_CPU
        .get(slot)
        .expect("scheduler locality slot exceeds capacity")
        .store(cpu, Ordering::Release);
}

#[inline]
pub(crate) fn last_cpu(slot: usize) -> u8 {
    LAST_CPU
        .get(slot)
        .expect("scheduler locality slot exceeds capacity")
        .load(Ordering::Acquire)
}
