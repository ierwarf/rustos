use super::*;

#[test]
fn descriptor_exhaustion_is_not_reported_as_a_bad_source_fd() {
    assert_eq!(duplicate_install_errno(VfsDupMode::Dup), LINUX_EMFILE);
    assert_eq!(duplicate_install_errno(VfsDupMode::Dup2), LINUX_EBADF);
    assert_eq!(duplicate_install_errno(VfsDupMode::Dup3), LINUX_EBADF);
}

#[test]
fn netd_reference_mutation_owns_the_complete_interactive_deadline() {
    assert_eq!(
        NETD_REF_OPERATION_TIMEOUT_MS,
        rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
    );
    assert_eq!(
        super::ipc_helpers::deadline::remaining_service_timeout_ms(
            NETD_REF_OPERATION_TIMEOUT_MS,
            0,
        ),
        Some(100)
    );
    assert!(
        !super::ipc_helpers::deadline::retryable_early_service_transport_error(LINUX_ETIMEDOUT)
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
            start_ns + 99 * super::vfs_meta::NETD_NANOS_PER_MILLI,
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
