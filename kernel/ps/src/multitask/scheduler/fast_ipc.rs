//! Scheduler dispatch leaf for an already-committed synchronous IPC handoff.

#[cfg(not(test))]
use super::runqueue;
use super::{SchedClass, Scheduler, SchedulerDispatch, dispatch_policy};

impl Scheduler {
    pub(in crate::multitask) fn dispatch_committed_fast_ipc_handoff(
        &mut self,
        current_rsp: usize,
        immediate_task_id: Option<u64>,
    ) -> Option<SchedulerDispatch> {
        let dispatch_cpu = nucleus_core::util::lockdep::current_cpu_index();
        let current_slot = self.current_task_slot();
        if !(self.retired[current_slot] || self.slot_blocked(current_slot))
            || dispatch_policy::atomic_activation_pending(Self::current_dispatch_cpu())
        {
            return None;
        }
        let next_idx = match immediate_task_id {
            Some(task_id) => self.immediate_synchronous_handoff_ready_slot(task_id)?,
            None => self.take_next_synchronous_pick_hint_ready_slot()?,
        };
        let next = self.contexts[next_idx]?;
        let next_saved_rsp = self.slot_saved_rsp(next_idx);
        if let Some(reason) = self.context_validation_error(next_idx, next, next_saved_rsp) {
            self.log_invalid_context(next_idx, next_saved_rsp, reason, "fast-ipc-next");
            self.retire_slot_due_to_invalid_context(next_idx, next_saved_rsp, reason);
            return None;
        }
        if !self.commit_scheduling_budget_for_dispatch(next_idx) {
            if immediate_task_id.is_some() {
                #[cfg(not(test))]
                let _ = runqueue::materialize_direct_handoff(
                    next_idx,
                    dispatch_cpu,
                    self.slot_weight(next_idx),
                );
            } else {
                let _ = self.enqueue_synchronous_handoff_slot(next_idx);
            }
            return None;
        }

        let current_task_id = self.starts[current_slot]
            .map(|start| start.id)
            .expect("running fast IPC sender missing scheduler identity");
        let next_task_id = self.starts[next_idx]
            .map(|start| start.id)
            .expect("fast IPC peer missing scheduler identity");
        let now_ticks = crate::arch::rtc::ticks();
        self.account_current_runtime(current_slot, now_ticks, true);
        self.mark_slot_ready(current_slot, current_rsp, false);

        #[cfg(not(test))]
        {
            let owner = runqueue::owner(current_slot);
            assert_eq!(
                owner.state,
                runqueue::RunOwnerState::Running,
                "committed fast IPC sender lost running custody slot={current_slot} owner={owner:?}"
            );
            runqueue::publish_blocked(current_slot, dispatch_cpu, self.slot_weight(current_slot));
            assert!(
                runqueue::claim_dispatch(next_idx, dispatch_cpu, self.slot_weight(next_idx)),
                "committed fast IPC peer lost direct runqueue custody slot={next_idx} cpu={dispatch_cpu}"
            );
        }

        self.trace_switch(current_slot, next_idx);
        #[cfg(test)]
        if let Some(context) = self.contexts[next_idx].as_mut() {
            context.test_ready = false;
        }
        self.set_slot_ready_since_ticks(next_idx, 0);
        self.set_slot_exec_start_ticks(next_idx, now_ticks);
        self.record_dispatch_streaks(next_idx, false);
        self.record_runtime_profile_dispatch(next_idx);
        self.record_runtime_profile_transition(current_slot, next_idx, dispatch_cpu);
        self.record_task_dispatch_cpu(next_idx, dispatch_cpu);
        self.record_synchronous_handoff(true, now_ticks);
        self.set_current_task_slot(next_idx);
        nucleus_core::util::lockdep::record_scheduler_dispatch(
            dispatch_cpu,
            current_task_id,
            next_task_id,
            current_slot,
            next_idx,
            next_saved_rsp,
            self.slot_class(next_idx) == Some(SchedClass::Idle),
            false,
        );
        nucleus_core::util::lockdep::set_current_task_owner(
            next_task_id
                .checked_add(1)
                .expect("task id exhausted lock owner token"),
        );
        Some(SchedulerDispatch::new(
            next_saved_rsp,
            self.scheduler_tick_divisor,
            current_slot,
            next_idx,
        ))
    }
}
