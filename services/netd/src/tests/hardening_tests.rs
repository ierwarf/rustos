use super::*;

fn request_for(op: u16, arg0: u64) -> NetdIpcRequest {
    NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op,
        arg0,
        pid: 1,
        tid: 1,
        ..NetdIpcRequest::default()
    }
}

#[test]
fn lifecycle_pool_is_required_for_all_charge_bearing_operations() {
    // Every charge-bearing operation is detached, not only provider waits.
    for op in [
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
        SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
    ] {
        assert!(is_accounting_request(&request_for(op, linux_abi::AF_UNIX)));
    }
    for op in [
        SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
        SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
    ] {
        assert!(!is_accounting_request(&request_for(op, linux_abi::AF_UNIX)));
    }
}

#[test]
fn lifecycle_pool_has_its_own_bound_and_cannot_borrow_blocking_slots() {
    // A pool with no workers refuses instead of queueing a request nobody runs.
    let request = NetdIpcRequest {
        deadline_ns: u64::MAX,
        ..request_for(SYSCALL_OFFLOAD_OP_LINUX_SOCKET, linux_abi::AF_INET)
    };
    let queue: &'static BlockingRequestQueue = Box::leak(Box::new(BlockingRequestQueue {
        requests: Mutex::new(VecDeque::new()),
        available: Condvar::new(),
    }));
    let pending = AtomicUsize::new(0);
    let active = AtomicUsize::new(0);
    assert_eq!(
        enqueue_to_pool(queue, &pending, 2, &active, request, 1, 1),
        Err(libc::EAGAIN)
    );
    active.store(1, Ordering::Release);
    assert_eq!(
        enqueue_to_pool(queue, &pending, 2, &active, request, 1, 1),
        Ok(())
    );
    assert_eq!(
        enqueue_to_pool(queue, &pending, 2, &active, request, 2, 1),
        Ok(())
    );
    assert_eq!(
        enqueue_to_pool(queue, &pending, 2, &active, request, 3, 1),
        Err(libc::EAGAIN)
    );
    assert_eq!(queue.requests.lock().unwrap().len(), 2);
    // The shared blocking-I/O counters were never touched.
    assert_eq!(PENDING_LIFECYCLE_REQUESTS.load(Ordering::Acquire), 0);
}

#[test]
fn blocking_retry_backoff_doubles_to_a_fixed_cap() {
    let ms: Vec<u128> = (0..6)
        .map(|attempt| blocking_retry_backoff(attempt).as_millis())
        .collect();
    assert_eq!(ms, [1, 2, 4, 4, 4, 4]);
    assert_eq!(blocking_retry_backoff(usize::MAX).as_millis(), 4);
}

fn replay_entry(operation_lo: u64, complete: bool, admitted: StdInstant) -> RefReplayEntry {
    RefReplayEntry {
        operation_hi: 1,
        operation_lo,
        op: SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
        socket_token: 9,
        status: 0,
        value: 0,
        release: Vec::new(),
        complete,
        admitted,
    }
}

#[test]
fn full_replay_log_reclaims_only_old_completed_results() {
    let base = StdInstant::now();
    let later = base + REF_REPLAY_MIN_RETENTION + Duration::from_secs(1);
    let mut log = VecDeque::from([
        // Old and acknowledged-by-nobody: reclaimable.
        replay_entry(1, true, base),
        // Old but still executing: its request owns the slot.
        replay_entry(2, false, base),
        // Recent completed result: a retry may still need it.
        replay_entry(3, true, later - Duration::from_secs(1)),
    ]);
    reclaim_stale_ref_results(&mut log, later);
    let kept: Vec<u64> = log.iter().map(|entry| entry.operation_lo).collect();
    assert_eq!(kept, [2, 3]);
}

#[test]
fn local_poll_passes_are_serialized_and_never_strand_a_request() {
    let requested = AtomicBool::new(false);
    let running = AtomicBool::new(false);
    let passes = std::cell::Cell::new(0_u32);
    run_serialized_passes(&requested, &running, || {
        let seen = passes.get() + 1;
        passes.set(seen);
        if seen == 1 {
            // Another worker commits a state change and asks for a pass while
            // this one is mid-flight: it must not run concurrently, and it must
            // not be dropped either.
            requested.store(true, Ordering::Release);
        }
    });
    assert_eq!(passes.get(), 2);
    assert!(!running.load(Ordering::Acquire));
    assert!(!requested.load(Ordering::Acquire));

    // A caller that arrives while a pass owns the slot returns immediately and
    // leaves its request for the owner.
    running.store(true, Ordering::Release);
    let extra = std::cell::Cell::new(0_u32);
    run_serialized_passes(&requested, &running, || extra.set(extra.get() + 1));
    assert_eq!(extra.get(), 0);
    assert!(requested.load(Ordering::Acquire));
}

#[test]
fn abandoned_caller_replies_are_counted_apart_from_real_failures() {
    let abandoned_before = ABANDONED_REPLY_DIAGNOSTICS.total();
    let failures_before = REPLY_FAILURE_DIAGNOSTICS.total();
    note_reply_failure(i64::from(libc::EINVAL));
    assert_eq!(ABANDONED_REPLY_DIAGNOSTICS.total(), abandoned_before + 1);
    assert_eq!(REPLY_FAILURE_DIAGNOSTICS.total(), failures_before);
    note_reply_failure(i64::from(libc::ENOMEM));
    assert_eq!(REPLY_FAILURE_DIAGNOSTICS.total(), failures_before + 1);
}

#[test]
fn oversize_reply_is_not_mistaken_for_an_abandoned_caller() {
    let response = NetdIpcResponse {
        payload_len: u32::MAX,
        ..NetdIpcResponse::default()
    };
    assert_eq!(
        reply_netd_response(1, &response),
        -i64::from(libc::EMSGSIZE)
    );
}

#[test]
fn unix_socket_table_ceiling_rejects_overflow_without_wrapping() {
    let mut state = NetState::new();
    assert!(state.unix_capacity_available(MAX_UNIX_SOCKETS));
    assert!(!state.unix_capacity_available(MAX_UNIX_SOCKETS + 1));
    assert!(!state.unix_capacity_available(usize::MAX));
    for token in 1..=MAX_UNIX_SOCKETS as u64 {
        state
            .sockets
            .insert(token, super::local_socket_poll_tests::connected_socket(0));
    }
    assert!(!state.unix_capacity_available(1));
    state.sockets.remove(&1);
    assert!(state.unix_capacity_available(1));
    assert!(!state.unix_capacity_available(2));
}

#[test]
fn ref_ack_rejects_an_operand_that_only_aliases_an_op_code_in_its_low_bits() {
    let ack = NetdIpcRequest {
        op: NETD_IPC_OP_REF_ACK,
        socket_token: 5,
        operation_hi: 0xa11a5,
        operation_lo: 0x77,
        arg0: (1_u64 << 32) | u64::from(SYSCALL_OFFLOAD_OP_LINUX_CLOSE),
        ..NetdIpcRequest::default()
    };
    assert_eq!(acknowledge_ref_result(&ack), libc::EINVAL);
    let exact = NetdIpcRequest {
        arg0: u64::from(SYSCALL_OFFLOAD_OP_LINUX_CLOSE),
        ..ack
    };
    // Unknown operation id: nothing to acknowledge, idempotent success.
    assert_eq!(acknowledge_ref_result(&exact), 0);
}

fn linked_pair(state: &mut NetState) {
    let connected = super::local_socket_poll_tests::connected_socket;
    state.sockets.insert(1, connected(2));
    state.sockets.insert(2, connected(1));
}

fn peer_connected(state: &mut NetState) -> &mut ConnectedState {
    match &mut state.sockets.get_mut(&2).unwrap().state {
        UnixSocketState::Connected(connected) => connected,
        _ => unreachable!(),
    }
}

#[test]
fn near_empty_plain_stream_send_never_reports_would_block() {
    // EAGAIN on an AF_UNIX send is a capacity verdict, never a latency artefact:
    // data-only sends fail only when the peer queue is full, regardless of how
    // much ancillary custody other sockets of the same listener hold.
    let mut state = NetState::new();
    linked_pair(&mut state);
    let request = NetdIpcRequest {
        socket_token: 1,
        ..NetdIpcRequest::default()
    };
    peer_connected(&mut state).pending_listener = Some(99);
    state.sockets.insert(3, {
        let mut other = super::local_socket_poll_tests::connected_socket(0);
        if let UnixSocketState::Connected(connected) = &mut other.state {
            connected.pending_listener = Some(99);
            connected.incoming_control_bytes = SOCKET_CONTROL_BUFFER_CAPACITY;
        }
        other
    });
    assert_eq!(
        send_socket_message_in_state(&mut state, &request, b"x", &[]),
        Ok(1)
    );
    peer_connected(&mut state).incoming_bytes = SOCKET_BUFFER_CAPACITY - 1;
    assert_eq!(
        send_socket_message_in_state(&mut state, &request, b"xy", &[]),
        Ok(1)
    );
    assert_eq!(
        send_socket_message_in_state(&mut state, &request, b"z", &[]),
        Err(libc::EAGAIN)
    );
}

#[test]
fn ancillary_send_is_refused_only_by_the_listener_wide_custody_budget() {
    let mut state = NetState::new();
    linked_pair(&mut state);
    let request = NetdIpcRequest {
        socket_token: 1,
        ..NetdIpcRequest::default()
    };
    let control = [0_u8; 16];
    peer_connected(&mut state).pending_listener = Some(99);
    assert_eq!(
        send_socket_message_in_state(&mut state, &request, b"x", &control),
        Ok(1)
    );
    // A sibling accepted-but-unsettled socket exhausts the shared budget.
    state.sockets.insert(3, {
        let mut other = super::local_socket_poll_tests::connected_socket(0);
        if let UnixSocketState::Connected(connected) = &mut other.state {
            connected.pending_listener = Some(99);
            connected.incoming_control_bytes = SOCKET_CONTROL_BUFFER_CAPACITY - 16;
        }
        other
    });
    assert_eq!(
        send_socket_message_in_state(&mut state, &request, b"x", &control),
        Err(libc::EAGAIN)
    );
    // Once the listener's accept publishes, the charge no longer applies.
    peer_connected(&mut state).pending_listener = None;
    assert_eq!(
        send_socket_message_in_state(&mut state, &request, b"x", &control),
        Ok(1)
    );
}

#[test]
fn unix_global_budget_bounds_unrelated_sockets_and_releases_payload_and_control() {
    let mut state = NetState::new();
    // Fill one shared budget through distinct queues, not a per-socket limit.
    let bytes = vec![7; SOCKET_BUFFER_CAPACITY];
    for pair in 0..UNIX_QUEUED_BYTE_CAPACITY / SOCKET_BUFFER_CAPACITY {
        let sender = pair as u64 * 2 + 1;
        let receiver = sender + 1;
        state.sockets.insert(
            sender,
            super::local_socket_poll_tests::connected_socket(receiver),
        );
        state.sockets.insert(
            receiver,
            super::local_socket_poll_tests::connected_socket(sender),
        );
        let request = NetdIpcRequest {
            socket_token: sender,
            ..NetdIpcRequest::default()
        };
        assert_eq!(
            send_socket_message_in_state(&mut state, &request, &bytes, &[]),
            Ok(bytes.len())
        );
    }
    linked_extra_pair(&mut state, 1001, 1002);
    linked_extra_pair(&mut state, 1003, 1004);
    state.record_readiness_changes();
    state.readiness_publications.clear();
    let send = NetdIpcRequest {
        socket_token: 1001,
        ..NetdIpcRequest::default()
    };
    assert_eq!(state.unix_queued_bytes, UNIX_QUEUED_BYTE_CAPACITY);
    assert_eq!(
        send_socket_message_in_state(&mut state, &send, b"x", &[]),
        Err(libc::EAGAIN)
    );
    assert_eq!(
        unix_socket_revents(&state, 1003, linux_abi::POLLOUT as u32),
        Ok(0)
    );
    let recv = NetdIpcRequest {
        socket_token: 2,
        ..NetdIpcRequest::default()
    };
    recv_socket_bytes_in_state(&mut state, &recv, &mut [0; 32], false).unwrap();
    state.record_readiness_changes();
    assert!(
        state
            .readiness_publications
            .iter()
            .any(|entry| entry.0 == 1003),
        "freeing capacity must wake senders unrelated to the receiving pair"
    );
    assert_eq!(
        unix_socket_revents(&state, 1003, linux_abi::POLLOUT as u32),
        Ok(linux_abi::POLLOUT as u32)
    );
    let before = state.unix_queued_bytes;
    assert_eq!(
        send_socket_message_in_state(&mut state, &send, &[0; 25], &[0; 8]),
        Err(libc::EAGAIN)
    );
    assert_eq!(
        state.unix_queued_bytes, before,
        "ancillary messages are atomic under global pressure"
    );
    assert_eq!(
        send_socket_message_in_state(&mut state, &send, b"12345678", &[0; 24]),
        Ok(8)
    );
    assert_eq!(state.unix_queued_bytes, UNIX_QUEUED_BYTE_CAPACITY);
    let recv = NetdIpcRequest {
        socket_token: 1002,
        ..NetdIpcRequest::default()
    };
    let result = recv_socket_bytes_in_state(&mut state, &recv, &mut [0; 4], false).unwrap();
    assert_eq!(result.discarded.len(), 24);
    assert_eq!(state.unix_queued_bytes, UNIX_QUEUED_BYTE_CAPACITY - 28);
    retire_unix_socket(&mut state, 1002);
    assert_eq!(state.unix_queued_bytes, UNIX_QUEUED_BYTE_CAPACITY - 32);
    for token in state.sockets.keys().copied().collect::<Vec<_>>() {
        retire_unix_socket(&mut state, token);
    }
    assert_eq!(state.unix_queued_bytes, 0);
}

fn linked_extra_pair(state: &mut NetState, left: u64, right: u64) {
    state.sockets.insert(
        left,
        super::local_socket_poll_tests::connected_socket(right),
    );
    state.sockets.insert(
        right,
        super::local_socket_poll_tests::connected_socket(left),
    );
}
