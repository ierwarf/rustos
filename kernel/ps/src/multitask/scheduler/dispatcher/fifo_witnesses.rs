//! Owner: SAME production FIFO/priority consumers over original born resources.
//! Boundary: explicit earlier FIFO/IRQ inputs are not activation or IPC grants.
//! Lifecycle: exact published UID/frame/funding -> physical claim; lower queues retained.
//! Failure: incomplete prefix retains ordering, stale UID drains without mutation.
//! Evidence: historical IDs; native bank completion is NOT architecture/RSP ACK.
use super::*;
use super::super::{dispatch_claim, execution_interval, local_dispatch::policy_witnesses};
const ID: u64 = 82800;

fn enqueue(slot: usize, id: u64, atomic: bool) {
    let mut policy = dispatch_policy::lock(0);
    let record = FirstTurnHandoff { slot, task_id: id };
    if atomic {
        assert!(policy.atomic_activation_pick_hints.enqueue(record).unwrap());
        policy.atomic_activation_handoff_remaining += 1;
        dispatch_policy::publish_atomic_activation_pending(0);
    } else { assert!(policy.spawn_pick_hints.enqueue(record).unwrap()); }
}
fn empty(atomic: bool) -> bool {
    let policy = dispatch_policy::lock(0);
    if atomic { policy.atomic_activation_pick_hints.is_empty() }
    else { policy.spawn_pick_hints.is_empty() }
}

pub(in crate::multitask::scheduler) fn first_turn_fifo() {
    for atomic in [true, false] {
        dispatch_claim::tests::fixture(false, |slot, turn| {
            turn.begin_priority_choice();
            // A genuinely born slot has a DIFFERENT exact published identity.
            // Reject and consume its stale ordering token before any capture.
            enqueue(slot, ID + 1, atomic);
            assert!(matches!(first_turn(turn, 0, atomic, donation_ledger::class_publication_sequence()), Decision::Empty));
            assert!(empty(atomic));
            assert!(turn.claimed_return().is_none());
            assert_eq!(current_identity::read_dispatch(slot).unwrap().task_id, Some(ID));
            enqueue(slot, ID, atomic);
            {
                let mut policy = dispatch_policy::lock(0);
                let record = FirstTurnHandoff { slot, task_id: ID };
                let inserted = if atomic { policy.atomic_activation_pick_hints.enqueue(record).unwrap() }
                    else { policy.spawn_pick_hints.enqueue(record).unwrap() };
                assert!(!inserted, "exact duplicate preserves FIFO position");
            }
            // Explicit later unknown member input: only the complete front may
            // claim; the incomplete tail must survive the same physical claim.
            enqueue(MAX_TASK - 1, ID + 2, atomic);
            assert!(sync_handoff::enqueue(0, sync_handoff::SyncHandoffRecord::new(slot, ID)));
            let decision = if atomic {
                // SAME production priority chain, not a parallel test ordering.
                choose(turn, 0, 121, false, 0, crate::arch::rtc::ticks(), donation_ledger::class_publication_sequence())
            } else { first_turn(turn, 0, false, donation_ledger::class_publication_sequence()) };
            assert!(matches!(decision, Decision::Claimed(urgency) if matches!((atomic, urgency), (true, Urgency::Atomic) | (false, Urgency::Spawn))));
            assert_eq!(turn.claimed_return().unwrap().id, ID);
            assert!(execution_interval::original_claim_is_active(turn));
            // Completion also runs after the FIFO arm's own successful claim;
            // it must not pop/decrement the following exact place a second time.
            dispatch_policy::discharge_claimed_first_turn(turn);
            assert!(!empty(atomic), "claim cannot consume the following FIFO place");
            assert_eq!(sync_handoff::model_len(0), 1, "first-turn claim retains lower IPC custody");
            if atomic {
                assert!(dispatch_policy::atomic_activation_pending(0));
                assert_eq!(dispatch_policy::lock(0).atomic_activation_handoff_remaining, 1);
            }
            dispatch_claim::tests::finish_modeled_preparation(slot, 121);
            dispatch_policy::remove_reclaimed_slot(MAX_TASK - 1);
            sync_handoff::remove_slot_all_cpus(slot);
        });
    }
}

pub(in crate::multitask::scheduler) fn incomplete_first_turn() {
    for atomic in [true, false] {
        dispatch_claim::tests::fixture(false, |_slot, turn| {
            turn.begin_priority_choice();
            let unknown = MAX_TASK - 1;
            assert!(current_identity::read_dispatch(unknown).is_none());
            assert!(matches!(local_candidate(unknown, 0, Urgency::Spawn), Decision::Incomplete));
            enqueue(unknown, ID + 2, atomic);
            assert!(matches!(first_turn(turn, 0, atomic, donation_ledger::class_publication_sequence()), Decision::Incomplete));
            assert!(!empty(atomic));
            assert!(turn.claimed_return().is_none());
            if atomic { assert!(dispatch_policy::atomic_activation_pending(0)); }
            dispatch_policy::remove_reclaimed_slot(unknown);
        });
    }
}

pub(in crate::multitask::scheduler) fn reader_exclusion() {
    super::tests::actual_incomplete_first_turn_retains_fifo_and_pending_while_original_reader_excludes_claim();
}

pub(in crate::multitask::scheduler) fn synchronous_fifo() {
    policy_witnesses::family(|root, first, second| {
        let id = |slot| current_identity::read_dispatch(slot).unwrap().task_id.unwrap();
        let record = |slot| sync_handoff::SyncHandoffRecord::new(slot, id(slot));
        let take = || match synchronous(0) {
            Decision::Selected(candidate) => Some((candidate.slot, candidate.id)),
            Decision::Empty => None,
            _ => panic!("complete born FIFO input lost its readiness"),
        };
        assert!(sync_handoff::enqueue(0, record(first)));
        assert!(sync_handoff::enqueue(0, record(first)));
        assert!(sync_handoff::enqueue(0, record(second)));
        assert_eq!(sync_handoff::model_len(0), 2);
        assert_eq!(take(), Some((first, id(first))));
        assert_eq!(take(), Some((second, id(second))));
        assert!(!sync_handoff::pending(0));
        assert!(sync_handoff::enqueue(0, record(first)));
        assert!(sync_handoff::enqueue(0, record(second)));
        // Charge the SAME actual local execution-chain clock transition; a
        // repeated timestamp costs no extra quantum, elapsed quanta are capped.
        sync_handoff::record_dispatch(0, true, 1);
        sync_handoff::record_dispatch(0, true, 1);
        sync_handoff::record_dispatch(0, true, 1 + super::super::MAX_SYNC_HANDOFF_CHAIN_TICKS as u64);
        assert_eq!(take(), None);
        assert_eq!(sync_handoff::model_len(0), 2, "fairness refusal retains both records");
        assert!(sync_handoff::pending(0));
        sync_handoff::record_dispatch(0, false, 1);
        assert_eq!(take(), Some((first, id(first))));
        sync_handoff::remove_slot_all_cpus(second);
        assert_eq!(take(), None, "exact retired withdrawal leaves no stale turn");
        assert!(!sync_handoff::pending(0));
        let owner = runqueue::owner(first);
        assert!(sync_handoff::enqueue(0, sync_handoff::SyncHandoffRecord::new(first, id(first) + 99)));
        assert_eq!(take(), None, "reused UID cannot inherit synchronous urgency");
        assert_eq!(runqueue::owner(first), owner);
        assert!(stack_resources::cpu_leases::owns_current(0, root));
    });
}
