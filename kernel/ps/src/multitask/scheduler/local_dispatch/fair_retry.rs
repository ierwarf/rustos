//! Bounded CPU-local re-observation after a torn fair publication.
//!
//! An incomplete snapshot is never authoritative. Each attempt obtains new
//! queue/class epochs; only one complete candidate set (minus candidates
//! unresolved through every earlier attempt, see `UnresolvedCandidates`) may
//! grant a generation-bound turn. Exhaustion defers to the per-CPU
//! idle/current fallback.

use super::LocalFallbackReason;
use crate::multitask::irq_pool::{SlabPool, SlabQueue};

const MAX_LOCAL_FAIR_OBSERVATIONS: usize = 4;
const INLINE_UNRESOLVED_CAP: usize = 16;

static OVERFLOW_POOL: SlabPool<usize> = SlabPool::new();

pub(in crate::multitask::scheduler) fn preallocate_pool(min_free: usize) {
    OVERFLOW_POOL.preallocate_reserve(min_free);
}

struct SlotSet {
    inline: [usize; INLINE_UNRESOLVED_CAP],
    inline_len: usize,
    overflow: SlabQueue<usize>,
    overflowed: bool,
}

impl SlotSet {
    const fn new() -> Self {
        Self {
            inline: [0; INLINE_UNRESOLVED_CAP],
            inline_len: 0,
            overflow: SlabQueue::new(),
            overflowed: false,
        }
    }

    fn clear(&mut self) {
        self.inline_len = 0;
        self.overflow.clear(&OVERFLOW_POOL);
        self.overflowed = false;
    }

    fn contains(&self, slot: usize) -> bool {
        if self.inline[..self.inline_len].contains(&slot) {
            return true;
        }
        if !self.overflow.is_empty() && self.overflow.any(&OVERFLOW_POOL, |&s| s == slot) {
            return true;
        }
        self.overflowed
    }

    fn insert(&mut self, slot: usize) {
        if self.contains(slot) {
            return;
        }
        if self.inline_len < INLINE_UNRESOLVED_CAP {
            self.inline[self.inline_len] = slot;
            self.inline_len += 1;
        } else if !self.overflow.push_back(&OVERFLOW_POOL, slot) {
            self.overflowed = true;
        }
    }
}

impl Drop for SlotSet {
    fn drop(&mut self) {
        self.overflow.clear(&OVERFLOW_POOL);
    }
}

/// Candidates whose own admission stayed unresolved through every strict
/// observation of one dispatch turn.
pub(in crate::multitask::scheduler) struct UnresolvedCandidates {
    observation: usize,
    current: SlotSet,
    persistent: SlotSet,
    last_reason: Option<LocalFallbackReason>,
    omitted: Option<(usize, LocalFallbackReason)>,
}

impl UnresolvedCandidates {
    /// One strict observation that never omits a candidate.
    pub(in crate::multitask::scheduler) const fn strict() -> Self {
        Self {
            observation: 0,
            current: SlotSet::new(),
            persistent: SlotSet::new(),
            last_reason: None,
            omitted: None,
        }
    }

    fn begin_observation(&mut self) {
        if self.observation == 1 {
            core::mem::swap(&mut self.persistent, &mut self.current);
        } else if self.observation > 1 {
            let mut next = SlotSet::new();
            for &slot in &self.persistent.inline[..self.persistent.inline_len] {
                if self.current.contains(slot) {
                    next.insert(slot);
                }
            }
            self.persistent.overflow.for_each(&OVERFLOW_POOL, |&slot| {
                if self.current.contains(slot) {
                    next.insert(slot);
                }
            });
            self.persistent = next;
        }
        self.current.clear();
        self.observation += 1;
    }

    /// `slot` could not be resolved in this observation for `reason`.
    pub(in crate::multitask::scheduler) fn record(&mut self, slot: usize, reason: LocalFallbackReason) {
        self.current.insert(slot);
        self.last_reason = Some(reason);
    }

    /// Whether the scan must leave `slot` out of this (final) observation: it
    /// was unresolved in every earlier one. Checked before the slot is
    /// evaluated, so a slot that admits but then fails its final qualification
    /// is left out as well.
    pub(in crate::multitask::scheduler) fn excludes(&mut self, slot: usize) -> bool {
        let exclude = self.observation == MAX_LOCAL_FAIR_OBSERVATIONS
            && self.persistent.contains(slot);
        if exclude {
            // Keep the CPU's evidence for the omitted slot's own liveness.
            self.current.insert(slot);
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
    LAST_OMITTED_CANDIDATE.store(
        (cpu << 40) | ((reason as u64 & 0xff) << 32) | (slot as u64 & 0xffff_ffff),
        core::sync::atomic::Ordering::Relaxed,
    );
}

/// `(omitted turns, (cpu << 40) | (reason << 32) | slot)` for this window.
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
