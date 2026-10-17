//! Owner: born Current/target metadata and architectural-cache regression witnesses.
//! Boundary: actual original bank/UID/funding loans through snapshots and last writes.
//! Lifecycle: close retains original units; new target admission and reuse refuse.
//! Concurrency: scalar aliases and missing banks cannot authorize Current effects.
//! Failure: wrong PID/UID/ABI/Linux cell or unaccepted TLS refuse without mutation.
//! Forbidden: native CPU/topology admission and external ACK as hardware acceptance.
//! Evidence: production engines with explicit modeled earlier CPU/ACK inputs.
use super::*;
use super::super::{current_terminal::tests as fixture, scheduling_runtime,
    terminal_quarantine, terminal_publication, TaskRetireReason};
use crate::multitask::cpu_local;
use fixture::ID;

#[test]
fn actual_current_runtime_retains_original_pair_through_close_and_last_snapshot() {
    fixture::current(|slot, scope| {
        let current = OriginalCurrent::borrow(scope).unwrap();
        let identity = current.context().scheduling_context.identity();
        let before = current.runtime_snapshot().unwrap();
        assert_eq!((before.executing_task_id, before.context_owner_task_id), (ID, ID));
        assert_eq!(before.context_identity_slot, identity.slot());
        assert_eq!(before.context_identity_generation, identity.generation());
        assert!(before.budget_ns > 0 && before.period_ns >= before.budget_ns);
        assert_eq!(before.context_consumed_ns, 0);
        assert!(!scheduling_runtime::close_for_reclaim(slot, identity));
        let after = current.runtime_snapshot().unwrap();
        assert_eq!(after.context_available_ns, before.context_available_ns);
        assert_eq!(after.domain_available_ns, before.domain_available_ns);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
            super::super::execution_interval::consume(0, slot, ID))).is_err());
        current.check();
        drop(current);
        assert!(runtime_in(scope).is_some());
    });
}

#[test]
fn actual_current_tls_requires_same_banks_linux_cell_and_accepted_value() {
    fixture::current(|slot, scope| {
        let old = runqueue::simd_tls::tls_fs_base(slot);
        assert!(!set_tls_in(scope, 0x1234));
        assert_eq!(runqueue::simd_tls::tls_fs_base(slot), old);
        linux_thread_state::test_update_exact(slot, ID, |state| state.fs_base = 0x1234);
        assert!(set_tls_in(scope, 0x1234));
        assert_eq!(runqueue::simd_tls::tls_fs_base(slot), 0x1234);
        cpu_local::with_test_current_scalar(0, slot + 1, || {
            assert!(!set_tls_in(scope, 0));
            assert!(runtime_in(scope).is_none());
        });
        assert_eq!(runqueue::simd_tls::tls_fs_base(slot), 0x1234);
        let missing = identity::execution::banks::isolate(0);
        assert!(!set_tls_in(scope, 0x1234));
        assert!(runtime_in(scope).is_none());
        drop(missing);
        assert!(!mark_root_idle_in(scope));
        assert!(!runqueue::is_idle_slot(slot));
    });
}

#[test]
fn actual_target_snapshot_retains_original_uid_and_refuses_foreign_pid_or_closed_lookup() {
    fixture::family(false, |slot, handle| {
        let scope = DispatchPreemption::enter_model(0);
        task_directory::forget(ID);
        assert!(Target::capture(ID).is_some());
        task_directory::record(ID, slot + 1); // wrong accelerator is not authority
        assert!(Target::capture(ID).is_some());
        assert!(alive_in(ID));
        let reservation = crate::multitask::process_table::begin_exec(handle).unwrap();
        let intent = identity::exec_intent::begin(slot, ID, reservation, false).unwrap();
        assert!(Target::capture(ID).is_none());
        assert!(alive_in(ID), "transient exec admission refusal is not task death");
        intent.prepare_cancel();
        intent.cancel(crate::multitask::process_table::cancel_exec_owned(reservation).unwrap());
        let target = Target::capture(ID).unwrap();
        assert_eq!(target.pin.slot(), slot);
        assert!(target.linux(ID + 99).is_none());
        assert!(Target::capture(ID + 99).is_none());
        let snapshot = target.linux(ID).unwrap();
        assert_eq!((snapshot.process_id, snapshot.thread_id), (ID, ID));
        let producer = terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
        assert!(Target::capture(ID).is_none());
        assert!(alive_in(ID), "pending close is not a committed retired publication");
        assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
        // Earlier admitted metadata observation still owns the exact old cell;
        // retirement cannot detach/rebind it while this original UID unit lives.
        assert!(target.linux(ID).is_some());
        drop(target);
        // SAFETY: modeled same masked CPU owns original Producer; no raw leaves
        // span complete terminal publication or subsequent external fixture ACK.
        unsafe { terminal_publication::guarded(producer, &|_| 1); }
        assert!(!alive_in(ID));
        drop(scope);
        None
    });
}
