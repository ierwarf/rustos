//! Owner: actual born Current/target stop regression witnesses.
//! Boundary: real descriptor/MM/funding/terminal producers; earlier CPU admission modeled.
//! Lifecycle: borrowed original banks/interval -> stop -> settlement -> bank drop -> consumer ACK.
//! Concurrency: root fixture serializes owners; no fabricated new permit after terminal close.
//! Failure: missing original unit or overlapping interval refuses before effects.
//! Forbidden: host register inputs or consumer ACK injection as hardware/RSP acceptance.
//! Evidence: same production Current/target/group admission and original producer work.
use super::*;
use super::super::{root_creation, thread_creation, scheduling_runtime, UserAbi, LinuxThreadState};
pub(in crate::multitask::scheduler) const ID: u64 = 82700;

fn root(current: bool, f: impl FnOnce(usize, process_table::ProcessHandle)) {
    family(current, |slot, handle| { f(slot, handle); None });
}
pub(in crate::multitask::scheduler) fn family(current: bool, f: impl FnOnce(usize, process_table::ProcessHandle) -> Option<usize>) {
    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
    boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x400000);
    boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x700000);
    boot.linux_thread_state = Some(LinuxThreadState::default());
    let sibling = core::cell::Cell::new(None);
    root_creation::tests::exec_root_with_release(ID, current, boot,
        |slot, handle| sibling.set(f(slot, handle)),
        |slot, id, handle| {
            // Native external consumer ACKs only after physical Current drops;
            // queue order is leader then sibling, not worker iteration order.
            terminal_publication::tests::release_published(slot, id, handle);
            if let Some(sibling) = sibling.get() {
                terminal_publication::tests::release_published(sibling, ID + 1, handle);
            }
        });
}
pub(in crate::multitask::scheduler) fn sibling(slot: usize) -> usize {
    let mut boot = crate::multitask::UserTaskBootstrap::new(UserAbi::Linux,
        x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x400000),
        x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x700000));
    boot.linux_thread_state = Some(LinuxThreadState::default());
    thread_creation::tests::active_sibling(slot, ID + 1, boot)
}
pub(in crate::multitask::scheduler) fn current(f: impl FnOnce(usize, &DispatchPreemption<'_>)) {
    root(true, |slot, _| with_current(slot, f));
}
pub(in crate::multitask::scheduler) fn with_current(slot: usize, f: impl FnOnce(usize, &DispatchPreemption<'_>)) {
        // Earlier CPU selection/admission is modeled, not new terminal authority.
        // Actual born MM allocation, UID RAII unit and FundingPin remain real.
        let banks = identity::execution::banks::isolate(0);
        identity::execution::banks::initialize(0, identity::execution::initial(slot, ID).unwrap());
        runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
        execution_interval::test_open_captured(slot, 10);
        let scope = DispatchPreemption::enter_model(0);
        f(slot, &scope);
        match execution_interval::consume(0, slot, ID) {
            execution_interval::ConsumedInterval::Active(interval) => { let _ = interval.charge(0, 1); }
            execution_interval::ConsumedInterval::AlreadySettled => {}
            execution_interval::ConsumedInterval::Absent => panic!("original interval vanished"),
        }
        if !identity::lifecycle(slot).retired {
            // Fixture teardown only; all assertions above use the actual entry.
            let producer = terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
            // SAFETY: same modeled masked CPU retains the actual physical
            // Current bank and UID permit until this fixture teardown returns.
            unsafe { terminal_publication::guarded(producer, &runqueue_policy::online_affinity_mask); }
        }
        drop(scope);
        drop(banks);
}

#[test]
fn actual_current_terminal_keeps_original_mm_uid_and_funding_until_last_effect() {
    current(|slot, scope| {
        let loan = OriginalCurrent::borrow(scope).unwrap();
        let context = loan.context();
        assert!(context.user_mode && context.process_handle.is_some());
        assert!(loan.stop(TaskRetireReason::Exited));
        loan.check();
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert!(identity::execution::banks::owns_current(0, slot));
        assert_eq!(identity::execution::count(slot), 1);
        assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
        assert!(!scheduling_runtime::close_for_reclaim(slot, context.scheduling_context.identity()));
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
            execution_interval::consume(0, slot, ID))).is_err());
        loan.check();
        drop(loan);
        let execution_interval::ConsumedInterval::Active(interval) = execution_interval::consume(0, slot, ID)
            else { panic!("original interval not retained"); };
        let charged = interval.charge(0, 1).unwrap();
        assert_eq!(charged.0, ID);
        assert!(matches!(execution_interval::consume(0, slot, ID),
            execution_interval::ConsumedInterval::AlreadySettled));
    });
}

#[test]
fn actual_current_fault_preserves_first_reason_and_terminal_current_without_fresh_admission() {
    current(|slot, scope| {
        assert!(matches!(fault_in(scope, 14, Some(7), 0xdead, 0xbeef), UserFaultDisposition::Retired));
        assert_eq!(retirement_cleanup::reason(slot), Some(TaskRetireReason::UserFault {
            vector: 14, error_code: Some(7), cr2: 0xdead, rip: 0xbeef }));
        assert!(OriginalCurrent::borrow(scope).is_some());
        assert!(matches!(fault_in(scope, 13, None, 0, 0), UserFaultDisposition::Retired));
        assert_eq!(retirement_cleanup::reason(slot), Some(TaskRetireReason::UserFault {
            vector: 14, error_code: Some(7), cr2: 0xdead, rip: 0xbeef }));
        assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
    });
}

#[test]
fn actual_fault_callback_returns_without_waiting_for_original_kernel_cleanup() {
    let mut observed = None;
    root(true, |slot, _| {
        // Model only the earlier installation; the actual public callback
        // must acquire its own unique dispatch scope, not overlap a fixture's.
        let banks = identity::execution::banks::isolate(0);
        identity::execution::banks::initialize(0, identity::execution::initial(slot, ID).unwrap());
        runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
        execution_interval::test_open_captured(slot, 10);
        let disposition = crate::multitask::retire_current_user_task_due_to_fault(
            14, Some(6), 0xdead, 0xbeef, 0xfeed,
        );
        let repeated = crate::multitask::retire_current_user_task_due_to_fault(
            13, None, 0, 0, 0,
        );
        let fence = retirement_cleanup::executor::Fence::capture();
        observed = Some((
            matches!(disposition, UserFaultDisposition::Retired),
            retirement_cleanup::kernel_pending(slot),
            !fence.complete(),
            stack_resources::cpu_leases::owns_current(0, slot),
            identity::execution::banks::owns_current(0, slot),
            retirement_cleanup::reason(slot),
            matches!(repeated, UserFaultDisposition::Retired),
            retirement_cleanup::test_cleanup_payload(slot),
        ));
        let scope = DispatchPreemption::enter_model(0);
        if !identity::lifecycle(slot).retired {
            let _ = fault_in(&scope, 14, Some(6), 0xdead, 0xbeef);
        }
        match execution_interval::consume(0, slot, ID) {
            execution_interval::ConsumedInterval::Active(interval) => { let _ = interval.charge(0, 1); }
            execution_interval::ConsumedInterval::AlreadySettled => {}
            execution_interval::ConsumedInterval::Absent => panic!("original interval vanished"),
        }
        drop(scope);
        drop(banks);
    });
    // The real external consumer ACKs only in fixture teardown, after the
    // original banks leave Current. The exception API must have returned first.
    let (retired, pending, incomplete, physical, uid, reason, repeated, cleanup) = observed.unwrap();
    assert!(retired && pending && incomplete && physical && uid && repeated);
    let cleanup = cleanup.expect("original runtime cleanup payload vanished");
    assert_eq!((cleanup.task_id(), cleanup.process_id(), cleanup.process_terminal()), (ID, ID, true));
    assert_eq!(cleanup.user_fault_vector(), Some(14));
    assert_eq!(reason, Some(TaskRetireReason::UserFault {
        vector: 14, error_code: Some(6), cr2: 0xdead, rip: 0xbeef,
    }));
}

#[test]
fn actual_current_terminal_refuses_missing_uid_alias_and_settled_interval() {
    current(|slot, scope| {
        let missing = identity::execution::banks::isolate(0);
        assert!(OriginalCurrent::borrow(scope).is_none());
        assert!(matches!(fault_in(scope, 14, None, 0, 0), UserFaultDisposition::Unhandled));
        drop(missing);
        cpu_local::with_test_current_scalar(0, slot + 1, || {
            // Change ONLY the scalar, not the parked UID permit. Otherwise
            // missing-UID refusal would conceal a broken Current qualifier.
            assert!(identity::execution::banks::owns_current(0, slot));
            assert!(OriginalCurrent::borrow(scope).is_none());
        });
        let execution_interval::ConsumedInterval::Active(interval) = execution_interval::consume(0, slot, ID)
            else { panic!("original interval missing"); };
        drop(interval);
        assert!(OriginalCurrent::borrow(scope).is_none());
        assert!(!identity::lifecycle(slot).retired);
    });
}

#[test]
fn actual_current_terminal_excludes_overlapping_effect_borrow_until_release() {
    current(|_, scope| {
        let loan = OriginalCurrent::borrow(scope).unwrap();
        assert!(OriginalCurrent::borrow(scope).is_none());
        drop(loan);
        let loan = OriginalCurrent::borrow(scope).unwrap();
        assert!(loan.stop(TaskRetireReason::Exited));
    });
}

pub(in crate::multitask::scheduler) fn last_task_witness() {
    root(false, |slot, _| {
        let scope = DispatchPreemption::enter_model(0);
        super::super::task_directory::forget(ID);
        assert!(terminate_in(&scope, ID));
        assert_eq!(process_table::is_process_exiting(ID), Some(true));
        assert!(identity::lifecycle(slot).retired);
        assert!(!terminate_in(&scope, ID));
        assert!(!terminate_in(&scope, ID + 100));
    });
}
#[test]
fn actual_target_terminal_transfers_original_binding_and_marks_last_process() { last_task_witness(); }

pub(in crate::multitask::scheduler) fn group_witness() {
    family(true, |slot, _| {
        let sibling = sibling(slot);
        let scope = DispatchPreemption::enter_model(0);
        let (receipt, fresh) =
            crate::multitask::current::group_exit::request_marked(&scope, ID, Some(7)).unwrap();
        assert!(fresh, "the first terminator publishes the termination");
        // A second terminator (for example the memory-pressure watchdog racing
        // the root supervisor) joins the queued request and must not own its
        // once-per-process follow-up.
        let (joined, second_fresh) =
            crate::multitask::current::group_exit::request_marked(&scope, ID, Some(8)).unwrap();
        assert!(!second_fresh, "a queued termination is not published twice");
        assert!(!joined.complete());
        // Original obligation: process exit admission MUST precede every worker
        // retirement; the old catalog regression now observes actual owners.
        assert_eq!(process_table::is_process_exiting(ID), Some(true));
        assert!(!identity::lifecycle(slot).retired);
        assert!(!identity::lifecycle(sibling).retired);
        assert!(!receipt.complete());
        assert!(super::super::group_exit::service(&scope));
        assert!(receipt.complete());
        assert!(joined.complete(), "both terminators observe the one exit");
        assert!(identity::lifecycle(slot).retired);
        assert!(identity::lifecycle(sibling).retired);
        Some(sibling)
    });
}
#[test]
fn actual_group_request_marks_exit_before_original_census_worker_effects() { group_witness(); }

#[test]
fn actual_target_terminal_leaves_original_sibling_alive_until_separate_group_request() {
    family(true, |slot, _| {
        let sibling = sibling(slot);
        let scope = DispatchPreemption::enter_model(0);
        assert!(terminate_in(&scope, ID + 1));
        assert!(identity::lifecycle(sibling).retired);
        assert!(!identity::lifecycle(slot).retired);
        assert_eq!(process_table::is_process_exiting(ID), Some(false));
        assert!(terminate_in(&scope, ID));
        assert_eq!(process_table::is_process_exiting(ID), Some(true));
        Some(sibling)
    });
}
