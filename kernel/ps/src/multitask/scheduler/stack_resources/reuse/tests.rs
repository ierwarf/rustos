//! Owner: actual producer-last resource/identity/reuse witnesses.
//! Boundary: stack/context/nonce custody, not complete group exit or hardware.
//! Lifecycle: exact old UID remains closed through detached-but-unacknowledged prefix.
//! Concurrency: real competing creator reservation and reclaim under shared fixture order.
//! Failure: writer/ref/intent/identity/budget absence never masquerades as drained ACK.
//! Forbidden: test injection/recovery is not production reopen or eventual cleanup.
//! Evidence: real Ticket, initialized stack, kernel descriptor and owned reuse methods.
use super::*;
use crate::multitask::current_identity as identity;
use crate::multitask::scheduler::{
    self as scheduler, ConsoleSessionHandle, TaskContext, TaskStart,
};
use std::panic::{AssertUnwindSafe, catch_unwind};
const SLOT: usize = 104;
const ID: u64 = 104001;
fn metadata() -> TaskContext {
    TaskContext {
        scheduling_context: scheduler::scheduling_context::SchedulingContext::bind(SLOT, ID),
        saved_rsp: 0,        address_space_root: 0,        user_mode: false,
        user_abi: None,
        console_session: ConsoleSessionHandle::SYSTEM,
        process_handle: None,
        process_id: None,
        user_stack: None,
        windows_thread_state: None,
    }
}
fn prepare() {
    let ticket = scheduler::root_slots::reserve(SLOT, ID).unwrap();
    scheduler::execution_image::reserved_kernel_entry(
        &ticket,
        8,
        16,
        2,
        crate::multitask::noop_task_entry as *const () as u64,
        0,
    );
    ticket.begin_commit();
    ticket.install_context(metadata());
    ticket.publish_start(TaskStart {
        entry: crate::multitask::noop_task_entry,
        id: ID,
    });
    let base = storage_base(SLOT);
    publish_binding(
        SLOT,
        ID,
        scheduler::context_validation::PublishedFrameView {
            user_mode_task: false,
            kernel_process_task: false,
            kernel_stack_base: (base + scheduler::TASK_STACK_GUARD_BYTES) as u64,
            kernel_stack_top: scheduler::align_kernel_stack_top(base + scheduler::TASK_STACK_SIZE)
                as u64,
            alternate_stack_base: 0,
            alternate_stack_top: 0,
            stack_canary_intact: true,
            scheduler_storage_base: 0,
            scheduler_storage_end: 0,
        },
    );
    identity::publish_metadata(
        SLOT,
        Some(identity::TaskIdentity {
            task_id: Some(ID),
            ..identity::TaskIdentity::default()
        }),
    );
    assert!(scheduler::root_slots::admit_blocked(ticket).is_ok());
}
fn detach(reuse: &mut Reuse) -> Vec<u8> {
    context::clear(SLOT);
    task_start::clear(SLOT);
    identity::publish_metadata(SLOT, None);
    reuse.take_storage().unwrap()
}
#[test]
fn detached_metadata_prefix_keeps_new_creator_closed_until_exact_reuse_ack_and_clears_hidden_binding()
 {
    let _fixture = scheduler::tests::boxed_scheduler();
    prepare();
    let mut reuse = begin(SLOT, Some(ID)).unwrap();
    assert!(admission::reserve(SLOT, ID + 1).is_none());
    let old = detach(&mut reuse);
    assert!(identity::read(SLOT).is_none());
    assert!(admission::reserve(SLOT, ID + 1).is_none());
    assert!(context::read(SLOT).is_none());
    // Fault projection proves the reuse seal independently of close observation.
    RESOURCES[SLOT].with(|r| r.closing = false);
    assert!(admission::reserve(SLOT, ID + 1).is_none());
    RESOURCES[SLOT].with(|r| r.closing = true);
    reuse.finish();
    let next = scheduler::root_slots::reserve(SLOT, ID + 1).unwrap();
    identity::publish_metadata(
        SLOT,
        Some(identity::TaskIdentity {
            task_id: Some(ID + 1),
            ..identity::TaskIdentity::default()
        }),
    );
    scheduler::root_slots::cancel_unpublished(next);
    identity::clear(SLOT);
    drop(old);
}
#[test]
fn foreign_pristine_or_old_uid_cannot_close_live_slot_and_epoch_exhaustion_has_no_partial_gate() {
    let _fixture = scheduler::tests::boxed_scheduler();
    prepare();
    assert!(begin(SLOT, None).is_none());
    assert!(begin(SLOT, Some(ID + 1)).is_none());
    let old = RESOURCES[SLOT].with(|r| {
        let old = r.reservation_generation;
        r.reservation_generation = u64::MAX;
        old
    });
    assert!(begin(SLOT, Some(ID)).is_none());
    RESOURCES[SLOT].with(|r| {
        assert!(r.reclaim.is_none());
        assert!(!r.closing);
        r.reservation_generation = old;
    });
    let mut reuse = begin(SLOT, Some(ID)).unwrap();
    drop(detach(&mut reuse));
    reuse.finish();
}
#[test]
fn unowned_storage_detach_and_early_ack_cannot_spend_owned_reclaim_or_reopen_constructor() {
    let _fixture = scheduler::tests::boxed_scheduler();
    prepare();
    let reuse = begin(SLOT, Some(ID)).unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| take_storage(SLOT))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| reuse.finish())).is_err());
    assert!(admission::reserve(SLOT, ID + 1).is_none());
    // Native recovery of deliberately poisoned ownership, not a production ACK.
    RESOURCES[SLOT].with(|r| r.reclaim = None);
    let mut recovered = begin(SLOT, Some(ID)).unwrap();
    drop(detach(&mut recovered));
    recovered.finish();
}
#[test]
fn abandoned_detached_reclaim_quarantines_even_empty_storage_and_next_nonce_cannot_be_spent_by_old_key()
 {
    let _fixture = scheduler::tests::boxed_scheduler();
    prepare();
    let mut reuse = begin(SLOT, Some(ID)).unwrap();
    let old_key = reuse.key;
    drop(detach(&mut reuse));
    drop(reuse);
    assert!(admission::reserve(SLOT, ID + 1).is_none());
    RESOURCES[SLOT].with(|r| assert_eq!(r.reclaim.unwrap().phase, Phase::Poisoned));
    // Restore this exact old native receipt, then prove old nonce differs from next occupancy.
    RESOURCES[SLOT].with(|r| {
        r.reclaim = Some(Key {
            phase: Phase::Detached,
            ..old_key
        });
    });
    Reuse {
        slot: SLOT,
        key: Key {
            phase: Phase::Detached,
            ..old_key
        },
        live: true,
    }
    .finish();
    let next = admission::reserve(SLOT, ID + 1).unwrap();
    RESOURCES[SLOT].with(|r| assert!(r.reservation_generation > old_key.nonce));
    drop(next);
    RESOURCES[SLOT].with(|r| r.reservation = None);
}
#[test]
fn close_refuses_all_constructor_phases_without_partial_close_or_spending_creator() {
    let _fixture = scheduler::tests::boxed_scheduler();
    let ticket = scheduler::root_slots::reserve(SLOT, ID).unwrap();
    for phase in [
        admission::Phase::Reserved,
        admission::Phase::Committing,
        admission::Phase::Canceling,
        admission::Phase::Poisoned,
    ] {
        ticket.set_phase_for_test(phase);
        assert!(!close_for_reclaim(SLOT));
        RESOURCES[SLOT].with(|r| assert!(!r.closing));
    }
    ticket.set_phase_for_test(admission::Phase::Reserved);
    scheduler::root_slots::cancel_unpublished(ticket);
}

#[test]
fn pristine_descriptor_absence_cannot_discard_unowned_close_or_reader_custody_on_reservation() {
    let _fixture = scheduler::tests::boxed_scheduler();
    let old = RESOURCES[SLOT].with(|r| r.reservation_generation);
    for (closing, readers) in [(true, 0), (false, 1)] {
        RESOURCES[SLOT].with(|r| r.closing = closing);
        // ORDERING: native fixture models reader custody on the real counter.
        RESOURCES[SLOT].readers.store(readers, Ordering::Release);
        assert!(admission::reserve(SLOT, ID).is_none());
        RESOURCES[SLOT].with(|r| {
            assert_eq!(r.reservation_generation, old);
            assert!(r.reservation.is_none());
        });
    }
    RESOURCES[SLOT].with(|r| r.closing = false);
    // ORDERING: native fixture models reader custody on the real counter.
    RESOURCES[SLOT].readers.store(0, Ordering::Release);
    let ticket = admission::reserve(SLOT, ID).unwrap();
    assert!(ticket.begin_cancel());
    ticket.finish_cancel();
}
