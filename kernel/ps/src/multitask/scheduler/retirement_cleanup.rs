//! Independent terminal custody for exact task slots.
//!
//! The directory freezes terminal metadata before publishing cleanup and kernel
//! revocation together. One per-slot owner holds deferred intent, reason and
//! both claim/ACK transactions. Consumers release it before IPC, user faults,
//! process detach, wake or other owner work. Neither claimed transaction permits
//! reset/reuse. A kernel claim is private, non-Copy and generation-bound; its ACK
//! is the last operation after external revocation. The directory still owns
//! execution/stack quiescence, not this token bank.

use core::sync::atomic::{AtomicBool, Ordering};
use nucleus_core::util::lockdep::LockClass;
use crate::multitask::irq_leaf::IrqLeaf;

use super::{FIRST_DYNAMIC_TASK_SLOT, MAX_TASK, TaskRetireReason, reclaim::RetirementSideEffect};
use crate::multitask::RetiredTaskCleanup;

pub(in crate::multitask) mod executor;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CleanupState {
    Vacant,
    Pending(RetiredTaskCleanup),
    Claimed(RetiredTaskCleanup),
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KernelState {
    Vacant,
    Pending(RetirementSideEffect),
    Claimed(RetirementSideEffect),
    Complete,
}

struct SlotState {
    generation: u64,
    kernel_done_generation: u64,
    task_id: Option<u64>,
    reason: Option<TaskRetireReason>,
    deferred: Option<TaskRetireReason>,
    producing: bool,
    cleanup: CleanupState,
    kernel: KernelState,
    reset_seal: Option<u64>,
    reset_nonce: u64,
    producer: Option<producer::Record>,
    producer_nonce: u64,
}

impl SlotState {
    const fn new() -> Self {
        Self {
            generation: 0,
            kernel_done_generation: 0,
            task_id: None,
            reason: None,
            deferred: None,
            producing: false,
            cleanup: CleanupState::Vacant,
            kernel: KernelState::Vacant,
            reset_seal: None,
            reset_nonce: 0,
            producer: None,
            producer_nonce: 0,
        }
    }
}

/// Negative native fault: isolate an unconsumed kernel payload from all other
/// refusal gates. This cannot ACK, retire, detach or grant an image; original
/// target backing and its intent/census stay retained by the enclosing probe.
#[cfg(test)]
pub(in crate::multitask::scheduler) fn test_with_pending_kernel<R>(
    slot: usize,
    id: u64,
    f: impl FnOnce() -> R,
) -> R {
    struct Restore(usize, u64);
    impl Drop for Restore {
        fn drop(&mut self) {
            let mut state = CELLS[self.0].state.lock();
            assert_eq!(state.kernel, KernelState::Pending(RetirementSideEffect::new(Some(self.1), None)));
            state.kernel = KernelState::Vacant;
        }
    }
    {
        let mut state = CELLS[slot].state.lock();
        assert_eq!(state.kernel, KernelState::Vacant);
        assert_eq!(state.cleanup, CleanupState::Vacant);
        assert!(state.reason.is_none() && state.deferred.is_none() && state.producer.is_none());
        state.kernel = KernelState::Pending(RetirementSideEffect::new(Some(id), None));
    }
    let _restore = Restore(slot, id);
    f()
}

/// Capture only an exact Identity51 -> Resources53 lifetime pin while the
/// terminal reservation leaf excludes a new stop/reset producer. The callback
/// must not perform queue, process, notification or external work.
pub(in crate::multitask) fn with_wake_binding<R>(
    slot: usize,
    capture: impl FnOnce() -> R,
) -> Option<R> {
    let row = CELLS.get(slot)?.state.lock();
    if row.reset_seal.is_some()
        || row.producer.is_some()
        || row.producing
        || row.deferred.is_some()
        || row.reason.is_some()
        || row.cleanup != CleanupState::Vacant
        || row.kernel != KernelState::Vacant
    {
        return None;
    }
    Some(capture())
}

struct CleanupCell {
    state: IrqLeaf<SlotState, { LockClass::SchedulerRetirementCleanup as u8 }>,
}

impl CleanupCell {
    const fn new() -> Self {
        Self {
            state: IrqLeaf::new(SlotState::new()),
        }
    }

    #[cfg(test)]
    fn publish(&self, cleanup: RetiredTaskCleanup) {
        let mut slot = self.state.lock();
        let state = &mut slot.cleanup;
        assert_eq!(
            *state,
            CleanupState::Vacant,
            "cleanup published before prior slot reset"
        );
        *state = CleanupState::Pending(cleanup);
    }

    #[cfg(test)]
    fn publish_terminal(
        &self,
        task_id: Option<u64>,
        reason: TaskRetireReason,
        cleanup: Option<RetiredTaskCleanup>,
        effects: RetirementSideEffect,
    ) {
        let mut state = self.state.lock();
        assert!(
            state.reset_seal.is_none() && state.producer.is_none(),
            "terminal publication crossed sealed reset"
        );
        assert_eq!(
            state.cleanup,
            CleanupState::Vacant,
            "cleanup published before prior slot reset"
        );
        assert_eq!(
            state.kernel,
            KernelState::Vacant,
            "kernel retirement published twice before reset"
        );
        assert!(
            state.task_id.is_none() || state.task_id == task_id,
            "retirement intent changed task identity"
        );
        assert!(
            cleanup.is_none_or(|payload| Some(payload.task_id) == task_id),
            "cleanup identity differs from terminal producer"
        );
        assert_eq!(
            effects.task_id(),
            task_id,
            "kernel revocation identity differs from terminal producer"
        );
        state.generation = state
            .generation
            .checked_add(1)
            .expect("retirement generation exhausted");
        state.task_id = task_id;
        state.reason = Some(reason);
        state.deferred = None;
        state.producing = false;
        state.cleanup = cleanup
            .map(CleanupState::Pending)
            .unwrap_or(CleanupState::Complete);
        state.kernel = KernelState::Pending(effects);
    }

    #[cfg(test)]
    fn defer(&self, task_id: Option<u64>, reason: TaskRetireReason) {
        let mut state = self.state.lock();
        assert!(
            state.reset_seal.is_none() && state.producer.is_none(),
            "deferred publication crossed sealed reset"
        );
        assert!(
            state.task_id.is_none() || state.task_id == task_id,
            "deferred retirement changed task identity"
        );
        assert_eq!(
            state.kernel,
            KernelState::Vacant,
            "finalized retirement cannot be deferred again"
        );
        state.task_id = task_id;
        state.reason = Some(reason);
        state.deferred = Some(reason);
        state.producing = false;
    }

    #[cfg(test)]
    fn take_deferred(&self) -> Option<TaskRetireReason> {
        let mut state = self.state.lock();
        assert!(
            state.producer.is_none(),
            "unowned deferred take crossed exact producer"
        );
        let reason = state.deferred.take()?;
        // The producer retains custody while assembling its final payload;
        // an empty deferred queue is not permission to reset this slot.
        state.producing = true;
        Some(reason)
    }

    #[cfg(test)]
    fn abort_empty_producer(&self) {
        let mut state = self.state.lock();
        assert!(
            state.producer.is_none(),
            "unowned abort crossed exact producer"
        );
        assert_eq!(state.cleanup, CleanupState::Vacant);
        assert_eq!(state.kernel, KernelState::Vacant);
        state.task_id = None;
        state.reason = None;
        state.deferred = None;
        state.producing = false;
    }

    fn claim(&self) -> Option<RetiredTaskCleanup> {
        let mut slot = self.state.lock();
        if slot.producer.is_some() {
            return None;
        }
        let state = &mut slot.cleanup;
        let CleanupState::Pending(cleanup) = *state else {
            return None;
        };
        *state = CleanupState::Claimed(cleanup);
        Some(cleanup)
    }

    fn complete(&self, cleanup: RetiredTaskCleanup) -> bool {
        let mut slot = self.state.lock();
        let state = &mut slot.cleanup;
        let CleanupState::Claimed(expected) = *state else {
            return false;
        };
        if !rustos_scheduler_frame_gate::retirement_cleanup_ack_custody(
            expected.task_id,
            cleanup.task_id,
            true,
            expected == cleanup,
        ) {
            return false;
        }
        *state = CleanupState::Complete;
        true
    }

    fn claim_kernel(&self) -> Option<(u64, RetirementSideEffect)> {
        let mut state = self.state.lock();
        if state.producer.is_some() {
            return None;
        }
        let KernelState::Pending(effects) = state.kernel else {
            return None;
        };
        state.kernel = KernelState::Claimed(effects);
        Some((state.generation, effects))
    }

    fn complete_kernel(&self, generation: u64, effects: RetirementSideEffect) -> bool {
        let mut state = self.state.lock();
        if state.generation != generation || state.kernel != KernelState::Claimed(effects) {
            return false;
        }
        state.kernel_done_generation = generation;
        state.kernel = KernelState::Complete;
        true
    }

    fn pending(&self) -> bool {
        matches!(
            self.state.lock().cleanup,
            CleanupState::Pending(_) | CleanupState::Claimed(_)
        )
    }

    fn kernel_pending(&self) -> bool {
        let state = self.state.lock();
        state.producer.is_some()
            || state.producing
            || state.deferred.is_some()
            || matches!(
                state.kernel,
                KernelState::Pending(_) | KernelState::Claimed(_)
            )
    }

    fn reset(&self) {
        let mut slot = self.state.lock();
        assert!(
            slot.reset_seal.is_none(),
            "unowned terminal reset crossed sealed lifetime"
        );
        assert!(
            slot.producer.is_none() && !slot.producing && slot.deferred.is_none(),
            "slot reset while retirement producer owns metadata"
        );
        assert!(
            matches!(slot.kernel, KernelState::Vacant | KernelState::Complete),
            "slot reset before kernel retirement ACK"
        );
        let state = &mut slot.cleanup;
        assert!(
            matches!(*state, CleanupState::Vacant | CleanupState::Complete),
            "slot cleanup reset before exact external ACK"
        );
        *state = CleanupState::Vacant;
        slot.kernel = KernelState::Vacant;
        slot.task_id = None;
        slot.reason = None;
        // Keep the generation: an old private kernel token cannot ACK reuse.
    }
}

static CELLS: [CleanupCell; MAX_TASK] = [const { CleanupCell::new() }; MAX_TASK];
static REAP_PENDING: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub(super) fn publish_terminal(
    slot: usize,
    task_id: Option<u64>,
    reason: TaskRetireReason,
    cleanup: Option<RetiredTaskCleanup>,
    effects: RetirementSideEffect,
) {
    assert!((FIRST_DYNAMIC_TASK_SLOT..MAX_TASK).contains(&slot));
    CELLS[slot].publish_terminal(task_id, reason, cleanup, effects);
    mark_reap_pending();
}

#[cfg(test)]
pub(super) fn defer(slot: usize, task_id: Option<u64>, reason: TaskRetireReason) {
    CELLS[slot].defer(task_id, reason);
    mark_reap_pending();
}

pub(super) fn deferred(slot: usize) -> Option<TaskRetireReason> {
    CELLS[slot].state.lock().deferred
}

#[cfg(test)]
pub(super) fn take_deferred(slot: usize) -> Option<TaskRetireReason> {
    CELLS[slot].take_deferred()
}

#[cfg(test)]
pub(super) fn abort_empty_producer(slot: usize) {
    CELLS[slot].abort_empty_producer();
}

pub(super) fn reason(slot: usize) -> Option<TaskRetireReason> {
    CELLS[slot].state.lock().reason
}

#[cfg(test)]
pub(super) fn test_kernel_detach_payload(slot: usize) -> Option<Option<super::ProcessHandle>> {
    match CELLS[slot].state.lock().kernel {
        KernelState::Pending(payload) | KernelState::Claimed(payload) => Some(payload.test_detach_payload()),
        _ => None,
    }
}

#[cfg(test)]
pub(super) fn test_cleanup_payload(slot: usize) -> Option<RetiredTaskCleanup> {
    match CELLS[slot].state.lock().cleanup {
        CleanupState::Pending(payload) | CleanupState::Claimed(payload) => Some(payload),
        _ => None,
    }
}

pub(in crate::multitask) fn claim_next() -> Option<RetiredTaskCleanup> {
    (FIRST_DYNAMIC_TASK_SLOT..MAX_TASK).find_map(|slot| CELLS[slot].claim())
}

pub(in crate::multitask) fn complete(cleanup: RetiredTaskCleanup) -> bool {
    let complete = (FIRST_DYNAMIC_TASK_SLOT..MAX_TASK).any(|slot| CELLS[slot].complete(cleanup));
    if complete {
        mark_reap_pending();
    }
    complete
}

/// Native row ACK only; this does not authorize or simulate external effects.
#[cfg(test)]
pub(super) fn ack_kernel_fixture(slot: usize) {
    let (generation, effects) = CELLS[slot].claim_kernel().unwrap();
    assert!(CELLS[slot].complete_kernel(generation, effects));
}

/// Private non-Copy claim; kernel ACK cannot precede external revocation.
pub(in crate::multitask) struct KernelClaim<'a> {
    executor: &'a executor::Executor,
    slot: usize,
    generation: u64,
    effects: RetirementSideEffect,
}

impl KernelClaim<'_> {
    pub(in crate::multitask) fn complete(self, wake_task: impl FnMut(u64)) {
        self.complete_with(|effects| effects.complete(wake_task));
    }
    /// Shared original executor/claim/ACK engine. Only the external consumer
    /// work is supplied; no raw leaf or dispatch scope spans that work.
    fn complete_with(self, external: impl FnOnce(RetirementSideEffect)) {
        self.executor.assert_current();
        external(self.effects);
        self.executor.assert_current();
        let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
        preemption.mask_irqs();
        assert!(
            CELLS[self.slot].complete_kernel(self.generation, self.effects),
            "kernel retirement lost its exact claim"
        );
        mark_reap_pending();
    }
    /// Native hardware/IPC consumer completion input, NOT a replacement claim
    /// or ACK. The same non-Copy kernel executor and original row generation
    /// must survive actual process detach through the shared final consumer.
    #[cfg(test)]
    pub(in crate::multitask::scheduler) fn complete_modeled_external(self,
        external: impl FnOnce(&RetirementSideEffect)) {
        self.complete_with(|effects| effects.complete_external_with(external));
    }
}

pub(in crate::multitask) fn claim_next_kernel(
    executor: &executor::Executor,
) -> Option<KernelClaim<'_>> {
    executor.assert_current();
    (FIRST_DYNAMIC_TASK_SLOT..MAX_TASK).find_map(|slot| {
        CELLS[slot]
            .claim_kernel()
            .map(|(generation, effects)| KernelClaim {
                executor,
                slot,
                generation,
                effects,
            })
    })
}

pub(super) fn pending(slot: usize) -> bool {
    CELLS[slot].pending()
}
pub(super) fn kernel_pending(slot: usize) -> bool {
    CELLS[slot].kernel_pending()
}

/// Consume work before scanning; never overwrite a concurrent producer/ACK.
pub(super) fn take_reap_pending() -> bool {
    REAP_PENDING.swap(false, Ordering::AcqRel)
}
pub(in crate::multitask) fn mark_reap_pending() {
    REAP_PENDING.store(true, Ordering::Release);
}

/// Read-only actual publication diagnostics, never a consumer claim or ACK.
#[cfg(test)]
pub(super) fn test_consumer_publication(slot: usize) -> (bool, bool) {
    let row = CELLS[slot].state.lock();
    (
        matches!(row.cleanup, CleanupState::Pending(_) | CleanupState::Claimed(_)),
        matches!(row.kernel, KernelState::Pending(_) | KernelState::Claimed(_)),
    )
}

/// Directory/reaper only, before any runtime, frame or slot identity teardown.
pub(super) fn reset(slot: usize) {
    CELLS[slot].reset();
}

pub(in crate::multitask) mod producer;
pub(super) mod reuse;
#[cfg(test)]
mod tests;
