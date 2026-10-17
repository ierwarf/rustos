use super::super::{context, task_start};
use super::*;
use crate::multitask::scheduler::{
    self, context_validation::PublishedFrameView, runqueue, scheduling_runtime,
};
use alloc::{vec, vec::Vec};
use std::panic::{AssertUnwindSafe, catch_unwind};
const SLOT: usize = 116;
const ID: u64 = 116001;
fn cancel(ticket: Ticket) {
    assert!(ticket.begin_cancel());
    drop(ticket.take_storage());
    ticket.finish_cancel();
}
#[test]
fn cancellation_retains_slot_through_external_release_and_stale_generation_cannot_spend_reuse() {
    let _serial = scheduling_runtime::test_serial_guard();
    let first = reserve(SLOT, ID).unwrap();
    let generation = first.generation;
    first.install_storage(vec![0; scheduler::TASK_STACK_SIZE]);
    assert!(first.is_current(SLOT, ID));
    assert!(first.begin_cancel());
    assert!(is_reserved(SLOT));
    assert!(reserve(SLOT, ID + 1).is_none());
    drop(first.take_storage());
    assert!(is_reserved(SLOT));
    first.finish_cancel();
    let next = reserve(SLOT, ID).unwrap(); // isolate generation even with same UID
    let stale = Ticket {
        slot: SLOT,
        task_id: ID,
        generation,
        live: true,
    };
    assert!(!stale.is_current(SLOT, ID));
    assert!(!stale.begin_cancel());
    drop(stale); // stale abandonment cannot poison the replacement either
    assert!(next.is_current(SLOT, ID));
    let foreign = Ticket {
        slot: SLOT,
        task_id: ID + 1,
        generation: next.generation,
        live: true,
    };
    assert!(!foreign.is_current(SLOT, ID + 1));
    assert!(!foreign.begin_cancel());
    drop(foreign);
    assert!(next.is_current(SLOT, ID));
    cancel(next);
}
#[test]
fn ordinary_allocation_and_detachment_cannot_cross_a_reserved_slot() {
    let _serial = scheduling_runtime::test_serial_guard();
    let ticket = reserve(SLOT, ID).unwrap();
    assert!(
        catch_unwind(AssertUnwindSafe(|| super::super::install_storage(
            SLOT,
            vec![0; scheduler::TASK_STACK_SIZE]
        )))
        .is_err()
    );
    ticket.install_storage(vec![0; scheduler::TASK_STACK_SIZE]);
    assert!(catch_unwind(AssertUnwindSafe(|| super::super::take_storage(SLOT))).is_err());
    assert!(ticket.is_current(SLOT, ID));
    cancel(ticket);
}

#[test]
fn cancellation_ack_cannot_reopen_before_storage_release() {
    let _serial = scheduling_runtime::test_serial_guard();
    let ticket = reserve(SLOT, ID).unwrap();
    ticket.install_storage(vec![0; scheduler::TASK_STACK_SIZE]);
    assert!(ticket.begin_cancel());
    assert!(catch_unwind(AssertUnwindSafe(|| ticket.finish_cancel())).is_err());
    assert!(is_reserved(SLOT));
    assert!(super::super::allocated(SLOT));
    assert!(reserve(SLOT, ID + 1).is_none());
    // Fixture-only recovery after proving production quarantine.
    RESOURCES[SLOT].with(|resources| resources.reservation = None);
    drop(super::super::take_storage(SLOT));
}
#[test]
fn concurrent_reservations_have_one_slot_owner_without_a_directory_guard() {
    let _serial = scheduling_runtime::test_serial_guard();
    let winners: Vec<_> = (0..8)
        .map(|offset| std::thread::spawn(move || reserve(SLOT, ID + offset)))
        .collect::<Vec<_>>()
        .into_iter()
        .filter_map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(winners.len(), 1);
    cancel(winners.into_iter().next().unwrap());
}
#[test]
fn abandoned_reservation_poison_is_not_implicit_rollback_or_slot_reopening() {
    let _serial = scheduling_runtime::test_serial_guard();
    let ticket = reserve(SLOT, ID).unwrap();
    drop(ticket);
    assert!(is_reserved(SLOT));
    assert!(reserve(SLOT, ID + 1).is_none());
    assert!(catch_unwind(AssertUnwindSafe(|| super::super::take_storage(SLOT))).is_err());
    assert_eq!(
        RESOURCES[SLOT].with(|resources| resources.reservation.unwrap().phase),
        Phase::Poisoned
    );
    // Fixture recovery only, never a production poison-reset permission.
    RESOURCES[SLOT].with(|resources| resources.reservation = None);
}
fn publish(ticket: &Ticket) {
    ticket.install_storage(vec![0; scheduler::TASK_STACK_SIZE]);
    ticket.begin_commit();
    ticket.install_context(context::tests::metadata(SLOT, ID));
    ticket.publish_start(scheduler::TaskStart {
        entry: crate::multitask::noop_task_entry,
        id: ID,
    });
    let base = super::super::storage_base(SLOT);
    super::super::publish_binding(
        SLOT,
        ID,
        PublishedFrameView {
            user_mode_task: true,
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
}

fn cleanup_published_fixture() {
    assert!(super::super::close_for_reclaim(SLOT));
    task_start::clear(SLOT);
    context::clear(SLOT);
    drop(super::super::take_storage(SLOT));
    crate::multitask::current_identity::clear(SLOT);
}
#[test]
fn context_and_descriptor_installation_require_the_exact_committing_ticket() {
    let _serial = scheduling_runtime::test_serial_guard();
    let ticket = reserve(SLOT, ID).unwrap();
    ticket.install_storage(vec![0; scheduler::TASK_STACK_SIZE]);
    assert!(
        catch_unwind(AssertUnwindSafe(
            || ticket.install_context(context::tests::metadata(SLOT, ID))
        ))
        .is_err()
    );
    ticket.begin_commit();
    // First real boot exposed empty-Identity native fixture admission hiding
    // initial affinity publication before the constructor's exact UID. The
    // original Ticket is retained, but cannot substitute for that publication.
    assert_eq!(crate::multitask::current_identity::read(SLOT), None);
    let before = runqueue::affinity_payload::affinity_snapshot(SLOT);
    assert!(!runqueue::affinity_payload::initialize_reserved(&ticket, 1, 1));
    assert_eq!(runqueue::affinity_payload::affinity_snapshot(SLOT), before);
    assert!(
        catch_unwind(AssertUnwindSafe(|| context::install(
            SLOT,
            context::tests::metadata(SLOT, ID)
        )))
        .is_err()
    );
    let foreign = Ticket {
        slot: SLOT,
        task_id: ID,
        generation: ticket.generation + 1,
        live: true,
    };
    assert!(
        catch_unwind(AssertUnwindSafe(
            || foreign.install_context(context::tests::metadata(SLOT, ID))
        ))
        .is_err()
    );
    drop(foreign);
    ticket.install_context(context::tests::metadata(SLOT, ID));
    let start = scheduler::TaskStart {
        entry: crate::multitask::noop_task_entry,
        id: ID,
    };
    assert!(catch_unwind(AssertUnwindSafe(|| task_start::publish(SLOT, start))).is_err());
    assert!(
        catch_unwind(AssertUnwindSafe(|| ticket.publish_start(
            scheduler::TaskStart {
                id: ID + 1,
                ..start
            }
        )))
        .is_err()
    );
    ticket.publish_start(start);
    drop(ticket);
    // Fixture recovery does not authorize production rollback of partial birth.
    RESOURCES[SLOT].with(|resources| resources.reservation = None);
    cleanup_published_fixture();
}
#[test]
fn publication_ack_refuses_a_foreign_published_identity_even_after_blocked_admission() {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let ticket = reserve(SLOT, ID).unwrap();
    publish(&ticket);
    runqueue::admit_blocked(SLOT);
    crate::multitask::current_identity::publish_metadata(
        SLOT,
        Some(crate::multitask::current_identity::TaskIdentity {
            task_id: Some(ID + 1),
            ..Default::default()
        }),
    );
    assert!(catch_unwind(AssertUnwindSafe(|| ticket.finish_commit())).is_err());
    assert!(is_reserved(SLOT));
    assert!(reserve(SLOT, ID + 1).is_none());
    RESOURCES[SLOT].with(|resources| resources.reservation = None);
    cleanup_published_fixture();
}
#[test]
fn publication_ack_follows_exact_context_descriptor_and_blocked_identity_admission() {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let ticket = reserve(SLOT, ID).unwrap();
    publish(&ticket);
    assert!(is_reserved(SLOT));
    assert!(!wake_allowed(SLOT)); // complete descriptor is not constructor ACK
    assert!(reserve(SLOT, ID + 1).is_none());
    runqueue::admit_blocked(SLOT);
    assert!(!wake_allowed(SLOT)); // Blocked prefix alone cannot authorize wake
    crate::multitask::current_identity::publish_metadata(
        SLOT,
        Some(crate::multitask::current_identity::TaskIdentity {
            task_id: Some(ID),
            ..Default::default()
        }),
    );
    ticket.finish_commit();
    assert!(!is_reserved(SLOT));
    assert!(wake_allowed(SLOT));
    assert!(reserve(SLOT, ID + 1).is_none()); // live context remains authority
    assert!(super::super::close_for_reclaim(SLOT));
    task_start::clear(SLOT);
    context::clear(SLOT);
    drop(super::super::take_storage(SLOT));
    crate::multitask::current_identity::clear(SLOT);
}
#[test]
fn partial_commit_cannot_ack_or_reopen_before_runqueue_publication() {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let ticket = reserve(SLOT, ID).unwrap();
    ticket.begin_commit();
    assert!(catch_unwind(AssertUnwindSafe(|| ticket.finish_commit())).is_err());
    assert!(is_reserved(SLOT));
    assert!(reserve(SLOT, ID + 1).is_none());
    assert_eq!(
        RESOURCES[SLOT].with(|resources| resources.reservation.unwrap().phase),
        Phase::Poisoned
    );
    RESOURCES[SLOT].with(|resources| resources.reservation = None);
}

#[test]
fn reservation_blocks_cold_binding_in_every_phase_even_when_descriptor_and_identity_are_complete() {
    use crate::multitask::current_identity as identity;
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    identity::clear(SLOT);
    let ticket = reserve(SLOT, ID).unwrap();
    publish(&ticket);
    identity::publish_metadata(
        SLOT,
        Some(identity::TaskIdentity {
            task_id: Some(ID),
            ..Default::default()
        }),
    );
    identity::update_live_lifecycle(SLOT, ID, identity::LifecycleGate::Suspended, true);
    for phase in [
        Phase::Reserved,
        Phase::Committing,
        Phase::Canceling,
        Phase::Poisoned,
    ] {
        RESOURCES[SLOT].with(|resources| resources.reservation.as_mut().unwrap().phase = phase);
        assert!(identity::cohort::Cohort::begin_activation(&[(SLOT, ID)]).is_none());
        assert_eq!(identity::execution::count(SLOT), 0);
    }
    RESOURCES[SLOT]
        .with(|resources| resources.reservation.as_mut().unwrap().phase = Phase::Committing);
    runqueue::admit_blocked(SLOT);
    ticket.finish_commit();
    let cohort = identity::cohort::Cohort::begin_activation(&[(SLOT, ID)]).unwrap();
    assert_eq!(identity::execution::count(SLOT), 1);
    drop(cohort);
    cleanup_published_fixture();
}

#[test]
fn borrowed_binding_and_exec_intent_refuse_reservation_before_any_nonce_or_storage_effect() {
    use crate::multitask::{current_identity as identity, process_table};
    let _serial = scheduling_runtime::test_serial_guard();
    identity::clear(SLOT);
    identity::publish_metadata(
        SLOT,
        Some(identity::TaskIdentity {
            task_id: Some(ID),
            ..Default::default()
        }),
    );
    let cohort = identity::cohort::Cohort::begin(&[(SLOT, ID)]).unwrap();
    let before = RESOURCES[SLOT].with(|resources| resources.reservation_generation);
    assert!(reserve(SLOT, ID + 1).is_none());
    assert_eq!(
        RESOURCES[SLOT].with(|resources| resources.reservation_generation),
        before
    );
    drop(cohort);
    let stamp = process_table::test_exec_stamp(2, 3, 17);
    identity::publish_metadata(
        SLOT,
        Some(identity::TaskIdentity {
            task_id: Some(ID),
            user_mode: true,
            abi: Some(crate::user::abi::UserAbi::Linux),
            process_handle: Some(stamp.process_handle()),
            process_id: Some(ID),
            ..Default::default()
        }),
    );
    let intent = identity::exec_intent::begin(SLOT, ID, stamp, false).unwrap();
    assert!(reserve(SLOT, ID + 1).is_none());
    assert_eq!(
        RESOURCES[SLOT].with(|resources| resources.reservation_generation),
        before
    );
    intent.prepare_cancel();
    intent.cancel(process_table::test_exec_cancellation(stamp));
    identity::clear(SLOT);
    cancel(reserve(SLOT, ID + 1).unwrap());
}

#[test]
fn reserved_frame_bytes_require_exact_live_ticket_and_cannot_reenter_or_replay_generation() {
    let _serial = scheduling_runtime::test_serial_guard();
    let first = reserve(SLOT, ID).unwrap();
    first.install_storage(vec![0; scheduler::TASK_STACK_SIZE]);
    assert!(
        catch_unwind(AssertUnwindSafe(|| super::super::initialize_storage(
            SLOT,
            |_| ()
        )))
        .is_err()
    );
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            super::super::frame_access::with_unpublished(SLOT, |_| ())
        }))
        .is_err()
    );
    let foreign_slot = Ticket {
        slot: SLOT + 1,
        task_id: ID,
        generation: first.generation,
        live: true,
    };
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            super::super::frame_access::with_reserved_image(SLOT, &foreign_slot, |_| ())
        }))
        .is_err()
    );
    drop(foreign_slot);
    let foreign = Ticket {
        slot: SLOT,
        task_id: ID + 1,
        generation: first.generation,
        live: true,
    };
    assert!(catch_unwind(AssertUnwindSafe(|| foreign.initialize_storage(|_| ()))).is_err());
    drop(foreign);
    first.initialize_storage(|bytes| bytes[64] = 0x71);
    first.with_image(|_| {
        assert!(catch_unwind(AssertUnwindSafe(|| first.with_image(|_| ()))).is_err());
    });
    let generation = first.generation;
    cancel(first);
    let next = reserve(SLOT, ID).unwrap();
    next.install_storage(vec![0; scheduler::TASK_STACK_SIZE]);
    let stale = Ticket {
        slot: SLOT,
        task_id: ID,
        generation,
        live: true,
    };
    assert!(catch_unwind(AssertUnwindSafe(|| stale.initialize_storage(|_| ()))).is_err());
    drop(stale);
    next.begin_commit();
    next.initialize_storage(|bytes| bytes[64] = 0x72);
    next.publish_start(scheduler::TaskStart {
        entry: crate::multitask::noop_task_entry,
        id: ID,
    });
    assert!(catch_unwind(AssertUnwindSafe(|| next.with_image(|_| ()))).is_err());
    drop(next);
    assert!(!super::super::close_for_reclaim(SLOT));
    RESOURCES[SLOT].with(|resources| {
        assert_eq!(resources.reservation.unwrap().phase, Phase::Poisoned);
        resources.reservation = None; // isolated partial-constructor recovery only
    });
    assert!(super::super::close_for_reclaim(SLOT));
    task_start::clear(SLOT);
    drop(super::super::take_storage(SLOT));
}

#[test]
fn reserved_descriptor_cannot_acquire_exec_seal_or_spend_next_exec_generation_before_ack() {
    use crate::multitask::{current_identity as identity, process_table};
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    identity::clear(SLOT);
    let ticket = reserve(SLOT, ID).unwrap();
    publish(&ticket);
    let stamp = process_table::test_exec_stamp(2, 3, 18);
    identity::publish_metadata(
        SLOT,
        Some(identity::TaskIdentity {
            task_id: Some(ID),
            user_mode: true,
            abi: Some(crate::user::abi::UserAbi::Linux),
            process_handle: Some(stamp.process_handle()),
            process_id: Some(ID),
            ..Default::default()
        }),
    );
    assert!(identity::exec_intent::begin(SLOT, ID, stamp, false).is_none());
    assert!(!identity::exec_intent::busy(SLOT));
    assert!(!identity::lifecycle(SLOT).exec_quiesced);
    runqueue::admit_blocked(SLOT);
    ticket.finish_commit();
    let intent = identity::exec_intent::begin(SLOT, ID, stamp, false).unwrap();
    intent.prepare_cancel();
    intent.cancel(process_table::test_exec_cancellation(stamp));
    cleanup_published_fixture();
}
