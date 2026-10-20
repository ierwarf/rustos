//! Bounded scheduler runtime attribution for acceptance and field diagnosis.
//! Owner: CPU counters/pending report; boundary: no resource/rendering in leaves.
//! Lifecycle: destructive due windows; concurrency: CPU leaves/UID history/CAS.
//! Failure: busy reports refuse/drop; forbidden: execution/billing/lifetime grants.
//! Evidence: native window/custody witnesses; no hardware/guard-deletion proof.
use super::*;
use core::cell::UnsafeCell;
use core::panic::Location;
use core::sync::atomic::{AtomicU8, Ordering};
mod local_shadow;
mod lock_census;
mod report;
use report::project_window;
use lock_census::{acquire_site_is_reportable, fnv1a32};

pub(in crate::multitask) mod owner;
const PROFILE_TOP_TASKS: usize = 4;
pub(super) const SCHEDULER_ENTRY_CAUSE_COUNT: usize = 4;
pub(super) const SCHEDULER_PHASE_COUNT: usize = 24;
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
    /// Disjoint split of [`SchedulerPhase::Account`], which measured as the
    /// dominant in-owner segment and covers three unrelated jobs: charging the
    /// outgoing task's runtime against its scheduling context and domain,
    /// validating the frame it published, and republishing its readiness.
    AccountCharge = 9,
    /// The outgoing frame's validation sweep, split out of `Account`.
    AccountValidate = 10,
    /// Ready/saved-frame republication, split out of `Account`.
    AccountReady = 11,
    /// Identity, clock, and elapsed-runtime derivation before any charge.
    AccountPreamble = 12,
    /// The scheduling-context and domain budget charge alone.
    AccountBudget = 13,
    /// Disjoint split of [`SchedulerPhase::ArchRestore`], which measured as the
    /// largest in-owner segment of a real task switch.
    ///
    /// The outgoing task's SIMD image. Both entry stubs already preserve
    /// `xmm0`-`xmm15`; this exists for the `ymm` upper halves and `MXCSR`,
    /// which no stub saves and which a second user task can destroy.
    ArchSimd = 14,
    /// Affinity snapshot/republication and the incoming frame's validation.
    ArchValidate = 15,
    /// CR3 activation and the TSS/syscall kernel-stack publication.
    ArchAddressSpace = 16,
    /// Data-segment reloads and the FS/GS base writes.
    ArchSegments = 17,
    /// Original typed call/reply publication, before dispatcher selection.
    IpcCallPublication = 18,
    IpcReplyPublication = 19,
    /// Actual original frame capture, funding admission, and final claim.
    ClaimCapture = 20,
    ClaimFunding = 21,
    ClaimCommit = 22,
    /// Borrowing the outgoing original Current/frame binding.
    ReturnCapture = 23,
}

/// Diagnostic-only wall time of the actual production boundary. No clock,
/// lock, allocation or telemetry instruction remains in shipping builds.
#[must_use]
pub(in crate::multitask) struct PhaseScope {
    #[cfg(rustos_scheduler_phase_profile)]
    phase: SchedulerPhase,
    #[cfg(rustos_scheduler_phase_profile)]
    started: u64,
}
#[inline(always)]
pub(in crate::multitask) fn phase_scope(phase: SchedulerPhase) -> PhaseScope {
    #[cfg(not(rustos_scheduler_phase_profile))]
    let _ = phase;
    PhaseScope {
        #[cfg(rustos_scheduler_phase_profile)]
        phase,
        #[cfg(rustos_scheduler_phase_profile)]
        started: crate::arch::clock::monotonic_nanos(),
    }
}
impl Drop for PhaseScope {
    #[inline(always)]
    fn drop(&mut self) {
        #[cfg(rustos_scheduler_phase_profile)]
        {
            let elapsed = crate::arch::clock::monotonic_nanos().saturating_sub(self.started);
            owner::local(|row| row.phase(self.phase, elapsed));
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::multitask) struct SchedulerRuntimeProfileEntry {
    pub(in crate::multitask) task_id: u64,
    pub(in crate::multitask) process_id: u64,
    pub(in crate::multitask) runtime_ns: u64,
    pub(in crate::multitask) dispatches: u64,
}

#[derive(Clone, Copy, Debug, Default)]
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
    /// Subset of the pick-hint attempts that found a ready synchronous IPC
    /// pick hint (`Some`), so `select` resolved without the CFS vruntime scan.
    pub(in crate::multitask) sync_handoff_hits: u64,
    /// The rest of the pick-hint attempts, split by why they missed.
    /// Indexed by `locality::SyncHandoffMissReason`. `sync_handoff_hits` plus
    /// this sum is the attempt count itself.
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
    pub(in crate::multitask) local_dispatch_shadow: (u64, [u64; 7], u64, u64, u64, u64, u64, u64),
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
                // ORDERING: claim exclusive producer custody after prior consumer release.
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
        // ORDERING: publish the complete non-atomic payload to the consumer.
        self.state.store(PROFILE_READY, Ordering::Release);
        true
    }

    fn take(&self) -> Option<SchedulerRuntimeProfile> {
        self.state
            .compare_exchange(
                PROFILE_READY,
                PROFILE_BUSY,
                // ORDERING: observe READY payload while excluding another consumer.
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .ok()?;
        // SAFETY: Acquire above observes the complete producer write and BUSY
        // excludes another producer until the consumer clears EMPTY.
        let profile = unsafe { (*self.profile.get()).take() };
        // ORDERING: payload removal precedes reopening exclusive producer custody.
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

// Formatting and transporting a zero row is still guest work. Default builds
// omit only empty phase rows; real values and explicit profiling's zero
// observations remain intact. No budget, error, census, or task row uses this.
#[inline(always)]
fn emit_phase_row(profile_enabled: bool, arg0: u64, arg1: u64, emit: impl FnOnce(u64, u64)) {
    if profile_enabled || arg0 != 0 || arg1 != 0 {
        emit(arg0, arg1);
    }
}

fn record_phase_row(name: &'static str, arg0: u64, arg1: u64) {
    emit_phase_row(cfg!(rustos_scheduler_phase_profile), arg0, arg1, |arg0, arg1| {
        crate::debug::record_milestone(crate::debug::LogCategory::Sched, name, arg0, arg1);
    });
}

/// Emits one census row, skipping rows that carry no information.
///
/// A zero count against a nonzero denominator is a real observation - "none of
/// N attempts took this path" - and is kept. A row where both are zero says
/// only that the window was idle, and repeating a hundred of those every
/// second is what made a boot log mostly scheduler telemetry.
fn record_census_row(name: &'static str, count: u64, total: u64) {
    if count == 0 && total == 0 {
        return;
    }
    crate::debug::record_milestone(crate::debug::LogCategory::Sched, name, count, total);
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
    let process_identity_divergences = super::super::process_table::publication_divergence_count();
    if process_identity_divergences != 0 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Process,
            "process-identity-divergence",
            process_identity_divergences,
            0,
        );
    }
    // The ranked class/site census is a diagnosis build feature. Rendering
    // its dozen debugcon records every second is observable work (one VM exit
    // per byte under KVM) and, on SMP, can consume most of an isolated
    // benchmark's wall-clock budget. Shipping builds retain the acquisition
    // counters used by exact work-budget assertions but neither drain nor
    // enable the rotating per-site census.
    #[cfg(rustos_lock_phase_profile)]
    lock_census::drain_class_and_site_census();
    let scheduling_context = super::scheduling_context::drain_runtime_counters();
    // Rendered here, outside the scheduler guard, for the reason
    // `latch_budget_exhaustion` documents.
    if let Some((owner_task_id, charged_ns)) =
        super::scheduling_context::take_latched_budget_exhaustion()
    {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "scheduling-budget-exhausted",
            owner_task_id,
            charged_ns,
        );
    }
    // A CPU that parked on idle because its current task lost run-queue custody
    // made a correct choice, but the task must then be woken elsewhere. A run
    // that misses its deadline without panicking is either accompanied by this
    // or it is not, and that is the difference between a scheduler stall and an
    // unrelated one.
    let fallback_idle = super::runqueue_policy::take_fallback_idle_dispatch_window();
    if fallback_idle != 0 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "scheduler-fallback-idle",
            fallback_idle,
            0,
        );
    }
    // A domain that refuses a dispatch it was admitted for is a lost race the
    // dispatch recovers from by reselecting, so it must not be silent: a rise
    // here means the admission scan and the budget commit are disagreeing more
    // often. arg0=refusals this window, arg1=(slot<<32)|(domain<<8)|cause.
    if let Some((refusals, last)) = super::scheduling_context::take_domain_budget_refusal_window() {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "scheduling-domain-budget-refused",
            refusals,
            last,
        );
    }
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
    // The pick hint's hit/attempt split: how often the synchronous IPC pick
    // hint actually held a ready slot, against how often it was asked. Hits and
    // the four miss causes partition every attempt, so their sum *is* the
    // attempt count -- it needs no separate counter to stay consistent with the
    // rows below. arg0=hits, arg1=attempts.
    let sync_handoff_attempts = profile
        .sync_handoff_misses
        .iter()
        .fold(profile.sync_handoff_hits, |total, misses| total + misses);
    record_census_row(
        "kernel-scheduler-step-sync-hits",
        profile.sync_handoff_hits,
        sync_handoff_attempts,
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
        (
            profile.sync_handoff_misses[3],
            "kernel-scheduler-step-sync-miss-flag-stale",
        ),
    ] {
        record_census_row(name, reason_count, sync_handoff_attempts);
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
        record_census_row(name, stale_count, drained_stale);
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
        record_census_row(name, state_count, generation_fail_total);
    }
    // Owner publication plus the deferred wake drain, which every acquisition
    // pays before its caller runs. arg0=prologue us, arg1=wakes drained.
    record_phase_row(
        "kernel-scheduler-phase-prologue",
        profile.phase_ns[SchedulerPhase::Prologue as usize] / 1_000,
        profile.deferred_wakes,
    );
    // The three disjoint jobs inside `Account`, which is the dominant in-owner
    // segment. arg0=(charge us, validate us), arg1=(ready us, 0).
    record_phase_row(
        "kernel-scheduler-phase-account",
        pack_u32_pair(
            profile.phase_ns[SchedulerPhase::AccountCharge as usize] / 1_000,
            profile.phase_ns[SchedulerPhase::AccountValidate as usize] / 1_000,
        ),
        pack_u32_pair(
            profile.phase_ns[SchedulerPhase::AccountReady as usize] / 1_000,
            0,
        ),
    );
    // arg0=(preamble us, budget us). Both are inside `AccountCharge`.
    record_phase_row(
        "kernel-scheduler-phase-account-charge",
        pack_u32_pair(
            profile.phase_ns[SchedulerPhase::AccountPreamble as usize] / 1_000,
            profile.phase_ns[SchedulerPhase::AccountBudget as usize] / 1_000,
        ),
        0,
    );
    // Disjoint in-owner segments in microseconds.
    // arg0=(account, balance), arg1=(validate, select).
    record_phase_row(
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
    record_phase_row(
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
    // The four disjoint jobs inside `ArchRestore`, the largest segment of a
    // real task switch. arg0=(simd us, validate us), arg1=(address space us,
    // segments us).
    record_phase_row(
        "kernel-scheduler-phase-arch",
        pack_u32_pair(
            profile.phase_ns[SchedulerPhase::ArchSimd as usize] / 1_000,
            profile.phase_ns[SchedulerPhase::ArchValidate as usize] / 1_000,
        ),
        pack_u32_pair(
            profile.phase_ns[SchedulerPhase::ArchAddressSpace as usize] / 1_000,
            profile.phase_ns[SchedulerPhase::ArchSegments as usize] / 1_000,
        ),
    );
    // Logical-CPU-index derivations since boot. Each executes `RDTSCP`, so the
    // per-window delta against the dispatch count says how many a dispatch
    // pays. arg0=total, arg1=0.
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-identity-derivations",
        nucleus_core::util::lockdep::work_budget::total_identity_derivations(),
        0,
    );
    // arg0=(commit, architectural restore), arg1=(attributed total, hold total).
    record_phase_row(
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
    #[cfg(rustos_scheduler_phase_profile)]
    {
        local_shadow::emit_milestones(&profile);
        for (name, first, second) in [
            ("kernel-dispatch-phase-ipc-us", SchedulerPhase::IpcCallPublication, SchedulerPhase::IpcReplyPublication),
            ("kernel-dispatch-phase-claim-us", SchedulerPhase::ClaimCapture, SchedulerPhase::ClaimFunding),
            ("kernel-dispatch-phase-commit-us", SchedulerPhase::ClaimCommit, SchedulerPhase::ReturnCapture),
        ] {
            crate::debug::record_milestone(crate::debug::LogCategory::Sched, name,
                profile.phase_ns[first as usize] / 1_000,
                profile.phase_ns[second as usize] / 1_000);
        }
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

/// Consume an original independent counter window. Attribution is telemetry,
/// not a frame/funding/dispatch grant; it may observe later UID publication.
pub(in crate::multitask) fn take_owned(
    turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'static>, now: u64,
) -> Option<SchedulerRuntimeProfile> {
    let period = crate::arch::rtc::ticks_per_second();
    observe_original_current(turn);
    let window = owner::take(now, period)?;
    let divergent = (0..MAX_TASK).find(|&slot| current_identity::binding_publication_diverges(slot));
    Some(project_window(window, now, period, &|slot| {
        let p = current_identity::read_dispatch(slot);
        (p.and_then(|p| p.task_id).unwrap_or(0), p.and_then(|p| p.process_id).unwrap_or(0))
    }, super::donation_ledger::live_len() as u64, divergent))
}
/// Independent diagnostic of this CPU's ORIGINAL pre-selection Current.
/// Remote CPU tables are not a coherent snapshot once the directory is gone;
/// do not reconstruct a remote execution grant or label in-flight halves bad.
/// Each entering CPU samples its own retained original bank BEFORE the shared
/// report epoch decides which collector can drain. This is not a remote-table
/// sweep or proof of agreement for CPUs taking the periodic continuation path.
/// Identity publication is separately checked under its original writer leaf.
pub(super) fn observe_original_current(
    turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'static>,
) {
    assert!(!turn.owns_priority_choice(), "diagnostic must precede selection");
    let (cpu, slot) = turn.original_current().expect("diagnostic lacks original Current bank");
    let owner = super::runqueue::owner(slot);
    use super::super::run_authority::{self, Mismatch};
    let mismatch = if !current_identity::execution::banks::owns_current(cpu, slot) {
        Some(Mismatch::CurrentPermitMissing)
    } else if owner.state == super::runqueue::RunOwnerState::Running {
        (owner.cpu != Some(cpu)).then_some(Mismatch::RunningOnDifferentCpus)
    } else if owner.state.is_terminal() {
        // Desired stop may win while original physical Current still executes
        // its last kernel continuation. Bank/permit persist through final ACK.
        None
    } else {
        Some(Mismatch::LegacyRunningQueueNot)
    };
    if let Some(kind) = mismatch { run_authority::record(slot, kind); }
}

pub(in crate::multitask) fn record_entry(cause: SchedulerEntryCause) {
    owner::local(|window| window.entry(cause));
}

/// Count work in the independent IRQ entry without lending counter mutation
/// authority to its caller. No diagnostic counter is a scheduling grant.
pub(in crate::multitask) fn record_prologue(wakes: usize, elapsed: u64) {
    owner::local(|row| {
        row.runtime_profile_deferred_wakes = row.runtime_profile_deferred_wakes.saturating_add(wakes as u64);
        row.phase(SchedulerPhase::Prologue, elapsed);
    });
}



#[cfg(test)]
mod tests {
    #[test]
    fn actual_phase_emitter_omits_only_disabled_empty_rows_and_keeps_real_observations() {
        for (enabled, a, b, expected) in [
            (false, 0, 0, false), (false, 1, 0, true),
            (false, 0, 1, true), (false, u64::MAX, u64::MAX, true),
            (true, 0, 0, true), (true, 1, 0, true), (true, 0, 1, true),
        ] {
            let mut observed = None;
            super::emit_phase_row(enabled, a, b, |a, b| observed = Some((a, b)));
            assert_eq!(observed, expected.then_some((a, b)), "enabled={enabled}, args=({a},{b})");
        }
    }
    // Earlier native counter input, not a production Scheduler authority.
    // Both fixtures and the actual independent drain use the SAME projector.
    fn take_counter_fixture(counters: &mut super::owner::Counters, now: u64)
        -> Option<super::SchedulerRuntimeProfile> {
        let period = crate::arch::rtc::ticks_per_second();
        if counters.runtime_profile_started_ticks == 0 {
            counters.runtime_profile_started_ticks = now;
            return None;
        }
        if now.saturating_sub(counters.runtime_profile_started_ticks) < period { return None; }
        let started = counters.runtime_profile_started_ticks;
        let mut window = counters.take();
        window.runtime_profile_started_ticks = started;
        counters.runtime_profile_started_ticks = now;
        Some(super::project_window(window, now, period, &|_| (0, 0), 0, None))
    }
    use alloc::boxed::Box;

    use super::*;

    fn boxed_counters() -> Box<owner::Counters> {
        Box::new(owner::Counters::new())
    }

    #[test]
    fn runtime_profile_is_windowed_ranked_and_destructive() {
        let mut scheduler = boxed_counters();
        scheduler.runtime_profile_started_ticks = 1;
        scheduler.runtime_profile_ns[3] = 7_000_000;
        scheduler.runtime_profile_dispatches[3] = 4;
        scheduler.runtime_profile_ns[9] = 11_000_000;
        scheduler.runtime_profile_dispatches[9] = 2;
        let now = 1 + crate::arch::rtc::ticks_per_second();
        let profile = take_counter_fixture(&mut scheduler, now).expect("profile window");
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
        assert!(take_counter_fixture(&mut scheduler, now).is_none());
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
            local_dispatch_shadow: (0, [0; 7], 0, 0, 0, 0, 0, 0),
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
        let mut scheduler = boxed_counters();
        scheduler.runtime_profile_started_ticks = 1;
        for (cause, count) in [
            (SchedulerEntryCause::Timer, 2),
            (SchedulerEntryCause::Rtc, 3),
            (SchedulerEntryCause::RescheduleIpi, 4),
            (SchedulerEntryCause::Software, 5),
        ] {
            for _ in 0..count {
                scheduler.entry(cause);
            }
        }
        let now = 1 + crate::arch::rtc::ticks_per_second();
        let profile = take_counter_fixture(&mut scheduler, now).expect("profile window");
        assert_eq!(profile.entry_causes, [2, 3, 4, 5]);
        assert_eq!(scheduler.runtime_profile_entry_counts, [0; 4]);
    }

    #[test]
    fn runtime_profile_distinguishes_switches_roots_and_migrations() {
        let _namespace = crate::multitask::scheduler::tests::boxed_scheduler();
        // Earlier switch/root samples are modeled diagnostic inputs, not
        // execution permission. Migration history and destructive projection
        // execute the SAME independent production owners, without a catalog.
        owner::clear_history(2);
        assert!(!owner::migration(2, 42, 0));
        assert!(owner::migration(2, 42, 1));
        let mut counters = boxed_counters();
        counters.runtime_profile_same_task_dispatches = 1;
        counters.runtime_profile_task_switches = 2;
        counters.runtime_profile_address_space_switches = 2;
        counters.runtime_profile_cross_cpu_migrations = 1;
        counters.runtime_profile_started_ticks = 1;
        let now = 1 + crate::arch::rtc::ticks_per_second();
        let profile = take_counter_fixture(&mut counters, now).unwrap();
        assert_eq!(profile.same_task_dispatches, 1);
        assert_eq!(profile.task_switches, 2);
        assert_eq!(profile.address_space_switches, 2);
        assert_eq!(profile.cross_cpu_migrations, 1);
        assert_eq!(counters.runtime_profile_same_task_dispatches, 0);
        assert_eq!(counters.runtime_profile_task_switches, 0);
        assert_eq!(counters.runtime_profile_address_space_switches, 0);
        assert_eq!(counters.runtime_profile_cross_cpu_migrations, 0);
        // The original per-slot history is NOT spent with a report window.
        assert!(!owner::migration(2, 42, 1));
        assert!(!owner::migration(2, 43, 0));
        owner::clear_history(2);
    }

    #[test]
    fn runtime_profile_lock_totals_and_maxima_are_destructive() {
        let mut scheduler = boxed_counters();
        scheduler.wait(300);
        scheduler.wait(700);
        let worst = Location::caller();
        let lesser = Location::caller();
        scheduler.hold(2_000, 1_500, worst);
        scheduler.hold(1_000, 900, lesser);
        scheduler.runtime_profile_started_ticks = 1;

        let now = 1 + crate::arch::rtc::ticks_per_second();
        let profile = take_counter_fixture(&mut scheduler, now).expect("profile window");
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
        let _namespace = crate::multitask::scheduler::tests::boxed_scheduler();
        owner::local(|row| *row = owner::Counters::new());
        record_prologue(0, 40);
        record_prologue(3, 260);
        let mut window = owner::local(|row| {
            assert_eq!(row.runtime_profile_phase_ns[SchedulerPhase::Prologue as usize], 300);
            assert_eq!(row.runtime_profile_phase_ns.iter().copied().sum::<u64>(), 300);
            row.take()
        });
        // Projection is outside the original diagnostic leaf, never nested work.
        window.runtime_profile_started_ticks = 1;
        let now = 1 + crate::arch::rtc::ticks_per_second();
        let profile = take_counter_fixture(&mut window, now).unwrap();
        assert_eq!(profile.deferred_wakes, 3);
        assert_eq!(profile.phase_ns[SchedulerPhase::Prologue as usize], 300);
        assert_eq!(owner::local(|row| row.runtime_profile_deferred_wakes), 0);
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
