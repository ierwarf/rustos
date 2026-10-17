//! Owner: real per-slot kernel revocation publication/claim/ACK fences.
//! Boundary: native terminal catalog publication into actual cells; shared production claim/ACK law.
//! Lifecycle: Pending/Claimed never completes; exact ACK watermark survives reset and identical reuse.
//! Concurrency: canonical runtime fixture serializes global rows, not production producers.
//! Failure: foreign slot/generation/payload never spends a receipt; dropped fence owns no work.
//! Forbidden: absence/reuse as completion or whole-system/physical execution claims.
//! Evidence: actual Fence, CleanupCell::claim_kernel/complete_kernel and reset methods.
use super::*;
const SLOT: usize = 124;
const OTHER: usize = 125;
const ID: u64 = 124011;
fn fixture(f: impl FnOnce()) {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    CELLS[SLOT].reset();
    CELLS[OTHER].reset();
    f();
    CELLS[SLOT].reset();
    CELLS[OTHER].reset();
}
fn publish(slot: usize) -> RetirementSideEffect {
    let effects = RetirementSideEffect::new(Some(ID), None);
    CELLS[slot].publish_terminal(Some(ID), TaskRetireReason::Exited, None, effects);
    effects
}
#[test]
fn captured_kernel_fence_waits_for_exact_ack_not_claim_or_empty_payload() {
    fixture(|| {
        publish(SLOT);
        let fence = Fence::capture();
        assert!(!fence.complete());
        let (generation, effects) = CELLS[SLOT].claim_kernel().unwrap();
        assert!(!fence.complete());
        assert!(!CELLS[SLOT].complete_kernel(generation - 1, effects));
        assert!(!fence.complete());
        assert!(CELLS[SLOT].complete_kernel(generation, effects));
        assert!(fence.complete());
    });
}
#[test]
fn kernel_fence_completion_persists_through_reset_but_identical_reuse_needs_a_new_ack() {
    fixture(|| {
        publish(SLOT);
        let old = Fence::capture();
        let (generation, effects) = CELLS[SLOT].claim_kernel().unwrap();
        assert!(CELLS[SLOT].complete_kernel(generation, effects));
        CELLS[SLOT].reset();
        assert!(old.complete());
        publish(SLOT);
        let next = Fence::capture();
        assert!(old.complete());
        assert!(!next.complete());
        let (next_generation, next_effects) = CELLS[SLOT].claim_kernel().unwrap();
        assert!(next_generation > generation);
        assert!(!CELLS[SLOT].complete_kernel(generation, effects));
        assert!(!next.complete());
        assert!(CELLS[SLOT].complete_kernel(next_generation, next_effects));
        assert!(old.complete() && next.complete());
    });
}
#[test]
fn copied_fence_drop_cannot_abandon_claim_and_foreign_slot_ack_cannot_complete_it() {
    fixture(|| {
        publish(SLOT);
        let fence = Fence::capture();
        let duplicate = fence;
        let (generation, effects) = CELLS[SLOT].claim_kernel().unwrap();
        let _ = fence;
        assert!(!duplicate.complete());
        publish(OTHER);
        let (other_generation, other_effects) = CELLS[OTHER].claim_kernel().unwrap();
        assert!(CELLS[OTHER].complete_kernel(other_generation, other_effects));
        assert!(!duplicate.complete());
        assert!(CELLS[SLOT].complete_kernel(generation, effects));
        assert!(duplicate.complete());
    });
}
