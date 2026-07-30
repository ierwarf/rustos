//! Scheduler-side custody for committed activation and synchronous IPC turns.
//!
//! - **Owner:** `kernel-ps` owns exact task-slot handoff admission/selection.
//! - **Boundary:** Callers provide task identities after activation, call
//!   enqueue with a live reply capability, or successful reply consumption.
//! - **Lifecycle:** Admit a deduplicated slot, retain it across fairness,
//!   dispatch FIFO, or remove it during exact slot retirement.
//! - **Concurrency:** The enclosing global scheduler guard serializes access.
//! - **Failure:** Impossible queue capacity panics instead of losing authority.
//! - **Forbidden:** No allocation, overwrite, class fabrication, or stale-slot
//!   transfer.
//! - **Evidence:** `atomic-process-activation-batch`,
//!   `bootstrap-activation-handoff`, and `synchronous-ipc-handoff`.

use super::{MAX_ATOMIC_ACTIVATION_HANDOFFS, MAX_CONSECUTIVE_SYNC_HANDOFFS, Scheduler};

impl Scheduler {
    pub(in crate::multitask) fn activate_suspended_user_task(&mut self, task_id: u64) -> bool {
        self.activate_suspended_user_tasks(core::slice::from_ref(&task_id))
    }

    /// Atomically publishes a bounded supervisor-selected sibling set.
    ///
    /// The first pass is a rollback-free preflight under the global scheduler
    /// lock. No task becomes runnable until every exact target is still a
    /// disjoint, suspended user task with a valid saved context. The second
    /// pass cannot fail without ring0 state corruption, so such a mismatch is
    /// fatal rather than leaving a partially activated startup cohort.
    pub(in crate::multitask) fn activate_suspended_user_tasks(&mut self, task_ids: &[u64]) -> bool {
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
                slot != self.current_task && !super::super::task_slot_is_running(slot),
                "scheduler activation invariant: suspended task {task_id} is already running"
            );
            if let Err(reason) =
                self.validate_saved_context(slot, context.user_mode, context.saved_rsp)
            {
                panic!(
                    "scheduler activation invariant: suspended task {task_id} has invalid context: {reason}"
                );
            }
        }

        assert!(
            task_ids.len() <= MAX_ATOMIC_ACTIVATION_HANDOFFS,
            "scheduler activation invariant: cohort exceeds bounded first-turn custody"
        );
        let prioritize_atomic_cohort = task_ids.len() > 1;
        if prioritize_atomic_cohort {
            assert!(
                self.atomic_activation_pick_hints.is_empty()
                    && self.atomic_activation_handoff_remaining == 0,
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
                let inserted = self
                    .atomic_activation_pick_hints
                    .enqueue(slot)
                    .expect("scheduler atomic activation queue overflow");
                assert!(
                    inserted,
                    "scheduler activation invariant: duplicate cohort slot"
                );
            } else {
                self.set_next_spawn_pick_hint(task_id);
            }
        }
        if prioritize_atomic_cohort {
            self.atomic_activation_handoff_remaining = task_ids.len();
        }
        true
    }

    /// Retains every exact peer required by a committed synchronous call or
    /// successful terminal reply. Concurrent transfers on other CPUs must not
    /// overwrite older custody.
    pub(in crate::multitask) fn set_next_synchronous_pick_hint(&mut self, task_id: u64) -> bool {
        let Some(slot) = self.find_task_slot(task_id) else {
            return false;
        };
        if !self.handoff_hint_eligible(slot) {
            return false;
        }
        self.sync_pick_hints
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
        if !context.ready || !self.context_is_schedulable(slot, context) {
            return;
        }
        let inserted = self
            .spawn_pick_hints
            .enqueue(slot)
            .expect("scheduler spawn handoff queue overflow");
        if !inserted {
            return;
        }
        self.apply_ipc_donation(slot);
    }

    /// Select the one-shot child-start transfer before an ordinary wakeup. A child
    /// reaches this slot only after its exact supervisor consumed the
    /// deferred-activation capability, so this order cannot open an
    /// unsupervised execution window. It does prevent a busy wakeup stream
    /// from stretching a committed bootstrap into whole seconds.
    pub(super) fn take_next_bootstrap_handoff_ready_slot(&mut self) -> Option<(usize, bool)> {
        self.take_next_spawn_pick_hint_ready_slot()
            .map(|slot| (slot, false))
            .or_else(|| {
                self.take_next_latency_pick_hint_ready_slot()
                    .map(|slot| (slot, true))
            })
    }

    pub(super) fn take_next_spawn_pick_hint_ready_slot(&mut self) -> Option<usize> {
        while let Some(hint) = self.spawn_pick_hints.pop() {
            if let Some(slot) = self.pick_hint_candidate_slot(Some(hint)) {
                return Some(slot);
            }
        }
        None
    }

    /// Dispatches only the cohort members covered by one atomic activation
    /// commit. Ordinary thread spawns live in a disjoint FIFO and therefore
    /// cannot consume this custody or disable the bounded first-turn prefix.
    pub(super) fn take_next_atomic_activation_handoff_ready_slot(&mut self) -> Option<usize> {
        assert!(
            self.atomic_activation_handoff_remaining <= MAX_ATOMIC_ACTIVATION_HANDOFFS,
            "scheduler atomic activation handoff bound corrupted"
        );
        while self.atomic_activation_handoff_remaining > 0 {
            self.atomic_activation_handoff_remaining -= 1;
            let Some(hint) = self.atomic_activation_pick_hints.pop() else {
                self.atomic_activation_handoff_remaining = 0;
                return None;
            };
            if let Some(slot) = self.pick_hint_candidate_slot(Some(hint)) {
                return Some(slot);
            }
        }
        None
    }

    pub(super) fn take_next_synchronous_pick_hint_ready_slot(&mut self) -> Option<usize> {
        if self.sync_handoff_streak >= MAX_CONSECUTIVE_SYNC_HANDOFFS {
            return None;
        }
        while let Some(hint) = self.sync_pick_hints.pop() {
            if let Some(slot) = self.pick_hint_candidate_slot(Some(hint)) {
                return Some(slot);
            }
        }
        None
    }

    pub(super) fn record_synchronous_handoff(&mut self, synchronous_handoff: bool) {
        self.sync_handoff_streak = if synchronous_handoff {
            self.sync_handoff_streak
                .saturating_add(1)
                .min(MAX_CONSECUTIVE_SYNC_HANDOFFS)
        } else {
            0
        };
    }
}
