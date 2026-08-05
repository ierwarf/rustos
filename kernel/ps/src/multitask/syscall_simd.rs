//! Per-slot syscall SIMD custody, held outside the scheduler lock.
//!
//! A syscall trust boundary saves the caller's SIMD image on entry and restores
//! it on return. That image is deliberately separate from the scheduler's own
//! per-task SIMD slot: a blocking syscall may context-switch while its kernel
//! continuation is live, and the scheduler must then save the continuation's
//! scratch state without exposing it to userspace.
//!
//! The buffers lived in `Scheduler`, so every capture and restore took the
//! exclusive global lock. The acquisition census measured roughly 13.1k of each
//! per second, and each one carried a full `XSAVE`/`XRSTOR` inside the critical
//! section — work that belongs to one task on one CPU and excludes nothing.
//!
//! **Exclusion.** The buffer for a slot is touched only by:
//!
//! - the CPU executing that slot's task, at a syscall boundary, with interrupts
//!   masked, which is what keeps the current slot stable across the access; and
//! - `reset`, when the scheduler binds or rebinds the slot while holding its
//!   lock. Fresh allocation targets a slot with no execution owner, and the
//!   exec rebind of a live slot runs on that owner's own CPU.
//!
//! A task slot cannot be current on two CPUs — `SchedulerAccessGuard::drop`
//! fails the kernel if it ever is — so no two CPUs can reach the same buffer.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use super::MAX_SCHEDULER_TASKS;
use crate::arch::simd::{SimdState, restore_state, save_state};

struct SyscallSimdSlots([UnsafeCell<SimdState>; MAX_SCHEDULER_TASKS]);

// SAFETY: see the module exclusion note. Only the CPU running a slot's task, or
// the scheduler owner while that slot has no running task, reaches its cell.
unsafe impl Sync for SyscallSimdSlots {}

static STATES: SyscallSimdSlots =
    SyscallSimdSlots([const { UnsafeCell::new(SimdState::new()) }; MAX_SCHEDULER_TASKS]);

/// Whether a slot currently holds a saved syscall image. Atomic because
/// `reset` publishes from the scheduler owner while the slot's own CPU reads
/// it, never because two CPUs contend for one slot.
static ACTIVE: [AtomicBool; MAX_SCHEDULER_TASKS] =
    [const { AtomicBool::new(false) }; MAX_SCHEDULER_TASKS];

/// Saves the running task's user SIMD image into `slot`.
///
/// Returns `false` when the slot already holds an image, which is how a nested
/// boundary declines to overwrite the outer one, and when the slot is out of
/// range.
///
/// # Safety
///
/// The caller must be executing `slot`'s task with interrupts masked.
pub(super) unsafe fn capture(slot: usize) -> bool {
    let Some(active) = ACTIVE.get(slot) else {
        return false;
    };
    // ORDERING: Acquire pairs with the Release in `reset` and `restore`.
    if active.load(Ordering::Acquire) {
        return false;
    }
    let Some(state) = STATES.0.get(slot) else {
        return false;
    };
    // SAFETY: the caller owns this slot's execution and interrupts are masked,
    // so no other context can reach this cell for the duration of the save.
    unsafe { save_state(&mut *state.get()) };
    // ORDERING: Release publishes the saved image before the flag that claims
    // one exists.
    active.store(true, Ordering::Release);
    true
}

/// Restores and consumes the image saved in `slot`.
///
/// Returns `false` when the slot holds no image, which is the case after an
/// exec rebind discarded it.
///
/// # Safety
///
/// The caller must be executing `slot`'s task with interrupts masked.
pub(super) unsafe fn restore(slot: usize) -> bool {
    let Some(active) = ACTIVE.get(slot) else {
        return false;
    };
    // ORDERING: Acquire observes the image published by the matching capture.
    if !active.load(Ordering::Acquire) {
        return false;
    }
    let Some(state) = STATES.0.get(slot) else {
        return false;
    };
    // SAFETY: as in `capture`; the caller owns this slot's execution.
    unsafe { restore_state(&*state.get()) };
    // ORDERING: Release makes the consumed image visible as absent before any
    // later capture can claim the slot again.
    active.store(false, Ordering::Release);
    true
}

/// Discards any image held for `slot`.
///
/// Called when the scheduler binds or rebinds the slot. Callers hold the
/// scheduler lock.
pub(super) fn reset(slot: usize) {
    if let Some(active) = ACTIVE.get(slot) {
        // ORDERING: Release retires the previous image for the next owner of
        // this slot.
        active.store(false, Ordering::Release);
    }
}

/// Whether `slot` currently holds a saved image.
#[cfg(test)]
fn is_active(slot: usize) -> bool {
    ACTIVE
        .get(slot)
        .is_some_and(|active| active.load(Ordering::Acquire))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Witness for `syscall-simd-lifecycle`.
    ///
    /// The entering user image is held in this module's own storage, so the
    /// scheduler's continuation save cannot reach it: `Scheduler` has no field
    /// for it at all, which the type system enforces rather than a layout
    /// assertion. What still needs proving is the custody protocol around that
    /// storage — one image per slot, consumed exactly once, and discarded by an
    /// exec rebind so a return cannot restore an image the slot no longer owns.
    #[test]
    fn syscall_simd_image_is_captured_once_consumed_once_and_dropped_by_rebind() {
        let slot = MAX_SCHEDULER_TASKS - 2;
        reset(slot);

        // capture-entering-user-image
        // SAFETY: the test thread is the sole accessor of this spare slot.
        assert!(unsafe { capture(slot) });
        assert!(is_active(slot));

        // reject-nested: a second boundary must not overwrite the outer image.
        // SAFETY: as above.
        assert!(!unsafe { capture(slot) });
        assert!(is_active(slot));

        // restore-entering-user-image, and the image is consumed by it.
        // SAFETY: as above; the image was written by the capture above.
        assert!(unsafe { restore(slot) });
        assert!(!is_active(slot));
        // SAFETY: as above.
        assert!(!unsafe { restore(slot) });

        // reject-wrong-task: an exec rebind discards the image, so the pending
        // return finds nothing to restore instead of restoring a foreign one.
        // SAFETY: as above.
        assert!(unsafe { capture(slot) });
        reset(slot);
        assert!(!is_active(slot));
        // SAFETY: as above.
        assert!(!unsafe { restore(slot) });
    }

    #[test]
    fn out_of_range_slots_are_declined_rather_than_panicking() {
        // SAFETY: an out-of-range slot never reaches a SIMD instruction.
        assert!(!unsafe { capture(MAX_SCHEDULER_TASKS) });
        // SAFETY: as above.
        assert!(!unsafe { restore(MAX_SCHEDULER_TASKS) });
        assert!(!is_active(MAX_SCHEDULER_TASKS));
    }
}
