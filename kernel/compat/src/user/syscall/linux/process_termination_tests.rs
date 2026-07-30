use super::{linux_fault_wait_status, should_record_process_exit};

#[test]
fn single_thread_exit_is_never_invented_from_missing_process_state() {
    assert!(should_record_process_exit(true, None));
    assert!(should_record_process_exit(false, Some(1)));
    assert!(!should_record_process_exit(false, Some(2)));
    assert!(!should_record_process_exit(false, None));
}

#[test]
fn x86_user_faults_have_linux_wait_signal_status() {
    assert_eq!(linux_fault_wait_status(0), 8);
    assert_eq!(linux_fault_wait_status(3), 5);
    assert_eq!(linux_fault_wait_status(6), 4);
    assert_eq!(linux_fault_wait_status(7), 11);
    assert_eq!(linux_fault_wait_status(11), 7);
    assert_eq!(linux_fault_wait_status(14), 11);
}
