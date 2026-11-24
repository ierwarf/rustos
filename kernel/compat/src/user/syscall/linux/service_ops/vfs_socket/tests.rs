use super::*;

#[test]
fn descriptor_exhaustion_is_not_reported_as_a_bad_source_fd() {
    assert_eq!(duplicate_install_errno(VfsDupMode::Dup), LINUX_EMFILE);
    assert_eq!(duplicate_install_errno(VfsDupMode::Dup2), LINUX_EBADF);
    assert_eq!(duplicate_install_errno(VfsDupMode::Dup3), LINUX_EBADF);
}

#[test]
fn netd_reference_mutation_owns_the_complete_state_mutation_lease() {
    assert_eq!(
        NETD_REF_OPERATION_TIMEOUT_MS,
        rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS
    );
    assert_eq!(
        super::ipc_helpers::deadline::remaining_service_timeout_ms(
            NETD_REF_OPERATION_TIMEOUT_MS,
            0,
        ),
        Some(5_000)
    );
    assert!(
        !super::ipc_helpers::deadline::retryable_early_service_transport_error(LINUX_ETIMEDOUT)
    );
}

#[test]
fn foreground_final_close_stays_on_the_short_rail_while_dup_owns_the_lease() {
    assert_eq!(
        netd_ref_foreground_timeout_ms(SYSCALL_OFFLOAD_OP_LINUX_CLOSE),
        rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
    );
    assert_eq!(
        netd_ref_foreground_timeout_ms(SYSCALL_OFFLOAD_OP_LINUX_DUP),
        rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS
    );
}

#[test]
fn netd_reference_retry_keeps_the_original_wire_deadline() {
    let start_ns = 17 * super::vfs_meta::NETD_NANOS_PER_MILLI;
    let deadline_ns =
        super::vfs_meta::netd_deadline_after_ms(start_ns, NETD_REF_OPERATION_TIMEOUT_MS);
    // The production builder also randomizes an idempotency token through
    // the kernel entropy source, which is intentionally unavailable to
    // host unit tests. The invariant under test is the copied wire end.
    let request = NetdIpcRequest {
        op: SYSCALL_OFFLOAD_OP_LINUX_DUP,
        socket_token: 0xfeed_beef,
        deadline_ns,
        ..NetdIpcRequest::default()
    };
    let retry = request;

    assert_eq!(request.deadline_ns, deadline_ns);
    assert_eq!(retry.deadline_ns, request.deadline_ns);
    assert_eq!(
        super::vfs_meta::netd_deadline_remaining_ms_at(
            retry.deadline_ns,
            start_ns + 4_999 * super::vfs_meta::NETD_NANOS_PER_MILLI,
        ),
        Some(1)
    );
    assert_eq!(
        super::vfs_meta::netd_deadline_remaining_ms_at(retry.deadline_ns, deadline_ns),
        None
    );
}

#[test]
fn transferred_input_description_keeps_the_waitset_service_reference() {
    let device = kernel_object::api::device::DeviceHandle::from_parts_with_token(
        kernel_object::api::device::DeviceId::Input,
        kernel_object::api::device::DeviceAccessKind::Evdev,
        u64::MAX - 811,
    );
    assert_eq!(
        service_handle_ref_for_handle(&multitask::KernelHandle::Device(device)),
        Some(ServiceHandleRef::Input(device.token_id()))
    );
}

#[test]
fn fork_service_refs_come_from_the_frozen_child_handle_snapshot() {
    let mut parent = multitask::HandleTable::new();
    let inherited = multitask::EpollHandle::new();
    let inherited_token = inherited.token_id();
    let fd = parent
        .install(multitask::KernelHandle::Epoll(inherited))
        .expect("epoll fd");
    let child_snapshot = parent.clone();

    assert!(parent.close(fd).is_some());
    let replacement = multitask::EpollHandle::new();
    let replacement_token = replacement.token_id();
    assert_eq!(
        parent.install(multitask::KernelHandle::Epoll(replacement)),
        Some(fd)
    );

    let inherited_refs = service_handle_refs_from_table(&child_snapshot);
    let replacement_refs = service_handle_refs_from_table(&parent);
    assert!(matches!(
        inherited_refs.as_slice(),
        [ServiceHandleRef::Epoll(epoll)] if epoll.token_id() == inherited_token
    ));
    assert!(matches!(
        replacement_refs.as_slice(),
        [ServiceHandleRef::Epoll(epoll)] if epoll.token_id() == replacement_token
    ));
}

#[test]
fn exit_service_refs_come_from_the_exact_closed_handle_set() {
    let mut handles = multitask::HandleTable::new();
    let epoll = multitask::EpollHandle::new();
    let token = epoll.token_id();
    handles
        .install(multitask::KernelHandle::Epoll(epoll))
        .expect("epoll fd");

    let closed = handles.close_all();
    assert!(handles.entries_snapshot(false).is_empty());
    let refs = service_handle_refs_from_handles(&closed);
    assert!(matches!(
        refs.as_slice(),
        [ServiceHandleRef::Epoll(epoll)] if epoll.token_id() == token
    ));
}

#[test]
fn remote_vfs_refs_are_local_and_provider_close_is_final_only() {
    let id = u64::MAX - 0x5f5;
    assert_eq!(register_remote_vfs_open_description(id), Ok(()));
    assert_eq!(acquire_remote_vfs_descriptor_ref(id), Ok(()));
    assert_eq!(release_remote_vfs_descriptor_ref(id), Ok(false));
    assert_eq!(release_remote_vfs_descriptor_ref(id), Ok(true));
    assert_eq!(release_remote_vfs_descriptor_ref(id), Err(LINUX_EBADF));
}

#[test]
fn remote_vfs_registry_preserves_collision_chains_across_tombstones() {
    let first = 0xfedc_ba98_7654_0001;
    let bucket = RemoteVfsRefRegistry::probe_start(first);
    let second = (first + 1..first + REMOTE_VFS_OPEN_DESCRIPTION_CAPACITY as u64 + 2)
        .find(|candidate| RemoteVfsRefRegistry::probe_start(*candidate) == bucket)
        .expect("bounded hash range contains a collision");

    assert_eq!(register_remote_vfs_open_description(first), Ok(()));
    assert_eq!(register_remote_vfs_open_description(second), Ok(()));
    assert_eq!(release_remote_vfs_descriptor_ref(first), Ok(true));
    assert_eq!(acquire_remote_vfs_descriptor_ref(second), Ok(()));
    assert_eq!(release_remote_vfs_descriptor_ref(second), Ok(false));
    assert_eq!(release_remote_vfs_descriptor_ref(second), Ok(true));
    assert_eq!(register_remote_vfs_open_description(first), Ok(()));
    assert_eq!(release_remote_vfs_descriptor_ref(first), Ok(true));
}

#[test]
fn memfd_reopen_flags_preserve_attenuated_rights_and_install_distinct_description() {
    let mut table = multitask::HandleTable::new();
    let memfd = multitask::MemfdHandle::new(alloc::string::String::from("shm"), true, None);
    let rw_fd = table
        .install_with_open_flags(multitask::KernelHandle::Memfd(memfd.clone()), linux_abi::O_RDWR)
        .expect("rw memfd");
    let rw_entry = table.get_entry(rw_fd).expect("rw entry").clone();
    assert!(rw_entry.rights().allows_read() && rw_entry.rights().allows_write());

    assert_eq!(
        validate_memfd_reopen_flags(linux_abi::O_RDONLY | linux_abi::O_CLOEXEC, rw_entry.rights()),
        Ok(false)
    );
    assert_eq!(
        validate_memfd_reopen_flags(linux_abi::O_RDONLY | linux_abi::O_TRUNC, rw_entry.rights()),
        Err(LINUX_EACCES)
    );
    assert_eq!(
        validate_memfd_reopen_flags(linux_abi::O_RDONLY | linux_abi::O_DIRECTORY, rw_entry.rights()),
        Err(LINUX_ENOTDIR)
    );

    let ro_fd = table
        .install_with_open_flags(
            multitask::KernelHandle::Memfd(memfd.reopen()),
            linux_abi::O_RDONLY | linux_abi::O_CLOEXEC,
        )
        .expect("ro memfd");
    let ro_entry = table.get_entry(ro_fd).expect("ro entry");
    assert_ne!(ro_entry.token(), rw_entry.token());
    assert_eq!(ro_entry.status_flags() & linux_abi::O_ACCMODE, linux_abi::O_RDONLY);
    assert_eq!(ro_entry.fd_flags(), multitask::FD_CLOEXEC);
    assert!(ro_entry.rights().allows_read());
    assert!(!ro_entry.rights().allows_write());
    assert_eq!(
        validate_memfd_reopen_flags(linux_abi::O_RDWR, ro_entry.rights()),
        Err(LINUX_EACCES),
        "reopening a read-only memfd descriptor must not widen back to writable"
    );
}

#[test]
fn preadv2_validation_handles_sentinel_negative_and_rwf_flags() {
    assert_eq!(preadv2_validate_offset_and_flags(u64::MAX, 0), Ok(None));
    assert_eq!(preadv2_validate_offset_and_flags(0, 0), Ok(Some(0)));
    assert_eq!(
        preadv2_validate_offset_and_flags(4096, linux_abi::RWF_NOWAIT | linux_abi::RWF_APPEND),
        Ok(Some(4096))
    );
    assert_eq!(
        preadv2_validate_offset_and_flags((-2_i64) as u64, 0),
        Err(LINUX_EINVAL)
    );
    assert_eq!(
        preadv2_validate_offset_and_flags(0, 1_u64 << 20),
        Err(LINUX_EOPNOTSUPP)
    );
}

#[test]
fn remote_vfs_handle_table_enforces_write_rights_by_open_flags() {
    let mut table = multitask::HandleTable::new();
    let ro_remote = multitask::RemoteVfsHandle::new(
        1,
        multitask::RemoteVfsHandleKind::File,
        alloc::string::String::from("/data/ro.txt"),
        0,
        0,
    )
    .expect("remote vfs handle");
    let ro_fd = table
        .install_with_open_flags(
            multitask::KernelHandle::RemoteVfs(ro_remote),
            linux_abi::O_RDONLY,
        )
        .expect("ro fd");
    let ro_entry = table.get_entry(ro_fd).expect("ro entry");
    assert!(ro_entry.rights().allows_read());
    assert!(!ro_entry.rights().allows_write());

    let rw_remote = multitask::RemoteVfsHandle::new(
        2,
        multitask::RemoteVfsHandleKind::File,
        alloc::string::String::from("/data/rw.txt"),
        0,
        0,
    )
    .expect("remote vfs handle");
    let rw_fd = table
        .install_with_open_flags(
            multitask::KernelHandle::RemoteVfs(rw_remote),
            linux_abi::O_RDWR,
        )
        .expect("rw fd");
    let rw_entry = table.get_entry(rw_fd).expect("rw entry");
    assert!(rw_entry.rights().allows_read());
    assert!(rw_entry.rights().allows_write());
}

struct MockRegularFileReadTransport {
    ops: alloc::vec::Vec<(u16, u64, usize)>,
    bulk_error: Option<i64>,
    file_len: usize,
    bulk_subchunk: usize,
}

impl MockRegularFileReadTransport {
    fn new(file_len: usize) -> Self {
        Self {
            ops: alloc::vec::Vec::new(),
            bulk_error: None,
            file_len,
            bulk_subchunk: VFS_IPC_PAYLOAD_CAPACITY,
        }
    }
}

impl RegularFileReadTransport for MockRegularFileReadTransport {
    fn bulk_read(
        &mut self,
        offset: u64,
        want: usize,
        deliver: &mut dyn FnMut(&[u8]) -> Result<(), i64>,
    ) -> Result<usize, i64> {
        self.ops.push((VFS_IPC_OP_BULK_READ, offset, want));
        if let Some(errno) = self.bulk_error {
            return Err(errno);
        }
        let start = offset as usize;
        let available = self.file_len.saturating_sub(start).min(want);
        let mut sent = 0usize;
        while sent < available {
            let step = (available - sent).min(self.bulk_subchunk);
            let chunk = alloc::vec![0x5a_u8; step];
            deliver(&chunk)?;
            sent += step;
        }
        Ok(available)
    }

    fn pread64(
        &mut self,
        offset: u64,
        want: usize,
        deliver: &mut dyn FnMut(&[u8]) -> Result<(), i64>,
    ) -> Result<usize, i64> {
        self.ops.push((VFS_IPC_OP_PREAD64, offset, want));
        let start = offset as usize;
        let available = self.file_len.saturating_sub(start).min(want);
        if available > 0 {
            let chunk = alloc::vec![0x3c_u8; available];
            deliver(&chunk)?;
        }
        Ok(available)
    }

    fn yield_between_chunks(&mut self) {}
}

#[test]
fn regular_file_read_fault_at_zero_leaves_cursor_and_dirty_state_untouched() {
    let remote = multitask::RemoteVfsHandle::new_with_epoch(
        77,
        multitask::RemoteVfsHandleKind::File,
        alloc::string::String::from("/data/a.bin"),
        4096,
        0,
        3,
    )
    .expect("remote handle");
    remote.set_synced_cursor(128, 3);
    let mut transport = MockRegularFileReadTransport::new(4096);

    let result = read_remote_vfs_file_loop(
        Some(remote.description().as_ref()),
        remote.cursor(),
        0x1000,
        512,
        &mut transport,
        |_, _| Err(LINUX_EFAULT),
    );
    assert_eq!(result, linux_errno(LINUX_EFAULT));
    assert_eq!(remote.cursor(), 128);
    assert_eq!(remote.durable_cursor(), 128);
    assert!(!remote.is_dirty(3));
    assert_eq!(transport.ops, alloc::vec![(VFS_IPC_OP_PREAD64, 128, 512)]);
}

#[test]
fn regular_file_read_mid_transfer_fault_advances_cursor_by_exact_copied_chunks() {
    let remote = multitask::RemoteVfsHandle::new_with_epoch(
        78,
        multitask::RemoteVfsHandleKind::File,
        alloc::string::String::from("/data/b.bin"),
        200_000,
        0,
        4,
    )
    .expect("remote handle");
    let mut transport = MockRegularFileReadTransport::new(200_000);
    let mut chunk_index = 0usize;

    let result = read_remote_vfs_file_loop(
        Some(remote.description().as_ref()),
        remote.cursor(),
        0x2000,
        150_000,
        &mut transport,
        |dest, bytes| {
            chunk_index += 1;
            if chunk_index == 1 {
                assert_eq!(dest, 0x2000);
                assert_eq!(bytes.len(), VFS_IPC_PAYLOAD_CAPACITY);
                Ok(())
            } else {
                Err(LINUX_EFAULT)
            }
        },
    );
    assert_eq!(result, VFS_IPC_PAYLOAD_CAPACITY as u64);
    assert_eq!(remote.cursor(), VFS_IPC_PAYLOAD_CAPACITY as u64);
    assert_eq!(remote.durable_cursor(), 0);
    assert!(remote.is_dirty(4));
    assert_eq!(
        transport.ops,
        alloc::vec![
            (VFS_IPC_OP_PREAD64, 0, VFS_IPC_PAYLOAD_CAPACITY),
            (
                VFS_IPC_OP_PREAD64,
                VFS_IPC_PAYLOAD_CAPACITY as u64,
                VFS_IPC_PAYLOAD_CAPACITY
            ),
        ]
    );
}

#[test]
fn regular_file_bulk_read_partial_fault_and_eopnotsupp_fallback_keep_exact_cursor() {
    let bulk_remote = multitask::RemoteVfsHandle::new_with_epoch(
        79,
        multitask::RemoteVfsHandleKind::File,
        alloc::string::String::from("/data/bulk.bin"),
        512 * 1024,
        0,
        2,
    )
    .expect("remote handle");
    let mut bulk_transport = MockRegularFileReadTransport::new(512 * 1024);
    let mut calls = 0usize;
    let partial = read_remote_vfs_file_loop(
        Some(bulk_remote.description().as_ref()),
        bulk_remote.cursor(),
        0x4000,
        VFS_BULK_READ_MIN_BYTES,
        &mut bulk_transport,
        |_, _| {
            calls += 1;
            if calls <= 2 {
                Ok(())
            } else {
                Err(LINUX_EFAULT)
            }
        },
    );
    assert_eq!(partial, (2 * VFS_IPC_PAYLOAD_CAPACITY) as u64);
    assert_eq!(bulk_remote.cursor(), (2 * VFS_IPC_PAYLOAD_CAPACITY) as u64);
    assert!(bulk_remote.is_dirty(2));
    assert_eq!(
        bulk_transport.ops,
        alloc::vec![(VFS_IPC_OP_BULK_READ, 0, VFS_BULK_READ_MIN_BYTES)]
    );

    let fallback_remote = multitask::RemoteVfsHandle::new_with_epoch(
        80,
        multitask::RemoteVfsHandleKind::File,
        alloc::string::String::from("/proc/1/stat"),
        (VFS_BULK_READ_MIN_BYTES + 128) as u64,
        0,
        2,
    )
    .expect("remote handle");
    let mut fallback_transport = MockRegularFileReadTransport::new(VFS_BULK_READ_MIN_BYTES + 128);
    fallback_transport.bulk_error = Some(LINUX_EOPNOTSUPP);
    let fallback_read = read_remote_vfs_file_loop(
        Some(fallback_remote.description().as_ref()),
        fallback_remote.cursor(),
        0x8000,
        VFS_BULK_READ_MIN_BYTES + 128,
        &mut fallback_transport,
        |_, _| Ok(()),
    );
    assert_eq!(fallback_read, (VFS_BULK_READ_MIN_BYTES + 128) as u64);
    assert_eq!(
        fallback_remote.cursor(),
        (VFS_BULK_READ_MIN_BYTES + 128) as u64
    );
    assert_eq!(fallback_transport.ops[0].0, VFS_IPC_OP_BULK_READ);
    assert!(
        fallback_transport.ops[1..]
            .iter()
            .all(|(op, _, _)| *op == VFS_IPC_OP_PREAD64),
        "after EOPNOTSUPP, remaining chunks must stay on PREAD64 without re-probing BULK_READ"
    );
}

#[test]
fn remote_vfs_cursor_dirty_tracking_tracks_reads_settle_sync_and_epoch_restarts() {
    let remote = multitask::RemoteVfsHandle::new_with_epoch(
        81,
        multitask::RemoteVfsHandleKind::File,
        alloc::string::String::from("/data/sync.bin"),
        1024,
        0,
        10,
    )
    .expect("remote handle");
    assert!(!remote.is_dirty(10));
    assert!(remote.is_dirty(11));

    remote.advance_cursor(256);
    assert_eq!(remote.cursor(), 256);
    assert_eq!(remote.durable_cursor(), 0);
    assert!(remote.is_dirty(10));

    remote.mark_settled(256, 10);
    assert!(!remote.is_dirty(10));
    assert!(remote.is_dirty(11));

    remote.set_synced_cursor(512, 11);
    assert_eq!(remote.cursor(), 512);
    assert_eq!(remote.durable_cursor(), 512);
    assert_eq!(remote.durable_epoch(), 11);
    assert!(!remote.is_dirty(11));
}
