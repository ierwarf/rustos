//! Generation-bound authority for ordinary per-CPU fair dispatch.
//!
//! The picker reads durable runqueue, weight, donation, budget, frame, and
//! lifecycle publications. It becomes authoritative only after a coherent
//! full-candidate revalidation. Exact activation and IPC FIFO custody remain
//! catalog-owned, and every incomplete observation fails closed to that
//! catalog rather than manufacturing local authority.

#[cfg(rustos_scheduler_phase_profile)]
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(test))]
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
enum LocalClass {
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

fn fallback(reason: LocalFallbackReason) -> Result<usize, LocalFallbackReason> {
    #[cfg(rustos_scheduler_phase_profile)]
    SHADOW_FALLBACKS[reason as usize].fetch_add(1, Ordering::Relaxed);
    Err(reason)
}

pub(super) fn catalog_revalidation_fallback() -> Result<usize, LocalFallbackReason> {
    fallback(LocalFallbackReason::CustodyUnavailable)
}

fn user_reservation_due(cpu: usize) -> bool {
    #[cfg(test)]
    {
        let _ = cpu;
        false
    }
    #[cfg(not(test))]
    {
        dispatch_policy::system_dispatch_streak(cpu) >= MAX_CONSECUTIVE_SYSTEM_DISPATCHES
    }
}

fn class(slot: usize) -> LocalClass {
    if runqueue::weight::value(slot) & SYSTEM_CLASS_WEIGHT_FLAG != 0
        || donation_ledger::inherited_system(slot)
    {
        LocalClass::System
    } else {
        LocalClass::User
    }
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
        && publication
            .pager_charge
            .is_none_or(|stamp| scheduling_runtime::published_stamp_is_eligible(stamp, now_ns))
        && owner.cpu == Some(cpu)
        && runqueue::owner_has_run_intent(owner)
}

fn candidate_is_qualified(slot: usize, cpu: usize, now_ns: u64) -> Result<(), LocalFallbackReason> {
    let Some(publication) = current_identity::read_dispatch(slot) else {
        return Err(LocalFallbackReason::PublicationUnavailable);
    };
    if !publication.lifecycle_dispatchable() {
        return Err(LocalFallbackReason::LifecycleIneligible);
    }
    if !publication
        .pager_charge
        .is_none_or(|stamp| scheduling_runtime::published_stamp_is_eligible(stamp, now_ns))
    {
        return Err(LocalFallbackReason::LifecycleIneligible);
    }
    // The frame token proves the stronger fair-queue custody condition
    // (`Local`, same CPU, runnable, non-executing, exact generation), so a
    // preceding generic dispatchability read would only sample OWNER twice.
    let Some(frame) = frame_publication::local_frame_token(slot, cpu) else {
        return Err(LocalFallbackReason::FrameUnavailable);
    };
    // `local_frame_token` proves this is the exact non-running, non-transition
    // owner generation before this pure validator reaches its one unsafe frame
    // dereference.
    context_validation::validate_published_saved_context(frame.slot, frame.saved_rsp, frame.view)
        .map_err(|_| LocalFallbackReason::FrameUnavailable)?;
    frame_publication::token_is_current(frame, cpu)
        .then_some(())
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
) -> Result<usize, LocalFallbackReason> {
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
            Ok(())
        } else {
            candidate_is_qualified(slot, cpu, eligibility_now_ns)
        };
        if let Err(reason) = qualification {
            first_revalidation_failure.get_or_insert(reason);
            continue;
        }
        let vruntime = runqueue::vruntime(slot);
        // Match legacy selection's same-slot turn bias. Voluntary yields omit
        // the current slot above and use circular-distance tie breaking.
        let key = if slot == current_slot {
            vruntime.saturating_add(1)
        } else {
            vruntime
        };
        let candidate_class = class(slot);
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
        #[cfg(rustos_scheduler_phase_profile)]
        SHADOW_DISPATCHES.fetch_add(1, Ordering::Relaxed);
        Ok(slot)
    } else {
        fallback(LocalFallbackReason::NoLocalCandidate)
    }
}

/// Records how the new authoritative fair policy compares with the preserved
/// catalog policy. Divergence is diagnostic policy evidence, not a cutover
/// gate; an unavailable local choice means the catalog retained authority.
#[cfg(rustos_scheduler_phase_profile)]
pub(super) fn record_policy_choice(
    cpu: usize,
    current_slot: usize,
    catalog_pick: usize,
    voluntary_yield: bool,
    local_pick: Option<usize>,
) {
    let Some(local_slot) = local_pick else {
        SHADOW_AUTHORITATIVE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
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
    let witness = (u64::from(flags) << 40)
        | ((cpu as u64) << 32)
        | ((current_slot as u64) << 24)
        | ((catalog_pick as u64) << 16)
        | ((local_slot as u64) << 8)
        | catalog_pick as u64;
    LAST_SHADOW_MISMATCH.store(witness, Ordering::Relaxed);
    SHADOW_MISMATCHES.fetch_add(1, Ordering::Relaxed);
}

/// Drains qualification and policy-comparison counters for runtime evidence.
#[cfg(rustos_scheduler_phase_profile)]
pub(super) fn drain_shadow_observations() -> (u64, [u64; 7], u64, u64, u64, u64) {
    (
        SHADOW_DISPATCHES.swap(0, Ordering::Relaxed),
        core::array::from_fn(|index| SHADOW_FALLBACKS[index].swap(0, Ordering::Relaxed)),
        SHADOW_MATCHES.swap(0, Ordering::Relaxed),
        SHADOW_MISMATCHES.swap(0, Ordering::Relaxed),
        SHADOW_AUTHORITATIVE_FALLBACKS.swap(0, Ordering::Relaxed),
        LAST_SHADOW_MISMATCH.swap(0, Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::{ordinary_voluntary_pick, prefer, prefer_local, select_class_candidate};

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
    }
}
