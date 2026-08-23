//! Bounded scheduler runtime attribution for acceptance and field diagnosis.
//!
//! The scheduler owns accounting and emits no diagnostics while holding its
//! global raw lock. The first eligible CPU takes one fixed-size snapshot per
//! second; the timer path renders it only after releasing the scheduler owner.

use super::*;
use core::cell::UnsafeCell;
use core::panic::Location;
use core::sync::atomic::{AtomicU8, Ordering};

const PROFILE_TOP_TASKS: usize = 4;
pub(super) const SCHEDULER_ENTRY_CAUSE_COUNT: usize = 4;
pub(super) const SCHEDULER_PHASE_COUNT: usize = 9;
const PROFILE_EMPTY: u8 = 0;
const PROFILE_BUSY: u8 = 1;
const PROFILE_READY: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask) enum SchedulerEntryCause {
    Timer = 0,
    Rtc = 1,
    RescheduleIpi = 2,
    Software = 3,
}

/// Disjoint segments of one serialized scheduling decision.
///
/// Total lock hold alone cannot distinguish "the critical section is long"
/// from "the owner was descheduled": the release gate needs the exact segment
/// that consumed the time. These accumulate wall nanoseconds inside the
/// scheduler owner and are rendered only after it is released.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask) enum SchedulerPhase {
    /// Outgoing runtime accounting, ready publication, and run-queue custody.
    Account = 0,
    /// Remote wake drain, idle steal, and bounded busy rebalance.
    Balance = 1,
    /// Bounded periodic validation sweep over immutable ready frames.
    Validate = 2,
    /// Class-minimum virtual-time refresh over the local runnable set.
    SelectVruntime = 3,
    /// Exact activation, synchronous, overdue, and bootstrap handoff chain.
    SelectHandoff = 4,
    /// Fair class selection and the minimum-granularity preemption guard.
    SelectPick = 5,
    /// Custody claim, dispatch records, and lockdep owner publication.
    Commit = 6,
    /// SIMD save/restore and the architectural task-state restore.
    ArchRestore = 7,
    /// Everything one acquisition does before its caller runs: owner
    /// publication and the deferred wake drain. This is in-owner cost charged
    /// to whichever caller took the lock next rather than to what it asked
    /// for, so it is only visible as a segment of its own.
    Prologue = 8,
}

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::multitask) struct SchedulerRuntimeProfileEntry {
    pub(in crate::multitask) task_id: u64,
    pub(in crate::multitask) process_id: u64,
    pub(in crate::multitask) runtime_ns: u64,
    pub(in crate::multitask) dispatches: u64,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::multitask) struct SchedulerRuntimeProfile {
    pub(in crate::multitask) elapsed_ms: u64,
    pub(in crate::multitask) total_runtime_ns: u64,
    pub(in crate::multitask) total_dispatches: u64,
    pub(in crate::multitask) entry_causes: [u64; SCHEDULER_ENTRY_CAUSE_COUNT],
    pub(in crate::multitask) same_task_dispatches: u64,
    pub(in crate::multitask) task_switches: u64,
    pub(in crate::multitask) address_space_switches: u64,
    pub(in crate::multitask) cross_cpu_migrations: u64,
    /// Guard acquisitions, which is the figure the global dispatch lock is
    /// actually contended on. Dispatch decisions undercount it: lifecycle,
    /// wake, and affinity paths take the same lock without dispatching.
    pub(in crate::multitask) lock_acquisitions: u64,
    pub(in crate::multitask) lock_wait_ns: u64,
    pub(in crate::multitask) lock_hold_ns: u64,
    pub(in crate::multitask) lock_wait_max_ns: u64,
    pub(in crate::multitask) lock_hold_max_ns: u64,
    /// The acquisition site that produced `lock_hold_max_ns`, and the in-owner
    /// segment time that same acquisition attributed. A maximum without these
    /// cannot separate a long critical section from a descheduled vCPU: if the
    /// attributed share is near zero the owner was not running.
    pub(in crate::multitask) lock_hold_max_caller: Option<&'static Location<'static>>,
    pub(in crate::multitask) lock_hold_max_attributed_ns: u64,
    pub(in crate::multitask) deferred_wakes: u64,
    /// Self-timed fair-class pick scan, which the phase marks attribute to
    /// `SelectHandoff` because both pick functions sit inside that span.
    pub(in crate::multitask) pick_scan_ns: u64,
    pub(in crate::multitask) pick_scan_calls: u64,
    pub(in crate::multitask) handoff_scan_ns: u64,
    pub(in crate::multitask) handoff_scan_calls: u64,
    /// Per-member cost of the handoff chain, in the order the chain runs them.
    /// `(ns, calls)`; steps two and five overlap `handoff_scan_ns`.
    pub(in crate::multitask) handoff_steps: [(u64, u64); super::locality::HANDOFF_STEP_COUNT],
    /// Subset of `handoff_steps[1]`'s calls that found a ready synchronous IPC
    /// pick hint (`Some`), so `select` resolved without the CFS vruntime scan.
    pub(in crate::multitask) sync_handoff_hits: u64,
    /// The rest of `handoff_steps[1]`'s calls, split by why they missed.
    /// Indexed by `locality::SyncHandoffMissReason`. `sync_handoff_hits` plus
    /// this sum should equal `handoff_steps[1].1`.
    pub(in crate::multitask) sync_handoff_misses:
        [u64; super::locality::SYNC_HANDOFF_MISS_REASON_COUNT],
    /// Sub-reasons for `sync_handoff_misses`'s `DrainedStale` bucket, indexed
    /// by `locality::SyncHandoffStaleReason`. Per-discard, not per-dispatch.
    pub(in crate::multitask) sync_handoff_stale:
        [u64; super::locality::SYNC_HANDOFF_STALE_REASON_COUNT],
    /// Arm-side (accepted, rejected) pairs, indexed by
    /// `locality::SyncHandoffArmSite` — call direction then reply direction.
    pub(in crate::multitask) sync_handoff_arms:
        [(u64, u64); super::locality::SYNC_HANDOFF_ARM_SITE_COUNT],
    /// Which exact condition failed inside a `ReplyWake` record's custody
    /// check, indexed by `locality::SyncHandoffReplyCustodyFailReason`. See
    /// that type's doc comment for the attribution caveat.
    pub(in crate::multitask) sync_handoff_reply_custody_fail:
        [u64; super::locality::SYNC_HANDOFF_REPLY_CUSTODY_FAIL_REASON_COUNT],
    /// The owner state found at the moment of a `Generation` mismatch,
    /// indexed by `RunOwnerState`'s discriminant. `Running` (index 3) means
    /// the caller was already dispatched through a different entry point
    /// before this FIFO record was ever reached.
    pub(in crate::multitask) sync_handoff_generation_fail_state:
        [u64; super::locality::SYNC_HANDOFF_GENERATION_FAIL_STATE_COUNT],
    pub(in crate::multitask) phase_ns: [u64; SCHEDULER_PHASE_COUNT],
    pub(in crate::multitask) runnable_samples: u64,
    /// Live priority donations when the window closed. The class derivation
    /// each pick scan runs per candidate walks this list, so it is the second
    /// factor in the scan cost.
    pub(in crate::multitask) live_ipc_donations: u64,
    /// First slot whose lock-free identity record disagrees with the
    /// scheduler's own tables, or `usize::MAX` when publication is complete.
    /// This is the standing proof that no identity write site was missed.
    pub(in crate::multitask) divergent_identity_slot: usize,
    /// First disagreement between the per-CPU owner word and the legacy tables
    /// this window, with the total, or `None` when the two sources agreed.
    ///
    /// The cutover evidence for `V5-SCHED-GLOBAL-001`. Nothing consumes it; it
    /// exists so removing the global lock is preceded by proof that the owner
    /// word already means what the legacy tables mean.
    pub(in crate::multitask) run_authority_divergence: Option<(u64, u64, u64)>,
    pub(in crate::multitask) top: [SchedulerRuntimeProfileEntry; PROFILE_TOP_TASKS],
}

struct PendingSchedulerRuntimeProfile {
    state: AtomicU8,
    profile: UnsafeCell<Option<SchedulerRuntimeProfile>>,
}

// SAFETY: `state` grants exclusive access to `profile`. A producer owns BUSY
// after EMPTY->BUSY and release-publishes READY only after the write. The
// consumer owns BUSY after READY->BUSY with Acquire before reading and clears
// to EMPTY only after the read completes.
unsafe impl Sync for PendingSchedulerRuntimeProfile {}

impl PendingSchedulerRuntimeProfile {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(PROFILE_EMPTY),
            profile: UnsafeCell::new(None),
        }
    }

    fn publish(&self, profile: SchedulerRuntimeProfile) -> bool {
        if self
            .state
            .compare_exchange(
                PROFILE_EMPTY,
                PROFILE_BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        // SAFETY: the successful transition above gives this producer
        // exclusive ownership until READY is release-published.
        unsafe { *self.profile.get() = Some(profile) };
        self.state.store(PROFILE_READY, Ordering::Release);
        true
    }

    fn take(&self) -> Option<SchedulerRuntimeProfile> {
        self.state
            .compare_exchange(
                PROFILE_READY,
                PROFILE_BUSY,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .ok()?;
        // SAFETY: Acquire above observes the complete producer write and BUSY
        // excludes another producer until the consumer clears EMPTY.
        let profile = unsafe { (*self.profile.get()).take() };
        self.state.store(PROFILE_EMPTY, Ordering::Release);
        profile
    }
}

static PENDING_RUNTIME_PROFILE: PendingSchedulerRuntimeProfile =
    PendingSchedulerRuntimeProfile::new();

pub(in crate::multitask) fn publish_scheduler_runtime_profile(
    profile: Option<SchedulerRuntimeProfile>,
) {
    if let Some(profile) = profile {
        let _ = PENDING_RUNTIME_PROFILE.publish(profile);
    }
}

fn pack_u32_pair(high: u64, low: u64) -> u64 {
    let high = high.min(u64::from(u32::MAX));
    let low = low.min(u64::from(u32::MAX));
    (high << 32) | low
}

/// Whether an acquisition site explains enough of the window to be worth a
/// debugcon line. The census is sorted, so the first site that fails this ends
/// the report.
fn acquire_site_is_reportable(count: u64, acquisitions: u64) -> bool {
    count != 0 && count.saturating_mul(100) >= acquisitions
}

/// Stable 32-bit file identity for the acquisition census.
fn fnv1a32(value: &str) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for byte in value.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

pub fn drain_scheduler_runtime_profile() -> usize {
    // Budget counters share the scheduler's one-second profile window.  They
    // used to be drained before this readiness check, so the fast
    // housekeeping loop emitted four debugcon milestones on almost every
    // pass.  Under KVM that diagnostic MMIO was charged back to the running
    // scheduling context, exhausted its budget, and could prevent ipcbench
    // from completing.  Take the window token first: no profile means no
    // rendering and, importantly, no destructive counter drain.
    let Some(profile) = PENDING_RUNTIME_PROFILE.take() else {
        return 0;
    };
    let scheduling_context = super::scheduling_context::drain_runtime_counters();
    let mut work = 0;
    for (budget, budget_name, refill_name) in [
        (
            scheduling_context.context,
            "kernel-scheduling-context-budget",
            "kernel-scheduling-context-refill",
        ),
        (
            scheduling_context.domain,
            "kernel-scheduling-domain-budget",
            "kernel-scheduling-domain-refill",
        ),
    ] {
        if budget == Default::default() {
            continue;
        }
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            budget_name,
            budget.charged_ns,
            pack_u32_pair(budget.exhaustions, budget.overrun_ns / 1_000),
        );
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            refill_name,
            budget.refill_ns,
            pack_u32_pair(budget.refills, budget.overflow_merges),
        );
        work += 1;
    }
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-profile",
        profile.elapsed_ms,
        pack_u32_pair(
            profile.total_runtime_ns / 1_000_000,
            profile.total_dispatches,
        ),
    );
    // arg0=(same-task dispatches, real task switches),
    // arg1=(cross-CPU migrations, address-space switches).
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-transition",
        pack_u32_pair(profile.same_task_dispatches, profile.task_switches),
        pack_u32_pair(profile.cross_cpu_migrations, profile.address_space_switches),
    );
    // Totals and maxima use microseconds so both halves retain useful range.
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-lock",
        pack_u32_pair(
            profile.lock_hold_ns / 1_000,
            profile.lock_hold_max_ns / 1_000,
        ),
        pack_u32_pair(
            profile.lock_wait_ns / 1_000,
            profile.lock_wait_max_ns / 1_000,
        ),
    );
    // The single worst owner turn of the window, with the segment time that
    // same turn attributed and the site that acquired it.
    // arg0=(hold max us, attributed us), arg1=(caller file hash, caller line).
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-hold-max",
        pack_u32_pair(
            profile.lock_hold_max_ns / 1_000,
            profile.lock_hold_max_attributed_ns / 1_000,
        ),
        profile
            .lock_hold_max_caller
            .map(|caller| (u64::from(fnv1a32(caller.file())) << 32) | u64::from(caller.line()))
            .unwrap_or(0),
    );
    // The fair-class pick scan, self-timed because the phase marks charge it to
    // `SelectHandoff`. arg0=us, arg1=calls.
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-phase-pick-scan",
        profile.pick_scan_ns / 1_000,
        profile.pick_scan_calls,
    );
    // The other two O(local runnable) walks inside the same span.
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-phase-handoff-scan",
        profile.handoff_scan_ns / 1_000,
        profile.handoff_scan_calls,
    );
    // The chain's own six members, in run order. Everything the two scans above
    // do not explain is here. arg0=us, arg1=calls.
    for (step, name) in [
        "kernel-scheduler-step-activation",
        "kernel-scheduler-step-sync",
        "kernel-scheduler-step-overdue",
        "kernel-scheduler-step-bootstrap",
        "kernel-scheduler-step-blocking-hint",
        "kernel-scheduler-step-overdue-hint",
        "kernel-scheduler-step-acquire",
    ]
    .into_iter()
    .enumerate()
    {
        let (ns, calls) = profile.handoff_steps[step];
        crate::debug::record_milestone(crate::debug::LogCategory::Sched, name, ns / 1_000, calls);
    }
    // Step 1's hit/attempt split: how often the synchronous IPC pick hint
    // actually held a ready slot, against how often it was asked.
    // arg0=hits, arg1=attempts (handoff_steps[1].1).
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-step-sync-hits",
        profile.sync_handoff_hits,
        profile.handoff_steps[1].1,
    );
    // The miss side of the same split, by cause. Each arg1 repeats the same
    // attempt count as the hits line above, so every row is self-contained.
    for (reason_count, name) in [
        (
            profile.sync_handoff_misses[0],
            "kernel-scheduler-step-sync-miss-empty",
        ),
        (
            profile.sync_handoff_misses[1],
            "kernel-scheduler-step-sync-miss-streak",
        ),
        (
            profile.sync_handoff_misses[2],
            "kernel-scheduler-step-sync-miss-stale",
        ),
    ] {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            name,
            reason_count,
            profile.handoff_steps[1].1,
        );
    }
    // Sub-reasons within the `-miss-stale` bucket: which check inside
    // `synchronous_handoff_record_is_ready` discarded a queued record.
    // arg1 repeats the stale total, so each row's share of it is self-evident.
    // Per-discard, not per-dispatch: one attempt can discard more than one.
    let drained_stale = profile.sync_handoff_misses[2];
    for (stale_count, name) in [
        (
            profile.sync_handoff_stale[0],
            "kernel-scheduler-step-sync-stale-identity",
        ),
        (
            profile.sync_handoff_stale[1],
            "kernel-scheduler-step-sync-stale-custody",
        ),
        (
            profile.sync_handoff_stale[2],
            "kernel-scheduler-step-sync-stale-not-candidate",
        ),
    ] {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            name,
            stale_count,
            drained_stale,
        );
    }
    // Arm-side outcome, split by which direction armed it. arg0=accepted,
    // arg1=rejected. Tests whether a round trip's hint shortfall is one-sided.
    for ((accepted, rejected), name) in [
        (
            profile.sync_handoff_arms[0],
            "kernel-scheduler-step-sync-arm-call",
        ),
        (
            profile.sync_handoff_arms[1],
            "kernel-scheduler-step-sync-arm-reply",
        ),
    ] {
        crate::debug::record_milestone(crate::debug::LogCategory::Sched, name, accepted, rejected);
    }
    // Which exact condition failed inside a ReplyWake record's custody check
    // (Generic records never reach this — see the type's doc comment for the
    // arm-vs-consume attribution caveat). arg1 repeats the stale-custody total
    // so each row's share is self-evident.
    let reply_custody_fail_total: u64 = profile.sync_handoff_reply_custody_fail.iter().sum();
    for (fail_count, name) in [
        (
            profile.sync_handoff_reply_custody_fail[0],
            "kernel-scheduler-step-sync-custody-fail-generation",
        ),
        (
            profile.sync_handoff_reply_custody_fail[1],
            "kernel-scheduler-step-sync-custody-fail-cpu",
        ),
        (
            profile.sync_handoff_reply_custody_fail[2],
            "kernel-scheduler-step-sync-custody-fail-not-runnable",
        ),
        (
            profile.sync_handoff_reply_custody_fail[3],
            "kernel-scheduler-step-sync-custody-fail-state",
        ),
    ] {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            name,
            fail_count,
            reply_custody_fail_total,
        );
    }
    // One-shot mechanism diagnostic: the owner state found at the moment a
    // Generation mismatch fired. `Running` means the caller was already
    // dispatched through a different entry point before this FIFO record was
    // ever reached. arg1 repeats the generation-fail total, matching
    // `sync_handoff_reply_custody_fail[0]` above.
    let generation_fail_total: u64 = profile.sync_handoff_generation_fail_state.iter().sum();
    for (state_count, name) in [
        (
            profile.sync_handoff_generation_fail_state[0],
            "kernel-scheduler-step-sync-generation-fail-dormant",
        ),
        (
            profile.sync_handoff_generation_fail_state[1],
            "kernel-scheduler-step-sync-generation-fail-local",
        ),
        (
            profile.sync_handoff_generation_fail_state[2],
            "kernel-scheduler-step-sync-generation-fail-remote-queued",
        ),
        (
            profile.sync_handoff_generation_fail_state[3],
            "kernel-scheduler-step-sync-generation-fail-running",
        ),
        (
            profile.sync_handoff_generation_fail_state[4],
            "kernel-scheduler-step-sync-generation-fail-migrating",
        ),
        (
            profile.sync_handoff_generation_fail_state[5],
            "kernel-scheduler-step-sync-generation-fail-blocked",
        ),
        (
            profile.sync_handoff_generation_fail_state[6],
            "kernel-scheduler-step-sync-generation-fail-retiring",
        ),
        (
            profile.sync_handoff_generation_fail_state[7],
            "kernel-scheduler-step-sync-generation-fail-retired",
        ),
    ] {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            name,
            state_count,
            generation_fail_total,
        );
    }
    // Owner publication plus the deferred wake drain, which every acquisition
    // pays before its caller runs. arg0=prologue us, arg1=wakes drained.
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-phase-prologue",
        profile.phase_ns[SchedulerPhase::Prologue as usize] / 1_000,
        profile.deferred_wakes,
    );
    // Disjoint in-owner segments in microseconds.
    // arg0=(account, balance), arg1=(validate, select).
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-phase",
        pack_u32_pair(
            profile.phase_ns[SchedulerPhase::Account as usize] / 1_000,
            profile.phase_ns[SchedulerPhase::Balance as usize] / 1_000,
        ),
        pack_u32_pair(
            profile.phase_ns[SchedulerPhase::Validate as usize] / 1_000,
            profile.phase_ns[SchedulerPhase::SelectVruntime as usize]
                .saturating_add(profile.phase_ns[SchedulerPhase::SelectHandoff as usize])
                .saturating_add(profile.phase_ns[SchedulerPhase::SelectPick as usize])
                / 1_000,
        ),
    );
    // arg0=(select vruntime, select handoff), arg1=(select pick, guard
    // acquisitions across the window).
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-phase-select",
        pack_u32_pair(
            profile.phase_ns[SchedulerPhase::SelectVruntime as usize] / 1_000,
            profile.phase_ns[SchedulerPhase::SelectHandoff as usize] / 1_000,
        ),
        pack_u32_pair(
            profile.phase_ns[SchedulerPhase::SelectPick as usize] / 1_000,
            profile.lock_acquisitions,
        ),
    );
    // arg0=(commit, architectural restore), arg1=(attributed total, hold total).
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-phase-tail",
        pack_u32_pair(
            profile.phase_ns[SchedulerPhase::Commit as usize] / 1_000,
            profile.phase_ns[SchedulerPhase::ArchRestore as usize] / 1_000,
        ),
        pack_u32_pair(
            profile.phase_ns.iter().copied().sum::<u64>() / 1_000,
            profile.lock_hold_ns / 1_000,
        ),
    );
    // How long the O(local-runnable) pick scans actually are, and how many
    // donations each candidate's class derivation walks. Both pick scans are
    // O(runnable) and the class walk inside them is O(donations), so their
    // product is the cost, and neither term was measured: the scan time alone
    // cannot say whether to shorten the list or the walk.
    // arg0=(runnable samples summed over dispatches), arg1=(live donations at
    // the sample, dispatches).
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-scan",
        profile.runnable_samples,
        pack_u32_pair(profile.live_ipc_donations, profile.total_dispatches),
    );
    // arg0=(timer, software), arg1=(reschedule IPI, RTC). Keep the high-rate
    // entry causes in one fixed record; debugcon is itself a VM-exit under KVM.
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-entry",
        pack_u32_pair(
            profile.entry_causes[SchedulerEntryCause::Timer as usize],
            profile.entry_causes[SchedulerEntryCause::Software as usize],
        ),
        pack_u32_pair(
            profile.entry_causes[SchedulerEntryCause::RescheduleIpi as usize],
            profile.entry_causes[SchedulerEntryCause::Rtc as usize],
        ),
    );
    // Per-caller acquisition census. Emitted here because this path already
    // runs after the scheduler owner is released, so naming the callers costs
    // no lock hold. Sorted so the sites worth moving off the global lock come
    // first; the dispatch total alone never showed them.
    let (mut census, overflow) = super::super::cpu_local::take_acquire_site_census();
    census.sort_unstable_by(|left, right| right.2.cmp(&left.2));
    // A divergence here means a slot mutated its identity without
    // republishing, which serves stale answers to syscall entry. It is
    // reported rather than left to inspection of the write sites.
    // arg0 = packed first edge (present bit, slot, from state/cpu, to
    // state/cpu), arg1 = rejected edges this window. An empty window emits
    // nothing, so any occurrence is the signal.
    if let Some((first, count, task_id)) = profile.run_authority_divergence {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "kernel-scheduler-run-authority-mismatch",
            first,
            pack_u32_pair(task_id, count),
        );
    }
    if profile.divergent_identity_slot != usize::MAX {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "kernel-scheduler-identity-divergence",
            u64::try_from(profile.divergent_identity_slot).unwrap_or(u64::MAX),
            0,
        );
    }
    // Milestones, not level-filtered logs: the ordinary info channel does not
    // reach the debug transport in the product configuration, and a diagnostic
    // that cannot be read is not evidence.
    //
    // Four names left most of the traffic anonymous. At eight vCPUs the census
    // reported 144,602 acquisitions per second and the four emitted sites
    // accounted for 70,715 of them, so the majority of the contention had no
    // caller attached to it and no cut could be argued from the record. The
    // sites are tracked sixty-four deep already; only the reporting was narrow.
    //
    // The floor is what keeps the wider report from costing anything on a quiet
    // system: a site below a percent of the window explains none of the wait,
    // and each name is a debugcon line, which is a port write per byte.
    const ACQUIRE_SITE_NAMES: [&str; 8] = [
        "kernel-scheduler-acquire-0",
        "kernel-scheduler-acquire-1",
        "kernel-scheduler-acquire-2",
        "kernel-scheduler-acquire-3",
        "kernel-scheduler-acquire-4",
        "kernel-scheduler-acquire-5",
        "kernel-scheduler-acquire-6",
        "kernel-scheduler-acquire-7",
    ];
    for (name, (file, line, count)) in ACQUIRE_SITE_NAMES.into_iter().zip(census.iter()) {
        if !acquire_site_is_reportable(*count, profile.lock_acquisitions) {
            break;
        }
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            name,
            *count,
            // The line alone is ambiguous across files, so pack a stable file
            // hash beside it. Offline, hashing a candidate path identifies the
            // caller exactly.
            (u64::from(fnv1a32(file)) << 32) | u64::from(*line),
        );
    }
    if overflow != 0 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "kernel-scheduler-acquire-overflow",
            overflow,
            0,
        );
    }

    let names = [
        "kernel-scheduler-top-0",
        "kernel-scheduler-top-1",
        "kernel-scheduler-top-2",
        "kernel-scheduler-top-3",
    ];
    for (name, entry) in names.into_iter().zip(profile.top) {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            name,
            pack_u32_pair(entry.process_id, entry.task_id),
            pack_u32_pair(entry.runtime_ns / 1_000_000, entry.dispatches),
        );
    }
    work + 1
}

impl Scheduler {
    pub(super) fn account_runtime_profile(&mut self, slot: usize, elapsed_ns: u64) {
        self.runtime_profile_ns[slot] = self.runtime_profile_ns[slot].saturating_add(elapsed_ns);
    }

    pub(super) fn record_runtime_profile_dispatch(&mut self, slot: usize) {
        self.runtime_profile_dispatches[slot] =
            self.runtime_profile_dispatches[slot].saturating_add(1);
    }

    /// Record the exact outcome of one serialized dispatch. Locality history
    /// is diagnostic only: policy must not consume these counters because a
    /// profiler may be reset or disabled without changing scheduling.
    pub(super) fn record_runtime_profile_transition(
        &mut self,
        previous_slot: usize,
        next_slot: usize,
        logical_cpu: usize,
    ) {
        if previous_slot == next_slot {
            self.runtime_profile_same_task_dispatches =
                self.runtime_profile_same_task_dispatches.saturating_add(1);
        } else {
            self.runtime_profile_task_switches =
                self.runtime_profile_task_switches.saturating_add(1);
            let previous_root = self.contexts[previous_slot]
                .map(|context| context.address_space_root)
                .expect("profiled outgoing task missing context");
            let next_root = self.contexts[next_slot]
                .map(|context| context.address_space_root)
                .expect("profiled incoming task missing context");
            if previous_root != next_root {
                self.runtime_profile_address_space_switches = self
                    .runtime_profile_address_space_switches
                    .saturating_add(1);
            }
        }

        let logical_cpu = u8::try_from(logical_cpu)
            .expect("profiled logical CPU index exceeds diagnostic capacity");
        let task_id = self.starts[next_slot]
            .map(|start| start.id)
            .expect("profiled incoming task missing identity");
        if self.runtime_profile_last_task_id[next_slot] != task_id {
            self.runtime_profile_last_task_id[next_slot] = task_id;
            self.runtime_profile_last_cpu[next_slot] = NO_IDLE_CPU;
        }
        let previous_cpu = self.runtime_profile_last_cpu[next_slot];
        if previous_cpu != NO_IDLE_CPU && previous_cpu != logical_cpu {
            self.runtime_profile_cross_cpu_migrations =
                self.runtime_profile_cross_cpu_migrations.saturating_add(1);
        }
        self.runtime_profile_last_cpu[next_slot] = logical_cpu;
    }

    pub(in crate::multitask) fn record_runtime_profile_lock_wait(&mut self, elapsed_ns: u64) {
        self.runtime_profile_lock_acquisitions =
            self.runtime_profile_lock_acquisitions.saturating_add(1);
        self.runtime_profile_lock_wait_ns =
            self.runtime_profile_lock_wait_ns.saturating_add(elapsed_ns);
        self.runtime_profile_lock_wait_max_ns =
            self.runtime_profile_lock_wait_max_ns.max(elapsed_ns);
    }

    /// Charges one complete owner turn.
    ///
    /// `attributed_ns` is how much of this turn the in-owner segments claimed.
    /// It is carried alongside the maximum rather than only summed, because the
    /// window total cannot say whether the single worst turn was work or a
    /// host-descheduled vCPU, and that distinction decides whether shortening
    /// the critical section can help at all.
    pub(in crate::multitask) fn record_runtime_profile_lock_hold(
        &mut self,
        elapsed_ns: u64,
        attributed_ns: u64,
        caller: &'static Location<'static>,
    ) {
        self.runtime_profile_lock_hold_ns =
            self.runtime_profile_lock_hold_ns.saturating_add(elapsed_ns);
        if elapsed_ns > self.runtime_profile_lock_hold_max_ns {
            self.runtime_profile_lock_hold_max_ns = elapsed_ns;
            self.runtime_profile_lock_hold_max_caller = Some(caller);
            self.runtime_profile_lock_hold_max_attributed_ns = attributed_ns;
        }
    }

    /// Total in-owner segment time so far, which is the baseline a single owner
    /// turn is measured against.
    #[inline]
    pub(in crate::multitask) fn runtime_profile_phase_total_ns(&self) -> u64 {
        self.runtime_profile_phase_ns
            .iter()
            .copied()
            .fold(0_u64, u64::saturating_add)
    }

    /// Charges one acquisition prologue and the deferred wakes it drained.
    pub(in crate::multitask) fn record_runtime_profile_prologue(
        &mut self,
        deferred_wakes: u64,
        elapsed_ns: u64,
    ) {
        self.runtime_profile_deferred_wakes = self
            .runtime_profile_deferred_wakes
            .saturating_add(deferred_wakes);
        self.record_runtime_profile_phase(SchedulerPhase::Prologue, elapsed_ns);
    }

    /// Charges one disjoint scheduling segment. The caller already holds the
    /// scheduler owner, so this is an allocation-free counter update and never
    /// a second time source or diagnostic emission.
    pub(in crate::multitask) fn record_runtime_profile_phase(
        &mut self,
        phase: SchedulerPhase,
        elapsed_ns: u64,
    ) {
        let total = &mut self.runtime_profile_phase_ns[phase as usize];
        *total = total.saturating_add(elapsed_ns);
    }

    pub(in crate::multitask) fn record_runtime_profile_entry(
        &mut self,
        cause: SchedulerEntryCause,
    ) {
        let count = &mut self.runtime_profile_entry_counts[cause as usize];
        *count = count.saturating_add(1);
    }

    pub(in crate::multitask) fn take_runtime_profile(
        &mut self,
        now_ticks: u64,
    ) -> Option<SchedulerRuntimeProfile> {
        if self.runtime_profile_started_ticks == 0 {
            self.runtime_profile_started_ticks = now_ticks;
            return None;
        }
        let elapsed_ticks = now_ticks.saturating_sub(self.runtime_profile_started_ticks);
        if elapsed_ticks < crate::arch::rtc::ticks_per_second() {
            return None;
        }

        let mut pick_scan_calls = 0_u64;
        let mut handoff_scan_calls = 0_u64;
        let mut profile = SchedulerRuntimeProfile {
            elapsed_ms: Scheduler::ticks_elapsed_ms(self.runtime_profile_started_ticks, now_ticks),
            total_runtime_ns: 0,
            total_dispatches: 0,
            entry_causes: self.runtime_profile_entry_counts,
            same_task_dispatches: self.runtime_profile_same_task_dispatches,
            task_switches: self.runtime_profile_task_switches,
            address_space_switches: self.runtime_profile_address_space_switches,
            cross_cpu_migrations: self.runtime_profile_cross_cpu_migrations,
            lock_acquisitions: self.runtime_profile_lock_acquisitions,
            lock_wait_ns: self.runtime_profile_lock_wait_ns,
            lock_hold_ns: self.runtime_profile_lock_hold_ns,
            lock_wait_max_ns: self.runtime_profile_lock_wait_max_ns,
            lock_hold_max_ns: self.runtime_profile_lock_hold_max_ns,
            lock_hold_max_caller: self.runtime_profile_lock_hold_max_caller,
            lock_hold_max_attributed_ns: self.runtime_profile_lock_hold_max_attributed_ns,
            deferred_wakes: self.runtime_profile_deferred_wakes,
            pick_scan_ns: {
                let (ns, calls) = super::locality::take_pick_scan_window();
                pick_scan_calls = calls;
                ns
            },
            pick_scan_calls,
            handoff_scan_ns: {
                let (ns, calls) = super::locality::take_handoff_scan_window();
                handoff_scan_calls = calls;
                ns
            },
            handoff_scan_calls,
            handoff_steps: super::locality::take_handoff_step_window(),
            sync_handoff_hits: super::locality::take_sync_handoff_hit_window(),
            sync_handoff_misses: super::locality::take_sync_handoff_miss_window(),
            sync_handoff_stale: super::locality::take_sync_handoff_stale_window(),
            sync_handoff_arms: super::locality::take_sync_handoff_arm_window(),
            sync_handoff_reply_custody_fail:
                super::locality::take_sync_handoff_reply_custody_fail_window(),
            sync_handoff_generation_fail_state:
                super::locality::take_sync_handoff_generation_fail_state_window(),
            phase_ns: self.runtime_profile_phase_ns,
            runnable_samples: self.runtime_profile_runnable_samples,
            live_ipc_donations: {
                #[cfg(not(test))]
                {
                    super::donation_ledger::live_len() as u64
                }
                #[cfg(test)]
                {
                    self.ipc_priority_donation_len as u64
                }
            },
            divergent_identity_slot: self.divergent_published_identity().unwrap_or(usize::MAX),
            run_authority_divergence: {
                // Sweep before taking the window so a position no publication
                // site reached is charged to this window rather than the next.
                self.sweep_run_authority();
                super::super::run_authority::take_window().map(|(first, count)| {
                    // Resolve the slot to its task while the lock is still
                    // held. A slot number alone forces the reader to guess
                    // which service it was, and guessing from a slot number is
                    // how the previous misattributions started.
                    let slot = ((first >> 32) & 0xFFFF) as usize;
                    let task_id = self
                        .starts
                        .get(slot)
                        .and_then(|start| *start)
                        .map(|start| start.id)
                        .unwrap_or(0);
                    (first, count, task_id)
                })
            },
            top: [SchedulerRuntimeProfileEntry::default(); PROFILE_TOP_TASKS],
        };
        for slot in 0..MAX_TASK {
            let runtime_ns = self.runtime_profile_ns[slot];
            let dispatches = self.runtime_profile_dispatches[slot];
            profile.total_runtime_ns = profile.total_runtime_ns.saturating_add(runtime_ns);
            profile.total_dispatches = profile.total_dispatches.saturating_add(dispatches);
            let entry = SchedulerRuntimeProfileEntry {
                task_id: self.starts[slot].map(|start| start.id).unwrap_or(0),
                process_id: self.contexts[slot]
                    .and_then(|context| context.process_id)
                    .unwrap_or(0),
                runtime_ns,
                dispatches,
            };
            if let Some(index) = profile.top.iter().position(|current| {
                (entry.runtime_ns, entry.dispatches) > (current.runtime_ns, current.dispatches)
            }) {
                profile.top[index..].rotate_right(1);
                profile.top[index] = entry;
            }
        }
        self.runtime_profile_ns.fill(0);
        self.runtime_profile_dispatches.fill(0);
        self.runtime_profile_entry_counts.fill(0);
        self.runtime_profile_same_task_dispatches = 0;
        self.runtime_profile_task_switches = 0;
        self.runtime_profile_address_space_switches = 0;
        self.runtime_profile_cross_cpu_migrations = 0;
        self.runtime_profile_lock_acquisitions = 0;
        self.runtime_profile_lock_wait_ns = 0;
        self.runtime_profile_lock_hold_ns = 0;
        self.runtime_profile_lock_wait_max_ns = 0;
        self.runtime_profile_lock_hold_max_ns = 0;
        self.runtime_profile_lock_hold_max_caller = None;
        self.runtime_profile_lock_hold_max_attributed_ns = 0;
        self.runtime_profile_deferred_wakes = 0;
        self.runtime_profile_phase_ns.fill(0);
        self.runtime_profile_runnable_samples = 0;
        self.runtime_profile_started_ticks = now_ticks;
        Some(profile)
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use super::*;

    static TEST_SCHEDULER_TEMPLATE: Scheduler = Scheduler::new();

    fn boxed_scheduler() -> Box<Scheduler> {
        let mut scheduler = Box::<Scheduler>::new_uninit();
        unsafe {
            // SAFETY: the immutable template is fully initialized and the
            // destination is one disjoint allocation of the exact same type.
            core::ptr::copy_nonoverlapping(
                core::ptr::addr_of!(TEST_SCHEDULER_TEMPLATE),
                scheduler.as_mut_ptr(),
                1,
            );
            scheduler.assume_init()
        }
    }

    fn profile_context(address_space_root: u64) -> TaskContext {
        TaskContext {
            scheduling_context: scheduling_context::SchedulingContext::bind(0, 1),
            saved_rsp: 0,
            #[cfg(test)]
            test_ready: true,
            ready_since_ticks: 0,
            blocked: false,
            blocked_since_ticks: 0,
            wake_armed: false,
            weight: NICE_0_LOAD,
            #[cfg(test)]
            vruntime_ns: 0,
            #[cfg(test)]
            exec_start_ticks: 0,
            address_space_root,
            kernel_stack_base: 0,
            kernel_stack_top: 0,
            alternate_kernel_stack_base: 0,
            alternate_kernel_stack_top: 0,
            user_mode: false,
            user_abi: None,
            console_session: ConsoleSessionHandle::SYSTEM,
            process_handle: None,
            process_id: None,
            user_stack: None,
            windows_thread_state: None,
        }
    }

    #[test]
    fn runtime_profile_is_windowed_ranked_and_destructive() {
        let mut scheduler = boxed_scheduler();
        scheduler.runtime_profile_started_ticks = 1;
        scheduler.runtime_profile_ns[3] = 7_000_000;
        scheduler.runtime_profile_dispatches[3] = 4;
        scheduler.runtime_profile_ns[9] = 11_000_000;
        scheduler.runtime_profile_dispatches[9] = 2;
        let now = 1 + crate::arch::rtc::ticks_per_second();
        let profile = scheduler.take_runtime_profile(now).expect("profile window");
        assert_eq!(profile.total_runtime_ns, 18_000_000);
        assert_eq!(profile.total_dispatches, 6);
        assert_eq!(profile.top[0].runtime_ns, 11_000_000);
        assert_eq!(profile.top[1].runtime_ns, 7_000_000);
        assert!(scheduler.runtime_profile_ns.iter().all(|value| *value == 0));
        assert!(
            scheduler
                .runtime_profile_dispatches
                .iter()
                .all(|value| *value == 0)
        );
        assert!(scheduler.take_runtime_profile(now).is_none());
    }

    #[test]
    fn pending_runtime_profile_is_single_slot_release_acquire_custody() {
        let pending = PendingSchedulerRuntimeProfile::new();
        let profile = SchedulerRuntimeProfile {
            elapsed_ms: 1_000,
            total_runtime_ns: 77,
            total_dispatches: 9,
            entry_causes: [0; SCHEDULER_ENTRY_CAUSE_COUNT],
            same_task_dispatches: 0,
            task_switches: 0,
            address_space_switches: 0,
            cross_cpu_migrations: 0,
            lock_acquisitions: 0,
            lock_wait_ns: 0,
            lock_hold_ns: 0,
            lock_wait_max_ns: 0,
            lock_hold_max_ns: 0,
            lock_hold_max_caller: None,
            lock_hold_max_attributed_ns: 0,
            deferred_wakes: 0,
            pick_scan_ns: 0,
            pick_scan_calls: 0,
            handoff_scan_ns: 0,
            handoff_scan_calls: 0,
            handoff_steps: [(0, 0); super::locality::HANDOFF_STEP_COUNT],
            sync_handoff_hits: 0,
            sync_handoff_misses: [0; super::locality::SYNC_HANDOFF_MISS_REASON_COUNT],
            sync_handoff_stale: [0; super::locality::SYNC_HANDOFF_STALE_REASON_COUNT],
            sync_handoff_arms: [(0, 0); super::locality::SYNC_HANDOFF_ARM_SITE_COUNT],
            sync_handoff_reply_custody_fail: [0;
                super::locality::SYNC_HANDOFF_REPLY_CUSTODY_FAIL_REASON_COUNT],
            sync_handoff_generation_fail_state: [0;
                super::locality::SYNC_HANDOFF_GENERATION_FAIL_STATE_COUNT],
            phase_ns: [0; SCHEDULER_PHASE_COUNT],
            runnable_samples: 0,
            live_ipc_donations: 0,
            divergent_identity_slot: usize::MAX,
            run_authority_divergence: None,
            top: [SchedulerRuntimeProfileEntry::default(); PROFILE_TOP_TASKS],
        };
        assert!(pending.publish(profile));
        assert!(!pending.publish(profile));
        let taken = pending.take().expect("published profile");
        assert_eq!(taken.total_runtime_ns, 77);
        assert!(pending.take().is_none());
        assert!(pending.publish(profile));
    }

    #[test]
    fn runtime_profile_milestone_fields_saturate_without_aliasing_halves() {
        assert_eq!(pack_u32_pair(7, 11), (7_u64 << 32) | 11);
        assert_eq!(pack_u32_pair(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn runtime_profile_entry_causes_are_exact_and_destructive() {
        let mut scheduler = boxed_scheduler();
        scheduler.runtime_profile_started_ticks = 1;
        for (cause, count) in [
            (SchedulerEntryCause::Timer, 2),
            (SchedulerEntryCause::Rtc, 3),
            (SchedulerEntryCause::RescheduleIpi, 4),
            (SchedulerEntryCause::Software, 5),
        ] {
            for _ in 0..count {
                scheduler.record_runtime_profile_entry(cause);
            }
        }
        let now = 1 + crate::arch::rtc::ticks_per_second();
        let profile = scheduler.take_runtime_profile(now).expect("profile window");
        assert_eq!(profile.entry_causes, [2, 3, 4, 5]);
        assert_eq!(scheduler.runtime_profile_entry_counts, [0; 4]);
    }

    #[test]
    fn runtime_profile_distinguishes_switches_roots_and_migrations() {
        let mut scheduler = boxed_scheduler();
        scheduler.contexts[1] = Some(profile_context(0x1000));
        scheduler.contexts[2] = Some(profile_context(0x2000));
        scheduler.starts[1] = Some(TaskStart {
            entry: |_| {},
            id: 41,
        });
        scheduler.starts[2] = Some(TaskStart {
            entry: |_| {},
            id: 42,
        });

        scheduler.record_runtime_profile_transition(1, 1, 0);
        scheduler.record_runtime_profile_transition(1, 2, 0);
        scheduler.record_runtime_profile_transition(1, 2, 1);

        assert_eq!(scheduler.runtime_profile_same_task_dispatches, 1);
        assert_eq!(scheduler.runtime_profile_task_switches, 2);
        assert_eq!(scheduler.runtime_profile_address_space_switches, 2);
        assert_eq!(scheduler.runtime_profile_cross_cpu_migrations, 1);

        scheduler.runtime_profile_started_ticks = 1;
        let now = 1 + crate::arch::rtc::ticks_per_second();
        let profile = scheduler.take_runtime_profile(now).expect("profile window");
        assert_eq!(profile.same_task_dispatches, 1);
        assert_eq!(profile.task_switches, 2);
        assert_eq!(profile.address_space_switches, 2);
        assert_eq!(profile.cross_cpu_migrations, 1);
        assert_eq!(scheduler.runtime_profile_same_task_dispatches, 0);
        assert_eq!(scheduler.runtime_profile_task_switches, 0);
        assert_eq!(scheduler.runtime_profile_address_space_switches, 0);
        assert_eq!(scheduler.runtime_profile_cross_cpu_migrations, 0);
        assert_eq!(scheduler.runtime_profile_last_cpu[2], 1);
        assert_eq!(scheduler.runtime_profile_last_task_id[2], 42);
    }

    #[test]
    fn runtime_profile_lock_totals_and_maxima_are_destructive() {
        let mut scheduler = boxed_scheduler();
        scheduler.record_runtime_profile_lock_wait(300);
        scheduler.record_runtime_profile_lock_wait(700);
        let worst = Location::caller();
        let lesser = Location::caller();
        scheduler.record_runtime_profile_lock_hold(2_000, 1_500, worst);
        scheduler.record_runtime_profile_lock_hold(1_000, 900, lesser);
        scheduler.runtime_profile_started_ticks = 1;

        let now = 1 + crate::arch::rtc::ticks_per_second();
        let profile = scheduler.take_runtime_profile(now).expect("profile window");
        assert_eq!(profile.lock_wait_ns, 1_000);
        assert_eq!(profile.lock_wait_max_ns, 700);
        assert_eq!(profile.lock_hold_ns, 3_000);
        assert_eq!(profile.lock_hold_max_ns, 2_000);
        // The attribution must follow the maximum, not the last sample.
        assert_eq!(profile.lock_hold_max_attributed_ns, 1_500);
        assert_eq!(
            profile.lock_hold_max_caller.map(Location::line),
            Some(worst.line())
        );
        assert_eq!(scheduler.runtime_profile_lock_wait_ns, 0);
        assert_eq!(scheduler.runtime_profile_lock_wait_max_ns, 0);
        assert_eq!(scheduler.runtime_profile_lock_hold_ns, 0);
        assert_eq!(scheduler.runtime_profile_lock_hold_max_ns, 0);
        assert!(scheduler.runtime_profile_lock_hold_max_caller.is_none());
        assert_eq!(scheduler.runtime_profile_lock_hold_max_attributed_ns, 0);
    }

    #[test]
    fn acquisition_prologue_is_a_disjoint_segment_with_its_wake_count() {
        let mut scheduler = boxed_scheduler();
        scheduler.record_runtime_profile_prologue(0, 40);
        scheduler.record_runtime_profile_prologue(3, 260);
        assert_eq!(
            scheduler.runtime_profile_phase_ns[SchedulerPhase::Prologue as usize],
            300
        );
        assert_eq!(scheduler.runtime_profile_phase_total_ns(), 300);
        scheduler.runtime_profile_started_ticks = 1;
        let now = 1 + crate::arch::rtc::ticks_per_second();
        let profile = scheduler.take_runtime_profile(now).expect("profile window");
        assert_eq!(profile.deferred_wakes, 3);
        assert_eq!(profile.phase_ns[SchedulerPhase::Prologue as usize], 300);
        assert_eq!(scheduler.runtime_profile_deferred_wakes, 0);
    }

    #[test]
    fn an_acquisition_site_below_a_percent_of_its_window_ends_the_report() {
        use super::acquire_site_is_reportable;
        let acquisitions = 144_602;

        // The four sites that were already named, and the fifth that was not.
        assert!(acquire_site_is_reportable(24_546, acquisitions));
        assert!(acquire_site_is_reportable(13_830, acquisitions));
        // Exactly one percent still explains a percent of the wait.
        assert!(acquire_site_is_reportable(1_447, acquisitions));
        assert!(!acquire_site_is_reportable(1_445, acquisitions));

        // A site with no acquisitions ends the report whatever the window is,
        // including an empty one, where every site would otherwise qualify.
        assert!(!acquire_site_is_reportable(0, acquisitions));
        assert!(!acquire_site_is_reportable(0, 0));
        assert!(acquire_site_is_reportable(1, 0));

        // A window large enough to overflow the percent multiply must not wrap
        // into reporting a site that explains nothing.
        assert!(!acquire_site_is_reportable(1, u64::MAX));
        assert!(acquire_site_is_reportable(u64::MAX, u64::MAX));
    }
}
