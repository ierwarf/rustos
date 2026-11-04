use super::*;

/// Every supported socket/socketpair/accept publishes initial references only
/// through the exact timely reply. Errors carry no prepared descriptors.
pub(super) fn netd_reply_uses_prepared_socket(request: &NetdIpcRequest) -> bool {
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR => {
            (request.op == SYSCALL_OFFLOAD_OP_LINUX_SOCKET
                && request.arg0 == linux_abi::AF_INET
                && rustos_user_abi::syscall::net_broker_inet_socket_shape_valid(
                    request.arg0,
                    request.arg1,
                    request.arg2,
                ))
                || rustos_user_abi::syscall::net_broker_unix_socket_shape_valid(
                    request.arg0,
                    request.arg1,
                    request.arg2,
                )
        }
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT => true,
        _ => false,
    }
}

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
        None => deadline::netd_timeout_class(request.op).timeout_ms(),
    };
    let reply_deadline = ipc_ops::service_reply_deadline_tick_for_service(
        IPC_SERVICE_NETD,
        request_bytes,
        class_timeout_ms,
    );
    let (response, entries) = ipc_ops::call_service_endpoint_with_received_entries_until(
        IPC_SERVICE_NETD,
        request_bytes,
        if request.op == SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR {
            2
        } else {
            1
        },
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
        return super::finish_netd_ipc_response(
            decoded,
            super::super::vfs_meta::consume_netd_release_payload,
        );
    }
    install_prepared_netd_socket_response(
        request,
        decoded,
        entries,
        ipc_ops::drop_transfer_entries,
        || ipc_ops::reply_deadline_expired(reply_deadline),
        |entries| {
            let prepared = ipc_ops::prepare_transfer_entries_for_current_process(entries)?;
            let fds = prepared.fds()?;
            if request.op == SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR {
                let [left, right] = fds.as_slice() else {
                    return Err(LINUX_EINVAL);
                };
                let mut bytes = [0_u8; 8];
                bytes[..4].copy_from_slice(&left.to_ne_bytes());
                bytes[4..].copy_from_slice(&right.to_ne_bytes());
                usermem::write_current_user_bytes(request.arg3, &bytes)
                    .map_err(address_space_error_to_linux_errno)?;
            } else if request.op == SYSCALL_OFFLOAD_OP_LINUX_ACCEPT && request.arg1 != 0 {
                super::super::vfs_meta::write_current_sockaddr_payload(
                    request.arg1,
                    request.arg2,
                    &decoded.payload[..decoded.payload_len as usize],
                )?;
            }
            if ipc_ops::reply_deadline_expired(reply_deadline) {
                return Err(LINUX_ETIMEDOUT);
            }
            prepared.commit()?;
            Ok(fds)
        },
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
    let fd = installed[0];
    // LIFECYCLE: The entry was transferred into this process only after the
    // reply take won the deadline race. The old token is no longer exposed in
    // this wire result; `value` is the exact installed fd.
    decoded.value = if request.op == SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR {
        0
    } else {
        fd as u64
    };
    decoded.payload_len = 0;
    Ok(decoded)
}

pub(super) fn prepared_netd_socket_response_matches(
    request: &NetdIpcRequest,
    decoded: &NetdIpcResponse,
    entries: &[multitask::TransferredHandleEntry],
) -> bool {
    let accept = request.op == SYSCALL_OFFLOAD_OP_LINUX_ACCEPT;
    let pair = request.op == SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR;
    let socket_type = if accept {
        // Linux accept/accept4 never inherit O_NONBLOCK from the listener.
        linux_abi::SOCK_STREAM
            | (request.arg3 & (linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC))
    } else {
        request.arg1
    };
    let expected_status_flags = socket_type & linux_abi::SOCK_NONBLOCK;
    let expected_fd_flags = if socket_type & linux_abi::SOCK_CLOEXEC != 0 {
        multitask::FD_CLOEXEC
    } else {
        0
    };
    if decoded.reserved0 != 0
        || decoded.reserved1 != 0
        || decoded.value == 0
        || entries.len() != if pair { 2 } else { 1 }
        || (!accept && decoded.payload_len != if pair { 8 } else { 0 })
    {
        return false;
    }
    let second = if pair {
        u64::from_ne_bytes(decoded.payload[..8].try_into().unwrap())
    } else {
        0
    };
    if pair && (second == 0 || second == decoded.value) {
        return false;
    }
    let domain = if accept {
        linux_abi::AF_UNIX
    } else {
        request.arg0
    };
    let protocol = if accept { 0 } else { request.arg2 };
    for (index, entry) in entries.iter().enumerate() {
        if entry.entry().fd_flags() != expected_fd_flags
            || entry.entry().status_flags() != expected_status_flags
        {
            return false;
        }
        let token = if index == 0 { decoded.value } else { second };
        let matches = match entry.entry().handle() {
            multitask::KernelHandle::InetSocket(socket) => {
                domain == linux_abi::AF_INET
                    && socket.token_id() == token
                    && socket.domain() == domain
                    && socket.type_() == (socket_type & linux_abi::SOCK_TYPE_MASK)
                    && socket.protocol() == protocol
            }
            multitask::KernelHandle::Socket(socket) => {
                domain == linux_abi::AF_UNIX
                    && socket.token_id() == token
                    && socket.domain() == domain
                    && socket.type_() == linux_abi::SOCK_STREAM
                    && socket.protocol() == protocol
                    && if accept {
                        socket.channel_id() != 0 && socket.channel_side() == 2
                    } else if pair {
                        socket.channel_id() != 0 && socket.channel_side() == index as u8 + 1
                    } else {
                        socket.channel_id() == 0 && socket.channel_side() == 0
                    }
            }
            _ => false,
        };
        if !matches {
            return false;
        }
    }
    if pair {
        let (multitask::KernelHandle::Socket(left), multitask::KernelHandle::Socket(right)) =
            (entries[0].entry().handle(), entries[1].entry().handle())
        else {
            return false;
        };
        if left.channel_id() != right.channel_id() {
            return false;
        }
    }
    true
}
