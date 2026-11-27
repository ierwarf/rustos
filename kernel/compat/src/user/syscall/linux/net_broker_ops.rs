use super::*;

use rustos_user_abi::syscall::{
    NET_BROKER_OP_PACKET_LEASE_GRANT, NET_BROKER_OP_PACKET_LEASE_RESET,
    NET_BROKER_OP_PACKET_LEASE_REVOKE, NET_BROKER_OP_PACKET_RX, NET_BROKER_OP_PACKET_STATUS,
    NET_BROKER_OP_PACKET_TX, NET_BROKER_OP_PACKET_WAIT_RX, NET_BROKER_OP_PREPARE_SOCKET_PUBLICATION,
    NET_BROKER_PACKET_WAIT_MAX_MS,
    NetBrokerPrepareSocketPublication, RustosNetBrokerArgs,
    ServiceCapability, net_broker_socket_publication_shape_valid,
};

pub(super) fn syscall_linux_rustos_net_broker(args_ptr: u64) -> u64 {
    linux_result((|| {
        let args: RustosNetBrokerArgs =
            require_capability_and_read_args(ServiceCapability::NetPolicy, args_ptr)?;
        if args.process_id == 0 || args.reserved0 != 0 || args.reserved1 != 0 {
            return Err(LINUX_EINVAL);
        }
        dispatch_net_broker(&args)
    })())
}

fn dispatch_net_broker(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    match args.op {
        NET_BROKER_OP_PREPARE_SOCKET_PUBLICATION => broker_prepare_socket_publication(args),
        NET_BROKER_OP_PACKET_STATUS => broker_packet_status(),
        NET_BROKER_OP_PACKET_TX => broker_packet_tx(args),
        NET_BROKER_OP_PACKET_RX => broker_packet_rx(args),
        NET_BROKER_OP_PACKET_WAIT_RX => broker_packet_wait_rx(args),
        NET_BROKER_OP_PACKET_LEASE_GRANT => broker_packet_lease(args, true),
        NET_BROKER_OP_PACKET_LEASE_REVOKE => broker_packet_lease(args, false),
        NET_BROKER_OP_PACKET_LEASE_RESET => broker_packet_lease_reset(args),
        _ => Err(LINUX_EINVAL),
    }
}

/// Moves initial netd socket references into the exact reply message.
/// There is intentionally no target-process fd installation in this broker
/// path: only the caller-side timely reply take publishes a descriptor.
fn broker_prepare_socket_publication(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    if args.arg0 == 0
        || args.arg1 != 0
        || args.arg2 != 0
        || args.arg3 != 0
        || args.arg4 != 0
        || args.arg5 != 0
    {
        return Err(LINUX_EINVAL);
    }
    let publication =
        usermem::read_current_user_struct::<NetBrokerPrepareSocketPublication>(args.arg0)
            .map_err(user_pointer_error_to_linux_errno)?;
    if !net_broker_socket_publication_shape_valid(&publication)
        || publication.caller_pid != args.process_id
    {
        return Err(LINUX_EINVAL);
    }
    let netd_process_id = multitask::current_user_process_id().ok_or(LINUX_EPERM)?;
    let (_, epoch) = ipc_ops::live_service_endpoint_owner_and_epoch(IPC_SERVICE_NETD)
        .filter(|(owner, _)| *owner == netd_process_id).ok_or(LINUX_EPERM)?;
    service_ops::vfs_socket::reserve_initial_netd_socket_ref(publication.socket_token, epoch)?;
    let descriptor = match multitask::register_new_inet_socket_transfer(
            publication.socket_token,
            publication.domain,
            publication.socket_type,
            publication.protocol,
        ) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            service_ops::vfs_socket::cancel_initial_netd_socket_ref(publication.socket_token, epoch);
            return Err(transfer_registry_error_to_linux_errno(error));
        }
    };
    let descriptors = alloc::vec![descriptor];
    let reply = kernel_ipc_runtime::api::KernelReplyHandle::from_raw(publication.reply_cap);
    if let Err(error) = kernel_ipc_runtime::api::endpoint::bind_prepared_reply_handles_for_process(
        reply,
        netd_process_id,
        descriptors,
    ) {
        // LIFECYCLE: This only removes the non-visible registry entry.  Netd
        // still owns the provider's initial reference and performs the sole
        // unpublished-token discard after this broker call fails.
        multitask::reclaim_unbound_socket_transfers(&error.handles);
        service_ops::vfs_socket::cancel_initial_netd_socket_ref(publication.socket_token, epoch);
        return Err(ipc_error_to_linux_errno(error.error));
    }
    Ok(0)
}

fn transfer_registry_error_to_linux_errno(error: multitask::IpcTransferRegistryError) -> i64 {
    match error {
        multitask::IpcTransferRegistryError::Exhausted => LINUX_ENOMEM,
        multitask::IpcTransferRegistryError::BindingMismatch
        | multitask::IpcTransferRegistryError::InvalidDescriptor => LINUX_EINVAL,
        multitask::IpcTransferRegistryError::InvalidState
        | multitask::IpcTransferRegistryError::StaleDescriptor => LINUX_ESTALE,
    }
}

fn ipc_error_to_linux_errno(error: kernel_ipc_runtime::api::IpcError) -> i64 {
    match error {
        kernel_ipc_runtime::api::IpcError::InvalidHandle
        | kernel_ipc_runtime::api::IpcError::InvalidArgument => LINUX_EINVAL,
        kernel_ipc_runtime::api::IpcError::PermissionDenied => LINUX_EPERM,
        kernel_ipc_runtime::api::IpcError::PeerClosed => LINUX_EPIPE,
        kernel_ipc_runtime::api::IpcError::BufferTooSmall => LINUX_EOVERFLOW,
        kernel_ipc_runtime::api::IpcError::NoMemory => LINUX_ENOMEM,
    }
}

/// Snapshot authority before waiting for the transport lock, then revalidate
/// its exact owner/epoch inside that lock. The entry capability check alone
/// cannot authorize an operation that sleeps across netd replacement.
fn packet_authority() -> Result<impl Fn() -> bool, i64> {
    let pid = multitask::current_user_process_id().ok_or(LINUX_ESRCH)?;
    let expected = ipc_ops::live_service_endpoint_owner_and_epoch(linux_abi::IPC_SERVICE_NETD)
        .filter(|(owner, _)| *owner == pid)
        .ok_or(LINUX_EPERM)?;
    Ok(move || ipc_ops::live_service_endpoint_owner_and_epoch(linux_abi::IPC_SERVICE_NETD) == Some(expected))
}

fn broker_packet_lease_reset(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    if args.arg0 != 0
        || args.arg1 != 0
        || args.arg2 != 0
        || args.arg3 != 0
        || args.arg4 != 0
        || args.arg5 != 0
    {
        return Err(LINUX_EINVAL);
    }
    kernel_io_manager::api::network::reset_dvm_transport_lease(packet_authority()?)
        .then_some(0).ok_or(LINUX_ESTALE)
}

fn broker_packet_lease(args: &RustosNetBrokerArgs, grant: bool) -> Result<u64, i64> {
    let generation = u32::try_from(args.arg0).map_err(|_| LINUX_EINVAL)?;
    if generation == 0
        || args.arg1 != 0
        || args.arg2 != 0
        || args.arg3 != 0
        || args.arg4 != 0
        || args.arg5 != 0
    {
        return Err(LINUX_EINVAL);
    }
    let authority = packet_authority()?;
    let changed = if grant {
        kernel_io_manager::api::network::grant_dvm_transport_lease(generation, authority)
    } else {
        kernel_io_manager::api::network::revoke_dvm_transport_lease(generation, authority)
    };
    if changed { Ok(1) } else { Err(LINUX_ESTALE) }
}

fn broker_packet_status() -> Result<u64, i64> {
    kernel_io_manager::api::network::transport_status(packet_authority()?)
        .map(|status| status as u64).map_err(packet_error_to_linux_errno)
}

fn broker_packet_tx(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let len = usize::try_from(args.arg1).map_err(|_| LINUX_EINVAL)?;
    if len > kernel_io_manager::api::network::PACKET_MTU {
        return Err(LINUX_EMSGSIZE);
    }
    if len == 0 {
        return Ok(0);
    }
    let mut frame = [0_u8; kernel_io_manager::api::network::PACKET_MTU];
    read_user_bytes(args.arg0, &mut frame[..len])?;
    kernel_io_manager::api::network::transmit_frame(&frame[..len], packet_authority()?)
        .map(|count| count as u64)
        .map_err(packet_error_to_linux_errno)
}

fn broker_packet_wait_rx(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    if !(1..=NET_BROKER_PACKET_WAIT_MAX_MS).contains(&args.arg0)
        || [args.arg1, args.arg2, args.arg3, args.arg4, args.arg5] != [0; 5]
    {
        return Err(LINUX_EINVAL);
    }
    kernel_io_manager::api::network::wait_receive(args.arg0, packet_authority()?)
        .map(|()| 0)
        .map_err(packet_error_to_linux_errno)
}

fn broker_packet_rx(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let cap = usize::try_from(args.arg1).map_err(|_| LINUX_EINVAL)?;
    if cap == 0 || cap > kernel_io_manager::api::network::PACKET_MTU {
        return Err(LINUX_EINVAL);
    }
    // receive_frame dequeues; refuse a bad destination before the frame is lost.
    validate_user_write(args.arg0, cap)?;
    let mut frame = [0_u8; kernel_io_manager::api::network::PACKET_MTU];
    let count = kernel_io_manager::api::network::receive_frame(&mut frame[..cap], packet_authority()?)
        .map_err(packet_error_to_linux_errno)?;
    let received = frame.get(..count).ok_or(LINUX_EIO)?;
    write_user_bytes(args.arg0, received)?;
    Ok(count as u64)
}

fn packet_error_to_linux_errno(err: kernel_io_manager::api::network::PacketError) -> i64 {
    match err {
        kernel_io_manager::api::network::PacketError::Unsupported => LINUX_EOPNOTSUPP,
        kernel_io_manager::api::network::PacketError::NoDevice => LINUX_ENODEV,
        kernel_io_manager::api::network::PacketError::Invalid => LINUX_EINVAL,
        kernel_io_manager::api::network::PacketError::Busy => LINUX_EAGAIN,
        kernel_io_manager::api::network::PacketError::TooLarge => LINUX_EMSGSIZE,
        kernel_io_manager::api::network::PacketError::WouldBlock => LINUX_EAGAIN,
        kernel_io_manager::api::network::PacketError::Revoked => LINUX_ESTALE,
    }
}
