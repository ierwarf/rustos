//! Owner: actual kernel executor resource/identity/terminal admission witnesses.
//! Boundary: real Class50/51/53 + bank CAS, not physical assembly execution.
//! Lifecycle: complete kernel descriptor admitted; stop/reuse denied until last scope releases.
//! Concurrency: canonical CPU -> runtime -> queue fixture order, no changed runner bounds.
//! Failure: user/prefix/foreign/busy/producer independently refuse; no injected resume predicate.
//! Forbidden: absence projection, backing alone as user continuation, or native reset as reopen.
//! Evidence: actual capture_binding, lifecycle, terminal reservation and Local Running claim.
use super::super::{
    TaskContext, current_identity as identity, retirement_cleanup, runqueue,
    scheduling_context, scheduling_runtime,
};
use super::*;
use crate::multitask::cpu_local;
const SLOT: usize = 123;
const ID: u64 = 123011;
const CPU: usize = 2;
fn metadata(id: u64) -> identity::TaskIdentity {
    identity::TaskIdentity {
        task_id: Some(id),
        ..Default::default()
    }
}
fn context(id: u64) -> TaskContext {
    TaskContext {
        scheduling_context: scheduling_context::SchedulingContext::bind(SLOT, id),
        saved_rsp: 0,        address_space_root: 0,        user_mode: false,
        user_abi: None,
        console_session: crate::io::session::ConsoleSessionHandle::SYSTEM,
        process_handle: None,
        process_id: None,
        user_stack: None,
        windows_thread_state: None,
    }
}
fn fixture(id: u64, f: impl FnOnce()) {
    let _cpu = cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    let banks = identity::execution::banks::isolate(CPU);
    runqueue::reset_before_publication();
    identity::clear(SLOT);
    retirement_cleanup::reset(SLOT);
    let mut view = tests::admit_context(SLOT, id, Some(context(id)));
    view.user_mode_task = false;
    publish_binding(SLOT, id, view);
    identity::publish_metadata(SLOT, Some(metadata(id)));
    runqueue::affinity_payload::initialize_affinity(SLOT, 1 << CPU, 1 << CPU);
    runqueue::admit_blocked(SLOT);
    runqueue::publish_local(SLOT, CPU, 100);
    f();
    drop(banks);
    assert!(close_for_reclaim(SLOT));
    if let Some(c) = context::read(SLOT) {
        scheduling_runtime::release(SLOT, c.scheduling_context.identity());
    }
    context::clear(SLOT);
    task_start::clear(SLOT);
    drop(take_storage(SLOT));
    identity::clear(SLOT);
    retirement_cleanup::reset(SLOT);
}
fn capture(id: u64) -> Option<identity::execution::kernel_executor::Executor> {
    retirement_cleanup::executor::capture_binding(SLOT, id)
}
#[test]
fn kernel_executor_refuses_terminal_and_every_stop_gate_but_retains_real_dispatch_admission() {
    fixture(ID, || {
        let scope = capture(ID).unwrap();
        assert_eq!(identity::execution::count(SLOT), 1);
        assert!(!terminal_binding_matches(SLOT, Some(ID)));
        for gate in [
            identity::LifecycleGate::Retired,
            identity::LifecycleGate::Suspended,
            identity::LifecycleGate::JobStopped,
            identity::LifecycleGate::ExecQuiesced,
        ] {
            assert!(!identity::update_live_lifecycle(SLOT, ID, gate, true));
        }
        assert!(runqueue::claim_dispatch_with_owner_generation(
            SLOT,
            CPU,
            100,
            Some(runqueue::owner(SLOT).generation)
        ));
        assert_eq!(identity::execution::count(SLOT), 2);
        drop(scope);
        assert_eq!(identity::execution::count(SLOT), 1);
        assert!(terminal_binding_matches(SLOT, Some(ID)));
    });
}
#[test]
fn kernel_executor_is_not_an_ordinary_backing_pin_and_last_scope_release_reopens_exact_stop() {
    fixture(ID, || {
        let first = capture(ID).unwrap();
        let second = capture(ID).unwrap();
        assert_eq!(identity::execution::count(SLOT), 2);
        drop(first);
        assert!(!terminal_binding_matches(SLOT, Some(ID)));
        assert!(!identity::update_live_lifecycle(
            SLOT,
            ID,
            identity::LifecycleGate::Retired,
            true
        ));
        drop(second);
        assert_eq!(identity::execution::count(SLOT), 0);
        assert!(terminal_binding_matches(SLOT, Some(ID)));
        assert!(identity::update_live_lifecycle(
            SLOT,
            ID,
            identity::LifecycleGate::Retired,
            true
        ));
        assert!(capture(ID).is_none());
        assert!(identity::update_live_lifecycle(
            SLOT,
            ID,
            identity::LifecycleGate::Retired,
            false
        ));
    });
}

#[test]
fn active_kernel_executor_refuses_metadata_withdrawal_unowned_stop_and_qualified_cohort_seal() {
    fixture(ID, || {
        let scope = capture(ID).unwrap();
        assert!(identity::cohort::Cohort::begin(&[(SLOT, ID)]).is_none());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                identity::publish_metadata(SLOT, None);
            }))
            .is_err()
        );
        let mut user = metadata(ID);
        user.user_mode = true;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                identity::publish_metadata(SLOT, Some(user));
            }))
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                identity::publish_lifecycle_change(
                    SLOT,
                    Some(metadata(ID)),
                    identity::LifecycleGate::Retired,
                    true,
                );
            }))
            .is_err()
        );
        assert_eq!(identity::execution::count(SLOT), 1);
        assert_eq!(identity::read(SLOT), Some(metadata(ID)));
        drop(scope);
        drop(identity::cohort::Cohort::begin(&[(SLOT, ID)]).unwrap());
    });
}
#[test]
fn kernel_executor_and_admitted_terminal_producer_exclude_each_other_before_effects() {
    fixture(ID, || {
        let producer = retirement_cleanup::producer::Producer::begin(
            SLOT,
            Some(ID),
            super::super::TaskRetireReason::Exited,
            false,
        )
        .unwrap();
        assert!(capture(ID).is_none());
        assert_eq!(identity::execution::count(SLOT), 0);
        drop(producer);
        let scope = capture(ID).unwrap();
        let producer = retirement_cleanup::producer::Producer::begin(
            SLOT,
            Some(ID),
            super::super::TaskRetireReason::Exited,
            false,
        )
        .unwrap();
        assert!(!terminal_binding_matches(SLOT, producer.task_id()));
        drop(producer);
        drop(scope);
        assert!(terminal_binding_matches(SLOT, Some(ID)));
    });
}
#[test]
fn kernel_executor_refuses_user_metadata_user_context_foreign_uid_and_unpublished_context_independently()
 {
    fixture(ID, || {
        assert!(capture(ID + 1).is_none());
        let mut user = metadata(ID);
        user.user_mode = true;
        identity::publish_metadata(SLOT, Some(user));
        assert!(capture(ID).is_none());
        identity::publish_metadata(SLOT, Some(metadata(ID)));
        let mut abi = metadata(ID);
        abi.abi = Some(crate::user::abi::UserAbi::Linux);
        identity::publish_metadata(SLOT, Some(abi));
        assert!(capture(ID).is_none());
        identity::publish_metadata(SLOT, Some(metadata(ID)));
        RESOURCES[SLOT].with(|r| {
            r.context.as_mut().unwrap().user_abi = Some(crate::user::abi::UserAbi::Linux)
        });
        assert!(capture(ID).is_none());
        RESOURCES[SLOT].with(|r| r.context.as_mut().unwrap().user_abi = None);
        RESOURCES[SLOT].with(|r| r.context.as_mut().unwrap().user_mode = true);
        assert!(capture(ID).is_none());
        RESOURCES[SLOT].with(|r| r.context.as_mut().unwrap().user_mode = false);
        let context = RESOURCES[SLOT].with(|r| r.context.take());
        assert!(capture(ID).is_none());
        RESOURCES[SLOT].with(|r| r.context = context);
        assert_eq!(identity::execution::count(SLOT), 0);
        drop(capture(ID).unwrap());
    });
}
#[test]
fn kernel_executor_refuses_constructor_writer_and_closing_without_spending_a_backing_reference() {
    fixture(ID, || {
        for busy in 0..3 {
            if busy == 0 {
                admission::set_phase_for_test(SLOT, ID, Some(admission::Phase::Committing));
            } else {
                RESOURCES[SLOT].with(|r| {
                    if busy == 1 {
                        r.frame_writing = true;
                    } else {
                        r.closing = true;
                    }
                });
            }
            assert!(capture(ID).is_none());
            assert_eq!(identity::execution::count(SLOT), 0);
            RESOURCES[SLOT].with(|r| {
                r.reservation = None;
                r.frame_writing = false;
                r.closing = false;
            });
        }
        drop(capture(ID).unwrap());
    });
}
#[test]
fn queued_kernel_stop_preserves_original_request_dispatch_and_nested_executor_until_last_release() {
    fixture(ID, || {
        use retirement_cleanup::producer::Producer;
        let reason = super::super::TaskRetireReason::Terminated {
            requested_by_pid: Some(67),
        };
        let executor = capture(ID).unwrap();
        Producer::begin(SLOT, Some(ID), reason, false)
            .unwrap()
            .wait_for_executor();
        assert_eq!(retirement_cleanup::deferred(SLOT), Some(reason));
        assert!(!identity::lifecycle(SLOT).retired);
        assert!(Producer::claim(SLOT, Some(ID)).is_none());
        assert!(
            Producer::begin(SLOT, Some(ID), super::super::TaskRetireReason::Exited, true).is_none()
        );
        let nested = capture(ID).expect("original executor must finish nested kernel revocation");
        drop(executor);
        assert!(Producer::claim(SLOT, Some(ID)).is_none());
        assert_eq!(identity::execution::count(SLOT), 1);
        drop(nested);
        assert!(
            capture(ID).is_none(),
            "queued stop forbids a fresh logical continuation"
        );
        assert!(Producer::claim(SLOT, Some(ID + 1)).is_none());
        let claimed = Producer::claim(SLOT, Some(ID)).unwrap();
        assert!(claimed.waited_for_executor());
        assert_eq!(claimed.reason(), reason);
        assert!(!claimed.detach());
        assert!(capture(ID).is_none());
        drop(claimed);
        assert!(
            capture(ID).is_none(),
            "untouched loan returns to durable waiting custody"
        );
        let mut claimed = Producer::claim(SLOT, Some(ID)).unwrap();
        claimed.begin_effects();
        claimed.publish(
            None,
            super::super::reclaim::RetirementSideEffect::new(Some(ID), None),
        );
        assert!(retirement_cleanup::kernel_pending(SLOT));
        // Shared actual row claim/ACK, without pretending to execute kernel effects.
        retirement_cleanup::ack_kernel_fixture(SLOT);
        retirement_cleanup::reset(SLOT);
    });
}

#[test]
fn last_executor_release_between_terminal_refusal_and_wait_publication_never_loses_exact_stop() {
    fixture(ID, || {
        use retirement_cleanup::producer::Producer;
        let executor = capture(ID).unwrap();
        let producer =
            Producer::begin(SLOT, Some(ID), super::super::TaskRetireReason::Exited, true).unwrap();
        assert!(!terminal_binding_matches(SLOT, Some(ID)));
        drop(executor);
        producer.wait_for_executor();
        assert!(capture(ID).is_none());
        let mut claimed = Producer::claim(SLOT, Some(ID))
            .expect("exact queued request survives last release gap");
        assert!(claimed.waited_for_executor());
        claimed.begin_effects();
        claimed.publish(
            None,
            super::super::reclaim::RetirementSideEffect::new(Some(ID), None),
        );
        retirement_cleanup::ack_kernel_fixture(SLOT);
        retirement_cleanup::reset(SLOT);
    });
}

#[test]
fn exact_kernel_uid_zero_is_executor_authority_not_an_empty_slot_or_user_projection() {
    fixture(0, || {
        let scope = capture(0).unwrap();
        assert!(!terminal_binding_matches(SLOT, Some(0)));
        assert!(capture(1).is_none());
        drop(scope);
        assert!(terminal_binding_matches(SLOT, Some(0)));
    });
}
