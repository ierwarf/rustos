//! Request validation and per-operation dispatch: every admitted netd request is routed to exactly one handler.
//!
//! - **Owner:** `netd` owns dispatch; the kernel client owns the request, its caller deadline and its socket token.
//! - **Boundary:** Request headers, payload lengths and socket tokens are untrusted and validated before any handler runs.
//! - **Lifecycle:** One request yields exactly one reply, or a detached reply capability owned by a bounded worker pool.
//! - **Concurrency:** Handlers run on the RPC workers; shared socket and stack state is reached only through the global tables' locks.
//! - **Failure:** A malformed or expired request returns a typed errno and mutates nothing; a late request after caller timeout is replay-safe.
//! - **Forbidden:** No handler may extend the caller's deadline, block an RPC worker on a provider wait, or reply twice.
//! - **Evidence:** `netd_v7_requires_a_nonzero_caller_deadline_and_exact_header_length` and the `netd-deadline-zero-admitted` mutant.

use super::*;

pub(super) fn reply_commercial_request(reply_cap: u64, request: &CommercialMaxProtocolRequest) -> i64 {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = match validate_commercial_request(request) {
        Ok(()) => dispatch_commercial_request(request, &mut response),
        Err(errno) => errno,
    };
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    )
}

pub(super) fn reply_commercial_error(
    reply_cap: u64,
    request: &CommercialMaxProtocolRequest,
    status: i32,
) -> i64 {
    let response = CommercialMaxProtocolResponse {
        header: request.header,
        status,
        ..CommercialMaxProtocolResponse::default()
    };
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    )
}

pub(super) fn dispatch_request(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    reply_cap: u64,
) -> i32 {
    if request.op == NETD_IPC_OP_REF_ACK {
        return acknowledge_ref_result(request);
    }
    let replay_safe_ref = matches!(
        request.op,
        SYSCALL_OFFLOAD_OP_LINUX_DUP | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
    );
    if replay_safe_ref {
        match begin_ref_result(request, response) {
            Ok(RefReplayAction::Replay(status)) => return status,
            Ok(RefReplayAction::Execute) => {}
            Err(errno) => return errno,
        }
    }
    let status = match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET => handle_socket(request, response, reply_cap),
        SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR => handle_socketpair(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_DUP => handle_dup(request),
        SYSCALL_OFFLOAD_OP_LINUX_CLOSE => handle_close(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_BIND => handle_bind(request),
        SYSCALL_OFFLOAD_OP_LINUX_LISTEN => handle_listen(request),
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT => handle_accept(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_CONNECT => handle_connect(request),
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG => {
            handle_send(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => {
            handle_recv(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET => handle_poll_socket(request, response),
        NETD_IPC_OP_POLL_BATCH => handle_poll_batch(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME => handle_getsockname(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME => handle_getpeername(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT => handle_setsockopt(request),
        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT => handle_getsockopt(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN => handle_shutdown(request),
        _ => libc::EINVAL,
    };
    if status != 0 && status != libc::EAGAIN {
        static ERRORS: AtomicUsize = AtomicUsize::new(0);
        let count = ERRORS.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 16 || count.is_power_of_two() {
            debug_line(&format!("netd: operation failed pid={} op={} arg0={} arg1={} arg2={} errno={} total={}", request.pid, request.op, request.arg0, request.arg1, request.arg2, status, count));
        }
    }
    if replay_safe_ref {
        complete_ref_result(request, response, status);
    }
    status
}

pub(super) fn handle_dvm_session(request: &NetdIpcRequest, sender_pid: u64) -> i32 {
    if rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_INPUTD, sender_pid) < 0 {
        return libc::EACCES;
    }
    let Ok(epoch) = u32::try_from(request.arg0) else {
        return libc::EINVAL;
    };
    if epoch == 0
        || request.arg2 != 0
        || request.arg3 != 0
        || request.arg4 != 0
        || request.arg5 != 0
    {
        return libc::EINVAL;
    }
    let Ok(mut active) = dvm_session_epoch().lock() else {
        return libc::EIO;
    };
    match admit_dvm_session_transition(*active, epoch, request.arg1) {
        Ok(DvmSessionTransition::Grant) => {
            match call_packet_broker(NET_BROKER_OP_PACKET_LEASE_GRANT, u64::from(epoch), 0) {
                Ok(_) => {
                    *active = epoch;
                    0
                }
                Err(errno) => errno,
            }
        }
        Ok(DvmSessionTransition::Revoke) => {
            match call_packet_broker(NET_BROKER_OP_PACKET_LEASE_REVOKE, u64::from(epoch), 0) {
                Ok(_) => {
                    *active = 0;
                    0
                }
                Err(errno) => errno,
            }
        }
        Err(errno) => errno,
    }
}

pub(super) fn admit_dvm_session_transition(
    active_epoch: u32,
    requested_epoch: u32,
    action: u64,
) -> Result<DvmSessionTransition, i32> {
    if requested_epoch == 0 {
        return Err(libc::EINVAL);
    }
    match action {
        NETD_DVM_SESSION_GRANT if active_epoch == 0 || active_epoch == requested_epoch => {
            Ok(DvmSessionTransition::Grant)
        }
        NETD_DVM_SESSION_GRANT => Err(libc::EBUSY),
        NETD_DVM_SESSION_REVOKE if active_epoch == requested_epoch => {
            Ok(DvmSessionTransition::Revoke)
        }
        NETD_DVM_SESSION_REVOKE => Err(libc::ESTALE),
        _ => Err(libc::EINVAL),
    }
}

pub(super) fn validate_request(received: usize, request: &NetdIpcRequest) -> Result<(), i32> {
    let expected_len = NETD_IPC_REQUEST_HEADER_SIZE.checked_add(request.payload_len as usize);
    if received < NETD_IPC_REQUEST_HEADER_SIZE
        || expected_len != Some(received)
        || received > size_of::<NetdIpcRequest>()
        || request.version != NETD_IPC_ABI_VERSION
        || request.flags != 0
        || request.reserved1 != 0
        || request.deadline_ns == 0
        || request.pid == 0
        || request.tid == 0
        || request.payload_len as usize > request.payload.len()
    {
        return Err(libc::EINVAL);
    }
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET
        && !matches!(request.arg2, NETD_POLL_MODE_QUERY | NETD_POLL_MODE_WAIT)
    {
        return Err(libc::EINVAL);
    }
    if matches!(
        request.op,
        SYSCALL_OFFLOAD_OP_LINUX_DUP | SYSCALL_OFFLOAD_OP_LINUX_CLOSE | NETD_IPC_OP_REF_ACK
    ) && request.operation_hi == 0
        && request.operation_lo == 0
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET
        | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR
        | SYSCALL_OFFLOAD_OP_LINUX_DUP
        | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
        | SYSCALL_OFFLOAD_OP_LINUX_BIND
        | SYSCALL_OFFLOAD_OP_LINUX_LISTEN
        | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT
        | SYSCALL_OFFLOAD_OP_LINUX_CONNECT
        | SYSCALL_OFFLOAD_OP_LINUX_SENDTO
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME
        | SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME
        | SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN
        | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
        | SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET
        | NETD_IPC_OP_POLL_BATCH
        | NETD_IPC_OP_DVM_SESSION
        | NETD_IPC_OP_REF_ACK => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

pub(super) fn request_msg_flags(request: &NetdIpcRequest) -> u64 {
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SENDMSG | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => request.arg2,
        _ => request.arg3,
    }
}

pub(super) fn request_credentials(request: &NetdIpcRequest) -> Credentials {
    Credentials {
        pid: request.pid as i32,
        uid: request.euid,
        gid: request.egid,
    }
}
