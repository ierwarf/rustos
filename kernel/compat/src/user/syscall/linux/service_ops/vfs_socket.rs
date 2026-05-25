use super::*;

const MAX_SOCKET_IO_BYTES: usize = 64 * 1024;
const MAX_IOVEC_COUNT: usize = 16;

pub fn syscall_linux_vfs_openat(dirfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    if path.contains("startup-programs") || path.contains("runtime-env") {
        crate::debug::info!(
            compat,
            "bootstrap path probe: op=openat dirfd={:#x} flags={:#x} path={}",
            dirfd,
            flags,
            path
        );
    }
    if is_devmgrd_open_path(path.as_str()) {
        match open_device_via_devmgrd(path.as_str(), flags) {
            Ok(fd) => return fd,
            Err(errno) => return linux_errno(errno),
        }
    }
    let mut request = new_vfs_request(VFS_IPC_OP_OPENAT);
    request.dirfd = dirfd;
    request.arg0 = flags;
    request.arg1 = mode;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let kind = match response.handle_kind {
        VFS_IPC_HANDLE_KIND_FILE => multitask::RemoteVfsHandleKind::File,
        VFS_IPC_HANDLE_KIND_DIR => multitask::RemoteVfsHandleKind::Directory,
        VFS_IPC_HANDLE_KIND_DEVICE => multitask::RemoteVfsHandleKind::Device,
        _ => return linux_errno(LINUX_EINVAL),
    };
    let handle = multitask::KernelHandle::RemoteVfs(multitask::RemoteVfsHandle::new(
        response.remote_id,
        kind,
        String::from(path.as_str()),
        response.value,
    ));
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_with_open_flags(handle, flags)
    }) {
        Some(fd) => fd,
        None => linux_errno(LINUX_EINVAL),
    }
}

pub fn syscall_linux_vfs_close(fd: u64) -> u64 {
    if let Some(remote) = current_remote_vfs_handle(fd) {
        let mut request = new_vfs_request(VFS_IPC_OP_CLOSE);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        if let Err(errno) =
            call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response))
        {
            return linux_errno(errno);
        }
    }
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().close(fd).is_some()
    }) {
        Some(true) => 0,
        _ => linux_errno(LINUX_EBADF),
    }
}

pub fn syscall_linux_vfs_dup(oldfd: u64, newfd: u64, flags: u64, mode: VfsDupMode) -> u64 {
    if let Some(remote) = current_remote_vfs_handle(oldfd) {
        let mut request = new_vfs_request(VFS_IPC_OP_DUP);
        request.fd = oldfd;
        request.remote_id = remote.remote_id();
        if let Err(errno) =
            call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response))
        {
            return linux_errno(errno);
        }
    }
    let close_on_exec = flags & linux_abi::O_CLOEXEC != 0;
    let result = multitask::with_current_user_process_state_mut(|_, _, process_state| match mode {
        VfsDupMode::Dup => process_state.handles_mut().duplicate_min(oldfd, 0, false),
        VfsDupMode::Dup2 if oldfd == newfd => {
            process_state.handles().get_entry(oldfd).map(|_| oldfd)
        }
        VfsDupMode::Dup2 => process_state
            .handles_mut()
            .duplicate_exact(oldfd, newfd, false),
        VfsDupMode::Dup3 => {
            if oldfd == newfd || flags & !linux_abi::O_CLOEXEC != 0 {
                None
            } else {
                process_state
                    .handles_mut()
                    .duplicate_exact(oldfd, newfd, close_on_exec)
            }
        }
    });
    match result.flatten() {
        Some(fd) => fd,
        None => linux_errno(LINUX_EBADF),
    }
}

pub fn syscall_linux_vfs_read(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if fd == 0 {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if !current_console_session_is_system() {
            return match console_read_via_sessiond(user_ptr, user_len) {
                Ok(read) => read,
                Err(errno) => linux_errno(errno),
            };
        }
        return match crate::user::sysops::console::read_into_current_process(user_ptr, user_len) {
            Ok(read) => read as u64,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    if let Some((socket, nonblocking)) = current_socket_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let mut bytes = alloc::vec![0_u8; user_len.min(VFS_IPC_PAYLOAD_CAPACITY)];
        let read = match socket.recv(bytes.as_mut_slice(), nonblocking) {
            Ok(read) => read,
            Err(err) => return linux_errno(socket_error_to_linux_errno(err)),
        };
        return match usermem::write_current_user_bytes(user_ptr, &bytes[..read]) {
            Ok(()) => read as u64,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    if let Some(mut file) = current_vfs_file_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let mut copied = 0usize;
        let mut chunk = alloc::vec![0_u8; user_len.min(64 * 1024)];
        while copied < user_len {
            let chunk_len = (user_len - copied).min(chunk.len());
            let read = file.read_into(&mut chunk[..chunk_len]);
            if read == 0 {
                break;
            }
            let Some(dest) = user_ptr.checked_add(copied as u64) else {
                return linux_errno(LINUX_EINVAL);
            };
            if let Err(err) = usermem::write_current_user_bytes(dest, &chunk[..read]) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            copied += read;
            multitask::cond_resched();
            if read < chunk_len {
                break;
            }
        }
        return copied as u64;
    }
    if let Some(mut memfd) = current_memfd_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let mut copied = 0usize;
        let mut chunk = alloc::vec![0_u8; user_len.min(64 * 1024)];
        while copied < user_len {
            let chunk_len = (user_len - copied).min(chunk.len());
            let read = memfd.read_into(&mut chunk[..chunk_len]);
            if read == 0 {
                break;
            }
            let Some(dest) = user_ptr.checked_add(copied as u64) else {
                return linux_errno(LINUX_EINVAL);
            };
            if let Err(err) = usermem::write_current_user_bytes(dest, &chunk[..read]) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            copied += read;
            multitask::cond_resched();
            if read < chunk_len {
                break;
            }
        }
        return copied as u64;
    }
    if let Some(inputd_access) = current_input_device_access(fd) {
        return read_input_device_via_inputd(fd, user_ptr, user_len, inputd_access);
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    if remote.kind() == multitask::RemoteVfsHandleKind::Device
        && matches!(remote.path().as_str(), "/dev/input0" | "/dev/input/event0")
    {
        let is_evdev = remote.path().as_str() == "/dev/input/event0";
        let inputd_access = if is_evdev {
            INPUTD_ACCESS_EVDEV
        } else {
            INPUTD_ACCESS_NATIVE
        };
        return read_input_device_via_inputd(fd, user_ptr, user_len, inputd_access);
    }
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut copied = 0usize;
    while copied < user_len {
        let chunk_len = (user_len - copied).min(VFS_IPC_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(VFS_IPC_OP_READ);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        request.arg1 = chunk_len as u64;
        let response = match call_vfs_ipc_request(&request) {
            Ok(response) => response,
            Err(errno) => return linux_errno(errno),
        };
        if let Err(errno) = ensure_vfs_status(&response) {
            return linux_errno(errno);
        }
        let read = response.payload_len as usize;
        if read > chunk_len {
            return linux_errno(LINUX_EINVAL);
        }
        if read == 0 {
            break;
        }
        let Some(dest) = user_ptr.checked_add(copied as u64) else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::write_current_user_bytes(dest, &response.payload[..read]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        copied += read;
        multitask::cond_resched();
        if read < chunk_len {
            break;
        }
    }
    copied as u64
}

pub fn syscall_linux_vfs_pread64(fd: u64, user_ptr: u64, user_len: u64, offset: u64) -> u64 {
    if let Some(file) = current_vfs_file_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        let Ok(offset) = usize::try_from(offset) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let mut copied = 0usize;
        let mut chunk = alloc::vec![0_u8; user_len.min(64 * 1024)];
        while copied < user_len {
            let chunk_len = (user_len - copied).min(chunk.len());
            let read = file.read_at(offset.saturating_add(copied), &mut chunk[..chunk_len]);
            if read == 0 {
                break;
            }
            let Some(dest) = user_ptr.checked_add(copied as u64) else {
                return linux_errno(LINUX_EINVAL);
            };
            if let Err(err) = usermem::write_current_user_bytes(dest, &chunk[..read]) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            copied += read;
            multitask::cond_resched();
            if read < chunk_len {
                break;
            }
        }
        return copied as u64;
    }
    if let Some(memfd) = current_memfd_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        let Ok(offset) = usize::try_from(offset) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let mut copied = 0usize;
        let mut chunk = alloc::vec![0_u8; user_len.min(64 * 1024)];
        while copied < user_len {
            let chunk_len = (user_len - copied).min(chunk.len());
            let read = memfd.read_at(offset.saturating_add(copied), &mut chunk[..chunk_len]);
            if read == 0 {
                break;
            }
            let Some(dest) = user_ptr.checked_add(copied as u64) else {
                return linux_errno(LINUX_EINVAL);
            };
            if let Err(err) = usermem::write_current_user_bytes(dest, &chunk[..read]) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            copied += read;
            multitask::cond_resched();
            if read < chunk_len {
                break;
            }
        }
        return copied as u64;
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut copied = 0usize;
    while copied < user_len {
        let chunk_len = (user_len - copied).min(VFS_IPC_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(VFS_IPC_OP_PREAD64);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        request.arg0 = offset.saturating_add(copied as u64);
        request.arg1 = chunk_len as u64;
        let response = match call_vfs_ipc_request(&request) {
            Ok(response) => response,
            Err(errno) => return linux_errno(errno),
        };
        if let Err(errno) = ensure_vfs_status(&response) {
            return linux_errno(errno);
        }
        let read = response.payload_len as usize;
        if read > chunk_len {
            return linux_errno(LINUX_EINVAL);
        }
        if read == 0 {
            break;
        }
        let Some(dest) = user_ptr.checked_add(copied as u64) else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::write_current_user_bytes(dest, &response.payload[..read]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        copied += read;
        multitask::cond_resched();
        if read < chunk_len {
            break;
        }
    }
    copied as u64
}

pub fn syscall_linux_vfs_write(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if fd <= 2 {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if !current_console_session_is_system() {
            return match console_write_via_sessiond(user_ptr, user_len) {
                Ok(written) => written,
                Err(errno) => linux_errno(errno),
            };
        }
        return match crate::user::sysops::console::write_from_current_process(user_ptr, user_len) {
            Ok(written) => written as u64,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    if let Some((socket, nonblocking)) = current_socket_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        let chunk_len = user_len.min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
        let mut bytes = alloc::vec![0_u8; chunk_len];
        if let Err(err) = usermem::copy_from_current_user_exact(user_ptr, bytes.as_mut_slice()) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        return match socket.send(bytes.as_slice(), nonblocking) {
            Ok(written) => written as u64,
            Err(err) => linux_errno(socket_error_to_linux_errno(err)),
        };
    }
    if let Some(mut memfd) = current_memfd_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        let mut copied = 0usize;
        let mut chunk = [0_u8; 256];
        while copied < user_len {
            let chunk_len = (user_len - copied).min(chunk.len());
            let Some(src) = user_ptr.checked_add(copied as u64) else {
                return linux_errno(LINUX_EINVAL);
            };
            if let Err(err) = usermem::copy_from_current_user_exact(src, &mut chunk[..chunk_len]) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            let written = match memfd.write_from(&chunk[..chunk_len]) {
                Ok(written) => written,
                Err(err) => return linux_errno(memfd_error_to_linux_errno(err)),
            };
            copied += written;
            multitask::cond_resched();
            if written < chunk_len {
                break;
            }
        }
        return copied as u64;
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    let chunk_len = user_len.min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
    let mut request = new_vfs_request(VFS_IPC_OP_WRITE);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.payload_len = chunk_len as u32;
    if let Err(err) =
        usermem::copy_from_current_user_exact(user_ptr, &mut request.payload[..chunk_len])
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    match call_vfs_ipc_request(&request).and_then(|response| {
        ensure_vfs_status(&response)?;
        Ok(response.value)
    }) {
        Ok(written) => written,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_vfs_writev(fd: u64, iov_ptr: u64, iovcnt: u64) -> u64 {
    const LINUX_UIO_MAXIOV: u64 = 1024;
    if iovcnt > LINUX_UIO_MAXIOV {
        return linux_errno(LINUX_EINVAL);
    }
    let mut total = 0_u64;
    for index in 0..iovcnt {
        let Some(entry_ptr) = iov_ptr.checked_add(index.saturating_mul(16)) else {
            return if total == 0 {
                linux_errno(LINUX_EFAULT)
            } else {
                total
            };
        };
        let mut entry = [0_u8; 16];
        if let Err(err) = usermem::copy_from_current_user_exact(entry_ptr, &mut entry) {
            return if total == 0 {
                linux_errno(address_space_error_to_linux_errno(err))
            } else {
                total
            };
        }
        let base = u64::from_le_bytes(entry[..8].try_into().unwrap_or([0; 8]));
        let len = u64::from_le_bytes(entry[8..16].try_into().unwrap_or([0; 8]));
        let mut written_for_iov = 0_u64;
        while written_for_iov < len {
            let Some(chunk_ptr) = base.checked_add(written_for_iov) else {
                return if total == 0 {
                    linux_errno(LINUX_EFAULT)
                } else {
                    total
                };
            };
            let chunk_len = (len - written_for_iov).min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY as u64);
            let result = syscall_linux_vfs_write(fd, chunk_ptr, chunk_len);
            if is_linux_error(result) {
                return if total == 0 { result } else { total };
            }
            if result == 0 {
                return total;
            }
            total = match total.checked_add(result) {
                Some(value) => value,
                None => return linux_errno(LINUX_EINVAL),
            };
            written_for_iov = match written_for_iov.checked_add(result) {
                Some(value) => value,
                None => return linux_errno(LINUX_EINVAL),
            };
            if result < chunk_len {
                return total;
            }
        }
    }
    total
}

pub fn syscall_linux_socket_sendto_direct(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    flags: u64,
    name_ptr: u64,
    name_len: u64,
) -> Option<u64> {
    let (socket, status_flags) = current_socket_with_flags(fd)?;
    if name_ptr != 0 || name_len != 0 {
        return Some(linux_errno(LINUX_EOPNOTSUPP));
    }
    let result = socket_send_current(&socket, user_ptr, user_len, status_flags, flags);
    Some(result.unwrap_or_else(linux_errno))
}

pub fn syscall_linux_socket_recvfrom_direct(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    flags: u64,
    name_ptr: u64,
    name_len_ptr: u64,
) -> Option<u64> {
    let (socket, status_flags) = current_socket_with_flags(fd)?;
    let result = socket_recv_current(&socket, user_ptr, user_len, status_flags, flags);
    if result.is_ok() && name_ptr != 0 && name_len_ptr != 0 {
        if let Err(errno) = write_current_sockaddr_un(socket.peer_path().unwrap_or_default())(
            name_ptr,
            name_len_ptr,
        ) {
            return Some(linux_errno(errno));
        }
    }
    Some(result.unwrap_or_else(linux_errno))
}

pub fn syscall_linux_socket_sendmsg_direct(fd: u64, msg_ptr: u64, flags: u64) -> Option<u64> {
    let (socket, status_flags) = current_socket_with_flags(fd)?;
    let result = socket_sendmsg_current(&socket, msg_ptr, status_flags, flags);
    Some(result.unwrap_or_else(linux_errno))
}

pub fn syscall_linux_socket_recvmsg_direct(fd: u64, msg_ptr: u64, flags: u64) -> Option<u64> {
    let (socket, status_flags) = current_socket_with_flags(fd)?;
    let result = socket_recvmsg_current(&socket, msg_ptr, status_flags, flags);
    Some(result.unwrap_or_else(linux_errno))
}

fn socket_send_current(
    socket: &multitask::SocketHandle,
    user_ptr: u64,
    user_len: u64,
    status_flags: u64,
    flags: u64,
) -> Result<u64, i64> {
    let len = checked_socket_io_len(user_len)?;
    if len == 0 {
        return Ok(0);
    }
    let mut bytes = alloc::vec![0_u8; len];
    usermem::copy_from_current_user_exact(user_ptr, &mut bytes)
        .map_err(address_space_error_to_linux_errno)?;
    let nonblocking = socket_nonblocking(status_flags, flags);
    let sent = socket
        .send(bytes.as_slice(), nonblocking)
        .map_err(socket_error_to_linux_errno)?;
    Ok(sent as u64)
}

fn socket_recv_current(
    socket: &multitask::SocketHandle,
    user_ptr: u64,
    user_len: u64,
    status_flags: u64,
    flags: u64,
) -> Result<u64, i64> {
    let len = checked_socket_io_len(user_len)?;
    if len == 0 {
        return Ok(0);
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, len) {
        return Err(address_space_error_to_linux_errno(err));
    }
    let mut bytes = alloc::vec![0_u8; len];
    let nonblocking = socket_nonblocking(status_flags, flags);
    let read = socket
        .recv(bytes.as_mut_slice(), nonblocking)
        .map_err(socket_error_to_linux_errno)?;
    usermem::write_current_user_bytes(user_ptr, &bytes[..read])
        .map_err(address_space_error_to_linux_errno)?;
    Ok(read as u64)
}

fn socket_sendmsg_current(
    socket: &multitask::SocketHandle,
    msg_ptr: u64,
    status_flags: u64,
    flags: u64,
) -> Result<u64, i64> {
    let header = usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(msg_ptr)
        .map_err(address_space_error_to_linux_errno)?;
    let bytes = read_current_iovec_bytes(header.msg_iov, header.msg_iovlen)?;
    let rights = read_current_scm_rights(header.msg_control, header.msg_controllen)?;
    let nonblocking = socket_nonblocking(status_flags, flags);
    let sent = socket
        .send_message(bytes, rights, nonblocking)
        .map_err(socket_error_to_linux_errno)?;
    Ok(sent as u64)
}

fn socket_recvmsg_current(
    socket: &multitask::SocketHandle,
    msg_ptr: u64,
    status_flags: u64,
    flags: u64,
) -> Result<u64, i64> {
    let mut header = usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(msg_ptr)
        .map_err(address_space_error_to_linux_errno)?;
    let iovecs = read_current_iovecs(header.msg_iov, header.msg_iovlen)?;
    let total_len = iovec_total_len(&iovecs)?;
    let mut bytes = alloc::vec![0_u8; total_len.min(MAX_SOCKET_IO_BYTES)];
    let nonblocking = socket_nonblocking(status_flags, flags);
    let (read, rights) = socket
        .recv_with_rights(bytes.as_mut_slice(), nonblocking)
        .map_err(socket_error_to_linux_errno)?;
    write_current_iovec_bytes(&iovecs, &bytes[..read])?;
    let (rights_written, control_len) = write_current_scm_rights(
        header.msg_control,
        header.msg_controllen,
        flags,
        rights.as_slice(),
    )?;
    header.msg_namelen = 0;
    header.msg_controllen = control_len as u64;
    header.msg_flags = 0;
    if rights_written < rights.len() {
        header.msg_flags |= linux_abi::MSG_CTRUNC as u32;
    }
    usermem::write_current_user_struct(msg_ptr, &header)
        .map_err(address_space_error_to_linux_errno)?;
    Ok(read as u64)
}

fn checked_socket_io_len(len: u64) -> Result<usize, i64> {
    let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
    if len > MAX_SOCKET_IO_BYTES {
        return Err(LINUX_EINVAL);
    }
    Ok(len)
}

fn socket_nonblocking(status_flags: u64, msg_flags: u64) -> bool {
    status_flags & linux_abi::O_NONBLOCK != 0 || msg_flags & linux_abi::MSG_DONTWAIT != 0
}

fn read_current_iovecs(iov_ptr: u64, iov_len: u64) -> Result<Vec<linux_abi::LinuxIovec>, i64> {
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
        let iov = usermem::read_current_user_struct::<linux_abi::LinuxIovec>(iov_ptr + offset)
            .map_err(address_space_error_to_linux_errno)?;
        iovecs.push(iov);
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

fn read_current_iovec_bytes(iov_ptr: u64, iov_len: u64) -> Result<Vec<u8>, i64> {
    let iovecs = read_current_iovecs(iov_ptr, iov_len)?;
    let total = iovec_total_len(&iovecs)?;
    let mut bytes = Vec::with_capacity(total);
    for iov in iovecs {
        let len = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?;
        let start = bytes.len();
        bytes.resize(start + len, 0);
        usermem::copy_from_current_user_exact(iov.iov_base, &mut bytes[start..])
            .map_err(address_space_error_to_linux_errno)?;
    }
    Ok(bytes)
}

fn write_current_iovec_bytes(iovecs: &[linux_abi::LinuxIovec], bytes: &[u8]) -> Result<(), i64> {
    let mut written = 0usize;
    for iov in iovecs {
        if written >= bytes.len() {
            break;
        }
        let len = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?;
        let chunk_len = len.min(bytes.len() - written);
        usermem::write_current_user_bytes(iov.iov_base, &bytes[written..written + chunk_len])
            .map_err(address_space_error_to_linux_errno)?;
        written += chunk_len;
    }
    Ok(())
}

fn read_current_scm_rights(
    control_ptr: u64,
    control_len: u64,
) -> Result<Vec<multitask::PassedHandle>, i64> {
    let control_len = usize::try_from(control_len).map_err(|_| LINUX_EINVAL)?;
    if control_ptr == 0 || control_len == 0 {
        return Ok(Vec::new());
    }
    if control_len > MAX_SOCKET_IO_BYTES {
        return Err(LINUX_EINVAL);
    }
    let mut control = alloc::vec![0_u8; control_len];
    usermem::copy_from_current_user_exact(control_ptr, &mut control)
        .map_err(address_space_error_to_linux_errno)?;
    let mut offset = 0usize;
    let mut rights = Vec::new();
    while offset + size_of::<linux_abi::LinuxCmsghdr>() <= control.len() {
        let header = read_cmsghdr_from_bytes(&control[offset..])?;
        let cmsg_len = usize::try_from(header.cmsg_len).map_err(|_| LINUX_EINVAL)?;
        if cmsg_len < size_of::<linux_abi::LinuxCmsghdr>() || offset + cmsg_len > control.len() {
            return Err(LINUX_EINVAL);
        }
        if header.cmsg_level == linux_abi::SOL_SOCKET as u32
            && header.cmsg_type == linux_abi::SCM_RIGHTS as u32
        {
            let data_start = offset + size_of::<linux_abi::LinuxCmsghdr>();
            let data_end = offset + cmsg_len;
            for fd_bytes in control[data_start..data_end].chunks_exact(size_of::<i32>()) {
                let fd = i32::from_ne_bytes(fd_bytes.try_into().map_err(|_| LINUX_EINVAL)?);
                if fd < 0 {
                    return Err(LINUX_EBADF);
                }
                rights.push(current_passed_handle(fd as u64)?);
            }
        }
        let next = offset
            .checked_add(cmsg_align(cmsg_len))
            .ok_or(LINUX_EINVAL)?;
        if next <= offset {
            return Err(LINUX_EINVAL);
        }
        offset = next;
    }
    Ok(rights)
}

fn write_current_scm_rights(
    control_ptr: u64,
    control_len: u64,
    flags: u64,
    rights: &[multitask::PassedHandle],
) -> Result<(usize, usize), i64> {
    if rights.is_empty() || control_ptr == 0 || control_len == 0 {
        return Ok((0, 0));
    }
    let control_len = usize::try_from(control_len).map_err(|_| LINUX_EINVAL)?;
    if control_len < size_of::<linux_abi::LinuxCmsghdr>() + size_of::<i32>() {
        return Ok((0, 0));
    }
    let fd_capacity = (control_len - size_of::<linux_abi::LinuxCmsghdr>()) / size_of::<i32>();
    let send_count = fd_capacity.min(rights.len());
    if send_count == 0 {
        return Ok((0, 0));
    }
    let close_on_exec = flags & linux_abi::MSG_CMSG_CLOEXEC != 0;
    let fds = install_passed_handles(&rights[..send_count], close_on_exec)?;
    let cmsg_len = size_of::<linux_abi::LinuxCmsghdr>() + fds.len() * size_of::<i32>();
    let mut control = alloc::vec![0_u8; cmsg_align(cmsg_len)];
    let header = linux_abi::LinuxCmsghdr {
        cmsg_len: cmsg_len as u64,
        cmsg_level: linux_abi::SOL_SOCKET as u32,
        cmsg_type: linux_abi::SCM_RIGHTS as u32,
    };
    write_cmsghdr_to_bytes(&mut control[..size_of::<linux_abi::LinuxCmsghdr>()], header);
    let mut data_offset = size_of::<linux_abi::LinuxCmsghdr>();
    for fd in &fds {
        control[data_offset..data_offset + size_of::<i32>()].copy_from_slice(&fd.to_ne_bytes());
        data_offset += size_of::<i32>();
    }
    usermem::write_current_user_bytes(control_ptr, &control[..cmsg_len])
        .map_err(address_space_error_to_linux_errno)?;
    Ok((send_count, cmsg_len))
}

fn current_passed_handle(fd: u64) -> Result<multitask::PassedHandle, i64> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let entry = process_state.handles().get_entry(fd).ok_or(LINUX_EBADF)?;
        if !entry.supports_transfer() {
            return Err(LINUX_EPERM);
        }
        Ok(multitask::PassedHandle::new_with_rights(
            entry.handle().clone(),
            entry.status_flags(),
            entry.rights(),
        ))
    })
    .unwrap_or(Err(LINUX_ESRCH))
}

fn install_passed_handles(
    rights: &[multitask::PassedHandle],
    close_on_exec: bool,
) -> Result<Vec<i32>, i64> {
    multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let mut fds = Vec::with_capacity(rights.len());
        for passed in rights {
            let fd_flags = if close_on_exec {
                multitask::FD_CLOEXEC
            } else {
                0
            };
            let fd =
                process_state
                    .handles_mut()
                    .install_entry(multitask::HandleEntry::new_with_rights(
                        passed.handle().clone(),
                        passed.rights(),
                        fd_flags,
                        passed.status_flags(),
                    ));
            let fd = i32::try_from(fd).map_err(|_| LINUX_EMFILE)?;
            fds.push(fd);
        }
        Ok(fds)
    })
    .unwrap_or(Err(LINUX_ESRCH))
}

fn read_cmsghdr_from_bytes(bytes: &[u8]) -> Result<linux_abi::LinuxCmsghdr, i64> {
    if bytes.len() < size_of::<linux_abi::LinuxCmsghdr>() {
        return Err(LINUX_EINVAL);
    }
    Ok(linux_abi::LinuxCmsghdr {
        cmsg_len: u64::from_ne_bytes(bytes[0..8].try_into().map_err(|_| LINUX_EINVAL)?),
        cmsg_level: u32::from_ne_bytes(bytes[8..12].try_into().map_err(|_| LINUX_EINVAL)?),
        cmsg_type: u32::from_ne_bytes(bytes[12..16].try_into().map_err(|_| LINUX_EINVAL)?),
    })
}

fn write_cmsghdr_to_bytes(bytes: &mut [u8], header: linux_abi::LinuxCmsghdr) {
    bytes[0..8].copy_from_slice(&header.cmsg_len.to_ne_bytes());
    bytes[8..12].copy_from_slice(&header.cmsg_level.to_ne_bytes());
    bytes[12..16].copy_from_slice(&header.cmsg_type.to_ne_bytes());
}

fn cmsg_align(len: usize) -> usize {
    let align = size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

fn write_current_sockaddr_un(path: String) -> impl FnOnce(u64, u64) -> Result<(), i64> {
    move |addr_ptr, addrlen_ptr| {
        if addrlen_ptr == 0 {
            return Err(LINUX_EINVAL);
        }
        let needed = size_of::<u16>()
            .checked_add(path.len())
            .and_then(|value| value.checked_add(1))
            .ok_or(LINUX_EINVAL)?;
        if addr_ptr != 0 {
            let mut len_bytes = [0_u8; size_of::<u32>()];
            usermem::copy_from_current_user_exact(addrlen_ptr, &mut len_bytes)
                .map_err(address_space_error_to_linux_errno)?;
            let capacity = u32::from_ne_bytes(len_bytes) as usize;
            if capacity < needed || path.len() >= linux_abi::UNIX_PATH_MAX {
                return Err(LINUX_EINVAL);
            }
            let mut sockaddr = linux_abi::LinuxSockaddrUn {
                sun_family: linux_abi::AF_UNIX as u16,
                sun_path: [0; linux_abi::UNIX_PATH_MAX],
            };
            sockaddr.sun_path[..path.len()].copy_from_slice(path.as_bytes());
            usermem::write_current_user_struct(addr_ptr, &sockaddr)
                .map_err(address_space_error_to_linux_errno)?;
        }
        usermem::write_current_user_bytes(addrlen_ptr, &(needed as u32).to_ne_bytes())
            .map_err(address_space_error_to_linux_errno)
    }
}

pub fn is_linux_error(result: u64) -> bool {
    let signed = result as i64;
    (-4095..0).contains(&signed)
}
