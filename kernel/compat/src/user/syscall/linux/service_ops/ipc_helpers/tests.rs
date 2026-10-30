use super::*;

#[test]
fn netd_failed_accept_settles_cleanup_before_returning_the_error() {
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
fn netd_accept_cleanup_is_allowed_only_for_a_complete_error_trailer() {
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
    assert!(decode(&response).is_ok());
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
fn only_inet_stream_socket_creation_requires_a_prepared_reply_entry() {
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
        !netd_reply_uses_prepared_socket(&request),
        "AF_UNIX still returns the fd installed by its legacy broker transaction"
    );
    request.arg0 = linux_abi::AF_INET;
    request.arg1 = linux_abi::SOCK_DGRAM;
    assert!(!netd_reply_uses_prepared_socket(&request));
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
