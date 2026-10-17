//! Native phase concurrency, not IRQ/RSP/hardware completion evidence.
use super::*;
use std::boxed::Box;

#[test]
fn publication_waits_for_the_original_choice_release_not_a_scalar_observation() {
    let phase = Phase::new();
    phase.admit(CHOOSING);
    let entered = core::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|threads| {
        let worker = threads.spawn(|| {
            phase.admit(PUBLISHING);
            entered.store(true, Ordering::Release);
            phase.0.store(VACANT, Ordering::Release);
        });
        assert_eq!(phase.0.load(Ordering::Acquire), CHOOSING);
        assert!(!entered.load(Ordering::Acquire));
        phase.0.store(VACANT, Ordering::Release);
        worker.join().unwrap();
    });
    assert!(entered.load(Ordering::Acquire));
    assert_eq!(phase.0.load(Ordering::Acquire), VACANT);
}

#[test]
fn an_abandoned_original_choice_poison_cannot_admit_a_priority_publication() {
    let phase = Box::leak(Box::new(Phase::new()));
    phase.admit(CHOOSING);
    drop(Choice { phase, live: true, affinity: PhantomData });
    assert_eq!(phase.0.load(Ordering::Acquire), POISONED);
    assert!(std::panic::catch_unwind(|| phase.admit(PUBLISHING)).is_err());
}

#[test]
fn actual_publication_borrows_original_masked_scope_through_release() {
    let _serial = super::super::execution_test_serial_guard();
    let mut scope = DispatchPreemption::enter_model_with_enabled_irqs(0);
    reset_model(1); // earlier modeled fixture ACK, not production recovery
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _publication = Publication::begin(&scope, 1);
    })).is_err());
    assert_eq!(PHASES[1].0.load(Ordering::Acquire), VACANT);
    scope.mask_irqs();
    // Dependency lockdep raw-depth tracking is disabled in this host lane;
    // this is NOT a production IRQ/raw-leaf exclusion witness. The original
    // masked-scope loan and actual publication phase below are exercised.
    {
        let publication = Publication::begin(&scope, 1);
        assert!(core::ptr::eq(publication.scope, &scope));
        assert!(publication.scope.is_masked());
        assert_eq!(PHASES[1].0.load(Ordering::Acquire), PUBLISHING);
        // The borrowed original scope cannot be consumed while publication
        // exists. Dropping the phase is NOT completion/ACK of that CPU owner.
        drop(publication);
    }
    assert_eq!(PHASES[1].0.load(Ordering::Acquire), VACANT);
    assert!(scope.is_masked());
}

#[test]
fn incomplete_fifo_observation_retains_exact_prefix_and_capacity() {
    use super::super::handoff_queue::{FirstTurnHandoff, SlotHandoffQueue};
    let mut queue: SlotHandoffQueue<2, FirstTurnHandoff> = SlotHandoffQueue::new();
    let first = FirstTurnHandoff { slot: 20, task_id: 120 };
    let second = FirstTurnHandoff { slot: 21, task_id: 121 };
    assert_eq!(queue.enqueue(first), Ok(true));
    assert_eq!(queue.enqueue(second), Ok(true));
    for _ in 0..8 {
        assert_eq!(queue.front(), Some(first));
        assert_eq!(queue.len(), 2);
    }
    assert_eq!(queue.pop(), Some(first));
    assert_eq!(queue.front(), Some(second));
    assert_eq!(queue.pop(), Some(second));
    assert_eq!(queue.front(), None);
}
