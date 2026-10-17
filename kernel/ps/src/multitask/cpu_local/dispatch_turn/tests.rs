use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

#[test]
fn unique_turn_refuses_overlap_and_commit_cannot_release_a_live_producer() {
    let cell = TurnCell::new();
    let turn = cell.begin_phase_model(0, 1).unwrap();
    assert!(cell.begin_phase_model(0, 1).is_none());
    cell.commit();
    assert_eq!(cell.phase.load(Ordering::Acquire), OPEN);
    turn.seal();
    cell.commit();
    assert_eq!(cell.phase.load(Ordering::Acquire), PUBLISHING);
    turn.finish(false);
    assert_eq!(cell.phase.load(Ordering::Acquire), VACANT);
    assert!(cell.begin_phase_model(0, 1).is_some());
}

#[test]
fn rsp_consumer_is_admitted_before_bank_ack_and_stays_closed_until_final_release() {
    let cell = TurnCell::new();
    let turn = cell.begin_phase_model(0, 1).unwrap();
    turn.seal();
    turn.finish(true);
    let ack = cell.begin_commit().unwrap();
    assert_eq!(cell.phase.load(Ordering::Acquire), COMMITTING);
    assert!(cell.begin_commit().is_none());
    assert!(cell.begin_phase_model(0, 2).is_none());
    ack.finish();
    assert_eq!(cell.phase.load(Ordering::Acquire), VACANT);
    let next = cell.begin_phase_model(0, 2).unwrap();
    assert!(cell.begin_commit().is_none());
    next.seal();
    next.finish(false);
}
#[test]
fn abandoned_admitted_rsp_consumer_quarantines_instead_of_reopening() {
    let cell = TurnCell::new();
    let turn = cell.begin_phase_model(0, 1).unwrap();
    turn.seal();
    turn.finish(true);
    drop(cell.begin_commit().unwrap());
    assert_eq!(cell.phase.load(Ordering::Acquire), POISONED);
    assert!(cell.begin_commit().is_none());
    assert!(cell.begin_phase_model(0, 2).is_none());
}
#[test]
fn assembly_ack_refusal_cannot_clear_a_new_open_producers_transition() {
    let _serial = test_publication_lock();
    let restore = super::super::test_support::install_test_current_owner(0, 115);
    let turn = begin_phase_model(DispatchPreemption::enter_model(0), 115);
    TRANSITION_ACTIVE[0].store(true, Ordering::Release);
    super::super::commit_context_switch();
    assert!(TRANSITION_ACTIVE[0].load(Ordering::Acquire));
    assert_eq!(test_phase(0), OPEN);
    TRANSITION_ACTIVE[0].store(false, Ordering::Release);
    turn.publish_return(115, false).after_directory_unlock();
    drop(restore);
}
#[test]
fn switched_return_stays_closed_until_rsp_commit_and_duplicate_cannot_spend_next_turn() {
    let cell = TurnCell::new();
    let turn = cell.begin_phase_model(0, 1).unwrap();
    turn.seal();
    turn.finish(true);
    assert_eq!(cell.phase.load(Ordering::Acquire), AWAITING_RSP);
    assert!(cell.begin_phase_model(0, 2).is_none());
    cell.commit();
    let next = cell.begin_phase_model(0, 2).unwrap();
    cell.commit();
    assert_eq!(cell.phase.load(Ordering::Acquire), OPEN);
    next.seal();
    next.finish(false);
}
#[test]
fn abandoned_or_partial_publication_poison_cannot_be_reopened_by_commit() {
    for sealed in [false, true] {
        let cell = TurnCell::new();
        let turn = cell.begin_phase_model(0, 1).unwrap();
        if sealed {
            turn.seal();
        }
        drop(turn);
        assert_eq!(cell.phase.load(Ordering::Acquire), POISONED);
        cell.commit();
        assert!(cell.begin_phase_model(0, 1).is_none());
    }
}
#[test]
fn concurrent_cpu_entry_has_one_unique_winner_not_a_global_writer_hint() {
    let cell = TurnCell::new();
    let start = std::sync::Barrier::new(3);
    let observed = std::sync::Barrier::new(3);
    let wins = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                start.wait();
                let turn = cell.begin_phase_model(0, 1);
                if turn.is_some() {
                    wins.fetch_add(1, Ordering::Relaxed);
                }
                observed.wait(); // winner cannot reopen before loser observed
                if let Some(turn) = turn {
                    turn.seal();
                    turn.finish(false);
                }
            });
        }
        start.wait();
        observed.wait();
    });
    assert_eq!(wins.load(Ordering::Acquire), 1);
    assert_eq!(cell.phase.load(Ordering::Acquire), VACANT);
}
#[test]
fn production_return_publishes_under_local_lease_and_waits_for_assembly_ack() {
    let _serial = test_publication_lock();
    let cpu = 5;
    let restore = super::super::test_support::install_test_current_owner(cpu, 115);
    let turn = begin_phase_model(DispatchPreemption::enter_model(cpu), 115);
    turn.publish_return(116, true).after_directory_unlock();
    assert_eq!(CURRENT_TASK_SLOTS[cpu].load(Ordering::Acquire), 116);
    assert_eq!(TRANSITION_FROM_SLOTS[cpu].load(Ordering::Acquire), 115);
    assert!(TRANSITION_ACTIVE[cpu].load(Ordering::Acquire));
    assert!(CURRENT_TASK_IDLE[cpu].load(Ordering::Acquire));
    assert_eq!(test_phase(cpu), AWAITING_RSP);
    assert!(
        catch_unwind(AssertUnwindSafe(|| begin_phase_model(
            DispatchPreemption::enter_model(cpu),
            116
        )))
        .is_err()
    );
    restore.commit_assembly();
    assert_eq!(test_phase(cpu), VACANT);
    let next = begin_phase_model(DispatchPreemption::enter_model(cpu), 116);
    next.publish_return(116, true).after_directory_unlock();
    assert_eq!(test_phase(cpu), VACANT);
}
#[test]
fn return_lease_refuses_replaced_current_and_remote_custody_before_publication() {
    let _serial = test_publication_lock();
    let cpu = 5;
    let _restore = super::super::test_support::install_test_current_owner(cpu, 115);
    let _remote = super::super::test_support::install_test_current_owner(6, 117);
    let original = begin_phase_model(DispatchPreemption::enter_model(cpu), 115);
    CURRENT_TASK_SLOTS[cpu].store(116, Ordering::Release);
    assert!(catch_unwind(AssertUnwindSafe(|| original.publish_return(118, false))).is_err());
    assert_eq!(CURRENT_TASK_SLOTS[cpu].load(Ordering::Acquire), 116);
    // Synthetic fixture restoration, not production recovery from poison.
    test_restore_phase(cpu, VACANT);
    let local = begin_phase_model(DispatchPreemption::enter_model(cpu), 116);
    assert!(catch_unwind(AssertUnwindSafe(|| local.publish_return(117, false))).is_err());
    assert_eq!(CURRENT_TASK_SLOTS[cpu].load(Ordering::Acquire), 116);
    assert_eq!(test_phase(cpu), POISONED);
}
