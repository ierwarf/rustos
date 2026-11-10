use super::*;

use core::sync::atomic::{AtomicU64, Ordering};
use rustos_user_abi::syscall::{
    NET_BROKER_OP_PACKET_LEASE_GRANT, NET_BROKER_OP_PACKET_LEASE_RESET,
    NET_BROKER_OP_PACKET_LEASE_REVOKE, NET_BROKER_OP_PACKET_RX, NET_BROKER_OP_PACKET_STATUS,
    NET_BROKER_OP_PACKET_TX, NET_BROKER_OP_PREPARE_SOCKET_PUBLICATION,
    NET_BROKER_OP_RELEASE_SOCKET_CONTROL, NET_BROKER_OP_UNIX_CONNECT_BIND,
    NET_BROKER_SOCKET_CONTROL_CAPACITY, NetBrokerPrepareSocketPublication, RustosNetBrokerArgs,
    ServiceCapability, net_broker_socket_publication_shape_valid,
};

pub(super) fn syscall_linux_rustos_net_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(ServiceCapability::NetPolicy) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<RustosNetBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.process_id == 0 || args.reserved0 != 0 || args.reserved1 != 0 {
        return linux_errno(LINUX_EINVAL);
    }

    match dispatch_net_broker(&args) {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

fn dispatch_net_broker(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    match args.op {
        NET_BROKER_OP_UNIX_CONNECT_BIND => broker_unix_connect_bind(args),
        NET_BROKER_OP_RELEASE_SOCKET_CONTROL => broker_release_socket_control(args),
        NET_BROKER_OP_PREPARE_SOCKET_PUBLICATION => broker_prepare_socket_publication(args),
        NET_BROKER_OP_PACKET_STATUS => broker_packet_status(),
        NET_BROKER_OP_PACKET_TX => broker_packet_tx(args),
        NET_BROKER_OP_PACKET_RX => broker_packet_rx(args),
        NET_BROKER_OP_PACKET_LEASE_GRANT => broker_packet_lease(args, true),
        NET_BROKER_OP_PACKET_LEASE_REVOKE => broker_packet_lease(args, false),
        NET_BROKER_OP_PACKET_LEASE_RESET => broker_packet_lease_reset(args),
        _ => Err(LINUX_EINVAL),
    }
}

/// Moves initial netd socket/accept references into the exact reply
/// message.  There is intentionally no target-process fd installation in this
/// broker path: only the caller-side timely reply take publishes a descriptor.
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
            .map_err(address_space_error_to_linux_errno)?;
    if !net_broker_socket_publication_shape_valid(&publication)
        || publication.caller_pid != args.process_id
    {
        return Err(LINUX_EINVAL);
    }
    let netd_process_id = multitask::current_user_process_id().ok_or(LINUX_EPERM)?;
    let descriptors = if publication.domain == linux_abi::AF_INET {
        alloc::vec![
            multitask::register_new_inet_socket_transfer(
                publication.socket_token,
                publication.domain,
                publication.socket_type,
                publication.protocol,
            )
            .map_err(transfer_registry_error_to_linux_errno)?
        ]
    } else {
        let channel = if publication.operation == SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR as u64 {
            allocate_unix_channel_id()?
        } else {
            publication.channel_id
        };
        multitask::register_ipc_transfer_entries(initial_unix_publication_entries(
            &publication,
            channel,
        )?)
        .map_err(transfer_registry_error_to_linux_errno)?
    };
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

/// NetPolicy owns the opaque tickets it withdraws from its queues. Decode the
/// entire bounded block before transferring any custody; never synchronously
/// call a provider from this broker (in particular, never reenter netd).
fn broker_release_socket_control(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let len = usize::try_from(args.arg1).map_err(|_| LINUX_EINVAL)?;
    if args.arg0 == 0
        || len == 0
        || len > NET_BROKER_SOCKET_CONTROL_CAPACITY
        || args.arg2 != 0
        || args.arg3 != 0
        || args.arg4 != 0
        || args.arg5 != 0
    {
        return Err(LINUX_EINVAL);
    }
    let mut control = alloc::vec![0;len];
    usermem::copy_from_current_user_exact(args.arg0, &mut control)
        .map_err(address_space_error_to_linux_errno)?;
    defer_socket_control_release(&control)?;
    Ok(0)
}

fn defer_socket_control_release(control: &[u8]) -> Result<(), i64> {
    if control.is_empty() || control.len() > NET_BROKER_SOCKET_CONTROL_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let tickets = service_ops::vfs_meta::decode_transfer_release_tickets(control)?;
    multitask::drop_ipc_transfer_tickets(&tickets);
    Ok(())
}

/// Snapshot authority before waiting for the transport lock, then revalidate
/// its exact owner/epoch inside that lock. The entry capability check alone
/// cannot authorize an operation that sleeps across netd replacement.
fn packet_authority() -> Result<impl FnOnce() -> bool, i64> {
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
    let mut frame = alloc::vec![0_u8; len];
    usermem::copy_from_current_user_exact(args.arg0, frame.as_mut_slice())
        .map_err(address_space_error_to_linux_errno)?;
    kernel_io_manager::api::network::transmit_frame(frame.as_slice(), packet_authority()?)
        .map(|count| count as u64)
        .map_err(packet_error_to_linux_errno)
}

fn broker_packet_rx(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let cap = usize::try_from(args.arg1).map_err(|_| LINUX_EINVAL)?;
    if cap == 0 || cap > kernel_io_manager::api::network::PACKET_MTU {
        return Err(LINUX_EINVAL);
    }
    // receive_frame dequeues; refuse a bad destination before the frame is lost.
    usermem::validate_current_user_write_buffer(args.arg0, cap)
        .map_err(address_space_error_to_linux_errno)?;
    let mut frame = alloc::vec![0_u8; cap];
    let count = kernel_io_manager::api::network::receive_frame(frame.as_mut_slice(), packet_authority()?)
        .map_err(packet_error_to_linux_errno)?;
    let received = frame.get(..count).ok_or(LINUX_EIO)?;
    usermem::write_current_user_bytes(args.arg0, received)
        .map_err(address_space_error_to_linux_errno)?;
    Ok(count as u64)
}

fn packet_error_to_linux_errno(err: kernel_io_manager::api::network::PacketError) -> i64 {
    match err {
        kernel_io_manager::api::network::PacketError::NoDevice => LINUX_ENODEV,
        kernel_io_manager::api::network::PacketError::Invalid => LINUX_EINVAL,
        kernel_io_manager::api::network::PacketError::Busy => LINUX_EAGAIN,
        kernel_io_manager::api::network::PacketError::TooLarge => LINUX_EMSGSIZE,
        kernel_io_manager::api::network::PacketError::WouldBlock => LINUX_EAGAIN,
        kernel_io_manager::api::network::PacketError::Revoked => LINUX_ESTALE,
    }
}

/// Construct invisible initial references, never dup an installed fd. Pair
/// registration is atomic; a failed bind leaves both references with netd.
fn initial_unix_publication_entries(
    publication: &NetBrokerPrepareSocketPublication,
    channel: u64,
) -> Result<Vec<multitask::TransferredHandleEntry>, i64> {
    let pair = publication.operation == SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR as u64;
    let mut entries = Vec::with_capacity(if pair { 2 } else { 1 });
    for (token, side) in [
        (publication.socket_token, if pair { 1 } else { 2 }),
        (publication.second_socket_token, 2),
    ] {
        if token == 0 {
            continue;
        }
        let socket = multitask::SocketHandle::from_token(
            token,
            publication.domain,
            linux_abi::SOCK_STREAM,
            publication.protocol,
        );
        if channel != 0 && !socket.bind_channel(channel, side) {
            return Err(LINUX_EINVAL);
        }
        let fd_flags = if publication.socket_type & linux_abi::SOCK_CLOEXEC != 0 {
            multitask::FD_CLOEXEC
        } else {
            0
        };
        let entry = multitask::HandleEntry::new(
            multitask::KernelHandle::Socket(socket),
            fd_flags,
            publication.socket_type & linux_abi::SOCK_NONBLOCK,
        );
        entries.push(
            multitask::TransferredHandleEntry::from_initial_entry(entry).ok_or(LINUX_EINVAL)?,
        );
    }
    Ok(entries)
}

fn broker_unix_connect_bind(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    if args.arg1 == 0 || args.arg2 == 0 || args.arg3 != 0 || args.arg4 != 0 || args.arg5 != 0 {
        return Err(LINUX_EINVAL);
    }
    let channel_id = allocate_unix_channel_id()?;
    let Some(bound) = multitask::with_process_state_by_pid_mut(args.process_id, |process_state| {
        let Some(entry) = process_state.handles_mut().get_entry_mut(args.arg0) else {
            return false;
        };
        let multitask::KernelHandle::Socket(socket) = entry.handle_mut() else {
            return false;
        };
        socket.token_id() == args.arg1 && socket.bind_channel(channel_id, 1)
    }) else {
        return Err(LINUX_ESRCH);
    };
    bound.then_some(channel_id).ok_or(LINUX_EINVAL)
}

fn allocate_unix_channel_id() -> Result<u64, i64> {
    static NEXT_UNIX_CHANNEL_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_UNIX_CHANNEL_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0).then(|| current.checked_add(1)).flatten()
        })
        .map_err(|_| LINUX_EOVERFLOW)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initial_unix_publication_entries_are_invisible_exact_channel_sides() {
        for (op, second, channel) in [
            (SYSCALL_OFFLOAD_OP_LINUX_SOCKET, 0, 0),
            (SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR, 72, 99),
            (SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, 0, 99),
        ] {
            let publication = NetBrokerPrepareSocketPublication {
                version: rustos_user_abi::syscall::NET_BROKER_SOCKET_PUBLICATION_VERSION,
                operation: op as u64,
                reply_cap: 11,
                caller_pid: 12,
                socket_token: 71,
                second_socket_token: second,
                channel_id: if op == SYSCALL_OFFLOAD_OP_LINUX_ACCEPT {
                    channel
                } else {
                    0
                },
                domain: linux_abi::AF_UNIX,
                socket_type: linux_abi::SOCK_STREAM | linux_abi::SOCK_CLOEXEC,
                ..NetBrokerPrepareSocketPublication::default()
            };
            assert!(net_broker_socket_publication_shape_valid(&publication));
            let entries = initial_unix_publication_entries(&publication, channel).unwrap();
            assert_eq!(entries.len(), if second != 0 { 2 } else { 1 });
            for (index, entry) in entries.iter().enumerate() {
                let multitask::KernelHandle::Socket(socket) = entry.entry().handle() else {
                    panic!("Unix socket")
                };
                assert_eq!(socket.token_id(), 71 + index as u64);
                assert_eq!(socket.channel_id(), channel);
                assert_eq!(
                    socket.channel_side(),
                    if channel == 0 {
                        0
                    } else if second == 0 {
                        2
                    } else {
                        index as u8 + 1
                    }
                );
                assert_eq!(entry.entry().fd_flags(), multitask::FD_CLOEXEC);
            }
            let descriptors = multitask::register_ipc_transfer_entries(entries).unwrap();
            multitask::reclaim_unbound_socket_transfers(&descriptors);
            assert!(
                multitask::take_ipc_transfer_entries(&descriptors).is_err(),
                "failed bind must reclaim every invisible descriptor"
            );
        }
    }
    #[test]
    fn retired_accept_control_is_deferred_once_without_any_caller_reply() {
        use kernel_ipc_runtime::api::{
            ChannelIdentity, ProcessIdentity, ServiceIdentity, TransferContext,
        };
        let token = u64::MAX - 9876;
        let entry =
            multitask::TransferredHandleEntry::from_initial_entry(multitask::HandleEntry::new(
                multitask::KernelHandle::Socket(multitask::SocketHandle::from_token(
                    token,
                    linux_abi::AF_UNIX,
                    linux_abi::SOCK_STREAM,
                    0,
                )),
                0,
                0,
            ))
            .unwrap();
        let descriptors = multitask::register_ipc_transfer_entries(alloc::vec![entry]).unwrap();
        let tickets = multitask::bind_ipc_transfer_tickets(
            &descriptors,
            TransferContext {
                source: ProcessIdentity {
                    pid: 71,
                    generation: 1,
                },
                service: ServiceIdentity {
                    service_id: linux_abi::IPC_SERVICE_NETD,
                    epoch: 1,
                },
                channel: ChannelIdentity {
                    channel_id: 99,
                    generation: 99,
                    receiver_side: 2,
                },
                stream_start: 0,
                stream_end: 1,
                intended_receiver: None,
                receiver_open_description: 0,
            },
        )
        .unwrap();
        multitask::commit_ipc_transfer_enqueue(&tickets).unwrap();
        let mut control = alloc::vec![0;40];
        control[..8].copy_from_slice(&40_u64.to_ne_bytes());
        control[8..12].copy_from_slice(&(linux_abi::SOL_SOCKET as u32).to_ne_bytes());
        control[12..16].copy_from_slice(&(linux_abi::SCM_RIGHTS as u32).to_ne_bytes());
        control[16..24].copy_from_slice(&tickets[0].transfer_id().to_le_bytes());
        control[24..32].copy_from_slice(&tickets[0].nonce().to_le_bytes());
        control[32..40].copy_from_slice(&tickets[0].batch_generation().to_le_bytes());
        let mut malformed = control.clone();
        malformed.push(0); // Trailing bytes must reject before dropping anything.
        assert_eq!(defer_socket_control_release(&malformed), Err(LINUX_EINVAL));
        assert!(multitask::take_deferred_ipc_transfer_drops(1).is_empty());
        defer_socket_control_release(&control).unwrap();
        assert!(multitask::take_ipc_transfer_entries(&descriptors).is_err());
        let released = multitask::take_deferred_ipc_transfer_drops(1);
        assert_eq!(released.len(), 1);
        let multitask::KernelHandle::Socket(socket) = released[0].entry().handle() else {
            panic!("Unix socket")
        };
        assert_eq!(socket.token_id(), token);
        defer_socket_control_release(&control).unwrap();
        assert!(multitask::take_deferred_ipc_transfer_drops(1).is_empty());
    }
}
