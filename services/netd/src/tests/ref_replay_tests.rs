use super::*;

#[test]
fn socket_ref_increment_does_not_report_success_on_overflow() {
    let mut refs = usize::MAX;
    assert_eq!(increment_socket_refs(&mut refs), libc::EOVERFLOW);
    assert_eq!(refs, usize::MAX);
    refs = 0;
    assert_eq!(increment_socket_refs(&mut refs), libc::EOVERFLOW);
    assert_eq!(refs, 0);
    refs = 1;
    assert_eq!(increment_socket_refs(&mut refs), 0);
    assert_eq!(refs, 2);
}

#[test]
fn close_retry_replays_exact_result_and_rejects_operation_alias() {
    let request = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op: SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
        pid: 1,
        tid: 1,
        socket_token: u64::MAX - 7,
        operation_hi: 0xfeed,
        operation_lo: 0xbeef,
        ..NetdIpcRequest::default()
    };
    let mut first = NetdIpcResponse::default();
    assert_eq!(dispatch_request(&request, &mut first, 0), libc::EBADF);
    let mut retry = NetdIpcResponse::default();
    assert_eq!(dispatch_request(&request, &mut retry, 0), libc::EBADF);

    let aliased = NetdIpcRequest {
        socket_token: request.socket_token - 1,
        ..request
    };
    assert_eq!(
        dispatch_request(&aliased, &mut NetdIpcResponse::default(), 0),
        libc::EPROTO
    );
}
