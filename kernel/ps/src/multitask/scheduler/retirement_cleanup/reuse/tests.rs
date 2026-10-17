//! Owner: drained terminal row reset seal witnesses.
//! Boundary: exact row/nonce, not complete process/group/hardware teardown.
//! Lifecycle: sealed reset blocks producers through effects and own release.
//! Concurrency: shared scheduler fixture orders native global row isolation.
//! Failure: pending custody/exhaustion refuses before nonce/effects; post-effect Drop retains seal.
//! Forbidden: native injected recovery is not a production close rollback.
//! Evidence: actual Class50 row/seal methods; resource vacancy proved independently.
use super::*;
use crate::multitask::scheduler;
use std::panic::{AssertUnwindSafe, catch_unwind};
const SLOT: usize = 105;
const ID: u64 = 105001;
fn initialize() -> u64 {
    let mut row = CELLS[SLOT].state.lock();
    assert!(row.reset_seal.is_none());
    row.cleanup = CleanupState::Complete;
    row.kernel = KernelState::Complete;
    row.task_id = Some(ID);
    row.reason = None;
    row.producing = false;
    row.deferred = None;
    row.generation = row.generation.checked_add(1).unwrap();
    row.generation
}
#[test]
fn sealed_terminal_row_denies_foreign_unowned_reset_and_deferred_producer_until_complete_ack() {
    let _fixture = scheduler::tests::boxed_scheduler();
    initialize();
    assert!(begin(SLOT, Some(ID + 1)).is_none());
    let mut seal = begin(SLOT, Some(ID)).unwrap();
    assert!(begin(SLOT, Some(ID)).is_none());
    assert!(catch_unwind(AssertUnwindSafe(|| CELLS[SLOT].reset())).is_err());
    assert!(
        catch_unwind(AssertUnwindSafe(
            || CELLS[SLOT].defer(Some(ID), TaskRetireReason::Exited)
        ))
        .is_err()
    );
    seal.mark_effects();
    seal.reset();
    // Vacant post-reset row removes the independent kernel/cleanup guards.
    assert!(
        catch_unwind(AssertUnwindSafe(
            || CELLS[SLOT].defer(Some(ID), TaskRetireReason::Exited)
        ))
        .is_err()
    );
    assert!(begin(SLOT, None).is_none());
    seal.finish();
    let seal = begin(SLOT, None).unwrap();
    drop(seal);
}
#[test]
fn each_terminal_custody_independently_refuses_seal_without_spending_nonce() {
    let _fixture = scheduler::tests::boxed_scheduler();
    initialize();
    let nonce = CELLS[SLOT].state.lock().reset_nonce;
    for mode in 0..4 {
        {
            let mut row = CELLS[SLOT].state.lock();
            match mode {
                0 => row.producing = true,
                1 => row.deferred = Some(TaskRetireReason::Exited),
                2 => {
                    row.cleanup = CleanupState::Pending(crate::multitask::RetiredTaskCleanup {
                        task_id: ID,
                        process_id: ID,
                        process_terminal: false,
                        clear_child_tid: 0,
                        robust_list_head: 0,
                        robust_list_len: 0,
                    })
                }
                _ => row.kernel = KernelState::Claimed(RetirementSideEffect::new(Some(ID), None)),
            }
        }
        assert!(begin(SLOT, Some(ID)).is_none());
        {
            let mut row = CELLS[SLOT].state.lock();
            assert_eq!(row.reset_nonce, nonce);
            row.producing = false;
            row.deferred = None;
            row.cleanup = CleanupState::Complete;
            row.kernel = KernelState::Complete;
        }
    }
    let mut seal = begin(SLOT, Some(ID)).unwrap();
    seal.mark_effects();
    seal.reset();
    seal.finish();
}
#[test]
fn abandoned_effects_keep_terminal_row_quarantined_and_nonce_exhaustion_never_wraps() {
    let _fixture = scheduler::tests::boxed_scheduler();
    initialize();
    let old = {
        let mut row = CELLS[SLOT].state.lock();
        let old = row.reset_nonce;
        row.reset_nonce = u64::MAX;
        old
    };
    assert!(begin(SLOT, Some(ID)).is_none());
    {
        let mut row = CELLS[SLOT].state.lock();
        assert!(row.reset_seal.is_none());
        row.reset_nonce = old;
    }
    let mut seal = begin(SLOT, Some(ID)).unwrap();
    seal.mark_effects();
    seal.reset();
    drop(seal);
    assert!(begin(SLOT, None).is_none());
    assert!(CELLS[SLOT].state.lock().reset_seal.is_some());
    CELLS[SLOT].state.lock().reset_seal = None;
}
#[test]
fn before_effect_rollback_is_exact_and_old_seal_cannot_release_next_producer_nonce() {
    let _fixture = scheduler::tests::boxed_scheduler();
    let generation = initialize();
    let old = begin(SLOT, Some(ID)).unwrap();
    let nonce = old.nonce;
    drop(old);
    let mut next = begin(SLOT, Some(ID)).unwrap();
    assert!(next.nonce > nonce);
    drop(ResetSeal {
        slot: SLOT,
        nonce,
        generation,
        effects: false,
        reset: false,
        live: true,
    });
    assert_eq!(CELLS[SLOT].state.lock().reset_seal, Some(next.nonce));
    next.mark_effects();
    next.reset();
    next.finish();
}
