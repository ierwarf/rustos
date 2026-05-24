// RING3-MIGRATION-REFERENCE START: commercial-max netd should own socket namespace,
// socket option policy, bind/connect/listen routing, fd transfer, and network syscall
// validation. Ring0 keeps current-process user-copy and raw socket/device handoff
// primitives.
use super::*;

use alloc::string::String;
use alloc::vec::Vec;
use core::mem::size_of;

use rustos_user_abi::syscall::{RustosNetBrokerArgs, IPC_SERVICE_CAP_NET_POLICY};
use x86_64::VirtAddr;

const MAX_SOCKET_IO_BYTES: usize = 64 * 1024;
const MAX_IOVEC_COUNT: usize = 16;

enum ProcessSocket {
    Unix {
        handle: multitask::SocketHandle,
        status_flags: u64,
    },
    Inet {
        handle: multitask::InetSocketHandle,
        status_flags: u64,
    },
}

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
        SYSCALL_OFFLOAD_OP_LINUX_BIND => broker_bind(args),
        SYSCALL_OFFLOAD_OP_LINUX_LISTEN => broker_listen(args),
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT => broker_accept(args),
        SYSCALL_OFFLOAD_OP_LINUX_CONNECT => broker_connect(args),
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO => broker_sendto(args),
        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => broker_recvfrom(args),
        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME => broker_getsockname(args),
        SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME => broker_getpeername(args),
        SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT => broker_setsockopt(args),
        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT => broker_getsockopt(args),
        SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN => broker_shutdown(args),
        SYSCALL_OFFLOAD_OP_LINUX_SENDMSG => broker_sendmsg(args),
        SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => broker_recvmsg(args),
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
            let credentials = process_socket_credentials(args.process_id)?;
            multitask::KernelHandle::Socket(multitask::SocketHandle::new_unix_stream_with_owner(
                credentials,
            ))
        }
        (linux_abi::AF_INET, linux_abi::SOCK_STREAM | linux_abi::SOCK_DGRAM) => {
            multitask::KernelHandle::InetSocket(multitask::InetSocketHandle::new(
                domain, base_type, protocol,
            ))
        }
        (linux_abi::AF_INET, linux_abi::SOCK_STREAM) => multitask::KernelHandle::InetSocket(
            multitask::InetSocketHandle::new(domain, base_type, protocol),
        ),
        (linux_abi::AF_INET, linux_abi::SOCK_DGRAM) => multitask::KernelHandle::InetSocket(
            multitask::InetSocketHandle::new(domain, base_type, protocol),
        ),
        _ => return Err(LINUX_EAFNOSUPPORT),
    };

    install_process_handle(args.process_id, handle, open_flags)
}

fn broker_socketpair(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    if args.arg0 != linux_abi::AF_UNIX
        || args.arg1 & linux_abi::SOCK_TYPE_MASK != linux_abi::SOCK_STREAM
    {
        return Err(LINUX_EAFNOSUPPORT);
    }
    let open_flags = args.arg1 & (linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC);
    let credentials = process_socket_credentials(args.process_id)?;
    let (left, right) = multitask::SocketHandle::socketpair(credentials);
    let left_fd = install_process_handle(
        args.process_id,
        multitask::KernelHandle::Socket(left),
        open_flags,
    )?;
    let right_fd = install_process_handle(
        args.process_id,
        multitask::KernelHandle::Socket(right),
        open_flags,
    )?;
    write_process_i32_pair(args.process_id, args.arg3, left_fd as i32, right_fd as i32)?;
    Ok(0)
}

fn broker_bind(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let ProcessSocket::Unix { handle, .. } = process_socket(args.process_id, args.arg0)? else {
        return Err(LINUX_EOPNOTSUPP);
    };
    let path = read_sockaddr_un_path(args.process_id, args.arg1, args.arg2)?;
    handle
        .bind(path.as_str())
        .map_err(socket_error_to_linux_errno)?;
    Ok(0)
}

fn broker_listen(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let ProcessSocket::Unix { handle, .. } = process_socket(args.process_id, args.arg0)? else {
        return Err(LINUX_EOPNOTSUPP);
    };
    let backlog = usize::try_from(args.arg1).unwrap_or(usize::MAX);
    handle
        .listen(backlog)
        .map_err(socket_error_to_linux_errno)?;
    Ok(0)
}

fn broker_accept(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let ProcessSocket::Unix {
        handle,
        status_flags,
    } = process_socket(args.process_id, args.arg0)?
    else {
        return Err(LINUX_EOPNOTSUPP);
    };
    let nonblocking = status_flags & linux_abi::O_NONBLOCK != 0;
    let accepted = handle
        .accept(nonblocking)
        .map_err(socket_error_to_linux_errno)?;
    if args.arg1 != 0 && args.arg2 != 0 {
        let path = accepted.peer_path().unwrap_or_default();
        write_sockaddr_un(args.process_id, args.arg1, args.arg2, path.as_str())?;
    }
    install_process_handle(
        args.process_id,
        multitask::KernelHandle::Socket(accepted),
        args.arg3 & (linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC),
    )
}

fn broker_connect(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    match process_socket(args.process_id, args.arg0)? {
        ProcessSocket::Unix { handle, .. } => {
            let path = read_sockaddr_un_path(args.process_id, args.arg1, args.arg2)?;
            handle
                .connect(path.as_str())
                .map_err(socket_error_to_linux_errno)?;
            Ok(0)
        }
        ProcessSocket::Inet { .. } => Err(LINUX_ENETUNREACH),
    }
}

fn broker_sendto(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let ProcessSocket::Unix {
        handle,
        status_flags,
    } = process_socket(args.process_id, args.arg0)?
    else {
        return Err(LINUX_ENETUNREACH);
    };
    let len = checked_socket_io_len(args.arg2)?;
    let mut bytes = alloc::vec![0_u8; len];
    copy_from_process(args.process_id, args.arg1, &mut bytes)?;
    let nonblocking =
        status_flags & linux_abi::O_NONBLOCK != 0 || args.arg3 & linux_abi::MSG_DONTWAIT != 0;
    let sent = handle
        .send(bytes.as_slice(), nonblocking)
        .map_err(socket_error_to_linux_errno)?;
    Ok(sent as u64)
}

fn broker_recvfrom(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let ProcessSocket::Unix {
        handle,
        status_flags,
    } = process_socket(args.process_id, args.arg0)?
    else {
        return Err(LINUX_ENETUNREACH);
    };
    let len = checked_socket_io_len(args.arg2)?;
    let mut bytes = alloc::vec![0_u8; len];
    let nonblocking =
        status_flags & linux_abi::O_NONBLOCK != 0 || args.arg3 & linux_abi::MSG_DONTWAIT != 0;
    let read = handle
        .recv(&mut bytes, nonblocking)
        .map_err(socket_error_to_linux_errno)?;
    write_process_bytes(args.process_id, args.arg1, &bytes[..read])?;
    if args.arg4 != 0 && args.arg5 != 0 {
        let path = handle.peer_path().unwrap_or_default();
        write_sockaddr_un(args.process_id, args.arg4, args.arg5, path.as_str())?;
    }
    Ok(read as u64)
}

fn broker_sendmsg(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let ProcessSocket::Unix {
        handle,
        status_flags,
    } = process_socket(args.process_id, args.arg0)?
    else {
        return Err(LINUX_ENETUNREACH);
    };
    let header = read_process_struct::<linux_abi::LinuxMsghdr>(args.process_id, args.arg1)?;
    if header.msg_control != 0 && header.msg_controllen != 0 {
        return Err(LINUX_ENOSYS);
    }
    let bytes = read_iovec_bytes(args.process_id, header.msg_iov, header.msg_iovlen)?;
    let nonblocking =
        status_flags & linux_abi::O_NONBLOCK != 0 || args.arg2 & linux_abi::MSG_DONTWAIT != 0;
    let sent = handle
        .send(bytes.as_slice(), nonblocking)
        .map_err(socket_error_to_linux_errno)?;
    Ok(sent as u64)
}

fn broker_recvmsg(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let ProcessSocket::Unix {
        handle,
        status_flags,
    } = process_socket(args.process_id, args.arg0)?
    else {
        return Err(LINUX_ENETUNREACH);
    };
    let mut header = read_process_struct::<linux_abi::LinuxMsghdr>(args.process_id, args.arg1)?;
    if header.msg_control != 0 && header.msg_controllen != 0 {
        header.msg_flags |= linux_abi::MSG_CTRUNC as u32;
    }
    let iovecs = read_iovecs(args.process_id, header.msg_iov, header.msg_iovlen)?;
    let total_len = iovec_total_len(&iovecs)?;
    let mut bytes = alloc::vec![0_u8; total_len.min(MAX_SOCKET_IO_BYTES)];
    let nonblocking =
        status_flags & linux_abi::O_NONBLOCK != 0 || args.arg2 & linux_abi::MSG_DONTWAIT != 0;
    let read = handle
        .recv(&mut bytes, nonblocking)
        .map_err(socket_error_to_linux_errno)?;
    write_iovec_bytes(args.process_id, &iovecs, &bytes[..read])?;
    write_process_struct(args.process_id, args.arg1, &header)?;
    Ok(read as u64)
}

fn broker_getsockname(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let ProcessSocket::Unix { handle, .. } = process_socket(args.process_id, args.arg0)? else {
        return Err(LINUX_EOPNOTSUPP);
    };
    let path = handle.local_path().unwrap_or_default();
    write_sockaddr_un(args.process_id, args.arg1, args.arg2, path.as_str())?;
    Ok(0)
}

fn broker_getpeername(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let ProcessSocket::Unix { handle, .. } = process_socket(args.process_id, args.arg0)? else {
        return Err(LINUX_EOPNOTSUPP);
    };
    let Some(path) = handle.peer_path() else {
        return Err(LINUX_ENOTCONN);
    };
    write_sockaddr_un(args.process_id, args.arg1, args.arg2, path.as_str())?;
    Ok(0)
}

fn broker_setsockopt(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let _ = process_socket(args.process_id, args.arg0)?;
    if args.arg1 != linux_abi::SOL_SOCKET {
        return Err(LINUX_EOPNOTSUPP);
    }
    match args.arg2 {
        linux_abi::SO_REUSEADDR
        | linux_abi::SO_REUSEPORT
        | linux_abi::SO_KEEPALIVE
        | linux_abi::SO_SNDBUF
        | linux_abi::SO_RCVBUF
        | linux_abi::SO_PASSCRED => Ok(0),
        _ => Err(LINUX_EOPNOTSUPP),
    }
}

fn broker_getsockopt(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let socket = process_socket(args.process_id, args.arg0)?;
    if args.arg1 != linux_abi::SOL_SOCKET {
        return Err(LINUX_EOPNOTSUPP);
    }

    match args.arg2 {
        linux_abi::SO_ERROR => write_sockopt_i32(args.process_id, args.arg3, args.arg4, 0),
        linux_abi::SO_TYPE => write_sockopt_i32(
            args.process_id,
            args.arg3,
            args.arg4,
            socket_type(&socket) as i32,
        ),
        linux_abi::SO_DOMAIN => write_sockopt_i32(
            args.process_id,
            args.arg3,
            args.arg4,
            socket_domain(&socket) as i32,
        ),
        linux_abi::SO_PROTOCOL => write_sockopt_i32(
            args.process_id,
            args.arg3,
            args.arg4,
            socket_protocol(&socket) as i32,
        ),
        linux_abi::SO_ACCEPTCONN => {
            let value = match &socket {
                ProcessSocket::Unix { handle, .. } if handle.is_listening() => 1,
                _ => 0,
            };
            write_sockopt_i32(args.process_id, args.arg3, args.arg4, value)
        }
        linux_abi::SO_PEERCRED => {
            let ProcessSocket::Unix { handle, .. } = socket else {
                return Err(LINUX_EOPNOTSUPP);
            };
            let Some(credentials) = handle.peer_credentials() else {
                return Err(LINUX_ENOTCONN);
            };
            let value = linux_abi::LinuxUCred {
                pid: credentials.pid(),
                uid: credentials.uid(),
                gid: credentials.gid(),
            };
            write_sockopt_struct(args.process_id, args.arg3, args.arg4, &value)
        }
        _ => Err(LINUX_EOPNOTSUPP),
    }
}

fn broker_shutdown(args: &RustosNetBrokerArgs) -> Result<u64, i64> {
    let ProcessSocket::Unix { handle, .. } = process_socket(args.process_id, args.arg0)? else {
        return Err(LINUX_EOPNOTSUPP);
    };
    handle
        .shutdown(args.arg1)
        .map_err(socket_error_to_linux_errno)?;
    Ok(0)
}

fn process_socket(process_id: u64, fd: u64) -> Result<ProcessSocket, i64> {
    let Some(result) = multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        let Some(entry) = process_state.handles().get_entry(fd) else {
            return Err(LINUX_EBADF);
        };
        match entry.handle() {
            multitask::KernelHandle::Socket(handle) => Ok(ProcessSocket::Unix {
                handle: handle.clone(),
                status_flags: entry.status_flags(),
            }),
            multitask::KernelHandle::InetSocket(handle) => Ok(ProcessSocket::Inet {
                handle: *handle,
                status_flags: entry.status_flags(),
            }),
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

fn process_socket_credentials(process_id: u64) -> Result<multitask::SocketCredentials, i64> {
    let Some(credentials) = multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        let security = process_state.security();
        multitask::SocketCredentials::new(process_id as i32, security.euid(), security.egid())
    }) else {
        return Err(LINUX_ESRCH);
    };
    Ok(credentials)
}

fn read_sockaddr_un_path(process_id: u64, ptr: u64, len: u64) -> Result<String, i64> {
    let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
    if ptr == 0 || len < size_of::<u16>() {
        return Err(LINUX_EINVAL);
    }
    let copy_len = len.min(size_of::<linux_abi::LinuxSockaddrUn>());
    let mut bytes = alloc::vec![0_u8; copy_len];
    copy_from_process(process_id, ptr, &mut bytes)?;
    let family = u16::from_ne_bytes([bytes[0], bytes[1]]) as u64;
    if family != linux_abi::AF_UNIX {
        return Err(LINUX_EAFNOSUPPORT);
    }
    let path_bytes = &bytes[size_of::<u16>()..];
    if path_bytes.first().copied() == Some(0) {
        return Err(LINUX_EINVAL);
    }
    let end = path_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(path_bytes.len());
    if end == 0 {
        return Err(LINUX_EINVAL);
    }
    String::from_utf8(path_bytes[..end].to_vec()).map_err(|_| LINUX_EINVAL)
}

fn write_sockaddr_un(
    process_id: u64,
    addr_ptr: u64,
    addrlen_ptr: u64,
    path: &str,
) -> Result<(), i64> {
    if addrlen_ptr == 0 {
        return Err(LINUX_EINVAL);
    }
    let needed = size_of::<u16>()
        .checked_add(path.len())
        .and_then(|value| value.checked_add(1))
        .ok_or(LINUX_EINVAL)?;
    if addr_ptr != 0 {
        let capacity = read_process_u32(process_id, addrlen_ptr)? as usize;
        if capacity < needed || path.len() >= linux_abi::UNIX_PATH_MAX {
            return Err(LINUX_EINVAL);
        }
        let mut sockaddr = linux_abi::LinuxSockaddrUn {
            sun_family: linux_abi::AF_UNIX as u16,
            sun_path: [0; linux_abi::UNIX_PATH_MAX],
        };
        sockaddr.sun_path[..path.len()].copy_from_slice(path.as_bytes());
        write_process_struct(process_id, addr_ptr, &sockaddr)?;
    }
    write_process_u32(process_id, addrlen_ptr, needed as u32)
}

fn write_sockopt_i32(
    process_id: u64,
    optval_ptr: u64,
    optlen_ptr: u64,
    value: i32,
) -> Result<u64, i64> {
    write_sockopt_struct(process_id, optval_ptr, optlen_ptr, &value)
}

fn write_sockopt_struct<T: Copy>(
    process_id: u64,
    optval_ptr: u64,
    optlen_ptr: u64,
    value: &T,
) -> Result<u64, i64> {
    if optval_ptr == 0 || optlen_ptr == 0 {
        return Err(LINUX_EINVAL);
    }
    let len = size_of::<T>();
    if read_process_u32(process_id, optlen_ptr)? as usize != len {
        let capacity = read_process_u32(process_id, optlen_ptr)? as usize;
        if capacity < len {
            return Err(LINUX_EINVAL);
        }
    }
    write_process_struct(process_id, optval_ptr, value)?;
    write_process_u32(process_id, optlen_ptr, len as u32)?;
    Ok(0)
}

fn socket_domain(socket: &ProcessSocket) -> u64 {
    match socket {
        ProcessSocket::Unix { .. } => linux_abi::AF_UNIX,
        ProcessSocket::Inet { handle, .. } => handle.domain(),
    }
}

fn socket_type(socket: &ProcessSocket) -> u64 {
    match socket {
        ProcessSocket::Unix { .. } => linux_abi::SOCK_STREAM,
        ProcessSocket::Inet { handle, .. } => handle.type_(),
    }
}

fn socket_protocol(socket: &ProcessSocket) -> u64 {
    match socket {
        ProcessSocket::Unix { .. } => 0,
        ProcessSocket::Inet { handle, .. } => handle.protocol(),
    }
}

fn checked_socket_io_len(len: u64) -> Result<usize, i64> {
    let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
    if len > MAX_SOCKET_IO_BYTES {
        return Err(LINUX_EINVAL);
    }
    Ok(len)
}

fn read_iovecs(
    process_id: u64,
    iov_ptr: u64,
    iov_len: u64,
) -> Result<Vec<linux_abi::LinuxIovec>, i64> {
    let iov_len = usize::try_from(iov_len).map_err(|_| LINUX_EINVAL)?;
    if iov_ptr == 0 || iov_len == 0 || iov_len > MAX_IOVEC_COUNT {
        return Err(LINUX_EINVAL);
    }
    let mut iovecs = Vec::with_capacity(iov_len);
    for index in 0..iov_len {
        let offset = index
            .checked_mul(size_of::<linux_abi::LinuxIovec>())
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(LINUX_EINVAL)?;
        iovecs.push(read_process_struct::<linux_abi::LinuxIovec>(
            process_id,
            iov_ptr + offset,
        )?);
    }
    Ok(iovecs)
}

fn iovec_total_len(iovecs: &[linux_abi::LinuxIovec]) -> Result<usize, i64> {
    let mut total = 0usize;
    for iov in iovecs {
        let len = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?;
        total = total.checked_add(len).ok_or(LINUX_EINVAL)?;
        if total > MAX_SOCKET_IO_BYTES {
            return Err(LINUX_EINVAL);
        }
    }
    Ok(total)
}

fn read_iovec_bytes(process_id: u64, iov_ptr: u64, iov_len: u64) -> Result<Vec<u8>, i64> {
    let iovecs = read_iovecs(process_id, iov_ptr, iov_len)?;
    let total = iovec_total_len(&iovecs)?;
    let mut bytes = Vec::with_capacity(total);
    for iov in iovecs {
        let len = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?;
        let start = bytes.len();
        bytes.resize(start + len, 0);
        copy_from_process(process_id, iov.iov_base, &mut bytes[start..])?;
    }
    Ok(bytes)
}

fn write_iovec_bytes(
    process_id: u64,
    iovecs: &[linux_abi::LinuxIovec],
    bytes: &[u8],
) -> Result<(), i64> {
    let mut written = 0usize;
    for iov in iovecs {
        if written >= bytes.len() {
            break;
        }
        let len = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?;
        let chunk_len = len.min(bytes.len() - written);
        write_process_bytes(
            process_id,
            iov.iov_base,
            &bytes[written..written + chunk_len],
        )?;
        written += chunk_len;
    }
    Ok(())
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

fn read_process_u32(process_id: u64, ptr: u64) -> Result<u32, i64> {
    let mut bytes = [0_u8; 4];
    copy_from_process(process_id, ptr, &mut bytes)?;
    Ok(u32::from_ne_bytes(bytes))
}

fn write_process_u32(process_id: u64, ptr: u64, value: u32) -> Result<(), i64> {
    write_process_bytes(process_id, ptr, &value.to_ne_bytes())
}

fn read_process_struct<T: Copy + Default>(process_id: u64, ptr: u64) -> Result<T, i64> {
    let mut value = T::default();
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(value).cast::<u8>(), size_of::<T>())
    };
    copy_from_process(process_id, ptr, bytes)?;
    Ok(value)
}

fn write_process_struct<T: Copy>(process_id: u64, ptr: u64, value: &T) -> Result<(), i64> {
    let bytes = unsafe {
        core::slice::from_raw_parts(core::ptr::addr_of!(*value).cast::<u8>(), size_of::<T>())
    };
    write_process_bytes(process_id, ptr, bytes)
}

fn copy_from_process(process_id: u64, ptr: u64, dest: &mut [u8]) -> Result<(), i64> {
    if ptr == 0 && !dest.is_empty() {
        return Err(LINUX_EFAULT);
    }
    let Some(result) = multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        process_state
            .address_space()
            .validate_user_read_buffer(VirtAddr::new(ptr), dest.len())?;
        process_state
            .address_space()
            .copy_from_user(VirtAddr::new(ptr), dest)
    }) else {
        return Err(LINUX_ESRCH);
    };
    result.map_err(address_space_error_to_linux_errno)
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

fn socket_error_to_linux_errno(error: multitask::SocketError) -> i64 {
    match error {
        multitask::SocketError::AddressInUse => LINUX_EADDRINUSE,
        multitask::SocketError::BrokenPipe => LINUX_EPIPE,
        multitask::SocketError::ConnectionRefused => LINUX_ECONNREFUSED,
        multitask::SocketError::InvalidArgument => LINUX_EINVAL,
        multitask::SocketError::IsConnected => LINUX_EISCONN,
        multitask::SocketError::NotConnected => LINUX_ENOTCONN,
        multitask::SocketError::NotFound => LINUX_ENOENT,
        multitask::SocketError::PermissionDenied => LINUX_EACCES,
        multitask::SocketError::TryAgain => LINUX_EAGAIN,
    }
}
// RING3-MIGRATION-REFERENCE END: commercial-max netd-owned socket broker policy.
