use core::mem::size_of;

use super::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, IPC_ABI_VERSION,
    IPC_MAX_INLINE_BYTES, IPC_SERVICE_DEVMGRD, IPC_SERVICE_INITD, IPC_SERVICE_PROCD,
    IPC_SERVICE_ROOTD, IPC_SERVICE_SESSIOND, LINUX_RLIMIT_SIZE, LINUX_SIGACTION_SIZE,
    LINUX_STATX_SIZE, LINUX_TIMESPEC_SIZE, LINUX_UTSNAME_SIZE, LOADER_OP_ACTIVATE,
    LOADER_OP_EXEC_TARGET, LOADER_OP_SPAWN_EXEC, LinuxRlimit, LinuxSigActionWire,
    LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, LinuxTimespecWire, LinuxUtsName,
    LoaderSpawnRequest, NET_BROKER_PUBLICATION_AF_INET,
    NET_BROKER_PUBLICATION_ALLOWED_SOCKET_FLAGS, NET_BROKER_PUBLICATION_SOCK_CLOEXEC,
    NET_BROKER_PUBLICATION_SOCK_NONBLOCK, NET_BROKER_PUBLICATION_SOCK_STREAM,
    NET_BROKER_SOCKET_PUBLICATION_VERSION, NETD_IPC_ABI_VERSION, NETD_IPC_PAYLOAD_CAPACITY,
    NETD_IPC_REQUEST_HEADER_SIZE, NETD_IPC_RESPONSE_HEADER_SIZE, NetBrokerPrepareSocketPublication,
    NetdIpcRequest, NetdIpcResponse, PROCD_SIGACTION_SA_NOCLDSTOP, PROCD_SIGCHLD_EVENT_EXIT,
    PROCD_SIGCHLD_EVENT_MASK, PRODUCT_EXECUTABLE_SNAPSHOT_BACKING_DVM_VOLUME,
    PRODUCT_EXECUTABLE_SNAPSHOT_EVIDENCE_ABI_VERSION, ProductExecutableSnapshotEvidence,
    RustosIpcValidateServiceOwnerArgs, STORAGED_BULK_READ_PAYLOAD_CAPACITY,
    STORAGED_BULK_READ_RESPONSE_HEADER_BYTES, SYSCALL_OFFLOAD_ABI_VERSION,
    SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY, SYSCALL_OFFLOAD_OP_LINUX_MPROTECT,
    SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_STATX,
    SYSCALL_OFFLOAD_PATH_CAPACITY, SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, StoragedBulkReadResponse,
    VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION, VFS_EXECUTABLE_SNAPSHOT_OP_OPEN, VFS_IPC_ABI_VERSION,
    VFS_IPC_OP_OPENAT, VFS_IPC_PAYLOAD_CAPACITY, VFS_IPC_RESPONSE_HEADER_BYTES,
    VfsExecutableSnapshotRequest, VfsExecutableSnapshotResponse, VfsIpcRequest, VfsIpcResponse,
    WAITSET_ABI_VERSION, WAITSET_PROVIDER_MAX, WAITSET_PROVIDER_VFSD, WaitSetInterestWire,
    WaitSetSignalBrokerArgs, identity_is_exact_sender, loader_service_role_allows_operation,
    net_broker_socket_publication_shape_valid, procd_sigchld_is_suppressed,
    product_executable_snapshot_evidence_shape_valid, waitset_interest_shape_valid,
    waitset_signal_shape_valid,
};

#[test]
fn product_executable_snapshot_evidence_requires_every_nonforgeable_input() {
    let valid = ProductExecutableSnapshotEvidence {
        storage_epoch: 7,
        mount_generation: 8,
        request_id: 9,
        file_bytes: 10,
        digest: [0x5a; 32],
        ..ProductExecutableSnapshotEvidence::default()
    };
    assert!(product_executable_snapshot_evidence_shape_valid(&valid));
    assert_eq!(
        valid.abi_version,
        PRODUCT_EXECUTABLE_SNAPSHOT_EVIDENCE_ABI_VERSION
    );
    assert_eq!(
        valid.backing,
        PRODUCT_EXECUTABLE_SNAPSHOT_BACKING_DVM_VOLUME
    );
    assert!(!product_executable_snapshot_evidence_shape_valid(
        &ProductExecutableSnapshotEvidence {
            request_id: 0,
            ..valid
        }
    ));
    assert!(!product_executable_snapshot_evidence_shape_valid(
        &ProductExecutableSnapshotEvidence {
            digest: [0; 32],
            ..valid
        }
    ));
    assert!(!product_executable_snapshot_evidence_shape_valid(
        &ProductExecutableSnapshotEvidence {
            reserved0: 1,
            ..valid
        }
    ));
}

#[test]
fn smp_qualification_worker_shape_is_exact_and_bounded() {
    super::smp_qualification_tests::worker_shape_is_exact_and_bounded();
}

#[test]
fn smp_qualification_bind_shape_is_closed_and_bounded() {
    super::smp_qualification_tests::bind_shape_is_closed_and_bounded();
}

#[test]
fn nocldstop_suppresses_only_nonterminal_child_state_changes() {
    let stop_or_continue = PROCD_SIGCHLD_EVENT_MASK & !PROCD_SIGCHLD_EVENT_EXIT;
    assert!(procd_sigchld_is_suppressed(
        stop_or_continue,
        PROCD_SIGACTION_SA_NOCLDSTOP
    ));
    assert!(!procd_sigchld_is_suppressed(
        stop_or_continue | PROCD_SIGCHLD_EVENT_EXIT,
        PROCD_SIGACTION_SA_NOCLDSTOP
    ));
    assert!(!procd_sigchld_is_suppressed(stop_or_continue, 0));
    assert!(!procd_sigchld_is_suppressed(
        0,
        PROCD_SIGACTION_SA_NOCLDSTOP
    ));
}

#[test]
fn waitset_signal_requires_the_exact_public_wire_shape() {
    let valid = WaitSetSignalBrokerArgs {
        abi_version: WAITSET_ABI_VERSION,
        provider: WAITSET_PROVIDER_VFSD,
        flags: 0,
        object_id: 0xfeed_beef,
        generation: 1,
        reserved0: 0,
    };
    assert!(waitset_signal_shape_valid(&valid));
    assert!(!waitset_signal_shape_valid(&WaitSetSignalBrokerArgs {
        object_id: 0,
        ..valid
    }));
    assert!(!waitset_signal_shape_valid(&WaitSetSignalBrokerArgs {
        generation: 0,
        ..valid
    }));
    assert!(!waitset_signal_shape_valid(&WaitSetSignalBrokerArgs {
        provider: 0,
        ..valid
    }));
    assert!(!waitset_signal_shape_valid(&WaitSetSignalBrokerArgs {
        reserved0: 1,
        ..valid
    }));
}

#[test]
fn waitset_interest_requires_the_exact_persistent_wire_shape() {
    let valid = WaitSetInterestWire {
        abi_version: WAITSET_ABI_VERSION,
        provider: WAITSET_PROVIDER_VFSD,
        flags: 0,
        target_fd: u16::MAX as u64,
        object_id: 0xfeed_beef,
        provider_epoch: 1,
        events: u32::MAX,
        reserved0: 0,
        data: u64::MAX,
    };
    assert!(waitset_interest_shape_valid(&valid));
    assert!(waitset_interest_shape_valid(&WaitSetInterestWire {
        provider: WAITSET_PROVIDER_MAX,
        ..valid
    }));
    assert!(!waitset_interest_shape_valid(&WaitSetInterestWire {
        abi_version: WAITSET_ABI_VERSION.wrapping_add(1),
        ..valid
    }));
    assert!(!waitset_interest_shape_valid(&WaitSetInterestWire {
        provider: WAITSET_PROVIDER_VFSD - 1,
        ..valid
    }));
    assert!(!waitset_interest_shape_valid(&WaitSetInterestWire {
        provider: WAITSET_PROVIDER_MAX + 1,
        ..valid
    }));
    assert!(!waitset_interest_shape_valid(&WaitSetInterestWire {
        flags: 1,
        ..valid
    }));
    assert!(!waitset_interest_shape_valid(&WaitSetInterestWire {
        target_fd: u16::MAX as u64 + 1,
        ..valid
    }));
    assert!(!waitset_interest_shape_valid(&WaitSetInterestWire {
        object_id: 0,
        ..valid
    }));
    assert!(!waitset_interest_shape_valid(&WaitSetInterestWire {
        provider_epoch: 0,
        ..valid
    }));
    assert!(!waitset_interest_shape_valid(&WaitSetInterestWire {
        reserved0: 1,
        ..valid
    }));
}

#[test]
fn commercial_response_envelope_matches_exact_request_and_bounds_nested_fields() {
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.protocol = 7;
    request.header.op = 3;
    request.header.service_id = 11;
    request.header.subject_pid = 13;
    request.header.subject_tid = 17;
    request.header.ticket = 19;
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    assert!(response.is_valid_envelope_for(&request));

    response.header.ticket += 1;
    assert!(!response.is_valid_envelope_for(&request));
    response.header = request.header;
    response.descriptor_count = 1;
    response.descriptors[0].name_len = (response.descriptors[0].name.len() + 1) as u16;
    assert!(!response.is_valid_envelope_for(&request));
    response.descriptors[0].name_len = 0;
    response.capability.reserved1 = 1;
    assert!(!response.is_valid_envelope_for(&request));
}

#[test]
fn commercial_request_envelope_rejects_reserved_flags_and_oversized_lengths() {
    let mut request = CommercialMaxProtocolRequest::default();
    assert!(request.has_valid_envelope());
    request.header.flags = 1;
    assert!(!request.has_valid_envelope());
    request.header.flags = 0;
    request.payload_len = (request.payload.len() + 1) as u32;
    assert!(!request.has_valid_envelope());
    request.payload_len = 0;
    request.path_len = (request.path.len() + 1) as u32;
    assert!(!request.has_valid_envelope());
}

#[test]
fn service_subject_identity_is_never_a_zero_or_foreign_wildcard() {
    let mut request = CommercialMaxProtocolRequest::default();
    assert!(!request.subject_is_exact_sender(17, 19));
    request.header.subject_pid = 17;
    request.header.subject_tid = 19;
    assert!(request.subject_is_exact_sender(17, 19));
    assert!(!request.subject_is_exact_sender(17, 20));
    assert!(!identity_is_exact_sender(17, 0, 17, 0));
}

#[test]
fn loader_requester_identity_is_bound_to_the_kernel_sender() {
    let mut request = LoaderSpawnRequest::default();
    assert!(!request.requester_is_exact_sender(23));
    request.requester_pid = 23;
    assert!(request.requester_is_exact_sender(23));
    assert!(!request.requester_is_exact_sender(29));

    let owner = RustosIpcValidateServiceOwnerArgs {
        abi_version: IPC_ABI_VERSION,
        service_id: IPC_SERVICE_DEVMGRD,
        process_id: 23,
        ..RustosIpcValidateServiceOwnerArgs::default()
    };
    assert_eq!(owner.flags, 0);
    assert_eq!(owner.reserved0, 0);
    assert_eq!(owner.reserved1, 0);
}

#[test]
fn privileged_loader_operations_have_an_explicit_service_role_matrix() {
    for service_id in [IPC_SERVICE_ROOTD, IPC_SERVICE_INITD, IPC_SERVICE_SESSIOND] {
        assert!(loader_service_role_allows_operation(
            LOADER_OP_SPAWN_EXEC,
            service_id,
        ));
    }
    assert!(!loader_service_role_allows_operation(
        LOADER_OP_SPAWN_EXEC,
        IPC_SERVICE_PROCD,
    ));
    assert!(loader_service_role_allows_operation(
        LOADER_OP_EXEC_TARGET,
        IPC_SERVICE_PROCD,
    ));
    assert!(!loader_service_role_allows_operation(
        LOADER_OP_EXEC_TARGET,
        IPC_SERVICE_ROOTD,
    ));
    assert!(!loader_service_role_allows_operation(
        LOADER_OP_ACTIVATE,
        IPC_SERVICE_ROOTD,
    ));
}

#[test]
fn storaged_bulk_read_response_fills_one_exact_inline_message() {
    assert_eq!(
        core::mem::offset_of!(StoragedBulkReadResponse, payload),
        STORAGED_BULK_READ_RESPONSE_HEADER_BYTES
    );
    assert_eq!(
        STORAGED_BULK_READ_PAYLOAD_CAPACITY,
        IPC_MAX_INLINE_BYTES - STORAGED_BULK_READ_RESPONSE_HEADER_BYTES
    );
    assert_eq!(size_of::<StoragedBulkReadResponse>(), IPC_MAX_INLINE_BYTES);
}

#[test]
fn storaged_bulk_read_response_binds_the_complete_request_header() {
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.protocol = 5;
    request.header.op = 12;
    request.header.ticket = 19;
    let mut response = StoragedBulkReadResponse {
        header: request.header,
        ..StoragedBulkReadResponse::default()
    };
    assert!(response.is_valid_envelope_for(&request));

    response.header.ticket += 1;
    assert!(!response.is_valid_envelope_for(&request));
    response.header = request.header;
    response.reserved0 = 1;
    assert!(!response.is_valid_envelope_for(&request));
    response.reserved0 = 0;
    response.payload_len = (response.payload.len() + 1) as u32;
    assert!(!response.is_valid_envelope_for(&request));
}

#[test]
fn statx_offload_messages_fit_inline_ipc_v1() {
    assert!(size_of::<LinuxSyscallOffloadRequest>() <= IPC_MAX_INLINE_BYTES);
    assert!(size_of::<LinuxSyscallOffloadResponse>() <= IPC_MAX_INLINE_BYTES);
    assert!(size_of::<VfsIpcRequest>() <= IPC_MAX_INLINE_BYTES);
    assert!(size_of::<VfsExecutableSnapshotRequest>() <= IPC_MAX_INLINE_BYTES);
    assert!(size_of::<VfsExecutableSnapshotResponse>() <= IPC_MAX_INLINE_BYTES);
    assert_eq!(
        core::mem::offset_of!(VfsIpcResponse, payload),
        VFS_IPC_RESPONSE_HEADER_BYTES
    );
    assert_eq!(size_of::<VfsIpcResponse>(), IPC_MAX_INLINE_BYTES);
    assert_eq!(
        VFS_IPC_PAYLOAD_CAPACITY,
        IPC_MAX_INLINE_BYTES - VFS_IPC_RESPONSE_HEADER_BYTES
    );
    assert_eq!(LINUX_STATX_SIZE, 0x100);
    assert_eq!(SYSCALL_OFFLOAD_PATH_CAPACITY, 256);
    assert_eq!(SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, 0x200);
    assert_eq!(LINUX_RLIMIT_SIZE, size_of::<LinuxRlimit>());
    assert_eq!(LINUX_TIMESPEC_SIZE, size_of::<LinuxTimespecWire>());
    assert_eq!(LINUX_SIGACTION_SIZE, size_of::<LinuxSigActionWire>());
    assert_eq!(LINUX_UTSNAME_SIZE, size_of::<LinuxUtsName>());
}

#[test]
fn statx_offload_defaults_are_valid_v1_headers() {
    let request = LinuxSyscallOffloadRequest::default();
    assert_eq!(request.version, SYSCALL_OFFLOAD_ABI_VERSION);
    assert_eq!(request.op, SYSCALL_OFFLOAD_OP_LINUX_STATX);
    assert_eq!(request.reserved0, 0);

    let response = LinuxSyscallOffloadResponse::default();
    assert_eq!(response.version, SYSCALL_OFFLOAD_ABI_VERSION);
    assert_eq!(response.op, SYSCALL_OFFLOAD_OP_LINUX_STATX);
    assert_eq!(response.reserved0, 0);
    assert_eq!(response.payload_len, 0);

    let vfs_request = VfsIpcRequest::default();
    assert_eq!(vfs_request.version, VFS_IPC_ABI_VERSION);
    assert_eq!(vfs_request.op, VFS_IPC_OP_OPENAT);

    let vfs_response = VfsIpcResponse::default();
    assert_eq!(vfs_response.version, VFS_IPC_ABI_VERSION);
    assert_eq!(vfs_response.op, VFS_IPC_OP_OPENAT);
    assert_eq!(vfs_response.reserved0, 0);

    let snapshot_request = VfsExecutableSnapshotRequest::default();
    assert_eq!(
        snapshot_request.version,
        VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION
    );
    assert_eq!(snapshot_request.op, VFS_EXECUTABLE_SNAPSHOT_OP_OPEN);
}

#[test]
fn socket_poll_owns_a_unique_offload_operation() {
    assert_ne!(
        SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
        SYSCALL_OFFLOAD_OP_LINUX_MPROTECT
    );
    const {
        assert!(SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET > SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY);
    }
}

#[test]
fn netd_v7_deadline_wire_keeps_the_fixed_header_layout() {
    assert_eq!(NETD_IPC_ABI_VERSION, 7);
    assert_eq!(NETD_IPC_REQUEST_HEADER_SIZE, 136);
    assert_eq!(NETD_IPC_RESPONSE_HEADER_SIZE, 32);
    assert_eq!(core::mem::offset_of!(NetdIpcRequest, deadline_ns), 112);
    assert_eq!(NetdIpcRequest::default().deadline_ns, 0);
    assert_eq!(
        size_of::<NetdIpcRequest>(),
        NETD_IPC_REQUEST_HEADER_SIZE + NETD_IPC_PAYLOAD_CAPACITY
    );
    assert_eq!(
        size_of::<NetdIpcResponse>(),
        NETD_IPC_RESPONSE_HEADER_SIZE + NETD_IPC_PAYLOAD_CAPACITY
    );
}

#[test]
fn prepared_socket_publication_wire_is_versioned_and_address_free() {
    assert_eq!(NET_BROKER_SOCKET_PUBLICATION_VERSION, 1);
    assert_eq!(
        core::mem::size_of::<NetBrokerPrepareSocketPublication>(),
        72
    );
    assert_eq!(
        core::mem::offset_of!(NetBrokerPrepareSocketPublication, reply_cap),
        8
    );
    assert_eq!(NetBrokerPrepareSocketPublication::default().reserved0, 0);
    assert_eq!(NetBrokerPrepareSocketPublication::default().reserved1, 0);
    assert_eq!(NetBrokerPrepareSocketPublication::default().reserved2, 0);
    assert_eq!(NetBrokerPrepareSocketPublication::default().reserved3, 0);
}

#[test]
fn prepared_socket_publication_shape_allows_only_inet_stream_open_flags() {
    let publication = NetBrokerPrepareSocketPublication {
        version: NET_BROKER_SOCKET_PUBLICATION_VERSION,
        reply_cap: 11,
        caller_pid: 12,
        socket_token: 13,
        domain: NET_BROKER_PUBLICATION_AF_INET,
        socket_type: NET_BROKER_PUBLICATION_SOCK_STREAM,
        protocol: 6,
        ..NetBrokerPrepareSocketPublication::default()
    };
    for flags in [
        0,
        NET_BROKER_PUBLICATION_SOCK_NONBLOCK,
        NET_BROKER_PUBLICATION_SOCK_CLOEXEC,
        NET_BROKER_PUBLICATION_ALLOWED_SOCKET_FLAGS,
    ] {
        let mut allowed = publication;
        allowed.socket_type |= flags;
        assert!(net_broker_socket_publication_shape_valid(&allowed));
    }

    let mut malformed = publication;
    malformed.version = malformed.version.wrapping_add(1);
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
    malformed = publication;
    malformed.reserved3 = 1;
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
    malformed = publication;
    malformed.reply_cap = 0;
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
    malformed = publication;
    malformed.caller_pid = 0;
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
    malformed = publication;
    malformed.socket_token = 0;
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
    malformed = publication;
    malformed.domain = 1;
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
    malformed = publication;
    malformed.socket_type = 2;
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
    malformed = publication;
    malformed.socket_type |= 1 << 30;
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
}
