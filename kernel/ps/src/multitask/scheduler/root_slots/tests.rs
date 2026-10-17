//! Owner: real user-root producer/storage/final-ACK witnesses.
//! Boundary: metadata fixtures are not process activation or CPU/MM grants.
//! Lifecycle: exact ticket survives prefix; cancellation drains before reuse.
//! Concurrency: native shared fixture and competing actual root reservations.
//! Failure: wrong phase preserves custody; abandonment is not rollback.
//! Forbidden: native cleanup is never a production settlement/return ACK.
//! Evidence: bounded native source-owner witnesses, not complete spawn proof.
use super::super::stack_resources::{context, task_start};
use super::super::{context_validation::PublishedFrameView, current_identity, stack_resources};
use super::*;
const SLOT: usize = 109;
const ID: u64 = 109001;
fn publish(ticket: &Ticket) {
    let process = super::super::tests::test_process(ticket.task_id());
    publish_with_process(ticket, process, true);
}
fn publish_with_process(
    ticket: &Ticket,
    process: crate::multitask::process_table::ProcessHandle,
    catalog: bool,
) {
    let slot = ticket.slot();
    ticket.begin_commit();
    let mut metadata = super::super::tests::test_user_context(process);
    metadata.scheduling_context =
        super::super::scheduling_context::SchedulingContext::bind(slot, ticket.task_id());
    ticket.install_context(metadata);
    ticket.publish_start(super::super::TaskStart {
        entry: crate::multitask::noop_task_entry,
        id: ticket.task_id(),
    });
    let base = stack_resources::storage_base(slot);
    stack_resources::publish_binding(
        slot,
        ticket.task_id(),
        PublishedFrameView {
            user_mode_task: true,
            kernel_process_task: false,
            kernel_stack_base: (base + TASK_STACK_GUARD_BYTES) as u64,
            kernel_stack_top: super::super::align_kernel_stack_top(base + TASK_STACK_SIZE) as u64,
            alternate_stack_base: 0,
            alternate_stack_top: 0,
            stack_canary_intact: true,
            scheduler_storage_base: 0,
            scheduler_storage_end: 0,
        },
    );
    runqueue::affinity_payload::initialize_affinity(slot, 1, 1);
    if catalog {
        crate::multitask::process_placement::register_root(process, slot, ticket.task_id(), 1)
            .unwrap();
    }
    current_identity::publish_metadata(
        slot,
        Some(current_identity::TaskIdentity {
            task_id: Some(ticket.task_id()),
            user_mode: true,
            abi: Some(crate::user::abi::UserAbi::Linux),
            process_handle: Some(process),
            process_id: Some(ticket.task_id()),
            ..Default::default()
        }),
    );
}
fn cleanup() {
    assert!(stack_resources::close_for_reclaim(SLOT));
    task_start::clear(SLOT);
    context::clear(SLOT);
    drop(stack_resources::take_storage(SLOT));
    current_identity::clear(SLOT);
}
#[test]
fn root_reservation_owns_initialized_canary_before_bytes_and_second_creator_cannot_spend_it() {
    let _fixture = super::super::tests::boxed_scheduler();
    let ticket = reserve(SLOT, 0).unwrap();
    assert_eq!(ticket.slot(), SLOT);
    assert_eq!(ticket.task_id(), 0);
    assert!(stack_resources::allocated(SLOT));
    assert_eq!(stack_resources::owned_canary_intact(SLOT), Some(true));
    let base = stack_resources::storage_base(SLOT);
    assert_ne!(base, 0);
    assert!(reserve(SLOT, ID).is_none());
    assert_eq!(stack_resources::storage_base(SLOT), base);
    assert!(ticket.is_current(SLOT, 0));
    assert!(context::read(SLOT).is_none());
    cancel_unpublished(ticket);
    assert!(!admission::is_reserved(SLOT));
    assert!(!stack_resources::allocated(SLOT));
    let next = reserve(SLOT, ID).unwrap();
    assert!(!next.is_current(SLOT, 0));
    cancel_unpublished(next);
}
#[test]
fn root_descriptor_prefix_stays_closed_until_exact_blocked_ack_without_directory_authority() {
    let _fixture = super::super::tests::boxed_scheduler();
    let ticket = reserve(SLOT, ID).unwrap();
    publish(&ticket);
    assert!(context::read(SLOT).is_some());
    assert!(task_start::read(SLOT).is_some());
    assert!(stack_resources::with_cold_binding(SLOT, ID, || ()).is_none());
    assert!(admission::is_reserved(SLOT));
    assert!(ticket.is_committing());
    assert!(current_identity::cohort::Cohort::begin(&[(SLOT, ID)]).is_none());
    assert!(admit_blocked(ticket).is_ok());
    assert!(!admission::is_reserved(SLOT));
    assert_eq!(
        runqueue::owner(SLOT).state,
        runqueue::RunOwnerState::Blocked
    );
    assert_eq!(
        stack_resources::with_cold_binding(SLOT, ID, || ()),
        Some(())
    );
    assert!(stack_resources::with_cold_binding(SLOT, ID + 1, || ()).is_none());
    cleanup();
}
#[test]
fn root_publication_refuses_reserved_phase_before_queue_effect_and_returns_original_producer() {
    let _fixture = super::super::tests::boxed_scheduler();
    let ticket = reserve(SLOT, ID).unwrap();
    let before = runqueue::owner(SLOT);
    let ticket = match admit_blocked(ticket) {
        Err(ticket) => ticket,
        Ok(()) => panic!("uncommitted root ACK admitted"),
    };
    assert_eq!(runqueue::owner(SLOT), before);
    assert!(ticket.is_current(SLOT, ID));
    assert!(reserve(SLOT, ID + 1).is_none());
    cancel_unpublished(ticket);
    assert!(!admission::is_reserved(SLOT));
}
#[test]
fn root_cancellation_cannot_cross_commit_or_destroy_published_binding_before_ack() {
    let _fixture = super::super::tests::boxed_scheduler();
    let ticket = reserve(SLOT, ID).unwrap();
    publish(&ticket);
    let base = stack_resources::storage_base(SLOT);
    assert!(!ticket.begin_cancel());
    assert!(ticket.is_committing());
    assert_eq!(stack_resources::storage_base(SLOT), base);
    assert!(admission::is_reserved(SLOT));
    assert!(reserve(SLOT, ID + 1).is_none());
    assert!(admit_blocked(ticket).is_ok());
    cleanup();
}
#[test]
fn competing_root_reservations_have_one_real_stack_producer_without_global_scheduler_guard() {
    let _fixture = super::super::tests::boxed_scheduler();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut workers = std::vec::Vec::new();
    for id in [ID, ID + 1] {
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            reserve(SLOT, id)
        }));
    }
    let mut grants: std::vec::Vec<_> = workers
        .into_iter()
        .filter_map(|w| w.join().unwrap())
        .collect();
    assert_eq!(grants.len(), 1);
    assert_eq!(stack_resources::owned_canary_intact(SLOT), Some(true));
    cancel_unpublished(grants.pop().unwrap());
}

#[test]
fn terminal_binding_admission_refuses_complete_constructor_prefix_until_exact_blocked_ack() {
    let _fixture = super::super::tests::boxed_scheduler();
    let ticket = reserve(SLOT, ID).unwrap();
    publish(&ticket);
    assert!(!stack_resources::terminal_binding_matches(SLOT, Some(ID)));
    assert!(!stack_resources::terminal_binding_matches(SLOT, None));
    assert!(!stack_resources::terminal_binding_matches(
        SLOT,
        Some(ID + 1)
    ));
    assert!(admit_blocked(ticket).is_ok());
    assert!(stack_resources::terminal_binding_matches(SLOT, Some(ID)));
    cleanup();
}

mod membership;
