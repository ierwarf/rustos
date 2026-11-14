use super::{int_args::linux_exit_wait_status, linux_fault_wait_status, should_record_process_exit};

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

#[test]
fn general_protection_fault_is_wifsignaled_sigsegv() {
    let status = linux_fault_wait_status(13);
    assert_eq!(status, 11);
    // WIFSIGNALED: low seven bits are neither 0 (exited) nor 0x7f (stopped).
    let signaled = (status & 0x7f) != 0 && (status & 0x7f) != 0x7f;
    assert!(signaled);
    assert_eq!(status & 0x7f, 11);
    assert_eq!(status & 0x80, 0, "no core dump is produced");
    assert_ne!(status, 0, "a crash must never look like exit(0)");
    for vector in [10_u8, 13, 14, 0xff] {
        assert_eq!(linux_fault_wait_status(vector), 11);
    }
}

#[test]
fn exit_codes_are_shifted_into_the_high_byte() {
    assert_eq!(linux_exit_wait_status(0), 0);
    assert_eq!(linux_exit_wait_status(42), 0x2a00);
    assert_eq!(linux_exit_wait_status(0x1ff), 0xff00);
    assert_eq!(linux_exit_wait_status(1) & 0x7f, 0);
}
