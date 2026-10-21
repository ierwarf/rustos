//! Owner: actual selected frame/funding/claim transaction witnesses.
//! Boundary: real root, resource/MM, CPU bank and original turn; earlier IRQ input modeled.
//! Lifecycle: actual original loans -> exact epoch/UID/funding -> real claim or refusal -> cleanup.
//! Concurrency: writer, owner, affinity, terminal and other-reader changes before final CAS.
//! Failure: no scratch funding/allocation is staged by refused selections.
//! Forbidden: native phase/bank cleanup as hardware ACK or joint guard-deletion evidence.
//! Evidence: real queue membership, allocation banks, context funding and consumed interval.
use super::super::{
    current_return::CurrentReturn, execution_interval, root_creation, scheduling_runtime,
};
use super::*;
use crate::multitask::cpu_local::dispatch_turn;
use crate::user::abi::UserAbi;
const ID: u64 = 82800;
const KERNEL: usize = 121;
pub(in crate::multitask::scheduler) fn fixture(current: bool, f: impl FnOnce(usize, &CpuDispatchTurn<'static>)) {
    root_creation::tests::exec_root(
        ID,
        current,
        root_creation::tests::bootstrap(UserAbi::Linux),
        |slot, _| {
            // Earlier IRQ/loader VA input is modeled, not hardware/MM acceptance.
            let rsp = runqueue::saved_rsp(slot);
            let view = frame_publication::test_read(slot).unwrap().view;
            assert!(view.context_stack_contains(rsp, rsp + super::super::SAVED_CONTEXT_BYTES));
            // SAFETY: isolated real root allocation and checked bounds model
            // the earlier IRQ/loader frame producer before any reader/claim.
            unsafe {
                let frame = &mut *(rsp as *mut super::super::SavedContext);
                frame.rip = crate::memory::paging::USER_SPACE_BASE + 0x1000;
                frame.rsp = crate::memory::paging::USER_SPACE_BASE + 0x4000;
            }
            let original = if current { slot } else { KERNEL };
            // Explicit native earlier execution-bank isolation; no production ACK.
            let _banks = crate::multitask::current_identity::execution::banks::isolate(0);
            dispatch_turn::with_original_current_model(original, |turn| {
                if current {
                    runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
                    // Model zero elapsed time, not exhaustion of the fixture's 4us budget.
                    execution_interval::test_open_captured(slot, 2);
                    let mut outgoing = CurrentReturn::capture(turn).unwrap();
                    assert_eq!(outgoing.account(2, false), 0);
                    outgoing.publish_frame(rsp, true);
                    assert!(matches!(
                        outgoing.publish_queue(true),
                        runqueue::ReturnPublication::Local
                    ));
                }
                f(slot, turn);
            });
        },
    );
}
pub(in crate::multitask::scheduler) fn capture<'a>(
    slot: usize,
    turn: &'a CpuDispatchTurn<'static>,
    custody: FrameCustody,
) -> DispatchFrame<'a> {
    let FrameCapture::Ready(frame) = DispatchFrame::capture(
        turn,
        slot,
        ID,
        runqueue::owner(slot).generation,
        runqueue::saved_rsp(slot),
        custody,
    ) else {
        panic!("modeled admitted frame did not capture");
    };
    frame
}
fn no_preparation(slot: usize) {
    assert!(!stack_resources::cpu_leases::owns_prepared(0, slot));
    assert!(scheduling_runtime::close_for_reclaim(
        slot,
        stack_resources::context::read(slot)
            .unwrap()
            .scheduling_context
            .identity()
    ));
}
pub(in crate::multitask::scheduler) fn finish_modeled_preparation(slot: usize, original: usize) {
    assert!(stack_resources::cpu_leases::owns_prepared(0, slot));
    assert!(matches!(
        execution_interval::consume(0, slot, ID),
        execution_interval::ConsumedInterval::Active(_)
    ));
    assert!(matches!(
        execution_interval::consume(0, slot, ID),
        execution_interval::ConsumedInterval::AlreadySettled
    ));
    // Explicit native bank refresh, not an architecture/RSP receipt. For a
    // peer fixture the modeled actor stays Current; park/release the prepared
    // allocation separately with actual publication before restoring that actor.
    stack_resources::cpu_leases::publish(0, original, slot);
    stack_resources::cpu_leases::commit(0);
}
#[test]
fn actual_selection_consumes_same_guard_with_one_resource_admission() {
    use nucleus_core::util::lockdep::{LockClass, work_budget};
    use rustos_user_abi::performance::FRAME_READ_MAX_RESOURCE_ACQUISITIONS;
    fixture(true, |slot, turn| {
        let budget = work_budget::declare(LockClass::TaskStackResources, FRAME_READ_MAX_RESOURCE_ACQUISITIONS);
        // SAFETY: the same original modeled masked turn spans the full loan.
        let selection = unsafe { FrameSelection::capture(0, slot, ID,
            runqueue::owner(slot).generation, runqueue::saved_rsp(slot), FrameCustody::Current) }
            .unwrap_or_else(|_| panic!("original selection refused"));
        assert_eq!(stack_resources::frame_readers::test_count(slot), 1);
        let FrameCapture::Ready(frame) = DispatchFrame::from_selection(turn, selection,
            super::super::donation_ledger::class_publication_sequence()) else {
            panic!("original selection was not consumed");
        };
        assert_eq!(stack_resources::frame_readers::test_count(slot), 1);
        drop(frame);
        assert_eq!(stack_resources::frame_readers::test_count(slot), 0);
        assert_eq!(budget.used(), Some(1), "live guard validation and consumption cannot reopen resources");
    });
}
#[test]
fn actual_selection_promotes_only_its_own_reader_after_final_owner_cas() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current).prepare().unwrap();
        assert_eq!(stack_resources::frame_readers::test_count(slot), 1);
        let before = runqueue::owner(slot);
        // Raw callers must not acquire execution through the selected reader.
        assert!(!runqueue::claim_dispatch_with_owner_generation(slot, 0,
            runqueue::weight::value(slot), Some(before.generation)));
        assert_eq!(runqueue::owner(slot), before);
        assert!(!stack_resources::cpu_leases::owns_prepared(0, slot));
        assert!(funded.claim());
        assert_eq!(stack_resources::frame_readers::test_count(slot), 0);
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Running);
        finish_modeled_preparation(slot, slot);
    });
}
#[test]
fn actual_selection_stale_owner_refuses_before_consumption_without_bank_effects() {
    fixture(true, |slot, turn| {
        // SAFETY: the original modeled masked turn retains this exact backing.
        let selection = unsafe { FrameSelection::capture(0, slot, ID,
            runqueue::owner(slot).generation, runqueue::saved_rsp(slot), FrameCustody::Current) }
            .unwrap_or_else(|_| panic!("original selection refused"));
        runqueue::publish_blocked(slot, 0, runqueue::weight::value(slot));
        runqueue::publish_local(slot, 0, runqueue::weight::value(slot));
        let before = runqueue::owner(slot);
        assert!(matches!(DispatchFrame::from_selection(turn, selection,
            super::super::donation_ledger::class_publication_sequence()), FrameCapture::Stale));
        assert_eq!(stack_resources::frame_readers::test_count(slot), 0);
        assert_eq!(runqueue::owner(slot), before);
        no_preparation(slot);
    });
}
#[test]
fn actual_current_claim_retains_original_funding_and_moves_allocation_once() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current)
            .prepare()
            .unwrap();
        assert!(funded.funding.funding.is_some());
        assert!(funded.claim());
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Running
        );
        finish_modeled_preparation(slot, slot);
    });
}
#[test]
fn actual_claim_opens_original_interval_before_return_without_scalar_identity_reacquisition() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current).prepare().unwrap();
        assert!(funded.claim());
        // No Scheduler opener or CPU preparation mailbox exists. The claim
        // already owns its original funding and allocation through publication.
        execution_interval::finish_return(turn, slot);
        let refused = crate::multitask::cpu_local::with_test_current_scalar(0, KERNEL, || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
                execution_interval::finish_return(turn, slot))).is_err()
        });
        assert!(refused);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
            execution_interval::finish_return(turn, KERNEL))).is_err());
        // A scalar descriptor is NOT the postclaim funding/identity owner.
        // Retain the actual original physical banks while fault-injecting it.
        let before = crate::multitask::current_identity::read(slot).unwrap();
        let mut changed = before;
        changed.pager_charge = None;
        crate::multitask::current_identity::publish_metadata(slot, Some(changed));
        execution_interval::finish_return(turn, slot);
        crate::multitask::current_identity::publish_metadata(slot, Some(before));
        finish_modeled_preparation(slot, slot);
    });
}
#[test]
fn actual_prepared_original_funder_cannot_be_rebound_from_a_later_scalar_charge_publication() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current).prepare().unwrap();
        let before = crate::multitask::current_identity::read(slot).unwrap();
        let mut changed = before;
        changed.pager_charge = None;
        // Model a later donor/publication change. The real prepared pin still
        // owns the earlier exact funder; a fresh lookup would mint free work.
        crate::multitask::current_identity::publish_metadata(slot, Some(changed));
        assert!(funded.claim());
        execution_interval::finish_return(turn, slot);
        let execution_interval::ConsumedInterval::Active(interval) =
            execution_interval::consume(0, slot, ID) else { panic!("missing original interval") };
        let (owner, outcome) = interval.charge(100, 1_000).unwrap();
        assert_eq!(owner, ID);
        assert_eq!(outcome.charged_ns, 1_000);
        crate::multitask::current_identity::publish_metadata(slot, Some(before));
        stack_resources::cpu_leases::publish(0, slot, slot);
        stack_resources::cpu_leases::commit(0);
    });
}
#[test]
fn actual_unused_original_funding_refusal_releases_final_closing_funder_outside_cpu_leaf() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current).prepare().unwrap();
        let identity = stack_resources::context::read(slot).unwrap().scheduling_context.identity();
        assert!(!scheduling_runtime::close_for_reclaim(slot, identity));
        super::super::retirement_cleanup::take_reap_pending();
        assert!(!funded.claim());
        assert!(super::super::retirement_cleanup::take_reap_pending());
        assert!(turn.claimed_return().is_none());
        assert!(matches!(execution_interval::consume(0, slot, ID),
            execution_interval::ConsumedInterval::AlreadySettled));
        no_preparation(slot);
    });
}
#[test]
fn actual_claimed_funding_survives_terminal_until_original_interval_settlement() {
    fixture(true, |slot, turn| {
        assert!(capture(slot, turn, FrameCustody::Current).prepare().unwrap().claim());
        let identity = stack_resources::context::read(slot).unwrap().scheduling_context.identity();
        assert!(!scheduling_runtime::close_for_reclaim(slot, identity));
        runqueue::retire(slot, runqueue::weight::value(slot));
        // Terminal owner publication cannot recycle the executing resource or
        // replace the already captured interval with an unbudgeted descriptor.
        execution_interval::finish_return(turn, slot);
        let execution_interval::ConsumedInterval::Active(interval) =
            execution_interval::consume(0, slot, ID) else { panic!("claim failed to open interval") };
        assert!(!scheduling_runtime::close_for_reclaim(slot, identity));
        let (owner, outcome) = interval.charge(100, 1_000).unwrap();
        assert_eq!(owner, ID);
        assert_eq!(outcome.charged_ns, 1_000);
        assert!(scheduling_runtime::close_for_reclaim(slot, identity));
        assert!(stack_resources::cpu_leases::owns_prepared(0, slot));
        // Earlier hardware ACK is explicitly modeled, not asserted as executed.
        stack_resources::cpu_leases::publish(0, slot, slot);
        stack_resources::cpu_leases::commit(0);
    });
}
#[test]
fn actual_selected_same_generation_frame_republication_refuses_final_leaf_cas_and_releases_funding()
{
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current)
            .prepare()
            .unwrap();
        let before = runqueue::owner(slot);
        let token = funded.frame.token;
        // Native fault injection of the actual frame publisher, not a forged
        // reader token. A production frame writer has its separate intent gate.
        let v = token.view;
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
        assert_eq!(runqueue::owner(slot), before);
        assert!(!funded.claim());
        assert_eq!(runqueue::owner(slot), before);
        no_preparation(slot);
    });
}
#[test]
fn actual_selected_republished_owner_refuses_without_membership_or_bank_changes() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current)
            .prepare()
            .unwrap();
        runqueue::publish_blocked(slot, 0, runqueue::weight::value(slot));
        runqueue::publish_local(slot, 0, runqueue::weight::value(slot));
        let before = runqueue::owner(slot);
        assert!(!funded.claim());
        assert_eq!(runqueue::owner(slot), before);
        no_preparation(slot);
    });
}
#[test]
fn actual_selected_other_reader_blocks_claim_until_reader_releases_without_funding_stage() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current)
            .prepare()
            .unwrap();
        let pin = frame_publication::pin_for_identity(funded.frame.token, ID).unwrap();
        // SAFETY: original borrowed turn excludes local IRQ/preemption and
        // this exact independent allocation pin precedes reader admission.
        let reader = unsafe { stack_resources::frame_readers::acquire(&pin, 0) }.unwrap();
        assert!(!funded.claim());
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        no_preparation(slot);
        drop(reader);
        drop(pin);
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_selected_affinity_exclusion_refuses_real_claim_and_original_funding_releases() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current)
            .prepare()
            .unwrap();
        // Only the task mask excludes CPU0: the process mask must not conceal
        // a broken task qualifier at the actual final claim boundary. Task
        // masks remain a subset of the process container, as production requires.
        runqueue::affinity_payload::set_affinity(slot, 2, 3, true);
        let before = runqueue::owner(slot);
        assert!(!funded.claim());
        assert_eq!(runqueue::owner(slot), before);
        no_preparation(slot);
    });
}
#[test]
fn actual_selected_terminal_after_funding_refuses_and_drops_original_grant() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current)
            .prepare()
            .unwrap();
        runqueue::retire(slot, runqueue::weight::value(slot));
        assert!(!funded.claim());
        assert!(runqueue::owner(slot).state.is_terminal());
        no_preparation(slot);
    });
}
#[test]
fn actual_selected_custody_and_saved_rsp_are_not_interchangeable_grants() {
    fixture(true, |slot, turn| {
        let generation = runqueue::owner(slot).generation;
        let rsp = runqueue::saved_rsp(slot);
        assert!(matches!(
            DispatchFrame::capture(turn, slot, ID, generation, rsp, FrameCustody::Local),
            FrameCapture::Stale
        ));
        assert!(matches!(
            DispatchFrame::capture(turn, slot, ID, generation, rsp, FrameCustody::Direct),
            FrameCapture::Stale
        ));
        assert!(matches!(
            DispatchFrame::capture(turn, slot, ID, generation, rsp + 8, FrameCustody::Current),
            FrameCapture::Stale
        ));
        no_preparation(slot);
    });
}
#[test]
fn actual_current_fallback_uses_same_frame_funding_and_claim_engine() {
    fixture(true, |slot, turn| {
        assert_eq!(claim_fallback(turn, KERNEL, super::super::donation_ledger::class_publication_sequence()), slot);
        finish_modeled_preparation(slot, slot);
    });
}

#[test]
fn actual_selected_local_peer_uses_real_allocation_mm_placement_and_funding_banks() {
    fixture(false, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Local).prepare().unwrap();
        assert!(funded.claim());
        assert!(crate::multitask::current_identity::execution::banks::owns_execution(0, slot));
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Running
        );
        finish_modeled_preparation(slot, KERNEL);
    });
}

#[test]
fn actual_selected_direct_peer_claims_exact_fifo_generation_without_fair_membership() {
    fixture(false, |slot, turn| {
        runqueue::publish_blocked(slot, 0, runqueue::weight::value(slot));
        runqueue::publish_direct_handoff(slot, 0);
        let before = runqueue::published_runnable_count(0);
        assert!(
            capture(slot, turn, FrameCustody::Direct)
                .prepare()
                .unwrap()
                .claim()
        );
        assert_eq!(runqueue::published_runnable_count(0), before);
        finish_modeled_preparation(slot, KERNEL);
    });
}

#[test]
fn actual_selected_direct_same_generation_frame_republication_refuses_final_leaf_cas() {
    fixture(false, |slot, turn| {
        runqueue::publish_blocked(slot, 0, runqueue::weight::value(slot));
        runqueue::publish_direct_handoff(slot, 0);
        let funded = capture(slot, turn, FrameCustody::Direct).prepare().unwrap();
        let token = funded.frame.token;
        let v = token.view;
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
        let before = runqueue::owner(slot);
        assert!(!funded.claim());
        assert_eq!(runqueue::owner(slot), before);
        no_preparation(slot);
    });
}

#[test]
fn actual_selected_original_scope_replacement_refuses_under_final_leaf_before_cas() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current)
            .prepare()
            .unwrap();
        let before = runqueue::owner(slot);
        let other = crate::multitask::cpu_local::install_test_current_owner(0, KERNEL);
        assert!(!funded.claim());
        assert_eq!(runqueue::owner(slot), before);
        drop(other);
        no_preparation(slot);
    });
}

#[test]
fn actual_selected_budget_close_refuses_without_prepared_cpu_grant() {
    fixture(true, |slot, turn| {
        let frame = capture(slot, turn, FrameCustody::Current);
        no_preparation(slot);
        assert!(frame.prepare().is_none());
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        no_preparation(slot);
    });
}

#[test]
fn actual_selected_original_funding_close_after_preparation_retains_pin_but_refuses_new_execution()
{
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current)
            .prepare()
            .unwrap();
        let context = stack_resources::context::read(slot).unwrap();
        // Irreversible actual close, not a read-only refcount probe.
        assert!(!scheduling_runtime::close_for_reclaim(
            slot,
            context.scheduling_context.identity()
        ));
        assert!(!funded.claim());
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        no_preparation(slot);
    });
}

#[test]
fn actual_selected_original_funding_exhausted_after_preparation_refuses_before_queue_cas() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current)
            .prepare()
            .unwrap();
        let stamp = current_identity::read_dispatch(slot)
            .unwrap()
            .pager_charge
            .unwrap();
        let other_interval = scheduling_runtime::pin_published_funding(stamp, 0, ID, 0).unwrap();
        let _ = other_interval.charge(0, 5000);
        let before = runqueue::owner(slot);
        assert!(!funded.claim());
        assert_eq!(runqueue::owner(slot), before);
        no_preparation(slot);
    });
}

#[test]
fn actual_selected_foreign_expected_uid_refuses_before_frame_and_funding_admission() {
    fixture(true, |slot, turn| {
        assert!(matches!(
            DispatchFrame::capture(
                turn,
                slot,
                ID + 1,
                runqueue::owner(slot).generation,
                runqueue::saved_rsp(slot),
                FrameCustody::Current
            ),
            FrameCapture::Stale
        ));
        no_preparation(slot);
    });
}

#[test]
fn actual_selected_live_guard_corruption_refuses_old_published_canary_snapshot() {
    fixture(true, |slot, turn| {
        let token = frame_publication::test_read(slot).unwrap();
        assert!(token.view.stack_canary_intact);
        let guard = (token.view.kernel_stack_base - super::super::TASK_STACK_GUARD_BYTES as u64)
            as *mut u64;
        // SAFETY: isolated original Current bank and exact allocated geometry;
        // modeled corruption is restored before assertions and real teardown.
        let old = unsafe { guard.read() };
        unsafe {
            guard.write(0);
        }
        let result = DispatchFrame::capture(
            turn,
            slot,
            ID,
            runqueue::owner(slot).generation,
            token.saved_rsp,
            FrameCustody::Current,
        );
        // SAFETY: restore the same original allocated guard before fixture teardown.
        unsafe {
            guard.write(old);
        }
        assert!(matches!(
            result,
            FrameCapture::Invalid("kernel stack guard was corrupted")
        ));
        no_preparation(slot);
    });
}

#[test]
fn actual_joint_local_claim_keeps_original_budget_and_class_leaves_through_owner_cas() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current).prepare().unwrap();
        runqueue::claim::probe_next_claim_admission(slot);
        assert!(funded.claim());
        // At the ACTUAL final resource/identity/placement CAS callback, a
        // competing close/debit/class writer cannot acquire these SAME cells.
        // This is native mutex exclusion, NOT target IRQ/lockdep/RSP proof.
        assert_eq!(runqueue::claim::take_claim_admission_probe(), 7);
        finish_modeled_preparation(slot, slot);
    });
}

#[test]
fn actual_joint_direct_claim_keeps_original_budget_and_class_leaves_through_owner_cas() {
    fixture(false, |slot, turn| {
        runqueue::publish_blocked(slot, 0, runqueue::weight::value(slot));
        runqueue::publish_direct_handoff(slot, 0);
        let funded = capture(slot, turn, FrameCustody::Direct).prepare().unwrap();
        runqueue::claim::probe_next_claim_admission(slot);
        assert!(funded.claim());
        assert_eq!(runqueue::claim::take_claim_admission_probe(), 7);
        finish_modeled_preparation(slot, KERNEL);
    });
}

#[test]
fn actual_joint_base_class_change_after_preparation_refuses_without_queue_or_bank_effects() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current).prepare().unwrap();
        let before = runqueue::owner(slot);
        let count = runqueue::published_runnable_count(0);
        let weight = runqueue::weight::value(slot);
        runqueue::weight::replace(slot, weight | super::super::SYSTEM_CLASS_WEIGHT_FLAG);
        assert!(!funded.claim());
        assert_eq!(runqueue::owner(slot), before);
        assert_eq!(runqueue::published_runnable_count(0), count);
        assert!(turn.claimed_return().is_none());
        no_preparation(slot);
    });
}

#[test]
fn actual_joint_original_selection_class_is_not_recaptured_after_frame_admission() {
    fixture(true, |slot, turn| {
        let original = super::super::donation_ledger::class_publication_sequence();
        let weight = runqueue::weight::value(slot);
        runqueue::weight::replace(slot, weight | super::super::SYSTEM_CLASS_WEIGHT_FLAG);
        let FrameCapture::Ready(frame) = DispatchFrame::capture_selected(turn, slot, ID,
            runqueue::owner(slot).generation, runqueue::saved_rsp(slot),
            FrameCustody::Current, original) else { panic!("original frame not admitted"); };
        let before = runqueue::owner(slot);
        assert!(!frame.prepare().unwrap().claim());
        assert_eq!(runqueue::owner(slot), before);
        no_preparation(slot);
    });
}

#[test]
fn actual_joint_fixed_idle_borrows_real_kernel_frame_without_late_class_or_funding_authority() {
    fixture(true, |slot, turn| {
        let idle_id = ID + 9;
        let ticket = super::super::root_slots::reserve(122, idle_id).unwrap();
        let idle = super::super::kernel_creation::publish_reserved(ticket, |_| {},
            119, 8, 16, 0x202, 0x1000, 0x4000, 0,
            |idle, _| runqueue::set_idle_slot(idle, true), |_| 1);
        let original_class = super::super::donation_ledger::class_publication_sequence();
        let FrameCapture::Ready(frame) = DispatchFrame::capture_selected(turn, idle, idle_id,
            runqueue::owner(idle).generation, runqueue::saved_rsp(idle),
            FrameCustody::Local, original_class) else { panic!("born kernel idle lost physical frame"); };
        let funded = frame.prepare().unwrap();
        assert!(funded.funding.funding.is_none());
        runqueue::weight::replace(slot,
            runqueue::weight::value(slot) | super::super::SYSTEM_CLASS_WEIGHT_FLAG);
        let before = runqueue::owner(idle);
        // Deliberately refuse the final exact CAS, so no native bank ACK or
        // physical RSP is manufactured. The real UID/resource/placement callback
        // must still be reached despite unrelated class publication.
        let attempted = funded.frame.with_admission(None, || {
            stack_resources::frame_readers::with_claim_activation_for_reader(idle, 0,
                Some(funded.frame.original_reader()), || Err::<(), ()>(()))
        }).unwrap().into_claimed().unwrap();
        assert!(attempted.0.is_err());
        assert!(attempted.1.is_none());
        assert!(attempted.2.is_some());
        assert_eq!(runqueue::owner(idle), before);
        assert!(!stack_resources::cpu_leases::owns_prepared(0, idle));
        drop(attempted);
        drop(funded);
        super::super::kernel_creation::tests::cleanup(idle);
    });
}

#[test]
fn actual_joint_refused_placement_returns_original_mm_and_uid_loans_before_any_drop() {
    fixture(true, |slot, turn| {
        let funded = capture(slot, turn, FrameCustody::Current).prepare().unwrap();
        runqueue::affinity_payload::set_affinity(slot, 2, 2, false);
        let refs = crate::multitask::current_identity::execution::count(slot);
        let attempt = funded.frame.with_admission(funded.funding.funding.as_ref(), || {
            stack_resources::frame_readers::with_claim_activation_for_reader(slot, 0,
                Some(funded.frame.original_reader()), || {
                panic!("refused placement reached final owner CAS");
            })
        }).expect("refused original loans were dropped under admission leaves");
        assert!(!attempt.is_claimed());
        let stack_resources::frame_readers::ClaimAttempt::Refused {
            activation: Some(activation), permit: Some(_),
        } = &attempt else {
            panic!("real user refusal lost original provisional MM/UID loans");
        };
        assert!(activation.root_for(slot, ID).is_some());
        assert_eq!(crate::multitask::current_identity::execution::count(slot), refs + 1);
        // SAME original pin observes that both actual budget mutexes and ledger
        // have left the callback, while original provisional loans still live.
        assert_eq!(funded.funding.funding.as_ref().unwrap().test_admission_exclusion(), (false, false));
        assert!(!super::super::donation_ledger::test_class_excluded());
        drop(attempt);
        assert_eq!(crate::multitask::current_identity::execution::count(slot), refs);
        drop(funded);
        runqueue::affinity_payload::set_affinity(slot, 1, 1, false);
        no_preparation(slot);
    });
}

#[test]
fn actual_joint_original_donation_producer_invalidates_selected_peer_before_claim() {
    super::super::irq_entry_tests::current_with_peer(true, |donor, peer| {
        let peer = peer.unwrap();
        let donor_id = current_identity::read_dispatch(donor).unwrap().task_id.unwrap();
        let peer_id = current_identity::read_dispatch(peer).unwrap().task_id.unwrap();
        runqueue::weight::replace(donor,
            runqueue::weight::value(donor) | super::super::SYSTEM_CLASS_WEIGHT_FLAG);
        dispatch_turn::with_original_current_model(donor, |turn| {
            let FrameCapture::Ready(frame) = DispatchFrame::capture(turn, peer, peer_id,
                runqueue::owner(peer).generation, runqueue::saved_rsp(peer), FrameCustody::Local)
                else { panic!("real born peer frame not admitted"); };
            let funded = frame.prepare().unwrap();
            let scope = turn.preemption();
            assert!(super::super::donation_publication::bind_with(scope, 8081, donor_id, peer_id, true));
            assert!(super::super::donation_ledger::inherited_system(peer));
            let before = runqueue::owner(peer);
            assert!(!funded.claim());
            assert_eq!(runqueue::owner(peer), before);
            assert!(turn.claimed_return().is_none());
            super::super::donation_ledger::release_reply(8081, super::super::ipc_donation::DonationNamespace::IpcReply);
            no_preparation(peer);
        });
    });
}
