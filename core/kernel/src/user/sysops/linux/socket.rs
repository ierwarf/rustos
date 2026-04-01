use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::mem::size_of;
use core::slice;

use crate::multitask;
use crate::user::handles::{FD_CLOEXEC, HandleEntry, KernelHandle};
use crate::user::socket::{PassedHandle, SocketHandle};

use super::*;

pub(crate) fn socket(domain: u64, type_: u64, protocol: u64) -> Result<u64, LinuxSysopError> {
    if domain != linux_abi::AF_UNIX {
        return Err(LinuxSysopError::AddressFamilyNotSupported);
    }
    if protocol != 0 {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    let (status_flags, fd_flags) = parse_stream_socket_type(type_)?;
    install_socket(SocketHandle::new_unix_stream(), status_flags, fd_flags)
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
    let (left, right) = SocketHandle::socketpair();
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
    let path = read_sockaddr_un_path(addr_ptr, addr_len)?;
    let (socket, _) = socket_handle_for_fd(fd)?;
    socket.connect(path.as_str()).map_err(Into::into)
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

pub(crate) fn write_current_process_socket(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
) -> Result<Option<usize>, LinuxSysopError> {
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

fn current_socket_for_fd(fd: u64) -> Result<Option<(SocketHandle, u64)>, LinuxSysopError> {
    if fd < 3 {
        return Ok(None);
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let Some(entry) = process_state.handles().get_entry(fd) else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };
        match entry.handle() {
            KernelHandle::Socket(socket) => Ok(Some((socket.clone(), entry.status_flags()))),
            _ => Ok(None),
        }
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn socket_handle_for_fd(fd: u64) -> Result<(SocketHandle, u64), LinuxSysopError> {
    if fd < 3 {
        return Err(LinuxSysopError::NotSocket);
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let Some(entry) = process_state.handles().get_entry(fd) else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };
        match entry.handle() {
            KernelHandle::Socket(socket) => Ok((socket.clone(), entry.status_flags())),
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
        return Err(LinuxSysopError::OperationNotSupported);
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
                let fd = process_state.handles_mut().install_entry(HandleEntry::new(
                    right.handle().clone(),
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
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let Some(entry) = process_state.handles().get_entry(fd) else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };
        Ok(PassedHandle::new(
            entry.handle().clone(),
            entry.status_flags(),
        ))
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };
    result
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
