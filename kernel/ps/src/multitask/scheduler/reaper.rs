//! Owner: exact retired slot detachment and final external-completion ACK.
//! Boundary: Class50 reset + Identity51/Resources53 reuse + funding closure.
//! Lifecycle: desired stop and all bank/frame/MM pins drain before any reset effect.
//! Concurrency: per-slot seals, never the Scheduler directory or a substitute mutex.
//! Failure: partial/abandoned effects quarantine both rows; no vacancy-based completion.
//! Forbidden: user-stack external loans or constructor rollback as retired authority.
//! Evidence: actual terminal/resource seals remain held through external detach.
use super::{
    FIRST_DYNAMIC_TASK_SLOT, MAX_TASK, TaskContext, TaskRetireReason, current_identity,
    dispatch_policy, donation_ledger, frame_publication, linux_thread_state,
    reclaim::RetiredSlotReclaim, retirement_cleanup, runqueue, runtime_profile, scheduling_runtime,
    stack_resources,
};
use crate::multitask::current_identity::execution::kernel_executor::Executor;

struct Grant {
    slot: usize,
    id: u64,
    context: TaskContext,
    bounds: (u64, u64),
    reason: Option<TaskRetireReason>,
    reset: retirement_cleanup::reuse::ResetSeal,
    reuse: stack_resources::reuse::Reuse,
}
impl Grant {
    fn capture(slot: usize, active_root: Option<u64>) -> Option<Self> {
        if !(FIRST_DYNAMIC_TASK_SLOT..MAX_TASK).contains(&slot) {
            return None;
        }
        let id = stack_resources::task_start::read(slot)?.id;
        if !current_identity::terminal_retired_matches(slot, id)
            || crate::multitask::task_slot_is_running(slot)
            || runqueue::owner(slot).state != runqueue::RunOwnerState::Retired
        {
            return None;
        }
        let context = stack_resources::context::read(slot)?;
        if context.user_mode && Some(runqueue::address_space::root(slot)) == active_root {
            return None;
        }
        // The terminal seal excludes producers throughout resource capture and teardown.
        // Failure rolls back only this untouched seal; irreversible funding close
        // retains desired-stop custody and will be retried after interval release.
        let reset = retirement_cleanup::reuse::begin(slot, Some(id))?;
        if !scheduling_runtime::close_for_reclaim(slot, context.scheduling_context.identity()) {
            return None;
        }
        let reuse = stack_resources::reuse::begin_retired(slot, id)?;
        let captured = stack_resources::context::read(slot).expect("retired grant lost context");
        assert_eq!(
            captured.scheduling_context.identity(),
            context.scheduling_context.identity(),
            "retired grant crossed funding replacement"
        );
        assert_eq!(
            captured.process_handle, context.process_handle,
            "retired grant crossed process replacement"
        );
        Some(Self {
            slot,
            id,
            context: captured,
            bounds: runqueue::kernel_stack_bounds(slot),
            reason: retirement_cleanup::reason(slot),
            reset,
            reuse,
        })
    }
    fn detach(mut self) -> Detached {
        // No leaf is retained across these effects. Both owned seals keep the
        // exact original generation closed until external completion, not only
        // until the metadata happens to become empty.
        self.reset.mark_effects();
        self.reset.reset();
        donation_ledger::release_task(self.id);
        runqueue::release_retired(self.slot);
        scheduling_runtime::release(self.slot, self.context.scheduling_context.identity());
        stack_resources::context::clear(self.slot);
        linux_thread_state::clear_reclaimed(self.slot);
        dispatch_policy::remove_reclaimed_slot(self.slot);
        for gate in [
            current_identity::LifecycleGate::Retired,
            current_identity::LifecycleGate::ExecQuiesced,
            current_identity::LifecycleGate::Suspended,
            current_identity::LifecycleGate::JobStopped,
        ] {
            current_identity::publish_lifecycle_change(self.slot, None, gate, false);
        }
        runqueue::simd_tls::reset_simd_state(self.slot);
        stack_resources::task_start::clear(self.slot);
        frame_publication::clear(self.slot);
        current_identity::publish_metadata(self.slot, None);
        runtime_profile::owner::clear_history(self.slot);
        let stack = self.reuse.take_storage();
        let reclaim = RetiredSlotReclaim::new(
            self.context.process_handle,
            stack,
            self.id,
            self.slot,
            self.context.user_mode,
            self.bounds.0,
            self.bounds.1,
            self.reason,
        );
        Detached {
            reclaim,
            reset: self.reset,
            reuse: self.reuse,
        }
    }
}
struct Detached {
    reclaim: RetiredSlotReclaim,
    reset: retirement_cleanup::reuse::ResetSeal,
    reuse: stack_resources::reuse::Reuse,
}
impl Detached {
    fn finish_with(self, effects: impl FnOnce(RetiredSlotReclaim)) {
        effects(self.reclaim);
        // External process/stack destruction must finish before either reopening ACK.
        self.reset.finish();
        self.reuse.finish();
    }
}
/// External completion borrows kill-safe actual kernel Current, never a user loan.
pub(in crate::multitask) struct Reap<'a> {
    executor: &'a Executor,
    detached: Detached,
}
impl Reap<'_> {
    pub(in crate::multitask) fn complete(self) {
        self.executor.assert_current();
        self.detached.finish_with(|reclaim| {
            reclaim.complete();
            self.executor.assert_current();
        });
    }
}
pub(in crate::multitask) fn claim_next(executor: &Executor) -> Option<Reap<'_>> {
    executor.assert_current();
    if !retirement_cleanup::take_reap_pending() {
        return None;
    }
    let current = crate::multitask::cpu_local::current_cpu_task_slot()?;
    let active_root =
        stack_resources::context::read(current).map(|_| runqueue::address_space::root(current));
    let mut pending = false;
    for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
        let Some(start) = stack_resources::task_start::read(slot) else {
            continue;
        };
        if !current_identity::terminal_retired_matches(slot, start.id) {
            continue;
        }
        pending = true;
        if let Some(grant) = Grant::capture(slot, active_root) {
            retirement_cleanup::mark_reap_pending();
            return Some(Reap {
                executor,
                detached: grant.detach(),
            });
        }
    }
    if pending {
        retirement_cleanup::mark_reap_pending();
    }
    None
}

#[cfg(test)]
pub(super) mod tests;
