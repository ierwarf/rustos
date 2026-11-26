use super::*;
use core::cell::Cell;

fn request(now: u64) -> NetdIpcRequest {
    NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op: SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
        pid: 7,
        tid: 8,
        deadline_ns: now + 5_000_000,
        ..NetdIpcRequest::default()
    }
}

#[test]
fn maintenance_requires_exact_live_kernel_sender_and_cannot_create_or_duplicate_sockets() {
    let now = 1_000_000_000;
    for (sender_pid, sender_tid, op, expected) in [
        (0, 42, SYSCALL_OFFLOAD_OP_LINUX_CLOSE, 0),
        (7, 42, SYSCALL_OFFLOAD_OP_LINUX_CLOSE, libc::EACCES),
        (0, 0, SYSCALL_OFFLOAD_OP_LINUX_CLOSE, libc::EACCES),
        (0, 42, SYSCALL_OFFLOAD_OP_LINUX_SOCKET, libc::EINVAL),
        (0, 42, SYSCALL_OFFLOAD_OP_LINUX_DUP, libc::EINVAL),
    ] {
        let mut request = NetdIpcRequest {
            pid: 0, tid: 42, op, socket_token: 9,
            operation_hi: 7, operation_lo: 1, ..request(now)
        };
        let called = Cell::new(false);
        let mut status = -1;
        let result = service_received_request_with(
            ReceivedNetdRequest {
                result: NETD_IPC_REQUEST_HEADER_SIZE as i64,
                request: &mut request, reply_cap: 11, sender_pid, sender_tid,
            },
            || now,
            |_, _, _| { called.set(true); Some(0) },
            |_, response| { status = response.status; 0 },
        );
        assert_eq!(result, ReceiveTurn::Replied { status: 0, poll_after_reply: true });
        assert_eq!(status, expected);
        assert_eq!(called.get(), expected == 0);
    }
}

#[test]
fn empty_request_gets_one_terminal_reply_then_the_next_live_request_is_admitted() {
    let now = 1_000_000_000;
    let dispatches = Cell::new(0);
    let clock_reads = Cell::new(0);
    let mut replies = Vec::new();
    for (received, reply_cap) in [
        (-i64::from(libc::EAGAIN), 0),
        (0, 11),
        (NETD_IPC_REQUEST_HEADER_SIZE as i64, 12),
    ] {
        let mut request = request(now);
        let result = service_received_request_with(
            ReceivedNetdRequest {
                result: received,
                request: &mut request,
                reply_cap,
                sender_pid: 7,
                sender_tid: 8,
            },
            || {
                clock_reads.set(clock_reads.get() + 1);
                now
            },
            |request, response, cap| {
                assert_eq!(cap, 12);
                assert!(request.deadline_ns > now);
                dispatches.set(dispatches.get() + 1);
                response.value = 99;
                Some(0)
            },
            |cap, response| {
                replies.push((cap, response.status, response.value));
                0
            },
        );
        assert_eq!(
            result,
            if received < 0 {
                ReceiveTurn::Retry
            } else {
                ReceiveTurn::Replied {
                    status: 0,
                    poll_after_reply: true,
                }
            }
        );
    }
    assert_eq!(replies, [(11, libc::EINVAL, 0), (12, 0, 99)]);
    assert_eq!(dispatches.get(), 1);
    assert_eq!(
        clock_reads.get(),
        1,
        "only an authenticated valid frame needs time admission"
    );
}

#[test]
fn empty_reply_refusal_is_not_misclassified_as_an_idle_receive() {
    let mut request = request(1_000_000_000);
    let mut replies = 0;
    let result = service_received_request_with(
        ReceivedNetdRequest {
            result: 0,
            request: &mut request,
            reply_cap: 11,
            sender_pid: 7,
            sender_tid: 8,
        },
        || panic!("malformed request read the clock"),
        |_, _, _| panic!("malformed request reached dispatch"),
        |cap, response| {
            assert_eq!(cap, 11);
            assert_eq!(response.status, libc::EINVAL);
            replies += 1;
            -i64::from(libc::EPIPE)
        },
    );
    assert_eq!(replies, 1);
    assert_eq!(
        result,
        ReceiveTurn::Replied {
            status: -i64::from(libc::EPIPE),
            poll_after_reply: true
        }
    );
}

#[test]
fn deferred_admission_transfers_reply_custody_without_replying_twice() {
    let now = 1_000_000_000;
    let mut request = request(now);
    let result = service_received_request_with(
        ReceivedNetdRequest {
            result: NETD_IPC_REQUEST_HEADER_SIZE as i64,
            request: &mut request,
            reply_cap: 12,
            sender_pid: 7,
            sender_tid: 8,
        },
        || now,
        |_, _, cap| {
            assert_eq!(cap, 12);
            None
        },
        |_, _| panic!("deferred request replied before its owner completed"),
    );
    assert_eq!(result, ReceiveTurn::Deferred);
}
