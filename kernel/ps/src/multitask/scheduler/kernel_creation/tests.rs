//! Owner: actual independent dynamic kernel producer and pre-ACK bank witnesses.
//! Boundary: common Ticket/frame/context/queue code; modeled kernel root only.
//! Lifecycle: complete Slot ACK retains backing until wake/terminal race finishes.
//! Concurrency: canonical native fixtures and real queue/resource owners.
//! Failure: zero UID valid, prefix/retirement/reuse and foreign publication refuse.
//! Forbidden: host terminal fixture cleanup as external/hardware teardown proof.
//! Evidence: actual constructor, physical publication loan, frames and fair floor.
use super::*;
use crate::multitask::current_identity as identity;
const SLOT: usize = 122;
const ID: u64 = 122011;
pub(in crate::multitask::scheduler) fn cleanup(slot: usize) {
    let context = stack_resources::context::read(slot).unwrap();
    let id = stack_resources::task_start::read(slot).unwrap().id;
    if !identity::lifecycle(slot).retired {
        identity::update_live_lifecycle(slot, id, identity::LifecycleGate::Retired, true);
        runqueue::retire(slot, runqueue::weight::value(slot));
    }
    assert!(scheduling_runtime::close_for_reclaim(
        slot,
        context.scheduling_context.identity()
    ));
    let mut reuse = stack_resources::reuse::begin_retired(slot, id).unwrap();
    runqueue::release_retired(slot);
    scheduling_runtime::release(slot, context.scheduling_context.identity());
    stack_resources::context::clear(slot);
    runqueue::simd_tls::reset_simd_state(slot);
    stack_resources::task_start::clear(slot);
    frame_publication::clear(slot);
    drop(reuse.take_storage());
    reuse.finish();
}
fn birth(id: u64, v: u64, f: impl FnOnce(usize, u64)) -> usize {
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
    preemption.mask_irqs();
    let ticket = root_slots::reserve(SLOT, id).unwrap();
    publish_reserved(
        ticket,
        |_| {},
        119,
        8,
        16,
        0x202,
        0x1000,
        0x4000,
        v,
        f,
        |_| 1,
    )
}
#[test]
fn actual_kernel_publication_keeps_physical_child_through_blocked_ack_and_runnable_frame_publication()
 {
    let _fixture = super::super::tests::boxed_scheduler();
    let slot = birth(ID, 17, |slot, id| {
        assert_eq!((slot, id), (SLOT, ID));
        assert!(!stack_resources::admission::is_reserved(slot));
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Blocked
        );
        // Failed close is irreversible; test actual reset refusal without
        // sealing a live binding or substituting a catalog lifetime predicate.
        assert!(std::panic::catch_unwind(|| stack_resources::context::clear(slot)).is_err());
        let token = frame_publication::test_read(slot).unwrap();
        assert_eq!(token.owner_generation, runqueue::owner(slot).generation);
        // SAFETY: exact Blocked reader/bank joins exclude writes/execution.
        let frame = unsafe { frame_observation::observe(slot, 0, token.saved_rsp, false) }.unwrap();
        assert_eq!(frame.saved.rip, 0x1000);
        assert_eq!(frame.saved.rflags, 0x202);
        assert_eq!(frame.saved.cs, 8);
        assert_eq!(frame.saved.ss, 16);
        assert_eq!(stack_resources::task_start::read(slot).unwrap().id, ID);
    });
    assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
    assert_eq!(runqueue::vruntime(slot), 17);
    let token = frame_publication::test_read(slot).unwrap();
    assert_eq!(token.owner_generation, runqueue::owner(slot).generation);
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
    preemption.mask_irqs();
    assert_eq!(
        creation_floor(preemption.cpu()),
        17 + SCHED_NEW_TASK_VRUNTIME_PENALTY_NS
    );
    for gate in [
        identity::LifecycleGate::Suspended,
        identity::LifecycleGate::JobStopped,
        identity::LifecycleGate::ExecQuiesced,
    ] {
        identity::update_live_lifecycle(slot, ID, gate, true);
        assert_eq!(
            creation_floor(preemption.cpu()),
            SCHED_NEW_TASK_VRUNTIME_PENALTY_NS
        );
        identity::update_live_lifecycle(slot, ID, gate, false);
    }
    let generation = runqueue::owner(slot).generation;
    frame_publication::bind_local_owner_generation(slot, generation + 1);
    assert_eq!(
        creation_floor(preemption.cpu()),
        SCHED_NEW_TASK_VRUNTIME_PENALTY_NS
    );
    frame_publication::bind_local_owner_generation(slot, generation);
    assert_eq!(
        creation_floor(preemption.cpu()),
        17 + SCHED_NEW_TASK_VRUNTIME_PENALTY_NS
    );
    drop(preemption);
    cleanup(slot);
}
#[test]
fn exact_kernel_zero_uid_is_complete_publication_not_absence_or_resume_permission() {
    let _fixture = super::super::tests::boxed_scheduler();
    let slot = birth(0, 0, |slot, id| {
        assert_eq!(id, 0);
        assert_eq!(identity::read_dispatch(slot).unwrap().task_id, Some(0));
        assert_eq!(runqueue::affinity_payload::affinity_snapshot(slot), (u64::MAX, u64::MAX, false));
        assert!(std::panic::catch_unwind(|| stack_resources::context::clear(slot)).is_err());
        assert!(
            identity::execution::kernel_executor::Executor::capture_binding(slot, 1, false)
                .is_none()
        );
    });
    assert_eq!(stack_resources::task_start::read(slot).unwrap().id, 0);
    cleanup(slot);
}
#[test]
fn stop_after_slot_ack_cannot_reuse_child_before_wake_and_never_resurrects_terminal_owner() {
    let _fixture = super::super::tests::boxed_scheduler();
    let slot = birth(ID, 0, |slot, id| {
        identity::update_live_lifecycle(slot, id, identity::LifecycleGate::Retired, true);
        runqueue::retire(slot, runqueue::weight::value(slot));
        let context = stack_resources::context::read(slot).unwrap();
        assert!(scheduling_runtime::close_for_reclaim(
            slot,
            context.scheduling_context.identity()
        ));
        assert!(stack_resources::reuse::begin_retired(slot, id).is_none());
        assert!(stack_resources::admission::reserve(slot, id + 1).is_none());
    });
    assert_eq!(
        runqueue::owner(slot).state,
        runqueue::RunOwnerState::Retired
    );
    assert!(identity::lifecycle(slot).retired);
    assert_eq!(stack_resources::task_start::read(slot).unwrap().id, ID);
    cleanup(slot);
}
#[test]
fn uncommitted_ticket_cannot_create_publication_backing_loan() {
    let _fixture = super::super::tests::boxed_scheduler();
    let ticket = root_slots::reserve(SLOT, ID).unwrap();
    let view = PublishedFrameView {
        user_mode_task: false,
        kernel_process_task: false,
        kernel_stack_base: 0,
        kernel_stack_top: 0,
        alternate_stack_base: 0,
        alternate_stack_top: 0,
        stack_canary_intact: true,
        scheduler_storage_base: 0,
        scheduler_storage_end: 0,
    };
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || ticket.retain_publication(view)
        ))
        .is_err()
    );
    assert!(ticket.is_reserved_current());
    root_slots::cancel_unpublished(ticket);
}
