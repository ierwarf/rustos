//! Single-endpoint input policy RPC loop and fused reply-receive boundary.
//!
//! - **Owner:** Inputd owns request validation, policy dispatch, and every
//!   terminal response; the service runtime owns only the raw syscall wrapper.
//! - **Boundary:** Kernel-stamped sender identity and all request bytes are
//!   untrusted until the exact protocol and delegation checks succeed.
//! - **Lifecycle:** Register once, receive one request, produce one terminal
//!   reply, then atomically enter the next receive without replaying old caps.
//! - **Concurrency:** The RPC loop never holds the input queue across IPC; the
//!   independent DVM ingestion worker retains transport progress.
//! - **Failure:** Malformed dequeued requests receive `EINVAL`; a pre-commit
//!   failure retries only the still-live reply through the standalone path,
//!   while a tagged post-commit failure enters a fresh receive with bounded logs.
//! - **Forbidden:** No identity-blind receive, abandoned caller, handle-bearing
//!   fused path, queue lock across IPC, or retry of a completed reply.
//! - **Evidence:** `ipc-reply-recv-transaction/IpcReplyRecvTransaction`, the
//!   malformed-request witness, source conformance, and focused mutations.

use super::*;

pub(super) fn run() {
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "inputd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }
    let register =
        rustos_svc_runtime::ipc::register_service_endpoint(IPC_SERVICE_INPUTD, endpoint as u64);
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "inputd: endpoint register failed errno={}",
            -register
        );
        return;
    }
    debug_line("inputd: input policy endpoint registered");
    serve(endpoint as u64);
}

pub(super) fn serve(endpoint: u64) {
    let queue = Arc::new(SharedInputQueueState::new());
    let dvm_ingress_log_state = Arc::new(DvmIngressLogState::default());
    start_dvm_ingestion_worker(Arc::clone(&queue), Arc::clone(&dvm_ingress_log_state));
    let mut request_buf = [0_u8; IPC_MAX_INLINE_BYTES];
    let mut reply_cap = 0_u64;
    let mut sender_pid = 0_u64;
    let mut sender_tid = 0_u64;
    let mut received = recv_input_request(
        endpoint,
        &mut request_buf,
        &mut reply_cap,
        &mut sender_pid,
        &mut sender_tid,
    );
    loop {
        if received < 0 {
            received = recv_input_request(
                endpoint,
                &mut request_buf,
                &mut reply_cap,
                &mut sender_pid,
                &mut sender_tid,
            );
            continue;
        }
        let request_size = received as usize;
        let next_received = if request_size == size_of::<CommercialMaxProtocolRequest>() {
            let request = read_unaligned::<CommercialMaxProtocolRequest>(&request_buf);
            let response = {
                let mut queue = lock_input_queue(&queue);
                commercial_response(&request, sender_pid, sender_tid, &mut queue)
            };
            log_dvm_ingress_observations(&queue, &dvm_ingress_log_state);
            reply_recv_input_request(
                endpoint,
                reply_cap,
                (&response as *const CommercialMaxProtocolResponse).cast::<u8>(),
                size_of::<CommercialMaxProtocolResponse>(),
                &mut request_buf,
                &mut reply_cap,
                &mut sender_pid,
                &mut sender_tid,
            )
        } else if request_size == size_of::<InputdPointerSurfaceRequest>() {
            debug_line("inputd: pointer surface request received");
            let request = read_unaligned::<InputdPointerSurfaceRequest>(&request_buf);
            let mut response = InputdIpcResponse {
                version: INPUTD_IPC_ABI_VERSION,
                op: request.op,
                ..InputdIpcResponse::default()
            };
            response.status = {
                let mut queue = lock_input_queue(&queue);
                if rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_UISERVER, sender_pid)
                    < 0
                {
                    libc::EACCES
                } else {
                    dispatch_pointer_surface_request(&request, &mut queue)
                }
            };
            response.approved_len = (response.status == 0) as u64;
            debug_line("inputd: pointer surface state applied");
            log_dvm_ingress_observations(&queue, &dvm_ingress_log_state);
            reply_recv_input_request(
                endpoint,
                reply_cap,
                (&response as *const InputdIpcResponse).cast::<u8>(),
                size_of::<InputdIpcResponse>(),
                &mut request_buf,
                &mut reply_cap,
                &mut sender_pid,
                &mut sender_tid,
            )
        } else if request_size == size_of::<InputdIpcRequest>() {
            let request = read_unaligned::<InputdIpcRequest>(&request_buf);
            if request.op == INPUTD_IPC_OP_READ {
                let mut response = InputdReadResponse {
                    version: INPUTD_IPC_ABI_VERSION,
                    op: request.op,
                    ..InputdReadResponse::default()
                };
                response.status = match validate(received as usize, &request) {
                    Ok(())
                        if !identity_is_exact_sender(
                            request.pid,
                            request.tid,
                            sender_pid,
                            sender_tid,
                        ) =>
                    {
                        libc::EACCES
                    }
                    Ok(()) => {
                        let mut queue = lock_input_queue(&queue);
                        dispatch_read(&request, &mut response, &mut queue)
                    }
                    Err(errno) => errno,
                };
                log_dvm_ingress_observations(&queue, &dvm_ingress_log_state);
                reply_recv_input_request(
                    endpoint,
                    reply_cap,
                    (&response as *const InputdReadResponse).cast::<u8>(),
                    size_of::<InputdReadResponse>(),
                    &mut request_buf,
                    &mut reply_cap,
                    &mut sender_pid,
                    &mut sender_tid,
                )
            } else {
                let mut response = InputdIpcResponse {
                    version: INPUTD_IPC_ABI_VERSION,
                    op: request.op,
                    ..InputdIpcResponse::default()
                };
                response.status = match validate(received as usize, &request) {
                    Ok(())
                        if !identity_is_exact_sender(
                            request.pid,
                            request.tid,
                            sender_pid,
                            sender_tid,
                        ) =>
                    {
                        libc::EACCES
                    }
                    Ok(()) => {
                        let mut queue = lock_input_queue(&queue);
                        dispatch(&request, &mut response, &mut queue)
                    }
                    Err(errno) => errno,
                };
                log_dvm_ingress_observations(&queue, &dvm_ingress_log_state);
                reply_recv_input_request(
                    endpoint,
                    reply_cap,
                    (&response as *const InputdIpcResponse).cast::<u8>(),
                    size_of::<InputdIpcResponse>(),
                    &mut request_buf,
                    &mut reply_cap,
                    &mut sender_pid,
                    &mut sender_tid,
                )
            }
        } else {
            // Every dequeued call owns a live one-shot reply capability. A
            // malformed request must receive an explicit terminal error; just
            // continuing would strand its caller until the deadline.
            let response = malformed_input_response();
            reply_recv_input_request(
                endpoint,
                reply_cap,
                (&response as *const InputdIpcResponse).cast::<u8>(),
                size_of::<InputdIpcResponse>(),
                &mut request_buf,
                &mut reply_cap,
                &mut sender_pid,
                &mut sender_tid,
            )
        };
        received = if next_received < 0 {
            recv_input_request(
                endpoint,
                &mut request_buf,
                &mut reply_cap,
                &mut sender_pid,
                &mut sender_tid,
            )
        } else {
            next_received
        };
    }
}

fn malformed_input_response() -> InputdIpcResponse {
    InputdIpcResponse {
        version: INPUTD_IPC_ABI_VERSION,
        status: libc::EINVAL,
        ..InputdIpcResponse::default()
    }
}

fn recv_input_request(
    endpoint: u64,
    request: &mut [u8; IPC_MAX_INLINE_BYTES],
    reply_cap: &mut u64,
    sender_pid: &mut u64,
    sender_tid: &mut u64,
) -> i64 {
    // SAFETY: every output points into this single-threaded loop's live stack
    // frame and remains exclusively borrowed for the complete blocking call.
    unsafe {
        rustos_svc_runtime::ipc::recv_with_sender(
            endpoint,
            request.as_mut_ptr(),
            request.len(),
            reply_cap,
            sender_pid,
            sender_tid,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn reply_recv_input_request(
    endpoint: u64,
    reply_cap: u64,
    response: *const u8,
    response_len: usize,
    request: &mut [u8; IPC_MAX_INLINE_BYTES],
    next_reply_cap: &mut u64,
    sender_pid: &mut u64,
    sender_tid: &mut u64,
) -> i64 {
    // SAFETY: response and request storage remain live and immovable until the
    // fused syscall returns; all output references are exclusive for that span.
    let result = unsafe {
        rustos_svc_runtime::ipc::reply_recv_with_sender(
            endpoint,
            reply_cap,
            response,
            response_len,
            request.as_mut_ptr(),
            request.len(),
            next_reply_cap,
            sender_pid,
            sender_tid,
        )
    };
    match classify_reply_recv_recovery(result) {
        ReplyRecvRecoveryAction::None => {}
        ReplyRecvRecoveryAction::PostCommit(errno) => {
            REPLY_FAILURE_DIAGNOSTICS.record("inputd", "reply-recv receive", errno);
        }
        ReplyRecvRecoveryAction::RetryReply(errno) => {
            REPLY_FAILURE_DIAGNOSTICS.record("inputd", "reply-recv reply", errno);
            // SAFETY: the fused syscall proved it did not consume `reply_cap`;
            // the same immutable response storage is still live for this
            // immediate, one-shot recovery attempt.
            let recovery =
                unsafe { rustos_svc_runtime::ipc::reply(reply_cap, response, response_len) };
            if recovery < 0 {
                REPLY_FAILURE_DIAGNOSTICS.record(
                    "inputd",
                    "reply-recv recovery",
                    recovery.checked_neg().unwrap_or(i64::MAX),
                );
            }
        }
        ReplyRecvRecoveryAction::ProtocolViolation => {
            panic!("inputd: invalid reply-recv result outside native ABI partition: {result}");
        }
    }
    result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplyRecvRecoveryAction {
    None,
    RetryReply(i64),
    PostCommit(i64),
    ProtocolViolation,
}

fn classify_reply_recv_recovery(result: i64) -> ReplyRecvRecoveryAction {
    match rustos_svc_runtime::ipc::reply_recv_result_kind(result) {
        rustos_user_abi::syscall::IpcReplyRecvResultKind::Success => ReplyRecvRecoveryAction::None,
        rustos_user_abi::syscall::IpcReplyRecvResultKind::PreCommitError(errno) => {
            ReplyRecvRecoveryAction::RetryReply(errno)
        }
        rustos_user_abi::syscall::IpcReplyRecvResultKind::PostCommitError(errno) => {
            ReplyRecvRecoveryAction::PostCommit(errno)
        }
        rustos_user_abi::syscall::IpcReplyRecvResultKind::Invalid => {
            ReplyRecvRecoveryAction::ProtocolViolation
        }
    }
}

fn commercial_response(
    request: &CommercialMaxProtocolRequest,
    sender_pid: u64,
    sender_tid: u64,
    queue: &mut InputQueue,
) -> CommercialMaxProtocolResponse {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = if !request.subject_is_exact_sender(sender_pid, sender_tid) {
        libc::EACCES
    } else {
        validate_commercial_request(request)
            .and_then(|_| dispatch_commercial_request(request, &mut response, queue))
            .err()
            .unwrap_or(0)
    };
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_dequeued_request_has_terminal_error_reply() {
        let response = malformed_input_response();
        assert_eq!(response.version, INPUTD_IPC_ABI_VERSION);
        assert_eq!(response.status, libc::EINVAL);
    }

    #[test]
    fn reply_recv_recovery_retries_only_a_proven_live_reply() {
        assert_eq!(
            classify_reply_recv_recovery(-i64::from(libc::EFAULT)),
            ReplyRecvRecoveryAction::RetryReply(i64::from(libc::EFAULT))
        );
        assert_eq!(
            classify_reply_recv_recovery(-4097),
            ReplyRecvRecoveryAction::PostCommit(1)
        );
        assert_eq!(
            classify_reply_recv_recovery(-4096),
            ReplyRecvRecoveryAction::ProtocolViolation
        );
        assert_eq!(
            classify_reply_recv_recovery(0),
            ReplyRecvRecoveryAction::None
        );
    }
}
