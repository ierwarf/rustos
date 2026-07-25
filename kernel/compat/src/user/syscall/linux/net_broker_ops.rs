use super::*;

use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_NET_POLICY, NET_BROKER_OP_PACKET_LEASE_GRANT, NET_BROKER_OP_PACKET_LEASE_RESET,
    NET_BROKER_OP_PACKET_LEASE_REVOKE, NET_BROKER_OP_PACKET_RX, NET_BROKER_OP_PACKET_STATUS,
    NET_BROKER_OP_PACKET_TX, RustosNetBrokerArgs,
};
use x86_64::VirtAddr;

pub(super) fn syscall_linux_rustos_net_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_NET_POLICY) {
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
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET => broker_socket(args),
        SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR => broker_socketpair(args),
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT => broker_accept(args),
        NET_BROKER_OP_PACKET_STATUS => broker_packet_status(),
        NET_BROKER_OP_PACKET_TX => broker_packet_tx(args),
        NET_BROKER_OP_PACKET_RX => broker_packet_rx(args),
        NET_BROKER_OP_PACKET_LEASE_GRANT => broker_packet_lease(args, true),
        NET_BROKER_OP_PACKET_LEASE_REVOKE => broker_packet_lease(args, false),
        NET_BROKER_OP_PACKET_LEASE_RESET => broker_packet_lease_reset(args),
        _ => Err(LINUX_EINVAL),
    }
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
    kernel_io_manager::api::network::reset_dvm_transport_lease();
    Ok(0)
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
    let changed = if grant {
        kernel_io_manager::api::network::grant_dvm_transport_lease(generation)
    } else {
        kernel_io_manager::api::network::revoke_dvm_transport_lease(generation)
    };
    if changed { Ok(1) } else { Err(LINUX_ESTALE) }
}

fn broker_packet_status() -> Result<u64, i64> {
    Ok(kernel_io_manager::api::network::transport_status() as u64)
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
    kernel_io_manager::api::network::transmit_frame(frame.as_slice())
        .map(|count| count as u64)
        .map_err(packet_error_to_linux_errno)
}

fn broker_packet_rx(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let cap = usize::try_from(args.arg1).map_err(|_| LINUX_EINVAL)?;
    if cap == 0 || cap > kernel_io_manager::api::network::PACKET_MTU {
        return Err(LINUX_EINVAL);
    }
    let mut frame = alloc::vec![0_u8; cap];
    let count = kernel_io_manager::api::network::receive_frame(frame.as_mut_slice())
        .map_err(packet_error_to_linux_errno)?;
    usermem::write_current_user_bytes(args.arg0, &frame[..count])
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
    }
}

fn broker_socket(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let domain = args.arg0;
    let socket_type = args.arg1;
    let protocol = args.arg2;
    let base_type = socket_type & linux_abi::SOCK_TYPE_MASK;
    let open_flags = socket_type & (linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC);

    let handle = match (domain, base_type) {
        (linux_abi::AF_UNIX, linux_abi::SOCK_STREAM) => {
            if args.arg3 == 0 {
                return Err(LINUX_EINVAL);
            }
            multitask::KernelHandle::Socket(multitask::SocketHandle::from_token(
                args.arg3, domain, base_type, protocol,
            ))
        }
        (linux_abi::AF_INET, linux_abi::SOCK_STREAM)
        | (linux_abi::AF_INET, linux_abi::SOCK_DGRAM) => {
            let handle = if args.arg3 != 0 {
                multitask::InetSocketHandle::from_token(args.arg3, domain, base_type, protocol)
            } else {
                multitask::InetSocketHandle::new(domain, base_type, protocol)
                    .ok_or(LINUX_EOVERFLOW)?
            };
            multitask::KernelHandle::InetSocket(handle)
        }
        _ => return Err(LINUX_EAFNOSUPPORT),
    };

    install_process_handle(args.process_id, handle, open_flags)
}

fn broker_socketpair(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    if args.arg0 != linux_abi::AF_UNIX
        || args.arg1 & linux_abi::SOCK_TYPE_MASK != linux_abi::SOCK_STREAM
        || args.arg4 == 0
        || args.arg5 == 0
    {
        return Err(LINUX_EAFNOSUPPORT);
    }
    let open_flags = args.arg1 & (linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC);
    install_process_socket_pair(
        args,
        multitask::KernelHandle::Socket(multitask::SocketHandle::from_token(
            args.arg4,
            linux_abi::AF_UNIX,
            linux_abi::SOCK_STREAM,
            args.arg2,
        )),
        multitask::KernelHandle::Socket(multitask::SocketHandle::from_token(
            args.arg5,
            linux_abi::AF_UNIX,
            linux_abi::SOCK_STREAM,
            args.arg2,
        )),
        open_flags,
    )
}

fn broker_accept(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let status_flags = process_socket_status_flags(args.process_id, args.arg0)?;
    if args.arg4 == 0 {
        return Err(LINUX_EINVAL);
    }
    install_process_handle(
        args.process_id,
        multitask::KernelHandle::Socket(multitask::SocketHandle::from_token(
            args.arg4,
            linux_abi::AF_UNIX,
            linux_abi::SOCK_STREAM,
            0,
        )),
        (status_flags & linux_abi::O_NONBLOCK)
            | (args.arg3 & (linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC)),
    )
}

fn process_socket_status_flags(process_id: u64, fd: u64) -> Result<u64, i64> {
    let Some(result) = multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        let Some(entry) = process_state.handles().get_entry(fd) else {
            return Err(LINUX_EBADF);
        };
        match entry.handle() {
            multitask::KernelHandle::Socket(_) => Ok(entry.status_flags()),
            multitask::KernelHandle::InetSocket(_) => Ok(entry.status_flags()),
            _ => Err(LINUX_ENOTSOCK),
        }
    }) else {
        return Err(LINUX_ESRCH);
    };
    result
}

fn install_process_handle(
    process_id: u64,
    handle: multitask::KernelHandle,
    open_flags: u64,
) -> Result<u64, i64> {
    let Some(fd) = multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        process_state
            .handles_mut()
            .install_with_open_flags(handle, open_flags)
    }) else {
        return Err(LINUX_ESRCH);
    };
    fd.ok_or(LINUX_EMFILE)
}

fn install_process_socket_pair(
    args: &RustosNetBrokerArgs,
    left: multitask::KernelHandle,
    right: multitask::KernelHandle,
    open_flags: u64,
) -> Result<u64, i64> {
    if args.arg3 == 0 {
        return Err(LINUX_EINVAL);
    }
    let Some(result) = multitask::with_process_state_by_pid_mut(args.process_id, |process_state| {
        process_state
            .address_space()
            .validate_user_write_buffer(VirtAddr::new(args.arg3), 8)
            .map_err(address_space_error_to_linux_errno)?;
        if !process_state.handles().can_install_additional(2) {
            return Err(LINUX_EMFILE);
        }
        let Some(left_fd) = process_state
            .handles_mut()
            .install_with_open_flags(left, open_flags)
        else {
            return Err(LINUX_EMFILE);
        };
        let Some(right_fd) = process_state
            .handles_mut()
            .install_with_open_flags(right, open_flags)
        else {
            let _ = process_state.handles_mut().close(left_fd);
            return Err(LINUX_EMFILE);
        };
        let (Ok(left_fd_i32), Ok(right_fd_i32)) = (i32::try_from(left_fd), i32::try_from(right_fd))
        else {
            let _ = process_state.handles_mut().close(left_fd);
            let _ = process_state.handles_mut().close(right_fd);
            return Err(LINUX_EMFILE);
        };
        let mut bytes = [0_u8; 8];
        bytes[..4].copy_from_slice(&left_fd_i32.to_ne_bytes());
        bytes[4..].copy_from_slice(&right_fd_i32.to_ne_bytes());
        if let Err(error) = process_state
            .address_space()
            .copy_into_user(VirtAddr::new(args.arg3), &bytes)
        {
            let _ = process_state.handles_mut().close(left_fd);
            let _ = process_state.handles_mut().close(right_fd);
            return Err(address_space_error_to_linux_errno(error));
        }
        Ok(0)
    }) else {
        return Err(LINUX_ESRCH);
    };
    result
}
