use super::*;

// RING3-MIGRATION-COMMENTED-OUT START: vfsd should own VFS metadata syscalls
// (lseek/fstat/ftruncate/getdents64/fcntl) and the current-handle accessors.
// Ring0 keeps only the handle-table substrate.
/*
pub fn syscall_linux_vfs_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    if let Some(mut file) = current_vfs_file_handle(fd) {
        let whence = match whence {
            value if value == linux_abi::SEEK_SET => multitask::FileHandleSeekWhence::Start,
            value if value == linux_abi::SEEK_CUR => multitask::FileHandleSeekWhence::Current,
            value if value == linux_abi::SEEK_END => multitask::FileHandleSeekWhence::End,
            _ => return linux_errno(LINUX_EINVAL),
        };
        return match file.seek(offset, whence) {
            Ok(pos) => pos,
            Err(_) => linux_errno(LINUX_EINVAL),
        };
    }
    if let Some(mut memfd) = current_memfd_handle(fd) {
        let whence = match whence {
            value if value == linux_abi::SEEK_SET => multitask::FileHandleSeekWhence::Start,
            value if value == linux_abi::SEEK_CUR => multitask::FileHandleSeekWhence::Current,
            value if value == linux_abi::SEEK_END => multitask::FileHandleSeekWhence::End,
            _ => return linux_errno(LINUX_EINVAL),
        };
        return match memfd.seek(offset, whence) {
            Ok(pos) => pos,
            Err(_) => linux_errno(LINUX_EINVAL),
        };
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let mut request = new_vfs_request(VFS_IPC_OP_LSEEK);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg0 = offset as u64;
    request.arg1 = whence;
    match call_vfs_ipc_request(&request).and_then(|response| {
        ensure_vfs_status(&response)?;
        Ok(response.value)
    }) {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_vfs_fstat(fd: u64, stat_ptr: u64) -> u64 {
    if fd <= 2 {
        return write_bootstrap_stat(stat_ptr, fd + 1, 0);
    }
    if let Some(file) = current_vfs_file_handle(fd) {
        return write_bootstrap_stat(
            stat_ptr,
            crate::vfs_core::path_inode(file.path().as_bytes()),
            file.len() as u64,
        );
    }
    if let Some(memfd) = current_memfd_handle(fd) {
        return write_bootstrap_stat(
            stat_ptr,
            crate::vfs_core::path_inode(memfd.path().as_bytes()),
            memfd.len() as u64,
        );
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(stat_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_FSTAT);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    if response.payload_len as usize != LINUX_STAT_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(stat_ptr, &response.payload[..LINUX_STAT_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub fn syscall_linux_vfs_ftruncate(fd: u64, len: u64) -> u64 {
    if let Some(memfd) = current_memfd_handle(fd) {
        let Ok(len) = usize::try_from(len) else {
            return linux_errno(LINUX_EINVAL);
        };
        return match memfd.truncate(len) {
            Ok(()) => 0,
            Err(err) => linux_errno(memfd_error_to_linux_errno(err)),
        };
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let mut request = new_vfs_request(VFS_IPC_OP_FTRUNCATE);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg0 = len;
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_vfs_getdents64(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_GETDENTS64);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg1 = user_len as u64;
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let len = response.payload_len as usize;
    if len > user_len || len > response.payload.len() {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(user_ptr, &response.payload[..len]) {
        Ok(()) => len as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub fn syscall_linux_vfs_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    if fd <= 2 {
        return match cmd {
            linux_abi::F_GETFD => 0,
            linux_abi::F_SETFD => 0,
            linux_abi::F_GETFL => (linux_abi::O_RDWR as u64),
            linux_abi::F_SETFL => 0,
            _ => linux_errno(LINUX_EINVAL),
        };
    }
    if matches!(
        cmd,
        linux_abi::F_DUPFD
            | linux_abi::F_DUPFD_CLOEXEC
            | linux_abi::F_GETFD
            | linux_abi::F_SETFD
            | linux_abi::F_GETFL
            | linux_abi::F_SETFL
    ) {
        return match fcntl_current_handle(fd, cmd, arg) {
            Some(value) => value,
            None => linux_errno(LINUX_EBADF),
        };
    }
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

fn fcntl_current_handle(fd: u64, cmd: u64, arg: u64) -> Option<u64> {
    multitask::with_current_user_process_state_mut(|_, _, process_state| {
        if matches!(cmd, linux_abi::F_DUPFD | linux_abi::F_DUPFD_CLOEXEC) {
            let min_fd = i32::try_from(arg).ok()?;
            if min_fd < 0 {
                return Some(linux_errno(LINUX_EINVAL));
            }
            let close_on_exec = cmd == linux_abi::F_DUPFD_CLOEXEC;
            return process_state
                .handles_mut()
                .duplicate_min(fd, min_fd as u64, close_on_exec);
        }
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
            _ => linux_errno(LINUX_EINVAL),
        })
    })
    .flatten()
}

pub fn current_socket_handle(fd: u64) -> Option<(multitask::SocketHandle, bool)> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let entry = process_state.handles().get_entry(fd)?;
        match entry.handle() {
            multitask::KernelHandle::Socket(socket) => Some((
                socket.clone(),
                entry.status_flags() & linux_abi::O_NONBLOCK != 0,
            )),
            _ => None,
        }
    })
    .flatten()
}

pub fn current_socket_with_flags(fd: u64) -> Option<(multitask::SocketHandle, u64)> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let entry = process_state.handles().get_entry(fd)?;
        match entry.handle() {
            multitask::KernelHandle::Socket(socket) => Some((socket.clone(), entry.status_flags())),
            _ => None,
        }
    })
    .flatten()
}

pub fn socket_error_to_linux_errno(error: multitask::SocketError) -> i64 {
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

pub fn syscall_linux_vfs_statx(
    dirfd: u64,
    path_ptr: u64,
    flags: u64,
    mask: u64,
    statx_ptr: u64,
) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    if path.contains("startup-programs") || path.contains("runtime-env") {
        crate::debug::info!(
            compat,
            "bootstrap path probe: op=statx dirfd={:#x} flags={:#x} path={}",
            dirfd,
            flags,
            path
        );
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(statx_ptr, LINUX_STATX_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_STATX);
    request.dirfd = dirfd;
    request.flags = flags as u32;
    request.arg1 = mask;
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
    if response.payload_len as usize != LINUX_STATX_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(statx_ptr, &response.payload[..LINUX_STATX_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub fn syscall_linux_vfs_newfstatat(dirfd: u64, path_ptr: u64, stat_ptr: u64, flags: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    if path.contains("startup-programs") || path.contains("runtime-env") {
        crate::debug::info!(
            compat,
            "bootstrap path probe: op=newfstatat dirfd={:#x} flags={:#x} path={}",
            dirfd,
            flags,
            path
        );
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(stat_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_NEWFSTATAT);
    request.dirfd = dirfd;
    request.flags = flags as u32;
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
    if response.payload_len as usize != LINUX_STAT_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(stat_ptr, &response.payload[..LINUX_STAT_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub fn syscall_linux_vfs_readlinkat(
    dirfd: u64,
    path_ptr: u64,
    user_ptr: u64,
    user_len: u64,
) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
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
    let mut request = new_vfs_request(VFS_IPC_OP_READLINKAT);
    request.dirfd = dirfd;
    request.arg0 = user_len as u64;
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
    let len = (response.payload_len as usize).min(user_len);
    match usermem::write_current_user_bytes(user_ptr, &response.payload[..len]) {
        Ok(()) => len as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub fn syscall_linux_vfs_access(dirfd: u64, path_ptr: u64, mode: u64, flags: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_ACCESS);
    request.dirfd = dirfd;
    request.flags = flags as u32;
    request.arg0 = mode;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn current_vfs_file_handle(fd: u64) -> Option<multitask::VfsFileHandle> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::VfsFile(file)) => Some(file.clone()),
            _ => None,
        }
    })
    .flatten()
}

pub fn current_memfd_handle(fd: u64) -> Option<multitask::MemfdHandle> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::Memfd(memfd)) => Some(memfd.clone()),
            _ => None,
        }
    })
    .flatten()
}

pub fn current_epoll_handle(fd: u64) -> Option<multitask::EpollHandle> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::Epoll(epoll)) => Some(epoll.clone()),
            _ => None,
        }
    })
    .flatten()
}

pub fn read_linux_epoll_event(user_ptr: u64) -> Result<(u32, u64), paging::AddressSpaceError> {
    let mut bytes = [0_u8; size_of::<linux_abi::LinuxEpollEvent>()];
    usermem::copy_from_current_user_exact(user_ptr, &mut bytes)?;
    Ok((
        u32::from_le_bytes(bytes[0..4].try_into().unwrap_or([0; 4])),
        u64::from_le_bytes(bytes[4..12].try_into().unwrap_or([0; 8])),
    ))
}

pub fn write_linux_epoll_event(
    user_ptr: u64,
    events: u32,
    data: u64,
) -> Result<(), paging::AddressSpaceError> {
    let mut bytes = [0_u8; size_of::<linux_abi::LinuxEpollEvent>()];
    bytes[0..4].copy_from_slice(&events.to_le_bytes());
    bytes[4..12].copy_from_slice(&data.to_le_bytes());
    usermem::write_current_user_bytes(user_ptr, &bytes)
}

pub fn current_kernel_handle(fd: u64) -> Option<multitask::KernelHandle> {
    if fd == 0 {
        return Some(multitask::KernelHandle::Console(
            multitask::ConsoleStreamKind::Input,
        ));
    }
    if fd == 1 {
        return Some(multitask::KernelHandle::Console(
            multitask::ConsoleStreamKind::Output,
        ));
    }
    if fd == 2 {
        return Some(multitask::KernelHandle::Console(
            multitask::ConsoleStreamKind::Error,
        ));
    }
    multitask::with_current_user_process_state(|_, _, process_state| {
        process_state.handles().get(fd).cloned()
    })
    .flatten()
}

pub fn epoll_error_to_linux_errno(err: multitask::EpollError) -> i64 {
    match err {
        multitask::EpollError::Busy => LINUX_EEXIST,
        multitask::EpollError::InvalidArgument => LINUX_EINVAL,
        multitask::EpollError::NotFound => LINUX_ENOENT,
    }
}

pub fn memfd_error_to_linux_errno(err: multitask::MemfdError) -> i64 {
    match err {
        multitask::MemfdError::Busy => LINUX_EBUSY,
        multitask::MemfdError::InvalidArgument => LINUX_EINVAL,
        multitask::MemfdError::NoMemory => LINUX_ENOMEM,
        multitask::MemfdError::PermissionDenied => LINUX_EACCES,
    }
}

fn write_bootstrap_stat(user_ptr: u64, inode: u64, len: u64) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut out = [0_u8; LINUX_STAT_SIZE];
    out[8..16].copy_from_slice(&inode.max(1).to_ne_bytes());
    out[16..24].copy_from_slice(&1_u64.to_ne_bytes());
    out[24..28].copy_from_slice(&(0o100000_u32 | 0o555).to_ne_bytes());
    out[48..56].copy_from_slice(&(len as i64).to_ne_bytes());
    out[56..64].copy_from_slice(&4096_i64.to_ne_bytes());
    out[64..72].copy_from_slice(&(len.div_ceil(512) as i64).to_ne_bytes());
    match usermem::write_current_user_bytes(user_ptr, &out) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub fn syscall_linux_vfs_getcwd(user_ptr: u64, user_len: u64) -> u64 {
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let request = new_vfs_request(VFS_IPC_OP_GETCWD);
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let len = response.payload_len as usize;
    if len.checked_add(1).map_or(true, |needed| needed > user_len) {
        return linux_errno(LINUX_ERANGE);
    }
    if let Err(err) = usermem::write_current_user_bytes(user_ptr, &response.payload[..len]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = usermem::write_current_user_bytes(user_ptr + len as u64, &[0]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    (len + 1) as u64
}

pub fn syscall_linux_vfs_chdir(dirfd: u64, path_ptr: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_CHDIR);
    request.dirfd = dirfd;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_vfs_mkdir(dirfd: u64, path_ptr: u64, mode: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_MKDIR);
    request.dirfd = dirfd;
    request.arg0 = mode;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_vfs_unlinkat(dirfd: u64, path_ptr: u64, flags: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_UNLINKAT);
    request.dirfd = dirfd;
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_vfs_mount(
    _source_ptr: u64,
    target_ptr: u64,
    _fstype_ptr: u64,
    flags: u64,
) -> u64 {
    let path = match copy_current_user_path(target_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_MOUNT);
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_vfs_umount2(target_ptr: u64, flags: u64) -> u64 {
    let path = match copy_current_user_path(target_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_UMOUNT2);
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_ioctl(fd: u64, request_number: u64, arg: u64) -> u64 {
    if ioctl_requires_sessiond_tty_policy(request_number) {
        match ioctl_tty_via_sessiond(fd, request_number, arg) {
            Ok(value) => return value,
            Err(errno) => return linux_errno(errno),
        }
    }

    if ioctl_requires_devmgrd_policy(request_number) {
        match ioctl_device_via_devmgrd(fd, request_number, arg) {
            Ok(value) => return value,
            Err(errno) => return linux_errno(errno),
        }
    }

    // Hot data-path ioctls stay direct: display present/input delivery costs
    // must stay on the narrow broker/data path instead of taking per-frame
    // policy IPC.
    match crate::user::sysops::device::ioctl_current_process_fd(fd, request_number, arg) {
        Ok(value) => value,
        Err(err) => linux_errno(super::super::broker_ops::device_sysop_error_to_linux_errno(
            err,
        )),
    }
}

fn ioctl_requires_devmgrd_policy(request_number: u64) -> bool {
    matches!(
        request_number,
        // Display setup policy — devmgrd delegates UI policy to uiserver.
        rustos_user_abi::device::DISPLAY_IOCTL_GET_INFO
            | rustos_user_abi::device::DISPLAY_IOCTL_CREATE_SURFACE
            // Console/session observation and input injection policy.
            | rustos_user_abi::console::CONSOLE_IOCTL_GET_STATE
            | rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT
            | rustos_user_abi::console::CONSOLE_IOCTL_SEND_INPUT_EVENT
            | rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSIONS
            // Session lifecycle — devmgrd owns session-table bookkeeping.
            | rustos_user_abi::console::CONSOLE_IOCTL_CREATE_SESSION
            | rustos_user_abi::console::CONSOLE_IOCTL_CLOSE_SESSION
            | rustos_user_abi::console::CONSOLE_IOCTL_BIND_CURRENT_SESSION
            | rustos_user_abi::console::CONSOLE_IOCTL_SET_SESSION_STATE
            | rustos_user_abi::console::CONSOLE_IOCTL_SET_FOCUS
    )
}

fn ioctl_requires_sessiond_tty_policy(request_number: u64) -> bool {
    matches!(
        request_number,
        linux_abi::TCGETS
            | linux_abi::TCSETS
            | linux_abi::TCSETSW
            | linux_abi::TCSETSF
            | linux_abi::FIONREAD
    )
}

pub fn syscall_linux_net4(op: u16, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    syscall_linux_net6(op, arg0, arg1, arg2, arg3, 0, 0)
}

pub fn syscall_linux_net6(
    op: u16,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> u64 {
    let mut request = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op,
        arg0,
        arg1,
        arg2,
        arg3,
        arg4,
        arg5,
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
    match call_netd_ipc_request(&request).map(|response| response.value) {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

*/
// RING3-MIGRATION-COMMENTED-OUT END
