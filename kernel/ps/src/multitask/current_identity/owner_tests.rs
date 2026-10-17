//! Exercises actual writer owners in the shared native resource namespace.
//! The namespace guard excludes OTHER fixtures, not the concurrent actors
//! inside each witness: their real Identity writer arbitration is unchanged.
use super::*;

fn identity(task_id: u64) -> TaskIdentity {
    TaskIdentity {
        task_id: Some(task_id),
        user_mode: false,
        abi: None,
        process_handle: None,
        process_id: None,
        console_session: ConsoleSessionHandle::from_raw(0),
        pager_charge: None,
    }
}

#[test]
fn metadata_republication_cannot_erase_newer_lifecycle_intent() {
    let _namespace = crate::multitask::scheduler::execution_test_serial_guard();
    let slot = MAX_SCHEDULER_TASKS - 11;
    clear(slot);
    let original = identity(80_001);
    publish_metadata(slot, Some(original));
    assert!(update_live_lifecycle(
        slot,
        80_001,
        LifecycleGate::Suspended,
        true
    ));
    let mut metadata = original;
    metadata.process_id = Some(81_001);
    publish_metadata(slot, Some(metadata));
    let (published, flags) = read_state(slot).unwrap();
    assert_eq!(published, metadata);
    assert_eq!(flags, DISPATCH_SUSPENDED);
    assert!(!read_dispatch(slot).unwrap().lifecycle_dispatchable());
    clear(slot);
}

#[test]
fn absent_partial_metadata_preserves_pending_start_and_stop_intent() {
    let _namespace = crate::multitask::scheduler::execution_test_serial_guard();
    let slot = MAX_SCHEDULER_TASKS - 12;
    clear(slot);
    publish_lifecycle_change(slot, None, LifecycleGate::Suspended, true);
    publish_lifecycle_change(slot, None, LifecycleGate::JobStopped, true);
    publish_metadata(slot, None);
    assert!(read_dispatch(slot).is_none());
    assert!(lifecycle(slot).suspended && lifecycle(slot).job_stopped);
    publish_metadata(slot, Some(identity(80_002)));
    let (_, flags) = read_state(slot).unwrap();
    assert_eq!(flags, DISPATCH_SUSPENDED | DISPATCH_JOB_STOPPED);
    clear(slot);
    assert_eq!(lifecycle(slot), DispatchLifecycle::default());
}

#[test]
fn stale_live_command_cannot_stop_or_resume_a_recycled_identity() {
    let _namespace = crate::multitask::scheduler::execution_test_serial_guard();
    let slot = MAX_SCHEDULER_TASKS - 13;
    clear(slot);
    publish_metadata(slot, Some(identity(80_003)));
    assert!(update_live_lifecycle(
        slot,
        80_003,
        LifecycleGate::Retired,
        true
    ));
    clear(slot);
    publish_metadata(slot, Some(identity(80_004)));
    let before = read_state(slot);
    for gate in [
        LifecycleGate::Retired,
        LifecycleGate::Suspended,
        LifecycleGate::JobStopped,
        LifecycleGate::ExecQuiesced,
    ] {
        assert!(!update_live_lifecycle(slot, 80_003, gate, true));
        assert!(!update_live_lifecycle(slot, 80_003, gate, false));
        assert_eq!(read_state(slot), before);
    }
    assert!(!update_live_lifecycle(
        MAX_SCHEDULER_TASKS,
        80_004,
        LifecycleGate::Retired,
        true
    ));
    assert!(update_live_lifecycle(
        slot,
        80_004,
        LifecycleGate::ExecQuiesced,
        true
    ));
    assert!(read_dispatch(slot).unwrap().exec_quiesced());
    clear(slot);
}

#[test]
fn concurrent_metadata_and_gate_writers_do_not_lose_each_others_fields() {
    let _namespace = crate::multitask::scheduler::execution_test_serial_guard();
    let slot = MAX_SCHEDULER_TASKS - 14;
    clear(slot);
    publish_metadata(slot, Some(identity(80_005)));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for value in 1..=128 {
                let mut metadata = identity(80_005);
                metadata.process_id = Some(value);
                publish_metadata(slot, Some(metadata));
            }
        });
        for gate in [
            LifecycleGate::Retired,
            LifecycleGate::Suspended,
            LifecycleGate::JobStopped,
            LifecycleGate::ExecQuiesced,
        ] {
            scope.spawn(move || {
                for _ in 0..128 {
                    assert!(update_live_lifecycle(slot, 80_005, gate, false));
                    assert!(update_live_lifecycle(slot, 80_005, gate, true));
                }
            });
        }
    });
    let (published, flags) = read_state(slot).unwrap();
    assert_eq!(published.task_id, Some(80_005));
    assert_eq!(published.process_id, Some(128));
    assert_eq!(flags, DISPATCH_INELIGIBLE);
    assert!(!read_dispatch(slot).unwrap().lifecycle_dispatchable());
    clear(slot);
}
