//! Owner: task-id candidate hints and exact scalar reply object custody.
//! Boundary: original Identity51 writer -> Resource53 UID/start/context comparison.
//! Lifecycle: hints may collide or disappear; retained terminal objects settle custody only.
//! Concurrency: original writer remains held through the bounded resource leaf, never reentered.
//! Failure: absent/foreign bindings refuse; hints cannot replace the exact monotonic UID.
//! Forbidden: catalog fallback for reply custody, execution grants, or hardware/frame callbacks.
//! Evidence: actual born resources, metadata withdrawal and explicit terminal consumer ACK.
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
//! or reused entry can only cost a bounded fallback. Reply custody resolves
//! candidates under original identity/resource leaves, never a catalog guard.

use core::sync::atomic::{AtomicU64, Ordering};

use super::runqueue;
use super::{MAX_TASK, current_identity};
use kernel_object::api::identity::ObjectIdentity;

/// Four-way set-associative `(task id, slot)` pairs, each packed in one word
/// as `(id + 1) << 8 | slot` so a reader can never pair one task's id with
/// another's slot; zero is an empty way. The previous direct-mapped byte per
/// `id % MAX_TASK` silently aliased two live tasks with the same residue, and
/// every lookup of the evicted one became a full resource-leaf walk.
const WAYS: usize = 4;
const SETS: usize = MAX_TASK;
const _: () = assert!(SETS.is_power_of_two());
const _: () = assert!(MAX_TASK <= u8::MAX as usize);

static HINTS: [AtomicU64; SETS * WAYS] = [const { AtomicU64::new(0) }; SETS * WAYS];

#[inline]
fn set(task_id: u64) -> &'static [AtomicU64] {
    let index = (task_id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize & (SETS - 1);
    &HINTS[index * WAYS..(index + 1) * WAYS]
}

#[inline]
fn tag(task_id: u64) -> u64 {
    task_id.wrapping_add(1) << 8
}

/// The slot last observed to hold `task_id`, if any. Callers must revalidate.
#[inline]
pub(super) fn hint(task_id: u64) -> Option<usize> {
    let tag = tag(task_id);
    // ORDERING: Relaxed is exact. The value is a hint that every caller
    // revalidates against authoritative identity before use, so no other
    // memory is published through it; the packed word cannot tear.
    set(task_id).iter().map(|way| way.load(Ordering::Relaxed))
        .find(|&entry| entry != 0 && entry & !0xff == tag)
        .map(|entry| (entry & 0xff) as usize)
        .filter(|&slot| slot < MAX_TASK)
}

/// Records `slot` as the accelerator for `task_id`.
#[inline]
pub(super) fn record(task_id: u64, slot: usize) {
    let slot = u8::try_from(slot).expect("scheduler slot exceeds task hint capacity");
    let tag = tag(task_id);
    let entry = tag | u64::from(slot);
    let ways = set(task_id);
    // ORDERING: see `hint`; repair is equally non-authoritative. Prefer this
    // task's own way, then an empty one, then a way chosen by the id.
    let way = ways.iter().find(|way| way.load(Ordering::Relaxed) & !0xff == tag)
        .or_else(|| ways.iter().find(|way| way.load(Ordering::Relaxed) == 0))
        .unwrap_or(&ways[task_id as usize % WAYS]);
    way.store(entry, Ordering::Relaxed);
}

/// Forgets the accelerator for `task_id` after an authoritative miss.
#[inline]
#[cfg(test)]
pub(super) fn forget(task_id: u64) {
    let tag = tag(task_id);
    for way in set(task_id) {
        let entry = way.load(Ordering::Relaxed);
        if entry != 0 && entry & !0xff == tag {
            // ORDERING: see `hint`. Only this exact entry is cleared; a racing
            // record of another task keeps its own word.
            let _ = way.compare_exchange(entry, 0, Ordering::Relaxed, Ordering::Relaxed);
        }
    }
}

/// Clears every hint. Boot-time scheduler reset only.
pub(super) fn reset() {
    for hint in &HINTS {
        hint.store(0, Ordering::Relaxed);
    }
}

/// Bounded candidates only. A missing/colliding accelerator is never an
/// authoritative miss; EACH consumer qualifies its own original lifetime.
pub(super) fn candidates(task_id: u64) -> impl Iterator<Item = usize> {
    let first = hint(task_id);
    first.into_iter().chain((0..MAX_TASK).filter(move |slot| Some(*slot) != first))
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
#[cfg(test)]
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

/// Scalar reply classification, not execution/funding authority. Resolve only
/// candidates; join exact original Identity51 -> Resources53 before comparing
/// actual bound object identity and live state in ONE physical resource leaf.
/// A retained retired exact object can still return custody, matching the
/// original reply settlement rule; absent old objects are terminal cancellation.
pub(in crate::multitask) fn reply_context_custody(
    task_id: u64,
    identity: ObjectIdentity,
) -> (bool, bool) {
    let inspect = |slot| {
        current_identity::with_reply_custody(slot, task_id, |live| {
            super::stack_resources::context::reply_custody(slot, task_id, identity, live)
        })
    };
    if let Some(result) = hint(task_id).and_then(inspect) {
        return result;
    }
    (0..MAX_TASK).find_map(inspect).unwrap_or((false, false))
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

    const CUSTODY_ID: u64 = 84200;
    fn actual_root(f: impl FnOnce(usize, super::super::ProcessHandle)) {
        let mut boot = super::super::root_creation::tests::bootstrap(crate::user::abi::UserAbi::Linux);
        boot.linux_thread_state = Some(super::super::LinuxThreadState::default());
        super::super::root_creation::tests::exec_root(CUSTODY_ID, false, boot, f);
    }

    #[test]
    fn actual_reply_custody_exact_resource_object_and_foreign_identity_are_one_scalar_result() {
        actual_root(|slot, _| {
            let expected = super::super::stack_resources::context::read(slot)
                .unwrap().scheduling_context.identity();
            assert_eq!(reply_context_custody(CUSTODY_ID, expected), (true, true));
            let wrong = super::super::scheduling_context::SchedulingContext::derived_identity(
                slot, CUSTODY_ID + 1,
            ).unwrap();
            assert_eq!(reply_context_custody(CUSTODY_ID, wrong), (false, true));
            assert_eq!(reply_context_custody(CUSTODY_ID + 1, expected), (false, false));
            assert_eq!(super::super::stack_resources::context::reply_custody(
                slot, CUSTODY_ID + 1, expected, true,
            ), None);
            // Explicit native negative publication withdrawal, NOT a new UID
            // or grant. Actual resource binding stays original/nonexecuting;
            // original metadata API forbids forging a different bound UID.
            let original = current_identity::read(slot).unwrap();
            current_identity::publish_metadata(slot, None);
            assert_eq!(reply_context_custody(CUSTODY_ID, expected), (false, false));
            current_identity::publish_metadata(slot, Some(original));
            assert_eq!(reply_context_custody(CUSTODY_ID, expected), (true, true));
        });
    }

    #[test]
    fn actual_reply_custody_stale_or_absent_hint_cannot_substitute_another_resource_binding() {
        actual_root(|slot, _| {
            let expected = super::super::stack_resources::context::read(slot)
                .unwrap().scheduling_context.identity();
            record(CUSTODY_ID, super::super::ROOT_TASK_SLOT);
            assert_eq!(reply_context_custody(CUSTODY_ID, expected), (true, true));
            forget(CUSTODY_ID);
            assert_eq!(reply_context_custody(CUSTODY_ID, expected), (true, true));
            record(CUSTODY_ID + 1, slot);
            assert_eq!(reply_context_custody(CUSTODY_ID + 1, expected), (false, false));
            forget(CUSTODY_ID + 1);
        });
    }

    #[test]
    fn actual_reply_custody_retained_terminal_object_returns_only_exact_custody_not_live_authority() {
        let mut boot = super::super::root_creation::tests::bootstrap(crate::user::abi::UserAbi::Linux);
        boot.linux_thread_state = Some(super::super::LinuxThreadState::default());
        super::super::root_creation::tests::exec_root_with_release(
            CUSTODY_ID, false, boot,
            |slot, _| {
                let expected = super::super::stack_resources::context::read(slot)
                    .unwrap().scheduling_context.identity();
                let owner = super::super::terminal_quarantine::begin(
                    slot, Some(CUSTODY_ID), super::super::TaskRetireReason::Exited, false,
                ).unwrap();
                let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
                preemption.mask_irqs();
                // SAFETY: original modeled masked owner through actual terminal effects;
                // physical born context retained, no foreign execution or bank borrowers.
                unsafe { super::super::terminal_publication::guarded(owner, &|_| 1); }
                assert_eq!(reply_context_custody(CUSTODY_ID, expected), (true, false));
                let wrong = super::super::scheduling_context::SchedulingContext::derived_identity(
                    slot, CUSTODY_ID + 1,
                ).unwrap();
                assert_eq!(reply_context_custody(CUSTODY_ID, wrong), (false, false));
            },
            // Original external ACK is explicitly native modeled after bank release.
            super::super::terminal_publication::tests::release_published,
        );
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
        crate::multitask::current_identity::publish_test_identity(
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

        // A stale hint naming the resident's slot for a different task: the
        // identity check is the only thing that can reject it.
        record(stranger, slot);
        assert_eq!(live_slot(stranger), None);
        forget(stranger);
        assert_eq!(published_user_log_ids(stranger), None);

        // A cleared slot leaves the hint behind and must still not answer.
        crate::multitask::current_identity::clear(slot);
        assert_eq!(live_slot(resident), None);
        forget(resident);
    }

    #[test]
    fn colliding_task_ids_keep_their_own_slots_and_never_alias() {
        let _serial = serialized();
        // Ids that shared the old direct-mapped residue each keep their own way.
        let first = 4_098;
        let second = first + MAX_TASK as u64;
        record(first, 3);
        record(second, 11);
        assert_eq!(hint(first), Some(3));
        assert_eq!(hint(second), Some(11));
        // Forgetting one never clears or redirects the other.
        forget(first);
        assert_eq!(hint(first), None);
        assert_eq!(hint(second), Some(11));
        forget(second);
        // A full set evicts a way; an evicted id misses rather than aliasing.
        let ids: [u64; WAYS + 1] = core::array::from_fn(|i| {
            (0..).map(|n: u64| 9_000 + n).filter(|id| core::ptr::eq(set(*id), set(9_000)))
                .nth(i).unwrap()
        });
        for (slot, id) in ids.iter().enumerate() {
            record(*id, slot);
        }
        for (slot, id) in ids.iter().enumerate() {
            assert!(hint(*id).is_none_or(|hinted| hinted == slot));
            forget(*id);
        }
    }
}
