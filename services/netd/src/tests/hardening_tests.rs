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
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
    ] {
        assert!(is_accounting_request(&request_for(op, linux_abi::AF_INET)));
    }
    for op in [
        SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
        SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
        SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
    ] {
        assert!(!is_accounting_request(&request_for(op, linux_abi::AF_INET)));
    }
}

#[test]
fn lifecycle_pool_has_its_own_bound_and_cannot_borrow_blocking_slots() {
    // A pool with no workers refuses instead of queueing a request nobody runs.
    let request = NetdIpcRequest {
        deadline_ns: u64::MAX,
        ..request_for(SYSCALL_OFFLOAD_OP_LINUX_SOCKET, linux_abi::AF_INET)
    };
    let queue: &'static BlockingRequestQueue = Box::leak(Box::new(BlockingRequestQueue::new()));
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
    // The same client is held to half the pool; a second one fills the rest.
    assert_eq!(
        enqueue_to_pool(queue, &pending, 2, &active, request, 2, 1),
        Err(libc::EAGAIN)
    );
    let other = NetdIpcRequest { pid: 2, ..request };
    assert_eq!(
        enqueue_to_pool(queue, &pending, 2, &active, other, 2, 1),
        Ok(())
    );
    let third = NetdIpcRequest { pid: 3, ..request };
    assert_eq!(
        enqueue_to_pool(queue, &pending, 2, &active, third, 3, 1),
        Err(libc::EAGAIN)
    );
    assert_eq!(queue.requests.lock().unwrap().len(), 2);
    // The shared blocking-I/O counters were never touched.
    assert_eq!(PENDING_LIFECYCLE_REQUESTS.load(Ordering::Acquire), 0);
}

#[test]
fn one_client_cannot_take_more_than_its_share_of_a_pool() {
    let queue: &'static BlockingRequestQueue = Box::leak(Box::new(BlockingRequestQueue::new()));
    let pending = AtomicUsize::new(0);
    let active = AtomicUsize::new(1);
    let from = |pid: u64| NetdIpcRequest {
        pid,
        deadline_ns: u64::MAX,
        ..request_for(SYSCALL_OFFLOAD_OP_LINUX_RECVFROM, linux_abi::AF_INET)
    };
    // Pool of 4: a client holds at most per_client_limit(4) == 2 of them.
    assert_eq!(per_client_limit(4), 2);
    for cap in 1..=2 {
        assert_eq!(enqueue_to_pool(queue, &pending, 4, &active, from(10), cap, 1), Ok(()));
    }
    assert_eq!(
        enqueue_to_pool(queue, &pending, 4, &active, from(10), 3, 1),
        Err(libc::EAGAIN)
    );
    // The refusal reserved nothing: another client still has the rest.
    assert_eq!(pending.load(Ordering::Acquire), 2);
    assert_eq!(queue.clients.held_by(10), 2);
    for cap in 3..=4 {
        assert_eq!(enqueue_to_pool(queue, &pending, 4, &active, from(11), cap, 1), Ok(()));
    }
    // A full pool refuses a third client without charging it.
    assert_eq!(
        enqueue_to_pool(queue, &pending, 4, &active, from(12), 5, 1),
        Err(libc::EAGAIN)
    );
    assert_eq!(queue.clients.held_by(12), 0);
    // Completion returns the share.
    release_pending_slot(&pending);
    queue.clients.release(10);
    assert_eq!(queue.clients.held_by(10), 1);
    assert_eq!(enqueue_to_pool(queue, &pending, 4, &active, from(10), 6, 1), Ok(()));
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
    let mut log: HashMap<(u64, u64), RefReplayEntry> = [
        // Old and acknowledged-by-nobody: reclaimable.
        replay_entry(1, true, base),
        // Old but still executing: its request owns the slot.
        replay_entry(2, false, base),
        // Recent completed result: a retry may still need it.
        replay_entry(3, true, later - Duration::from_secs(1)),
    ]
    .into_iter()
    .map(|entry| ((entry.operation_hi, entry.operation_lo), entry))
    .collect();
    reclaim_stale_ref_results(&mut log, later);
    let mut kept: Vec<u64> = log.values().map(|entry| entry.operation_lo).collect();
    kept.sort_unstable();
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
fn netlink_socket_table_ceiling_rejects_overflow() {
    let mut state = NetState::new();
    assert!(state.netlink_capacity_available());
    for token in 1..=MAX_NETLINK_SOCKETS as u64 {
        state
            .netlink_sockets
            .insert(token, super::netlink_tests::netlink_socket(libc::SOCK_RAW as u64));
    }
    assert!(!state.netlink_capacity_available());
    state.netlink_sockets.remove(&1);
    assert!(state.netlink_capacity_available());
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
