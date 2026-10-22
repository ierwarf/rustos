//! Owner: actual root producer + process Join + final Ticket receipt witnesses.
//! Boundary: real process/resource/identity/queue leaves, not physical CPU/MM completion.
//! Lifecycle: constructor-visible prefix stays sealed until exact slot ACK AND Join release.
//! Concurrency: canonical scheduler fixture, no runner bounds or retry weakening.
//! Failure: early refusal returns ticket; registered abandonment retains poisoned process custody.
//! Forbidden: metadata, Blocked observation, or native poison recovery as production ACK.
//! Evidence: actual factory, Ticket publication receipt, census/terminal/thread admission.
use super::*;
use crate::multitask::process_placement::{self as placement, root_membership::RootJoin};
use crate::user::abi::UserAbi;
fn capture(
    process: crate::multitask::process_table::ProcessHandle,
    ticket: Ticket,
) -> RootPublication {
    match RootPublication::begin(process, ticket, 1) {
        Ok(p) => p,
        Err(_) => panic!("root factory refused complete producer"),
    }
}
#[test]
fn actual_root_capsule_publishes_membership_only_with_final_slot_ack_then_opens_child_creation() {
    let _fixture = super::super::super::tests::boxed_scheduler();
    let process = super::super::super::tests::test_process(ID);
    let publication = capture(process, reserve(SLOT, ID).unwrap());
    assert!(RootJoin::begin(process, (SLOT + 1, ID + 1), 1, 1).is_none());
    publish_with_process(publication.ticket(), process, false);
    assert!(placement::Join::begin(process, (SLOT, ID), UserAbi::Linux).is_none());
    assert!(!stack_resources::admission::wake_allowed(SLOT));
    publication.finish();
    assert!(!admission::is_reserved(SLOT));
    assert_eq!(placement::mask(process), Some(1));
    assert!(admission::wake_allowed(SLOT));
    let child = placement::Join::begin(process, (SLOT, ID), UserAbi::Linux).unwrap();
    drop(child);
    assert!(placement::Change::begin(process, (SLOT, ID)).is_some());
    cleanup();
}
#[test]
fn root_join_waits_through_visible_prefix_and_blocked_slot_ack_until_actual_membership_release() {
    let _fixture = super::super::super::tests::boxed_scheduler();
    let process = super::super::super::tests::test_process(ID);
    let publication = capture(process, reserve(SLOT, ID).unwrap());
    publish_with_process(publication.ticket(), process, false);
    let RootPublication {
        ticket, mut join, ..
    } = publication;
    join.register().unwrap();
    assert!(placement::terminal::Turn::begin(process, (SLOT, ID)).is_none());
    assert!(placement::Change::begin(process, (SLOT, ID)).is_none());
    let pending = placement::exit_membership::PendingExit::begin(process, (SLOT, ID)).unwrap();
    let pending = match pending.try_capture() {
        Err(p) => p,
        Ok(_) => panic!("visible root prefix released census"),
    };
    runqueue::admit_blocked(SLOT);
    assert!(placement::terminal::Turn::begin(process, (SLOT, ID)).is_none());
    let generation = ticket.generation();
    let receipt = ticket.finish_commit();
    assert!(receipt.matches((SLOT, ID), generation));
    assert!(!receipt.matches((SLOT + 1, ID), generation));
    assert!(!receipt.matches((SLOT, ID + 1), generation));
    assert!(!receipt.matches((SLOT, ID), generation + 1));
    assert!(!admission::is_reserved(SLOT));
    assert!(placement::root_membership::pending(process));
    assert!(!admission::wake_allowed(SLOT));
    assert!(
        current_identity::execution::with_claim(SLOT, |_| panic!(
            "root prefix obtained executing grant"
        ))
        .is_none()
    );
    assert!(!stack_resources::terminal_binding_matches(SLOT, Some(ID)));
    assert!(placement::Join::begin(process, (SLOT, ID), UserAbi::Linux).is_none());
    let pending = match pending.try_capture() {
        Err(p) => p,
        Ok(_) => panic!("slot ACK alone released root Join"),
    };
    join.finish(receipt);
    let census = match pending.try_capture() {
        Ok(c) => c,
        Err(_) => panic!("actual root ACK did not drain constructor"),
    };
    assert_eq!(census.members(), &[(SLOT, ID)]);
    drop(census);
    cleanup();
}
#[test]
fn exec_intent_after_slot_ack_cannot_capture_root_census_before_owned_membership_ack() {
    let _fixture = super::super::super::tests::boxed_scheduler();
    let process = super::super::super::tests::test_process(ID);
    let publication = capture(process, reserve(SLOT, ID).unwrap());
    publish_with_process(publication.ticket(), process, false);
    let RootPublication {
        ticket, mut join, ..
    } = publication;
    join.register().unwrap();
    runqueue::admit_blocked(SLOT);
    let receipt = ticket.finish_commit();
    let reservation = crate::multitask::process_table::begin_exec(process).unwrap();
    assert!(current_identity::exec_intent::begin(SLOT, ID, reservation, false).is_none());
    assert!(crate::multitask::process_table::cancel_exec(reservation));
    join.finish(receipt);
    let reservation = crate::multitask::process_table::begin_exec(process).unwrap();
    let intent = current_identity::exec_intent::begin(SLOT, ID, reservation, false).unwrap();
    let census = placement::exec_membership::ExecMembership::begin(&intent).unwrap();
    drop(census);
    let cancelled = intent.cancel_with(
        nucleus_core::util::lockdep::DispatchPreemption::enter_model(0),
        crate::multitask::process_table::cancel_exec_owned,
    );
    assert!(!cancelled.2);
    assert!(placement::Join::begin(process, (SLOT, ID), UserAbi::Linux).is_some());
    cleanup();
}

#[test]
fn root_factory_early_mask_refusal_returns_original_ticket_without_process_gate_or_resource_effect()
{
    let _fixture = super::super::super::tests::boxed_scheduler();
    let process = super::super::super::tests::test_process(ID);
    let ticket = reserve(SLOT, ID).unwrap();
    let base = stack_resources::storage_base(SLOT);
    let generation = ticket.generation();
    let ticket = match RootPublication::begin(process, ticket, 0) {
        Err(t) => t,
        Ok(_) => panic!("zero process mask admitted"),
    };
    assert!(ticket.is_reserved_current());
    assert_eq!(ticket.generation(), generation);
    assert_eq!(stack_resources::storage_base(SLOT), base);
    assert_eq!(placement::mask(process), None);
    let publication = capture(process, ticket);
    publish_with_process(publication.ticket(), process, false);
    publication.finish();
    cleanup();
}
#[test]
fn untouched_root_join_rollback_preserves_actual_ticket_and_only_owned_empty_process_gate_reopens()
{
    let _fixture = super::super::super::tests::boxed_scheduler();
    let process = super::super::super::tests::test_process(ID);
    let publication = capture(process, reserve(SLOT, ID).unwrap());
    let RootPublication { ticket, join, .. } = publication;
    let generation = ticket.generation();
    drop(join);
    assert_eq!(placement::mask(process), None);
    assert!(ticket.is_reserved_current());
    let publication = capture(process, ticket);
    assert_eq!(publication.ticket().generation(), generation);
    publish_with_process(publication.ticket(), process, false);
    publication.finish();
    cleanup();
}
#[test]
fn root_factory_refuses_committing_producer_without_spending_or_cancelling_original_binding() {
    let _fixture = super::super::super::tests::boxed_scheduler();
    let ticket = reserve(SLOT, ID).unwrap();
    publish(&ticket);
    let process = context::read(SLOT).unwrap().process_handle.unwrap();
    let base = stack_resources::storage_base(SLOT);
    let ticket = match RootPublication::begin(process, ticket, 1) {
        Err(t) => t,
        Ok(_) => panic!("root factory captured committing prefix"),
    };
    assert!(ticket.is_committing());
    assert_eq!(stack_resources::storage_base(SLOT), base);
    assert!(admit_blocked(ticket).is_ok());
    cleanup();
}
#[test]
fn registered_root_join_abandonment_keeps_process_sealed_even_after_independent_slot_ack() {
    let _fixture = super::super::super::tests::boxed_scheduler();
    let process = super::super::super::tests::test_process(ID);
    let publication = capture(process, reserve(SLOT, ID).unwrap());
    publish_with_process(publication.ticket(), process, false);
    let RootPublication {
        ticket, mut join, ..
    } = publication;
    join.register().unwrap();
    drop(join);
    runqueue::admit_blocked(SLOT);
    let _receipt = ticket.finish_commit();
    assert!(placement::Join::begin(process, (SLOT, ID), UserAbi::Linux).is_none());
    assert!(placement::Change::begin(process, (SLOT, ID)).is_none());
    assert!(placement::terminal::Turn::begin(process, (SLOT, ID)).is_none());
    assert!(RootJoin::begin(process, (SLOT + 1, ID + 1), 2, 1).is_none());
    placement::root_membership::recover_fixture(process); // native-only quarantine recovery, never ACK
    cleanup();
}

#[test]
fn exhausted_root_nonce_returns_real_ticket_without_partial_membership_or_epoch_wrap() {
    let _fixture = super::super::super::tests::boxed_scheduler();
    let process = super::super::super::tests::test_process(ID);
    let previous = placement::root_membership::set_nonce_fixture(process, u64::MAX);
    let ticket = reserve(SLOT, ID).unwrap();
    let ticket = match RootPublication::begin(process, ticket, 1) {
        Err(t) => t,
        Ok(publication) => {
            let RootPublication { ticket, join, .. } = publication;
            drop(join);
            cancel_unpublished(ticket);
            placement::root_membership::set_nonce_fixture(process, previous);
            panic!("exhausted root epoch wrapped into a live constructor");
        }
    };
    assert!(ticket.is_reserved_current());
    assert_eq!(placement::mask(process), None);
    assert!(!placement::root_membership::pending(process));
    assert_eq!(
        placement::root_membership::set_nonce_fixture(process, previous),
        u64::MAX
    );
    let publication = capture(process, ticket);
    publish_with_process(publication.ticket(), process, false);
    publication.finish();
    cleanup();
}
