//! Bounded CPU-local re-observation after a torn fair publication.
//!
//! An incomplete snapshot is never authoritative. Each attempt obtains new
//! queue/class epochs; only one complete candidate set (minus candidates
//! unresolved through every earlier attempt, see `UnresolvedCandidates`) may
//! grant a generation-bound turn. Exhaustion defers to the per-CPU
//! idle/current fallback.

use super::LocalFallbackReason;

const MAX_LOCAL_FAIR_OBSERVATIONS: usize = 4;

const SLOT_WORDS: usize = super::super::MAX_TASK.div_ceil(64);

/// Candidates whose own admission stayed unresolved through every strict
/// observation of one dispatch turn.
///
/// A scan treats a candidate it cannot resolve (missing identity or class
/// publication, transitioning affinity, stale budget stamp, absent frame
/// token) as making the whole candidate set non-authoritative, because that
/// candidate might be the class or vruntime winner. That is right for a torn
/// observation and wrong for a candidate that stays unresolved: every turn of
/// the CPU then ends `Incomplete`, the CPU runs idle or the outgoing task, and
/// every other task queued there starves for as long as the one candidate
/// stays stuck. So the first `MAX_LOCAL_FAIR_OBSERVATIONS - 1` observations
/// stay strict, and the last one omits exactly the slots that were unresolved
/// in ALL earlier observations, as if they were not admitted this turn. An
/// epoch tear is not attributed to a slot and stays strict; an omitted slot is
/// re-observed from scratch on the next turn; the owner CAS, not scan
/// completeness, is what makes the eventual claim safe.
pub(in crate::multitask::scheduler) struct UnresolvedCandidates {
    observation: usize,
    current: [u64; SLOT_WORDS],
    persistent: [u64; SLOT_WORDS],
    last_reason: Option<LocalFallbackReason>,
    omitted: Option<(usize, LocalFallbackReason)>,
}

impl UnresolvedCandidates {
    /// One strict observation that never omits a candidate.
    pub(in crate::multitask::scheduler) const fn strict() -> Self {
        Self {
            observation: 0, current: [0; SLOT_WORDS], persistent: [!0; SLOT_WORDS],
            last_reason: None, omitted: None,
        }
    }

    fn begin_observation(&mut self) {
        if self.observation != 0 {
            for (persistent, current) in self.persistent.iter_mut().zip(self.current) {
                *persistent &= current;
            }
        }
        self.current = [0; SLOT_WORDS];
        self.observation += 1;
    }

    /// `slot` could not be resolved in this observation for `reason`.
    pub(in crate::multitask::scheduler) fn record(&mut self, slot: usize, reason: LocalFallbackReason) {
        self.current[slot / 64] |= 1_u64 << (slot % 64);
        self.last_reason = Some(reason);
    }

    /// Whether the scan must leave `slot` out of this (final) observation: it
    /// was unresolved in every earlier one. Checked before the slot is
    /// evaluated, so a slot that admits but then fails its final qualification
    /// is left out as well.
    pub(in crate::multitask::scheduler) fn excludes(&mut self, slot: usize) -> bool {
        let exclude = self.observation == MAX_LOCAL_FAIR_OBSERVATIONS
            && self.persistent[slot / 64] & (1_u64 << (slot % 64)) != 0;
        if exclude {
            // Keep the CPU's evidence for the omitted slot's own liveness.
            self.current[slot / 64] |= 1_u64 << (slot % 64);
            if self.omitted.is_none() {
                self.omitted = Some((slot, self.last_reason.unwrap_or(LocalFallbackReason::CustodyUnavailable)));
            }
        }
        exclude
    }

    /// The first slot the final observation omitted, for the caller's diagnostic.
    pub(in crate::multitask::scheduler) fn omitted(&self) -> Option<(usize, LocalFallbackReason)> {
        self.omitted
    }
}

/// Earlier strict-class and reservation decisions cannot interpret an odd
/// publication as 'no peer': that would let a later policy arm overtake a
/// possibly eligible older System or reserved User continuation. Retry fresh
/// epochs, preserving an unresolved result for the original frame/funded fallback.
/// The production fair candidate uses this same retry boundary and limit;
/// retaining its exact candidate avoids rebuilding identity after observation.
/// The final observation omits only candidates unresolved in every earlier one
/// (see `UnresolvedCandidates`), so one stuck task cannot idle the whole CPU.
pub(in crate::multitask::scheduler) fn retry_incomplete_policy<T>(
    mut observe: impl FnMut(&mut UnresolvedCandidates) -> Result<Option<T>, LocalFallbackReason>,
) -> Result<Option<T>, LocalFallbackReason> {
    let mut unresolved = UnresolvedCandidates::strict();
    for _ in 0..MAX_LOCAL_FAIR_OBSERVATIONS {
        unresolved.begin_observation();
        match observe(&mut unresolved) {
            Ok(choice) => {
                if let Some((slot, reason)) = unresolved.omitted() {
                    record_omitted_candidate(slot, reason);
                }
                return Ok(choice);
            }
            Err(LocalFallbackReason::NoLocalCandidate) => return Ok(None),
            Err(_) => continue,
        }
    }
    Err(LocalFallbackReason::CustodyUnavailable) // unresolved earlier class
}

/// Degraded turns are rare and must not be silent: a rising count means a
/// candidate is stuck unresolved on a CPU (its own liveness is still at risk).
/// One relaxed RMW, only on a turn that actually omitted a candidate.
static OMITTED_CANDIDATES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static LAST_OMITTED_CANDIDATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn record_omitted_candidate(slot: usize, reason: LocalFallbackReason) {
    let cpu = nucleus_core::util::lockdep::current_cpu_index() as u64;
    // ORDERING: Relaxed; a diagnostic window drained by the runtime profile.
    OMITTED_CANDIDATES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    LAST_OMITTED_CANDIDATE.store((cpu << 32) | ((reason as u64) << 16) | slot as u64,
        core::sync::atomic::Ordering::Relaxed);
}

/// `(omitted turns, (cpu << 32) | (reason << 16) | slot)` for this window.
pub(in crate::multitask::scheduler) fn take_omitted_candidate_window() -> Option<(u64, u64)> {
    let count = OMITTED_CANDIDATES.swap(0, core::sync::atomic::Ordering::Relaxed);
    (count != 0).then(|| (count, LAST_OMITTED_CANDIDATE.load(core::sync::atomic::Ordering::Relaxed)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn incomplete_early_policy_never_means_no_due_peer() {
        use super::{LocalFallbackReason, MAX_LOCAL_FAIR_OBSERVATIONS, retry_incomplete_policy};
        let mut attempts = 0;
        let unresolved = retry_incomplete_policy::<(usize, u64)>(|_| {
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
            retry_incomplete_policy(|_| {
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
        assert_eq!(retry_incomplete_policy::<(usize, u64)>(|_| Ok(None)).unwrap(), None);
    }

    /// A candidate unresolved in every strict observation is omitted by the
    /// last one only; a slot that resolves in between, or an epoch tear with no
    /// slot, keeps the whole set strict.
    #[test]
    fn only_a_candidate_unresolved_in_every_observation_is_omitted() {
        use super::{LocalFallbackReason, MAX_LOCAL_FAIR_OBSERVATIONS, retry_incomplete_policy};
        // Slot 9 is stuck; slot 4 is torn only in the first observation.
        let scan = |unresolved: &mut super::UnresolvedCandidates, attempt: usize| {
            let mut failed = false;
            for slot in [4, 9, 12] {
                if unresolved.excludes(slot) { continue; }
                if slot == 9 || (slot == 4 && attempt == 1) {
                    unresolved.record(slot, LocalFallbackReason::LifecycleIneligible);
                    failed = true;
                }
            }
            if failed { Err(LocalFallbackReason::LifecycleIneligible) } else { Ok(Some(12)) }
        };
        let mut attempts = 0;
        let choice = retry_incomplete_policy(|unresolved| { attempts += 1; scan(unresolved, attempts) });
        assert_eq!(choice.unwrap(), Some(12));
        assert_eq!(attempts, MAX_LOCAL_FAIR_OBSERVATIONS);

        let mut attempts = 0;
        let choice = retry_incomplete_policy::<usize>(|unresolved| {
            attempts += 1;
            // Unresolved only from the second observation on: never omitted.
            if attempts > 1 && !unresolved.excludes(5) {
                unresolved.record(5, LocalFallbackReason::FrameUnavailable);
                return Err(LocalFallbackReason::FrameUnavailable);
            }
            Err(LocalFallbackReason::CustodyUnavailable)
        });
        assert!(matches!(choice, Err(LocalFallbackReason::CustodyUnavailable)));
    }
}
