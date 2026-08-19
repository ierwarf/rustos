//! Per-slot SIMD and TLS payloads kept outside the scheduler catalog.
//!
//! The owner word in the parent module remains the lifecycle authority. This
//! module holds only architecture-facing state: a lock-free FS-base cache and
//! one owner-bound raw SIMD image for every task slot.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{MAX_TASK, RunOwnerState, owner};
use crate::arch::simd::{SimdState, restore_state, save_state};

/// Per-slot Linux FS base used on every return to user mode. The complete
/// Linux thread state remains behind its generation-bound lock; this is only
/// the architectural hot field that dispatch must load without taking it.
static TLS_FS_BASE: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

/// One SIMD image for each task slot, deliberately outside the scheduler
/// catalog. A SIMD save/restore is issued only by the exact execution owner;
/// admission/reset runs before owner publication or after terminal release.
/// Those lifecycle rules make the raw cell single-writer even though it is
/// shared across CPUs.
struct SlotSimdState(UnsafeCell<SimdState>);

// SAFETY: callers are limited to the owner-word/lifecycle transitions above.
// No two CPUs may execute, save, restore, or reset one live slot concurrently.
unsafe impl Sync for SlotSimdState {}

impl SlotSimdState {
    const fn new() -> Self {
        Self(UnsafeCell::new(SimdState::new()))
    }
}

static SIMD_STATES: [SlotSimdState; MAX_TASK] = [const { SlotSimdState::new() }; MAX_TASK];

pub(crate) fn reset_before_publication() {
    for tls_fs_base in &TLS_FS_BASE {
        tls_fs_base.store(0, Ordering::Release);
    }
    for state in &SIMD_STATES {
        // SAFETY: reset runs before scheduler/AP publication.
        unsafe { *state.0.get() = SimdState::new() };
    }
}

#[inline]
pub(crate) fn tls_fs_base(slot: usize) -> u64 {
    TLS_FS_BASE
        .get(slot)
        .expect("scheduler TLS FS base slot exceeds capacity")
        .load(Ordering::Acquire)
}

/// Updates the hot TLS cache after the generation-bound Linux-thread authority
/// has accepted its new FS base. Release makes that state visible to the next
/// dispatch that acquires this slot's owner publication.
#[inline]
pub(crate) fn set_tls_fs_base(slot: usize, value: u64) {
    TLS_FS_BASE
        .get(slot)
        .expect("scheduler TLS FS base slot exceeds capacity")
        .store(value, Ordering::Release);
}

pub(crate) fn clear_tls_fs_base(slot: usize) {
    TLS_FS_BASE[slot].store(0, Ordering::Release);
}

/// Installs the architectural default SIMD image before this slot receives an
/// owner publication.
pub(crate) fn initialize_simd_state(slot: usize) {
    assert_eq!(
        owner(slot).state,
        RunOwnerState::Dormant,
        "scheduler SIMD state initialized after owner publication"
    );
    reset_simd_state(slot);
}

/// Restores the architectural default image for a slot whose current owner
/// has replaced its execution image (exec) or has terminally released it.
pub(crate) fn reset_simd_state(slot: usize) {
    let state = SIMD_STATES
        .get(slot)
        .expect("scheduler SIMD state slot exceeds capacity");
    // SAFETY: see `SlotSimdState`; callers retain exact execution or terminal
    // lifecycle ownership of `slot` for this complete replacement.
    unsafe { *state.0.get() = SimdState::new() };
}

#[inline]
pub(crate) fn save_simd_state(slot: usize) {
    let state = SIMD_STATES
        .get(slot)
        .expect("scheduler SIMD state slot exceeds capacity");
    // SAFETY: the exact CPU executing `slot` saves into its owner-bound image.
    unsafe { save_state(&mut *state.0.get()) };
}

#[inline]
pub(crate) fn restore_simd_state(slot: usize) {
    let state = SIMD_STATES
        .get(slot)
        .expect("scheduler SIMD state slot exceeds capacity");
    // SAFETY: the exact CPU executing `slot` restores its owner-bound image.
    unsafe { restore_state(&*state.0.get()) };
}
