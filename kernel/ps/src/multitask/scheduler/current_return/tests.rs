//! Owner: actual original Current return/frame/interval publication witnesses.
//! Boundary: real root/backing bank and CPU turn; earlier queue/IRQ/MM entry modeled.
//! Lifecycle: one capture -> funding/account/frame/queue -> failed peer fallback -> original drop.
//! Concurrency: actual wake/stop between return effects, not permissive Blocked recapture.
//! Failure: bank/custody/geometry refusal; original terminal loan retains backing without new admission.
//! Forbidden: native CPU phase restoration as production recovery or complete guard deletion proof.
//! Evidence: exact UID, actual captured funding, frame epoch, queue generation and fairness delta.
use super::*;
use crate::multitask::cpu_local::{self, dispatch_turn};
use crate::user::abi::UserAbi;
const ID: u64 = 82700;
fn root(current: bool, running: bool, f: impl FnOnce(usize, &CpuDispatchTurn<'static>)) {
    super::super::root_creation::tests::exec_root(
        ID,
        current,
        super::super::root_creation::tests::bootstrap(UserAbi::Linux),
        |slot, _| {
            if running {
                runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
            }
            dispatch_turn::with_original_current_model(slot, |turn| f(slot, turn));
        },
    );
}
fn begin_account(slot: usize, outgoing: &mut CurrentReturn<'_>) -> u64 {
    execution_interval::test_open_captured(slot, 1);
    let elapsed = outgoing.account(2, true);
    assert!(elapsed > 0);
    elapsed
}
#[test]
fn actual_original_return_failed_peer_fallback_reuses_interval_frame_and_queue_receipt_once() {
    root(true, true, |slot, turn| {
        let mut outgoing = CurrentReturn::capture(turn).unwrap();
        assert_eq!(outgoing.id(), ID);
        let rsp = runqueue::saved_rsp(slot);
        // Model the IRQ producer's fresh user frame, not the root fixture's
        // deliberately non-admitted low VA inputs or physical MM acceptance.
        assert!(
            outgoing
                .view
                .context_stack_contains(rsp, rsp + super::super::SAVED_CONTEXT_BYTES)
        );
        // SAFETY: native original turn/bank owns this exact allocated frame;
        // this is the modeled earlier IRQ producer, before any queue release.
        unsafe {
            let frame = &mut *(rsp as *mut super::super::SavedContext);
            frame.rip = crate::memory::paging::USER_SPACE_BASE + 0x1000;
            frame.rsp = crate::memory::paging::USER_SPACE_BASE + 0x4000;
        }
        assert!(outgoing.validate(rsp).is_none());
        let before_vruntime = runqueue::vruntime(slot);
        let elapsed = begin_account(slot, &mut outgoing);
        let charged_vruntime = runqueue::vruntime(slot);
        assert_eq!(
            charged_vruntime - before_vruntime,
            task_weight::weighted_vruntime_delta(
                elapsed.max(SCHED_MIN_GRANULARITY_NS),
                runqueue::weight::value(slot)
            )
        );
        runqueue::wait::publish_arm(slot, runqueue::wait::REASON_ENDPOINT_REPLY, 42);
        assert_eq!(
            runqueue::wait::commit(slot),
            runqueue::WaitCommitOutcome::Committed
        );
        outgoing.publish_frame(rsp, false);
        assert_eq!(
            outgoing.publish_queue(true),
            runqueue::ReturnPublication::Blocked {
                was_runnable: false
            }
        );
        let published = frame_publication::test_read(slot).unwrap();
        let generation = runqueue::owner(slot).generation;
        // An exact peer claim fails after sender publication. Earlier peer
        // selection is modeled; actual generation claim refuses empty custody.
        assert!(!runqueue::claim_dispatch_with_owner_generation(
            120,
            0,
            100,
            Some(999)
        ));
        assert!(CurrentReturn::capture(turn).is_none()); // no fresh Blocked grant
        assert_eq!(outgoing.account(99, true), elapsed);
        outgoing.publish_frame(rsp + 8, true); // fallback cannot replace original frame
        assert_eq!(
            outgoing.validate(rsp + 8),
            Some("saved context pointer is outside the task stack")
        );
        assert!(outgoing.validate(rsp).is_none()); // exact original receipt, no frame re-read
        assert_eq!(
            outgoing.publish_queue(true),
            runqueue::ReturnPublication::Blocked {
                was_runnable: false
            }
        );
        assert_eq!(runqueue::owner(slot).generation, generation);
        assert_eq!(
            frame_publication::test_read(slot).unwrap().frame_epoch,
            published.frame_epoch
        );
        assert_eq!(runqueue::saved_rsp(slot), rsp);
        assert_eq!(runqueue::vruntime(slot), charged_vruntime);
        assert!(matches!(
            execution_interval::consume(0, slot, ID),
            execution_interval::ConsumedInterval::AlreadySettled
        ));
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
    });
}
#[test]
fn actual_original_return_raced_completed_wait_is_not_cleared_and_publishes_local() {
    root(true, true, |slot, turn| {
        let mut outgoing = CurrentReturn::capture(turn).unwrap();
        begin_account(slot, &mut outgoing);
        runqueue::wait::publish_arm(slot, runqueue::wait::REASON_ENDPOINT_REPLY, 43);
        assert_eq!(
            runqueue::wait::commit(slot),
            runqueue::WaitCommitOutcome::Committed
        );
        runqueue::wake_wait(slot); // actual wake wins between stopped observation and publication
        assert!(runqueue::owner(slot).runnable);
        outgoing.publish_frame(runqueue::saved_rsp(slot), false);
        assert!(runqueue::owner(slot).runnable);
        assert_eq!(
            outgoing.publish_queue(true),
            runqueue::ReturnPublication::Local
        );
        assert_eq!(
            frame_publication::test_read(slot).unwrap().owner_generation,
            runqueue::owner(slot).generation
        );
        // Queue publication is NOT a suspended-frame grant while this
        // original physical Current still owns its consumed frame.
        assert!(frame_publication::local_frame_token(slot, 0).is_none());
        assert!(CurrentReturn::capture(turn).is_none()); // no fresh Local grant
    });
}
#[test]
fn actual_original_return_terminal_after_capture_keeps_original_bank_and_refuses_resurrection() {
    root(true, true, |slot, turn| {
        let mut outgoing = CurrentReturn::capture(turn).unwrap();
        begin_account(slot, &mut outgoing);
        runqueue::retire(slot, runqueue::weight::value(slot));
        outgoing.publish_frame(runqueue::saved_rsp(slot), false);
        assert_eq!(
            outgoing.publish_queue(true),
            runqueue::ReturnPublication::Terminal
        );
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Retired
        );
        assert!(!runqueue::owner(slot).runnable);
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
    });
}
#[test]
fn actual_original_return_already_terminal_borrows_old_bank_not_a_new_post_stop_pin() {
    root(true, true, |slot, turn| {
        execution_interval::test_open_captured(slot, 1);
        runqueue::retire(slot, runqueue::weight::value(slot));
        let refs = current_identity::execution::count(slot);
        let mut outgoing = CurrentReturn::capture(turn).unwrap();
        assert_eq!(current_identity::execution::count(slot), refs);
        assert!(outgoing.account(2, true) > 0);
        outgoing.publish_frame(runqueue::saved_rsp(slot), false);
        assert_eq!(
            outgoing.publish_queue(true),
            runqueue::ReturnPublication::Terminal
        );
        assert_eq!(current_identity::execution::count(slot), refs);
    });
}
#[test]
fn actual_original_return_scalar_running_without_physical_bank_refuses_before_effects() {
    root(false, true, |slot, turn| {
        let scalar = cpu_local::install_test_current_owner(0, slot);
        let before = runqueue::owner(slot);
        assert!(turn.original_current().is_none());
        let token = frame_publication::test_read(slot).unwrap();
        // A different allocation borrower makes pins nonzero but is NOT this
        // CPU's physical Current bank. Test both independent refusal gates.
        let borrowed = frame_publication::pin_for_identity(token, ID).unwrap();
        assert!(stack_resources::current_return_binding(0, slot, token.view).is_none());
        assert!(CurrentReturn::capture(turn).is_none());
        assert_eq!(runqueue::owner(slot), before);
        drop(borrowed);
        drop(scalar);
    });
}
#[test]
fn actual_original_return_physical_bank_without_running_or_terminal_custody_refuses() {
    root(true, false, |slot, turn| {
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        assert!(CurrentReturn::capture(turn).is_none());
    });
}
#[test]
fn actual_original_return_replaced_scalar_current_refuses_even_with_retained_original_bank() {
    root(true, true, |slot, turn| {
        let replaced = cpu_local::install_test_current_owner(0, slot + 1);
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert!(turn.original_current().is_none());
        assert!(CurrentReturn::capture(turn).is_none());
        drop(replaced);
        assert!(CurrentReturn::capture(turn).is_some());
    });
}
#[test]
fn actual_original_return_foreign_frame_geometry_refuses_before_any_byte_read_or_effect() {
    root(true, true, |slot, turn| {
        let token = frame_publication::test_read(slot).unwrap();
        let v = token.view;
        let publish = |base| {
            frame_publication::publish(
                slot,
                token.saved_rsp,
                base,
                v.kernel_stack_top,
                v.alternate_stack_base,
                v.alternate_stack_top,
                v.user_mode_task,
                v.kernel_process_task,
                v.stack_canary_intact,
                v.scheduler_storage_base,
                v.scheduler_storage_end,
            )
        };
        publish(1); // scalar payload fault only, not admitted alternate geometry
        assert!(CurrentReturn::capture(turn).is_none());
        publish(v.kernel_stack_base);
        assert!(CurrentReturn::capture(turn).is_some());
    });
}
#[test]
fn actual_original_return_blocked_frame_refresh_observes_live_guard_not_old_snapshot() {
    root(true, true, |slot, turn| {
        let mut outgoing = CurrentReturn::capture(turn).unwrap();
        assert!(outgoing.view.stack_canary_intact);
        begin_account(slot, &mut outgoing);
        runqueue::wait::publish_arm(slot, runqueue::wait::REASON_ENDPOINT_REPLY, 44);
        assert_eq!(
            runqueue::wait::commit(slot),
            runqueue::WaitCommitOutcome::Committed
        );
        let base = (outgoing.view.kernel_stack_base - super::super::TASK_STACK_GUARD_BYTES as u64)
            as *mut u64;
        // SAFETY: native modeled executing-task guard corruption, under the
        // actual original Current bank. Restore before real fixture teardown.
        unsafe {
            base.write(0);
        }
        outgoing.publish_frame(runqueue::saved_rsp(slot), false);
        assert!(
            !frame_publication::test_read(slot)
                .unwrap()
                .view
                .stack_canary_intact
        );
        // SAFETY: same retained native allocated guard as above.
        unsafe {
            base.write(super::super::STACK_CANARY_WORD);
        }
        assert_eq!(
            outgoing.publish_queue(true),
            runqueue::ReturnPublication::Blocked {
                was_runnable: false
            }
        );
    });
}
