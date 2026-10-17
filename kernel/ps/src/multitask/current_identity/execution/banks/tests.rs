use super::*;
fn bind(slot: usize, id: u64) {
    clear(slot);
    publish_metadata(
        slot,
        Some(TaskIdentity {
            task_id: Some(id),
            user_mode: false,
            abi: None,
            process_handle: None,
            process_id: None,
            console_session: ConsoleSessionHandle::from_raw(0),
            pager_charge: None,
        }),
    );
}
const A: usize = 122;
const B: usize = 123;
#[test]
fn incoming_and_outgoing_identity_cannot_reset_until_exact_rsp_release() {
    let _serial = scheduler::execution_test_serial_guard();
    bind(A, 122001);
    bind(B, 123001);
    let bank = Banks::new();
    bank.initialize(initial(A, 122001).unwrap());
    Banks::put(&bank.prepared, initial(B, 123001).unwrap());
    assert!(update_live_lifecycle(
        A,
        122001,
        LifecycleGate::Retired,
        true
    ));
    bank.publish(A, B);
    assert_eq!(count(A), 1);
    assert_eq!(count(B), 1);
    assert_eq!(bank.outgoing.load(Ordering::Acquire), A + 1);
    assert_eq!(bank.current.load(Ordering::Acquire), B + 1);
    assert!(std::panic::catch_unwind(|| clear(A)).is_err());
    bank.commit();
    assert_eq!(count(A), 0);
    clear(A);
    bank.commit();
    assert_eq!(count(B), 1); // duplicate cannot spend incoming Current
    drop(Banks::take(&bank.current));
    clear(B);
}
#[test]
fn same_task_refresh_and_replaced_selection_never_release_current_identity() {
    let _serial = scheduler::execution_test_serial_guard();
    bind(A, 122001);
    bind(B, 123001);
    let bank = Banks::new();
    bank.initialize(initial(A, 122001).unwrap());
    assert!(Banks::put(&bank.prepared, initial(B, 123001).unwrap()).is_none());
    drop(Banks::put(&bank.prepared, initial(A, 122001).unwrap()));
    assert_eq!(count(B), 0);
    assert_eq!(count(A), 2);
    bank.publish(A, A);
    assert_eq!(count(A), 1);
    assert_eq!(bank.outgoing.load(Ordering::Acquire), 0);
    bank.commit();
    assert_eq!(count(A), 1);
    drop(Banks::take(&bank.current));
    clear(A);
    clear(B);
}
#[test]
fn desired_stop_denies_new_admission_but_cannot_spend_captured_current_or_outgoing() {
    let _serial = scheduler::execution_test_serial_guard();
    bind(A, 122001);
    bind(B, 123001);
    let bank = Banks::new();
    bank.initialize(initial(A, 122001).unwrap());
    Banks::put(&bank.prepared, initial(B, 123001).unwrap());
    update_live_lifecycle(B, 123001, LifecycleGate::JobStopped, true);
    assert!(initial(B, 123001).is_none());
    bank.publish(A, B); // admitted before stop; cold completion must wait for custody
    assert_eq!(count(B), 1);
    bank.commit();
    assert_eq!(count(A), 0);
    drop(Banks::take(&bank.current));
    clear(A);
    clear(B);
}
