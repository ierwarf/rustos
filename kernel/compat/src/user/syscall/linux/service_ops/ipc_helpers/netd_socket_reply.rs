use super::*;

/// Only AF_INET stream creation uses reply-bound descriptor publication.
/// AF_UNIX keeps its existing broker-installed fd reply, and rejected socket
/// families/types return an ordinary byte response without a descriptor.
pub(super) fn netd_reply_uses_prepared_socket(request: &NetdIpcRequest) -> bool {
    request.op == SYSCALL_OFFLOAD_OP_LINUX_SOCKET
        && request.arg0 == linux_abi::AF_INET
        && request.arg1 & linux_abi::SOCK_TYPE_MASK == linux_abi::SOCK_STREAM
}

/// Socket creation is the one netd operation whose successful reply carries a
/// descriptor. Take exactly one prepared entry under the original reply end,
/// validate the complete v7 response and token before publication, then move
/// that entry into the current caller's descriptor table.
pub(super) fn call_netd_socket_ipc_request_impl(
    request: &NetdIpcRequest,
    timeout_ms: Option<u64>,
) -> Result<NetdIpcResponse, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let request_len = NETD_IPC_REQUEST_HEADER_SIZE
        .checked_add(request.payload_len as usize)
        .filter(|len| *len <= size_of::<NetdIpcRequest>())
        .ok_or(LINUX_EINVAL)?;
    let request_bytes = &as_bytes(request)[..request_len];
    let class_timeout_ms = match timeout_ms {
        Some(timeout_ms) => deadline::netd_timeout_class(request.op).cap_timeout_ms(timeout_ms),
        None => ipc_ops::ServiceIpcClass::BulkData.timeout_ms(),
    };
    let reply_deadline = ipc_ops::service_reply_deadline_tick_for_service(
        IPC_SERVICE_NETD,
        request_bytes,
        class_timeout_ms,
    );
    let (response, entries) = ipc_ops::call_service_endpoint_with_received_entries_until(
        IPC_SERVICE_NETD,
        request_bytes,
        1,
        reply_deadline,
    )?;
    let elapsed_ms = ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks());
    diagnostics::log_slow_service_call(
        "netd",
        request.op,
        elapsed_ms,
        request.pid,
        request.tid,
        response.len() as i64,
        Some("reply-bound-socket"),
    );

    let decoded = match decode_netd_ipc_response(request, response.as_slice()) {
        Ok(decoded) => decoded,
        Err(errno) => {
            ipc_ops::drop_transfer_entries(entries);
            return Err(errno);
        }
    };
    if decoded.status != 0 {
        ipc_ops::drop_transfer_entries(entries);
        return Err(decoded.status.unsigned_abs() as i64);
    }
    install_prepared_netd_socket_response(
        request,
        decoded,
        entries,
        ipc_ops::drop_transfer_entries,
        || ipc_ops::reply_deadline_expired(reply_deadline),
        |entries| ipc_ops::install_transfer_entries_for_current_process(entries),
    )
}

/// Installs the validated socket entry after its timely reply take. The
/// closure owns its input on every result; the production closure publishes it
/// atomically into the current process, while the deterministic witness uses
/// the same one-entry decision boundary without a live scheduler context.
pub(super) fn install_prepared_netd_socket_response(
    request: &NetdIpcRequest,
    mut decoded: NetdIpcResponse,
    entries: Vec<multitask::TransferredHandleEntry>,
    discard: impl FnOnce(Vec<multitask::TransferredHandleEntry>),
    deadline_expired: impl FnOnce() -> bool,
    install: impl FnOnce(Vec<multitask::TransferredHandleEntry>) -> Result<Vec<i32>, i64>,
) -> Result<NetdIpcResponse, i64> {
    if !prepared_netd_socket_response_matches(request, &decoded, entries.as_slice()) {
        discard(entries);
        return Err(LINUX_EINVAL);
    }
    // The response take is the primary reply linearization point. Recheck the
    // same immutable absolute end immediately before the distinct descriptor
    // publication transaction so scheduler delay during decode cannot turn a
    // timed-out socket into a visible fd.
    if deadline_expired() {
        discard(entries);
        return Err(LINUX_ETIMEDOUT);
    }
    let installed = install(entries)?;
    let [fd] = installed.as_slice() else {
        unreachable!("one validated prepared socket entry must install one fd");
    };
    // LIFECYCLE: The entry was transferred into this process only after the
    // reply take won the deadline race. The old token is no longer exposed in
    // this wire result; `value` is the exact installed fd.
    decoded.value = *fd as u64;
    Ok(decoded)
}

pub(super) fn prepared_netd_socket_response_matches(
    request: &NetdIpcRequest,
    decoded: &NetdIpcResponse,
    entries: &[multitask::TransferredHandleEntry],
) -> bool {
    let expected_status_flags = request.arg1 & linux_abi::SOCK_NONBLOCK;
    let expected_fd_flags = if request.arg1 & linux_abi::SOCK_CLOEXEC != 0 {
        multitask::FD_CLOEXEC
    } else {
        0
    };
    decoded.payload_len == 0
        && decoded.reserved0 == 0
        && decoded.reserved1 == 0
        && decoded.value != 0
        && matches!(
            entries,
            [entry] if entry.entry().fd_flags() == expected_fd_flags
                && entry.entry().status_flags() == expected_status_flags
                && matches!(
                    entry.entry().handle(),
                    multitask::KernelHandle::InetSocket(socket)
                        if socket.token_id() == decoded.value
                            && socket.domain() == request.arg0
                            && socket.type_() == (request.arg1 & linux_abi::SOCK_TYPE_MASK)
                            && socket.protocol() == request.arg2
                )
        )
}
