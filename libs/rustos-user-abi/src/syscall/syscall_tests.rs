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
    NET_BROKER_SOCKET_PUBLICATION_VERSION, NETD_IPC_ABI_VERSION, NETD_IPC_OP_REF_ACK_BATCH,
    NETD_IPC_PAYLOAD_CAPACITY, NETD_IPC_REQUEST_HEADER_SIZE, NETD_IPC_RESPONSE_HEADER_SIZE,
    NETD_REF_ACK_BATCH_MAX, NetBrokerPrepareSocketPublication, NetdIpcRequest, NetdIpcResponse,
    NetdRefAckWire, PROCD_SIGACTION_SA_NOCLDSTOP, PROCD_SIGCHLD_EVENT_EXIT,
    PROCD_SIGCHLD_EVENT_MASK, PRODUCT_EXECUTABLE_SNAPSHOT_BACKING_DVM_VOLUME,
    PRODUCT_EXECUTABLE_SNAPSHOT_EVIDENCE_ABI_VERSION, ProductExecutableSnapshotEvidence,
    RustosIpcValidateServiceOwnerArgs, RustosPagerFaultReplyArgs, RustosPagerFaultWaitArgs,
    RustosSchedulingContextPolicy, STORAGED_BULK_READ_PAYLOAD_CAPACITY,
    STORAGED_BULK_READ_RESPONSE_HEADER_BYTES, STORAGED_BULK_WRITE_PAYLOAD_CAPACITY,
    STORAGED_BULK_WRITE_REQUEST_HEADER_BYTES, SYS_RUSTOS_PAGER_FAULT_REPLY,
    SYS_RUSTOS_PAGER_FAULT_WAIT, SYSCALL_OFFLOAD_ABI_VERSION,
    SYSCALL_OFFLOAD_OP_LINUX_MPROTECT, SYSCALL_OFFLOAD_OP_LINUX_PROCESS_EXIT,
    SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_STATX,
    SYSCALL_OFFLOAD_PATH_CAPACITY, SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, StoragedBulkReadResponse,
    StoragedBulkWriteRequest,
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
fn process_prepare_job_authority_layout_is_explicit() {
    use super::{RustosProcPrepareBrokerArgs, PROC_BROKER_ABI_VERSION,
        PROC_BROKER_PREPARE_FLAG_EXEC_TICKET, PROC_BROKER_PREPARE_FLAG_JOB_HANDLE};
    assert_eq!(PROC_BROKER_ABI_VERSION, 5);
    assert_eq!(size_of::<RustosProcPrepareBrokerArgs>(), 32);
    assert_eq!(core::mem::offset_of!(RustosProcPrepareBrokerArgs, reserved0), 8);
    assert_eq!(core::mem::offset_of!(RustosProcPrepareBrokerArgs, stack_top), 16);
    assert_eq!(core::mem::offset_of!(RustosProcPrepareBrokerArgs, job_handle), 24);
    assert_ne!(PROC_BROKER_PREPARE_FLAG_JOB_HANDLE, 0);
    assert_eq!(PROC_BROKER_PREPARE_FLAG_JOB_HANDLE & PROC_BROKER_PREPARE_FLAG_EXEC_TICKET, 0);
}

#[test]
fn pager_fault_wire_is_distinct_fixed_and_append_only() {
    assert_ne!(SYS_RUSTOS_PAGER_FAULT_WAIT, SYS_RUSTOS_PAGER_FAULT_REPLY);
    assert_eq!(size_of::<RustosPagerFaultWaitArgs>(), 16);
    assert_eq!(size_of::<RustosPagerFaultReplyArgs>(), 120);
}

#[test]
fn scheduling_context_policy_is_closed_versioned_and_budget_bounded() {
    let valid = RustosSchedulingContextPolicy::new(0b11, 2_000_000, 10_000_000, 4, 1, 7, 9);
    assert!(valid.is_canonical());
    assert_eq!(size_of::<RustosSchedulingContextPolicy>(), 72);

    let mut malformed = valid;
    malformed.abi_version = malformed.abi_version.wrapping_add(1);
    assert!(!malformed.is_canonical());
    malformed = valid;
    malformed.budget_ns = malformed.period_ns + 1;
    assert!(!malformed.is_canonical());
    malformed = valid;
    malformed.refill_capacity = 9;
    assert!(!malformed.is_canonical());
    malformed = valid;
    malformed.reserved1 = 1;
    assert!(!malformed.is_canonical());
}

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
fn waitset_batch_wires_have_the_documented_layout_and_bounds() {
    use super::{
        NETD_POLL_BATCH_MAX, NetdPollQueryWire, NetdPollResultWire, VFS_EPOLL_CLAIM_MAX,
        VFS_IPC_REQUEST_PAYLOAD_CAPACITY, VFS_POLL_BATCH_MAX, WAITSET_SNAPSHOT_STATE_DISABLED,
        WaitSetClaimWire, WaitSetSnapshotWire, waitset_claim_shape_valid,
        waitset_snapshot_shape_valid,
    };
    assert_eq!(core::mem::size_of::<NetdPollQueryWire>(), 16);
    assert_eq!(core::mem::size_of::<NetdPollResultWire>(), 16);
    assert_eq!(core::mem::size_of::<WaitSetClaimWire>(), 32);
    assert_eq!(core::mem::size_of::<WaitSetSnapshotWire>(), 64);
    assert!(VFS_POLL_BATCH_MAX * 8 <= VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
    assert!(VFS_EPOLL_CLAIM_MAX * 32 <= VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
    assert!(NETD_POLL_BATCH_MAX * 16 <= NETD_IPC_PAYLOAD_CAPACITY);

    let interest = WaitSetInterestWire {
        abi_version: WAITSET_ABI_VERSION,
        provider: WAITSET_PROVIDER_VFSD,
        target_fd: 3,
        object_id: 9,
        provider_epoch: 1,
        ..WaitSetInterestWire::default()
    };
    let snapshot = WaitSetSnapshotWire {
        interest,
        edge_generation: 4,
        state: WAITSET_SNAPSHOT_STATE_DISABLED,
        reserved0: 0,
    };
    assert!(waitset_snapshot_shape_valid(&snapshot));
    assert!(!waitset_snapshot_shape_valid(&WaitSetSnapshotWire {
        state: 2,
        ..snapshot
    }));
    assert!(!waitset_snapshot_shape_valid(&WaitSetSnapshotWire {
        reserved0: 1,
        ..snapshot
    }));

    let claim = WaitSetClaimWire {
        provider: WAITSET_PROVIDER_VFSD,
        target_fd: 3,
        object_id: 9,
        generation: 1,
        ..WaitSetClaimWire::default()
    };
    assert!(waitset_claim_shape_valid(&claim));
    assert!(!waitset_claim_shape_valid(&WaitSetClaimWire {
        generation: 0,
        ..claim
    }));
    assert!(!waitset_claim_shape_valid(&WaitSetClaimWire {
        provider: WAITSET_PROVIDER_MAX + 1,
        ..claim
    }));
    assert!(!waitset_claim_shape_valid(&WaitSetClaimWire {
        reserved1: 1,
        ..claim
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
fn loader_spawn_request_carries_the_application_request_after_the_requester() {
    use crate::application_request::ApplicationRequest;
    use crate::syscall::LOADER_REQUEST_ABI_VERSION;
    assert_eq!(LOADER_REQUEST_ABI_VERSION, 5);
    assert_eq!(core::mem::size_of::<ApplicationRequest>(), 24);
    assert_eq!(
        core::mem::offset_of!(LoaderSpawnRequest, application_request),
        core::mem::offset_of!(LoaderSpawnRequest, requester_pid) + 8
    );
    assert_eq!(
        core::mem::offset_of!(LoaderSpawnRequest, scheduling_context),
        core::mem::offset_of!(LoaderSpawnRequest, application_request) + 24
    );
    // Zero is the only default: a service request never carries a Job request.
    assert!(!LoaderSpawnRequest::default().application_request.is_declared());
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
fn storaged_bulk_read_reply_length_is_header_plus_exact_payload() {
    const HDR: usize = STORAGED_BULK_READ_RESPONSE_HEADER_BYTES;
    assert_eq!(StoragedBulkReadResponse::reply_len_for(0), Some(HDR));
    assert_eq!(
        StoragedBulkReadResponse::reply_len_for(STORAGED_BULK_READ_PAYLOAD_CAPACITY),
        Some(IPC_MAX_INLINE_BYTES)
    );
    assert_eq!(
        StoragedBulkReadResponse::reply_len_for(STORAGED_BULK_READ_PAYLOAD_CAPACITY + 1),
        None
    );

    let mut response = StoragedBulkReadResponse::default();
    response.payload_len = 4096;
    response.payload[..4096].fill(0xa5);
    response.payload[4096] = 0xee;
    assert_eq!(response.payload_of_reply(HDR + 4096).map(<[u8]>::len), Some(4096));
    assert!(response.payload_of_reply(HDR + 4096).unwrap().iter().all(|b| *b == 0xa5));
    // Short, long and header-truncated receives are all rejected.
    assert!(response.payload_of_reply(HDR + 4095).is_none());
    assert!(response.payload_of_reply(HDR + 4097).is_none());
    assert!(response.payload_of_reply(HDR - 1).is_none());
    assert!(response.payload_of_reply(0).is_none());
    // An oversized claimed payload is rejected even at the full buffer size.
    response.payload_len = (STORAGED_BULK_READ_PAYLOAD_CAPACITY + 1) as u32;
    assert!(response.payload_of_reply(IPC_MAX_INLINE_BYTES).is_none());
    // An error reply is header-only.
    response.payload_len = 0;
    assert_eq!(response.payload_of_reply(HDR), Some(&[][..]));
}

#[test]
fn storaged_bulk_write_request_length_is_header_plus_exact_payload() {
    const HDR: usize = STORAGED_BULK_WRITE_REQUEST_HEADER_BYTES;
    assert_eq!(StoragedBulkWriteRequest::request_len_for(0), Some(HDR));
    assert_eq!(
        StoragedBulkWriteRequest::request_len_for(STORAGED_BULK_WRITE_PAYLOAD_CAPACITY),
        Some(IPC_MAX_INLINE_BYTES)
    );
    assert_eq!(
        StoragedBulkWriteRequest::request_len_for(STORAGED_BULK_WRITE_PAYLOAD_CAPACITY + 1),
        None
    );
    let mut req = StoragedBulkWriteRequest::default();
    req.payload_len = 512;
    assert_eq!(req.payload_of_request(HDR + 512).map(<[u8]>::len), Some(512));
    assert!(req.payload_of_request(HDR + 511).is_none());
    assert!(req.payload_of_request(HDR + 513).is_none());
    assert!(req.payload_of_request(HDR - 1).is_none());
    req.payload_len = (STORAGED_BULK_WRITE_PAYLOAD_CAPACITY + 1) as u32;
    assert!(req.payload_of_request(IPC_MAX_INLINE_BYTES).is_none());
}

#[test]
fn storaged_bulk_write_request_fills_one_exact_inline_message() {
    assert_eq!(
        core::mem::offset_of!(StoragedBulkWriteRequest, payload),
        STORAGED_BULK_WRITE_REQUEST_HEADER_BYTES
    );
    assert_eq!(
        STORAGED_BULK_WRITE_PAYLOAD_CAPACITY,
        IPC_MAX_INLINE_BYTES - STORAGED_BULK_WRITE_REQUEST_HEADER_BYTES
    );
    assert_eq!(size_of::<StoragedBulkWriteRequest>(), IPC_MAX_INLINE_BYTES);

    let mut req = StoragedBulkWriteRequest {
        generation: 1,
        block_count: 1,
        payload_len: 512,
        ..StoragedBulkWriteRequest::default()
    };
    assert!(req.has_valid_shape());
    req.reserved0 = 1;
    assert!(!req.has_valid_shape());
    req.reserved0 = 0;
    req.payload_len = (req.payload.len() + 1) as u32;
    assert!(!req.has_valid_shape());
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
    assert_eq!(snapshot_request.target_pid, 0);
    assert_eq!(snapshot_request.dirfd, 0);

    let snapshot_response = VfsExecutableSnapshotResponse::default();
    assert_eq!(snapshot_response.resolved_path_len, 0);
}

#[test]
fn socket_poll_owns_a_unique_offload_operation() {
    assert_ne!(
        SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
        SYSCALL_OFFLOAD_OP_LINUX_MPROTECT
    );
    const {
        assert!(SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET > SYSCALL_OFFLOAD_OP_LINUX_PROCESS_EXIT);
    }
}

#[test]
fn netd_v10_reference_ack_batch_keeps_the_fixed_header_layout() {
    assert_eq!(NETD_IPC_ABI_VERSION, 10);
    assert_eq!(NETD_IPC_OP_REF_ACK_BATCH, 0x8007);
    assert_eq!(NETD_REF_ACK_BATCH_MAX, 32);
    assert_eq!(size_of::<NetdRefAckWire>(), 32);
    assert_eq!(core::mem::offset_of!(NetdRefAckWire, socket_token), 16);
    assert_eq!(core::mem::offset_of!(NetdRefAckWire, op), 24);
    assert_eq!(NETD_IPC_REQUEST_HEADER_SIZE, 136);
    assert_eq!(NETD_IPC_RESPONSE_HEADER_SIZE, 32);
    assert_eq!(core::mem::offset_of!(NetdIpcResponse, reserved2), 12);
    assert_eq!(NetdIpcResponse::default().reserved2, 0);
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
    assert_eq!(NET_BROKER_SOCKET_PUBLICATION_VERSION, 2);
    assert_eq!(
        core::mem::size_of::<NetBrokerPrepareSocketPublication>(),
        96
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
fn prepared_socket_publication_shape_allows_only_nonraw_inet_open_flags() {
    let publication = NetBrokerPrepareSocketPublication {
        version: NET_BROKER_SOCKET_PUBLICATION_VERSION,
        operation: super::SYSCALL_OFFLOAD_OP_LINUX_SOCKET as u64,
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
    malformed.socket_type = 3;
    malformed.protocol = 0;
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
    malformed = publication;
    malformed.socket_type |= 1 << 30;
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
    for protocol in [0, 1, 17] {
        let mut datagram = publication;
        datagram.socket_type = 2;
        datagram.protocol = protocol;
        assert!(net_broker_socket_publication_shape_valid(&datagram));
    }
    malformed = publication;
    malformed.protocol = 1;
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
    malformed.socket_type = 2;
    malformed.protocol = 6;
    assert!(!net_broker_socket_publication_shape_valid(&malformed));
}

#[test]
fn netlink_socket_shape_is_route_raw_or_dgram_and_socket_only() {
    use super::{
        NET_BROKER_PUBLICATION_AF_NETLINK, NET_BROKER_PUBLICATION_SOCK_RAW,
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
        SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR, net_broker_netlink_socket_shape_valid as netlink,
    };
    let flags = NET_BROKER_PUBLICATION_ALLOWED_SOCKET_FLAGS;
    let n = NET_BROKER_PUBLICATION_AF_NETLINK;
    assert!(netlink(n, NET_BROKER_PUBLICATION_SOCK_RAW | flags, 0));
    assert!(netlink(n, NET_BROKER_PUBLICATION_SOCK_RAW | flags, 15));
    assert!(netlink(n, 2, 0));
    assert!(netlink(n, 2, 15));
    for ty in [0, 1, 4, 5, 6] {
        assert!(!netlink(n, ty, 0), "type {ty}");
    }
    assert!(
        !netlink(n, NET_BROKER_PUBLICATION_SOCK_RAW, 9),
        "only NETLINK_ROUTE or NETLINK_KOBJECT_UEVENT"
    );
    assert!(!netlink(n, NET_BROKER_PUBLICATION_SOCK_RAW | 1 << 30, 0), "unknown flag");
    assert!(!netlink(1, NET_BROKER_PUBLICATION_SOCK_RAW, 0), "AF_UNIX is not netlink");
    let socket = NetBrokerPrepareSocketPublication {
        version: NET_BROKER_SOCKET_PUBLICATION_VERSION,
        operation: SYSCALL_OFFLOAD_OP_LINUX_SOCKET as u64,
        reply_cap: 11,
        caller_pid: 12,
        socket_token: 13,
        domain: n,
        socket_type: NET_BROKER_PUBLICATION_SOCK_RAW,
        ..NetBrokerPrepareSocketPublication::default()
    };
    assert!(net_broker_socket_publication_shape_valid(&socket));
    // A netlink socket cannot be paired or accepted.
    let pair = NetBrokerPrepareSocketPublication {
        operation: SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR as u64,
        reserved4: 14,
        ..socket
    };
    let accept = NetBrokerPrepareSocketPublication {
        operation: SYSCALL_OFFLOAD_OP_LINUX_ACCEPT as u64,
        reserved5: 15,
        ..socket
    };
    assert!(!net_broker_socket_publication_shape_valid(&pair));
    assert!(!net_broker_socket_publication_shape_valid(&accept));
}

#[test]
fn vfs_writable_ipc_opcodes_are_distinct_and_version_unchanged() {
    use super::{
        VFS_IPC_ABI_VERSION, VFS_IPC_OP_FSYNC, VFS_IPC_OP_LIBRARY_READ, VFS_IPC_OP_PWRITE64,
        VFS_IPC_OP_RENAMEAT2, VFS_IPC_OP_SET_ATTRS, VFS_IPC_OP_STATFS, VFS_IPC_OP_SYMLINKAT,
    };
    assert_eq!(VFS_IPC_ABI_VERSION, 5);
    assert_eq!(VFS_IPC_OP_LIBRARY_READ, 27);
    assert_eq!(VFS_IPC_OP_PWRITE64, 28);
    assert_eq!(VFS_IPC_OP_FSYNC, 29);
    assert_eq!(VFS_IPC_OP_RENAMEAT2, 30);
    assert_eq!(VFS_IPC_OP_STATFS, 31);
    assert_eq!(VFS_IPC_OP_SYMLINKAT, 32);
    assert_eq!(VFS_IPC_OP_SET_ATTRS, 33);
}

#[test]
fn vfs_memfs_ops_are_distinct_additive_and_decode_modes() {
    use super::{
        vfs_fallocate_keep_size, vfs_mknod_kind, VfsMknodKind, VFS_FALLOC_FL_KEEP_SIZE,
        VFS_FALLOC_FL_PUNCH_HOLE, VFS_IPC_ABI_VERSION, VFS_IPC_OP_CHMOD, VFS_IPC_OP_FALLOCATE,
        VFS_IPC_OP_FCHMOD, VFS_IPC_OP_MKNOD, VFS_IPC_OP_SET_ATTRS,
    };
    assert_eq!(VFS_IPC_ABI_VERSION, 5);
    let ops = [
        VFS_IPC_OP_SET_ATTRS,
        VFS_IPC_OP_MKNOD,
        VFS_IPC_OP_CHMOD,
        VFS_IPC_OP_FCHMOD,
        VFS_IPC_OP_FALLOCATE,
    ];
    assert_eq!(ops, [33, 34, 35, 36, 37]);
    assert_eq!(vfs_mknod_kind(0o140755), Some(VfsMknodKind::Socket));
    assert_eq!(vfs_mknod_kind(0o100644), Some(VfsMknodKind::Regular));
    assert_eq!(vfs_mknod_kind(0o644), Some(VfsMknodKind::Regular));
    assert_eq!(vfs_mknod_kind(0o010600), Some(VfsMknodKind::Fifo));
    assert_eq!(vfs_mknod_kind(0o020600), Some(VfsMknodKind::Device));
    assert_eq!(vfs_mknod_kind(0o060600), Some(VfsMknodKind::Device));
    assert_eq!(vfs_mknod_kind(0o040755), None);
    assert_eq!(vfs_mknod_kind(0o120777), None);
    assert_eq!(vfs_fallocate_keep_size(0), Some(false));
    assert_eq!(vfs_fallocate_keep_size(VFS_FALLOC_FL_KEEP_SIZE), Some(true));
    assert_eq!(
        vfs_fallocate_keep_size(VFS_FALLOC_FL_PUNCH_HOLE | VFS_FALLOC_FL_KEEP_SIZE),
        None
    );
    assert_eq!(vfs_fallocate_keep_size(0x10), None);
}

#[test]
fn vfs_statfs_wire_layout_and_round_trip() {
    use super::VfsStatFsWire;
    assert_eq!(size_of::<VfsStatFsWire>(), 120);
    assert_eq!(VfsStatFsWire::WIRE_BYTES, 120);
    assert!(VfsStatFsWire::WIRE_BYTES <= VFS_IPC_PAYLOAD_CAPACITY);

    let wire = VfsStatFsWire {
        f_type: 0x4d44,
        f_bsize: 4096,
        f_blocks: 1_000_000,
        f_bfree: 500_000,
        f_bavail: 450_000,
        f_files: 65_536,
        f_ffree: 60_000,
        f_fsid: [0x1234_5678, -0x0102_0304],
        f_namelen: 255,
        f_frsize: 4096,
        f_flags: 0,
        f_spare: [1, 2, 3, 4],
    };
    let bytes = wire.to_bytes();
    assert_eq!(VfsStatFsWire::from_bytes(&bytes), Some(wire));
    assert_eq!(VfsStatFsWire::from_bytes(&bytes[..119]), None);
    let mut long = [0_u8; 121];
    long[..120].copy_from_slice(&bytes);
    assert_eq!(VfsStatFsWire::from_bytes(&long), None);
}

#[test]
fn vfs_two_path_payload_round_trip_and_bounds() {
    use super::{
        VFS_IPC_PATH_CAPACITY, VFS_IPC_REQUEST_PAYLOAD_CAPACITY, decode_two_path_payload,
        encode_two_path_payload,
    };
    let mut buf = [0_u8; VFS_IPC_REQUEST_PAYLOAD_CAPACITY];
    let len = encode_two_path_payload(b"/mnt/fat/old.txt", b"/mnt/fat/new.txt", &mut buf)
        .expect("valid two-path payload");
    assert_eq!(len, 4 + 16 + 16);
    assert_eq!(
        decode_two_path_payload(&buf[..len]),
        Some((b"/mnt/fat/old.txt".as_slice(), b"/mnt/fat/new.txt".as_slice()))
    );

    // Exact capacity fit: 4 + 254 + 254 = 512 == VFS_IPC_REQUEST_PAYLOAD_CAPACITY
    let max_half = [b'a'; 254];
    let max_len = encode_two_path_payload(&max_half, &max_half, &mut buf)
        .expect("exact capacity two-path payload");
    assert_eq!(max_len, VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
    assert_eq!(
        decode_two_path_payload(&buf[..max_len]),
        Some((max_half.as_slice(), max_half.as_slice()))
    );

    // Overflowing combined payload is rejected.
    let too_long = [b'b'; 255];
    assert_eq!(encode_two_path_payload(&max_half, &too_long, &mut buf), None);

    // Path exceeding VFS_IPC_PATH_CAPACITY is rejected even with larger slice.
    let over_path = [b'c'; VFS_IPC_PATH_CAPACITY + 1];
    let mut big_buf = [0_u8; VFS_IPC_PATH_CAPACITY + 16];
    assert_eq!(encode_two_path_payload(&over_path, b"x", &mut big_buf), None);

    // Short output buffer is rejected.
    let mut small_buf = [0_u8; 8];
    assert_eq!(encode_two_path_payload(b"abc", b"def", &mut small_buf), None);

    // Malformed decode inputs: too short, length mismatch, trailing bytes.
    assert_eq!(decode_two_path_payload(&buf[..3]), None);
    assert_eq!(decode_two_path_payload(&buf[..len - 1]), None);
    let mut trailing = [0_u8; 4 + 16 + 16 + 1];
    let _ = encode_two_path_payload(b"/mnt/fat/old.txt", b"/mnt/fat/new.txt", &mut trailing);
    assert_eq!(decode_two_path_payload(&trailing), None);
}

#[test]
fn vfs_cursor_sync_constants_are_distinct_and_nonzero() {
    use super::{
        VFS_CURSOR_SETTLE_CANCEL, VFS_CURSOR_SETTLE_COMMIT, VFS_CURSOR_SETTLE_SYNC,
        VFS_LSEEK_FLAG_SYNC_CURSOR, VFS_WRITE_FLAG_CURSOR_SYNC,
    };
    assert_eq!(VFS_CURSOR_SETTLE_COMMIT, 1);
    assert_eq!(VFS_CURSOR_SETTLE_CANCEL, 2);
    assert_eq!(VFS_CURSOR_SETTLE_SYNC, 3);
    assert_eq!(VFS_LSEEK_FLAG_SYNC_CURSOR, 1);
    assert_eq!(VFS_WRITE_FLAG_CURSOR_SYNC, 1);
}
