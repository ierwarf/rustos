//! Service entry: health worker, request-worker pool, INET readiness worker, receive loop and reply publication.

use super::*;

/// Creates and registers the service endpoint, then serves it forever.
pub(super) fn run() {
    debug_line("netd: service start");
    let endpoint = rustos_svc_runtime::ipc::endpoint_create_with_limits(size_of::<NetdIpcRequest>() as u64, 0);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "netd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }

    // Registration atomically retires predecessor transport authority before
    // publishing the rootd-authorized owner. A pre-registration packet-broker
    // call cannot hold NetPolicy; a post-registration reset races new work.
    let register =
        rustos_svc_runtime::ipc::register_service_endpoint(IPC_SERVICE_NETD, endpoint as u64);
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "netd: endpoint register failed errno={}",
            -register
        );
        return;
    }

    debug_line("netd: network policy endpoint registered");
    if let Err(error)=start_service_health_worker() {
        debug_line(&format!("netd: heartbeat worker creation failed error={error}"));
        return;
    }
    serve(endpoint as u64);
}

/// Own health proof independently of socket traffic. Slow rootd control IPC
/// cannot be allowed to wait for a NetState, poll-waiter or deferred-reply lock.
pub(super) fn start_service_health_worker()->Result<(),std::io::Error>{
    thread::Builder::new()
        .name("netd-health".into())
        .spawn(||{
            let mut health=rustos_svc_runtime::heartbeat::Heartbeat::new(IPC_SERVICE_NETD);
            loop {
                let now=rustos_svc_runtime::syscall::monotonic_nanos();
                let _=health.poll(now);
                thread::sleep(Duration::from_millis(250));
            }
        }).map(|_|())
}

pub(super) fn serve(endpoint: u64) {
    start_blocking_workers();
    start_inet_readiness_worker();
    for worker_index in 1..NETD_REQUEST_WORKERS {
        let name = format!("netd-rpc-{worker_index}");
        if let Err(error) = thread::Builder::new()
            .name(name)
            .spawn(move || serve_request_loop(endpoint))
        {
            debug_line(&format!(
                "netd: request worker unavailable index={worker_index} error={error}"
            ));
        }
    }
    serve_request_loop(endpoint);
}

pub(super) fn start_inet_readiness_worker() {
    if let Err(error) = thread::Builder::new()
        .name("netd-inet-readiness".to_owned())
        .spawn(inet_readiness_worker_loop)
    {
        debug_line(&format!(
            "netd: INET readiness worker unavailable error={error}"
        ));
        std::process::exit(134);
    }
}

pub(super) fn inet_readiness_worker_loop() {
    loop {
        let has_sockets = {
            let mut state = lock_net_state();
            if let Some(stack) = state.inet.as_mut() {
                stack.poll_budget(INET_READINESS_POLL_BUDGET);
            }
            !state.inet_sockets.is_empty()
        };
        thread::sleep(if has_sockets {
            INET_READINESS_POLL_INTERVAL
        } else {
            INET_IDLE_POLL_INTERVAL
        });
    }
}

pub(super) fn serve_request_loop(endpoint: u64) {
    loop {
        service_local_poll_waiters();
        let mut request = NetdIpcRequest::default();
        let mut reply_cap = 0_u64;
        let mut sender_pid = 0_u64;
        let mut sender_tid = 0_u64;
        // With nothing deferred this loop has exactly one event source and may
        // block on it indefinitely. With a poll deferred it has two, and the
        // second one is a deadline only this loop can honour.
        let idle_budget_ms = next_local_poll_budget_ms();
        let received = match idle_budget_ms {
            None => syscall6(
                SYS_RUSTOS_IPC_RECV_WITH_SENDER,
                endpoint,
                (&mut request as *mut NetdIpcRequest) as u64,
                size_of::<NetdIpcRequest>() as u64,
                (&mut reply_cap as *mut u64) as u64,
                (&mut sender_pid as *mut u64) as u64,
                (&mut sender_tid as *mut u64) as u64,
            ),
            Some(budget_ms) => {
                let args = IpcRecvWithSenderArgs {
                    endpoint,
                    request_ptr: (&mut request as *mut NetdIpcRequest) as u64,
                    request_capacity: size_of::<NetdIpcRequest>() as u64,
                    reply_cap_ptr: (&mut reply_cap as *mut u64) as u64,
                    sender_pid_ptr: (&mut sender_pid as *mut u64) as u64,
                    sender_tid_ptr: (&mut sender_tid as *mut u64) as u64,
                };
                syscall2(
                    SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED,
                    (&args as *const IpcRecvWithSenderArgs) as u64,
                    budget_ms,
                )
            }
        };
        match service_received_request_with(
            ReceivedNetdRequest {
                result: received,
                request: &mut request,
                reply_cap,
                sender_pid,
                sender_tid,
            },
            rustos_svc_runtime::syscall::monotonic_nanos,
            dispatch_admitted_request,
            reply_netd_response,
        ) {
            ReceiveTurn::Retry => {
                // A bounded idle wait already spent its deadline; service its
                // deferred waiter now rather than adding another backoff.
                if idle_budget_ms.is_some() && received == -(libc::EAGAIN as i64) {
                    continue;
                }
                thread::sleep(RECV_BACKOFF);
            }
            ReceiveTurn::Deferred => continue,
            ReceiveTurn::Replied {
                status,
                poll_after_reply,
            } => {
                if status < 0 {
                    note_reply_failure(-status);
                }
                if poll_after_reply {
                    service_local_poll_waiters();
                }
            }
        }
    }
}

/// Accounts one failed reply. `EINVAL` from a response that passed the local
/// length check means the caller abandoned the call (see
/// `ABANDONED_REPLY_DIAGNOSTICS`); anything else is a real publication fault.
pub(super) fn note_reply_failure(errno: i64) {
    if errno == i64::from(libc::EINVAL) {
        ABANDONED_REPLY_DIAGNOSTICS.record("netd", "abandoned-caller ipc", errno);
    } else {
        REPLY_FAILURE_DIAGNOSTICS.record("netd", "ipc", errno);
    }
}

/// Publishes the one terminal reply of a detached request (blocking worker or
/// deferred poll) and accounts a rejection exactly like the receive loop does.
pub(super) fn publish_reply(reply_cap: u64, response: &NetdIpcResponse) -> i64 {
    let status = reply_netd_response(reply_cap, response);
    if status < 0 {
        note_reply_failure(-status);
    }
    status
}

#[cfg(test)]
mod startup_publication_tests {
    #[test]
    fn publication_does_not_require_a_preexisting_netpolicy_endpoint() {
        let source = include_str!("service_loop.rs");
        let start = source.split("pub(super) fn run()").nth(1)
            .and_then(|body| body.split("/// Own health proof").next())
            .expect("netd startup source");
        assert!(start.contains("register_service_endpoint(IPC_SERVICE_NETD"));
        assert!(!start.contains("call_packet_broker("),
            "startup must not require NetPolicy before registration or reset a published lease");
        // Runtime lease serialization is also exercised by kernel-io-manager's
        // netd_publication_serializes_reset_and_preserves_rejected_admission.

    }
}

pub(super) fn reply_netd_response(reply_cap: u64, response: &NetdIpcResponse) -> i64 {
    let Some(response_len) = NETD_IPC_RESPONSE_HEADER_SIZE
        .checked_add(response.payload_len as usize)
        .filter(|len| *len <= size_of::<NetdIpcResponse>())
    else {
        // Distinct from the kernel's EINVAL (abandoned reply capability).
        return -(libc::EMSGSIZE as i64);
    };
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (response as *const NetdIpcResponse) as u64,
        response_len as u64,
    )
}
