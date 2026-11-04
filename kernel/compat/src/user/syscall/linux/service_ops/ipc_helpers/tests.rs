use super::*;

#[test]
fn netd_failed_mutation_settles_cleanup_before_returning_the_error() {
    let response = NetdIpcResponse {
        status: LINUX_EAGAIN as i32,
        payload_len: 16,
        reserved0: 16,
        ..NetdIpcResponse::default()
    };
    let mut releases = 0;
    let result = finish_netd_ipc_response(response, |decoded| {
        releases += 1;
        assert_eq!(decoded.reserved0, 16);
        decoded.reserved0 = 0;
        decoded.payload_len = 0;
        Ok(())
    });
    assert!(matches!(result, Err(LINUX_EAGAIN)));
    assert_eq!(releases, 1);
}

#[test]
fn netd_accept_never_relies_on_an_error_reply_for_ancillary_release() {
    let request = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
        ..NetdIpcRequest::default()
    };
    let mut response = NetdIpcResponse {
        version: NETD_IPC_ABI_VERSION,
        op: request.op,
        status: LINUX_EAGAIN as i32,
        payload_len: 16,
        reserved0: 16,
        ..NetdIpcResponse::default()
    };
    let decode = |response: &NetdIpcResponse| {
        // Same fixed wire envelope as the service, with no handle dereference.
        let bytes = unsafe { core::slice::from_raw_parts(
            (response as *const NetdIpcResponse).cast::<u8>(),
            NETD_IPC_RESPONSE_HEADER_SIZE + response.payload_len as usize,
        ) };
        decode_netd_ipc_response(&request, bytes)
    };
    assert!(matches!(decode(&response), Err(LINUX_EINVAL)));
    response.status = 0;
    assert!(matches!(decode(&response), Err(LINUX_EINVAL)));
    response.status = LINUX_EAGAIN as i32;
    response.reserved0 = 8;
    assert!(matches!(decode(&response), Err(LINUX_EINVAL)));
    response.reserved0 = 17;
    assert!(matches!(decode(&response), Err(LINUX_EINVAL)));
}

#[test]
fn netd_invalid_cleanup_cannot_be_hidden_by_an_operation_error() {
    let response = NetdIpcResponse {
        status: LINUX_EAGAIN as i32,
        ..NetdIpcResponse::default()
    };
    assert!(matches!(finish_netd_ipc_response(response, |_| Err(LINUX_EINVAL)), Err(LINUX_EINVAL)));
}

#[test]
fn direct_bootstrap_requires_typed_job_authority_before_loading_and_keeps_the_generation() {
    // Source wiring witness, not production user-copy or boot fault injection.
    let source = include_str!("bootstrap_spawn.rs");
    let shape = source.find("bootstrap_spawn_shape_valid(&args)").unwrap();
    let authority = source.find("resolve_spawn_job_descriptor(requester_pid, args.job_handle)").unwrap();
    let scheduling = source.find("consume_direct_bootstrap_scheduling_context(").unwrap();
    let spawn = source.find("match spawn_bootstrap_exec_direct(").unwrap();
    assert!(shape < authority && authority < scheduling && scheduling < spawn);
    assert!(source.contains(".with_job(job)"));
    let console = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/user/console_host.rs"));
    let call = console.split_once("let spawned = process::spawn_bootstrap_linux_process_with_launch_and_scheduling_context(").unwrap().1;
    assert!(call.split_once(");").unwrap().0.contains("program.job"));
    let process = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/user/process/mod.rs"));
    let construct = process.split_once("pub fn spawn_bootstrap_linux_process_with_launch_and_scheduling_context(").unwrap().1;
    assert!(construct.find("reserve_process_spawn_transaction_in(job)?").unwrap()
        < construct.find("prepare_bootstrap_linux_process_with_launch(image, launch)?").unwrap());
}

#[test]
fn direct_bootstrap_consumes_exact_rootd_scheduling_authority_before_spawn() {
    let source = include_str!("bootstrap_spawn.rs");
    let direct = source
        .split_once("pub fn syscall_linux_loader_spawn_exec(")
        .expect("direct bootstrap syscall")
        .1;
    let consume = direct
        .find("consume_direct_bootstrap_scheduling_context")
        .expect("one-shot scheduling authority consumption");
    let spawn = direct
        .find("spawn_bootstrap_exec_direct(")
        .expect("direct bootstrap spawn");
    assert!(consume < spawn);
    assert!(direct.contains(".with_scheduling_context(scheduling_policy)"));
}

#[test]
fn empty_nonblocking_console_read_returns_eagain_without_retry() {
    assert_eq!(empty_console_read_result(true, 0), Some(Err(LINUX_EAGAIN)));
    assert_eq!(empty_console_read_result(false, 0), None);
    assert_eq!(empty_console_read_result(true, 7), Some(Ok(7)));
}

#[test]
fn vfs_response_envelope_rejects_oversized_payload_before_slice_use() {
    let mut response = VfsIpcResponse {
        version: VFS_IPC_ABI_VERSION,
        op: VFS_IPC_OP_OPENAT,
        ..VfsIpcResponse::default()
    };
    assert_eq!(
        validate_vfs_response_envelope(VFS_IPC_OP_OPENAT, &response),
        Ok(())
    );

    response.payload_len = response.payload.len() as u32 + 1;
    assert_eq!(
        validate_vfs_response_envelope(VFS_IPC_OP_OPENAT, &response),
        Err(LINUX_EINVAL)
    );
}

#[test]
fn only_tombstoning_vfs_mutations_require_visibility_ack() {
    let close = VfsIpcRequest {
        op: VFS_IPC_OP_CLOSE,
        ..VfsIpcRequest::default()
    };
    assert!(vfs_checkpoint_ack_required(&close));

    let mut poll = VfsIpcRequest {
        op: VFS_IPC_OP_POLL_QUERY,
        arg0: VFS_POLL_QUERY_EPOLL_CTL,
        arg1: linux_abi::EPOLL_CTL_ADD,
        ..VfsIpcRequest::default()
    };
    assert!(!vfs_checkpoint_ack_required(&poll));
    poll.arg1 = linux_abi::EPOLL_CTL_DEL;
    assert!(vfs_checkpoint_ack_required(&poll));
    poll.arg0 = VFS_POLL_QUERY_EPOLL_RETIRE;
    assert!(vfs_checkpoint_ack_required(&poll));
    poll.arg0 = VFS_POLL_QUERY_EPOLL_PURGE_OBJECT;
    assert!(vfs_checkpoint_ack_required(&poll));

    let read = VfsIpcRequest {
        op: VFS_IPC_OP_READ,
        ..VfsIpcRequest::default()
    };
    assert!(!vfs_checkpoint_ack_required(&read));
}

#[test]
fn epoll_snapshot_reads_are_retry_safe() {
    let mut request = VfsIpcRequest {
        op: VFS_IPC_OP_POLL_QUERY,
        ..VfsIpcRequest::default()
    };
    request.arg0 = VFS_POLL_QUERY_EPOLL_SNAPSHOT;
    assert!(vfs_request_is_replay_safe(&request));

    request.arg0 = u64::MAX;
    assert!(!vfs_request_is_replay_safe(&request));
}

#[test]
fn housekeeping_vfs_maintenance_is_one_bounded_replay_turn() {
    assert_eq!(HOUSEKEEPING_VFS_MAINTENANCE_ATTEMPTS, 1);
    assert_eq!(
        rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
        100
    );
}

#[test]
fn vfs_cursor_and_checkpoint_mutations_are_control_not_readiness_work() {
    for op in [VFS_IPC_OP_CLOSE, VFS_IPC_OP_CURSOR_SETTLE, VFS_IPC_OP_CHECKPOINT_ACK] {
        let request = VfsIpcRequest { op, ..VfsIpcRequest::default() };
        assert!(matches!(vfs_request_service_class(&request), ipc_ops::ServiceIpcClass::StateMutation));
    }
    for arg0 in [VFS_POLL_QUERY_EPOLL_CREATE, VFS_POLL_QUERY_EPOLL_CTL, VFS_POLL_QUERY_EPOLL_RETIRE, VFS_POLL_QUERY_EPOLL_PURGE_OBJECT] {
        let request = VfsIpcRequest { op: VFS_IPC_OP_POLL_QUERY, arg0, ..VfsIpcRequest::default() };
        assert!(matches!(vfs_request_service_class(&request), ipc_ops::ServiceIpcClass::StateMutation));
    }
    let read = VfsIpcRequest { op: VFS_IPC_OP_READ, ..VfsIpcRequest::default() };
    assert!(matches!(vfs_request_service_class(&read), ipc_ops::ServiceIpcClass::BulkData));
}

#[test]
fn a_prepared_cursor_cannot_orphan_a_rejected_final_close() {
    let mut request = VfsIpcRequest { op: VFS_IPC_OP_CLOSE, ..VfsIpcRequest::default() };
    for status in [LINUX_EBUSY as i32, -(LINUX_EBUSY as i32)] {
        let response = VfsIpcResponse { status, ..VfsIpcResponse::default() };
        assert!(vfs_response_needs_reconciliation(&request, &response));
    }
    for status in [0, LINUX_EBADF as i32, LINUX_EINVAL as i32] {
        let response = VfsIpcResponse { status, ..VfsIpcResponse::default() };
        assert!(!vfs_response_needs_reconciliation(&request, &response));
    }
    request.op = VFS_IPC_OP_READ;
    let response = VfsIpcResponse { status: LINUX_EBUSY as i32, ..VfsIpcResponse::default() };
    assert!(!vfs_response_needs_reconciliation(&request, &response));
}

#[test]
fn deferred_vfs_replay_preserves_all_operation_arguments_and_payload() {
    let mut original = VfsIpcRequest {
        op: VFS_IPC_OP_CHECKPOINT_ACK, fd: 8, remote_id: 0xfeed, flags: 3, dirfd: 9,
        arg0: 11, arg1: 12, arg2: u64::from(VFS_IPC_OP_POLL_QUERY),
        arg3: VFS_POLL_QUERY_EPOLL_RETIRE,
        operation_hi: 13, operation_lo: 14, payload_len: 4,
        ..VfsIpcRequest::default()
    };
    original.payload[..4].copy_from_slice(b"wire");
    let pending = PendingVfsMutation::capture(&original).unwrap();
    let mut replay = VfsIpcRequest::default();
    pending.restore_into(&mut replay);
    assert_eq!((replay.op, replay.fd, replay.remote_id, replay.flags, replay.dirfd),
        (original.op, original.fd, original.remote_id, original.flags, original.dirfd));
    assert_eq!((replay.arg0, replay.arg1, replay.arg2, replay.arg3),
        (original.arg0, original.arg1, original.arg2, original.arg3));
    assert_eq!((replay.operation_hi, replay.operation_lo), (13, 14));
    assert_eq!(replay.payload_len, 4);
    assert_eq!(&replay.payload[..4], b"wire");
    original.path_len = 1;
    assert!(matches!(PendingVfsMutation::capture(&original), Err(LINUX_EINVAL)));
}

#[test]
fn failed_vfs_replay_retains_its_owner_without_starving_unrelated_work() {
    let mutation = |remote_id| PendingVfsMutation::capture(&VfsIpcRequest {
        remote_id, ..VfsIpcRequest::default()
    }).unwrap();
    let mut queue = PendingVfsMutationQueue::<3>::new();
    assert!(queue.push(mutation(1)));
    assert!(queue.push(mutation(2)));
    let in_flight = queue.pop(None).unwrap();
    // Foreground fills the ordinary quota while the old owner is in IPC.
    assert!(queue.push(mutation(3)));
    retry_vfs_mutation_later(&mut queue, in_flight);
    for expected in [2, 3, 1] {
        let next = queue.pop(None).unwrap();
        assert_eq!(next.remote_id, expected);
        queue.finish(next, None);
    }
    assert!(queue.pop(None).is_none());
}

#[test]
fn lost_cursor_settlement_reply_cannot_be_overtaken_by_its_queued_close() {
    let mutation = |op, remote_id, operation_lo| PendingVfsMutation::capture(&VfsIpcRequest {
        op, remote_id, operation_lo, ..VfsIpcRequest::default()
    }).unwrap();
    let mut queue = PendingVfsMutationQueue::<4>::new();
    assert!(queue.push(mutation(VFS_IPC_OP_CURSOR_SETTLE, 1, 10)));
    let settlement = queue.pop(None).unwrap();
    // Provider committed, but its reply was lost. A racing foreground close
    // must see the still-owned in-flight lane and become a blocked follower.
    assert!(queue.any(|entry| entry.operation_lo == 10));
    assert!(queue.push(mutation(VFS_IPC_OP_CLOSE, 1, 20)));
    assert!(queue.push(mutation(VFS_IPC_OP_CHECKPOINT_ACK, 2, 30)));
    retry_vfs_mutation_later(&mut queue, settlement);
    let unrelated = queue.pop(None).unwrap();
    assert_eq!(unrelated.remote_id, 2);
    queue.finish(unrelated, None);
    let replay = queue.pop(None).unwrap();
    assert_eq!(replay.op, VFS_IPC_OP_CURSOR_SETTLE);
    assert_eq!(replay.operation_lo, 10);
    queue.finish(replay, None);
    let close = queue.pop(None).unwrap();
    assert_eq!(close.op, VFS_IPC_OP_CLOSE);
    queue.finish(close, None);
    assert!(queue.pop(None).is_none());
}

#[test]
fn multiple_failed_vfs_lanes_cannot_starve_an_unrelated_third_lane() {
    let mut queue = PendingVfsMutationQueue::<5>::new();
    for remote_id in [1, 1, 2, 2, 3] {
        assert!(queue.push(PendingVfsMutation::capture(&VfsIpcRequest { remote_id, ..VfsIpcRequest::default() }).unwrap()));
    }
    for expected in [1, 2, 3, 1, 2, 3] {
        let next = queue.pop(None).unwrap();
        assert_eq!(next.remote_id, expected);
        retry_vfs_mutation_later(&mut queue, next);
    }
}

#[test]
fn zero_remote_id_purges_use_provider_object_lanes_and_their_acks_keep_that_lane() {
    let purge = |provider, object| VfsIpcRequest {
        op: VFS_IPC_OP_POLL_QUERY, arg0: VFS_POLL_QUERY_EPOLL_PURGE_OBJECT,
        arg1: provider, arg2: object, operation_hi: provider, operation_lo: object,
        ..VfsIpcRequest::default()
    };
    let first = PendingVfsMutation::capture(&purge(u64::from(WAITSET_PROVIDER_NETD), 101)).unwrap();
    let second = PendingVfsMutation::capture(&purge(u64::from(WAITSET_PROVIDER_NETD), 102)).unwrap();
    let third = PendingVfsMutation::capture(&purge(u64::from(WAITSET_PROVIDER_INPUTD), 101)).unwrap();
    assert_ne!(first.lane, second.lane);
    assert_ne!(first.lane, third.lane);
    let mut queue = PendingVfsMutationQueue::<4>::new();
    assert!(queue.push(first));
    assert!(queue.push(second));
    assert!(queue.push(third));
    let failed = queue.pop(None).unwrap();
    retry_vfs_mutation_later(&mut queue, failed);
    let successful = queue.pop(None).unwrap();
    assert_eq!(successful.lane, second.lane);
    let ack = VfsIpcRequest {
        op: VFS_IPC_OP_CHECKPOINT_ACK, arg0: successful.operation_hi, arg1: successful.operation_lo,
        arg2: u64::from(VFS_IPC_OP_POLL_QUERY), arg3: VFS_POLL_QUERY_EPOLL_PURGE_OBJECT,
        ..VfsIpcRequest::default()
    };
    // The wire ACK does not carry the original target. It cannot invent a
    // zero-object lane; foreground and background owners inherit the target.
    assert!(matches!(PendingVfsMutation::capture(&ack), Err(LINUX_EINVAL)));
    let mut original = VfsIpcRequest::default();
    successful.restore_into(&mut original);
    let pending_ack = PendingVfsMutation::capture_ack(&original, &ack).unwrap();
    let mut conflicting = ack;
    conflicting.arg1 ^= 1;
    assert!(matches!(PendingVfsMutation::capture_ack(&original, &conflicting), Err(LINUX_EINVAL)));
    queue.finish(successful, Some(pending_ack));
    let next = queue.pop(None).unwrap();
    assert_eq!(next.lane, third.lane);
    queue.finish(next, None);
    let retry = queue.pop(None).unwrap();
    assert_eq!(retry.lane, first.lane);
    retry_vfs_mutation_later(&mut queue, retry);
    let visibility = queue.pop(None).unwrap();
    assert_eq!(visibility.op, VFS_IPC_OP_CHECKPOINT_ACK);
    assert_eq!(visibility.lane, second.lane);
    queue.finish(visibility, None);
    let vfs_target = PendingVfsMutation::capture(&purge(u64::from(WAITSET_PROVIDER_VFSD), 101)).unwrap();
    assert_eq!(vfs_target.lane, VfsMutationLane::Description(101));
}

#[test]
fn only_supported_inet_socket_creation_requires_a_prepared_reply_entry() {
    let mut request = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op: SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
        arg0: linux_abi::AF_INET,
        arg1: linux_abi::SOCK_STREAM | linux_abi::SOCK_NONBLOCK,
        ..NetdIpcRequest::default()
    };
    assert!(netd_reply_uses_prepared_socket(&request));

    request.arg0 = linux_abi::AF_UNIX;
    assert!(
        netd_reply_uses_prepared_socket(&request),
        "AF_UNIX must never install a descriptor before the timely reply take"
    );
    request.arg0 = linux_abi::AF_INET;
    request.arg1 = linux_abi::SOCK_DGRAM;
    assert!(netd_reply_uses_prepared_socket(&request));
    request.arg2 = 1;
    assert!(netd_reply_uses_prepared_socket(&request));
    request.arg2 = 0;
    request.op = SYSCALL_OFFLOAD_OP_LINUX_CLOSE;
    request.arg1 = linux_abi::SOCK_STREAM;
    assert!(!netd_reply_uses_prepared_socket(&request));
}

#[test]
fn timely_netd_socket_decode_installs_one_exact_entry_and_returns_that_fd() {
    let request = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op: SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
        arg0: linux_abi::AF_INET,
        arg1: linux_abi::SOCK_STREAM | linux_abi::SOCK_NONBLOCK,
        arg2: 6,
        ..NetdIpcRequest::default()
    };
    let token = 0x5eed_7101;
    let entry = multitask::TransferredHandleEntry::from_initial_entry(multitask::HandleEntry::new(
        multitask::KernelHandle::InetSocket(multitask::InetSocketHandle::from_token(
            token,
            linux_abi::AF_INET,
            linux_abi::SOCK_STREAM,
            6,
        )),
        0,
        linux_abi::SOCK_NONBLOCK,
    ))
    .expect("initial Inet socket entry is transferable");
    let decoded = NetdIpcResponse {
        version: NETD_IPC_ABI_VERSION,
        op: SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
        value: token,
        ..NetdIpcResponse::default()
    };
    let installs = core::cell::Cell::new(0_u8);

    let response = install_prepared_netd_socket_response(
        &request,
        decoded,
        alloc::vec![entry],
        |entries| panic!("valid reply unexpectedly discarded {} entry", entries.len()),
        || false,
        |entries| {
            installs.set(installs.get() + 1);
            assert!(prepared_netd_socket_response_matches(
                &request,
                &decoded,
                entries.as_slice()
            ));
            assert_eq!(entries.len(), 1);
            Ok(alloc::vec![37])
        },
    )
    .expect("timely exact prepared socket response");

    assert_eq!(installs.get(), 1);
    assert_eq!(response.value, 37);

    let expired_entry =
        multitask::TransferredHandleEntry::from_initial_entry(multitask::HandleEntry::new(
            multitask::KernelHandle::InetSocket(multitask::InetSocketHandle::from_token(
                token,
                linux_abi::AF_INET,
                linux_abi::SOCK_STREAM,
                6,
            )),
            0,
            linux_abi::SOCK_NONBLOCK,
        ))
        .expect("expired Inet socket entry is transferable");
    assert_eq!(
        install_prepared_netd_socket_response(
            &request,
            decoded,
            alloc::vec![expired_entry],
            |entries| {
                assert_eq!(entries.len(), 1);
                drop(entries);
            },
            || true,
            |_| {
                installs.set(installs.get() + 1);
                Ok(alloc::vec![38])
            },
        ),
        Err(LINUX_ETIMEDOUT)
    );
    assert_eq!(
        installs.get(),
        1,
        "expiry before publication must drop the entry without installing it"
    );

    let wrong_fd_flags =
        multitask::TransferredHandleEntry::from_initial_entry(multitask::HandleEntry::new(
            multitask::KernelHandle::InetSocket(multitask::InetSocketHandle::from_token(
                token,
                linux_abi::AF_INET,
                linux_abi::SOCK_STREAM,
                6,
            )),
            multitask::FD_CLOEXEC,
            linux_abi::SOCK_NONBLOCK,
        ))
        .expect("mismatched close-on-exec entry remains structurally transferable");
    assert!(
        !prepared_netd_socket_response_matches(&request, &decoded, &[wrong_fd_flags]),
        "the reply must not publish descriptor flags that differ from the request"
    );

    let wrong_status_flags =
        multitask::TransferredHandleEntry::from_initial_entry(multitask::HandleEntry::new(
            multitask::KernelHandle::InetSocket(multitask::InetSocketHandle::from_token(
                token,
                linux_abi::AF_INET,
                linux_abi::SOCK_STREAM,
                6,
            )),
            0,
            0,
        ))
        .expect("mismatched status-flags entry remains structurally transferable");
    assert!(
        !prepared_netd_socket_response_matches(&request, &decoded, &[wrong_status_flags]),
        "the reply must preserve the requested nonblocking open-description state"
    );
}

#[test]
fn procd_response_envelope_rejects_cross_op_and_oversized_payload() {
    let mut response = ProcdIpcResponse {
        op: PROCD_OP_SELECT_SIGNAL,
        ..ProcdIpcResponse::default()
    };
    assert_eq!(
        validate_procd_response_envelope(PROCD_OP_SELECT_SIGNAL, &response),
        Ok(())
    );
    assert_eq!(
        validate_procd_response_envelope(PROCD_OP_EXECVE, &response),
        Err(LINUX_EINVAL)
    );

    response.payload_len = response.payload.len() as u32 + 1;
    assert_eq!(
        validate_procd_response_envelope(PROCD_OP_SELECT_SIGNAL, &response),
        Err(LINUX_EINVAL)
    );
}

/// A reaped child leaves SIGCHLD pending with its event bits; procd echoes the
/// selected events in `reserved0`. Only SELECT_SIGNAL may carry them - rejecting
/// the reply made every later syscall of that process repeat a failing call.
#[test]
fn procd_select_signal_reply_may_carry_sigchld_events_but_no_other_op_may() {
    let response = ProcdIpcResponse {
        op: PROCD_OP_SELECT_SIGNAL,
        reserved0: 1,
        ..ProcdIpcResponse::default()
    };
    assert_eq!(
        validate_procd_response_envelope(PROCD_OP_SELECT_SIGNAL, &response),
        Ok(())
    );
    let other = ProcdIpcResponse {
        op: PROCD_OP_EXECVE,
        reserved0: 1,
        ..ProcdIpcResponse::default()
    };
    assert_eq!(
        validate_procd_response_envelope(PROCD_OP_EXECVE, &other),
        Err(LINUX_EINVAL)
    );
    let reserved1 = ProcdIpcResponse {
        op: PROCD_OP_SELECT_SIGNAL,
        reserved1: 1,
        ..ProcdIpcResponse::default()
    };
    assert_eq!(
        validate_procd_response_envelope(PROCD_OP_SELECT_SIGNAL, &reserved1),
        Err(LINUX_EINVAL)
    );
}

/// A memoized routing answer must be discarded the moment a different devmgrd
/// could be the one answering. The table is compiled into that service, so an
/// entry recorded under a superseded registration is a guess about a binary
/// that is no longer running.
#[test]
fn a_memoized_ioctl_route_never_outlives_the_registration_that_produced_it() {
    use super::{IOCTL_ROUTE_MEMO_CAPACITY, memoized_ioctl_route, record_ioctl_route};
    use rustos_user_abi::syscall::{
        DEVMGRD_IOCTL_ROUTE_DEVMGRD, DEVMGRD_IOCTL_ROUTE_DIRECT, DEVMGRD_IOCTL_ROUTE_SESSIOND_TTY,
    };

    let epoch = 7;
    let request = 0x4321_u64;
    assert_eq!(memoized_ioctl_route(request, epoch), None);

    record_ioctl_route(request, epoch, DEVMGRD_IOCTL_ROUTE_SESSIOND_TTY);
    assert_eq!(
        memoized_ioctl_route(request, epoch),
        Some(DEVMGRD_IOCTL_ROUTE_SESSIOND_TTY),
        "a repeat of the same question is answered without asking again"
    );

    // A newer registration invalidates every entry, not just the one asked for.
    assert_eq!(memoized_ioctl_route(request, epoch + 1), None);
    record_ioctl_route(request, epoch + 1, DEVMGRD_IOCTL_ROUTE_DEVMGRD);
    assert_eq!(
        memoized_ioctl_route(request, epoch + 1),
        Some(DEVMGRD_IOCTL_ROUTE_DEVMGRD),
        "the new registration's answer replaces the old one"
    );
    assert_eq!(
        memoized_ioctl_route(request, epoch),
        None,
        "and the superseded epoch can never read its own entry back"
    );

    // Past capacity the memo stops growing rather than evicting, so a caller
    // simply pays the query it paid before.
    for number in 0..IOCTL_ROUTE_MEMO_CAPACITY as u64 + 8 {
        record_ioctl_route(0x9000 + number, epoch + 1, DEVMGRD_IOCTL_ROUTE_DIRECT);
    }
    assert_eq!(
        memoized_ioctl_route(request, epoch + 1),
        Some(DEVMGRD_IOCTL_ROUTE_DEVMGRD),
        "overflow must not evict or corrupt what is already recorded"
    );
}

#[test]
fn cursor_gate_orders_foreground_users_after_unsettled_mutations_of_the_same_description() {
    let settle = |remote_id| PendingVfsMutation::capture(&VfsIpcRequest {
        op: VFS_IPC_OP_CURSOR_SETTLE, remote_id, operation_lo: 10, ..VfsIpcRequest::default()
    }).unwrap();
    let mut queue = PendingVfsMutationQueue::<4>::new();
    assert!(matches!(queue.try_claim_cursor(1), VfsCursorClaim::Claimed));
    // A second foreground user of the same description waits; others do not.
    assert!(matches!(queue.try_claim_cursor(1), VfsCursorClaim::Busy));
    assert!(matches!(queue.try_claim_cursor(2), VfsCursorClaim::Claimed));
    queue.release_cursor(1);
    // A timed-out COMMIT parked in the queue still blocks the next cursor
    // operation: vfsd would otherwise answer it with EBUSY.
    assert!(queue.push(settle(1)));
    assert!(matches!(queue.try_claim_cursor(1), VfsCursorClaim::Busy));
    // Replay ownership (in flight) counts as unsettled too.
    let in_flight = queue.pop(Some(VfsMutationLane::Description(1))).unwrap();
    assert!(matches!(queue.try_claim_cursor(1), VfsCursorClaim::Busy));
    // Only one replayer owns the queue; a second one backs off, not panics.
    assert!(queue.pop(None).is_none());
    queue.finish(in_flight, None);
    assert!(matches!(queue.try_claim_cursor(1), VfsCursorClaim::Claimed));
    queue.release_cursor(1);
    queue.release_cursor(2);
}

#[test]
fn cursor_gate_slots_are_bounded_and_reusable() {
    let mut queue = PendingVfsMutationQueue::<1>::new();
    for id in 1..=VFS_CURSOR_GATE_SLOTS as u64 {
        assert!(matches!(queue.try_claim_cursor(id), VfsCursorClaim::Claimed));
    }
    assert!(matches!(queue.try_claim_cursor(1_000), VfsCursorClaim::Busy));
    queue.release_cursor(7);
    assert!(matches!(queue.try_claim_cursor(1_000), VfsCursorClaim::Claimed));
}

#[test]
fn accepted_socket_flags_come_only_from_accept4_not_the_listener() {
    for listener_flags in [0, linux_abi::O_NONBLOCK] {
        for accepted_flags in [0, linux_abi::SOCK_NONBLOCK, linux_abi::SOCK_CLOEXEC,
            linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC] {
            let request = NetdIpcRequest {
                op: SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
                status_flags: listener_flags,
                arg3: accepted_flags,
                ..NetdIpcRequest::default()
            };
            let socket = multitask::SocketHandle::from_token(71, linux_abi::AF_UNIX, linux_abi::SOCK_STREAM, 0);
            assert!(socket.bind_channel(99, 2));
            let entry = multitask::TransferredHandleEntry::from_initial_entry(
                multitask::HandleEntry::new(multitask::KernelHandle::Socket(socket),
                    if accepted_flags & linux_abi::SOCK_CLOEXEC != 0 { multitask::FD_CLOEXEC } else { 0 },
                    accepted_flags & linux_abi::SOCK_NONBLOCK)
            ).unwrap();
            let response = NetdIpcResponse { op: request.op, value: 71, ..NetdIpcResponse::default() };
            assert!(prepared_netd_socket_response_matches(&request, &response, &[entry]),
                "accepted flags must be independent of listener flags");
        }
    }
}

#[test]
fn prepared_unix_socket_pair_and_accept_abandonment_never_install_unseen_fds() {
    fn entries(op:u16)->Vec<multitask::TransferredHandleEntry> {
        (0..if op==SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR {2} else {1}).map(|index| {
            let socket=multitask::SocketHandle::from_token(71+index,linux_abi::AF_UNIX,linux_abi::SOCK_STREAM,0);
            if op!=SYSCALL_OFFLOAD_OP_LINUX_SOCKET {
                assert!(socket.bind_channel(99,if op==SYSCALL_OFFLOAD_OP_LINUX_ACCEPT {2} else {index as u8+1}));
            }
            multitask::TransferredHandleEntry::from_initial_entry(multitask::HandleEntry::new(
                multitask::KernelHandle::Socket(socket),multitask::FD_CLOEXEC,linux_abi::SOCK_NONBLOCK)).unwrap()
        }).collect()
    }
    for op in [SYSCALL_OFFLOAD_OP_LINUX_SOCKET,SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,SYSCALL_OFFLOAD_OP_LINUX_ACCEPT] {
        let request=NetdIpcRequest {op,arg0:linux_abi::AF_UNIX,
            arg1:linux_abi::SOCK_STREAM | linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC,
            arg3:linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC,
            ..NetdIpcRequest::default()};
        let mut response=NetdIpcResponse{op,value:71,..NetdIpcResponse::default()};
        let count=if op==SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR {2} else {1};
        if count==2 {
            response.payload[..8].copy_from_slice(&72_u64.to_ne_bytes());
            response.payload_len=8;
        }
        assert!(netd_reply_uses_prepared_socket(&request));
        assert!(prepared_netd_socket_response_matches(&request,&response,&entries(op)));
        let discarded=core::cell::Cell::new(0);
        let installed=core::cell::Cell::new(0);
        assert_eq!(install_prepared_netd_socket_response(&request,response,entries(op),|batch| {
            discarded.set(discarded.get()+batch.len());
        },||true,|_| {installed.set(installed.get()+count);Ok(alloc::vec![37;count])}),Err(LINUX_ETIMEDOUT));
        assert_eq!(installed.get(),0);
        assert_eq!(discarded.get(),count,"every initial reference must reach deferred close exactly once");
        // User-copy/slot-commit failure owns and releases the whole claimed batch.
        assert_eq!(install_prepared_netd_socket_response(&request,response,entries(op),|_|panic!("valid batch"),
            ||false,|batch| {discarded.set(discarded.get()+batch.len());Err(LINUX_EFAULT)}),Err(LINUX_EFAULT));
        assert_eq!(discarded.get(),2*count);
        assert_eq!(installed.get(),0);
        let result=install_prepared_netd_socket_response(&request,response,entries(op),|_|panic!("valid batch"),
            ||false,|batch| {installed.set(batch.len());Ok((37..37+count as i32).collect())}).unwrap();
        assert_eq!(installed.get(),count);
        assert_eq!(result.value,if count==2 {0} else {37});
        assert_eq!(result.payload_len,0);
    }
}
