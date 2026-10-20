//! Owner: real terminal row producer/consumer/reset custody witnesses.
//! Boundary: UID/epoch/effects admission only; no physical CPU/MM or process-last-live proof.
//! Lifecycle: exact pre-effects rollback, retained deferred loan, producer-last dual publication.
//! Concurrency: shared runtime fixture lock; native selection remains parallel.
//! Failure: stale or abandoned effect keys never reopen or publish next generation.
//! Forbidden: injected row phases/counts are predicate isolation, not production recovery.
//! Evidence: actual Producer, CleanupCell consumers and physical reuse ResetSeal methods.
use super::*;
const SLOT: usize = 107;
const ID: u64 = 107001;
fn fixture(f: impl FnOnce()) {
    let _serial = crate::multitask::scheduler::execution_test_serial_guard();
    *CELLS[SLOT].state.lock() = SlotState::new();
    f();
    *CELLS[SLOT].state.lock() = SlotState::new();
}
fn begin() -> Producer {
    Producer::begin(SLOT, Some(ID), TaskRetireReason::Exited, true).unwrap()
}
fn payload() -> RetiredTaskCleanup {
    RetiredTaskCleanup {
        task_id: ID,
        process_id: ID,
        process_terminal: true,
        user_fault_vector: None,
        clear_child_tid: 0,
        robust_list_head: 0,
        robust_list_len: 0,
    }
}
fn effects() -> RetirementSideEffect {
    RetirementSideEffect::new(Some(ID), Some(ID))
}
#[test]
fn producer_reserves_before_effects_excludes_duplicates_consumers_and_reset_without_reclaiming_uid()
{
    fixture(|| {
        let owner = begin();
        assert!(Producer::begin(SLOT, Some(ID), TaskRetireReason::Exited, false).is_none());
        assert!(Producer::claim(SLOT, Some(ID)).is_none());
        assert!(CELLS[SLOT].claim().is_none());
        assert!(CELLS[SLOT].claim_kernel().is_none());
        assert!(reuse::begin(SLOT, Some(ID)).is_none());
        assert!(CELLS[SLOT].kernel_pending());
        drop(owner);
        assert!(Producer::begin(SLOT, Some(ID), TaskRetireReason::Exited, false).is_some());
    });
}
#[test]
fn payload_visibility_is_one_last_producer_operation_and_both_exact_consumer_acks_precede_reuse() {
    fixture(|| {
        let mut owner = begin();
        owner.begin_effects();
        assert!(CELLS[SLOT].claim().is_none());
        assert!(CELLS[SLOT].claim_kernel().is_none());
        owner.publish(Some(payload()), effects());
        let external = CELLS[SLOT].claim().unwrap();
        let (generation, kernel) = CELLS[SLOT].claim_kernel().unwrap();
        assert!(reuse::begin(SLOT, Some(ID)).is_none());
        assert!(CELLS[SLOT].complete(external));
        assert!(reuse::begin(SLOT, Some(ID)).is_none());
        CELLS[SLOT].complete_kernel(generation, kernel);
        let mut seal = reuse::begin(SLOT, Some(ID)).unwrap();
        seal.mark_effects();
        seal.reset();
        seal.finish();
        assert!(Producer::begin(SLOT, Some(ID + 1), TaskRetireReason::Exited, false).is_some());
    });
}
#[test]
fn deferred_loan_keeps_original_reason_detach_and_uid_and_two_claimants_cannot_spend_one_effect_owner()
 {
    fixture(|| {
        let mut owner = begin();
        owner.begin_effects();
        owner.defer();
        assert!(Producer::claim(SLOT, Some(ID + 1)).is_none());
        let claim = Producer::claim(SLOT, Some(ID)).unwrap();
        assert!(claim.detach());
        assert_eq!(claim.reason(), TaskRetireReason::Exited);
        assert!(Producer::claim(SLOT, Some(ID)).is_none());
        assert!(reuse::begin(SLOT, Some(ID)).is_none());
        drop(claim);
        let mut next = Producer::claim(SLOT, Some(ID)).unwrap();
        next.begin_effects();
        next.publish(Some(payload()), effects());
        assert!(Producer::claim(SLOT, Some(ID)).is_none());
    });
}
#[test]
fn checked_reserve_generation_and_nonce_and_deferred_claim_nonce_exhaust_independently_without_effects()
 {
    fixture(|| {
        CELLS[SLOT].state.lock().generation = u64::MAX;
        assert!(Producer::begin(SLOT, Some(ID), TaskRetireReason::Exited, false).is_none());
        {
            let mut row = CELLS[SLOT].state.lock();
            assert!(row.producer.is_none());
            row.generation = 0;
            row.producer_nonce = u64::MAX;
        }
        assert!(Producer::begin(SLOT, Some(ID), TaskRetireReason::Exited, false).is_none());
        CELLS[SLOT].state.lock().producer_nonce = 0;
        let mut owner = begin();
        owner.begin_effects();
        owner.defer();
        {
            let mut row = CELLS[SLOT].state.lock();
            row.producer_nonce = u64::MAX;
            row.producer.as_mut().unwrap().key.nonce = u64::MAX;
        }
        assert!(Producer::claim(SLOT, Some(ID)).is_none());
        assert_eq!(
            CELLS[SLOT].state.lock().deferred,
            Some(TaskRetireReason::Exited)
        );
    });
}
#[test]
fn abandoned_effects_quarantine_even_with_empty_deferred_queue_and_no_consumer_payload() {
    fixture(|| {
        let mut owner = begin();
        owner.begin_effects();
        drop(owner);
        assert!(CELLS[SLOT].kernel_pending());
        assert!(reuse::begin(SLOT, Some(ID)).is_none());
        assert!(Producer::claim(SLOT, Some(ID)).is_none());
        assert!(Producer::begin(SLOT, Some(ID), TaskRetireReason::Exited, false).is_none());
        assert!(
            CELLS[SLOT]
                .state
                .lock()
                .producer
                .is_some_and(|r| r.phase == Phase::Poisoned)
        );
    });
}
#[test]
fn stale_claim_rollback_cannot_begin_effects_or_erase_next_claim_nonce_despite_same_uid_and_lifetime_generation()
 {
    fixture(|| {
        let mut owner = begin();
        owner.begin_effects();
        owner.defer();
        let old = Producer::claim(SLOT, Some(ID)).unwrap();
        let key = old.key;
        drop(old);
        let next = Producer::claim(SLOT, Some(ID)).unwrap();
        assert!(next.key.nonce > key.nonce);
        let mut stale = Producer {
            slot: SLOT,
            key,
            reason: TaskRetireReason::Exited,
            detach: true,
            turn: None,
            claimed: true,
            waited_for_executor: false,
            effects: false,
            live: true,
        };
        let rejected =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| stale.begin_effects()));
        stale.live = false;
        assert!(rejected.is_err());
        assert!(next.exact(&CELLS[SLOT].state.lock()));
        drop(next);
        assert!(Producer::claim(SLOT, Some(ID)).is_some());
    });
}
#[test]
fn producer_key_independently_blocks_reuse_pending_and_consumers_when_compatibility_bits_or_payloads_are_misleading()
 {
    fixture(|| {
        let owner = begin();
        {
            let mut row = CELLS[SLOT].state.lock();
            row.producing = false;
        }
        assert!(reuse::begin(SLOT, Some(ID)).is_none());
        assert!(CELLS[SLOT].kernel_pending());
        {
            let mut row = CELLS[SLOT].state.lock();
            row.cleanup = CleanupState::Pending(payload());
            row.kernel = KernelState::Pending(effects());
        }
        assert!(CELLS[SLOT].claim().is_none());
        assert!(CELLS[SLOT].claim_kernel().is_none());
        {
            let mut row = CELLS[SLOT].state.lock();
            row.cleanup = CleanupState::Vacant;
            row.kernel = KernelState::Vacant;
            row.producing = true;
        }
        drop(owner);
    });
}
#[test]
fn unowned_terminal_deferred_take_reset_and_abort_cannot_spend_real_producer() {
    fixture(|| {
        let owner = begin();
        for operation in [0, 1, 2, 3, 4] {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match operation {
                    0 => CELLS[SLOT].publish_terminal(
                        Some(ID),
                        TaskRetireReason::Exited,
                        Some(payload()),
                        effects()
                    ),
                    1 => CELLS[SLOT].defer(Some(ID), TaskRetireReason::Exited),
                    2 => {
                        CELLS[SLOT].take_deferred();
                    }
                    3 => CELLS[SLOT].abort_empty_producer(),
                    _ => CELLS[SLOT].reset(),
                }))
                .is_err()
            );
        }
        assert!(owner.exact(&CELLS[SLOT].state.lock()));
        drop(owner);
    });
}
#[test]
fn unqualified_publication_rolls_back_but_foreign_payload_quarantines_effects_before_consumer_visibility()
 {
    fixture(|| {
        let owner = begin();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || owner.publish(Some(payload()), effects())
            ))
            .is_err()
        );
        assert!(CELLS[SLOT].state.lock().producer.is_none());
        let mut owner = begin();
        owner.begin_effects();
        let mut wrong = payload();
        wrong.task_id += 1;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || owner.publish(Some(wrong), effects())
            ))
            .is_err()
        );
        assert!(CELLS[SLOT].claim().is_none());
        assert!(CELLS[SLOT].claim_kernel().is_none());
        assert!(
            CELLS[SLOT]
                .state
                .lock()
                .producer
                .is_some_and(|r| r.phase == Phase::Poisoned)
        );
    });
}
#[test]
fn kernel_uid_zero_is_an_exact_producer_not_absence_and_reset_never_rewinds_next_admission_epoch() {
    fixture(|| {
        let mut owner = Producer::begin(SLOT, Some(0), TaskRetireReason::Exited, false).unwrap();
        let epoch = owner.key.nonce;
        owner.begin_effects();
        owner.publish(None, RetirementSideEffect::new(Some(0), None));
        let (generation, kernel) = CELLS[SLOT].claim_kernel().unwrap();
        CELLS[SLOT].complete_kernel(generation, kernel);
        let mut seal = reuse::begin(SLOT, Some(0)).unwrap();
        seal.mark_effects();
        seal.reset();
        seal.finish();
        let next = begin();
        assert!(next.key.nonce > epoch);
        drop(next);
    });
}
