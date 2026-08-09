//! Placement policy at the scheduler-to-per-CPU-runqueue boundary.
//!
//! The runqueue module owns queue mechanics and owner-word transitions.  This
//! module owns only affinity-aware placement and targeted notification so the
//! monolithic scheduler implementation does not grow a second queue backend.

use super::*;

#[cfg(not(test))]
use core::sync::atomic::{AtomicU8, Ordering};

const ACTIVE_BALANCE_INTERVAL_OPPORTUNITIES: u8 = 8;

/// Returns whether the next loaded balance opportunity must inspect this CPU.
///
/// The counter contains the number of earlier calls that observed at least two
/// published runnable continuations on the exact source CPU. Zero therefore
/// makes the first loaded opportunity due; later attempts are separated by at
/// most eight such opportunities. RTC time is deliberately not an input.
const fn active_balance_opportunity_due(previous_opportunities: u8) -> bool {
    previous_opportunities % ACTIVE_BALANCE_INTERVAL_OPPORTUNITIES == 0
}

#[cfg(not(test))]
static ACTIVE_BALANCE_OPPORTUNITIES: [AtomicU8; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicU8::new(0) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];

const fn runqueue_is_imbalanced(source_count: usize, target_count: usize) -> bool {
    source_count > target_count.saturating_add(1)
}

impl Scheduler {
    #[cfg(not(test))]
    fn runqueue_online_affinity_mask(&self, slot: usize) -> u64 {
        let topology = kernel_hal::api::cpu::topology()
            .expect("scheduler runqueue placement requires admitted topology");
        let mut online_mask = 0_u64;
        for descriptor in topology.cpus() {
            let Some(snapshot) = kernel_hal::api::cpu::lifecycle_snapshot(descriptor.logical_index)
            else {
                continue;
            };
            if snapshot.state == kernel_hal::api::cpu::CpuLifecycleState::Online {
                online_mask |= 1_u64 << descriptor.logical_index;
            }
        }
        let affinity = self.task_affinity_masks[slot] & self.process_affinity_masks[slot];
        let eligible = affinity & online_mask;
        if eligible != 0 {
            return eligible;
        }

        let current = nucleus_core::util::lockdep::current_cpu_index();
        let current_bit = 1_u64 << current;
        assert!(
            online_mask & current_bit != 0 && affinity & current_bit != 0,
            "scheduler task has no Online CPU inside affinity slot={} affinity={:#x} online={:#x}",
            slot,
            affinity,
            online_mask
        );
        current_bit
    }

    #[cfg(not(test))]
    fn runqueue_target_cpu(&self, slot: usize) -> usize {
        let eligible = self.runqueue_online_affinity_mask(slot);
        let current = nucleus_core::util::lockdep::current_cpu_index();
        let last = usize::from(self.task_last_cpu[slot]);
        let preferred = if last < nucleus_core::util::lockdep::MAX_TRACKED_CPUS
            && eligible & (1_u64 << last) != 0
        {
            last
        } else {
            let online_count = usize::try_from(eligible.count_ones()).unwrap_or(1).max(1);
            let spread = slot % online_count;
            (0..nucleus_core::util::lockdep::MAX_TRACKED_CPUS)
                .filter(|cpu| eligible & (1_u64 << cpu) != 0)
                .nth(spread)
                .unwrap_or(current)
        };
        runqueue::least_loaded_cpu(eligible, preferred)
    }

    /// Publishes one exact wake to a placement target chosen by the caller.
    ///
    /// The owner-word CAS and mailbox record remain the authority in every
    /// build.  Callers that already have an exact target (the outgoing CPU of
    /// an assembly transition, for example) use this rather than recreating
    /// the publication protocol around a test-only mirror.
    pub(super) fn publish_runqueue_wake_to(
        &self,
        slot: usize,
        target: usize,
    ) -> runqueue::RemoteWakeOutcome {
        let Some(context) = self.contexts[slot] else {
            return runqueue::RemoteWakeOutcome::Rejected;
        };
        let outcome = runqueue::publish_remote_wake(slot, target, context.weight);
        #[cfg(not(test))]
        if let runqueue::RemoteWakeOutcome::Published { cpu, notify: true } = outcome {
            // Notification only shortens the latency to observe the already
            // published mailbox owner; it never creates wake authority.
            super::super::irq::request_target_reschedule(cpu);
        }
        outcome
    }

    #[cfg(not(test))]
    pub(super) fn publish_runqueue_wake(&self, slot: usize) -> bool {
        match self.publish_runqueue_wake_to(slot, self.runqueue_target_cpu(slot)) {
            runqueue::RemoteWakeOutcome::Rejected => false,
            runqueue::RemoteWakeOutcome::AlreadyOwned { .. } => true,
            runqueue::RemoteWakeOutcome::Published { .. } => true,
        }
    }

    #[cfg(not(test))]
    pub(super) fn admit_runqueue_slot(&self, slot: usize, runnable: bool) {
        runqueue::admit_blocked(slot);
        if runnable {
            assert!(
                self.publish_runqueue_wake(slot),
                "scheduler could not publish newly admitted runnable slot={slot}"
            );
        }
    }

    #[cfg(not(test))]
    pub(super) fn rehome_runqueue_slot(&self, slot: usize) {
        let Some(context) = self.contexts[slot] else {
            return;
        };
        let target = self.runqueue_target_cpu(slot);
        let outcome = runqueue::rehome_queued(slot, target, context.weight);
        let notify_cpu = match outcome {
            runqueue::RemoteWakeOutcome::Rejected => None,
            runqueue::RemoteWakeOutcome::AlreadyOwned { cpu } => cpu,
            runqueue::RemoteWakeOutcome::Published { cpu, notify } => notify.then_some(cpu),
        };
        if let Some(cpu) = notify_cpu {
            super::super::irq::request_target_reschedule(cpu);
        }
    }

    /// Pull one eligible non-idle continuation only when this CPU has no
    /// local work. This is the bounded idle-balance point used by Linux,
    /// FreeBSD ULE, and Zircon-style per-CPU schedulers: ordinary ticks never
    /// scan foreign queues, and transfer uses the existing source-owner CAS
    /// plus target mailbox instead of holding two runqueue locks.
    #[cfg(not(test))]
    pub(super) fn steal_one_for_idle_cpu(&self, target_cpu: usize) -> bool {
        let has_local_work = runqueue::local_runnable_slots(target_cpu).any(|slot| {
            self.contexts[slot].is_some_and(|context| {
                self.slot_is_runnable(slot)
                    && self.is_fair_candidate_slot(slot)
                    && self.context_is_schedulable(slot, context)
            })
        });
        if has_local_work {
            return false;
        }

        let mut selected: Option<(SchedClass, u64, usize)> = None;
        for source_cpu in 0..nucleus_core::util::lockdep::MAX_TRACKED_CPUS {
            if source_cpu == target_cpu || runqueue::published_runnable_count(source_cpu) == 0 {
                continue;
            }
            for slot in runqueue::local_runnable_slots(source_cpu) {
                let Some(context) = self.contexts[slot] else {
                    continue;
                };
                if !self.is_fair_candidate_slot(slot)
                    || !self
                        .context_is_migratable_from_source(slot, context, source_cpu, target_cpu)
                {
                    continue;
                }
                let Some(class) = self.slot_class(slot) else {
                    continue;
                };
                let candidate = (class, context.vruntime_ns, slot);
                if selected.is_none_or(|current| candidate < current) {
                    selected = Some(candidate);
                }
            }
        }

        let Some((_, _, slot)) = selected else {
            return false;
        };
        let context = self.contexts[slot].expect("idle steal candidate disappeared");
        match runqueue::rehome_queued(slot, target_cpu, context.weight) {
            runqueue::RemoteWakeOutcome::Published { cpu, .. } => {
                assert_eq!(cpu, target_cpu, "idle steal crossed target CPU");
                assert!(
                    runqueue::drain_remote_wakes(target_cpu) != 0,
                    "idle steal target mailbox lost transferred task"
                );
                true
            }
            runqueue::RemoteWakeOutcome::AlreadyOwned { .. }
            | runqueue::RemoteWakeOutcome::Rejected => false,
        }
    }

    /// Moves at most one queued continuation from this CPU when both CPUs are
    /// busy but their published runnable counts differ by more than one.
    /// Wake placement and idle stealing alone cannot repair a task that stays
    /// runnable for a long time; Linux scheduler domains perform the analogous
    /// bounded periodic balance. Transfer retains the one-owner CAS/mailbox
    /// protocol and therefore never takes two runqueue locks.
    #[cfg(not(test))]
    pub(super) fn rebalance_one_from_busy_cpu(&self, source_cpu: usize, _now_ticks: u64) -> bool {
        let source_count = runqueue::published_runnable_count(source_cpu);
        if source_count < 2 {
            return false;
        }
        // ORDERING: This counter only bounds independent placement-policy
        // scans; it publishes no runqueue, task, or execution ownership. The
        // source-owner CAS and target mailbox provide those synchronization
        // edges, so Relaxed keeps the cadence local to this source CPU.
        let previous_opportunities =
            ACTIVE_BALANCE_OPPORTUNITIES[source_cpu].fetch_add(1, Ordering::Relaxed);
        if !active_balance_opportunity_due(previous_opportunities) {
            return false;
        }

        let mut selected: Option<(SchedClass, u64, usize, usize)> = None;
        for slot in runqueue::local_runnable_slots(source_cpu) {
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !self.slot_is_runnable(slot) || !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(class) = self.slot_class(slot) else {
                continue;
            };
            if class == SchedClass::Idle {
                continue;
            }
            let eligible = self.runqueue_online_affinity_mask(slot);
            let target_cpu = runqueue::least_loaded_cpu(eligible, source_cpu);
            if target_cpu == source_cpu
                || !runqueue_is_imbalanced(
                    source_count,
                    runqueue::published_runnable_count(target_cpu),
                )
                || !self.context_is_migratable_from_source(slot, context, source_cpu, target_cpu)
            {
                continue;
            }
            let candidate = (class, context.vruntime_ns, slot, target_cpu);
            if selected.is_none_or(|current| candidate < current) {
                selected = Some(candidate);
            }
        }

        let Some((_, _, slot, target_cpu)) = selected else {
            return false;
        };
        let context = self.contexts[slot].expect("active balance candidate disappeared");
        match runqueue::rehome_queued(slot, target_cpu, context.weight) {
            runqueue::RemoteWakeOutcome::Published { cpu, notify } => {
                assert_eq!(cpu, target_cpu, "active balance crossed target CPU");
                if notify {
                    super::super::irq::request_target_reschedule(cpu);
                }
                true
            }
            runqueue::RemoteWakeOutcome::AlreadyOwned { .. }
            | runqueue::RemoteWakeOutcome::Rejected => false,
        }
    }

    /// Notify only the CPU that currently owns this slot. Lifecycle policy
    /// changes (stop, continue, exec, retirement) must not wake unrelated
    /// CPUs merely because their metadata shares the scheduler catalog.
    pub(super) fn request_runqueue_owner_reschedule(&self, slot: usize) {
        #[cfg(not(test))]
        if let Some(cpu) = runqueue::owner(slot).cpu {
            super::super::irq::request_target_reschedule(cpu);
        }
        #[cfg(test)]
        {
            let _ = slot;
            super::super::request_deferred_reschedule();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{active_balance_opportunity_due, runqueue_is_imbalanced};

    #[test]
    fn active_balance_requires_more_than_one_excess_runnable() {
        assert!(!runqueue_is_imbalanced(1, 0));
        assert!(!runqueue_is_imbalanced(4, 3));
        assert!(runqueue_is_imbalanced(2, 0));
        assert!(runqueue_is_imbalanced(5, 3));
    }

    #[test]
    fn active_balance_is_due_first_then_every_eighth_loaded_opportunity() {
        for previous_opportunities in 0_u8..=24 {
            let expected = matches!(previous_opportunities, 0 | 8 | 16 | 24);
            assert_eq!(
                active_balance_opportunity_due(previous_opportunities),
                expected,
                "unexpected balance cadence after {previous_opportunities} loaded opportunities"
            );
        }
    }

    #[test]
    fn active_balance_cadence_is_independent_of_rtc_residue() {
        let expected = [true, false, false, false, false, false, false, false, true];
        for rtc_residue in 0_u64..8 {
            for (previous_opportunities, due) in expected.into_iter().enumerate() {
                assert_eq!(
                    active_balance_opportunity_due(previous_opportunities as u8),
                    due,
                    "RTC residue {rtc_residue} changed balance cadence"
                );
            }
        }
    }
}
