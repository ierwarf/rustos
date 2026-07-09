// RING3-MIGRATION-REFERENCE START: vfsd/netd fd-usercopy substrate exception.
// vfsd/netd own file/socket policy. Ring0 keeps current-process user-copy,
// fd-table mutation, and remote-handle/socket-token installation substrate.
use super::*;

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
    if should_try_devmgrd_open(path.as_str()) {
        return match open_device_via_devmgrd(path.as_str(), flags) {
            Ok(fd) => fd,
            Err(errno) => linux_errno(errno),
        };
    }
    let mut request = new_vfs_request(VFS_IPC_OP_OPENAT);
    request.arg0 = flags;
    request.arg1 = mode;
    if let Err(errno) = populate_vfs_path_base(&mut request, dirfd, &path) {
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
    let handle_path = if response.payload_len > 0 {
        let len = response.payload_len as usize;
        match core::str::from_utf8(&response.payload[..len]) {
            Ok(path) => alloc::string::String::from(path),
            Err(_) => return linux_errno(LINUX_EINVAL),
        }
    } else {
        alloc::string::String::from(path.as_str())
    };
    let handle = multitask::KernelHandle::RemoteVfs(multitask::RemoteVfsHandle::new(
        response.remote_id,
        kind,
        handle_path,
        response.value,
        response.aux as u16,
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
    if current_socket_fd(fd) {
        let result = syscall_linux_net4(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, fd, 0, 0, 0);
        if is_linux_error(result) {
            return result;
        }
    } else if let Some(remote) = current_remote_vfs_handle(fd) {
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
        process_state.handles_mut().close(fd)
    }) {
        Some(Some(_)) => 0,
        _ => linux_errno(LINUX_EBADF),
    }
}

pub fn syscall_linux_vfs_dup(oldfd: u64, newfd: u64, flags: u64, mode: VfsDupMode) -> u64 {
    if matches!(mode, VfsDupMode::Dup3) && (oldfd == newfd || flags & !linux_abi::O_CLOEXEC != 0) {
        return linux_errno(LINUX_EINVAL);
    }

    let socket_token = current_socket_token(oldfd);
    let remote = current_remote_vfs_handle(oldfd);
    if socket_token.is_none() && remote.is_none() && !current_fd_exists(oldfd) {
        return linux_errno(LINUX_EBADF);
    }
    if matches!(mode, VfsDupMode::Dup2) && oldfd == newfd {
        return oldfd;
    }

    let close_on_exec = flags & linux_abi::O_CLOEXEC != 0;
    let replaced_socket = match (socket_token, mode) {
        (Some(_), VfsDupMode::Dup2 | VfsDupMode::Dup3) => current_socket_token(newfd),
        _ => None,
    };
    let replaced_remote = match mode {
        VfsDupMode::Dup => None,
        VfsDupMode::Dup2 | VfsDupMode::Dup3 => current_remote_vfs_handle(newfd),
    };
    if let Some(token) = socket_token {
        if let Err(errno) = call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_DUP, token) {
            return linux_errno(errno);
        }
    }
    if let Some(remote) = remote.as_ref() {
        let mut request = new_vfs_request(VFS_IPC_OP_DUP);
        request.fd = oldfd;
        request.remote_id = remote.remote_id();
        if let Err(errno) =
            call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response))
        {
            if let Some(token) = socket_token {
                let _ = call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, token);
            }
            return linux_errno(errno);
        }
    }
    let result = multitask::with_current_user_process_state_mut(|_, _, process_state| match mode {
        VfsDupMode::Dup => process_state.handles_mut().duplicate_min(oldfd, 0, false),
        VfsDupMode::Dup2 => process_state
            .handles_mut()
            .duplicate_exact(oldfd, newfd, false),
        VfsDupMode::Dup3 => {
            process_state
                .handles_mut()
                .duplicate_exact(oldfd, newfd, close_on_exec)
        }
    })
    .flatten();
    let Some(fd) = result else {
        if let Some(token) = socket_token {
            let _ = call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, token);
        }
        if let Some(remote) = remote.as_ref() {
            let _ = call_vfs_remote_close(remote.remote_id());
        }
        return linux_errno(LINUX_EBADF);
    };

    if let Some(token) = replaced_socket {
        let _ = call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, token);
    }
    if let Some(remote) = replaced_remote {
        let _ = call_vfs_remote_close(remote.remote_id());
    }
    fd
}

pub fn syscall_linux_vfs_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    match cmd {
        linux_abi::F_DUPFD | linux_abi::F_DUPFD_CLOEXEC => {
            let Ok(min_fd) = i32::try_from(arg) else {
                return linux_errno(LINUX_EINVAL);
            };
            if min_fd < 0 {
                return linux_errno(LINUX_EINVAL);
            }
            let socket_token = current_socket_token(fd);
            let remote = current_remote_vfs_handle(fd);
            if socket_token.is_none() && remote.is_none() && !current_fd_exists(fd) {
                return linux_errno(LINUX_EBADF);
            }
            if let Some(token) = socket_token {
                if let Err(errno) = call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_DUP, token) {
                    return linux_errno(errno);
                }
            }
            if let Some(remote) = remote.as_ref() {
                let mut request = new_vfs_request(VFS_IPC_OP_DUP);
                request.fd = fd;
                request.remote_id = remote.remote_id();
                if let Err(errno) =
                    call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response))
                {
                    if let Some(token) = socket_token {
                        let _ = call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, token);
                    }
                    return linux_errno(errno);
                }
            }
            let close_on_exec = cmd == linux_abi::F_DUPFD_CLOEXEC;
            let result = multitask::with_current_user_process_state_mut(|_, _, process_state| {
                process_state
                    .handles_mut()
                    .duplicate_min(fd, min_fd as u64, close_on_exec)
            })
            .flatten();
            match result {
                Some(new_fd) => new_fd,
                None => {
                    if let Some(token) = socket_token {
                        let _ = call_netd_socket_token_op(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, token);
                    }
                    if let Some(remote) = remote.as_ref() {
                        let _ = call_vfs_remote_close(remote.remote_id());
                    }
                    linux_errno(LINUX_EBADF)
                }
            }
        }
        linux_abi::F_GETFD | linux_abi::F_SETFD | linux_abi::F_GETFL | linux_abi::F_SETFL => {
            if fd <= 2 {
                return match cmd {
                    linux_abi::F_GETFD => 0,
                    linux_abi::F_SETFD => 0,
                    linux_abi::F_GETFL => linux_abi::O_RDWR as u64,
                    linux_abi::F_SETFL => 0,
                    _ => unreachable!(),
                };
            }
            if matches!(cmd, linux_abi::F_GETFL | linux_abi::F_SETFL) {
                if let Some(remote) = current_remote_vfs_handle(fd) {
                    let mut request = new_vfs_request(VFS_IPC_OP_FCNTL);
                    request.fd = fd;
                    request.remote_id = remote.remote_id();
                    request.arg0 = cmd;
                    request.arg1 = arg;
                    let value = match call_vfs_ipc_request(&request).and_then(|response| {
                        ensure_vfs_status(&response)?;
                        Ok(response.value)
                    }) {
                        Ok(value) => value,
                        Err(errno) => return linux_errno(errno),
                    };
                    if cmd == linux_abi::F_SETFL {
                        let _ = multitask::with_current_user_process_state_mut(
                            |_, _, process_state| {
                                process_state
                                    .handles_mut()
                                    .get_entry_mut(fd)
                                    .map(|entry| entry.set_status_flags(value));
                            },
                        );
                        return 0;
                    }
                    return value;
                }
            }
            match multitask::with_current_user_process_state_mut(|_, _, process_state| {
                let entry = process_state.handles_mut().get_entry_mut(fd)?;
                Some(match cmd {
                    linux_abi::F_GETFD => entry.fd_flags() as u64,
                    linux_abi::F_SETFD => {
                        entry.set_fd_flags(arg as u32);
                        0
                    }
                    linux_abi::F_GETFL => entry.status_flags(),
                    linux_abi::F_SETFL => {
                        entry.set_status_flags(arg);
                        0
                    }
                    _ => unreachable!(),
                })
            })
            .flatten()
            {
                Some(value) => value,
                None => linux_errno(LINUX_EBADF),
            }
        }
        _ => {
            if let Some(memfd) = current_memfd_handle(fd) {
                return match cmd {
                    linux_abi::F_GET_SEALS => memfd.seals() as u64,
                    linux_abi::F_ADD_SEALS => match memfd.add_seals(arg as u32) {
                        Ok(()) => 0,
                        Err(err) => linux_errno(memfd_error_to_linux_errno(err)),
                    },
                    _ => linux_errno(LINUX_EINVAL),
                };
            }
            let Some(remote) = current_remote_vfs_handle(fd) else {
                return linux_errno(LINUX_EBADF);
            };
            let mut request = new_vfs_request(VFS_IPC_OP_FCNTL);
            request.fd = fd;
            request.remote_id = remote.remote_id();
            request.arg0 = cmd;
            request.arg1 = arg;
            match call_vfs_ipc_request(&request).and_then(|response| {
                ensure_vfs_status(&response)?;
                Ok(response.value)
            }) {
                Ok(value) => value,
                Err(errno) => linux_errno(errno),
            }
        }
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
    if current_socket_fd(fd) {
        return syscall_linux_net6(
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
            fd,
            user_ptr,
            user_len,
            0,
            0,
            0,
        );
    }
    if let Some(mut memfd) = current_memfd_handle(fd) {
        return read_local_vfs_file(user_ptr, user_len, |_, chunk| memfd.read_into(chunk));
    }
    if let Some((inputd_access, status_flags)) = current_input_device_access(fd) {
        return read_input_device_via_inputd(fd, user_ptr, user_len, inputd_access, status_flags);
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    if remote.kind() == multitask::RemoteVfsHandleKind::Device
        && matches!(
            remote.device_access(),
            INPUTD_ACCESS_NATIVE | INPUTD_ACCESS_EVDEV
        )
    {
        let access = remote.device_access();
        let status_flags = current_fd_status_flags(fd).unwrap_or(0);
        return read_input_device_via_inputd(fd, user_ptr, user_len, access, status_flags);
    }
    read_remote_vfs(fd, remote.remote_id(), user_ptr, user_len, None)
}

pub fn syscall_linux_vfs_pread64(fd: u64, user_ptr: u64, user_len: u64, offset: u64) -> u64 {
    if let Some(memfd) = current_memfd_handle(fd) {
        let Ok(offset) = usize::try_from(offset) else {
            return linux_errno(LINUX_EINVAL);
        };
        return read_local_vfs_file(user_ptr, user_len, |copied, chunk| {
            memfd.read_at(offset.saturating_add(copied), chunk)
        });
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    read_remote_vfs(fd, remote.remote_id(), user_ptr, user_len, Some(offset))
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
    if current_socket_fd(fd) {
        return syscall_linux_net6(
            SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
            fd,
            user_ptr,
            user_len,
            0,
            0,
            0,
        );
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
    write_remote_vfs(fd, remote.remote_id(), user_ptr, user_len)
}

pub fn syscall_linux_vfs_writev(fd: u64, iov_ptr: u64, iovcnt: u64) -> u64 {
    if current_socket_fd(fd) {
        let bytes = match read_socket_writev_payload(iov_ptr, iovcnt) {
            Ok(bytes) => bytes,
            Err(errno) => return linux_errno(errno),
        };
        if bytes.is_empty() {
            return 0;
        }
        let mut written = 0usize;
        while written < bytes.len() {
            let chunk = &bytes[written..];
            let count = match call_netd_socket_payload(fd, SYSCALL_OFFLOAD_OP_LINUX_SENDTO, chunk) {
                Ok(count) => count as usize,
                Err(errno) => {
                    return if written == 0 {
                        linux_errno(errno)
                    } else {
                        written as u64
                    };
                }
            };
            if count == 0 {
                break;
            }
            written = written.saturating_add(count);
        }
        return written as u64;
    }
    writev_via_write(fd, iov_ptr, iovcnt)
}

pub fn is_linux_error(result: u64) -> bool {
    let signed = result as i64;
    (-4095..0).contains(&signed)
}

fn current_fd_exists(fd: u64) -> bool {
    fd <= 2
        || multitask::with_current_user_process_state(|_, _, process_state| {
            process_state.handles().get_entry(fd).is_some()
        })
        .unwrap_or(false)
}

fn call_vfs_remote_close(remote_id: u64) -> Result<(), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_CLOSE);
    request.remote_id = remote_id;
    call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response))
}

fn read_local_vfs_file<F>(user_ptr: u64, user_len: u64, mut read: F) -> u64
where
    F: FnMut(usize, &mut [u8]) -> usize,
{
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
        let read_len = read(copied, &mut chunk[..chunk_len]);
        if read_len == 0 {
            break;
        }
        let Some(dest) = user_ptr.checked_add(copied as u64) else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::write_current_user_bytes(dest, &chunk[..read_len]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        copied += read_len;
        multitask::cond_resched();
        if read_len < chunk_len {
            break;
        }
    }
    copied as u64
}

fn read_remote_vfs(
    fd: u64,
    remote_id: u64,
    user_ptr: u64,
    user_len: u64,
    offset: Option<u64>,
) -> u64 {
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
        let mut request = new_vfs_request(if offset.is_some() {
            VFS_IPC_OP_PREAD64
        } else {
            VFS_IPC_OP_READ
        });
        request.fd = fd;
        request.remote_id = remote_id;
        request.arg1 = chunk_len as u64;
        if let Some(offset) = offset {
            request.arg0 = offset.saturating_add(copied as u64);
        }
        let response = match call_vfs_ipc_request(&request) {
            Ok(response) => response,
            Err(errno) => return linux_errno(errno),
        };
        if let Err(errno) = ensure_vfs_status(&response) {
            return linux_errno(errno);
        }
        let read = response.payload_len as usize;
        if read > chunk_len || read > response.payload.len() {
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

fn write_remote_vfs(fd: u64, remote_id: u64, user_ptr: u64, user_len: u64) -> u64 {
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    let chunk_len = user_len.min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
    let mut request = new_vfs_request(VFS_IPC_OP_WRITE);
    request.fd = fd;
    request.remote_id = remote_id;
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

fn writev_via_write(fd: u64, iov_ptr: u64, iovcnt: u64) -> u64 {
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
        let iov = match usermem::read_current_user_struct::<linux_abi::LinuxIovec>(entry_ptr) {
            Ok(iov) => iov,
            Err(err) => {
                return if total == 0 {
                    linux_errno(address_space_error_to_linux_errno(err))
                } else {
                    total
                };
            }
        };
        let mut written_for_iov = 0_u64;
        while written_for_iov < iov.iov_len {
            let Some(chunk_ptr) = iov.iov_base.checked_add(written_for_iov) else {
                return if total == 0 {
                    linux_errno(LINUX_EFAULT)
                } else {
                    total
                };
            };
            let chunk_len =
                (iov.iov_len - written_for_iov).min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY as u64);
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

fn current_socket_fd(fd: u64) -> bool {
    multitask::with_current_user_process_state(|_, _, process_state| {
        matches!(
            process_state.handles().get(fd),
            Some(multitask::KernelHandle::Socket(_) | multitask::KernelHandle::InetSocket(_))
        )
    })
    .unwrap_or(false)
}

fn current_socket_token(fd: u64) -> Option<u64> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::Socket(socket)) => Some(socket.token_id()),
            Some(multitask::KernelHandle::InetSocket(socket)) => Some(socket.token_id()),
            _ => None,
        }
    })
    .flatten()
}

fn current_socket_token_and_flags(fd: u64) -> Option<(u64, u64)> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let entry = process_state.handles().get_entry(fd)?;
        match entry.handle() {
            multitask::KernelHandle::Socket(socket) => {
                Some((socket.token_id(), entry.status_flags()))
            }
            multitask::KernelHandle::InetSocket(socket) => {
                Some((socket.token_id(), entry.status_flags()))
            }
            _ => None,
        }
    })
    .flatten()
}

fn new_netd_socket_request(op: u16, socket_token: u64) -> NetdIpcRequest {
    let mut request = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op,
        socket_token,
        ..NetdIpcRequest::default()
    };
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }
    request
}

fn call_netd_socket_token_op(op: u16, socket_token: u64) -> Result<u64, i64> {
    let request = new_netd_socket_request(op, socket_token);
    call_netd_ipc_request(&request).map(|response| response.value)
}

fn call_netd_socket_payload(fd: u64, op: u16, payload: &[u8]) -> Result<u64, i64> {
    let Some((token, status_flags)) = current_socket_token_and_flags(fd) else {
        return Err(LINUX_EBADF);
    };
    if payload.len() > NETD_IPC_PAYLOAD_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let mut request = new_netd_socket_request(op, token);
    request.arg0 = fd;
    request.status_flags = status_flags;
    request.payload_len = payload.len() as u32;
    request.payload[..payload.len()].copy_from_slice(payload);
    call_netd_ipc_request(&request).map(|response| response.value)
}

fn read_socket_writev_payload(iov_ptr: u64, iovcnt: u64) -> Result<Vec<u8>, i64> {
    let Ok(iovcnt) = usize::try_from(iovcnt) else {
        return Err(LINUX_EINVAL);
    };
    if iovcnt > 16 {
        return Err(LINUX_EINVAL);
    }
    let mut payload = Vec::new();
    for index in 0..iovcnt {
        let Some(entry_ptr) =
            iov_ptr.checked_add((index * size_of::<linux_abi::LinuxIovec>()) as u64)
        else {
            return Err(LINUX_EFAULT);
        };
        let iov = usermem::read_current_user_struct::<linux_abi::LinuxIovec>(entry_ptr)
            .map_err(address_space_error_to_linux_errno)?;
        let len = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?;
        if len == 0 {
            continue;
        }
        if payload.len().saturating_add(len) > NETD_IPC_PAYLOAD_CAPACITY {
            return Err(LINUX_EINVAL);
        }
        let start = payload.len();
        payload.resize(start + len, 0);
        usermem::copy_from_current_user_exact(iov.iov_base, &mut payload[start..])
            .map_err(address_space_error_to_linux_errno)?;
    }
    Ok(payload)
}
// RING3-MIGRATION-REFERENCE END: vfsd/netd fd-usercopy substrate exception.
