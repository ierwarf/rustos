use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::mem::size_of;
use core::slice;

use crate::multitask;
use crate::user::handles::{FD_CLOEXEC, HandleEntry, InetSocketHandle, KernelHandle};
use crate::user::socket::{PassedHandle, SocketCredentials, SocketHandle};

use super::*;

const MAX_SOCKET_IOV_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_SOCKET_CONTROL_BYTES: usize = 64 * 1024;

pub(crate) fn socket(domain: u64, type_: u64, protocol: u64) -> Result<u64, LinuxSysopError> {
    if domain == linux_abi::AF_INET {
        let (status_flags, fd_flags, base_type) = parse_inet_socket_type(type_)?;
        if !matches!(base_type, value if value == linux_abi::SOCK_STREAM || value == linux_abi::SOCK_DGRAM)
        {
            return Err(LinuxSysopError::OperationNotSupported);
        }
        if base_type != linux_abi::SOCK_STREAM {
            return Err(LinuxSysopError::OperationNotSupported);
        }
        let token = kernel_io_manager::api::network::create_inet_socket(base_type, protocol);
        return install_inet_socket(
            InetSocketHandle::from_token(token, domain, base_type, protocol),
            status_flags,
            fd_flags,
        );
    }

    if domain != linux_abi::AF_UNIX {
        return Err(LinuxSysopError::AddressFamilyNotSupported);
    }
    if protocol != 0 {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    let (status_flags, fd_flags) = parse_stream_socket_type(type_)?;
    install_socket(
        SocketHandle::new_unix_stream_with_owner(current_socket_credentials()),
        status_flags,
        fd_flags,
    )
}

fn install_inet_socket(
    socket: InetSocketHandle,
    status_flags: u64,
    fd_flags: u32,
) -> Result<u64, LinuxSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        Ok(process_state.handles_mut().install_entry(HandleEntry::new(
            KernelHandle::InetSocket(socket),
            fd_flags,
            status_flags,
        )))
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn socketpair(
    domain: u64,
    type_: u64,
    protocol: u64,
    sv_ptr: u64,
) -> Result<(), LinuxSysopError> {
    if domain != linux_abi::AF_UNIX {
        return Err(LinuxSysopError::AddressFamilyNotSupported);
    }
    if protocol != 0 {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    let (status_flags, fd_flags) = parse_stream_socket_type(type_)?;
    let (left, right) = SocketHandle::socketpair(current_socket_credentials());
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let left_fd = process_state.handles_mut().install_entry(HandleEntry::new(
            KernelHandle::Socket(left),
            fd_flags,
            status_flags,
        ));
        let right_fd = process_state.handles_mut().install_entry(HandleEntry::new(
            KernelHandle::Socket(right),
            fd_flags,
            status_flags,
        ));
        let left_fd = i32::try_from(left_fd).map_err(|_| LinuxSysopError::InvalidArgument)?;
        let right_fd = i32::try_from(right_fd).map_err(|_| LinuxSysopError::InvalidArgument)?;
        let bytes = [
            left_fd.to_le_bytes()[0],
            left_fd.to_le_bytes()[1],
            left_fd.to_le_bytes()[2],
            left_fd.to_le_bytes()[3],
            right_fd.to_le_bytes()[0],
            right_fd.to_le_bytes()[1],
            right_fd.to_le_bytes()[2],
            right_fd.to_le_bytes()[3],
        ];
        usermem::write_current_user_bytes(sv_ptr, &bytes)?;
        Ok(())
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn bind(fd: u64, addr_ptr: u64, addr_len: u64) -> Result<(), LinuxSysopError> {
    let path = read_sockaddr_un_path(addr_ptr, addr_len)?;
    let (socket, _) = socket_handle_for_fd(fd)?;
    socket.bind(path.as_str()).map_err(Into::into)
}

pub(crate) fn listen(fd: u64, backlog: u64) -> Result<(), LinuxSysopError> {
    let (socket, _) = socket_handle_for_fd(fd)?;
    let backlog = ((backlog as u32) as i32).max(0) as usize;
    socket.listen(backlog).map_err(Into::into)
}

pub(crate) fn accept(fd: u64, addr_ptr: u64, addr_len_ptr: u64) -> Result<u64, LinuxSysopError> {
    accept4(fd, addr_ptr, addr_len_ptr, 0)
}

pub(crate) fn accept4(
    fd: u64,
    addr_ptr: u64,
    addr_len_ptr: u64,
    flags: u64,
) -> Result<u64, LinuxSysopError> {
    let (socket, status_flags) = socket_handle_for_fd(fd)?;
    let (new_status_flags, fd_flags) = parse_accept4_flags(flags)?;
    let accepted = socket.accept(status_flags & linux_abi::O_NONBLOCK != 0)?;

    let accepted_fd = install_socket(accepted, linux_abi::O_RDWR | new_status_flags, fd_flags)?;
    write_accept_address(addr_ptr, addr_len_ptr)?;
    Ok(accepted_fd)
}

pub(crate) fn connect(fd: u64, addr_ptr: u64, addr_len: u64) -> Result<(), LinuxSysopError> {
    if let Some(socket) = inet_socket_handle_for_fd(fd)? {
        let (addr, port) = read_sockaddr_in(addr_ptr, addr_len)?;
        return kernel_io_manager::api::network::connect_inet_socket(socket.token_id(), addr, port)
            .map_err(map_inet_error);
    }
    let path = read_sockaddr_un_path(addr_ptr, addr_len)?;
    let (socket, _) = socket_handle_for_fd(fd)?;
    socket.connect(path.as_str()).map_err(Into::into)
}

pub(crate) fn getsockname(
    fd: u64,
    addr_ptr: u64,
    addr_len_ptr: u64,
) -> Result<(), LinuxSysopError> {
    let (socket, _) = socket_handle_for_fd(fd)?;
    write_socket_address_result(socket.local_path().as_deref(), addr_ptr, addr_len_ptr)
}

pub(crate) fn getpeername(
    fd: u64,
    addr_ptr: u64,
    addr_len_ptr: u64,
) -> Result<(), LinuxSysopError> {
    let (socket, _) = socket_handle_for_fd(fd)?;
    if socket.peer_credentials().is_none() {
        return Err(LinuxSysopError::NotConnected);
    }
    write_socket_address_result(socket.peer_path().as_deref(), addr_ptr, addr_len_ptr)
}

pub(crate) fn setsockopt(
    fd: u64,
    level: u64,
    optname: u64,
    optval_ptr: u64,
    optlen: u64,
) -> Result<(), LinuxSysopError> {
    let (socket, _) = socket_handle_for_fd(fd)?;
    if level != linux_abi::SOL_SOCKET {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    match optname {
        linux_abi::SO_REUSEADDR
        | linux_abi::SO_KEEPALIVE
        | linux_abi::SO_REUSEPORT
        | linux_abi::SO_PASSCRED => {
            let _ = read_socket_option_i32(optval_ptr, optlen)?;
            Ok(())
        }
        linux_abi::SO_SNDBUF | linux_abi::SO_RCVBUF => {
            let value = read_socket_option_i32(optval_ptr, optlen)?;
            if value <= 0 {
                return Err(LinuxSysopError::InvalidArgument);
            }
            Ok(())
        }
        linux_abi::SO_ERROR
        | linux_abi::SO_TYPE
        | linux_abi::SO_ACCEPTCONN
        | linux_abi::SO_PEERCRED
        | linux_abi::SO_DOMAIN
        | linux_abi::SO_PROTOCOL => {
            let _ = socket;
            Err(LinuxSysopError::OperationNotSupported)
        }
        _ => Err(LinuxSysopError::OperationNotSupported),
    }
}

pub(crate) fn getsockopt(
    fd: u64,
    level: u64,
    optname: u64,
    optval_ptr: u64,
    optlen_ptr: u64,
) -> Result<(), LinuxSysopError> {
    let (socket, _) = socket_handle_for_fd(fd)?;
    if level != linux_abi::SOL_SOCKET {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    match optname {
        linux_abi::SO_TYPE => write_socket_option_i32(
            optval_ptr,
            optlen_ptr,
            i32::try_from(linux_abi::SOCK_STREAM).map_err(|_| LinuxSysopError::InvalidArgument)?,
        ),
        linux_abi::SO_ERROR => write_socket_option_i32(optval_ptr, optlen_ptr, 0),
        linux_abi::SO_ACCEPTCONN => {
            write_socket_option_i32(optval_ptr, optlen_ptr, i32::from(socket.is_listening()))
        }
        linux_abi::SO_DOMAIN => write_socket_option_i32(
            optval_ptr,
            optlen_ptr,
            i32::try_from(linux_abi::AF_UNIX).map_err(|_| LinuxSysopError::InvalidArgument)?,
        ),
        linux_abi::SO_PROTOCOL => write_socket_option_i32(optval_ptr, optlen_ptr, 0),
        linux_abi::SO_PASSCRED
        | linux_abi::SO_REUSEADDR
        | linux_abi::SO_KEEPALIVE
        | linux_abi::SO_REUSEPORT => write_socket_option_i32(optval_ptr, optlen_ptr, 0),
        linux_abi::SO_PEERCRED => {
            let creds = socket
                .peer_credentials()
                .ok_or(LinuxSysopError::NotConnected)?;
            write_socket_option_ucred(optval_ptr, optlen_ptr, creds)
        }
        _ => Err(LinuxSysopError::OperationNotSupported),
    }
}

pub(crate) fn shutdown(fd: u64, how: u64) -> Result<(), LinuxSysopError> {
    let (socket, _) = socket_handle_for_fd(fd)?;
    socket.shutdown(how).map_err(Into::into)
}

pub(crate) fn sendmsg(fd: u64, msghdr_ptr: u64, flags: u64) -> Result<usize, LinuxSysopError> {
    if flags & !(linux_abi::MSG_DONTWAIT | linux_abi::MSG_NOSIGNAL) != 0 {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    let msghdr = read_msghdr(msghdr_ptr)?;
    if msghdr.msg_name != 0 || msghdr.msg_namelen != 0 {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    let (socket, status_flags) = socket_handle_for_fd(fd)?;
    let nonblocking =
        flags & linux_abi::MSG_DONTWAIT != 0 || status_flags & linux_abi::O_NONBLOCK != 0;
    let iovecs = read_iovecs(msghdr.msg_iov, msghdr.msg_iovlen)?;
    let payload = read_iovec_payload(&iovecs)?;
    let rights = read_passed_handles_from_control(msghdr.msg_control, msghdr.msg_controllen)?;

    socket
        .send_message(payload, rights, nonblocking)
        .map_err(Into::into)
}

pub(crate) fn recvmsg(fd: u64, msghdr_ptr: u64, flags: u64) -> Result<usize, LinuxSysopError> {
    if flags & !(linux_abi::MSG_DONTWAIT | linux_abi::MSG_NOSIGNAL | linux_abi::MSG_CMSG_CLOEXEC)
        != 0
    {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    let mut msghdr = read_msghdr(msghdr_ptr)?;
    let iovecs = read_iovecs(msghdr.msg_iov, msghdr.msg_iovlen)?;
    let (socket, status_flags) = socket_handle_for_fd(fd)?;
    let requested_nonblocking =
        flags & linux_abi::MSG_DONTWAIT != 0 || status_flags & linux_abi::O_NONBLOCK != 0;

    let mut total = 0usize;
    let mut received_rights = Vec::new();
    let mut first = true;
    for iovec in iovecs {
        let chunk_len =
            usize::try_from(iovec.iov_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        if chunk_len == 0 {
            continue;
        }

        let mut buffer = vec![0_u8; chunk_len];
        let nonblocking = if first { requested_nonblocking } else { true };
        match socket.recv_with_rights(&mut buffer, nonblocking) {
            Ok((read, rights)) => {
                if read == 0 {
                    break;
                }
                if !rights.is_empty() {
                    received_rights.extend(rights);
                }
                usermem::write_current_user_bytes(iovec.iov_base, &buffer[..read])?;
                total = total
                    .checked_add(read)
                    .ok_or(LinuxSysopError::InvalidArgument)?;
                if read < buffer.len() {
                    break;
                }
            }
            Err(err) if !first && err == crate::user::socket::SocketError::TryAgain => break,
            Err(err) => return Err(err.into()),
        }
        first = false;
    }

    msghdr.msg_flags = 0;
    msghdr.msg_namelen = 0;
    msghdr.msg_controllen = write_passed_handles_to_control(
        msghdr.msg_control,
        msghdr.msg_controllen,
        &received_rights,
        flags & linux_abi::MSG_CMSG_CLOEXEC != 0,
        &mut msghdr.msg_flags,
    )?;
    write_msghdr(msghdr_ptr, &msghdr)?;
    Ok(total)
}

pub(crate) fn sendto(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    flags: u64,
    addr_ptr: u64,
    addr_len: u64,
) -> Result<usize, LinuxSysopError> {
    if flags & !(linux_abi::MSG_DONTWAIT | linux_abi::MSG_NOSIGNAL) != 0 {
        return Err(LinuxSysopError::OperationNotSupported);
    }
    if let Some(socket) = inet_socket_handle_for_fd(fd)? {
        if addr_ptr != 0 || addr_len != 0 {
            return Err(LinuxSysopError::OperationNotSupported);
        }
        let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        if len == 0 {
            return Ok(0);
        }
        let mut buffer = vec![0_u8; len];
        usermem::copy_from_current_user_exact(user_ptr, &mut buffer)?;
        return kernel_io_manager::api::network::send_inet_socket(socket.token_id(), &buffer)
            .map_err(map_inet_error);
    }

    if addr_ptr != 0 || addr_len != 0 {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    let (socket, status_flags) = socket_handle_for_fd(fd)?;
    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Ok(0);
    }

    let mut buffer = vec![0_u8; len];
    usermem::copy_from_current_user_exact(user_ptr, &mut buffer)?;
    let nonblocking =
        flags & linux_abi::MSG_DONTWAIT != 0 || status_flags & linux_abi::O_NONBLOCK != 0;
    socket.send(&buffer, nonblocking).map_err(Into::into)
}

pub(crate) fn recvfrom(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    flags: u64,
    _addr_ptr: u64,
    addr_len_ptr: u64,
) -> Result<usize, LinuxSysopError> {
    if flags & !(linux_abi::MSG_DONTWAIT | linux_abi::MSG_NOSIGNAL) != 0 {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    if let Some(socket) = inet_socket_handle_for_fd(fd)? {
        let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        if len == 0 {
            if addr_len_ptr != 0 {
                usermem::write_current_user_bytes(addr_len_ptr, &0_u32.to_le_bytes())?;
            }
            return Ok(0);
        }
        let mut buffer = vec![0_u8; len];
        let nonblocking = flags & linux_abi::MSG_DONTWAIT != 0
            || inet_status_flags_for_fd(fd)? & linux_abi::O_NONBLOCK != 0;
        let read = kernel_io_manager::api::network::recv_inet_socket(
            socket.token_id(),
            &mut buffer,
            nonblocking,
        )
        .map_err(map_inet_error)?;
        usermem::write_current_user_bytes(user_ptr, &buffer[..read])?;
        if addr_len_ptr != 0 {
            usermem::write_current_user_bytes(addr_len_ptr, &0_u32.to_le_bytes())?;
        }
        return Ok(read);
    }

    let (socket, status_flags) = socket_handle_for_fd(fd)?;
    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        if addr_len_ptr != 0 {
            usermem::write_current_user_bytes(addr_len_ptr, &0_u32.to_le_bytes())?;
        }
        return Ok(0);
    }

    let mut buffer = vec![0_u8; len];
    let nonblocking =
        flags & linux_abi::MSG_DONTWAIT != 0 || status_flags & linux_abi::O_NONBLOCK != 0;
    let read = socket.recv(&mut buffer, nonblocking)?;
    usermem::write_current_user_bytes(user_ptr, &buffer[..read])?;

    // AF_UNIX stream sockets in this kernel do not expose a per-message source address.
    if addr_len_ptr != 0 {
        usermem::write_current_user_bytes(addr_len_ptr, &0_u32.to_le_bytes())?;
    }

    Ok(read)
}

pub(crate) fn write_current_process_socket(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
) -> Result<Option<usize>, LinuxSysopError> {
    if let Some((socket, _status_flags)) = current_inet_socket_for_fd(fd)? {
        let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        if len == 0 {
            return Ok(Some(0));
        }
        let mut buffer = vec![0_u8; len];
        usermem::copy_from_current_user_exact(user_ptr, &mut buffer)?;
        return kernel_io_manager::api::network::send_inet_socket(socket.token_id(), &buffer)
            .map(Some)
            .map_err(map_inet_error);
    }

    let Some((socket, status_flags)) = current_socket_for_fd(fd)? else {
        return Ok(None);
    };

    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Ok(Some(0));
    }

    let mut buffer = vec![0_u8; len];
    usermem::copy_from_current_user_exact(user_ptr, &mut buffer)?;
    let nonblocking = status_flags & linux_abi::O_NONBLOCK != 0;
    socket
        .send(&buffer, nonblocking)
        .map(Some)
        .map_err(Into::into)
}

pub(crate) fn read_current_process_socket(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
) -> Result<Option<usize>, LinuxSysopError> {
    if let Some((socket, status_flags)) = current_inet_socket_for_fd(fd)? {
        let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        if len == 0 {
            return Ok(Some(0));
        }
        let mut buffer = vec![0_u8; len];
        let read = kernel_io_manager::api::network::recv_inet_socket(
            socket.token_id(),
            &mut buffer,
            status_flags & linux_abi::O_NONBLOCK != 0,
        )
        .map_err(map_inet_error)?;
        usermem::write_current_user_bytes(user_ptr, &buffer[..read])?;
        return Ok(Some(read));
    }

    let Some((socket, status_flags)) = current_socket_for_fd(fd)? else {
        return Ok(None);
    };

    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Ok(Some(0));
    }

    let mut buffer = vec![0_u8; len];
    let nonblocking = status_flags & linux_abi::O_NONBLOCK != 0;
    let read = socket.recv(&mut buffer, nonblocking)?;
    usermem::write_current_user_bytes(user_ptr, &buffer[..read])?;
    Ok(Some(read))
}

pub(crate) fn readable_socket_bytes_for_fd(fd: u64) -> Result<Option<u64>, LinuxSysopError> {
    if let Some((socket, _)) = current_inet_socket_for_fd(fd)? {
        return kernel_io_manager::api::network::inet_readable_bytes(socket.token_id())
            .and_then(|len| {
                u64::try_from(len)
                    .map_err(|_| kernel_io_manager::api::network::InetSocketError::InvalidArgument)
            })
            .map(Some)
            .map_err(map_inet_error);
    }

    let Some((socket, _)) = current_socket_for_fd(fd)? else {
        return Ok(None);
    };
    u64::try_from(socket.readable_bytes())
        .map(Some)
        .map_err(|_| LinuxSysopError::InvalidArgument)
}

fn install_socket(
    socket: SocketHandle,
    status_flags: u64,
    fd_flags: u32,
) -> Result<u64, LinuxSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        Ok(process_state.handles_mut().install_entry(HandleEntry::new(
            KernelHandle::Socket(socket),
            fd_flags,
            status_flags,
        )))
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn current_socket_credentials() -> SocketCredentials {
    let snapshot = multitask::current_user_snapshot();
    let pid = snapshot
        .and_then(|user| i32::try_from(user.process_id()).ok())
        .unwrap_or(0);
    let security = snapshot.map(|user| user.security());
    SocketCredentials::new(
        pid,
        security.map(|context| context.euid()).unwrap_or(0),
        security.map(|context| context.egid()).unwrap_or(0),
    )
}

fn current_socket_for_fd(fd: u64) -> Result<Option<(SocketHandle, u64)>, LinuxSysopError> {
    if fd < 3 {
        return Ok(None);
    }

    let Some(result) = multitask::with_current_user_process_state(|_, _, process_state| {
        let Some(entry) = process_state.handles().get_entry(fd) else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };
        match entry.handle() {
            KernelHandle::Socket(socket) => Ok(Some((socket.clone(), entry.status_flags()))),
            KernelHandle::InetSocket(_) => Err(LinuxSysopError::OperationNotSupported),
            _ => Ok(None),
        }
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn current_inet_socket_for_fd(fd: u64) -> Result<Option<(InetSocketHandle, u64)>, LinuxSysopError> {
    if fd < 3 {
        return Ok(None);
    }

    let Some(result) = multitask::with_current_user_process_state(|_, _, process_state| {
        let Some(entry) = process_state.handles().get_entry(fd) else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };
        match entry.handle() {
            KernelHandle::InetSocket(socket) => Ok(Some((*socket, entry.status_flags()))),
            _ => Ok(None),
        }
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn inet_socket_handle_for_fd(fd: u64) -> Result<Option<InetSocketHandle>, LinuxSysopError> {
    current_inet_socket_for_fd(fd).map(|entry| entry.map(|(socket, _)| socket))
}

fn inet_status_flags_for_fd(fd: u64) -> Result<u64, LinuxSysopError> {
    Ok(current_inet_socket_for_fd(fd)?
        .map(|(_, status_flags)| status_flags)
        .unwrap_or(0))
}

fn socket_handle_for_fd(fd: u64) -> Result<(SocketHandle, u64), LinuxSysopError> {
    if fd < 3 {
        return Err(LinuxSysopError::NotSocket);
    }

    let Some(result) = multitask::with_current_user_process_state(|_, _, process_state| {
        let Some(entry) = process_state.handles().get_entry(fd) else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };
        match entry.handle() {
            KernelHandle::Socket(socket) => Ok((socket.clone(), entry.status_flags())),
            KernelHandle::InetSocket(_) => Err(LinuxSysopError::OperationNotSupported),
            _ => Err(LinuxSysopError::NotSocket),
        }
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn parse_stream_socket_type(type_: u64) -> Result<(u64, u32), LinuxSysopError> {
    let base_type = type_ & linux_abi::SOCK_TYPE_MASK;
    let flags = type_ & !linux_abi::SOCK_TYPE_MASK;
    if flags & !(linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC) != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if base_type != linux_abi::SOCK_STREAM {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    let mut status_flags = linux_abi::O_RDWR;
    let mut fd_flags = 0_u32;
    if flags & linux_abi::SOCK_NONBLOCK != 0 {
        status_flags |= linux_abi::O_NONBLOCK;
    }
    if flags & linux_abi::SOCK_CLOEXEC != 0 {
        fd_flags |= FD_CLOEXEC;
    }
    Ok((status_flags, fd_flags))
}

fn parse_inet_socket_type(type_: u64) -> Result<(u64, u32, u64), LinuxSysopError> {
    let base_type = type_ & linux_abi::SOCK_TYPE_MASK;
    let flags = type_ & !linux_abi::SOCK_TYPE_MASK;
    if flags & !(linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC) != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let mut status_flags = linux_abi::O_RDWR;
    let mut fd_flags = 0_u32;
    if flags & linux_abi::SOCK_NONBLOCK != 0 {
        status_flags |= linux_abi::O_NONBLOCK;
    }
    if flags & linux_abi::SOCK_CLOEXEC != 0 {
        fd_flags |= FD_CLOEXEC;
    }
    Ok((status_flags, fd_flags, base_type))
}

fn parse_accept4_flags(flags: u64) -> Result<(u64, u32), LinuxSysopError> {
    if flags & !(linux_abi::SOCK_NONBLOCK | linux_abi::SOCK_CLOEXEC) != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let mut status_flags = 0_u64;
    let mut fd_flags = 0_u32;
    if flags & linux_abi::SOCK_NONBLOCK != 0 {
        status_flags |= linux_abi::O_NONBLOCK;
    }
    if flags & linux_abi::SOCK_CLOEXEC != 0 {
        fd_flags |= FD_CLOEXEC;
    }
    Ok((status_flags, fd_flags))
}

fn read_sockaddr_un_path(addr_ptr: u64, addr_len: u64) -> Result<String, LinuxSysopError> {
    let len = usize::try_from(addr_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len < size_of::<u16>() || len > size_of::<linux_abi::LinuxSockaddrUn>() {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let mut bytes = vec![0_u8; len];
    usermem::copy_from_current_user_exact(addr_ptr, &mut bytes)?;
    let family = u16::from_le_bytes([bytes[0], bytes[1]]) as u64;
    if family != linux_abi::AF_UNIX {
        return Err(LinuxSysopError::AddressFamilyNotSupported);
    }

    let path_bytes = &bytes[size_of::<u16>()..];
    if path_bytes.is_empty() {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if path_bytes[0] == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let path_len = path_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(path_bytes.len());
    if path_len == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    Ok(String::from_utf8_lossy(&path_bytes[..path_len]).into_owned())
}

fn read_sockaddr_in(addr_ptr: u64, addr_len: u64) -> Result<([u8; 4], u16), LinuxSysopError> {
    let len = usize::try_from(addr_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if addr_ptr == 0 || len < size_of::<linux_abi::LinuxSockaddrIn>() {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let mut bytes = [0_u8; size_of::<linux_abi::LinuxSockaddrIn>()];
    usermem::copy_from_current_user_exact(addr_ptr, &mut bytes)?;
    let family = u16::from_le_bytes([bytes[0], bytes[1]]) as u64;
    if family != linux_abi::AF_INET {
        return Err(LinuxSysopError::AddressFamilyNotSupported);
    }

    Ok((
        [bytes[4], bytes[5], bytes[6], bytes[7]],
        u16::from_be_bytes([bytes[2], bytes[3]]),
    ))
}

fn map_inet_error(error: kernel_io_manager::api::network::InetSocketError) -> LinuxSysopError {
    match error {
        kernel_io_manager::api::network::InetSocketError::BadFileDescriptor => {
            LinuxSysopError::BadFileDescriptor
        }
        kernel_io_manager::api::network::InetSocketError::InvalidArgument => {
            LinuxSysopError::InvalidArgument
        }
        kernel_io_manager::api::network::InetSocketError::NetworkUnreachable => {
            LinuxSysopError::NetworkUnreachable
        }
        kernel_io_manager::api::network::InetSocketError::NotConnected => {
            LinuxSysopError::NotConnected
        }
        kernel_io_manager::api::network::InetSocketError::OperationNotSupported => {
            LinuxSysopError::OperationNotSupported
        }
        kernel_io_manager::api::network::InetSocketError::TryAgain => LinuxSysopError::TryAgain,
    }
}

fn write_accept_address(addr_ptr: u64, addr_len_ptr: u64) -> Result<(), LinuxSysopError> {
    if addr_ptr == 0 {
        return Ok(());
    }
    if addr_len_ptr == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let available = usize::try_from(usermem::read_current_user_u32(addr_len_ptr)?)
        .map_err(|_| LinuxSysopError::InvalidArgument)?;
    let sockaddr = linux_abi::LinuxSockaddrUn {
        sun_family: linux_abi::AF_UNIX as u16,
        ..Default::default()
    };
    let sockaddr_bytes = unsafe {
        slice::from_raw_parts(
            core::ptr::addr_of!(sockaddr).cast::<u8>(),
            size_of::<linux_abi::LinuxSockaddrUn>(),
        )
    };
    let write_len = available.min(size_of::<u16>());
    usermem::write_current_user_bytes(addr_ptr, &sockaddr_bytes[..write_len])?;
    usermem::write_current_user_u32(addr_len_ptr, size_of::<u16>() as u32)?;
    Ok(())
}

fn write_socket_address_result(
    path: Option<&str>,
    addr_ptr: u64,
    addr_len_ptr: u64,
) -> Result<(), LinuxSysopError> {
    if addr_ptr == 0 || addr_len_ptr == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let available = usize::try_from(usermem::read_current_user_u32(addr_len_ptr)?)
        .map_err(|_| LinuxSysopError::InvalidArgument)?;
    let mut sockaddr = linux_abi::LinuxSockaddrUn {
        sun_family: linux_abi::AF_UNIX as u16,
        ..Default::default()
    };
    let mut actual_len = size_of::<u16>();
    if let Some(path) = path {
        let path_bytes = path.as_bytes();
        if path_bytes.len() > linux_abi::UNIX_PATH_MAX {
            return Err(LinuxSysopError::InvalidArgument);
        }
        sockaddr.sun_path[..path_bytes.len()].copy_from_slice(path_bytes);
        actual_len = actual_len.saturating_add(path_bytes.len());
        if path_bytes.len() < linux_abi::UNIX_PATH_MAX {
            actual_len = actual_len.saturating_add(1);
        }
    }

    let sockaddr_bytes = unsafe {
        slice::from_raw_parts(
            core::ptr::addr_of!(sockaddr).cast::<u8>(),
            size_of::<linux_abi::LinuxSockaddrUn>(),
        )
    };
    let write_len = available.min(actual_len);
    usermem::write_current_user_bytes(addr_ptr, &sockaddr_bytes[..write_len])?;
    usermem::write_current_user_u32(
        addr_len_ptr,
        u32::try_from(actual_len).map_err(|_| LinuxSysopError::InvalidArgument)?,
    )?;
    Ok(())
}

fn read_socket_option_i32(optval_ptr: u64, optlen: u64) -> Result<i32, LinuxSysopError> {
    let len = usize::try_from(optlen).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len < size_of::<i32>() {
        return Err(LinuxSysopError::InvalidArgument);
    }
    let mut bytes = [0_u8; size_of::<i32>()];
    usermem::copy_from_current_user_exact(optval_ptr, &mut bytes)?;
    Ok(i32::from_ne_bytes(bytes))
}

fn write_socket_option_i32(
    optval_ptr: u64,
    optlen_ptr: u64,
    value: i32,
) -> Result<(), LinuxSysopError> {
    write_socket_option_bytes(optval_ptr, optlen_ptr, &value.to_ne_bytes())
}

fn write_socket_option_ucred(
    optval_ptr: u64,
    optlen_ptr: u64,
    value: SocketCredentials,
) -> Result<(), LinuxSysopError> {
    let ucred = linux_abi::LinuxUCred {
        pid: value.pid(),
        uid: value.uid(),
        gid: value.gid(),
    };
    let bytes = unsafe {
        slice::from_raw_parts(
            core::ptr::addr_of!(ucred).cast::<u8>(),
            size_of::<linux_abi::LinuxUCred>(),
        )
    };
    write_socket_option_bytes(optval_ptr, optlen_ptr, bytes)
}

fn write_socket_option_bytes(
    optval_ptr: u64,
    optlen_ptr: u64,
    bytes: &[u8],
) -> Result<(), LinuxSysopError> {
    if optval_ptr == 0 || optlen_ptr == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let available = usize::try_from(usermem::read_current_user_u32(optlen_ptr)?)
        .map_err(|_| LinuxSysopError::InvalidArgument)?;
    let write_len = available.min(bytes.len());
    usermem::write_current_user_bytes(optval_ptr, &bytes[..write_len])?;
    usermem::write_current_user_u32(
        optlen_ptr,
        u32::try_from(bytes.len()).map_err(|_| LinuxSysopError::InvalidArgument)?,
    )?;
    Ok(())
}

fn read_msghdr(msghdr_ptr: u64) -> Result<linux_abi::LinuxMsghdr, LinuxSysopError> {
    let mut msghdr = linux_abi::LinuxMsghdr::default();
    let bytes = unsafe {
        slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(msghdr).cast::<u8>(),
            size_of::<linux_abi::LinuxMsghdr>(),
        )
    };
    usermem::copy_from_current_user_exact(msghdr_ptr, bytes)?;
    Ok(msghdr)
}

fn write_msghdr(msghdr_ptr: u64, msghdr: &linux_abi::LinuxMsghdr) -> Result<(), LinuxSysopError> {
    let bytes = unsafe {
        slice::from_raw_parts(
            core::ptr::addr_of!(*msghdr).cast::<u8>(),
            size_of::<linux_abi::LinuxMsghdr>(),
        )
    };
    usermem::write_current_user_bytes(msghdr_ptr, bytes)?;
    Ok(())
}

fn read_iovecs(
    iov_ptr: u64,
    iov_count: u64,
) -> Result<Vec<linux_abi::LinuxIovec>, LinuxSysopError> {
    let count = usize::try_from(iov_count).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if count > MAX_IOV_COUNT {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if count == 0 {
        return Ok(Vec::new());
    }

    let mut iovecs = vec![linux_abi::LinuxIovec::default(); count];
    let bytes_len = count
        .checked_mul(size_of::<linux_abi::LinuxIovec>())
        .ok_or(LinuxSysopError::InvalidArgument)?;
    let bytes = unsafe { slice::from_raw_parts_mut(iovecs.as_mut_ptr().cast::<u8>(), bytes_len) };
    usermem::copy_from_current_user_exact(iov_ptr, bytes)?;
    Ok(iovecs)
}

fn read_iovec_payload(iovecs: &[linux_abi::LinuxIovec]) -> Result<Vec<u8>, LinuxSysopError> {
    let mut payload_len = 0usize;
    for iovec in iovecs {
        let chunk_len =
            usize::try_from(iovec.iov_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        payload_len = payload_len
            .checked_add(chunk_len)
            .ok_or(LinuxSysopError::InvalidArgument)?;
        if payload_len > MAX_SOCKET_IOV_PAYLOAD_BYTES {
            return Err(LinuxSysopError::InvalidArgument);
        }
    }

    let mut payload = vec![0_u8; payload_len];
    let mut copied = 0usize;
    for iovec in iovecs {
        let chunk_len =
            usize::try_from(iovec.iov_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        if chunk_len == 0 {
            continue;
        }
        usermem::copy_from_current_user_exact(
            iovec.iov_base,
            &mut payload[copied..copied + chunk_len],
        )?;
        copied += chunk_len;
    }
    Ok(payload)
}

fn read_passed_handles_from_control(
    control_ptr: u64,
    control_len: u64,
) -> Result<Vec<PassedHandle>, LinuxSysopError> {
    let len = usize::try_from(control_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Ok(Vec::new());
    }
    if len > MAX_SOCKET_CONTROL_BYTES {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let mut bytes = vec![0_u8; len];
    usermem::copy_from_current_user_exact(control_ptr, &mut bytes)?;

    let mut rights = Vec::new();
    let mut offset = 0usize;
    let header_len = cmsg_align(size_of::<linux_abi::LinuxCmsghdr>());
    while offset + header_len <= bytes.len() {
        let header = read_cmsghdr_at(&bytes, offset)?;
        let cmsg_len =
            usize::try_from(header.cmsg_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        if cmsg_len < header_len || offset + cmsg_len > bytes.len() {
            return Err(LinuxSysopError::InvalidArgument);
        }

        let data_start = offset + header_len;
        let data_end = offset + cmsg_len;
        if header.cmsg_level != linux_abi::SOL_SOCKET as u32
            || header.cmsg_type != linux_abi::SCM_RIGHTS as u32
        {
            return Err(LinuxSysopError::OperationNotSupported);
        }
        if (data_end - data_start) % size_of::<i32>() != 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }

        for fd_bytes in bytes[data_start..data_end].chunks_exact(size_of::<i32>()) {
            let fd = i32::from_le_bytes([fd_bytes[0], fd_bytes[1], fd_bytes[2], fd_bytes[3]]);
            rights.push(passed_handle_for_fd(fd as u64)?);
        }

        offset = offset
            .checked_add(cmsg_align(cmsg_len))
            .ok_or(LinuxSysopError::InvalidArgument)?;
    }

    Ok(rights)
}

fn write_passed_handles_to_control(
    control_ptr: u64,
    control_len: u64,
    rights: &[PassedHandle],
    close_on_exec: bool,
    msg_flags: &mut u32,
) -> Result<u64, LinuxSysopError> {
    if rights.is_empty() {
        return Ok(0);
    }

    let data_len = rights
        .len()
        .checked_mul(size_of::<i32>())
        .ok_or(LinuxSysopError::InvalidArgument)?;
    let required = cmsg_space(data_len);
    let available = usize::try_from(control_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if control_ptr == 0 || available < required {
        *msg_flags |= linux_abi::MSG_CTRUNC as u32;
        return Ok(0);
    }

    let mut received_fds = Vec::with_capacity(rights.len());
    let fd_flags = if close_on_exec { FD_CLOEXEC } else { 0 };
    let Some(result): Option<Result<(), LinuxSysopError>> =
        multitask::with_current_user_process_state_mut(|_, _, process_state| {
            for right in rights {
                ensure_handle_transfer_allowed(right.handle(), right.rights())?;
                let fd = process_state
                    .handles_mut()
                    .install_entry(HandleEntry::new_with_rights(
                        right.handle().clone(),
                        right.rights(),
                        fd_flags,
                        right.status_flags(),
                    ));
                received_fds.push(i32::try_from(fd).map_err(|_| LinuxSysopError::InvalidArgument)?);
            }
            Ok(())
        })
    else {
        return Err(LinuxSysopError::Unsupported);
    };
    result?;

    let header = linux_abi::LinuxCmsghdr {
        cmsg_len: cmsg_len(data_len) as u64,
        cmsg_level: linux_abi::SOL_SOCKET as u32,
        cmsg_type: linux_abi::SCM_RIGHTS as u32,
    };
    let mut bytes = vec![0_u8; required];
    let header_bytes = unsafe {
        slice::from_raw_parts(
            core::ptr::addr_of!(header).cast::<u8>(),
            size_of::<linux_abi::LinuxCmsghdr>(),
        )
    };
    bytes[..size_of::<linux_abi::LinuxCmsghdr>()].copy_from_slice(header_bytes);
    let data_offset = cmsg_align(size_of::<linux_abi::LinuxCmsghdr>());
    for (index, fd) in received_fds.into_iter().enumerate() {
        let start = data_offset + index * size_of::<i32>();
        bytes[start..start + size_of::<i32>()].copy_from_slice(&fd.to_le_bytes());
    }
    usermem::write_current_user_bytes(control_ptr, &bytes)?;
    Ok(required as u64)
}

fn passed_handle_for_fd(fd: u64) -> Result<PassedHandle, LinuxSysopError> {
    let Some(result) = multitask::with_current_user_process_state(|_, _, process_state| {
        let Some(entry) = process_state.handles().get_entry(fd) else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };
        ensure_handle_transfer_allowed(entry.handle(), entry.rights())?;
        Ok(PassedHandle::new_with_rights(
            entry.handle().clone(),
            entry.status_flags(),
            entry.rights(),
        ))
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };
    result
}

fn ensure_handle_transfer_allowed(
    handle: &KernelHandle,
    rights: kernel_object::api::handle::HandleRights,
) -> Result<(), LinuxSysopError> {
    if handle.supports_descriptor_transfer(rights) {
        Ok(())
    } else {
        Err(LinuxSysopError::PermissionDenied)
    }
}

fn read_cmsghdr_at(
    bytes: &[u8],
    offset: usize,
) -> Result<linux_abi::LinuxCmsghdr, LinuxSysopError> {
    let end = offset
        .checked_add(size_of::<linux_abi::LinuxCmsghdr>())
        .ok_or(LinuxSysopError::InvalidArgument)?;
    let header_bytes = bytes
        .get(offset..end)
        .ok_or(LinuxSysopError::InvalidArgument)?;
    let mut header = linux_abi::LinuxCmsghdr::default();
    unsafe {
        core::ptr::copy_nonoverlapping(
            header_bytes.as_ptr(),
            core::ptr::addr_of_mut!(header).cast::<u8>(),
            size_of::<linux_abi::LinuxCmsghdr>(),
        );
    }
    Ok(header)
}

fn cmsg_align(len: usize) -> usize {
    (len + size_of::<usize>() - 1) & !(size_of::<usize>() - 1)
}

fn cmsg_len(data_len: usize) -> usize {
    cmsg_align(size_of::<linux_abi::LinuxCmsghdr>()) + data_len
}

fn cmsg_space(data_len: usize) -> usize {
    cmsg_align(size_of::<linux_abi::LinuxCmsghdr>()) + cmsg_align(data_len)
}

#[cfg(test)]
mod tests {
    use super::ensure_handle_transfer_allowed;
    use crate::user::epoll::EpollHandle;
    use crate::user::handles::{
        ConsoleStreamKind, DisplaySurfaceHandle, KernelHandle, VfsDirectoryHandle, VfsFileHandle,
    };
    use crate::user::memfd::MemfdHandle;
    use crate::user::socket::SocketHandle;
    use alloc::vec;
    use kernel_object::api::handle::{FileHandleRights, HandleRights};

    fn transfer_allowed(handle: &KernelHandle) -> bool {
        ensure_handle_transfer_allowed(handle, handle.default_rights(0)).is_ok()
    }

    #[test]
    fn scm_rights_whitelist_allows_only_safe_handle_classes() {
        assert!(transfer_allowed(&KernelHandle::Socket(
            SocketHandle::socketpair(Default::default()).0,
        )));
        assert!(transfer_allowed(&KernelHandle::Memfd(MemfdHandle::new(
            "test".into(),
            true,
        ))));
        assert!(transfer_allowed(&KernelHandle::VfsFile(
            VfsFileHandle::read_only_memory("/test".into(), vec![]),
        )));

        assert!(!transfer_allowed(&KernelHandle::Console(
            ConsoleStreamKind::Output,
        )));
        assert!(!transfer_allowed(&KernelHandle::Epoll(EpollHandle::new())));
        assert!(transfer_allowed(&KernelHandle::VfsDirectory(
            VfsDirectoryHandle::new("/test".into(), vec![]),
        )));
        assert!(!transfer_allowed(&KernelHandle::DisplaySurface(
            DisplaySurfaceHandle::new(16, 16, crate::user::abi::device::PIXEL_FORMAT_BGRA8888, 1)
                .expect("surface"),
        )));
    }

    #[test]
    fn scm_rights_rejects_transferable_class_without_transfer_right() {
        let handle = KernelHandle::VfsFile(VfsFileHandle::read_only_memory("/test".into(), vec![]));
        assert!(
            ensure_handle_transfer_allowed(&handle, HandleRights::File(FileHandleRights::READ))
                .is_err()
        );
        assert!(
            ensure_handle_transfer_allowed(
                &handle,
                HandleRights::File(FileHandleRights::READ.union(FileHandleRights::TRANSFER))
            )
            .is_ok()
        );
    }
}
