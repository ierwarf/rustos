//! Bounded CPU-local fair selection.
//!
//! - **Owner:** the serialized scheduler owns exact per-task last-CPU policy
//!   metadata; the runtime profiler owns a separate resettable observation.
//! - **Boundary:** locality is considered only after class, affinity,
//!   lifecycle, idle ownership, and remote-running-owner admission.
//! - **Invariant:** a local candidate may beat the same-class global minimum
//!   by at most `SCHED_CPU_LOCALITY_LAG_NS`; exact handoffs never enter here.
//! - **Failure:** slot reuse clears locality and an out-of-range CPU panics.

use super::*;

impl Scheduler {
    fn candidate_is_local_to_current_cpu(&self, slot: usize) -> bool {
        if slot == self.current_task_slot() {
            return true;
        }
        let logical_cpu = u8::try_from(nucleus_core::util::lockdep::current_cpu_index())
            .expect("logical CPU index exceeds scheduler locality capacity");
        self.slot_last_cpu(slot) == logical_cpu
    }

    /// Select a local fair candidate only while its virtual-runtime lag from
    /// the same-class global minimum is explicitly bounded. The caller has
    /// already enforced class, affinity, lifecycle, and remote-owner rules.
    fn prefer_local_candidate(
        global: Option<(usize, u64)>,
        local: Option<(usize, u64)>,
    ) -> Option<usize> {
        match (global, local) {
            (Some((_global_slot, global_vruntime)), Some((local_slot, local_vruntime)))
                if local_vruntime <= global_vruntime.saturating_add(SCHED_CPU_LOCALITY_LAG_NS) =>
            {
                Some(local_slot)
            }
            (Some((global_slot, _)), _) => Some(global_slot),
            (None, Some((local_slot, _))) => Some(local_slot),
            (None, None) => None,
        }
    }

    pub(super) fn record_task_dispatch_cpu(&mut self, slot: usize, logical_cpu: usize) {
        self.record_slot_last_cpu(
            slot,
            u8::try_from(logical_cpu)
                .expect("logical CPU index exceeds scheduler locality capacity"),
        );
    }

    /// Walk scheduling classes in priority order and select the global
    /// least-vruntime task or one bounded local alternative in that class.
    pub(super) fn pick_min_vruntime(&self, current: usize) -> Option<usize> {
        let started_ns = scan_clock();
        let picked = self.pick_min_vruntime_inner(current);
        charge_pick_scan(scan_clock().saturating_sub(started_ns));
        picked
    }

    fn pick_min_vruntime_inner(&self, current: usize) -> Option<usize> {
        let mut best_by_class = [None::<(usize, u64)>; SchedClass::COUNT];
        let mut local_by_class = [None::<(usize, u64)>; SchedClass::COUNT];
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !self.slot_is_runnable(slot) || !self.context_is_schedulable(slot, context) {
                continue;
            }
            let Some(class) = self.slot_class(slot) else {
                continue;
            };
            let vruntime = self.slot_vruntime(slot);
            let key = if slot == current {
                vruntime.saturating_add(1)
            } else {
                vruntime
            };
            let candidate = &mut best_by_class[class.index()];
            if candidate
                .map(|(_, best_key)| key < best_key)
                .unwrap_or(true)
            {
                *candidate = Some((slot, key));
            }
            if self.candidate_is_local_to_current_cpu(slot) {
                let local = &mut local_by_class[class.index()];
                if local.map(|(_, best_key)| key < best_key).unwrap_or(true) {
                    *local = Some((slot, key));
                }
            }
        }
        for class in 0..SchedClass::COUNT {
            if best_by_class[class].is_some() {
                return Self::prefer_local_candidate(best_by_class[class], local_by_class[class]);
            }
        }
        None
    }

    pub(super) fn pick_min_vruntime_excluding(&self, excluded: usize) -> Option<usize> {
        let started_ns = scan_clock();
        let picked = self.pick_min_vruntime_excluding_inner(excluded);
        charge_pick_scan(scan_clock().saturating_sub(started_ns));
        picked
    }

    fn pick_min_vruntime_excluding_inner(&self, excluded: usize) -> Option<usize> {
        let mut best_by_class = [None::<(usize, u64, usize)>; SchedClass::COUNT];
        let mut local_by_class = [None::<(usize, u64, usize)>; SchedClass::COUNT];
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if slot == excluded || !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !self.slot_is_runnable(slot) || !self.context_is_schedulable(slot, context) {
                continue;
            }
            let Some(class) = self.slot_class(slot) else {
                continue;
            };
            let distance = (slot + MAX_TASK - excluded) % MAX_TASK;
            let candidate = &mut best_by_class[class.index()];
            let vruntime = self.slot_vruntime(slot);
            if candidate
                .map(|(_, best_vruntime, best_distance)| {
                    vruntime < best_vruntime
                        || (vruntime == best_vruntime && distance < best_distance)
                })
                .unwrap_or(true)
            {
                *candidate = Some((slot, vruntime, distance));
            }
            if self.candidate_is_local_to_current_cpu(slot) {
                let local = &mut local_by_class[class.index()];
                if local
                    .map(|(_, best_vruntime, best_distance)| {
                        vruntime < best_vruntime
                            || (vruntime == best_vruntime && distance < best_distance)
                    })
                    .unwrap_or(true)
                {
                    *local = Some((slot, vruntime, distance));
                }
            }
        }
        for class in 0..SchedClass::COUNT {
            if best_by_class[class].is_some() {
                return Self::prefer_local_candidate(
                    best_by_class[class].map(|(slot, vruntime, _)| (slot, vruntime)),
                    local_by_class[class].map(|(slot, vruntime, _)| (slot, vruntime)),
                );
            }
        }
        None
    }

    /// Pick one task in an explicitly selected class without permitting the
    /// locality preference to cross that class boundary.
    pub(super) fn pick_min_vruntime_in_class(
        &self,
        current: usize,
        class: SchedClass,
    ) -> Option<usize> {
        let mut best: Option<(usize, u64)> = None;
        let mut local: Option<(usize, u64)> = None;
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(ctx) = self.contexts[slot] else {
                continue;
            };
            if !self.slot_is_runnable(slot) || !self.context_is_schedulable(slot, ctx) {
                continue;
            }
            if self.slot_class(slot) != Some(class) {
                continue;
            }
            let vruntime = self.slot_vruntime(slot);
            let key = if slot == current {
                vruntime.saturating_add(1)
            } else {
                vruntime
            };
            match best {
                None => best = Some((slot, key)),
                Some((_, best_key)) if key < best_key => best = Some((slot, key)),
                _ => {}
            }
            if self.candidate_is_local_to_current_cpu(slot) {
                match local {
                    None => local = Some((slot, key)),
                    Some((_, local_key)) if key < local_key => local = Some((slot, key)),
                    _ => {}
                }
            }
        }
        Self::prefer_local_candidate(best, local)
    }

    pub(super) fn pick_burst_alternate_in_current_class(&self, current: usize) -> Option<usize> {
        let class = self.slot_class(current)?;
        let mut best: Option<(usize, u64)> = None;
        let mut local: Option<(usize, u64)> = None;
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if slot == current || !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(ctx) = self.contexts[slot] else {
                continue;
            };
            if !self.slot_is_runnable(slot) || !self.context_is_schedulable(slot, ctx) {
                continue;
            }
            if self.slot_class(slot) != Some(class) {
                continue;
            }
            let key = self.slot_vruntime(slot);
            if best
                .map(|(_, current_key)| key < current_key)
                .unwrap_or(true)
            {
                best = Some((slot, key));
            }
            if self.candidate_is_local_to_current_cpu(slot)
                && local
                    .map(|(_, current_key)| key < current_key)
                    .unwrap_or(true)
            {
                local = Some((slot, key));
            }
        }
        Self::prefer_local_candidate(best, local)
    }
}

/// Self-timed cost of the fair-class pick scan.
///
/// The phase marks charge this to `SelectHandoff`, because both pick functions
/// are lexically inside that span, so the scheduler profile reported handoff at
/// 238 ms per second of lock hold — 40 percent of the total — while
/// `SelectPick` read under 1 ms. A segment that names the wrong work invites
/// optimising the wrong thing; one reorder of the handoff predicates was
/// already spent that way and moved nothing.
///
/// Static rather than a `Scheduler` field because both functions take `&self`.
/// The scheduler lock already serializes every writer.
static PICK_SCAN_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static PICK_SCAN_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn charge_pick_scan(elapsed_ns: u64) {
    // ORDERING: Relaxed. Diagnostic counters with no other state attached, read
    // only by the once-per-second drain.
    PICK_SCAN_NS.fetch_add(elapsed_ns, core::sync::atomic::Ordering::Relaxed);
    PICK_SCAN_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Self-timed cost of the handoff chain's own scans.
///
/// Charged by the overdue-class and reserved-user scans, which are the other
/// two O(local runnable) walks inside the `SelectHandoff` span. Splitting them
/// from the fair pick is what says which of the three costs; the fair pick
/// turned out to be 29 ms of a 216 ms segment, so it is not the one.
static HANDOFF_SCAN_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static HANDOFF_SCAN_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The scan stopwatch's clock, or zero when dispatch is not instrumented.
///
/// Both pick scans run on every dispatch and each was bracketed by two
/// `lfence; rdtsc` reads plus two globally shared atomic adds -- to time a walk
/// over a handful of slots. See `Scheduler::mark_phase`.
#[inline]
fn scan_clock() -> u64 {
    #[cfg(rustos_scheduler_phase_profile)]
    {
        crate::arch::clock::monotonic_nanos()
    }
    #[cfg(not(rustos_scheduler_phase_profile))]
    {
        0
    }
}

pub(in crate::multitask) fn charge_handoff_scan(elapsed_ns: u64) {
    // ORDERING: Relaxed; diagnostic counters drained once per second.
    HANDOFF_SCAN_NS.fetch_add(elapsed_ns, core::sync::atomic::Ordering::Relaxed);
    HANDOFF_SCAN_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Takes the window's handoff-scan cost, clearing it for the next window.
pub(in crate::multitask) fn take_handoff_scan_window() -> (u64, u64) {
    (
        HANDOFF_SCAN_NS.swap(0, core::sync::atomic::Ordering::Relaxed),
        HANDOFF_SCAN_CALLS.swap(0, core::sync::atomic::Ordering::Relaxed),
    )
}

/// Takes the window's pick-scan cost, clearing it for the next window.
pub(in crate::multitask) fn take_pick_scan_window() -> (u64, u64) {
    (
        PICK_SCAN_NS.swap(0, core::sync::atomic::Ordering::Relaxed),
        PICK_SCAN_CALLS.swap(0, core::sync::atomic::Ordering::Relaxed),
    )
}

/// Per-member cost of the handoff chain itself.
///
/// Splitting the fair pick and the two class scans out of `SelectHandoff` left
/// 63 percent of the segment — about 5.6 us of every dispatch — attributed to
/// nothing but the chain's own six steps. Each step is a separate hint queue
/// with its own per-CPU policy lock traffic, so which of them owns that time is
/// not derivable from the source; it has to be measured before any of them is
/// touched.
///
/// Steps 2 and 5 re-enter the overdue class scan, so their totals overlap
/// `HANDOFF_SCAN_NS` by construction. Every other step is disjoint.
///
/// Step 6 is the chain's single acquisition of this CPU's dispatch policy.
///
/// The first measurement returned every step between 0.95 and 2.5 us regardless
/// of how much work it does — the step that finds an empty queue and returns
/// cost nearly as much as the one that scans. Two controls priced that: an
/// empty timed span read 0.032 us and one bare policy acquisition read
/// 0.724 us, against a 0.72 us per-step floor. The chain was paying for
/// acquisitions, not decisions, about ten of them per dispatch. It now takes
/// the guard once and threads it, so step 6 is the entire acquisition cost the
/// chain pays.
/// The pick hint's `Some` outcomes: the synchronous IPC pick-hint queue held a
/// ready receiver or caller, so `select` resolved in one FIFO pop instead of
/// the CFS-style vruntime scan. This plus the three miss causes below partition
/// every attempt, so the gap between this and their sum is how often a dispatch
/// pays for the scan the hint queue exists to skip.
///
/// This does not say whether the *dispatch itself* was skippable — every
/// dispatch still runs the full seven-phase pipeline (account/balance/
/// validate/select/commit/arch_restore/prologue) whether or not this hits.
/// It only sizes the FIFO/scan trade at the specific step the trade lives in.
static SYNC_HANDOFF_HITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub(in crate::multitask) fn record_sync_handoff_hit() {
    // ORDERING: Relaxed; diagnostic counter drained once per second.
    SYNC_HANDOFF_HITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Takes the window's sync-handoff hit count, clearing it for the next window.
pub(in crate::multitask) fn take_sync_handoff_hit_window() -> u64 {
    SYNC_HANDOFF_HITS.swap(0, core::sync::atomic::Ordering::Relaxed)
}

/// The pick hint's `None` outcomes, split by cause. Hits (above) plus these
/// three are the whole attempt count.
pub(in crate::multitask) const SYNC_HANDOFF_MISS_REASON_COUNT: usize = 3;

#[derive(Clone, Copy)]
pub(in crate::multitask) enum SyncHandoffMissReason {
    /// Nothing was armed for this CPU's queue (or it already drained) —
    /// caught by the fast `pending()` check before any lock is taken.
    QueueEmpty = 0,
    /// The queue may hold a ready record, but this CPU's consecutive-hit
    /// streak already reached `MAX_CONSECUTIVE_SYNC_HANDOFFS`.
    StreakCapped = 1,
    /// The consume loop ran dry — either it started empty (a narrow race
    /// against the outer `pending()` check) or every record it held was
    /// discarded as stale. See `SyncHandoffStaleReason` for why.
    DrainedStale = 2,
}

static SYNC_HANDOFF_MISSES: [core::sync::atomic::AtomicU64; SYNC_HANDOFF_MISS_REASON_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; SYNC_HANDOFF_MISS_REASON_COUNT];

pub(in crate::multitask) fn record_sync_handoff_miss(reason: SyncHandoffMissReason) {
    // ORDERING: Relaxed; diagnostic counter drained once per second.
    SYNC_HANDOFF_MISSES[reason as usize].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Takes the window's sync-handoff miss breakdown, clearing it for the next
/// window. Indexed by `SyncHandoffMissReason`.
pub(in crate::multitask) fn take_sync_handoff_miss_window() -> [u64; SYNC_HANDOFF_MISS_REASON_COUNT]
{
    core::array::from_fn(|reason| {
        SYNC_HANDOFF_MISSES[reason].swap(0, core::sync::atomic::Ordering::Relaxed)
    })
}

/// Sub-reasons for `DrainedStale`: which check inside
/// `synchronous_handoff_record_is_ready` discarded a queued record. One
/// consume attempt can discard more than one record (dedup only prevents two
/// records for the *same* task), so these are per-discard counts — a
/// proportion within `DrainedStale`, not a per-dispatch partition of it.
pub(in crate::multitask) const SYNC_HANDOFF_STALE_REASON_COUNT: usize = 3;

#[derive(Clone, Copy)]
pub(in crate::multitask) enum SyncHandoffStaleReason {
    /// The slot's published start either disappeared or no longer names the
    /// queued task id — the queued task retired or the slot was reused.
    Identity = 0,
    /// The record's dispatch custody no longer matches — a migration or a
    /// generation change since the record was enqueued.
    Custody = 1,
    /// The slot no longer passes `pick_hint_candidate_slot` — armed, but not
    /// presently schedulable (e.g. already dispatched through another entry
    /// point).
    NotCandidate = 2,
}

static SYNC_HANDOFF_STALE: [core::sync::atomic::AtomicU64; SYNC_HANDOFF_STALE_REASON_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; SYNC_HANDOFF_STALE_REASON_COUNT];

pub(in crate::multitask) fn record_sync_handoff_stale(reason: SyncHandoffStaleReason) {
    // ORDERING: Relaxed; diagnostic counter drained once per second.
    SYNC_HANDOFF_STALE[reason as usize].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Takes the window's stale-discard breakdown, clearing it for the next
/// window. Indexed by `SyncHandoffStaleReason`.
pub(in crate::multitask) fn take_sync_handoff_stale_window()
-> [u64; SYNC_HANDOFF_STALE_REASON_COUNT] {
    core::array::from_fn(|reason| {
        SYNC_HANDOFF_STALE[reason].swap(0, core::sync::atomic::Ordering::Relaxed)
    })
}

/// Arm-side outcome, split by which direction armed it: the call path waking
/// a receiver (`set_next_synchronous_pick_hint`, reached from
/// `commit_ipc_call_handoff`) versus the reply path waking a caller
/// (`enqueue_reply_wake`). Tests whether a round trip's hint shortfall is
/// one-sided, independent of anything on the consume side above.
pub(in crate::multitask) const SYNC_HANDOFF_ARM_SITE_COUNT: usize = 2;

#[derive(Clone, Copy)]
pub(in crate::multitask) enum SyncHandoffArmSite {
    Call = 0,
    Reply = 1,
}

static SYNC_HANDOFF_ARM_ACCEPTED: [core::sync::atomic::AtomicU64; SYNC_HANDOFF_ARM_SITE_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; SYNC_HANDOFF_ARM_SITE_COUNT];
static SYNC_HANDOFF_ARM_REJECTED: [core::sync::atomic::AtomicU64; SYNC_HANDOFF_ARM_SITE_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; SYNC_HANDOFF_ARM_SITE_COUNT];

pub(in crate::multitask) fn record_sync_handoff_arm(site: SyncHandoffArmSite, accepted: bool) {
    // ORDERING: Relaxed; diagnostic counter drained once per second.
    let table = if accepted {
        &SYNC_HANDOFF_ARM_ACCEPTED
    } else {
        &SYNC_HANDOFF_ARM_REJECTED
    };
    table[site as usize].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Takes the window's arm accepted/rejected counts, clearing it for the next
/// window. Indexed by `SyncHandoffArmSite`; `(accepted, rejected)` per site.
pub(in crate::multitask) fn take_sync_handoff_arm_window()
-> [(u64, u64); SYNC_HANDOFF_ARM_SITE_COUNT] {
    core::array::from_fn(|site| {
        (
            SYNC_HANDOFF_ARM_ACCEPTED[site].swap(0, core::sync::atomic::Ordering::Relaxed),
            SYNC_HANDOFF_ARM_REJECTED[site].swap(0, core::sync::atomic::Ordering::Relaxed),
        )
    })
}

/// Which condition inside `ReplyWake` custody's ordered check first failed.
/// `SyncHandoffCustody::Generic` never reaches this — only reply-derived
/// records carry a real custody check, which `SyncHandoffStaleReason::Custody`
/// is therefore entirely attributable to.
///
/// `matches_dispatch_owner` is called from more than the consume path (also
/// from the arm-time token self-check and the post-enqueue recheck in
/// `enqueue_reply_wake_after_catalog`), so this counter is not purely
/// consume-side. In practice the arm-side calls contribute ~0: the paired
/// `SyncHandoffArmSite::Reply` accept/reject counters (above) show the arm
/// accepting essentially every time, so a near-zero arm-side failure rate is
/// independently confirmed rather than assumed.
pub(in crate::multitask) const SYNC_HANDOFF_REPLY_CUSTODY_FAIL_REASON_COUNT: usize = 4;

#[derive(Clone, Copy)]
pub(in crate::multitask) enum SyncHandoffReplyCustodyFailReason {
    /// The runqueue owner generation moved since the reply token captured it.
    Generation = 0,
    /// The owner's current CPU no longer matches the token's target CPU.
    Cpu = 1,
    /// The owner is not currently runnable.
    NotRunnable = 2,
    /// The owner's state left `Local`/`RemoteQueued`.
    State = 3,
}

static SYNC_HANDOFF_REPLY_CUSTODY_FAIL: [core::sync::atomic::AtomicU64;
    SYNC_HANDOFF_REPLY_CUSTODY_FAIL_REASON_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; SYNC_HANDOFF_REPLY_CUSTODY_FAIL_REASON_COUNT];

pub(in crate::multitask) fn record_sync_handoff_reply_custody_fail(
    reason: SyncHandoffReplyCustodyFailReason,
) {
    // ORDERING: Relaxed; diagnostic counter drained once per second.
    SYNC_HANDOFF_REPLY_CUSTODY_FAIL[reason as usize]
        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Takes the window's reply-custody-failure breakdown, clearing it for the
/// next window. Indexed by `SyncHandoffReplyCustodyFailReason`.
pub(in crate::multitask) fn take_sync_handoff_reply_custody_fail_window()
-> [u64; SYNC_HANDOFF_REPLY_CUSTODY_FAIL_REASON_COUNT] {
    core::array::from_fn(|reason| {
        SYNC_HANDOFF_REPLY_CUSTODY_FAIL[reason].swap(0, core::sync::atomic::Ordering::Relaxed)
    })
}

/// One-shot diagnostic for the generation-mismatch mechanism: what state the
/// owner snapshot was actually found in at the moment a `ReplyWake` token's
/// generation no longer matched. `Running` means the caller was already
/// dispatched through some other entry point before this FIFO record was
/// ever reached. Indices mirror `runqueue::RunOwnerState`'s discriminants
/// (0=Dormant, 1=Local, 2=RemoteQueued, 3=Running, 4=Migrating, 5=Blocked,
/// 6=Retiring, 7=Retired).
pub(in crate::multitask) const SYNC_HANDOFF_GENERATION_FAIL_STATE_COUNT: usize = 8;

static SYNC_HANDOFF_GENERATION_FAIL_STATE: [core::sync::atomic::AtomicU64;
    SYNC_HANDOFF_GENERATION_FAIL_STATE_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; SYNC_HANDOFF_GENERATION_FAIL_STATE_COUNT];

pub(in crate::multitask) fn record_sync_handoff_generation_fail_state(state_index: usize) {
    if let Some(counter) = SYNC_HANDOFF_GENERATION_FAIL_STATE.get(state_index) {
        // ORDERING: Relaxed; diagnostic counter drained once per second.
        counter.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Takes the window's generation-fail-state breakdown, clearing it for the
/// next window.
pub(in crate::multitask) fn take_sync_handoff_generation_fail_state_window()
-> [u64; SYNC_HANDOFF_GENERATION_FAIL_STATE_COUNT] {
    core::array::from_fn(|state| {
        SYNC_HANDOFF_GENERATION_FAIL_STATE[state].swap(0, core::sync::atomic::Ordering::Relaxed)
    })
}
