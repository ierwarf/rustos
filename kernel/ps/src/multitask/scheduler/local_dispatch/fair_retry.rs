//! Bounded CPU-local re-observation after a torn fair publication.
//!
//! Catalog selection cannot make an incomplete snapshot authoritative. Each
//! attempt obtains new queue/class epochs; only one complete candidate set may
//! grant a generation-bound turn. Exhaustion defers to the per-CPU idle/current
//! fallback rather than dispatching a catalog-only peer.

use super::LocalFallbackReason;
#[cfg(not(test))]
use super::{current_can_keep_running, runqueue};

const MAX_LOCAL_FAIR_OBSERVATIONS: usize = 4;

pub(in crate::multitask::scheduler) fn retry_incomplete_fair(
    mut observe: impl FnMut() -> Result<Option<(usize, u64)>, LocalFallbackReason>,
) -> Option<(usize, u64)> {
    for _ in 0..MAX_LOCAL_FAIR_OBSERVATIONS {
        match observe() {
            Ok(choice) => return choice,
            Err(LocalFallbackReason::NoLocalCandidate) => return None,
            Err(_) => continue,
        }
    }
    None
}

/// A one-shot Local hint can carry only complete published admission. A
/// missing generation has no catalog alternative and stays with fair custody.
pub(in crate::multitask::scheduler) fn complete_hint_generation(
    qualification: Result<Option<u64>, LocalFallbackReason>,
) -> Option<u64> {
    qualification.ok().flatten()
}

/// Earlier strict-class and reservation decisions cannot interpret an odd
/// publication as 'no peer': that would let a later policy arm overtake a
/// possibly eligible older System or reserved User continuation. Retry fresh
/// epochs, preserving an unresolved result for the original frame/funded fallback.
/// The production fair candidate uses this same retry boundary and limit;
/// retaining its exact candidate avoids rebuilding identity after observation.
pub(in crate::multitask::scheduler) fn retry_incomplete_policy<T>(
    mut observe: impl FnMut() -> Result<Option<T>, LocalFallbackReason>,
) -> Result<Option<T>, LocalFallbackReason> {
    for _ in 0..MAX_LOCAL_FAIR_OBSERVATIONS {
        match observe() {
            Ok(choice) => return Ok(choice),
            Err(LocalFallbackReason::NoLocalCandidate) => return Ok(None),
            Err(_) => continue,
        }
    }
    Err(LocalFallbackReason::CustodyUnavailable) // unresolved earlier class
}

/// An unresolved earlier class decision must not fall through to later
/// hints or ordinary fair selection. Keep a qualified current Local owner,
/// otherwise the exact published idle owner; absence goes to the guarded
/// idle fallback, not a catalog-selected peer.
#[cfg(not(test))]
pub(in crate::multitask::scheduler) fn incomplete_policy_fallback(
    current_slot: usize,
    cpu: usize,
    idle_slot: usize,
) -> Option<(usize, u64)> {
    let now_ns = crate::arch::clock::monotonic_nanos();
    current_after_incomplete(current_slot, cpu, now_ns).or_else(|| {
        let generation = super::empty_fair_idle_generation(runqueue::owner(idle_slot), cpu)?;
        (super::idle_frame_generation(idle_slot, cpu, now_ns) == Ok(Some(generation)))
            .then_some((idle_slot, generation))
    })
}

/// An incomplete peer snapshot cannot displace a live current frame. The
/// outgoing frame was already validated this turn; retain only its exact
/// Local generation, never an unbound catalog-selected continuation.
#[cfg(not(test))]
pub(in crate::multitask::scheduler) fn current_after_incomplete(
    slot: usize,
    cpu: usize,
    now_ns: u64,
) -> Option<(usize, u64)> {
    if !current_can_keep_running(slot, cpu, now_ns) {
        return None;
    }
    let owner = runqueue::owner(slot);
    (owner.state == runqueue::RunOwnerState::Local && owner.cpu == Some(cpu) && owner.runnable)
        .then_some((slot, owner.generation))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torn_fair_observations_retry_without_authorizing_a_partial_candidate() {
        let mut attempts = 0;
        let pick = retry_incomplete_fair(|| {
            attempts += 1;
            if attempts < 3 {
                Err(LocalFallbackReason::PublicationUnavailable)
            } else {
                Ok(Some((12, 41)))
            }
        });
        assert_eq!(pick, Some((12, 41)));
        assert_eq!(attempts, 3);

        let mut attempts = 0;
        assert_eq!(
            retry_incomplete_fair(|| {
                attempts += 1;
                Err(LocalFallbackReason::FrameUnavailable)
            }),
            None,
        );
        assert_eq!(attempts, MAX_LOCAL_FAIR_OBSERVATIONS);

        let mut attempts = 0;
        assert_eq!(
            retry_incomplete_fair(|| {
                attempts += 1;
                Err(LocalFallbackReason::NoLocalCandidate)
            }),
            None,
        );
        assert_eq!(attempts, 1);
    }

    #[test]
    fn incomplete_non_fifo_hint_cannot_inherit_a_catalog_peer() {
        use super::{LocalFallbackReason, complete_hint_generation};
        assert_eq!(complete_hint_generation(Ok(Some(19))), Some(19));
        assert_eq!(complete_hint_generation(Ok(None)), None);
        assert_eq!(
            complete_hint_generation(Err(LocalFallbackReason::CustodyUnavailable)),
            None
        );
        assert_eq!(
            complete_hint_generation(Err(LocalFallbackReason::FrameUnavailable)),
            None
        );
    }

    #[test]
    fn incomplete_early_policy_never_means_no_due_peer() {
        use super::{LocalFallbackReason, MAX_LOCAL_FAIR_OBSERVATIONS, retry_incomplete_policy};
        let mut attempts = 0;
        let unresolved = retry_incomplete_policy::<(usize, u64)>(|| {
            attempts += 1;
            Err(LocalFallbackReason::CustodyUnavailable)
        });
        assert!(matches!(
            unresolved,
            Err(LocalFallbackReason::CustodyUnavailable)
        ));
        assert_eq!(attempts, MAX_LOCAL_FAIR_OBSERVATIONS);
        let mut attempts = 0;
        assert_eq!(
            retry_incomplete_policy(|| {
                attempts += 1;
                if attempts == 2 {
                    Ok(Some((7, 18)))
                } else {
                    Err(LocalFallbackReason::CustodyUnavailable)
                }
            })
            .unwrap(),
            Some((7, 18))
        );
        assert_eq!(attempts, 2);
        assert_eq!(retry_incomplete_policy::<(usize, u64)>(|| Ok(None)).unwrap(), None);
    }
}
