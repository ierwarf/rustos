//! Owner: actual original Current wait/backing loan witnesses.
//! Boundary: actual root and physical bank; earlier queue claim and IRQ entry are modeled.
//! Lifecycle: capture -> exact arm/commit/rollback -> original loan release before bank drop.
//! Concurrency: actual wait/wake/terminal owner operations, not catalog fallback inputs.
//! Failure: copied scalar Current without physical bank refuses; terminal never resurrects.
//! Forbidden: these native fixtures as hardware/MM activation or joint guard-deletion proof.
//! Evidence: exact UID ref count, physical Current and real atomic wait payload operations.
use super::*;
pub(in crate::multitask::scheduler) mod legacy_witnesses;
use crate::user::abi::UserAbi;
const ID: u64 = 82600;
fn root(current: bool, f: impl FnOnce(usize)) {
    super::super::root_creation::tests::exec_root(
        ID,
        current,
        super::super::root_creation::tests::bootstrap(UserAbi::Linux),
        |slot, _| {
            runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
            f(slot);
        },
    );
}
fn capture() -> Option<CurrentWait> {
    // SAFETY: fixture retains original modeled masked CPU scope and physical
    // bank through the callback. No loan escapes the callback or bank drop.
    unsafe { CurrentWait::capture() }
}
#[test]
fn actual_current_wait_arm_cancel_retains_original_bank_uid_and_exact_reason() {
    root(true, |slot| {
        let refs = identity::execution::count(slot);
        let wait = capture().unwrap();
        assert_eq!(identity::execution::count(slot), refs + 1);
        let stop = super::super::terminal_quarantine::begin(
            slot,
            Some(ID),
            super::super::TaskRetireReason::Exited,
            false,
        )
        .unwrap();
        assert!(capture().is_none());
        assert_eq!(identity::execution::count(slot), refs + 1);
        drop(stop); // original untouched reservation, not a stopped producer
        assert!(wait.arm(BlockReason::EndpointReply(0x4242)));
        assert_eq!(
            runqueue::wait::reason(slot),
            (runqueue::wait::REASON_ENDPOINT_REPLY, 0x4242)
        );
        assert!(wait.cancel());
        assert!(!runqueue::wait::armed(slot));
        assert_eq!(runqueue::wait::reason(slot).0, runqueue::wait::REASON_NONE);
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        drop(wait);
        assert_eq!(identity::execution::count(slot), refs);
    });
}
#[test]
fn actual_current_wait_pager_commit_rollback_uses_same_original_loan_and_exact_token() {
    root(true, |slot| {
        // SAFETY: fixture's original masked CPU/bank scope contains arm loan.
        assert_eq!(
            unsafe { arm_current_wait(BlockReason::PagerFault(0xface)) },
            Some(true)
        );
        // IRQ fused publication captures the already-published exact token,
        // then retains THIS original loan through commit and rollback.
        let wait = capture().unwrap();
        assert!(wait.commit());
        assert!(!runqueue::owner(slot).runnable);
        assert!(!wait.restore_pager(0xbeef));
        assert!(!runqueue::owner(slot).runnable);
        assert!(wait.restore_pager(0xface));
        assert!(runqueue::owner(slot).runnable);
        assert_eq!(runqueue::wait::reason(slot).0, runqueue::wait::REASON_NONE);
    });
}
#[test]
fn actual_current_wait_raced_wake_withdraws_arm_and_refuses_commit_without_catalog() {
    root(true, |slot| {
        let wait = capture().unwrap();
        assert!(wait.arm(BlockReason::EndpointReceive(17)));
        runqueue::wake_wait(slot);
        assert!(!wait.commit());
        assert!(runqueue::owner(slot).runnable);
        assert!(!runqueue::wait::armed(slot));
    });
}
#[test]
fn actual_current_wait_terminal_after_capture_refuses_arm_and_acknowledges_cancelled_pager_without_wake()
 {
    root(true, |slot| {
        let wait = capture().unwrap();
        assert!(wait.arm(BlockReason::PagerFault(0xface)));
        assert!(wait.commit());
        // Actual queue terminal producer; full process detach/consumer ACK is
        // not modeled by this component witness or granted by its scalar stop.
        runqueue::retire(slot, runqueue::weight::value(slot));
        assert!(!wait.arm(BlockReason::Generic));
        assert!(!wait.commit());
        assert!(!wait.restore_pager(0xbeef));
        assert!(wait.restore_pager(0xface));
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Retired
        );
        assert!(!runqueue::owner(slot).runnable);
        assert!(!runqueue::wait::armed(slot));
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
    });
}
#[test]
fn actual_current_wait_scalar_running_without_original_physical_bank_refuses_before_arm() {
    root(false, |slot| {
        let current = cpu_local::install_test_current_owner(0, slot);
        assert!(!stack_resources::cpu_leases::owns_current(0, slot));
        assert!(capture().is_none());
        // SAFETY: the masked fixture scope contains this rejected capture; no
        // returned loan may escape its original CPU/physical-bank scope.
        assert!(unsafe { identity::execution::pin_current(0, slot, ID) }.is_none());
        assert!(!runqueue::wait::armed(slot));
        drop(current);
    });
}
#[test]
fn actual_current_wait_physical_bank_without_running_queue_custody_refuses_before_arm() {
    super::super::root_creation::tests::exec_root(
        ID,
        true,
        super::super::root_creation::tests::bootstrap(UserAbi::Linux),
        |slot, _| {
            assert!(stack_resources::cpu_leases::owns_current(0, slot));
            assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
            assert!(capture().is_none());
            assert!(!runqueue::wait::armed(slot));
        },
    );
}

#[test]
fn actual_current_wait_already_committed_wait_refuses_another_arm_or_cancel() {
    root(true, |slot| {
        let wait = capture().unwrap();
        assert!(wait.arm(BlockReason::PagerService));
        assert!(wait.commit());
        assert!(!wait.arm(BlockReason::Generic));
        assert!(!wait.cancel());
        assert_eq!(
            runqueue::wait::reason(slot).0,
            runqueue::wait::REASON_PAGER_SERVICE
        );
    });
}
