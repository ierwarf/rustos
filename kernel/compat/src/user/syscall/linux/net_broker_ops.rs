use super::*;

use rustos_user_abi::syscall::{IPC_SERVICE_CAP_NET_POLICY, RustosNetBrokerArgs};
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
        _ => Err(LINUX_EINVAL),
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
        | (linux_abi::AF_INET, linux_abi::SOCK_DGRAM) => multitask::KernelHandle::InetSocket(
            multitask::InetSocketHandle::new(domain, base_type, protocol),
        ),
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
    let left_fd = install_process_handle(
        args.process_id,
        multitask::KernelHandle::Socket(multitask::SocketHandle::from_token(
            args.arg4,
            linux_abi::AF_UNIX,
            linux_abi::SOCK_STREAM,
            args.arg2,
        )),
        open_flags,
    )?;
    let right_fd = install_process_handle(
        args.process_id,
        multitask::KernelHandle::Socket(multitask::SocketHandle::from_token(
            args.arg5,
            linux_abi::AF_UNIX,
            linux_abi::SOCK_STREAM,
            args.arg2,
        )),
        open_flags,
    )?;
    write_process_i32_pair(args.process_id, args.arg3, left_fd as i32, right_fd as i32)?;
    Ok(0)
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
    multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        process_state
            .handles_mut()
            .install_with_open_flags(handle, open_flags)
    })
    .ok_or(LINUX_ESRCH)
}

fn write_process_i32_pair(process_id: u64, ptr: u64, left: i32, right: i32) -> Result<(), i64> {
    if ptr == 0 {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = [0_u8; 8];
    bytes[..4].copy_from_slice(&left.to_ne_bytes());
    bytes[4..].copy_from_slice(&right.to_ne_bytes());
    write_process_bytes(process_id, ptr, &bytes)
}

fn write_process_bytes(process_id: u64, ptr: u64, bytes: &[u8]) -> Result<(), i64> {
    if ptr == 0 && !bytes.is_empty() {
        return Err(LINUX_EFAULT);
    }
    let Some(result) = multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        process_state
            .address_space()
            .validate_user_write_buffer(VirtAddr::new(ptr), bytes.len())?;
        process_state
            .address_space()
            .copy_into_user(VirtAddr::new(ptr), bytes)
    }) else {
        return Err(LINUX_ESRCH);
    };
    result.map_err(address_space_error_to_linux_errno)
}
