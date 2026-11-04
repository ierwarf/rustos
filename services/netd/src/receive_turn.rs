//! One dequeued request's validation, admission and terminal reply custody.
//!
//! - **Owner:** `netd` owns admission of every dequeued request.
//! - **Boundary:** Frames, sender identity (`sender_pid`/`sender_tid`) and the commercial envelope are untrusted.
//! - **Lifecycle:** Receive, validate, check sender, clamp deadline once, dispatch, then reply or explicitly defer.
//! - **Concurrency:** Runs on each fixed receiver thread; shared state is reached only through dispatch.
//! - **Failure:** Negative receives own no reply; every nonnegative result reaches one terminal reply or deferral.
//! - **Forbidden:** No dispatch before sender equality, no unvalidated frame, no silent drop of a received request.
//! - **Evidence:** `receive_custody_tests`, `netd-deferred-reply-lifecycle`.
//!
//! Negative receive errors own no reply. Every nonnegative result, including
//! an empty frame, reaches either one terminal reply or explicit deferral.

use super::*;

// The commercial frame is reinterpreted in place inside the NetdIpcRequest
// receive buffer; the receive capacity must be able to hold (and align) it.
const _: () = assert!(
    size_of::<CommercialMaxProtocolRequest>() <= size_of::<NetdIpcRequest>()
        && align_of::<CommercialMaxProtocolRequest>() <= align_of::<NetdIpcRequest>()
);

pub(super) struct ReceivedNetdRequest<'a> {
    pub result: i64,
    pub request: &'a mut NetdIpcRequest,
    pub reply_cap: u64,
    pub sender_pid: u64,
    pub sender_tid: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ReceiveTurn {
    Retry,
    Deferred,
    Replied { status: i64, poll_after_reply: bool },
}

pub(super) fn service_received_request_with(
    turn: ReceivedNetdRequest<'_>,
    now: impl FnOnce() -> u64,
    dispatch: impl FnOnce(&NetdIpcRequest, &mut NetdIpcResponse, u64) -> Option<i32>,
    reply: impl FnOnce(u64, &NetdIpcResponse) -> i64,
) -> ReceiveTurn {
    let Ok(received) = usize::try_from(turn.result) else {
        return ReceiveTurn::Retry;
    };
    let request = turn.request;
    if received == size_of::<CommercialMaxProtocolRequest>() {
        // SAFETY: the received length equals the commercial frame size and the
        // module-level assertion proves the buffer holds and aligns it.
        let commercial = unsafe {
            &*((request as *const NetdIpcRequest).cast::<CommercialMaxProtocolRequest>())
        };
        if commercial.header.version == COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
            && commercial.header.protocol == COMMERCIAL_MAX_PROTOCOL_NETD
        {
            let status = if commercial.header.subject_pid == turn.sender_pid
                && commercial.header.subject_tid == turn.sender_tid
            {
                reply_commercial_request(turn.reply_cap, commercial)
            } else {
                reply_commercial_error(turn.reply_cap, commercial, libc::EACCES)
            };
            return ReceiveTurn::Replied {
                status,
                poll_after_reply: false,
            };
        }
    }
    let mut response = NetdIpcResponse {
        version: NETD_IPC_ABI_VERSION,
        op: request.op,
        ..NetdIpcResponse::default()
    };
    response.status = match validate_request(received, request) {
        Err(errno) => errno,
        Ok(()) if request.pid != turn.sender_pid || request.tid != turn.sender_tid => libc::EACCES,
        Ok(()) => {
            // Admission stamps the immutable caller deadline once; retries
            // and deferred queues cannot refresh their service-local budget.
            match clamp_request_deadline_at(request, now()) {
                Err(errno) => errno,
                Ok(()) => match dispatch(request, &mut response, turn.reply_cap) {
                    Some(status) => status,
                    None => return ReceiveTurn::Deferred,
                },
            }
        }
    };
    ReceiveTurn::Replied {
        status: reply(turn.reply_cap, &response),
        poll_after_reply: true,
    }
}

pub(super) fn dispatch_admitted_request(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    reply_cap: u64,
) -> Option<i32> {
    let status = if request.op == NETD_IPC_OP_DVM_SESSION {
        let status = handle_dvm_session(request, request.pid);
        if status != 0 {
            let _ = writeln!(
                std::io::stderr(),
                "netd: DVM session transition rejected status={status} epoch={} action={}",
                request.arg0,
                request.arg1,
            );
        }
        status
    } else if is_deferred_local_poll_request(request) {
        let status = handle_poll_socket(request, response);
        if status == 0 && response.value == 0 {
            match defer_local_poll_request(*request, reply_cap) {
                Ok(()) => return None,
                Err(errno) => errno,
            }
        } else {
            if status == 0 {
                response.reserved1 = NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF;
            }
            status
        }
    } else if is_accounting_request(request) {
        match enqueue_lifecycle_request(*request, reply_cap) {
            Ok(()) => return None,
            Err(errno) => errno,
        }
    } else if is_blocking_request(request) {
        match enqueue_blocking_request(*request, reply_cap) {
            Ok(()) => return None,
            Err(errno) => errno,
        }
    } else {
        let status = dispatch_request(request, response, reply_cap);
        if local_socket_reply_requests_latency_handoff(request, response, status) {
            response.reserved1 = NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF;
        }
        status
    };
    Some(status)
}
