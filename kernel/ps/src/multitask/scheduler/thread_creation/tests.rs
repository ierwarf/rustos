//! Owner: actual independent thread reservation and publication witnesses.
//! Boundary: shared real Ticket/physical MM fixture, not native catalog construction.
//! Lifecycle: exact producer and original MM survive cancellation and suspended Slot ACK.
//! Concurrency: canonical scheduler fixture and real parent binding/cohort admission.
//! Failure: missing/retired caller and publication refusal never imply reuse or completion.
//! Forbidden: modeled MM or scalar receipts as physical switch/external completion proof.
//! Evidence: actual common reservation/cancel/frame/runtime/membership publication code.
use super::*;
use crate::multitask::scheduler::process_affinity::tests::affinity_fixture;
pub(in crate::multitask::scheduler) mod legacy_witnesses;
pub(in crate::multitask::scheduler) use legacy_witnesses::rejected_attachment;
const ROOT: usize = 124;
const ID: u64 = 1245501;
/// Actual same-process sibling birth plus actual activation for terminal tests.
/// Current/topology are the enclosing fixture's explicit modeled inputs.
pub(in crate::multitask::scheduler) fn active_sibling(
    parent: usize,
    id: u64,
    boot: UserTaskBootstrap,
) -> usize {
    let creator = capture_creator(parent).unwrap();
    let r = reserve_with(id, &creator).unwrap();
    let publication = Publication::capture(&r, boot.abi).unwrap();
    let mut turn = nucleus_core::util::lockdep::DispatchPreemption::enter();
    turn.mask_irqs();
    let (slot, _) = publish(r, boot, 0x23, 0x1b, 0x202, publication, &turn, |_, _| {}).unwrap();
    let activation = super::super::activation::Activation::capture(&[(slot, id)]).unwrap();
    super::super::root_creation::tests::with_activation_current(&turn, || {
        assert!(activation.commit_owned(&turn, |_| 1, || {}));
    });
    slot
}

pub(super) fn reserve_actual() -> Reservation {
    let creator = capture_creator(ROOT).unwrap();
    reserve_with(ID + 8, &creator).unwrap()
}

pub(super) fn bootstrap(abi: UserAbi) -> UserTaskBootstrap {
    let mut result = UserTaskBootstrap::new(
        abi,
        x86_64::VirtAddr::new(0x400000),
        x86_64::VirtAddr::new(0x700000),
    );
    result.registers.rax = 42;
    result
}
/// Native terminal receipt models external producer ACK only. The physical
/// Resource/Reset seals and actual process detachment still own this cleanup;
/// this fixture is not a kernel Executor/hardware joint-refinement witness.
pub(in crate::multitask::scheduler) fn release_born(slot: usize, id: u64, handle: ProcessHandle) {
    use super::super::retirement_cleanup::producer::Producer;
    let context = stack_resources::context::read(slot).unwrap();
    let mut turn =
        super::super::super::process_placement::terminal::Turn::begin(handle, (slot, id)).unwrap();
    identity::update_live_lifecycle(slot, id, identity::LifecycleGate::Retired, true);
    runqueue::retire(slot, NICE_0_LOAD);
    turn.mark_stopped();
    let mut producer = Producer::begin(slot, Some(id), TaskRetireReason::Exited, true).unwrap();
    producer.bind_process_turn(turn.credential());
    producer.begin_effects();
    let receipt = producer.publish(None, RetirementSideEffect::new(Some(id), None));
    retirement_cleanup::ack_kernel_fixture(slot);
    turn.finish(receipt);
    let mut reset = retirement_cleanup::reuse::begin(slot, Some(id)).unwrap();
    assert!(scheduling_runtime::close_for_reclaim(
        slot,
        context.scheduling_context.identity()
    ));
    let mut reuse = stack_resources::reuse::begin_retired(slot, id).unwrap();
    // Real physical owner withdraws ONLY this fixture's hints before reuse;
    // do not leave birth/activation FIFO entries for the next shared fixture.
    super::super::dispatch_policy::remove_reclaimed_slot(slot);
    reset.mark_effects();
    reset.reset();
    runqueue::release_retired(slot);
    scheduling_runtime::release(slot, context.scheduling_context.identity());
    stack_resources::context::clear(slot);
    linux_thread_state::clear_reclaimed(slot);
    runqueue::simd_tls::reset_simd_state(slot);
    stack_resources::task_start::clear(slot);
    frame_publication::clear(slot);
    let storage = reuse.take_storage();
    process_table::detach_task(handle).unwrap();
    drop(storage);
    reset.finish();
    reuse.finish();
}

#[test]
fn actual_publication_keeps_parent_mm_and_ticket_through_blocked_frame_membership_and_slot_ack() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |h| {
        let r = reserve_actual();
        let slot = r.ticket().slot();
        let publication = Publication::capture(&r, UserAbi::Linux).unwrap();
        assert!(identity::cohort::busy(ROOT));
        assert_eq!(identity::execution::count(ROOT), 2);
        assert!(identity::with_reclaim_binding(ROOT, Some(ID), || ()).is_none());
        let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
        preemption.mask_irqs();
        let result = publish(
            r,
            bootstrap(UserAbi::Linux),
            0x23,
            0x1b,
            0x202,
            publication,
            &preemption,
            |r, p| {
                assert!(r.ticket().is_committing());
                assert!(p.membership.matches_ticket(r.ticket()));
                assert_eq!(
                    runqueue::owner(slot).state,
                    runqueue::RunOwnerState::Blocked
                );
                assert!(!stack_resources::admission::wake_allowed(slot));
                assert!(
                    identity::execution::with_claim(slot, |_| panic!(
                        "unacknowledged child admitted"
                    ))
                    .is_none()
                );
                assert!(identity::cohort::Cohort::begin(&[(ROOT, ID)]).is_none());
                assert_eq!(identity::execution::count(ROOT), 2);
                let token = frame_publication::test_read(slot).unwrap();
                assert_eq!(token.owner_generation, runqueue::owner(slot).generation);
                // SAFETY: actual reserved allocation/reader owners must refuse
                // this fully visible descriptor prefix until the real Slot ACK.
                assert!(
                    unsafe { frame_observation::observe(slot, 0, token.saved_rsp, false) }
                        .is_none()
                );
            },
        )
        .unwrap();
        drop(preemption);
        assert_eq!(result, (slot, NICE_0_LOAD));
        assert!(!identity::cohort::busy(ROOT));
        assert_eq!(identity::execution::count(ROOT), 0);
        assert!(identity::lifecycle(slot).suspended);
        assert!(!identity::lifecycle(slot).retired);
        assert!(stack_resources::admission::wake_allowed(slot));
        assert!(
            identity::execution::with_claim(slot, |_| panic!("suspended child dispatched"))
                .is_none()
        );
        let token = frame_publication::test_read(slot).unwrap();
        // SAFETY: the actual non-executing FrameReader and StackPin join a
        // complete Blocked generation and exclude all writers/claims during copy.
        let frame = unsafe { frame_observation::observe(slot, 0, token.saved_rsp, false) }.unwrap();
        assert_eq!(frame.saved.rax, 42);
        assert_eq!(frame.saved.rip, 0x400000);
        assert_eq!(frame.saved.rsp, 0x700000);
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(slot),
            (7, 7, false)
        );
        assert_eq!(super::super::super::process_placement::mask(h), Some(7));
        release_born(slot, ID + 8, h);
    });
}

#[test]
pub(in crate::multitask::scheduler) fn publication_inherits_new_complete_process_mask_after_reservation_not_old_snapshot() {
    affinity_fixture(UserAbi::Windows, UserAbi::Windows, false, |h| {
        let r = reserve_actual();
        let slot = r.ticket().slot();
        super::super::process_affinity::tests::change_blocked_process_mask(h, 3);
        let publication = Publication::capture(&r, UserAbi::Windows).unwrap();
        assert_eq!(publication.task_mask, 3);
        assert_eq!(publication.membership.mask(), 3);
        assert!(super::super::super::process_placement::Change::begin(h, (ROOT, ID)).is_none());
        assert!(identity::cohort::Cohort::begin(&[(ROOT, ID)]).is_none());
        let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
        preemption.mask_irqs();
        publish(
            r,
            bootstrap(UserAbi::Windows),
            0x23,
            0x1b,
            0x202,
            publication,
            &preemption,
            |_, _| {},
        )
        .unwrap();
        drop(preemption);
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(slot),
            (3, 3, false)
        );
        release_born(slot, ID + 8, h);
    });
}

#[test]
pub(in crate::multitask::scheduler) fn final_retired_creator_or_wrong_abi_refusal_cancels_real_unpublished_ticket_and_attachment() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |_| {
        let before = process_table::thread_count_by_pid(ID).unwrap();
        let r = reserve_actual();
        let slot = r.ticket().slot();
        assert!(commit(r, bootstrap(UserAbi::Windows), 0x23, 0x1b, 0x202).is_none());
        assert!(!stack_resources::allocated(slot));
        assert_eq!(process_table::thread_count_by_pid(ID), Some(before));
        let r = reserve_actual();
        identity::update_live_lifecycle(ROOT, ID, identity::LifecycleGate::Retired, true);
        assert!(commit(r, bootstrap(UserAbi::Linux), 0x23, 0x1b, 0x202).is_none());
        assert!(!stack_resources::allocated(slot));
        assert_eq!(process_table::thread_count_by_pid(ID), Some(before));
        assert!(!identity::cohort::busy(ROOT));
    });
}
#[test]
fn actual_reservation_and_cancel_retain_ticket_mm_and_attachment_until_complete_ack() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |h| {
        let before = process_table::thread_count_by_pid(ID).unwrap();
        let r = reserve_actual();
        let slot = r.ticket().slot();
        assert!(r.ticket().is_reserved_current());
        assert_eq!(r.process_handle(), h);
        assert_eq!(r.process_id(), ID);
        assert!(r.mm.as_ref().unwrap().matches(h, 0x4000));
        assert_eq!(process_table::thread_count_by_pid(ID), Some(before + 1));
        assert!(stack_resources::allocated(slot));
        assert!(stack_resources::context::read(slot).is_none());
        assert!(stack_resources::admission::reserve(slot, ID + 9).is_none());
        assert!(
            identity::execution::with_claim(slot, |_| panic!("reserved thread admitted")).is_none()
        );
        cancel(r);
        assert!(!stack_resources::allocated(slot));
        assert_eq!(process_table::thread_count_by_pid(ID), Some(before));
        let replacement = stack_resources::admission::reserve(slot, ID + 9).unwrap();
        assert!(replacement.begin_cancel());
        replacement.finish_cancel();
    });
}
#[test]
fn retired_creator_cannot_capture_physical_reservation_input() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |_| {
        identity::update_live_lifecycle(ROOT, ID, identity::LifecycleGate::Retired, true);
        assert!(capture_creator(ROOT).is_none());
        assert_eq!(identity::execution::count(ROOT), 0);
    });
}
