use super::{CleanupCell, CleanupState};
use crate::multitask::RetiredTaskCleanup;

fn token(task_id: u64) -> RetiredTaskCleanup {
    RetiredTaskCleanup {
        task_id,
        process_id: 71,
        process_terminal: false,
        user_fault_vector: None,
        clear_child_tid: 0x1000,
        robust_list_head: 0x2000,
        robust_list_len: 24,
    }
}

#[test]
fn cleanup_requires_exact_claim_and_ack_before_slot_reset_or_reuse() {
    let cell = CleanupCell::new();
    let old = token(19);
    cell.publish(old);
    assert!(cell.pending());
    assert!(!cell.complete(old));
    assert_eq!(cell.claim(), Some(old));
    assert_eq!(cell.claim(), None);
    assert!(cell.pending());
    for wrong in [
        token(20),
        RetiredTaskCleanup {
            process_id: 72,
            ..old
        },
        RetiredTaskCleanup {
            process_terminal: true,
            ..old
        },
        RetiredTaskCleanup {
            user_fault_vector: Some(14),
            ..old
        },
        RetiredTaskCleanup {
            clear_child_tid: 0x1008,
            ..old
        },
        RetiredTaskCleanup {
            robust_list_head: 0x3000,
            ..old
        },
        RetiredTaskCleanup {
            robust_list_len: 32,
            ..old
        },
    ] {
        assert!(!cell.complete(wrong));
        assert_eq!(cell.state.lock().cleanup, CleanupState::Claimed(old));
        assert!(cell.pending());
    }
    assert!(cell.complete(old));
    assert!(!cell.complete(old));
    assert!(!cell.pending());
    cell.reset();
    let fresh = token(20);
    cell.publish(fresh);
    assert_eq!(cell.claim(), Some(fresh));
    assert!(!cell.complete(old));
    assert!(cell.pending());
    assert!(cell.complete(fresh));
    cell.reset();
    assert_eq!(cell.state.lock().cleanup, CleanupState::Vacant);
}

#[test]
#[should_panic(expected = "slot cleanup reset before exact external ACK")]
fn pending_cleanup_cannot_be_reset_before_claim() {
    let cell = CleanupCell::new();
    cell.publish(token(19));
    cell.reset();
}

#[test]
#[should_panic(expected = "slot cleanup reset before exact external ACK")]
fn claimed_cleanup_cannot_be_reset_before_ack() {
    let cell = CleanupCell::new();
    cell.publish(token(19));
    assert!(cell.claim().is_some());
    cell.reset();
}

#[test]
#[should_panic(expected = "cleanup published before prior slot reset")]
fn acknowledged_cleanup_must_be_reset_before_new_publication() {
    let cell = CleanupCell::new();
    let old = token(19);
    cell.publish(old);
    assert_eq!(cell.claim(), Some(old));
    assert!(cell.complete(old));
    cell.publish(token(20));
}

#[test]
fn concurrent_consumers_claim_exactly_once_without_directory_serialization() {
    use std::{sync::Arc, thread};
    let cell = Arc::new(CleanupCell::new());
    let cleanup = token(19);
    cell.publish(cleanup);
    let peers: std::vec::Vec<_> = (0..8)
        .map(|_| {
            let cell = cell.clone();
            thread::spawn(move || cell.claim())
        })
        .collect();
    let claimed: std::vec::Vec<_> = peers
        .into_iter()
        .filter_map(|peer| peer.join().unwrap())
        .collect();
    assert_eq!(claimed, [cleanup]);
    assert!(cell.complete(cleanup));
    assert!(!cell.pending());
}

#[test]
fn kernel_claim_is_unique_and_pins_slot_until_its_own_ack() {
    use super::{KernelState, RetirementSideEffect, TaskRetireReason};
    let cell = CleanupCell::new();
    let cleanup = token(31);
    let effects = RetirementSideEffect::new(Some(31), None);
    cell.publish_terminal(Some(31), TaskRetireReason::Exited, Some(cleanup), effects);
    assert_eq!(cell.claim(), Some(cleanup));
    assert!(cell.complete(cleanup));
    assert!(!cell.pending());
    let (generation, claimed) = cell.claim_kernel().unwrap();
    assert_eq!(claimed, effects);
    assert_eq!(cell.claim_kernel(), None);
    assert!(cell.kernel_pending());
    assert_eq!(cell.state.lock().kernel, KernelState::Claimed(effects));
    assert!(!cell.complete_kernel(generation, RetirementSideEffect::new(Some(32), None)));
    assert!(cell.kernel_pending());
    assert!(cell.complete_kernel(generation, effects));
    assert!(!cell.complete_kernel(generation, effects));
    assert!(!cell.kernel_pending());
    cell.reset();
}

#[test]
fn old_kernel_claim_cannot_ack_an_identical_payload_after_slot_reuse() {
    use super::{RetirementSideEffect, TaskRetireReason};
    let cell = CleanupCell::new();
    let effects = RetirementSideEffect::new(None, None);
    cell.publish_terminal(None, TaskRetireReason::Exited, None, effects);
    let (old, _) = cell.claim_kernel().unwrap();
    assert!(cell.complete_kernel(old, effects));
    cell.reset();
    cell.publish_terminal(None, TaskRetireReason::Exited, None, effects);
    let (fresh, _) = cell.claim_kernel().unwrap();
    assert_ne!(old, fresh);
    assert!(!cell.complete_kernel(old, effects));
    assert!(cell.kernel_pending());
    assert!(cell.complete_kernel(fresh, effects));
    cell.reset();
}

#[test]
#[should_panic(expected = "slot reset before kernel retirement ACK")]
fn external_cleanup_ack_does_not_release_a_claimed_kernel_revocation() {
    use super::{RetirementSideEffect, TaskRetireReason};
    let cell = CleanupCell::new();
    let cleanup = token(31);
    cell.publish_terminal(
        Some(31),
        TaskRetireReason::Exited,
        Some(cleanup),
        RetirementSideEffect::new(Some(31), None),
    );
    assert_eq!(cell.claim(), Some(cleanup));
    assert!(cell.complete(cleanup));
    assert!(cell.claim_kernel().is_some());
    cell.reset();
}

#[test]
#[should_panic(expected = "slot reset while retirement producer owns metadata")]
fn taken_deferred_intent_remains_pinned_until_terminal_publication() {
    use super::TaskRetireReason;
    let cell = CleanupCell::new();
    cell.defer(Some(31), TaskRetireReason::Exited);
    assert!(cell.kernel_pending());
    assert!(cell.take_deferred().is_some());
    assert!(cell.take_deferred().is_none());
    assert!(cell.claim().is_none());
    assert!(cell.claim_kernel().is_none());
    assert!(cell.kernel_pending());
    cell.reset();
}

#[test]
fn terminal_producer_atomically_freezes_both_tokens_after_deferred_intent() {
    use super::{RetirementSideEffect, TaskRetireReason};
    let cell = CleanupCell::new();
    let cleanup = token(31);
    cell.defer(Some(31), TaskRetireReason::Exited);
    assert!(cell.take_deferred().is_some());
    let effects = RetirementSideEffect::new(Some(31), Some(cleanup.process_id));
    cell.publish_terminal(Some(31), TaskRetireReason::Exited, Some(cleanup), effects);
    assert!(cell.state.lock().deferred.is_none());
    assert!(!cell.state.lock().producing);
    assert_eq!(cell.claim(), Some(cleanup));
    let (generation, payload) = cell.claim_kernel().unwrap();
    assert_eq!(payload, effects);
    assert!(cell.complete_kernel(generation, effects));
    assert!(cell.pending());
    assert!(cell.complete(cleanup));
    cell.reset();
}
