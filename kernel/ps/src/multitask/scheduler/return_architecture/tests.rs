//! Owner: real claimed-return architecture receipt witnesses, not register ACK.
//! Boundary: actual root/UID/MM/placement/funding/allocation/unique original turn.
//! Lifecycle: real selected claim -> borrowed plan or refusal -> native cleanup.
//! Concurrency: exact frame replacement, scalar Current loss and terminal owner.
//! Failure: copied result cannot recreate a lost/missing grant or prepared bank.
//! Forbidden: native bank refresh/phase recovery as hardware RSP evidence.
//! Evidence: real retained receipt and no fresh post-claim MM admission.
use super::super::dispatch_claim::{
    FrameCustody,
    tests::{capture, finish_modeled_preparation, fixture},
};
use super::*;
const KERNEL: usize = 121;
fn result(slot: usize, original: usize) -> SchedulerDispatch {
    SchedulerDispatch::new(runqueue::saved_rsp(slot), 0, original, slot)
}
#[test]
fn actual_copied_dispatch_without_successful_claim_refuses_architecture() {
    fixture(true, |slot, turn| {
        assert!(ReturnArchitecture::capture(turn, result(slot, slot)).is_none());
        assert!(
            capture(slot, turn, FrameCustody::Current)
                .prepare()
                .unwrap()
                .claim()
        );
        assert!(ReturnArchitecture::capture(turn, result(slot, slot)).is_some());
        finish_modeled_preparation(slot, slot);
    });
}
#[test]
fn actual_same_current_retains_exact_claim_and_mm_without_architectural_replay() {
    fixture(true, |slot, turn| {
        assert!(
            capture(slot, turn, FrameCustody::Current)
                .prepare()
                .unwrap()
                .claim()
        );
        let plan = ReturnArchitecture::capture(turn, result(slot, slot)).unwrap();
        assert!(!plan.changed);
        assert_eq!(plan.incoming.root, plan.receipt.previous.root);
        assert_eq!(
            turn.claimed_mm_root(slot, plan.receipt.id),
            Some(plan.incoming.root)
        );
        assert!(plan.receipt.return_to_user);
        drop(plan);
        finish_modeled_preparation(slot, slot);
    });
}
#[test]
fn actual_local_peer_plan_uses_original_frame_mm_and_placement_banks() {
    fixture(false, |slot, turn| {
        assert!(
            capture(slot, turn, FrameCustody::Local)
                .prepare()
                .unwrap()
                .claim()
        );
        let plan = ReturnArchitecture::capture(turn, result(slot, KERNEL)).unwrap();
        assert!(plan.changed);
        assert!(plan.incoming.register_ipc); // original Linux context, not a PID hint
        let mut effects = std::vec::Vec::new();
        lane_effects(plan.receipt.previous, plan.incoming, slot, |effect| effects.push(effect));
        assert!(effects.iter().any(|effect| matches!(effect,
            ArchitectureEffect::SyscallStack(top, true) if *top == plan.incoming.stack_top)));
        // The actual fixture actor is a kernel continuation in the SAME
        // process, not a forged cross-root execution grant.
        assert_eq!(plan.incoming.root, plan.receipt.previous.root);
        assert!(plan.receipt.return_to_user);
        assert_eq!(
            plan.incoming.stack_top as usize,
            plan.receipt.token.view.kernel_stack_top as usize
        );
        drop(plan);
        finish_modeled_preparation(slot, KERNEL);
    });
}
#[test]
fn actual_direct_peer_plan_requires_original_fifo_claim_not_result_scalar() {
    fixture(false, |slot, turn| {
        runqueue::publish_blocked(slot, 0, runqueue::weight::value(slot));
        runqueue::publish_direct_handoff(slot, 0);
        assert!(
            capture(slot, turn, FrameCustody::Direct)
                .prepare()
                .unwrap()
                .claim()
        );
        assert!(ReturnArchitecture::capture(turn, result(slot, KERNEL)).is_some());
        finish_modeled_preparation(slot, KERNEL);
    });
}
#[test]
fn actual_foreign_return_rsp_prior_or_selected_slot_refuses_original_claim() {
    fixture(true, |slot, turn| {
        assert!(
            capture(slot, turn, FrameCustody::Current)
                .prepare()
                .unwrap()
                .claim()
        );
        let good = result(slot, slot);
        let mut bad = good;
        bad.next_rsp += 8;
        assert!(ReturnArchitecture::capture(turn, bad).is_none());
        bad = good;
        bad.previous_slot = KERNEL;
        assert!(ReturnArchitecture::capture(turn, bad).is_none());
        bad = good;
        bad.next_slot = KERNEL;
        assert!(ReturnArchitecture::capture(turn, bad).is_none());
        assert!(ReturnArchitecture::capture(turn, good).is_some());
        finish_modeled_preparation(slot, slot);
    });
}
#[test]
fn actual_claimed_frame_republication_refuses_without_reading_a_new_header() {
    fixture(true, |slot, turn| {
        assert!(
            capture(slot, turn, FrameCustody::Current)
                .prepare()
                .unwrap()
                .claim()
        );
        let token = turn.claimed_return().unwrap().token;
        let v = token.view;
        // Explicit native publisher fault; no production frame-writer permission.
        frame_publication::publish(
            slot,
            token.saved_rsp,
            v.kernel_stack_base,
            v.kernel_stack_top,
            v.alternate_stack_base,
            v.alternate_stack_top,
            v.user_mode_task,
            v.kernel_process_task,
            v.stack_canary_intact,
            v.scheduler_storage_base,
            v.scheduler_storage_end,
        );
        assert!(ReturnArchitecture::capture(turn, result(slot, slot)).is_none());
        assert!(stack_resources::cpu_leases::owns_prepared(0, slot));
        finish_modeled_preparation(slot, slot);
    });
}
#[test]
fn actual_claimed_return_requires_original_physical_prepared_bank() {
    fixture(true, |slot, turn| {
        assert!(
            capture(slot, turn, FrameCustody::Current)
                .prepare()
                .unwrap()
                .claim()
        );
        let good = result(slot, slot);
        assert!(ReturnArchitecture::capture(turn, good).is_some());
        // Model ONLY bank ACK; retain the actual executing grant so this
        // witness cannot pass merely because interval admission also refused.
        stack_resources::cpu_leases::publish(0, slot, slot);
        stack_resources::cpu_leases::commit(0);
        assert!(super::super::execution_interval::original_claim_is_active(turn));
        assert!(ReturnArchitecture::capture(turn, good).is_none());
        assert!(matches!(super::super::execution_interval::consume(0, slot, turn.claimed_return().unwrap().id),
            super::super::execution_interval::ConsumedInterval::Active(_)));
    });
}
#[test]
fn actual_settled_original_grant_cannot_admit_architecture_with_retained_frame_banks() {
    fixture(true, |slot, turn| {
        assert!(capture(slot, turn, FrameCustody::Current).prepare().unwrap().claim());
        let good = result(slot, slot);
        assert!(ReturnArchitecture::capture(turn, good).is_some());
        assert!(matches!(super::super::execution_interval::consume(0, slot, turn.claimed_return().unwrap().id),
            super::super::execution_interval::ConsumedInterval::Active(_)));
        assert!(stack_resources::cpu_leases::owns_prepared(0, slot));
        assert!(crate::multitask::current_identity::execution::banks::owns_execution(0, slot));
        assert!(ReturnArchitecture::capture(turn, good).is_none());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
            super::super::execution_interval::finish_return(turn, slot))).is_err());
        // Model the earlier architecture/RSP ACK only for fixture teardown.
        stack_resources::cpu_leases::publish(0, slot, slot);
        stack_resources::cpu_leases::commit(0);
    });
}
#[test]
fn actual_claimed_return_refuses_replaced_scalar_current_with_retained_banks() {
    fixture(true, |slot, turn| {
        assert!(
            capture(slot, turn, FrameCustody::Current)
                .prepare()
                .unwrap()
                .claim()
        );
        let refused = crate::multitask::cpu_local::with_test_current_scalar(0, KERNEL, || {
            ReturnArchitecture::capture(turn, result(slot, slot)).is_none()
        });
        assert!(refused);
        assert!(ReturnArchitecture::capture(turn, result(slot, slot)).is_some());
        finish_modeled_preparation(slot, slot);
    });
}
#[test]
fn actual_terminal_after_claim_cannot_reopen_architecture_or_drop_original_backing() {
    fixture(true, |slot, turn| {
        assert!(
            capture(slot, turn, FrameCustody::Current)
                .prepare()
                .unwrap()
                .claim()
        );
        let id = turn.claimed_return().unwrap().id;
        runqueue::retire(slot, runqueue::weight::value(slot));
        assert!(ReturnArchitecture::capture(turn, result(slot, slot)).is_none());
        assert!(stack_resources::cpu_leases::owns_prepared(0, slot));
        assert!(crate::multitask::current_identity::execution::banks::owns_execution(0, slot));
        assert!(matches!(
            super::super::execution_interval::consume(0, slot, id),
            super::super::execution_interval::ConsumedInterval::Active(_)
        ));
        // Explicit native cleanup, not a successful register/RSP return.
        stack_resources::cpu_leases::publish(0, slot, slot);
        stack_resources::cpu_leases::commit(0);
    });
}

#[test]
fn actual_retained_architecture_refuses_foreign_uid_and_frame_geometry() {
    fixture(true, |slot, turn| {
        assert!(
            capture(slot, turn, FrameCustody::Current)
                .prepare()
                .unwrap()
                .claim()
        );
        let receipt = turn.claimed_return().unwrap();
        let view = receipt.token.view;
        assert!(
            stack_resources::context::retained_architecture(0, slot, receipt.id, view).is_some()
        );
        assert!(
            stack_resources::context::retained_architecture(0, slot, receipt.id + 1, view)
                .is_none()
        );
        let mut foreign = view;
        foreign.kernel_stack_top += 16;
        assert!(
            stack_resources::context::retained_architecture(0, slot, receipt.id, foreign).is_none()
        );
        finish_modeled_preparation(slot, slot);
    });
}

#[test]
fn actual_original_architecture_phase_admits_once_and_ready_is_not_a_fresh_claim() {
    fixture(true, |slot, turn| {
        assert!(
            capture(slot, turn, FrameCustody::Current)
                .prepare()
                .unwrap()
                .claim()
        );
        let dispatch = result(slot, slot);
        assert!(ReturnArchitecture::capture(turn, dispatch).is_some());
        turn.begin_architecture();
        assert!(ReturnArchitecture::capture(turn, dispatch).is_none());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| turn.begin_architecture()))
                .is_err()
        );
        // Explicit native completion MODEL: no register/RSP acceptance claimed.
        turn.mark_architecture_ready();
        assert!(ReturnArchitecture::capture(turn, dispatch).is_none());
        finish_modeled_preparation(slot, slot);
    });
}
