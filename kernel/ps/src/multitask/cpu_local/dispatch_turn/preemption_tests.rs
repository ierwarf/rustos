use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

#[test]
fn independent_preemption_stays_owned_across_directory_wait_not_after_raw_acquire() {
    let _serial = test_publication_lock();
    let cell = TurnCell::new();
    let directory = std::sync::Mutex::new(());
    let held = directory.lock().unwrap();
    let waiting = std::sync::Barrier::new(2);
    let cpu = 5;
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let preemption = DispatchPreemption::enter_model(cpu);
            let mut turn = cell.begin_phase_model(cpu, 115).unwrap();
            turn.preemption = Some(preemption);
            waiting.wait();
            let raw = directory.lock().unwrap();
            turn.seal();
            drop(raw);
            turn.finish(false);
        });
        waiting.wait();
        assert_eq!(cell.phase.load(Ordering::Acquire), OPEN);
        assert_eq!(DispatchPreemption::model_depth_on(cpu), 1);
        drop(held);
        waiter.join().unwrap();
    });
    assert_eq!(DispatchPreemption::model_depth_on(cpu), 0);
    assert_eq!(cell.phase.load(Ordering::Acquire), VACANT);
}
#[test]
fn completion_keeps_preemption_until_rsp_and_duplicate_cannot_spend_next_turn() {
    let _serial = test_publication_lock();
    let cpu = 5;
    let restore = super::super::test_support::install_test_current_owner(cpu, 115);
    let turn = begin_phase_model(DispatchPreemption::enter_model(cpu), 115);
    assert_eq!(DispatchPreemption::model_depth_on(cpu), 1);
    let completion = turn.publish_return(116, false);
    assert_eq!(test_phase(cpu), PUBLISHING);
    commit(cpu);
    assert_eq!(test_phase(cpu), PUBLISHING);
    assert_eq!(DispatchPreemption::model_depth_on(cpu), 1);
    completion.after_directory_unlock();
    assert_eq!(test_phase(cpu), AWAITING_RSP);
    assert_eq!(DispatchPreemption::model_depth_on(cpu), 1);
    restore.commit_assembly();
    assert_eq!(DispatchPreemption::model_depth_on(cpu), 0);
    let next = begin_phase_model(DispatchPreemption::enter_model(cpu), 116);
    commit(cpu);
    assert_eq!(test_phase(cpu), OPEN);
    assert_eq!(DispatchPreemption::model_depth_on(cpu), 1);
    next.publish_return(116, false).after_directory_unlock();
    assert_eq!(test_phase(cpu), VACANT);
    assert_eq!(DispatchPreemption::model_depth_on(cpu), 0);
}
#[test]
fn abandoned_completion_poison_preserves_published_current_and_releases_only_its_scope() {
    let _serial = test_publication_lock();
    let cpu = 5;
    let _restore = super::super::test_support::install_test_current_owner(cpu, 115);
    let completion = begin_phase_model(DispatchPreemption::enter_model(cpu), 115).publish_return(116, false);
    assert_eq!(CURRENT_TASK_SLOTS[cpu].load(Ordering::Acquire), 116);
    drop(completion);
    assert_eq!(test_phase(cpu), POISONED);
    assert_eq!(DispatchPreemption::model_depth_on(cpu), 0);
    commit(cpu);
    assert_eq!(test_phase(cpu), POISONED);
    assert!(
        catch_unwind(AssertUnwindSafe(|| begin_phase_model(
            DispatchPreemption::enter_model(cpu),
            116
        )))
        .is_err()
    );
    assert_eq!(DispatchPreemption::model_depth_on(cpu), 0);
}
#[test]
fn fixture_reset_acknowledges_parked_scope_rather_than_erasing_its_ownership() {
    let _serial = test_publication_lock();
    let cpu = 5;
    let _restore = super::super::test_support::install_test_current_owner(cpu, 115);
    begin_phase_model(DispatchPreemption::enter_model(cpu), 115)
        .publish_return(116, false)
        .after_directory_unlock();
    assert_eq!(DispatchPreemption::model_depth_on(cpu), 1);
    test_restore_phase(cpu, VACANT);
    assert_eq!(DispatchPreemption::model_depth_on(cpu), 0);
    assert_eq!(test_phase(cpu), VACANT);
}
