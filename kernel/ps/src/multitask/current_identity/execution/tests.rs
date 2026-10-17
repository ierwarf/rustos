use super::*;
extern crate std;
fn identity(id: u64) -> TaskIdentity {
    TaskIdentity {
        task_id: Some(id),
        user_mode: false,
        abi: None,
        process_handle: None,
        process_id: None,
        console_session: ConsoleSessionHandle::from_raw(0),
        pager_charge: None,
    }
}
const SLOT: usize = 121;
const ID: u64 = 121001;
#[test]
fn all_lifecycle_gates_refuse_before_claim_callback_and_resume_preserves_existing_holder() {
    let _serial = scheduler::execution_test_serial_guard();
    clear(SLOT);
    publish_metadata(SLOT, Some(identity(ID)));
    let (_, pin) = with_claim(SLOT, |_| ()).unwrap();
    let pin = pin.unwrap();
    for gate in [
        LifecycleGate::Retired,
        LifecycleGate::Suspended,
        LifecycleGate::JobStopped,
        LifecycleGate::ExecQuiesced,
    ] {
        assert!(update_live_lifecycle(SLOT, ID, gate, true));
        assert!(with_claim(SLOT, |_| panic!("closed lifecycle executed claim")).is_none());
        assert_eq!(count(SLOT), 1);
        assert!(update_live_lifecycle(SLOT, ID, gate, false));
        let (_, next) = with_claim(SLOT, |_| ()).unwrap();
        assert_eq!(count(SLOT), 2);
        drop(next);
        assert_eq!(count(SLOT), 1);
    }
    drop(pin);
    assert_eq!(count(SLOT), 0);
    clear(SLOT);
}
#[test]
fn captured_execution_blocks_clear_and_absent_metadata_cannot_rebind_its_uid() {
    let _serial = scheduler::execution_test_serial_guard();
    clear(SLOT);
    publish_metadata(SLOT, Some(identity(ID)));
    let (_, pin) = with_claim(SLOT, |_| ()).unwrap();
    assert!(std::panic::catch_unwind(|| clear(SLOT)).is_err());
    publish_metadata(SLOT, None);
    assert!(std::panic::catch_unwind(|| publish_metadata(SLOT, Some(identity(ID + 1)))).is_err());
    assert_eq!(binding(SLOT), Some(ID));
    assert_eq!(count(SLOT), 1);
    publish_metadata(SLOT, Some(identity(ID)));
    drop(pin);
    clear(SLOT);
    publish_metadata(SLOT, Some(identity(ID + 1)));
    assert_eq!(binding(SLOT), Some(ID + 1));
    clear(SLOT);
}
#[test]
fn writer_and_claim_have_one_lifecycle_linearization_point_without_directory() {
    let _serial = scheduler::execution_test_serial_guard();
    for _ in 0..32 {
        clear(SLOT);
        publish_metadata(SLOT, Some(identity(ID)));
        let entered = std::sync::Barrier::new(2);
        let stopped = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let thread = scope.spawn(|| {
                entered.wait();
                update_live_lifecycle(SLOT, ID, LifecycleGate::JobStopped, true);
                stopped.store(true, Ordering::Release);
            });
            let (_, pin) = with_claim(SLOT, |_| {
                entered.wait();
                assert!(!stopped.load(Ordering::Acquire)); // writer cannot cross CAS callback
            })
            .unwrap();
            thread.join().unwrap();
            assert!(stopped.load(Ordering::Acquire));
            assert!(with_claim(SLOT, |_| ()).is_none());
            assert_eq!(count(SLOT), 1);
            drop(pin);
        });
        clear(SLOT);
    }
}
#[test]
fn saturation_refuses_callback_and_unwind_leaves_no_unparked_permit() {
    let _serial = scheduler::execution_test_serial_guard();
    clear(SLOT);
    publish_metadata(SLOT, Some(identity(ID)));
    REFS[SLOT].store(u32::MAX, Ordering::Release);
    assert!(with_claim(SLOT, |_| panic!("saturated execution count admitted")).is_none());
    assert_eq!(count(SLOT), u32::MAX);
    REFS[SLOT].store(0, Ordering::Release);
    assert!(
        std::panic::catch_unwind(|| with_claim(SLOT, |_| panic!("modeled claim unwind"))).is_err()
    );
    assert_eq!(count(SLOT), 0);
    clear(SLOT);
}
#[test]
fn root_uid_zero_is_bound_authority_not_an_empty_sentinel() {
    let _serial = scheduler::execution_test_serial_guard();
    clear(SLOT);
    publish_metadata(SLOT, Some(identity(0)));
    let (_, permit) = with_claim(SLOT, |_| ()).unwrap();
    assert!(permit.is_some());
    assert_eq!(binding(SLOT), Some(0));
    assert_eq!(count(SLOT), 1);
    drop(permit);
    clear(SLOT);
}

#[test]
fn user_identity_requires_each_abi_handle_and_pid_qualifier_before_claim() {
    let _serial = scheduler::execution_test_serial_guard();
    clear(SLOT);
    let mut complete = identity(ID);
    complete.user_mode = true;
    complete.abi = Some(UserAbi::Linux);
    complete.process_handle = Some(crate::multitask::process_table::ProcessHandle::new(1, 2));
    complete.process_id = Some(ID);
    for missing in 0..3 {
        let mut partial = complete;
        match missing {
            0 => partial.abi = None,
            1 => partial.process_handle = None,
            _ => partial.process_id = None,
        }
        publish_metadata(SLOT, Some(partial));
        assert!(with_claim(SLOT, |_| panic!("incomplete user identity admitted")).is_none());
        assert_eq!(count(SLOT), 0);
    }
    publish_metadata(SLOT, Some(complete));
    let (_, pin) = with_claim(SLOT, |_| ()).unwrap();
    drop(pin);
    clear(SLOT);
}
