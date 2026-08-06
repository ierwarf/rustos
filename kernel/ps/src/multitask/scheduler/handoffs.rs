//! Scheduler-side custody for committed activation and synchronous IPC turns.
//!
//! - **Owner:** `kernel-ps` owns exact task-slot handoff admission/selection.
//! - **Boundary:** Callers provide task identities after activation, call
//!   enqueue with a live reply capability, or successful reply consumption.
//! - **Lifecycle:** Admit a deduplicated slot, retain it across fairness,
//!   dispatch FIFO, or remove it during exact slot retirement.
//! - **Concurrency:** lifecycle publication uses the catalog guard, while each
//!   handoff queue and fairness streak belongs to the target logical CPU.
//! - **Failure:** Impossible queue capacity panics instead of losing authority.
//! - **Forbidden:** No allocation, overwrite, class fabrication, or stale-slot
//!   transfer.
//! - **Evidence:** `atomic-process-activation-batch`,
//!   `bootstrap-activation-handoff`, and `synchronous-ipc-handoff`.

use super::{
    CpuDispatchGuard, MAX_ATOMIC_ACTIVATION_HANDOFFS, MAX_CONSECUTIVE_SYNC_HANDOFFS, Scheduler,
    TaskContext,
};

impl Scheduler {
    /// Publishes the geometry and contents behind a rejected activation frame.
    ///
    /// Emitted as milestones because the ordinary log channel does not reach
    /// the debug transport in the product configuration, and because the panic
    /// that follows must not depend on formatting to be diagnosable.
    fn report_invalid_activation_context(&self, task_id: u64, slot: usize, context: &TaskContext) {
        let saved_rsp = context.saved_rsp as u64;
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "sched-activation-invalid-slot",
            (task_id << 32) | slot as u64,
            saved_rsp,
        );
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "sched-activation-invalid-stack",
            context.kernel_stack_base,
            context.kernel_stack_top,
        );
        let (rflags, cs, rip, rsp) = match Self::saved_context_ref(context.saved_rsp) {
            Some(saved) => (saved.rflags, saved.cs, saved.rip, saved.rsp),
            None => (0, 0, 0, 0),
        };
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "sched-activation-invalid-frame",
            rflags,
            (cs << 32) | (rip & u64::from(u32::MAX)),
        );
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "sched-activation-invalid-frame-rsp",
            rsp,
            u64::from(self.stack_frame_is_all_zero(context.saved_rsp)),
        );
    }

    pub(in crate::multitask) fn activate_suspended_user_task(&mut self, task_id: u64) -> bool {
        self.activate_suspended_user_tasks(core::slice::from_ref(&task_id))
    }

    pub(in crate::multitask) fn activate_suspended_user_tasks_with_commit<F>(
        &mut self,
        task_ids: &[u64],
        commit_authority: F,
    ) -> bool
    where
        F: FnOnce(),
    {
        if !self.preflight_suspended_user_tasks(task_ids) {
            return false;
        }

        // LIFECYCLE: the caller owns the outer capability registry and the
        // documented ProcBrokerRegistry -> Scheduler lock order. Consuming
        // the exact one-shot authority is intentionally before publication:
        // publishing first would leave a runnable child with live activation
        // authority. The post-callback assertion is the executable boundary;
        // moving publication ahead of authority consumption panics while both
        // owners are still held rather than exposing a partial cohort.
        // The callback must be bounded, infallible, allocation-free registry
        // mutation; a panic leaves every target suspended and fails closed.
        commit_authority();
        self.assert_authority_commit_preserved_suspension(task_ids);
        self.publish_suspended_user_tasks(task_ids);
        true
    }

    /// Atomically publishes a bounded supervisor-selected sibling set.
    ///
    /// The first pass is a rollback-free preflight under the global scheduler
    /// lock. No task becomes runnable until every exact target is still a
    /// disjoint, suspended user task with a valid saved context. The second
    /// pass cannot fail without ring0 state corruption, so such a mismatch is
    /// fatal rather than leaving a partially activated startup cohort.
    pub(in crate::multitask) fn activate_suspended_user_tasks(&mut self, task_ids: &[u64]) -> bool {
        self.activate_suspended_user_tasks_with_commit(task_ids, || {})
    }

    fn preflight_suspended_user_tasks(&self, task_ids: &[u64]) -> bool {
        if task_ids.is_empty() {
            return false;
        }
        for (index, task_id) in task_ids.iter().copied().enumerate() {
            if task_id == 0 || task_ids[..index].contains(&task_id) {
                return false;
            }
            let Some(slot) = self.find_user_task_slot(task_id) else {
                return false;
            };
            if self.retired[slot] || !self.start_suspended[slot] {
                return false;
            }
            let Some(context) = self.contexts[slot] else {
                panic!("scheduler activation invariant: suspended task {task_id} lost its context");
            };
            assert!(
                slot != self.current_task_slot() && !super::super::task_slot_is_running(slot),
                "scheduler activation invariant: suspended task {task_id} is already running"
            );
            if let Err(reason) =
                self.validate_saved_context(slot, context.user_mode, context.saved_rsp)
            {
                // The reason alone cannot separate "the frame was never
                // written" from "the frame was written somewhere else" from
                // "something overwrote it". Report the exact slot geometry and
                // the words actually present so the next occurrence is
                // attributable without another run.
                self.report_invalid_activation_context(task_id, slot, &context);
                panic!(
                    "scheduler activation invariant: suspended task {task_id} has invalid context: {reason}"
                );
            }
        }

        assert!(
            task_ids.len() <= MAX_ATOMIC_ACTIVATION_HANDOFFS,
            "scheduler activation invariant: cohort exceeds bounded first-turn custody"
        );
        true
    }

    /// Checks the non-observable interior of a registry-to-scheduler commit.
    ///
    /// The callback may consume only its outer one-shot authority. It must
    /// not make a preflighted task runnable, retire it, or otherwise change
    /// scheduler custody before this method publishes the complete cohort.
    /// A violation is kernel corruption: returning an error after authority
    /// consumption would leave no userspace recovery path.
    fn assert_authority_commit_preserved_suspension(&self, task_ids: &[u64]) {
        for task_id in task_ids.iter().copied() {
            let slot = self
                .find_user_task_slot(task_id)
                .expect("scheduler activation invariant: preflight target disappeared after authority commit");
            assert!(
                !self.retired[slot] && self.start_suspended[slot],
                "scheduler activation invariant: authority commit changed suspension before cohort publication for task {task_id}"
            );
            assert!(
                slot != self.current_task_slot() && !super::super::task_slot_is_running(slot),
                "scheduler activation invariant: authority commit made task {task_id} runnable before cohort publication"
            );
        }
    }

    fn publish_suspended_user_tasks(&mut self, task_ids: &[u64]) {
        let prioritize_atomic_cohort = task_ids.len() > 1;
        if prioritize_atomic_cohort {
            assert!(
                self.cpu_dispatch.iter().all(|policy| {
                    let policy = policy.lock();
                    policy.atomic_activation_pick_hints.is_empty()
                        && policy.atomic_activation_handoff_remaining == 0
                }),
                "scheduler activation invariant: overlapping atomic cohorts"
            );
        }
        for task_id in task_ids.iter().copied() {
            let slot = self
                .find_user_task_slot(task_id)
                .expect("scheduler activation preflight target disappeared");
            self.start_suspended[slot] = false;
            assert!(
                self.wake_task_slot(slot),
                "scheduler activation invariant: preflighted task {task_id} could not wake"
            );
            // Activation is the supervisor's commit point. The cohort FIFO is
            // disjoint from ordinary thread-spawn custody, so an unrelated
            // startup burst cannot consume or disable these exact first turns.
            if prioritize_atomic_cohort {
                let target_cpu = self.slot_dispatch_cpu(slot);
                let mut policy = self.cpu_dispatch[target_cpu].lock();
                let inserted = policy
                    .atomic_activation_pick_hints
                    .enqueue(slot)
                    .expect("scheduler atomic activation queue overflow");
                assert!(
                    inserted,
                    "scheduler activation invariant: duplicate cohort slot"
                );
                policy.atomic_activation_handoff_remaining = policy
                    .atomic_activation_handoff_remaining
                    .checked_add(1)
                    .expect("scheduler atomic activation handoff count overflow");
            } else {
                self.set_next_spawn_pick_hint(task_id);
            }
        }
    }

    /// Retains every exact peer required by a committed synchronous call or
    /// successful terminal reply. Concurrent transfers on other CPUs must not
    /// overwrite older custody.
    pub(in crate::multitask) fn set_next_synchronous_pick_hint(&mut self, task_id: u64) -> bool {
        let Some(slot) = self.find_task_slot(task_id) else {
            return false;
        };
        if !self.handoff_slot_ready(slot) {
            return false;
        }
        let target_cpu = self.slot_dispatch_cpu(slot);
        self.cpu_dispatch[target_cpu]
            .lock()
            .sync_pick_hints
            .enqueue(slot)
            .expect("scheduler synchronous IPC handoff queue overflow");
        true
    }

    pub(in crate::multitask) fn set_next_spawn_pick_hint(&mut self, task_id: u64) {
        let Some(slot) = self.find_task_slot(task_id) else {
            return;
        };
        let Some(context) = self.contexts[slot] else {
            return;
        };
        if !self.slot_is_runnable(slot) || !self.handoff_slot_ready(slot) {
            return;
        }
        let target_cpu = self.slot_dispatch_cpu(slot);
        let inserted = self.cpu_dispatch[target_cpu]
            .lock()
            .spawn_pick_hints
            .enqueue(slot)
            .expect("scheduler spawn handoff queue overflow");
        if !inserted {
            return;
        }
        if target_cpu == Self::current_dispatch_cpu() {
            self.apply_ipc_donation(slot);
        }
    }

    /// Select the one-shot child-start transfer before an ordinary wakeup. A child
    /// reaches this slot only after its exact supervisor consumed the
    /// deferred-activation capability, so this order cannot open an
    /// unsupervised execution window. It does prevent a busy wakeup stream
    /// from stretching a committed bootstrap into whole seconds.
    pub(super) fn take_next_bootstrap_handoff_ready_slot(
        &self,
        policy: &mut CpuDispatchGuard<'_>,
    ) -> Option<(usize, bool)> {
        self.take_next_spawn_pick_hint_ready_slot(policy)
            .map(|slot| (slot, false))
            .or_else(|| {
                self.take_next_latency_pick_hint_ready_slot(policy)
                    .map(|slot| (slot, true))
            })
    }

    pub(super) fn take_next_spawn_pick_hint_ready_slot(
        &self,
        policy: &mut CpuDispatchGuard<'_>,
    ) -> Option<usize> {
        while let Some(hint) = policy.spawn_pick_hints.pop() {
            if let Some(slot) = self.pick_hint_candidate_slot(Some(hint)) {
                return Some(slot);
            }
        }
        None
    }

    /// Dispatches only the cohort members covered by one atomic activation
    /// commit. Ordinary thread spawns live in a disjoint FIFO and therefore
    /// cannot consume this custody or disable the bounded first-turn prefix.
    pub(super) fn take_next_atomic_activation_handoff_ready_slot(
        &self,
        policy: &mut CpuDispatchGuard<'_>,
    ) -> Option<usize> {
        assert!(
            policy.atomic_activation_handoff_remaining <= MAX_ATOMIC_ACTIVATION_HANDOFFS,
            "scheduler atomic activation handoff bound corrupted"
        );
        while policy.atomic_activation_handoff_remaining > 0 {
            policy.atomic_activation_handoff_remaining -= 1;
            let Some(hint) = policy.atomic_activation_pick_hints.pop() else {
                policy.atomic_activation_handoff_remaining = 0;
                return None;
            };
            if let Some(slot) = self.pick_hint_candidate_slot(Some(hint)) {
                return Some(slot);
            }
        }
        None
    }

    pub(super) fn take_next_synchronous_pick_hint_ready_slot(
        &self,
        policy: &mut CpuDispatchGuard<'_>,
    ) -> Option<usize> {
        if policy.sync_handoff_streak >= MAX_CONSECUTIVE_SYNC_HANDOFFS {
            return None;
        }
        while let Some(hint) = policy.sync_pick_hints.pop() {
            if let Some(slot) = self.pick_hint_candidate_slot(Some(hint)) {
                return Some(slot);
            }
        }
        None
    }

    pub(super) fn record_synchronous_handoff(&mut self, synchronous_handoff: bool) {
        let next = if synchronous_handoff {
            self.current_dispatch_policy()
                .sync_handoff_streak
                .saturating_add(1)
                .min(MAX_CONSECUTIVE_SYNC_HANDOFFS)
        } else {
            0
        };
        self.current_dispatch_policy_mut().sync_handoff_streak = next;
    }
}
