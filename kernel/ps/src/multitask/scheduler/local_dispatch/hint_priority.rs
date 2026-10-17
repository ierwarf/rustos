//! Published strict-class veto for an ordinary non-FIFO pick hint.
//!
//! An ordering hint cannot let a User continuation pass an eligible System
//! peer. Incomplete queue/class, budget, affinity or frame evidence does not
//! prove absence: the hint waits while ordinary fair custody remains intact.

use super::LocalFallbackReason;
use super::{
    LocalClass, candidate_is_qualified, observed_class, current_can_keep_running, donation_ledger, runqueue,
};

/// A latency exception belongs only to a stable published User class. If a
/// donation/class writer overlaps consumption, leave this one-shot hint
/// behind; the Local queue retains normal fair custody.
pub(in crate::multitask::scheduler) fn stable_user_hint_class(slot: usize) -> bool {
    let before = donation_ledger::class_publication_sequence();
    if before & 1 != 0 {
        return false;
    }
    published_user_hint_class(before, observed_class(slot) == Ok(LocalClass::User),
        donation_ledger::class_publication_sequence(),
    )
}

fn published_user_hint_class(before: u64, user: bool, after: u64) -> bool {
    before & 1 == 0 && before == after && user
}

pub(in crate::multitask::scheduler) fn user_hint_blocked_by_system(
    hint_slot: usize,
    current_slot: usize,
    cpu: usize,
    now_ns: u64,
) -> Result<bool, LocalFallbackReason> {
    let queue_epoch = runqueue::local_runnable_sequence(cpu);
    let class_epoch = donation_ledger::class_publication_sequence();
    if queue_epoch & 1 != 0 || class_epoch & 1 != 0 {
        return Err(LocalFallbackReason::CustodyUnavailable);
    }
    let hinted_user = observed_class(hint_slot)? == LocalClass::User;
    let mut system_ready = false;
    if hinted_user {
        for peer in runqueue::local_runnable_slots(cpu) {
            if runqueue::is_idle_slot(peer) { continue; }
            if observed_class(peer)? != LocalClass::System {
                continue;
            }
            let qualified = if peer == current_slot && current_can_keep_running(peer, cpu, now_ns) {
                Ok(Some(runqueue::owner(peer).generation))
            } else {
                candidate_is_qualified(peer, cpu, now_ns)
            };
            match qualified {
                Ok(Some(_)) => {
                    system_ready = true;
                    break;
                }
                Ok(None) => continue,
                Err(reason) => return Err(reason),
            }
        }
    }
    if runqueue::local_runnable_sequence(cpu) != queue_epoch
        || donation_ledger::class_publication_sequence() != class_epoch
    {
        return Err(LocalFallbackReason::CustodyUnavailable);
    }
    Ok(hinted_user && system_ready)
}

/// Unknown System presence cannot authorize a User hint. A complete negative
/// observation can release it; any qualified System owner retains priority.
pub(in crate::multitask::scheduler) fn refuse_unproven_hint_priority(
    observation: Result<bool, LocalFallbackReason>,
) -> bool {
    observation.unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::{LocalFallbackReason, refuse_unproven_hint_priority};

    #[test]
    fn incomplete_system_presence_cannot_release_a_user_hint() {
        assert!(refuse_unproven_hint_priority(Ok(true)));
        assert!(!refuse_unproven_hint_priority(Ok(false)));
        assert!(refuse_unproven_hint_priority(Err(
            LocalFallbackReason::CustodyUnavailable
        )));
        assert!(refuse_unproven_hint_priority(Err(
            LocalFallbackReason::FrameUnavailable
        )));
    }

    #[test]
    fn latency_class_requires_a_stable_user_publication() {
        use super::published_user_hint_class;
        assert!(published_user_hint_class(2, true, 2));
        assert!(!published_user_hint_class(3, true, 3));
        assert!(!published_user_hint_class(2, true, 4));
        assert!(!published_user_hint_class(2, false, 2));
    }
}
