//! Owner: real claimed-return architecture receipt witnesses, not register ACK.
//! Boundary: actual root/UID/MM/placement/funding/allocation/unique original turn.
//! Lifecycle: real selected claim -> borrowed plan or refusal -> native cleanup.
//! Concurrency: exact frame replacement, scalar Current loss and terminal owner.
//! Failure: copied result cannot recreate a lost/missing grant or prepared bank.
//! Forbidden: native bank refresh/phase recovery as hardware RSP evidence.
//! Evidence: real retained receipt and no fresh post-claim MM admission.
use super::super::dispatch_claim::{
    FrameCustody,
    tests::{capture, finish_modeled_preparation, fixture, fixture_with_release},
};
use super::*;
use super::super::{retirement_cleanup, terminal_publication, terminal_quarantine, TaskRetireReason};
use crate::multitask::{cpu_local, current_identity as identity};
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

/// The window these two tests pin: a dispatch claim is the owner CAS to
/// `Running(cpu)`, and the task is not published as that CPU's Current until
/// the switch. A stop requested from another CPU in that window used to retire
/// the owner word under the claimer, whose install then refused a claim it
/// owned and stopped the kernel ("IRQ architecture lacks the original
/// successful claim"). The stop must instead wait, untouched, for the claim.
#[test]
fn actual_remote_terminal_cannot_retire_a_slot_whose_dispatch_claim_is_in_flight() {
    fixture_with_release(
        false,
        |slot, turn| {
            assert!(
                capture(slot, turn, FrameCustody::Local)
                    .prepare()
                    .unwrap()
                    .claim()
            );
            let id = turn.claimed_return().unwrap().id;
            // The claim is in flight: Running, and not yet any CPU's Current.
            assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Running);
            assert_eq!(cpu_local::task_execution_owner(slot), None);
            assert_eq!(
                runqueue::retire_unless_claim_in_flight(slot, runqueue::weight::value(slot)),
                runqueue::RetireOutcome::ClaimInFlight
            );

            let stop =
                terminal_quarantine::begin(slot, Some(id), TaskRetireReason::Exited, false)
                    .unwrap();
            // SAFETY: the fixture's original masked scope is retained throughout.
            unsafe { terminal_publication::guarded(stop, &|_| 1); }

            // Nothing was withdrawn under the claimer...
            assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Running);
            assert!(!identity::lifecycle(slot).retired);
            assert!(ReturnArchitecture::capture(turn, result(slot, KERNEL)).is_some());
            // ...and the stop request was kept, not dropped.
            assert_eq!(retirement_cleanup::deferred(slot), Some(TaskRetireReason::Exited));

            // The claimer finishes. Natively this models the install and the
            // switch publication; it is not a hardware acceptance.
            finish_modeled_preparation(slot, KERNEL);

            // The parked stop is resumed by the next claimant and now retires
            // the task, exactly once, from the top.
            let resumed = terminal_quarantine::begin(
                slot,
                Some(id),
                TaskRetireReason::Exited,
                false,
            )
            .expect("the parked stop is resumable");
            // The switch has published the task as CPU 0's Current (modeled by
            // the scalar), which is what makes the remote stop applicable.
            cpu_local::with_test_current_scalar(0, slot, || {
                // SAFETY: as above.
                unsafe { terminal_publication::guarded(resumed, &|_| 1); }
            });
            assert!(identity::lifecycle(slot).retired);
            assert!(runqueue::owner(slot).state.is_terminal());
        },
        // The stop above was withdrawn but is still deferred behind the modeled
        // switch, so the fixture's own producer must not be started a second
        // time. Only the external cleanup is injected here: the actual seals,
        // bank, context and process detachment still run and still refuse any
        // outstanding loan.
        release_withdrawn_stop,
    );
}

#[test]
fn actual_published_current_is_still_stopped_remotely_without_waiting() {
    // The wait is for an in-flight claim only. A task already published as a
    // CPU's Current is the ordinary remote-stop case and must retire at once,
    // otherwise a running task could never be stopped from another CPU.
    fixture(true, |slot, turn| {
        assert!(
            capture(slot, turn, FrameCustody::Current)
                .prepare()
                .unwrap()
                .claim()
        );
        assert_eq!(
            runqueue::retire_unless_claim_in_flight(slot, runqueue::weight::value(slot)),
            runqueue::RetireOutcome::Retired,
            "a Current slot has a published execution role, so no claim is in flight"
        );
        assert!(runqueue::owner(slot).state.is_terminal());
        finish_modeled_preparation(slot, slot);
    });
}

/// Teardown for a task whose stop was withdrawn and deferred by the test: it
/// completes the same cleanup as the default release but finishes the retained
/// producer instead of beginning a new one.
fn release_withdrawn_stop(slot: usize, id: u64, handle: crate::multitask::process_table::ProcessHandle) {
    let masked = nucleus_core::util::lockdep::DispatchPreemption::enter();
    let mut masked = masked;
    masked.mask_irqs();
    // SAFETY: a fresh masked same-CPU scope drains the retained original stop;
    // no source UID, context or execution grant is reacquired.
    unsafe { terminal_publication::drain_guarded(&|_| 1); }
    drop(masked);
    terminal_publication::tests::release_published(slot, id, handle);
}
