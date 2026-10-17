//! Owner: actual terminal stop producer witnesses, not native Scheduler arrays.
//! Boundary: real root storage, lifecycle, queue, deferred nonce and resource bank.
//! Lifecycle: root -> independent withdrawal -> original deferred claim -> fixture ACK.
//! Concurrency: retained physical Current bank excludes reuse even after stop publication.
//! Failure: foreign UID and competing producers cannot replace original authority/reason.
//! Forbidden: fixture ACK as process-last-live, funding return or hardware completion proof.
//! Evidence: all-build stop engine with real terminal rows; external completion is injected.
use super::super::{UserAbi, reclaim::RetirementSideEffect, retirement_cleanup, root_creation};
use super::*;
const ID: u64 = 81400;

fn root(f: impl FnOnce(usize)) {
    root_creation::tests::exec_root(
        ID,
        true,
        root_creation::tests::bootstrap(UserAbi::Linux),
        |slot, _| f(slot),
    );
}

fn fixture_ack(slot: usize) {
    let mut original = claim(slot, Some(ID)).expect("original deferred stop lost");
    original.begin_effects();
    original.publish(None, RetirementSideEffect::new(Some(ID), None));
    // Explicit modeled external completion before fixture namespace cleanup.
    // This does not detach the process, drain the hardware bank or authorize reuse.
    retirement_cleanup::ack_kernel_fixture(slot);
    retirement_cleanup::reset(slot);
}

#[test]
fn actual_stop_withdraws_queue_and_wait_before_deferred_reason_without_detaching_resources() {
    root(|slot| {
        let context = stack_resources::context::read(slot).unwrap();
        let rsp = runqueue::saved_rsp(slot);
        let root = runqueue::address_space::root(slot);
        let funding = context.scheduling_context.identity();
        runqueue::wait::set_blocked(slot, true);
        runqueue::wait::set_armed(slot, true);
        runqueue::wait::set_ready_since_ticks(slot, 99);
        runqueue::wait::set_blocked_since_ticks(slot, 88);
        runqueue::wait::set_reason(slot, 1, 0);
        request(slot, ID, TaskRetireReason::Exited);
        assert!(identity::lifecycle(slot).retired);
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Retired
        );
        assert!(!runqueue::owner(slot).runnable);
        assert_eq!(runqueue::wait::ready_since_ticks(slot), 0);
        assert!(!runqueue::wait::blocked(slot));
        assert!(!runqueue::wait::armed(slot));
        assert_eq!(runqueue::wait::blocked_since_ticks(slot), 0);
        assert_eq!(runqueue::wait::reason(slot), (0, 0));
        assert_eq!(
            retirement_cleanup::deferred(slot),
            Some(TaskRetireReason::Exited)
        );
        assert_eq!(retirement_cleanup::test_consumer_publication(slot), (false, false));
        assert!(retirement_cleanup::kernel_pending(slot));
        let after = stack_resources::context::read(slot).unwrap();
        assert_eq!(after.process_handle, context.process_handle);
        assert_eq!(after.scheduling_context.identity(), funding);
        assert_eq!(runqueue::saved_rsp(slot), rsp);
        assert_eq!(runqueue::address_space::root(slot), root);
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
        fixture_ack(slot);
    });
}

#[test]
fn actual_stop_foreign_uid_refuses_before_lifecycle_queue_or_terminal_reservation() {
    root(|slot| {
        let before = runqueue::owner(slot);
        request(slot, ID + 1, TaskRetireReason::Exited);
        assert_eq!(runqueue::owner(slot), before);
        assert!(!identity::lifecycle(slot).retired);
        assert_eq!(retirement_cleanup::deferred(slot), None);
        assert_eq!(retirement_cleanup::reason(slot), None);
        assert!(begin(slot, Some(ID + 1), TaskRetireReason::Exited, false).is_none());
        assert_eq!(retirement_cleanup::reason(slot), None);
        assert!(!identity::lifecycle(slot).retired);
    });
}

#[test]
fn actual_stop_retained_owner_closes_all_fresh_binding_and_cohort_admission_without_claim_gap() {
    root(|slot| {
        let original = begin(slot, Some(ID), TaskRetireReason::Exited, true).unwrap();
        let retained = retain_for_finalization(original);
        assert!(identity::lifecycle(slot).retired);
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Retired);
        assert!(identity::cohort::Cohort::begin(&[(slot, ID)]).is_none());
        assert!(identity::execution::pin_wake(slot, ID).is_none());
        assert!(identity::execution::with_claim(slot, |_| panic!("stopped task claimed")).is_none());
        assert!(claim(slot, Some(ID)).is_none());
        assert!(begin(slot, Some(ID), TaskRetireReason::Exited, false).is_none());
        assert!(retained.detach());
        assert_eq!(retained.reason(), TaskRetireReason::Exited);
        assert_eq!(retirement_cleanup::test_consumer_publication(slot), (false, false));
        drop(retained); // original stop survives untouched election abandonment
        let original = claim(slot, Some(ID)).unwrap();
        assert!(original.detach());
        assert_eq!(original.reason(), TaskRetireReason::Exited);
        drop(original);
        fixture_ack(slot);
    });
}

#[test]
fn actual_stop_preserves_first_reason_and_detach_through_repeat_and_claim_abandonment() {
    root(|slot| {
        let original = begin(slot, Some(ID), TaskRetireReason::Exited, true).unwrap();
        publish(original);
        let owner = runqueue::owner(slot);
        let first = claim(slot, Some(ID)).unwrap();
        assert_eq!(first.reason(), TaskRetireReason::Exited);
        assert!(first.detach());
        assert!(claim(slot, Some(ID)).is_none());
        assert!(
            Producer::begin(
                slot,
                Some(ID),
                TaskRetireReason::Terminated {
                    requested_by_pid: None
                },
                false
            )
            .is_none()
        );
        drop(first);
        request(
            slot,
            ID,
            TaskRetireReason::Terminated {
                requested_by_pid: None,
            },
        );
        assert_eq!(runqueue::owner(slot), owner);
        let repeat = claim(slot, Some(ID)).unwrap();
        assert_eq!(repeat.reason(), TaskRetireReason::Exited);
        assert!(repeat.detach());
        drop(repeat);
        fixture_ack(slot);
    });
}
