//! Generation-bound authority for ordinary per-CPU fair dispatch.
//!
//! The picker reads durable runqueue, weight, donation, budget, frame, and
//! lifecycle publications. It becomes authoritative only after a coherent
//! full-candidate revalidation. Exact activation and IPC FIFO custody have
//! separate generation-bound claims. Incomplete fair observations retry with
//! fresh CPU-local epochs; they cannot authorize a partial candidate set.

#[cfg(test)]
mod current_frame;
mod fair_retry;
#[cfg(test)]
pub(super) mod policy_witnesses;
mod hint_priority;
pub(super) use fair_retry::{
    UnresolvedCandidates, preallocate_pool as preallocate_retry_pool, retry_incomplete_policy,
    take_omitted_candidate_window,
};
pub(super) use hint_priority::{
    refuse_unproven_hint_priority, stable_user_hint_class, user_hint_blocked_by_system,
};

#[cfg(rustos_scheduler_phase_profile)]
use core::sync::atomic::{AtomicU64, Ordering};

use super::{MAX_CONSECUTIVE_SYSTEM_DISPATCHES, dispatch_policy};
use super::{
    SCHED_CPU_LOCALITY_LAG_NS, SCHED_MAX_BURST_NS, SCHED_MIN_GRANULARITY_NS,
    SYSTEM_CLASS_WEIGHT_FLAG, context_validation, current_identity, donation_ledger,
    frame_publication, runqueue, scheduling_runtime,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LocalFallbackReason {
    AtomicActivationPending = 0,
    SynchronousHandoffPending = 1,
    NoLocalCandidate = 2,
    PublicationUnavailable = 3,
    LifecycleIneligible = 4,
    CustodyUnavailable = 5,
    FrameUnavailable = 6,
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum LocalClass {
    System,
    User,
}

#[cfg(rustos_scheduler_phase_profile)]
static SHADOW_DISPATCHES: AtomicU64 = AtomicU64::new(0);
#[cfg(rustos_scheduler_phase_profile)]
static SHADOW_FALLBACKS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

fn fallback<T>(reason: LocalFallbackReason) -> Result<T, LocalFallbackReason> {
    #[cfg(rustos_scheduler_phase_profile)]
    SHADOW_FALLBACKS[reason as usize].fetch_add(1, Ordering::Relaxed);
    Err(reason)
}

pub(super) fn user_reservation_due(cpu: usize) -> bool {
    dispatch_policy::system_dispatch_streak(cpu) >= MAX_CONSECUTIVE_SYSTEM_DISPATCHES
}

pub(super) fn observed_class(slot: usize) -> Result<LocalClass, LocalFallbackReason> {
    let weight = runqueue::weight::observe(slot).ok_or(LocalFallbackReason::PublicationUnavailable)?;
    Ok(if weight & SYSTEM_CLASS_WEIGHT_FLAG != 0
        || donation_ledger::inherited_system(slot)
    {
        LocalClass::System
    } else {
        LocalClass::User
    })
}
fn class(slot: usize) -> LocalClass {
    observed_class(slot).expect("qualified live candidate lost class publication")
}

pub(super) fn is_user_class(slot: usize) -> bool {
    class(slot) == LocalClass::User
}

// A borrowed worker's own identity stamp is NOT its effective budget. Under
// Scheduler, donation bind/release and slot retirement are serialized with
// this read. Resolve the exact published charge token first, then the live
// owner's immutable identity stamp; a missing owner publication fails closed.
// The second token read rejects a changed owner/reply while the budget cells
// were checked (the Context -> Domain lock order is unchanged).
pub(super) fn effective_charge_stamp(
    own: Option<current_identity::PagerChargeStamp>,
    borrowed: Option<Option<current_identity::DispatchPublication>>,
) -> Option<Option<current_identity::PagerChargeStamp>> {
    match borrowed {
        None => Some(own),
        Some(Some(owner)) if owner.task_id.is_some() => Some(owner.pager_charge),
        Some(_) => None,
    }
}

/// Complete exact-budget denial is different from an unknown donation or
/// runtime-cell observation. The reply token is read before and after the
/// Context -> Domain locks; a changed token never turns a stale denial into
/// permission to skip this candidate.
fn effective_charge_admission(
    slot: usize,
    own: Option<current_identity::PagerChargeStamp>,
    cpu: usize,
    now_ns: u64,
) -> Option<bool> {
    let token = donation_ledger::borrowed_context_charge_token(slot);
    let borrowed = token.map(|(owner_slot, _)| current_identity::read_dispatch(owner_slot));
    let stamp = effective_charge_stamp(own, borrowed)?;
    let owner_slot = token.map_or(slot, |(owner_slot, _)| owner_slot);
    let admitted = match stamp {
        None => Some(true),
        Some(stamp) => {
            let exact_owner = u64::try_from(owner_slot)
                .ok()
                .and_then(|owner_slot| owner_slot.checked_add(1))
                == Some(stamp.context_slot);
            if !exact_owner {
                return None;
            }
            scheduling_runtime::published_stamp_admission(stamp, cpu, now_ns)
        }
    };
    rustos_scheduler_frame_gate::budget_admission_after_token_check(
        admitted,
        donation_ledger::borrowed_context_charge_token(slot) == token,
    )
}

pub(super) fn effective_charge_is_eligible(
    slot: usize,
    own: Option<current_identity::PagerChargeStamp>,
    cpu: usize,
    now_ns: u64,
) -> bool {
    effective_charge_admission(slot, own, cpu, now_ns) == Some(true)
}

#[inline]
fn affinity_masks_allow_cpu(task_mask: u64, process_mask: u64, cpu: usize) -> bool {
    u32::try_from(cpu)
        .ok()
        .and_then(|index| 1_u64.checked_shl(index))
        .is_some_and(|bit| task_mask & process_mask & bit != 0)
}

/// A zero-mask snapshot is an overlapping/reset publication, not stable
/// exclusion. Migration intent survives until the exact placement/RSP ACK:
/// it vetoes retained Current, never a fresh claim on an included destination.
fn published_affinity_admission(slot: usize, cpu: usize, retain_current: bool) -> Option<bool> {
    let (task_mask, process_mask, migration_pending) =
        runqueue::affinity_payload::affinity_snapshot(slot);
    if task_mask == 0 || process_mask == 0 || (retain_current && migration_pending) {
        return None;
    }
    Some(
        (slot != super::ROOT_TASK_SLOT || cpu == 0)
            && affinity_masks_allow_cpu(task_mask, process_mask, cpu),
    )
}

fn current_can_keep_running(slot: usize, cpu: usize, now_ns: u64) -> bool {
    if runqueue::is_idle_slot(slot) {
        return false;
    }
    let Some(publication) = current_identity::read_dispatch(slot) else {
        return false;
    };
    let owner = runqueue::owner(slot);
    publication.lifecycle_dispatchable()
        && published_affinity_admission(slot, cpu, true) == Some(true)
        && effective_charge_is_eligible(slot, publication.pager_charge, cpu, now_ns)
        && owner.cpu == Some(cpu)
        && runqueue::owner_has_run_intent(owner)
}

/// Stable published lifecycle/affinity and the exact effective runtime pair
/// decide admission for either Local fair or bound Direct FIFO custody. An
/// incomplete observation never becomes a positive or stable negative answer.
pub(super) fn published_candidate_admission(
    slot: usize,
    cpu: usize,
    now_ns: u64,
) -> Result<bool, LocalFallbackReason> {
    let Some(publication) = current_identity::read_dispatch(slot) else {
        return Err(LocalFallbackReason::PublicationUnavailable);
    };
    if publication.task_id.is_none() {
        return Err(LocalFallbackReason::PublicationUnavailable);
    }
    let affinity_admission = published_affinity_admission(slot, cpu, false);
    if rustos_scheduler_frame_gate::stable_local_admission_rejected(
        publication.lifecycle_dispatchable(),
        affinity_admission,
    ) {
        return Ok(false);
    }
    if affinity_admission.is_none() {
        return Err(LocalFallbackReason::CustodyUnavailable);
    }
    match effective_charge_admission(slot, publication.pager_charge, cpu, now_ns) {
        Some(true) => Ok(true),
        Some(false) => Ok(false),
        None => Err(LocalFallbackReason::LifecycleIneligible),
    }
}

fn candidate_is_qualified(
    slot: usize,
    cpu: usize,
    now_ns: u64,
) -> Result<Option<u64>, LocalFallbackReason> {
    if !published_candidate_admission(slot, cpu, now_ns)? {
        return Ok(None);
    }
    // The frame token proves the stronger fair-queue custody condition
    // (`Local`, same CPU, runnable, non-executing, exact generation), so a
    // preceding generic dispatchability read would only sample OWNER twice.
    let Some(frame) = frame_publication::local_frame_token(slot, cpu) else {
        return Err(LocalFallbackReason::FrameUnavailable);
    };
    // The token is only an observation. Retain exact allocation lifetime AND
    // reader/claim exclusion through the unsafe copy and final token check in
    // native witnesses just as in production; generations cannot replace pins.
    // SAFETY: selection runs under the original CPU's dispatch exclusion.
    let _frame_reader = unsafe { frame_publication::read_allocation(frame, cpu) }
        .ok_or(LocalFallbackReason::FrameUnavailable)?;
    context_validation::validate_published_saved_context(frame.slot, frame.saved_rsp, frame.view)
        .map_err(|_| LocalFallbackReason::FrameUnavailable)?;
    frame_publication::token_is_current(frame, cpu)
        .then_some(Some(frame.owner_generation))
        .ok_or(LocalFallbackReason::FrameUnavailable)
}

/// Ordering admission for one fair-scan candidate: the same published
/// lifecycle/affinity/budget answer and Local frame token as
/// `candidate_is_qualified`, without the allocation pin, frame reader and saved
/// context copy. Only the scan's winner can be dispatched, and it is re-proven
/// through `candidate_is_qualified` before the scan returns it; a corrupt frame
/// behind a non-winner is the periodic ready-frame sweep's to quarantine.
/// Qualifying every runnable candidate cost three resource-leaf acquisitions
/// per candidate on every fair dispatch, most of a round trip's leaf traffic.
fn candidate_is_admitted(
    slot: usize,
    cpu: usize,
    now_ns: u64,
) -> Result<Option<u64>, LocalFallbackReason> {
    match published_candidate_admission(slot, cpu, now_ns)? {
        false => Ok(None),
        true => frame_publication::local_frame_token(slot, cpu)
            .map(|frame| Some(frame.owner_generation))
            .ok_or(LocalFallbackReason::FrameUnavailable),
    }
}

/// A hinted Local owner may bypass the catalog's second schedulability
/// check only with a complete published lifecycle, affinity, budget and frame
/// observation. Other owner states and incomplete reads retain the catalog
/// path; an idle owner on a foreign CPU is a stable rejection.
#[cfg(test)]
pub(super) fn qualified_local_handoff_generation(
    slot: usize,
    cpu: usize,
    now_ns: u64,
) -> Result<Option<u64>, LocalFallbackReason> {
    if runqueue::is_idle_slot(slot) && super::smp::idle_slot_for_cpu(cpu) != Some(slot) {
        return Ok(None);
    }
    candidate_is_qualified(slot, cpu, now_ns)
}



/// The direct IPC streak must yield to a genuinely Local fair competitor.
/// Bitmap entries can outlive a concurrent owner withdrawal, so recheck
/// runnable Local custody on this CPU and exclude published idle slots.
pub(super) fn local_fair_competitor_ready(cpu: usize) -> bool {
    if runqueue::published_runnable_count(cpu) == 0 {
        return false;
    }
    runqueue::local_runnable_slots(cpu)
        .any(|slot| !runqueue::is_idle_slot(slot) && runqueue::is_local_dispatchable(slot, cpu))
}


/// Validates the saved frame behind one exact FIFO handoff. The caller
/// separately checks record identity and lifecycle/budget/affinity admission.
#[cfg(test)]
pub(super) fn direct_handoff_frame_generation(
    slot: usize,
    cpu: usize,
) -> Result<u64, LocalFallbackReason> {
    let frame = frame_publication::direct_handoff_frame_token(slot, cpu)
        .ok_or(LocalFallbackReason::FrameUnavailable)?;
    // SAFETY: selection runs under the original CPU's dispatch exclusion.
    let _frame_reader = unsafe { frame_publication::read_allocation(frame, cpu) }
        .ok_or(LocalFallbackReason::FrameUnavailable)?;
    context_validation::validate_published_saved_context(frame.slot, frame.saved_rsp, frame.view)
        .map_err(|_| LocalFallbackReason::FrameUnavailable)?;
    frame_publication::direct_handoff_token_is_current(frame, cpu)
        .then_some(frame.owner_generation)
        .ok_or(LocalFallbackReason::FrameUnavailable)
}


const fn ordinary_voluntary_pick(voluntary_yield: bool, reservation_applied: bool) -> bool {
    voluntary_yield && !reservation_applied
}

fn prefer(
    current: Option<(usize, u64, usize)>,
    candidate: (usize, u64, usize),
) -> Option<(usize, u64, usize)> {
    match current {
        Some((_, best_vruntime, best_distance))
            if candidate.1 > best_vruntime
                || (candidate.1 == best_vruntime && candidate.2 >= best_distance) =>
        {
            current
        }
        _ => Some(candidate),
    }
}

type Candidate = (usize, u64, usize);

fn prefer_local(best: Option<Candidate>, local: Option<Candidate>) -> Option<Candidate> {
    match (best, local) {
        (Some(best), Some(local))
            if local.1 <= best.1.saturating_add(SCHED_CPU_LOCALITY_LAG_NS) =>
        {
            Some(local)
        }
        (Some(best), _) => Some(best),
        (None, local) => local,
    }
}

fn select_class_candidate(
    reserve_user: bool,
    reservation: Option<Candidate>,
    system: Option<Candidate>,
    user: Option<Candidate>,
    yielded_current: Option<Candidate>,
) -> (Option<Candidate>, bool) {
    let reservation_applied = reserve_user && reservation.is_some();
    let picked = if reservation_applied {
        reservation
    } else {
        system.or(user).or(yielded_current)
    };
    (picked, reservation_applied)
}

/// Reconstructs the ordinary local fair decision from published state.
///
/// Atomic activation and direct synchronous handoff carry exact FIFO custody
/// that is intentionally not in the fair bitmap. Those paths fail closed to
/// the catalog; normal System/User fair ordering, the user reservation, and
/// voluntary-yield exclusion are shadowed here.
#[cfg(test)]
pub(super) fn choose_local_next(
    cpu: usize, current_slot: usize, voluntary_yield: bool, current_runtime_ns: u64,
    eligibility_now_ns: u64, expected_queue_sequence: u64, expected_class_sequence: u64,
    atomic_activation_pending: bool, synchronous_handoff_pending: bool,
) -> Result<(usize, u64), LocalFallbackReason> {
    choose_local_next_with(cpu, current_slot, voluntary_yield, current_runtime_ns,
        eligibility_now_ns, expected_queue_sequence, expected_class_sequence,
        atomic_activation_pending, synchronous_handoff_pending, &mut UnresolvedCandidates::strict(),
        candidate_is_qualified)
}

#[cfg(rustos_scheduler_phase_profile)]
struct ScanTimer {
    started_ns: u64,
    handoff: bool,
}

#[cfg(rustos_scheduler_phase_profile)]
impl ScanTimer {
    #[inline]
    fn start(handoff: bool) -> Self {
        Self {
            started_ns: crate::arch::clock::monotonic_nanos(),
            handoff,
        }
    }
}

#[cfg(rustos_scheduler_phase_profile)]
impl Drop for ScanTimer {
    #[inline]
    fn drop(&mut self) {
        let elapsed = crate::arch::clock::monotonic_nanos().saturating_sub(self.started_ns);
        if self.handoff {
            super::locality::record_handoff_scan(elapsed);
        } else {
            super::locality::record_pick_scan(elapsed);
        }
    }
}

/// The production consumer retains its winner's actual frame loan through
/// final claim. Observational callers use the SAME ordering/admission engine
/// above but discard their scoped reader, never manufacturing execution.
pub(super) fn choose_local_next_with(
    cpu: usize, current_slot: usize, voluntary_yield: bool, current_runtime_ns: u64,
    eligibility_now_ns: u64, expected_queue_sequence: u64, expected_class_sequence: u64,
    atomic_activation_pending: bool, synchronous_handoff_pending: bool,
    unresolved: &mut UnresolvedCandidates,
    mut qualify: impl FnMut(usize, usize, u64) -> Result<Option<u64>, LocalFallbackReason>,
) -> Result<(usize, u64), LocalFallbackReason> {
    #[cfg(rustos_scheduler_phase_profile)]
    let _scan_timer = ScanTimer::start(false);
    if atomic_activation_pending {
        return fallback(LocalFallbackReason::AtomicActivationPending);
    }
    if synchronous_handoff_pending {
        return fallback(LocalFallbackReason::SynchronousHandoffPending);
    }
    if expected_queue_sequence & 1 != 0
        || expected_class_sequence & 1 != 0
        || runqueue::local_runnable_sequence(cpu) != expected_queue_sequence
        || donation_ledger::class_publication_sequence() != expected_class_sequence
    {
        return fallback(LocalFallbackReason::CustodyUnavailable);
    }
    if runqueue::published_runnable_count(cpu) == 0 {
        return fallback(LocalFallbackReason::NoLocalCandidate);
    }

    let reserve_user = user_reservation_due(cpu);
    // The outgoing execution owner cannot change during this scheduler turn.
    // Snapshot its publication once so candidate admission and the later
    // min-granularity decision cannot disagree because they sampled different
    // points of an otherwise valid current-frame transition.
    //
    // The snapshot is memoized and taken at first use: a voluntary yield that
    // finds another candidate never needs it, and that evaluation is the
    // scan's Context+Domain budget pair for the outgoing task.
    let mut current_live: Option<bool> = None;
    macro_rules! current_is_live {
        () => {
            *current_live.get_or_insert_with(|| {
                current_can_keep_running(current_slot, cpu, eligibility_now_ns)
            })
        };
    }
    let mut yielded_current_deferred = false;
    let mut best_system = None;
    let mut best_user = None;
    let mut local_system = None;
    let mut local_user = None;
    // Reservation is an earlier legacy-policy arm with different current/yield
    // semantics. Keep its User-only ordering separate from ordinary CFS so a
    // reservation that is merely due, but has no User candidate, cannot alter
    // the subsequent voluntary-yield decision.
    let mut reserved_user = None;
    let mut local_reserved_user = None;
    let mut alternate_system = None;
    let mut alternate_user = None;
    let mut local_alternate_system = None;
    let mut local_alternate_user = None;
    let mut current_candidate = None;
    let mut first_revalidation_failure = None;
    #[cfg(rustos_scheduler_phase_profile)]
    let mut sampled: u64 = 0;
    for slot in runqueue::local_runnable_slots(cpu) {
        if runqueue::is_idle_slot(slot) || unresolved.excludes(slot) {
            continue;
        }
        #[cfg(rustos_scheduler_phase_profile)]
        {
            sampled = sampled.saturating_add(1);
        }
        // The outgoing slot is still the CPU's live execution frame and must
        // not manufacture a stable saved-frame token. Its current-CPU owner
        // and lifecycle publication are the coherent evidence for retaining
        // it; every replacement candidate requires the generation-bound frame
        // token below.
        // A yielding current is only the fallback when nothing else is
        // eligible, so it is qualified after the scan, and only then. A due
        // User reservation can pick it outright and keeps the eager order.
        if voluntary_yield && slot == current_slot && !reserve_user {
            yielded_current_deferred = true;
            continue;
        }
        // Check published class first (two atomic loads) before expensive
        // lifecycle/affinity/budget/frame admission. Once a higher-priority
        // class already has an admitted winner for this epoch, lower-priority
        // class slots cannot win and skip admission entirely.
        let candidate_class = match observed_class(slot) {
            Ok(class) => class,
            Err(reason) => {
                unresolved.record(slot, reason);
                first_revalidation_failure.get_or_insert(reason);
                continue;
            }
        };
        if !reserve_user && best_system.is_some() && candidate_class == LocalClass::User {
            continue;
        }
        if reserve_user && reserved_user.is_some() && candidate_class == LocalClass::System {
            continue;
        }
        let qualification = if slot == current_slot && current_is_live!() {
            Ok(Some(runqueue::owner(slot).generation))
        } else {
            candidate_is_admitted(slot, cpu, eligibility_now_ns)
        };
        match qualification {
            Ok(Some(_)) => {}
            // Only a complete, stable negative lifecycle/affinity or exact
            // budget denial can be omitted. Missing identity, transitioning
            // affinity, stale donor/budget or frame evidence still invalidates
            // the entire scan, unless it persisted through every earlier
            // observation of this turn (`UnresolvedCandidates`).
            Ok(None) => continue,
            Err(reason) => {
                unresolved.record(slot, reason);
                first_revalidation_failure.get_or_insert(reason);
                continue;
            }
        }
        let vruntime = runqueue::vruntime(slot);
        // Match legacy selection's same-slot turn bias. Voluntary yields omit
        // the current slot above and use circular-distance tie breaking.
        let key = if slot == current_slot {
            vruntime.saturating_add(1)
        } else {
            vruntime
        };
        let is_local =
            slot == current_slot || usize::from(runqueue::affinity_payload::last_cpu(slot)) == cpu;
        if reserve_user && candidate_class == LocalClass::User {
            let reservation_candidate = (slot, key, slot);
            reserved_user = prefer(reserved_user, reservation_candidate);
            if is_local {
                local_reserved_user = prefer(local_reserved_user, reservation_candidate);
            }
        }
        // If reservation finds no User candidate, legacy selection proceeds
        // through ordinary CFS. Its voluntary arm excludes current and uses
        // circular distance regardless of whether reservation was due.
        let distance = if voluntary_yield {
            (slot + crate::multitask::task_table::MAX_TASK_SLOTS - current_slot)
                % crate::multitask::task_table::MAX_TASK_SLOTS
        } else {
            slot
        };
        if voluntary_yield && slot == current_slot {
            current_candidate = Some((slot, key, distance));
            continue;
        }
        let candidate = (slot, key, distance);
        match candidate_class {
            LocalClass::System => {
                best_system = prefer(best_system, candidate);
                if slot != current_slot {
                    alternate_system = prefer(alternate_system, candidate);
                }
                if is_local {
                    local_system = prefer(local_system, candidate);
                    if slot != current_slot {
                        local_alternate_system = prefer(local_alternate_system, candidate);
                    }
                }
            }
            LocalClass::User => {
                best_user = prefer(best_user, candidate);
                if slot != current_slot {
                    alternate_user = prefer(alternate_user, candidate);
                }
                if is_local {
                    local_user = prefer(local_user, candidate);
                    if slot != current_slot {
                        local_alternate_user = prefer(local_alternate_user, candidate);
                    }
                }
            }
        }
    }

    #[cfg(rustos_scheduler_phase_profile)]
    if sampled != 0 {
        super::runtime_profile::owner::local(|row| {
            row.runtime_profile_runnable_samples =
                row.runtime_profile_runnable_samples.saturating_add(sampled);
        });
    }

    if yielded_current_deferred && best_system.is_none() && best_user.is_none()
        && !unresolved.excludes(current_slot)
    {
        let slot = current_slot;
        let qualification = if current_is_live!() {
            Ok(Some(runqueue::owner(slot).generation))
        } else {
            candidate_is_admitted(slot, cpu, eligibility_now_ns)
        };
        match qualification.and_then(|q| observed_class(slot).map(|_| q)) {
            Ok(Some(_)) => {
                current_candidate = Some((slot, runqueue::vruntime(slot).saturating_add(1), 0));
            }
            Ok(None) => {}
            Err(reason) => {
                unresolved.record(slot, reason);
                first_revalidation_failure.get_or_insert(reason);
            }
        }
    }

    // A partial candidate set is not authoritative: the unavailable slot may
    // be the class or vruntime winner. Fall back even when another qualified
    // candidate exists rather than silently selecting around missing state.
    if let Some(reason) = first_revalidation_failure {
        return fallback(reason);
    }
    if runqueue::local_runnable_sequence(cpu) != expected_queue_sequence
        || donation_ledger::class_publication_sequence() != expected_class_sequence
    {
        return fallback(LocalFallbackReason::CustodyUnavailable);
    }

    let system = prefer_local(best_system, local_system);
    let user = prefer_local(best_user, local_user);
    let reservation = prefer_local(reserved_user, local_reserved_user);
    let (picked, reservation_applied) =
        select_class_candidate(reserve_user, reservation, system, user, current_candidate);
    if let Some((mut slot, _, _)) = picked {
        if !ordinary_voluntary_pick(voluntary_yield, reservation_applied)
            && !reservation_applied
            && slot == current_slot
            && current_runtime_ns >= SCHED_MAX_BURST_NS
        {
            let Ok(current_class) = observed_class(current_slot) else {
                return fallback(LocalFallbackReason::PublicationUnavailable);
            };
            let alternate = match current_class {
                LocalClass::System => prefer_local(alternate_system, local_alternate_system),
                LocalClass::User => prefer_local(alternate_user, local_alternate_user),
            };
            if let Some((alternate_slot, _, _)) = alternate {
                slot = alternate_slot;
            }
        }
        // A class publication withdrawn by a racing retirement between the
        // scan and here is an incomplete observation, never a panic.
        let granularity_applies = if !voluntary_yield
            && !reservation_applied
            && slot != current_slot
            && current_is_live!()
        {
            match (observed_class(slot), observed_class(current_slot)) {
                // Legacy fairness preempts across a higher-priority class, but
                // applies min-granularity to same/lower-priority peers. The current
                // frame is actively executing, so retaining it needs no stable
                // saved-frame token; every replacement still passed one above.
                (Ok(candidate), Ok(current)) => candidate >= current,
                _ => return fallback(LocalFallbackReason::PublicationUnavailable),
            }
        } else {
            false
        };
        if granularity_applies {
            let advantage =
                runqueue::vruntime(current_slot).saturating_sub(runqueue::vruntime(slot));
            if current_runtime_ns < SCHED_MIN_GRANULARITY_NS
                && advantage < SCHED_MIN_GRANULARITY_NS
                && current_runtime_ns < SCHED_MAX_BURST_NS
            {
                slot = current_slot;
            }
        }
        // The chosen slot must still name an eligible frame and its exact
        // generation after ordering and min-granularity. The earlier scan's
        // token cannot authorize a later, possibly reused Local owner.
        let generation = if slot == current_slot && current_is_live!() {
            if !current_can_keep_running(slot, cpu, eligibility_now_ns) {
                return fallback(LocalFallbackReason::LifecycleIneligible);
            }
            runqueue::owner(slot).generation
        } else {
            match qualify(slot, cpu, eligibility_now_ns) {
                Ok(Some(generation)) => generation,
                Ok(None) => return fallback(LocalFallbackReason::LifecycleIneligible),
                Err(reason) => {
                    // A winner whose frame keeps failing its final proof would
                    // otherwise be re-selected, and fail, on every turn.
                    unresolved.record(slot, reason);
                    return fallback(reason);
                }
            }
        };
        if runqueue::local_runnable_sequence(cpu) != expected_queue_sequence
            || donation_ledger::class_publication_sequence() != expected_class_sequence
        {
            return fallback(LocalFallbackReason::CustodyUnavailable);
        }
        #[cfg(rustos_scheduler_phase_profile)]
        SHADOW_DISPATCHES.fetch_add(1, Ordering::Relaxed);
        Ok((slot, generation))
    } else {
        fallback(LocalFallbackReason::NoLocalCandidate)
    }
}

fn older_overdue_system(
    oldest: Option<(usize, u64, u64, u64)>,
    candidate: (usize, u64, u64, u64),
) -> Option<(usize, u64, u64, u64)> {
    match oldest {
        Some((_, since, vruntime, _))
            if candidate.1 > since || (candidate.1 == since && candidate.2 >= vruntime) =>
        {
            oldest
        }
        _ => Some(candidate),
    }
}

/// Reconstructs the strict-class ready-age gate from the same CPU-local
/// runnable/class/age publications as the ordinary fair scan. A failed
/// eligible System candidate must not be silently skipped: it may be the
/// oldest one, so retain the catalog path when the observation is partial.
pub(super) fn choose_overdue_system(
    cpu: usize,
    current_slot: usize,
    now_ticks: u64,
    now_ns: u64,
    expected_queue_sequence: u64,
    expected_class_sequence: u64,
    unresolved: &mut UnresolvedCandidates,
) -> Result<Option<(usize, u64)>, LocalFallbackReason> {
    if expected_queue_sequence & 1 != 0
        || expected_class_sequence & 1 != 0
        || runqueue::local_runnable_sequence(cpu) != expected_queue_sequence
        || donation_ledger::class_publication_sequence() != expected_class_sequence
    {
        return Err(LocalFallbackReason::CustodyUnavailable);
    }
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    if donation_ledger::live_inherited_system_count() == 0
        && runqueue::overdue_system_short_circuit(
            cpu,
            now_ticks,
            ticks_per_second,
            super::SYSTEM_READY_LATENCY_BOUND_MS,
            expected_queue_sequence,
        )
    {
        return Ok(None);
    }
    #[cfg(rustos_scheduler_phase_profile)]
    let _scan_timer = ScanTimer::start(true);
    let mut oldest = None;
    let mut first_failure = None;
    for slot in runqueue::local_runnable_slots(cpu) {
        if slot == current_slot || runqueue::is_idle_slot(slot) || unresolved.excludes(slot) { continue; }
        let class = match observed_class(slot) {
            Ok(class) => class,
            Err(reason) => { unresolved.record(slot, reason); first_failure.get_or_insert(reason); continue; }
        };
        if class != LocalClass::System { continue; }
        let since = runqueue::wait::ready_since_ticks(slot);
        let elapsed_ms = now_ticks
            .saturating_sub(since)
            .saturating_mul(1000)
            .saturating_div(ticks_per_second);
        if since == 0 || elapsed_ms < super::SYSTEM_READY_LATENCY_BOUND_MS {
            continue;
        }
        let generation = match candidate_is_qualified(slot, cpu, now_ns) {
            Ok(Some(generation)) => generation,
            Ok(None) => continue,
            Err(reason) => { unresolved.record(slot, reason); first_failure.get_or_insert(reason); continue; }
        };
        oldest = older_overdue_system(oldest, (slot, since, runqueue::vruntime(slot), generation));
    }
    // A failed eligible System candidate may be the oldest one: the set is
    // authoritative only without it (or once it is persistently omitted).
    if let Some(reason) = first_failure {
        return Err(reason);
    }
    if runqueue::local_runnable_sequence(cpu) != expected_queue_sequence
        || donation_ledger::class_publication_sequence() != expected_class_sequence
    {
        return Err(LocalFallbackReason::CustodyUnavailable);
    }
    Ok(oldest.map(|(slot, _, _, generation)| (slot, generation)))
}

/// Drains the local-dispatch qualification and fallback counters for runtime evidence.
#[cfg(rustos_scheduler_phase_profile)]
pub(super) fn drain_shadow_observations() -> (u64, [u64; 7]) {
    (
        SHADOW_DISPATCHES.swap(0, Ordering::Relaxed),
        core::array::from_fn(|index| SHADOW_FALLBACKS[index].swap(0, Ordering::Relaxed)),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        affinity_masks_allow_cpu, ordinary_voluntary_pick, prefer, prefer_local,
        select_class_candidate,
    };

    #[test]
    fn final_local_frame_rechecks_exact_generation_rsp_and_validated_payload() {
        use super::super::dispatch_claim::{DispatchFrame, FrameCapture, FrameCustody, tests::fixture};
        fixture(false, |slot, turn| {
            let id = super::super::current_identity::read_dispatch(slot).unwrap().task_id.unwrap();
            let generation = super::runqueue::owner(slot).generation;
            let rsp = super::runqueue::saved_rsp(slot);
            let capture = |g, r, custody| DispatchFrame::capture(turn, slot, id, g, r, custody);
            assert!(matches!(capture(generation, rsp, FrameCustody::Local), FrameCapture::Ready(_)));
            assert!(matches!(capture(generation + 1, rsp, FrameCustody::Local), FrameCapture::Stale));
            assert!(matches!(capture(generation, rsp + 8, FrameCustody::Local), FrameCapture::Stale));
            // Explicit earlier native frame-producer input in an isolated born
            // allocation, before any reader/claim. This is not a product writer.
            let flags = unsafe { (*(rsp as *const super::super::SavedContext)).rflags };
            unsafe { (*(rsp as *mut super::super::SavedContext)).rflags = 0; }
            assert!(matches!(capture(generation, rsp, FrameCustody::Local),
                FrameCapture::Invalid("saved rflags lost the reserved bit")));
            unsafe { (*(rsp as *mut super::super::SavedContext)).rflags = flags; }
            let FrameCapture::Ready(frame) = capture(generation, rsp, FrameCustody::Local) else { panic!("original frame lost"); };
            assert!(frame.prepare().unwrap().claim());
            assert!(matches!(capture(generation, rsp, FrameCustody::Local), FrameCapture::Stale));
            super::super::dispatch_claim::tests::finish_modeled_preparation(slot, 121);
        });
        fixture(false, |slot, turn| {
            let id = super::super::current_identity::read_dispatch(slot).unwrap().task_id.unwrap();
            let rsp = super::runqueue::saved_rsp(slot);
            super::runqueue::publish_blocked(slot, 0, super::runqueue::weight::value(slot));
            assert!(matches!(super::runqueue::publish_direct_handoff(slot, 0),
                super::runqueue::RemoteWakeOutcome::Published { .. }));
            let generation = super::runqueue::owner(slot).generation;
            let capture = |r, custody| DispatchFrame::capture(turn, slot, id, generation, r, custody);
            assert!(matches!(capture(rsp, FrameCustody::Direct), FrameCapture::Ready(_)));
            assert!(matches!(capture(rsp + 8, FrameCustody::Direct), FrameCapture::Stale));
            assert!(matches!(capture(rsp, FrameCustody::Local), FrameCapture::Stale));
        });
    }

    #[test]
    fn actual_selection_refuses_published_frame_without_original_allocation() {
        use super::super::current_identity::{self, TaskIdentity};
        use std::boxed::Box;
        let _cpu = crate::multitask::cpu_local::test_publication_lock();
        let _runtime = super::super::scheduling_runtime::test_serial_guard();
        let _queue = super::runqueue::test_serial_guard();
        super::frame_publication::reset_before_publication();
        super::runqueue::reset_before_publication();
        let (slot, cpu) = (47, 0);
        let mut saved = Box::new(unsafe { core::mem::zeroed::<super::super::SavedContext>() });
        saved.rflags = 2;
        saved.cs = u64::from(crate::arch::gdt::kernel_code_selector().0);
        saved.rip = 0xffff_8000_0000_0000;
        saved.rsp = 1;
        let rsp = (&mut *saved) as *mut super::super::SavedContext as usize;
        let top = rsp as u64 + core::mem::size_of::<super::super::SavedContext>() as u64 + 16;
        super::frame_publication::publish(slot, rsp, rsp as u64, top, 0, 0, false, false, true, 0, 0);
        current_identity::publish_test_identity(slot, TaskIdentity { task_id: Some(82999), ..TaskIdentity::default() });
        super::runqueue::affinity_payload::set_affinity(slot, 1, 1, false);
        super::runqueue::admit_blocked(slot);
        super::runqueue::publish_local(slot, cpu, 100);
        assert_eq!(super::published_candidate_admission(slot, cpu, 0), Ok(true));
        let token = super::frame_publication::local_frame_token(slot, cpu).unwrap();
        assert!(super::frame_publication::pin_allocation(token).is_none());
        assert_eq!(super::candidate_is_qualified(slot, cpu, 0), Err(super::LocalFallbackReason::FrameUnavailable));
        // The fair scan orders on admission and the token alone; only the
        // winner's re-proof above pins the allocation and reads the frame.
        assert_eq!(super::candidate_is_admitted(slot, cpu, 0), Ok(Some(token.owner_generation)));
        super::runqueue::publish_blocked(slot, cpu, 100);
        assert!(matches!(super::runqueue::publish_direct_handoff(slot, cpu),
            super::runqueue::RemoteWakeOutcome::Published { .. }));
        assert!(super::frame_publication::direct_handoff_frame_token(slot, cpu).is_some());
        assert_eq!(super::direct_handoff_frame_generation(slot, cpu), Err(super::LocalFallbackReason::FrameUnavailable));
        current_identity::clear(slot);
    }

    #[test]
    fn local_affinity_admits_only_the_intersection_and_a_representable_cpu() {
        assert!(affinity_masks_allow_cpu(0b0110, 0b1100, 2));
        assert!(!affinity_masks_allow_cpu(0b0110, 0b1100, 1));
        assert!(!affinity_masks_allow_cpu(0b0110, 0b1100, 3));
        assert!(!affinity_masks_allow_cpu(u64::MAX, u64::MAX, 64));
        assert!(!affinity_masks_allow_cpu(u64::MAX, u64::MAX, usize::MAX));
    }

    #[test]
    fn overdue_system_prefers_oldest_age_then_vruntime_without_reordering_ties() {
        let oldest = super::older_overdue_system(None, (6, 20, 7, 4));
        assert_eq!(super::older_overdue_system(oldest, (2, 21, 0, 5)), oldest);
        let oldest = super::older_overdue_system(oldest, (8, 20, 6, 3));
        assert_eq!(oldest, Some((8, 20, 6, 3)));
        assert_eq!(super::older_overdue_system(oldest, (4, 20, 6, 9)), oldest);
        assert_eq!(
            super::older_overdue_system(oldest, (9, 19, 99, 2)),
            Some((9, 19, 99, 2)),
        );
    }

    #[test]
    fn direct_ipc_fair_competitor_requires_live_nonidle_local_custody() {
        let _cpu = crate::multitask::cpu_local::test_publication_lock();
        let _queue = super::runqueue::test_serial_guard();
        super::runqueue::reset_before_publication();
        let (idle, peer, cpu, weight) = (45, 46, 2, 144);
        super::runqueue::admit_blocked(idle);
        super::runqueue::set_idle_slot(idle, true);
        super::runqueue::publish_local(idle, cpu, weight);
        assert!(!super::local_fair_competitor_ready(cpu));

        super::runqueue::admit_blocked(peer);
        super::runqueue::publish_local(peer, cpu, weight);
        assert!(super::local_fair_competitor_ready(cpu));
        super::runqueue::publish_blocked(peer, cpu, weight);
        assert!(!super::local_fair_competitor_ready(cpu));
        super::runqueue::set_idle_slot(peer, true);
        super::runqueue::publish_local(peer, cpu, weight);
        assert!(!super::local_fair_competitor_ready(cpu));
    }

    #[test]
    fn borrowed_context_selects_published_owner_stamp_or_fails_closed() {
        use crate::multitask::current_identity::{DispatchPublication, PagerChargeStamp};

        let own = Some(PagerChargeStamp {
            context_slot: 6,
            context_generation: 1,
            ..PagerChargeStamp::default()
        });
        let donor = PagerChargeStamp {
            context_slot: 5,
            context_generation: 2,
            ..PagerChargeStamp::default()
        };
        let mut owner = DispatchPublication::default();
        owner.task_id = Some(41);
        owner.pager_charge = Some(donor);
        assert_eq!(super::effective_charge_stamp(own, None), Some(own));
        assert_eq!(
            super::effective_charge_stamp(own, Some(Some(owner))),
            Some(Some(donor)),
        );
        owner.pager_charge = None;
        assert_eq!(
            super::effective_charge_stamp(own, Some(Some(owner))),
            Some(None)
        );
        assert_eq!(super::effective_charge_stamp(own, Some(None)), None);
        owner.task_id = None;
        assert_eq!(super::effective_charge_stamp(own, Some(Some(owner))), None);
    }

    #[test]
    fn complete_yield_prefers_an_eligible_peer_before_reusing_current() {
        let peer = Some((7, 20, 7));
        let outgoing = Some((2, 0, 2));
        assert_eq!(
            select_class_candidate(false, None, None, peer, outgoing),
            (peer, false),
        );
        assert_eq!(
            select_class_candidate(false, None, None, None, outgoing),
            (outgoing, false),
        );
    }

    #[test]
    fn unstable_epoch_rejects_before_candidate_scan() {
        for (queue, class) in [(1, 0), (0, 1), (1, 1)] {
            assert_eq!(
                super::choose_local_next(0, 0, true, 0, 0, queue, class, false, false),
                Err(super::LocalFallbackReason::CustodyUnavailable),
            );
        }
    }

    #[test]
    fn user_reservation_precedes_voluntary_exclusion_only_when_applied() {
        assert!(ordinary_voluntary_pick(true, false));
        assert!(!ordinary_voluntary_pick(true, true));
        assert!(!ordinary_voluntary_pick(false, false));
    }

    #[test]
    fn reservation_without_an_eligible_user_preserves_ordinary_system_progress() {
        let system = Some((7, 20, 7));
        let (picked, applied) = select_class_candidate(true, None, system, None, None);
        assert_eq!(picked, system);
        assert!(!applied);
    }

    #[test]
    fn eligible_user_reservation_preempts_the_ordinary_system_candidate() {
        let reservation = Some((11, 50, 11));
        let (picked, applied) =
            select_class_candidate(true, reservation, Some((7, 10, 7)), None, None);
        assert_eq!(picked, reservation);
        assert!(applied);
    }

    #[test]
    fn locality_is_bounded_by_the_configured_vruntime_lag() {
        let global = Some((3, 10, 3));
        assert_eq!(
            prefer_local(global, Some((9, 10 + super::SCHED_CPU_LOCALITY_LAG_NS, 9))),
            Some((9, 10 + super::SCHED_CPU_LOCALITY_LAG_NS, 9))
        );
        assert_eq!(
            prefer_local(global, Some((9, 11 + super::SCHED_CPU_LOCALITY_LAG_NS, 9))),
            global
        );
    }

    #[test]
    fn fair_shadow_uses_vruntime_then_requested_tie_break() {
        assert_eq!(prefer(Some((2, 10, 2)), (3, 11, 1)), Some((2, 10, 2)));
        assert_eq!(prefer(Some((2, 10, 2)), (3, 10, 1)), Some((3, 10, 1)));
        assert_eq!(prefer(Some((2, 10, 2)), (3, 10, 3)), Some((2, 10, 2)));
        // Outgoing same-slot turns use a +1 key, so an exact vruntime
        // tie favors a fresh peer without overriding System class priority.
        assert_eq!(prefer(Some((2, 11, 2)), (3, 10, 3)), Some((3, 10, 3)));
        let system = Some((3, 99, 3));
        let user = Some((2, 1, 2));
        assert_eq!(
            select_class_candidate(false, None, system, user, None),
            (system, false)
        );
    }

    #[test]
    fn three_candidate_fair_order_is_permutation_invariant() {
        use super::LocalClass;

        // Independent lexicographic oracle: class, biased vruntime, slot.
        // All candidates are equally CPU-local; locality lag, min-granularity,
        // budgets, and frame admission are separate obligations.
        const ORDERS: [[usize; 3]; 6] = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        for system_mask in 0..8_u8 {
            for runtime_code in 0..27_u64 {
                let class = |slot: usize| {
                    if system_mask & (1 << slot) != 0 {
                        LocalClass::System
                    } else {
                        LocalClass::User
                    }
                };
                let runtime = |slot: usize| (runtime_code / 3_u64.pow(slot as u32)) % 3;
                let key = |slot: usize| runtime(slot) + u64::from(slot == 0);
                let expected = (0..3)
                    .min_by_key(|&slot| (class(slot), key(slot), slot))
                    .expect("three candidates");
                for order in ORDERS {
                    let mut system = None;
                    let mut user = None;
                    for slot in order {
                        let candidate = (slot, key(slot), slot);
                        let best = if class(slot) == LocalClass::System {
                            &mut system
                        } else {
                            &mut user
                        };
                        *best = prefer(*best, candidate);
                    }
                    let (actual, reservation_applied) =
                        select_class_candidate(false, None, system, user, None);
                    assert!(!reservation_applied);
                    assert_eq!(
                        actual.map(|(slot, _, _)| slot),
                        Some(expected),
                        "class mask={system_mask} runtimes={runtime_code} order={order:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn overdue_system_short_circuit_handles_user_only_fresh_system_and_in_place_updates() {
        let _cpu = crate::multitask::cpu_local::test_publication_lock();
        let _queue = super::runqueue::test_serial_guard();
        super::runqueue::reset_before_publication();
        let (cpu, user_slot, sys_slot) = (2, 48, 49);
        let tps = 100_u64;
        let bound_ms = super::super::SYSTEM_READY_LATENCY_BOUND_MS;

        // Empty queue short-circuits immediately.
        let seq = super::runqueue::local_runnable_sequence(cpu);
        assert!(super::runqueue::overdue_system_short_circuit(cpu, 1000, tps, bound_ms, seq));
        assert!(super::runqueue::has_no_system_runnable(cpu, seq));

        // User-only queue short-circuits for both overdue and hint checks.
        super::runqueue::weight::initialize(user_slot, super::super::NICE_0_LOAD);
        super::runqueue::wait::set_ready_since_ticks(user_slot, 1);
        super::runqueue::admit_blocked(user_slot);
        super::runqueue::publish_local(user_slot, cpu, super::super::NICE_0_LOAD);
        let seq = super::runqueue::local_runnable_sequence(cpu);
        assert!(super::runqueue::overdue_system_short_circuit(cpu, 100_000, tps, bound_ms, seq));
        assert!(super::runqueue::has_no_system_runnable(cpu, seq));

        // Fresh System task short-circuits when elapsed < bound_ms, and stops short-circuiting when overdue.
        let sys_w = super::super::SYSTEM_CLASS_WEIGHT_FLAG | super::super::NICE_0_LOAD;
        super::runqueue::weight::initialize(sys_slot, sys_w);
        super::runqueue::wait::set_ready_since_ticks(sys_slot, 1000);
        super::runqueue::admit_blocked(sys_slot);
        super::runqueue::publish_local(sys_slot, cpu, sys_w);
        let seq = super::runqueue::local_runnable_sequence(cpu);
        assert!(!super::runqueue::has_no_system_runnable(cpu, seq));
        assert!(super::runqueue::overdue_system_short_circuit(cpu, 1000, tps, bound_ms, seq));
        assert!(!super::runqueue::overdue_system_short_circuit(cpu, 100_000, tps, bound_ms, seq));

        // In-place ready_since update on a Local slot invalidates summary so scan observes it.
        super::runqueue::wait::set_ready_since_ticks(sys_slot, 1);
        assert!(!super::runqueue::overdue_system_short_circuit(cpu, 1000, tps, bound_ms, seq));

        // Removing the System slot restores User-only O(1) short-circuit.
        super::runqueue::publish_blocked(sys_slot, cpu, sys_w);
        let seq = super::runqueue::local_runnable_sequence(cpu);
        assert!(super::runqueue::overdue_system_short_circuit(cpu, 100_000, tps, bound_ms, seq));
        assert!(super::runqueue::has_no_system_runnable(cpu, seq));

        // In-place weight::clear on a queued slot forces scan to observe PublicationUnavailable.
        super::runqueue::weight::clear(user_slot);
        assert!(!super::runqueue::overdue_system_short_circuit(cpu, 1000, tps, bound_ms, seq));
        assert!(!super::runqueue::has_no_system_runnable(cpu, seq));

        super::runqueue::retire(user_slot, super::super::NICE_0_LOAD);
        super::runqueue::release_retired(user_slot);
        super::runqueue::retire(sys_slot, sys_w);
        super::runqueue::release_retired(sys_slot);
        super::runqueue::reset_before_publication();
    }
}
