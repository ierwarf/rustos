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
        let (task_mask, process_mask, _) = self.slot_affinity_snapshot(slot);
        let affinity = task_mask & process_mask;
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
        let last = usize::from(self.slot_last_cpu(slot));
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
    ///
    /// When `target` is the CPU already executing this wake, the mailbox
    /// round trip is unnecessary — nothing needs cross-CPU synchronization to
    /// touch a local runqueue we already exclusively own via the scheduler
    /// guard — so this publishes directly (`publish_local_wake`) instead of
    /// going through `publish_remote_wake`'s Blocked -> RemoteQueued -> Local
    /// two-step. See `publish_local_wake`'s doc comment for why the two-step
    /// path's second, deferred generation bump matters: it is what made a
    /// same-CPU synchronous-IPC reply-wake token observe a generation one
    /// past what it captured, deterministically, every time.
    pub(super) fn publish_runqueue_wake_to(
        &self,
        slot: usize,
        target: usize,
    ) -> runqueue::RemoteWakeOutcome {
        if self.contexts[slot].is_none() {
            return runqueue::RemoteWakeOutcome::Rejected;
        }
        let outcome = if target == Self::current_dispatch_cpu() {
            runqueue::publish_local_wake(slot, target, self.slot_weight(slot))
        } else {
            runqueue::publish_remote_wake(slot, target, self.slot_weight(slot))
        };
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
        if self.contexts[slot].is_none() {
            return;
        }
        let target = self.runqueue_target_cpu(slot);
        let outcome = runqueue::rehome_queued(slot, target, self.slot_weight(slot));
        let notify_cpu = match outcome {
            runqueue::RemoteWakeOutcome::Rejected => None,
            runqueue::RemoteWakeOutcome::AlreadyOwned { cpu } => cpu,
            runqueue::RemoteWakeOutcome::Published { cpu, notify } => notify.then_some(cpu),
        };
        if let Some(cpu) = notify_cpu {
            super::super::irq::request_target_reschedule(cpu);
        }
    }

}

/// The fallback target, given whether the current slot's custody was claimed.
///
/// Split out so the decision has a witness. The claim itself is a lock-free CAS
/// that host unit schedulers do not model, but the rule it feeds is the whole
/// defect: a *failed* claim must never resolve back to the current slot, which
/// is what asserting custody there amounted to.
pub(super) const fn fallback_slot_for(
    current_slot: usize,
    idle_slot: usize,
    current_claimed: bool,
) -> usize {
    if current_claimed { current_slot } else { idle_slot }
}

/// How many times a dispatch fell back to idle because the current slot had
/// lost custody, and whether any has happened at all this window.
static FALLBACK_IDLE_DISPATCHES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

#[cfg(not(test))]
fn latch_fallback_idle_dispatch() {
    // ORDERING: Relaxed; a diagnostic counter drained once per profile window.
    FALLBACK_IDLE_DISPATCHES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Takes the window's idle-fallback count, clearing it for the next window.
pub(in crate::multitask) fn take_fallback_idle_dispatch_window() -> u64 {
    FALLBACK_IDLE_DISPATCHES.swap(0, core::sync::atomic::Ordering::Relaxed)
}

impl super::Scheduler {
    /// The slot the dispatch fallback may actually claim, taking its custody.
    ///
    /// Prefers keeping the current task, which is the common and cheapest
    /// outcome. When the balance phase has already published that task
    /// `Blocked` or rehomed it to another CPU, it must not keep running, and
    /// this CPU's idle slot is the one slot that always has local custody --
    /// it is never retired, never loses affinity for its own CPU, and is
    /// exactly what a CPU with nothing to run exists to dispatch.
    ///
    /// The idle claim keeps a fail-stop because an unclaimable idle slot is a
    /// real invariant break rather than a lost race: there is no correct
    /// dispatch left to make.
    #[cfg(not(test))]
    pub(super) fn claimable_fallback_slot(&self, current_slot: usize, dispatch_cpu: usize) -> usize {
        let current_claimed =
            runqueue::claim_dispatch(current_slot, dispatch_cpu, self.slot_weight(current_slot));
        let idle_slot = self.idle_fallback_slot();
        if fallback_slot_for(current_slot, idle_slot, current_claimed) == current_slot {
            return current_slot;
        }
        assert!(
            idle_slot != current_slot,
            "scheduler idle slot lost its own local rq custody slot={idle_slot} cpu={dispatch_cpu}"
        );
        assert!(
            runqueue::claim_dispatch(idle_slot, dispatch_cpu, self.slot_weight(idle_slot)),
            "scheduler idle fallback lost local rq custody slot={idle_slot} cpu={dispatch_cpu}"
        );
        // Parking a CPU on idle because the current task lost custody is
        // correct but not free: the task must be woken elsewhere for the system
        // to make progress. Counting it is what separates "this path ran" from
        // "this path stalled something" the next time a run misses its boot
        // deadline without panicking.
        latch_fallback_idle_dispatch();
        idle_slot
    }

    /// Host unit schedulers publish no owner words, so there is no custody to
    /// claim and the current slot is always the fallback.
    #[cfg(test)]
    pub(super) fn claimable_fallback_slot(&self, current_slot: usize, _dispatch_cpu: usize) -> usize {
        current_slot
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
                let candidate = (class, self.slot_vruntime(slot), slot);
                if selected.is_none_or(|current| candidate < current) {
                    selected = Some(candidate);
                }
            }
        }

        let Some((_, _, slot)) = selected else {
            return false;
        };
        self.contexts[slot].expect("idle steal candidate disappeared");
        match runqueue::rehome_queued(slot, target_cpu, self.slot_weight(slot)) {
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
            let candidate = (class, self.slot_vruntime(slot), slot, target_cpu);
            if selected.is_none_or(|current| candidate < current) {
                selected = Some(candidate);
            }
        }

        let Some((_, _, slot, target_cpu)) = selected else {
            return false;
        };
        self.contexts[slot].expect("active balance candidate disappeared");
        match runqueue::rehome_queued(slot, target_cpu, self.slot_weight(slot)) {
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

    /// The defect this rule exists for: the dispatch fallback used to assert
    /// that the current slot still held local run-queue custody. The balance
    /// phase publishes that slot `Blocked` when it retired or stopped being
    /// runnable, and rehomes it when it lost affinity for this CPU, so a failed
    /// claim is a legitimate outcome and must resolve to the CPU's idle slot --
    /// never back to a task this CPU may no longer run.
    #[test]
    fn a_failed_current_claim_never_resolves_back_to_the_current_slot() {
        let current_slot = 7;
        let idle_slot = 127;
        assert_eq!(
            super::fallback_slot_for(current_slot, idle_slot, true),
            current_slot,
            "a claimed current slot is the cheapest and correct fallback"
        );
        assert_eq!(
            super::fallback_slot_for(current_slot, idle_slot, false),
            idle_slot,
            "an unclaimable current slot must fall back to idle, not to itself"
        );
        assert_ne!(
            super::fallback_slot_for(current_slot, idle_slot, false),
            current_slot
        );
    }
}
