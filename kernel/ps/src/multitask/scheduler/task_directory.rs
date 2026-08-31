//! Lock-free task-id resolution shared by the catalog and its readers.
//!
//! Two questions dominate the synchronous IPC round trip and neither of them
//! needs the global catalog: "which log identity does this exact task carry?"
//! and "is this scheduling-context identity still the live custody of that
//! task?". Both were answered by taking the exclusive scheduler guard purely
//! to walk `starts`/`contexts`, so each round trip paid two acquisitions to
//! read state that is already published per slot.
//!
//! The direct-mapped hint below is the same accelerator `find_task_slot` has
//! always used, lifted out of the locked struct so an unlocked reader can use
//! it too. It is never authority. Every hit is revalidated against the
//! seqlock-published per-slot identity and against the owner word's lifetime,
//! and a miss falls back to the original locked scan, so a stale, colliding,
//! or reused entry can only cost a bounded fallback.

use core::sync::atomic::{AtomicU8, Ordering};

use super::runqueue;
use super::scheduling_context::SchedulingContext;
use super::{MAX_TASK, current_identity};
use kernel_object::api::identity::ObjectIdentity;

/// No slot is currently hinted for this residue class.
const EMPTY: u8 = u8::MAX;

/// One bucket per residue class of the monotonic task id. `MAX_TASK` is a
/// power of two, so the index is a mask rather than a division.
const _: () = assert!(MAX_TASK.is_power_of_two());
const _: () = assert!(MAX_TASK < EMPTY as usize);

static HINTS: [AtomicU8; MAX_TASK] = [const { AtomicU8::new(EMPTY) }; MAX_TASK];

#[inline]
const fn bucket(task_id: u64) -> usize {
    task_id as usize & (MAX_TASK - 1)
}

/// The slot last observed to hold `task_id`, if any. Callers must revalidate.
#[inline]
pub(super) fn hint(task_id: u64) -> Option<usize> {
    // ORDERING: Relaxed is exact. The value is a hint that every caller
    // revalidates against authoritative identity before use, so no other
    // memory is published through it.
    let slot = usize::from(HINTS[bucket(task_id)].load(Ordering::Relaxed));
    (slot < MAX_TASK).then_some(slot)
}

/// Records `slot` as the accelerator for `task_id`.
#[inline]
pub(super) fn record(task_id: u64, slot: usize) {
    let slot = u8::try_from(slot).expect("scheduler slot exceeds task hint capacity");
    // ORDERING: see `hint`; repair is equally non-authoritative.
    HINTS[bucket(task_id)].store(slot, Ordering::Relaxed);
}

/// Forgets the accelerator for `task_id` after an authoritative miss.
#[inline]
pub(super) fn forget(task_id: u64) {
    HINTS[bucket(task_id)].store(EMPTY, Ordering::Relaxed);
}

/// Clears every hint. Boot-time scheduler reset only.
pub(super) fn reset() {
    for hint in &HINTS {
        hint.store(EMPTY, Ordering::Relaxed);
    }
}

/// Resolves `task_id` to a live slot without the catalog guard.
///
/// `None` means the question must be re-asked under the scheduler guard: the
/// hint was empty, pointed at a different task, caught a publication in
/// progress, or named a slot whose owner word has already reached a terminal
/// state. It never means "no such task".
fn live_published_slot(task_id: u64) -> Option<(usize, current_identity::TaskIdentity)> {
    let slot = hint(task_id)?;
    // The owner word is the lifetime authority. A retired slot may still carry
    // a catalog context until reclaim runs, and the locked lookup excludes it,
    // so this reader must exclude it identically rather than more loosely.
    if runqueue::owner(slot).state.is_terminal() {
        return None;
    }
    let identity = current_identity::read(slot)?;
    (identity.task_id == Some(task_id)).then_some((slot, identity))
}

/// The live slot holding `task_id`, or `None` when the guard must answer.
pub(super) fn live_slot(task_id: u64) -> Option<usize> {
    live_published_slot(task_id).map(|(slot, _)| slot)
}

/// Log identity for `task_id`, or `None` when the guard must answer.
///
/// The inner `None` is a definitive kernel task, matching the locked query's
/// own absence of a user log pair.
pub(in crate::multitask) fn published_user_log_ids(task_id: u64) -> Option<Option<(u64, u64)>> {
    let (_, identity) = live_published_slot(task_id)?;
    identity.complete_user_log_ids()
}

/// Whether `identity` is still the live scheduling-context custody of
/// `task_id`, or `None` when the guard must answer.
///
/// A scheduling context's identity is derived entirely from the slot that owns
/// it and the monotonic task bound to it, so proving the published binding is
/// the whole check; the budget the catalog also stores is not part of custody.
pub(in crate::multitask) fn published_scheduling_context_matches(
    task_id: u64,
    identity: ObjectIdentity,
) -> Option<bool> {
    let raw_slot = identity.slot().checked_sub(1)?;
    let identity_slot = usize::try_from(raw_slot).ok()?;
    let (slot, _) = live_published_slot(task_id)?;
    if slot != identity_slot {
        // The identity names a different slot than the one holding the task.
        // That is conclusively a mismatch, but the locked path is what owns
        // saying so, and this is not a hot outcome.
        return None;
    }
    Some(SchedulingContext::derived_identity(slot, task_id) == Some(identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hint table is one process-wide static, exactly as it is in the
    /// kernel. Scheduler fixtures already serialize on this same lock for the
    /// same reason, so a witness that mutates the table must hold it too.
    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        crate::multitask::cpu_local::test_publication_lock()
    }

    #[test]
    fn a_hint_round_trips_and_clears() {
        let _serial = serialized();
        let task_id = 4_097;
        forget(task_id);
        assert_eq!(hint(task_id), None);
        record(task_id, 9);
        assert_eq!(hint(task_id), Some(9));
        forget(task_id);
        assert_eq!(hint(task_id), None);
    }

    /// The unlocked readers accept a hint only when the slot it names still
    /// publishes the exact task. Without that revalidation a reused or
    /// colliding bucket would answer for the wrong task, so the property has
    /// its own witness rather than only being exercised indirectly.
    #[test]
    fn an_unvalidated_hint_never_answers_for_another_task() {
        let _serial = serialized();
        let slot = 21;
        let resident = 0x6101;
        let stranger = resident + MAX_TASK as u64;
        crate::multitask::current_identity::clear(slot);
        crate::multitask::current_identity::publish(
            slot,
            crate::multitask::current_identity::TaskIdentity {
                task_id: Some(resident),
                user_mode: true,
                abi: Some(crate::user::abi::UserAbi::Linux),
                process_handle: None,
                process_id: Some(0x901),
                console_session: crate::io::session::ConsoleSessionHandle::SYSTEM,
                pager_charge: None,
            },
        );
        record(resident, slot);
        assert_eq!(live_slot(resident), Some(slot));
        assert_eq!(
            published_user_log_ids(resident),
            Some(Some((0x901, resident)))
        );

        // Same bucket, different task: the hint hits and the identity check is
        // the only thing that can reject it.
        assert_eq!(bucket(stranger), bucket(resident));
        assert_eq!(live_slot(stranger), None);
        assert_eq!(published_user_log_ids(stranger), None);

        // A cleared slot leaves the hint behind and must still not answer.
        crate::multitask::current_identity::clear(slot);
        assert_eq!(live_slot(resident), None);
        forget(resident);
    }

    #[test]
    fn colliding_task_ids_share_one_bucket_and_never_alias_a_slot() {
        let _serial = serialized();
        let first = 4_098;
        let second = first + MAX_TASK as u64;
        assert_eq!(bucket(first), bucket(second));
        record(first, 3);
        record(second, 11);
        // The loser reads the winner's slot, which is why every caller
        // revalidates the exact task identity before using it.
        assert_eq!(hint(first), Some(11));
        forget(first);
    }
}
