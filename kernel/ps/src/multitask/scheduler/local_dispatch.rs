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
pub(super) use fair_retry::retry_incomplete_policy;
#[cfg(not(test))]
pub(super) use fair_retry::{current_after_incomplete, incomplete_policy_fallback};
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
#[cfg(rustos_scheduler_phase_profile)]
static SHADOW_MATCHES: AtomicU64 = AtomicU64::new(0);
#[cfg(rustos_scheduler_phase_profile)]
static SHADOW_MISMATCHES: AtomicU64 = AtomicU64::new(0);
#[cfg(rustos_scheduler_phase_profile)]
static SHADOW_AUTHORITATIVE_FALLBACKS: AtomicU64 = AtomicU64::new(0);
// [flags:8 | cpu:8 | current:8 | catalog-pick:8 | local-policy:8 |
// catalog-pick:8]. Slots are bounded by MAX_TASK=128. The final byte is
// intentionally repeated for compatibility with the existing diagnostic
// decoder while divergence becomes policy evidence rather than a safety gate.
#[cfg(rustos_scheduler_phase_profile)]
static LAST_SHADOW_MISMATCH: AtomicU64 = AtomicU64::new(0);
/// Exact fair keys for the last policy difference. They are diagnostic-only:
/// recording them cannot participate in a selection or repair a publication.
#[cfg(rustos_scheduler_phase_profile)]
static LAST_SHADOW_LOCAL_VRUNTIME: AtomicU64 = AtomicU64::new(0);
#[cfg(rustos_scheduler_phase_profile)]
static LAST_SHADOW_CATALOG_VRUNTIME: AtomicU64 = AtomicU64::new(0);

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
    let _allocation_pin = frame_publication::pin_allocation(frame).ok_or(LocalFallbackReason::FrameUnavailable)?;
    let _frame_reader = unsafe { super::stack_resources::frame_readers::acquire(&_allocation_pin, cpu) }
        .ok_or(LocalFallbackReason::FrameUnavailable)?;
    context_validation::validate_published_saved_context(frame.slot, frame.saved_rsp, frame.view)
        .map_err(|_| LocalFallbackReason::FrameUnavailable)?;
    frame_publication::token_is_current(frame, cpu)
        .then_some(Some(frame.owner_generation))
        .ok_or(LocalFallbackReason::FrameUnavailable)
}

/// A hinted Local owner may bypass the catalog's second schedulability
/// check only with a complete published lifecycle, affinity, budget and frame
/// observation. Other owner states and incomplete reads retain the catalog
/// path; an idle owner on a foreign CPU is a stable rejection.
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
    runqueue::local_runnable_slots(cpu)
        .any(|slot| !runqueue::is_idle_slot(slot) && runqueue::is_local_dispatchable(slot, cpu))
}

/// An empty fair scan may select an idle owner only if its *same* Local
/// generation also has a complete published lifecycle, affinity, budget and
/// saved-frame proof. The currently executing idle frame has no stable token
/// and stays on the guarded fallback path.
#[cfg(not(test))]
pub(super) fn idle_frame_generation(
    slot: usize,
    cpu: usize,
    now_ns: u64,
) -> Result<Option<u64>, LocalFallbackReason> {
    if !runqueue::is_idle_slot(slot) {
        return Err(LocalFallbackReason::CustodyUnavailable);
    }
    candidate_is_qualified(slot, cpu, now_ns)
}

/// A guarded catalog fallback may name the *target CPU's* noncurrent idle
/// owner. Only a complete published Local qualification can promote that
/// choice to an exact-generation frame validation and final claim; otherwise
/// the original guarded validation retains custody.
pub(super) fn qualified_catalog_idle_generation(
    slot: usize,
    current_slot: usize,
    target_idle_slot: Option<usize>,
    prior_generation: Option<u64>,
    qualify: impl FnOnce() -> Result<Option<u64>, LocalFallbackReason>,
) -> Option<u64> {
    if prior_generation.is_some() || slot == current_slot || target_idle_slot != Some(slot) {
        return None;
    }
    qualify().ok().flatten()
}

/// Validates the saved frame behind one exact FIFO handoff. The caller
/// separately checks record identity and lifecycle/budget/affinity admission.
pub(super) fn direct_handoff_frame_generation(
    slot: usize,
    cpu: usize,
) -> Result<u64, LocalFallbackReason> {
    let frame = frame_publication::direct_handoff_frame_token(slot, cpu)
        .ok_or(LocalFallbackReason::FrameUnavailable)?;
    let _allocation_pin =
        frame_publication::pin_allocation(frame).ok_or(LocalFallbackReason::FrameUnavailable)?;
    let _frame_reader =
        unsafe { super::stack_resources::frame_readers::acquire(&_allocation_pin, cpu) }
            .ok_or(LocalFallbackReason::FrameUnavailable)?;
    context_validation::validate_published_saved_context(frame.slot, frame.saved_rsp, frame.view)
        .map_err(|_| LocalFallbackReason::FrameUnavailable)?;
    frame_publication::direct_handoff_token_is_current(frame, cpu)
        .then_some(frame.owner_generation)
        .ok_or(LocalFallbackReason::FrameUnavailable)
}

/// Only a generation-bound Direct FIFO record may use complete published
/// lifecycle, affinity, exact runtime credit and Direct frame as positive
/// admission. Incomplete observations retain the guarded catalog fallback.
pub(super) fn qualified_direct_handoff_generation(
    slot: usize,
    cpu: usize,
    now_ns: u64,
) -> Result<Option<u64>, LocalFallbackReason> {
    if runqueue::is_idle_slot(slot) && super::smp::idle_slot_for_cpu(cpu) != Some(slot) {
        return Ok(None);
    }
    if !published_candidate_admission(slot, cpu, now_ns)? {
        return Ok(None);
    }
    direct_handoff_frame_generation(slot, cpu).map(Some)
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
pub(super) fn choose_local_next(
    cpu: usize,
    current_slot: usize,
    voluntary_yield: bool,
    current_runtime_ns: u64,
    eligibility_now_ns: u64,
    expected_queue_sequence: u64,
    expected_class_sequence: u64,
    atomic_activation_pending: bool,
    synchronous_handoff_pending: bool,
) -> Result<(usize, u64), LocalFallbackReason> {
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

    let reserve_user = user_reservation_due(cpu);
    // The outgoing execution owner cannot change during this scheduler turn.
    // Snapshot its publication once so candidate admission and the later
    // min-granularity decision cannot disagree because they sampled different
    // points of an otherwise valid current-frame transition.
    let current_is_live = current_can_keep_running(current_slot, cpu, eligibility_now_ns);
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
    for slot in runqueue::local_runnable_slots(cpu) {
        if runqueue::is_idle_slot(slot) {
            continue;
        }
        // The outgoing slot is still the CPU's live execution frame and must
        // not manufacture a stable saved-frame token. Its current-CPU owner
        // and lifecycle publication are the coherent evidence for retaining
        // it; every replacement candidate requires the generation-bound frame
        // token below.
        let qualification = if slot == current_slot && current_is_live {
            Ok(Some(runqueue::owner(slot).generation))
        } else {
            candidate_is_qualified(slot, cpu, eligibility_now_ns)
        };
        match qualification {
            Ok(Some(_)) => {}
            // Only a complete, stable negative lifecycle/affinity or exact
            // budget denial can be omitted. Missing identity, transitioning
            // affinity, stale donor/budget or frame evidence still invalidates
            // the entire scan.
            Ok(None) => continue,
            Err(reason) => {
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
        let candidate_class = observed_class(slot)?;
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
            (slot + super::MAX_TASK - current_slot) % super::MAX_TASK
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
            let alternate = match class(current_slot) {
                LocalClass::System => prefer_local(alternate_system, local_alternate_system),
                LocalClass::User => prefer_local(alternate_user, local_alternate_user),
            };
            if let Some((alternate_slot, _, _)) = alternate {
                slot = alternate_slot;
            }
        }
        if !voluntary_yield
            && !reservation_applied
            && slot != current_slot
            && current_is_live
            // Legacy fairness preempts across a higher-priority class, but
            // applies min-granularity to same/lower-priority peers. The current
            // frame is actively executing, so retaining it needs no stable
            // saved-frame token; every replacement still passed one above.
            && class(slot) >= class(current_slot)
        {
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
        let generation = if slot == current_slot && current_is_live {
            if !current_can_keep_running(slot, cpu, eligibility_now_ns) {
                return fallback(LocalFallbackReason::LifecycleIneligible);
            }
            runqueue::owner(slot).generation
        } else {
            match candidate_is_qualified(slot, cpu, eligibility_now_ns) {
                Ok(Some(generation)) => generation,
                Ok(None) => return fallback(LocalFallbackReason::LifecycleIneligible),
                Err(reason) => return fallback(reason),
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
) -> Result<Option<(usize, u64)>, LocalFallbackReason> {
    if expected_queue_sequence & 1 != 0
        || expected_class_sequence & 1 != 0
        || runqueue::local_runnable_sequence(cpu) != expected_queue_sequence
        || donation_ledger::class_publication_sequence() != expected_class_sequence
    {
        return Err(LocalFallbackReason::CustodyUnavailable);
    }
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let mut oldest = None;
    for slot in runqueue::local_runnable_slots(cpu) {
        if slot == current_slot || runqueue::is_idle_slot(slot) { continue; }
        if observed_class(slot)? != LocalClass::System { continue; }
        let since = runqueue::wait::ready_since_ticks(slot);
        let elapsed_ms = now_ticks
            .saturating_sub(since)
            .saturating_mul(1000)
            .saturating_div(ticks_per_second);
        if since == 0 || elapsed_ms < super::SYSTEM_READY_LATENCY_BOUND_MS {
            continue;
        }
        let Some(generation) = candidate_is_qualified(slot, cpu, now_ns)? else {
            continue;
        };
        oldest = older_overdue_system(oldest, (slot, since, runqueue::vruntime(slot), generation));
    }
    if runqueue::local_runnable_sequence(cpu) != expected_queue_sequence
        || donation_ledger::class_publication_sequence() != expected_class_sequence
    {
        return Err(LocalFallbackReason::CustodyUnavailable);
    }
    Ok(oldest.map(|(slot, _, _, generation)| (slot, generation)))
}

/// An empty, stable fair bitmap can use this CPU's exact Local idle owner
/// instead of rescanning a catalog that contains no local fair candidate.
/// Generation zero, remote, nonrunnable, and direct-handoff custody cannot
/// manufacture an idle claim.
pub(super) fn empty_fair_idle_generation(
    owner: runqueue::RunOwnerSnapshot,
    cpu: usize,
) -> Option<u64> {
    (owner.state == runqueue::RunOwnerState::Local
        && owner.cpu == Some(cpu)
        && owner.runnable
        && owner.generation != 0)
        .then_some(owner.generation)
}

/// Host-only catalog fair fixture. Production retries CPU-local observations
/// and defers to its exact idle/current fallback on repeated torn snapshots.
#[cfg(test)]
#[inline]
pub(super) fn catalog_fair_fallback(
    deferred: bool,
    local_pick: Option<usize>,
    catalog_pick: impl FnOnce() -> Option<usize>,
    idle_slot: impl FnOnce() -> usize,
) -> Option<usize> {
    (deferred && local_pick.is_none()).then(|| catalog_pick().unwrap_or_else(idle_slot))
}

/// Host-only legacy min-granularity comparator. Production's published fair
/// picker includes its own class-aware minimum slice and burst limit.
#[cfg(test)]
#[inline]
pub(super) fn catalog_min_granularity_fallback(
    profile: bool,
    local_pick: Option<usize>,
    exact_yield_or_reserved: bool,
    selected: usize,
    catalog_guard: impl FnOnce() -> usize,
) -> usize {
    if exact_yield_or_reserved || (!profile && local_pick.is_some()) {
        selected
    } else {
        catalog_guard()
    }
}

/// Compare a qualified local observation with the preserved catalog policy.
/// Excluded arms are observable without receiving dispatch authority; record
/// the actual selection separately so agreement cannot be manufactured by
/// simply disabling observations on an unproven arm.
#[cfg(rustos_scheduler_phase_profile)]
pub(super) fn record_policy_choice(
    cpu: usize,
    current_slot: usize,
    catalog_pick: usize,
    voluntary_yield: bool,
    local_pick: Option<usize>,
    authoritative_pick: Option<usize>,
    catalog_recheck: Option<usize>,
    catalog_admission: u8,
) {
    if authoritative_pick.is_none() {
        SHADOW_AUTHORITATIVE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
    }
    let Some(local_slot) = local_pick else {
        return;
    };
    if local_slot == catalog_pick {
        SHADOW_MATCHES.fetch_add(1, Ordering::Relaxed);
        return;
    }

    debug_assert!(cpu < 256 && current_slot < 256 && catalog_pick < 256 && local_slot < 256);
    let flags = u8::from(voluntary_yield)
        | (u8::from(user_reservation_due(cpu)) << 1)
        | (u8::from(class(local_slot) == LocalClass::System) << 2)
        | (u8::from(class(catalog_pick) == LocalClass::System) << 3)
        | (u8::from(runqueue::is_idle_slot(local_slot)) << 4)
        | (u8::from(runqueue::is_idle_slot(catalog_pick)) << 5)
        | (u8::from(usize::from(runqueue::affinity_payload::last_cpu(local_slot)) == cpu) << 6)
        | (u8::from(usize::from(runqueue::affinity_payload::last_cpu(catalog_pick)) == cpu) << 7);
    // Bit 56 marks this extended witness. The upper seven bits encode the
    // first catalog yield scan's admission stage for the local slot; 127
    // denotes an arm without a yield-scan observation.
    let witness = ((u64::from(catalog_admission & 0x7f) << 57) | (1_u64 << 56))
        | ((catalog_recheck.unwrap_or(255) as u64) << 48)
        | (u64::from(flags) << 40)
        | ((cpu as u64) << 32)
        | ((current_slot as u64) << 24)
        | ((catalog_pick as u64) << 16)
        | ((local_slot as u64) << 8)
        | authoritative_pick.unwrap_or(catalog_pick) as u64;
    LAST_SHADOW_LOCAL_VRUNTIME.store(runqueue::vruntime(local_slot), Ordering::Relaxed);
    LAST_SHADOW_CATALOG_VRUNTIME.store(runqueue::vruntime(catalog_pick), Ordering::Relaxed);
    LAST_SHADOW_MISMATCH.store(witness, Ordering::Relaxed);
    SHADOW_MISMATCHES.fetch_add(1, Ordering::Relaxed);
}

/// Drains qualification and policy-comparison counters for runtime evidence.
#[cfg(rustos_scheduler_phase_profile)]
pub(super) fn drain_shadow_observations() -> (u64, [u64; 7], u64, u64, u64, u64, u64, u64) {
    (
        SHADOW_DISPATCHES.swap(0, Ordering::Relaxed),
        core::array::from_fn(|index| SHADOW_FALLBACKS[index].swap(0, Ordering::Relaxed)),
        SHADOW_MATCHES.swap(0, Ordering::Relaxed),
        SHADOW_MISMATCHES.swap(0, Ordering::Relaxed),
        SHADOW_AUTHORITATIVE_FALLBACKS.swap(0, Ordering::Relaxed),
        LAST_SHADOW_MISMATCH.swap(0, Ordering::Relaxed),
        LAST_SHADOW_LOCAL_VRUNTIME.swap(0, Ordering::Relaxed),
        LAST_SHADOW_CATALOG_VRUNTIME.swap(0, Ordering::Relaxed),
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
    fn empty_fair_scan_claims_only_the_exact_local_idle_generation() {
        use super::super::runqueue::{RunOwnerSnapshot, RunOwnerState};

        let owner = RunOwnerSnapshot {
            state: RunOwnerState::Local,
            cpu: Some(2),
            generation: 9,
            runnable: true,
            wait_reason_kind: 0,
            wait_armed: false,
        };
        assert_eq!(super::empty_fair_idle_generation(owner, 2), Some(9));
        assert_eq!(super::empty_fair_idle_generation(owner, 1), None);
        assert_eq!(
            super::empty_fair_idle_generation(
                RunOwnerSnapshot {
                    runnable: false,
                    ..owner
                },
                2
            ),
            None,
        );
        assert_eq!(
            super::empty_fair_idle_generation(
                RunOwnerSnapshot {
                    state: RunOwnerState::DirectHandoff,
                    ..owner
                },
                2
            ),
            None,
        );
        assert_eq!(
            super::empty_fair_idle_generation(
                RunOwnerSnapshot {
                    generation: 0,
                    ..owner
                },
                2
            ),
            None,
        );
    }

    #[test]
    fn catalog_idle_promotes_only_an_exact_noncurrent_target_local_publication() {
        let target_idle = Some(15);
        assert_eq!(
            super::qualified_catalog_idle_generation(15, 3, target_idle, None, || Ok(Some(27))),
            Some(27),
        );
        for (slot, current, idle, prior) in [
            (15, 15, target_idle, None),
            (15, 3, Some(16), None),
            (15, 3, target_idle, Some(21)),
        ] {
            assert_eq!(
                super::qualified_catalog_idle_generation(slot, current, idle, prior, || {
                    panic!("a noneligible catalog idle must not observe a published frame")
                }),
                None,
            );
        }
        assert_eq!(
            super::qualified_catalog_idle_generation(15, 3, target_idle, None, || {
                Err(super::LocalFallbackReason::FrameUnavailable)
            }),
            None,
        );
        assert_eq!(
            super::qualified_catalog_idle_generation(15, 3, target_idle, None, || Ok(None)),
            None,
        );
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
    fn deferred_catalog_fair_scan_runs_only_for_incomplete_local_observations() {
        assert_eq!(
            super::catalog_fair_fallback(
                true,
                Some(3),
                || panic!("duplicate fair scan"),
                || { panic!("idle not required") }
            ),
            None
        );
        assert_eq!(
            super::catalog_fair_fallback(
                false,
                None,
                || panic!("not a fair arm"),
                || { panic!("idle not required") }
            ),
            None
        );
        assert_eq!(
            super::catalog_fair_fallback(true, None, || Some(7), || panic!("not idle")),
            Some(7)
        );
        assert_eq!(
            super::catalog_fair_fallback(true, None, || None, || 5),
            Some(5)
        );
    }

    #[test]
    fn qualified_local_pick_drops_only_the_discarded_catalog_granularity_check() {
        use super::catalog_min_granularity_fallback as choose;

        assert_eq!(
            choose(false, Some(9), false, 9, || panic!(
                "discarded catalog guard"
            )),
            9
        );
        // An incomplete observation still needs the old guard on its actual
        // catalog fair/idle selection, not a made-up local result.
        assert_eq!(choose(false, None, false, 9, || 5), 5);
        // Instrumented builds retain the comparison even for a qualified
        // choice, but local authority still wins after the shadow result.
        assert_eq!(choose(true, Some(9), false, 9, || 5), 5);
        for (profile, local) in [(false, None), (false, Some(9)), (true, Some(9))] {
            assert_eq!(
                choose(profile, local, true, 9, || panic!(
                    "exact arm cannot use CFS guard"
                )),
                9
            );
        }
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
}
