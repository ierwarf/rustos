//! Owner: original Current wait and physical wake witnesses replacing catalog producers.
//! Boundary: actual born allocation/MM and Current; earlier IRQ/queue entry is modeled.
//! Lifecycle: retain the original loan through race, commit, cancel and terminal refusal.
//! Concurrency: no raw owner leaf crosses wake or callback effects.
//! Failure: independent retired/suspended gates refuse without changing wait/queue state.
//! Forbidden: copied Current or descriptor generations as physical lifetime or hardware ACK.
//! Evidence: historical public IDs exercise the same production wait/wake owners.
use super::*;

pub(in crate::multitask::scheduler) fn race_and_typed_cancel() {
    root(true, |slot| {
        let saved = runqueue::saved_rsp(slot);
        // A Current frame has already been consumed; no wake may inspect it.
        runqueue::set_saved_rsp(slot, 1);
        let wait = capture().unwrap();
        assert!(wait.arm(BlockReason::Generic));
        assert!(super::super::super::wake_publication::wake_task(ID));
        assert!(!wait.commit());
        assert!(!runqueue::wait::blocked(slot));
        assert!(!runqueue::wait::armed(slot));
        assert_eq!(runqueue::saved_rsp(slot), 1);
        assert!(wait.arm(BlockReason::Generic));
        assert!(wait.commit());
        assert!(!wait.arm(BlockReason::Generic));
        assert!(!wait.cancel());
        // Same original loan also qualifies the atomic owner refusal, not
        // merely the redundant outer blocked preflight.
        let blocked = (runqueue::owner(slot), runqueue::wait::reason(slot));
        assert!(!runqueue::wait::try_publish_arm(slot, runqueue::wait::REASON_GENERIC, 0));
        assert_eq!((runqueue::owner(slot), runqueue::wait::reason(slot)), blocked);
        assert!(super::super::super::wake_publication::wake_task(ID));
        assert!(!runqueue::wait::blocked(slot));
        for (reason, exact) in [
            (BlockReason::EndpointReceive(0xfeed), (runqueue::wait::REASON_ENDPOINT_RECEIVE, 0xfeed)),
            (BlockReason::EndpointReply(0xcafe), (runqueue::wait::REASON_ENDPOINT_REPLY, 0xcafe)),
        ] {
            assert!(wait.arm(reason));
            assert_eq!(runqueue::wait::reason(slot), exact);
            assert!(wait.cancel());
            assert_eq!(runqueue::wait::reason(slot).0, runqueue::wait::REASON_NONE);
            assert!(!runqueue::wait::armed(slot));
        }
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        runqueue::set_saved_rsp(slot, saved);
    });
}

pub(in crate::multitask::scheduler) fn independent_terminal_gates() {
    root(true, |slot| {
        for gate in [identity::LifecycleGate::Retired, identity::LifecycleGate::Suspended] {
            identity::update_live_lifecycle(slot, ID, gate, true);
            let before = (runqueue::owner(slot), runqueue::wait::reason(slot),
                runqueue::wait::blocked(slot), runqueue::wait::armed(slot),
                runqueue::wait::ready_since_ticks(slot), identity::execution::count(slot));
            assert!(capture().is_none());
            assert!(!super::super::super::wake_publication::wake_task(ID));
            assert_eq!((runqueue::owner(slot), runqueue::wait::reason(slot),
                runqueue::wait::blocked(slot), runqueue::wait::armed(slot),
                runqueue::wait::ready_since_ticks(slot), identity::execution::count(slot)), before);
            assert!(stack_resources::cpu_leases::owns_current(0, slot));
            identity::update_live_lifecycle(slot, ID, gate, false);
        }
    });
    actual_current_wait_terminal_after_capture_refuses_arm_and_acknowledges_cancelled_pager_without_wake();
    actual_current_wait_scalar_running_without_original_physical_bank_refuses_before_arm();
}
