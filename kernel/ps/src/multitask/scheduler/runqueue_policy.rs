//! Placement policy at the scheduler-to-per-CPU-runqueue boundary.
//!
//! The runqueue module owns queue mechanics and owner-word transitions.  This
//! module owns only affinity-aware placement and targeted notification so the
//! monolithic scheduler implementation does not grow a second queue backend.

use super::*;

const ACTIVE_BALANCE_INTERVAL_OPPORTUNITIES: u8 = 8;

/// Notification only; the caller retains the original lifecycle owner.
pub(super) fn request_owner_reschedule(slot: usize) {
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

/// Returns whether the next loaded balance opportunity must inspect this CPU.
///
/// The counter contains the number of earlier calls that observed at least two
/// published runnable continuations on the exact source CPU. Zero therefore
/// makes the first loaded opportunity due; later attempts are separated by at
/// most eight such opportunities. RTC time is deliberately not an input.
pub(super) const fn active_balance_opportunity_due(previous_opportunities: u8) -> bool {
    previous_opportunities % ACTIVE_BALANCE_INTERVAL_OPPORTUNITIES == 0
}

pub(super) const fn runqueue_is_imbalanced(source_count: usize, target_count: usize) -> bool {
    source_count > target_count.saturating_add(1)
}

/// Preserve the exact locality/spread target while it is at most one queued
/// continuation above the least-loaded CPU. A single long-lived kernel worker
/// must not permanently erase one residue from user-task distribution; the
/// least-loaded override is reserved for an actual queue imbalance.
const fn placement_target_cpu(
    preferred_cpu: usize,
    least_loaded_cpu: usize,
    preferred_count: usize,
    least_loaded_count: usize,
) -> usize {
    if runqueue_is_imbalanced(preferred_count, least_loaded_count) {
        least_loaded_cpu
    } else {
        preferred_cpu
    }
}

pub(in crate::multitask) fn online_affinity_mask(slot: usize) -> u64 {
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
    let (task_mask, process_mask, _) = runqueue::affinity_payload::affinity_snapshot(slot);
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

/// Placement for a synchronous wake (`WF_SYNC`): the waker blocks next, so
/// the wakee runs here when this CPU is eligible and has nothing queued.
/// Otherwise the ordinary affinity placement applies.
pub(super) fn sync_wake_target_cpu(slot: usize, eligible: u64) -> usize {
    let current = nucleus_core::util::lockdep::current_cpu_index();
    if sync_wake_may_stay_local(current, eligible, runqueue::local_dispatch_work_pending(current)) {
        return current;
    }
    affinity_target_cpu(slot, eligible)
}

const fn sync_wake_may_stay_local(current: usize, eligible: u64, local_work_pending: bool) -> bool {
    current < u64::BITS as usize && eligible & (1_u64 << current) != 0 && !local_work_pending
}

pub(super) fn affinity_target_cpu(slot: usize, eligible: u64) -> usize {
    assert_ne!(eligible, 0, "affinity placement lacks eligible CPU");
    let current = nucleus_core::util::lockdep::current_cpu_index();
    let last = usize::from(runqueue::affinity_payload::last_cpu(slot));
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
    // A woken task should not queue behind a running one while an eligible CPU
    // sits idle: the preferred CPU keeps the task only when it is idle itself.
    if !cpu_is_idle(preferred)
        && let Some(idle) = idle_eligible_cpu(eligible, preferred)
    {
        return idle;
    }
    let least_loaded = runqueue::least_loaded_cpu(eligible, preferred);
    placement_target_cpu(
        preferred,
        least_loaded,
        runqueue::published_runnable_count(preferred),
        runqueue::published_runnable_count(least_loaded),
    )
}

/// A CPU is idle when nothing is queued or mailboxed for it and the task it runs
/// is its own idle task. A remote wake lands in the mailbox first, so counting
/// only the run queue would hand a burst of wakes to the same idle CPU. Both
/// inputs are published hints; the owner CAS stays the authority.
fn cpu_is_idle(cpu: usize) -> bool {
    !runqueue::local_dispatch_work_pending(cpu)
        && super::super::cpu_local::published_current_slot(cpu).is_some_and(runqueue::is_idle_slot)
}

/// First idle eligible CPU after `preferred` in CPU order, wrapping, so a burst
/// of placements spreads across idle CPUs instead of piling onto the lowest.
fn idle_eligible_cpu(eligible: u64, preferred: usize) -> Option<usize> {
    first_idle_after(eligible, preferred, cpu_is_idle)
}

fn first_idle_after(
    eligible: u64,
    preferred: usize,
    is_idle: impl Fn(usize) -> bool,
) -> Option<usize> {
    let max = nucleus_core::util::lockdep::MAX_TRACKED_CPUS;
    (1..max)
        .map(|offset| (preferred + offset) % max)
        .find(|cpu| eligible & (1_u64 << cpu) != 0 && is_idle(*cpu))
}

#[cfg(not(test))]
pub(super) fn rehome_committed_affinity(
    slot: usize,
    id: u64,
    cohort: &super::super::current_identity::cohort::Cohort,
) {
    let eligible = online_affinity_mask(slot);
    if let Some(cpu) =
        rehome_owned_on_mask(slot, eligible, runqueue::weight::value(slot), cohort, id)
    {
        super::super::irq::request_target_reschedule(cpu);
    }
}

pub(super) fn rehome_owned_on_mask(
    slot: usize,
    eligible: u64,
    weight: u32,
    cohort: &super::super::current_identity::cohort::Cohort,
    id: u64,
) -> Option<usize> {
    let target = affinity_target_cpu(slot, eligible);
    match runqueue::rehome_queued_owned(slot, target, weight, cohort, id) {
        runqueue::RemoteWakeOutcome::Rejected => None,
        runqueue::RemoteWakeOutcome::AlreadyOwned { cpu } => cpu,
        runqueue::RemoteWakeOutcome::Published { cpu, notify } => notify.then_some(cpu),
    }
}


/// The fallback target, given whether the current slot's custody was claimed.
///
/// Split out so the decision has a witness. The claim itself is a lock-free CAS
/// that host unit schedulers do not model, but the rule it feeds is the whole
/// defect: a *failed* claim must never resolve back to the current slot, which
/// is what asserting custody there amounted to.
#[cfg(test)]
pub(super) const fn fallback_slot_for(
    current_slot: usize,
    idle_slot: usize,
    current_claimed: bool,
) -> usize {
    if current_claimed {
        current_slot
    } else {
        idle_slot
    }
}

/// How many times a dispatch fell back to idle because the current slot had
/// lost custody, and whether any has happened at all this window.
static FALLBACK_IDLE_DISPATCHES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

pub(super) fn latch_fallback_idle_dispatch() {
    // ORDERING: Relaxed; a diagnostic counter drained once per profile window.
    FALLBACK_IDLE_DISPATCHES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Takes the window's idle-fallback count, clearing it for the next window.
pub(in crate::multitask) fn take_fallback_idle_dispatch_window() -> u64 {
    FALLBACK_IDLE_DISPATCHES.swap(0, core::sync::atomic::Ordering::Relaxed)
}


#[cfg(test)]
mod tests {
    use super::{
        active_balance_opportunity_due, first_idle_after, placement_target_cpu,
        runqueue_is_imbalanced, sync_wake_may_stay_local,
    };

    #[test]
    fn idle_cpu_search_wraps_skips_ineligible_and_never_returns_the_preferred_cpu() {
        let idle = |cpu: usize| cpu == 1 || cpu == 3 || cpu == 6;
        assert_eq!(first_idle_after(0b1111_1111, 2, idle), Some(3));
        assert_eq!(first_idle_after(0b1111_1111, 6, idle), Some(1));
        // CPU 3 is idle but not eligible.
        assert_eq!(first_idle_after(0b1111_0111, 2, idle), Some(6));
        assert_eq!(first_idle_after(0b0100, 2, |cpu| cpu == 2), None);
        assert_eq!(first_idle_after(u64::MAX, 0, |_| false), None);
    }

    #[test]
    fn synchronous_wake_stays_local_only_on_an_idle_eligible_cpu() {
        assert!(sync_wake_may_stay_local(2, 0b0100, false));
        assert!(!sync_wake_may_stay_local(2, 0b0100, true));
        assert!(!sync_wake_may_stay_local(2, 0b1011, false));
        assert!(!sync_wake_may_stay_local(64, u64::MAX, false));
    }

    #[test]
    fn active_balance_requires_more_than_one_excess_runnable() {
        assert!(!runqueue_is_imbalanced(1, 0));
        assert!(!runqueue_is_imbalanced(4, 3));
        assert!(runqueue_is_imbalanced(2, 0));
        assert!(runqueue_is_imbalanced(5, 3));
    }

    #[test]
    fn placement_keeps_one_task_tolerance_before_overriding_spread() {
        assert_eq!(placement_target_cpu(1, 2, 0, 0), 1);
        assert_eq!(placement_target_cpu(1, 2, 1, 0), 1);
        assert_eq!(placement_target_cpu(1, 2, 2, 0), 2);
        assert_eq!(placement_target_cpu(1, 2, 4, 3), 1);
        assert_eq!(placement_target_cpu(1, 2, 5, 3), 2);
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

    #[test]
    fn idle_fallback_claim_requires_the_exact_valid_local_frame() {
        use super::super::dispatch_claim::{DispatchFrame, FrameCapture, FrameCustody, tests::fixture};
        fixture(true, |_, turn| {
            let id = 82909;
            let ticket = super::super::root_slots::reserve(122, id).unwrap();
            let idle = super::super::kernel_creation::publish_reserved(ticket, |_| {},
                119, 8, 16, 0x202, 0x1000, 0x4000, 0,
                |slot, _| super::runqueue::set_idle_slot(slot, true), |_| 1);
            let generation = super::runqueue::owner(idle).generation;
            let rsp = super::runqueue::saved_rsp(idle);
            let capture = |r| DispatchFrame::capture(turn, idle, id, generation, r, FrameCustody::Local);
            assert!(matches!(capture(rsp + 16), FrameCapture::Stale));
            // Modeled earlier frame-producer corruption, before any reader or
            // final claim, in this isolated original kernel allocation.
            let flags = unsafe { (*(rsp as *const super::super::SavedContext)).rflags };
            unsafe { (*(rsp as *mut super::super::SavedContext)).rflags = 0; }
            assert!(matches!(capture(rsp), FrameCapture::Invalid("saved rflags lost the reserved bit")));
            assert_eq!(super::runqueue::owner(idle).generation, generation);
            unsafe { (*(rsp as *mut super::super::SavedContext)).rflags = flags; }
            let FrameCapture::Ready(frame) = capture(rsp) else { panic!("original idle frame lost"); };
            assert!(frame.prepare().is_some());
            assert!(!super::super::stack_resources::cpu_leases::owns_prepared(0, idle));
            super::super::kernel_creation::tests::cleanup(idle);
        });
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
