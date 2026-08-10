use super::{
    NETD_NANOS_PER_MILLI, encode_transfer_tickets, ioctl_is_display_policy_request,
    is_console_handle, netd_deadline_after_ms, netd_deadline_remaining_ms_at, read_transfer_ticket,
};
use crate::multitask;
use rustos_user_abi::syscall::NetdIpcRequest;

#[test]
fn netd_producer_stamps_one_monotonic_end_for_request_and_retries() {
    let start_ns = 7 * NETD_NANOS_PER_MILLI + 91;
    let deadline_ns = netd_deadline_after_ms(start_ns, 100);
    let request = NetdIpcRequest {
        deadline_ns,
        ..NetdIpcRequest::default()
    };

    assert_eq!(request.deadline_ns, deadline_ns);
    assert_eq!(
        netd_deadline_remaining_ms_at(request.deadline_ns, start_ns),
        Some(100)
    );
    assert_eq!(
        netd_deadline_remaining_ms_at(request.deadline_ns, start_ns + 63 * NETD_NANOS_PER_MILLI),
        Some(37)
    );
    assert_eq!(
        netd_deadline_remaining_ms_at(request.deadline_ns, deadline_ns),
        None
    );
}

#[test]
fn tty_policy_route_requires_an_actual_console_open_description() {
    let console = multitask::KernelHandle::Console(multitask::ConsoleHandle::new(
        multitask::ConsoleStreamKind::Input,
    ));
    let epoll = multitask::KernelHandle::Epoll(multitask::EpollHandle::new());
    assert!(is_console_handle(&console));
    assert!(!is_console_handle(&epoll));
}

#[test]
fn ui_policy_direct_set_is_limited_to_display_contracts() {
    assert!(ioctl_is_display_policy_request(
        rustos_user_abi::device::DISPLAY_IOCTL_GET_INFO
    ));
    assert!(ioctl_is_display_policy_request(
        rustos_user_abi::device::DISPLAY_IOCTL_CREATE_SURFACE
    ));
    assert!(ioctl_is_display_policy_request(
        rustos_user_abi::device::DISPLAY_IOCTL_GPU_GET_INFO
    ));
    assert!(ioctl_is_display_policy_request(
        rustos_user_abi::device::DISPLAY_IOCTL_GPU_SUBMIT
    ));
    assert!(ioctl_is_display_policy_request(
        rustos_user_abi::device::DISPLAY_IOCTL_GPU_QUERY_COMPLETION
    ));
    assert!(!ioctl_is_display_policy_request(
        rustos_user_abi::console::CONSOLE_IOCTL_GET_STATE
    ));
}

#[test]
fn transfer_ticket_wire_is_integer_only_exact_and_nonzero() {
    let ticket = kernel_ipc_runtime::api::KernelTransferTicket::new(7, 11, 13)
        .expect("valid transfer ticket");
    let bytes = encode_transfer_tickets(&[ticket]).expect("encode ticket");
    assert_eq!(read_transfer_ticket(&bytes), Ok(ticket));

    let mut zero_id = bytes.clone();
    zero_id[..8].fill(0);
    assert!(read_transfer_ticket(&zero_id).is_err());
    let mut zero_nonce = bytes.clone();
    zero_nonce[8..].fill(0);
    assert!(read_transfer_ticket(&zero_nonce).is_err());
    assert!(read_transfer_ticket(&bytes[..15]).is_err());
}

#[test]
fn an_oversized_stream_sendmsg_takes_a_prefix_rather_than_rejecting_the_call() {
    use super::{LINUX_EMSGSIZE, sendmsg_data_capacity};
    use rustos_user_abi::syscall::{NETD_IPC_PAYLOAD_CAPACITY, NETD_SENDMSG_PAYLOAD_HEADER_SIZE};

    let room = NETD_IPC_PAYLOAD_CAPACITY - NETD_SENDMSG_PAYLOAD_HEADER_SIZE;
    assert_eq!(sendmsg_data_capacity(0), Ok(room));

    // A control block takes its space out of the same payload, and what is left
    // is still a legal short write rather than a rejected call.
    assert_eq!(sendmsg_data_capacity(64), Ok(room - 64));
    assert_eq!(sendmsg_data_capacity(room - 1), Ok(1));

    // No room left is not a prefix anyone can take, and a control block larger
    // than the whole payload never had one. Both are EMSGSIZE, never EINVAL:
    // a caller that asked for more than this transport carries has not made a
    // programming error, and a stream writer retries a short write but not an
    // argument fault.
    assert_eq!(sendmsg_data_capacity(room), Err(LINUX_EMSGSIZE));
    assert_eq!(
        sendmsg_data_capacity(NETD_IPC_PAYLOAD_CAPACITY * 2),
        Err(LINUX_EMSGSIZE)
    );
}
