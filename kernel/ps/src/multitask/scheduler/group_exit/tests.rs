//! Owner: independent group worker and original suspension witnesses.
//! Boundary: actual born backing/census/process pin/terminal rows; modeled CPU/IRQ and consumer ACKs.
//! Lifecycle: durable request -> original effects/suspension -> desired ACK -> consumer publication.
//! Concurrency: canonical root fixture; competing original producer or old binding loan is real.
//! Failure: missing scope refuses before claim; reserved terminal excludes fresh member loans.
//! Forbidden: a receipt, desired-stop ACK or host register input as physical teardown proof.
//! Evidence: actual worker, terminal publication and retained original census, not Scheduler methods.
use super::*;
const ID: u64 = 81690;
fn root(f: impl FnOnce(usize, process_table::ProcessHandle)) {
    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
    boot.linux_thread_state = Some(LinuxThreadState::default());
    root_creation::tests::exec_root_with_release(ID, true, boot, f,
        terminal_publication::tests::release_published);
}
fn submit(slot: usize, handle: process_table::ProcessHandle) -> exit_membership::Receipt {
    exit_membership::PendingExit::begin(handle, (slot, ID)).unwrap().publish(ID,
        Reason::Exited { slot, task_id: ID, process_id: ID, logical_cpu: 0 })
}
#[test]
fn actual_group_worker_transfers_original_member_loan_to_terminal_and_acks_without_catalog() {
    root(|slot, handle| {
        let receipt = submit(slot, handle);
        let scope = DispatchPreemption::enter_model_with_enabled_irqs(0);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| service(&scope))).is_err());
        drop(scope);
        assert!(!receipt.complete());
        assert!(!identity::lifecycle(slot).retired);
        let scope = DispatchPreemption::enter_model(0);
        assert!(service(&scope));
        assert!(receipt.complete());
        assert!(identity::lifecycle(slot).retired);
        assert_eq!(identity::execution::count(slot), 0);
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert_eq!(retirement_cleanup::test_consumer_publication(slot), (true, true));
        assert!(!service(&scope));
    });
}
#[test]
fn actual_competing_terminal_reservation_suspends_original_census_and_resumes_same_request() {
    root(|slot, handle| {
        let receipt = submit(slot, handle);
        let competing = terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
        let scope = DispatchPreemption::enter_model(0);
        assert!(!service(&scope));
        assert!(!receipt.complete());
        assert!(!identity::lifecycle(slot).retired);
        assert!(exit_membership::PendingExit::begin(handle, (slot, ID)).is_none());
        assert_eq!(process_table::reap_exited_processes(), 0);
        // Untouched competitor rolls back. The suspended work owns the old
        // census, nonce and process pin; it is not a replacement request.
        drop(competing);
        assert!(service(&scope));
        assert!(receipt.complete());
        assert_eq!(retirement_cleanup::test_consumer_publication(slot), (true, true));
    });
}
#[test]
fn actual_group_desired_ack_does_not_complete_terminal_consumers_before_original_borrower_drains() {
    root(|slot, handle| {
        let loan = wake_publication::Binding::capture(slot, ID).unwrap();
        let receipt = submit(slot, handle);
        let scope = DispatchPreemption::enter_model(0);
        assert!(service(&scope));
        assert!(receipt.complete());
        assert!(identity::lifecycle(slot).retired);
        assert_eq!(retirement_cleanup::test_consumer_publication(slot), (false, false));
        assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
        drop(loan);
        assert!(!service_deferred(&scope));
        assert_eq!(retirement_cleanup::test_consumer_publication(slot), (true, true));
    });
}
