use super::*;
pub fn syscall_linux_vfs_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
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
    match call_pinned_remote_vfs_request(&request).and_then(|response| {
        ensure_vfs_status(&response)?;
        Ok(response.value)
    }) {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_vfs_fstat(fd: u64, stat_ptr: u64) -> u64 {
    if let Some(multitask::KernelHandle::Console(console)) = current_kernel_handle(fd) {
        return write_bootstrap_stat(stat_ptr, console.token_id(), 0);
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
    let response = match call_pinned_remote_vfs_request(&request) {
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
    match call_pinned_remote_vfs_request(&request).and_then(|response| ensure_vfs_status(&response))
    {
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
    let remote_id = remote.remote_id();
    if let Err(errno) = acquire_current_remote_vfs_ref(fd, remote_id) {
        return linux_errno(errno);
    }
    let result = (|| {
        let mut request = new_vfs_request(VFS_IPC_OP_GETDENTS64);
        request.fd = fd;
        request.remote_id = remote_id;
        request.arg1 = user_len as u64;
        let response = match call_vfs_ipc_request(&request) {
            Ok(response) => response,
            Err(errno) => {
                let _ = settle_vfs_cursor_mutation(&request, false);
                return linux_errno(errno);
            }
        };
        if let Err(errno) = ensure_vfs_status(&response) {
            return linux_errno(errno);
        }
        let len = response.payload_len as usize;
        if len > user_len || len > response.payload.len() {
            let _ = settle_vfs_cursor_mutation(&request, false);
            return linux_errno(LINUX_EINVAL);
        }
        match usermem::write_current_user_bytes(user_ptr, &response.payload[..len]) {
            Ok(()) => match settle_vfs_cursor_mutation(&request, true) {
                Ok(()) => len as u64,
                Err(errno) => {
                    // Bytes are already visible; reconciliation retains the
                    // prepared cursor until the exact COMMIT settles.
                    if len > 0 {
                        len as u64
                    } else {
                        linux_errno(errno)
                    }
                }
            },
            Err(err) => {
                let _ = settle_vfs_cursor_mutation(&request, false);
                linux_errno(address_space_error_to_linux_errno(err))
            }
        }
    })();
    release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(remote_id)]);
    result
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
    request.flags = flags as u32;
    request.arg1 = mask;
    if let Err(errno) = populate_vfs_path_base(&mut request, dirfd, &path) {
        return linux_errno(errno);
    }
    let response = match call_pinned_remote_vfs_request(&request) {
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
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path_base(&mut request, dirfd, &path) {
        return linux_errno(errno);
    }
    let response = match call_pinned_remote_vfs_request(&request) {
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
    request.arg0 = user_len as u64;
    if let Err(errno) = populate_vfs_path_base(&mut request, dirfd, &path) {
        return linux_errno(errno);
    }
    let response = match call_pinned_remote_vfs_request(&request) {
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
    request.flags = flags as u32;
    request.arg0 = mode;
    if let Err(errno) = populate_vfs_path_base(&mut request, dirfd, &path) {
        return linux_errno(errno);
    }
    match call_pinned_remote_vfs_request(&request).and_then(|response| ensure_vfs_status(&response))
    {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
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
    let mut event_bytes = [0_u8; 4];
    event_bytes.copy_from_slice(&bytes[0..4]);
    let mut data_bytes = [0_u8; 8];
    data_bytes.copy_from_slice(&bytes[4..12]);
    Ok((
        u32::from_le_bytes(event_bytes),
        u64::from_le_bytes(data_bytes),
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
    multitask::with_current_user_process_state(|_, _, process_state| {
        process_state.handles().get(fd).cloned()
    })
    .flatten()
}

pub fn memfd_error_to_linux_errno(err: multitask::MemfdError) -> i64 {
    match err {
        multitask::MemfdError::Busy => LINUX_EBUSY,
        multitask::MemfdError::InvalidArgument => LINUX_EINVAL,
        multitask::MemfdError::NoMemory => LINUX_ENOMEM,
        multitask::MemfdError::PermissionDenied => LINUX_EACCES,
    }
}

// RING3-MIGRATION-REFERENCE START: vfsd owns remote-file stat materialization.
// Ring0 materializes stat only for kernel-owned console and memfd handle kinds;
// this is their primary substrate, not a weaker VFS fallback.
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
// RING3-MIGRATION-REFERENCE END: kernel-owned handle stat substrate.

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
    let response = match call_pinned_remote_vfs_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let len = response.payload_len as usize;
    if len.checked_add(1).is_none_or(|needed| needed > user_len) {
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
    if let Err(errno) = populate_vfs_path_base(&mut request, dirfd, &path) {
        return linux_errno(errno);
    }
    match call_pinned_remote_vfs_request(&request).and_then(|response| ensure_vfs_status(&response))
    {
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
    request.arg0 = mode;
    if let Err(errno) = populate_vfs_path_base(&mut request, dirfd, &path) {
        return linux_errno(errno);
    }
    match call_pinned_remote_vfs_request(&request).and_then(|response| ensure_vfs_status(&response))
    {
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
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path_base(&mut request, dirfd, &path) {
        return linux_errno(errno);
    }
    match call_pinned_remote_vfs_request(&request).and_then(|response| ensure_vfs_status(&response))
    {
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
    match call_pinned_remote_vfs_request(&request).and_then(|response| ensure_vfs_status(&response))
    {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

/// Copy the caller's wait request in, hold it in the console broker until the
/// graph moves past the generation it presented, and copy the answer back.
fn console_wait_graph(arg: u64) -> Result<u64, i64> {
    let mut request =
        usermem::read_current_user_struct::<rustos_user_abi::console::ConsoleWaitGraphRequest>(arg)
            .map_err(super::super::address_space_error_to_linux_errno)?;
    if request.reserved != 0 {
        return Err(LINUX_EINVAL);
    }
    request.generation = super::ipc_helpers::console_graph_readiness_via_sessiond(
        request.generation,
        u64::from(request.wait_ms),
    )?;
    usermem::write_current_user_struct(arg, &request)
        .map_err(super::super::address_space_error_to_linux_errno)?;
    Ok(0)
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
    match call_pinned_remote_vfs_request(&request).and_then(|response| ensure_vfs_status(&response))
    {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_ioctl(fd: u64, request_number: u64, arg: u64) -> u64 {
    // A console-graph wait is answered by the console broker holding the reply
    // until the graph moves. Asking devmgrd to authorize and forward it would
    // park devmgrd's single serving loop for the whole wait, stalling every
    // unrelated device ioctl behind a call whose entire purpose is to block.
    // Console read, write, and per-session readiness already take this direct
    // rail for the same reason. The console handle is the authority: a caller
    // without one is refused here, before the broker is reached.
    if request_number == rustos_user_abi::console::CONSOLE_IOCTL_WAIT_GRAPH {
        return match console_graph_wait_authority(fd).and_then(|()| console_wait_graph(arg)) {
            Ok(value) => value,
            Err(errno) => linux_errno(errno),
        };
    }
    let route = if ioctl_is_direct_display_present(request_number)
        || ioctl_is_display_policy_request(request_number)
            && ipc_ops::current_process_has_service_capability(
                rustos_user_abi::syscall::IPC_SERVICE_CAP_UI_POLICY,
            ) {
        rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_DIRECT
    } else {
        match ioctl_route_via_devmgrd(fd, request_number) {
            Ok(route) => route,
            Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_DEVMGRD) => {
                rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_DIRECT
            }
            Err(errno) => return linux_errno(errno),
        }
    };
    match route {
        rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_SESSIOND_TTY => {
            if !current_kernel_handle(fd).is_some_and(|handle| is_console_handle(&handle)) {
                return linux_errno(LINUX_ENOTTY);
            }
            match ioctl_tty_via_sessiond(fd, request_number, arg) {
                Ok(value) => value,
                Err(errno) => linux_errno(errno),
            }
        }
        rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_DEVMGRD => {
            match ioctl_device_via_devmgrd(fd, request_number, arg) {
                Ok(value) => value,
                Err(errno) => linux_errno(errno),
            }
        }
        rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT => {
            match ioctl_device_via_devmgrd(fd, request_number, arg) {
                Ok(value) => value,
                Err(errno) => linux_errno(errno),
            }
        }
        rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_DIRECT => {
            match crate::user::sysops::device::ioctl_current_process_fd(fd, request_number, arg) {
                Ok(value) => value,
                Err(err) => linux_errno(
                    super::super::broker_ops::device_sysop_error_to_linux_errno(err),
                ),
            }
        }
        _ => linux_errno(LINUX_EINVAL),
    }
}

fn is_console_handle(handle: &multitask::KernelHandle) -> bool {
    matches!(handle, multitask::KernelHandle::Console(_))
}

/// True for a handle opened on the console *device*, which is what a process
/// that serves the console gets from `open(CONSOLE_PATH)`.
///
/// This is deliberately not [`is_console_handle`]. That one is true only for a
/// process's own stdin/stdout/stderr, the handles a shell holds for its single
/// session; the compositor never has one, because it reaches the console as a
/// device. Guarding a compositor-only call with the shell's handle kind is how
/// the graph wait came to fail `ENOTTY` on every call it ever received.
fn is_console_device_handle(handle: &multitask::KernelHandle) -> bool {
    matches!(handle, multitask::KernelHandle::Device(device)
        if device.device_id() == crate::io::device::DeviceId::Console)
}

/// Identifies the handle kind behind an fd, for refusal diagnostics only.
///
/// The milestone log carries integers, not strings, so a refusal reports which
/// kind it actually found rather than only that it found the wrong one. Naming
/// the kind is the whole diagnostic: the guard that failed here was rejecting
/// a handle nobody had yet established the kind of.
fn kernel_handle_kind_code(handle: &multitask::KernelHandle) -> u64 {
    match handle {
        multitask::KernelHandle::Console(_) => 1,
        multitask::KernelHandle::Device(_) => 2,
        multitask::KernelHandle::Epoll(_) => 3,
        multitask::KernelHandle::InetSocket(_) => 4,
        multitask::KernelHandle::Memfd(_) => 5,
        multitask::KernelHandle::RemoteVfs(_) => 6,
        multitask::KernelHandle::Socket(_) => 7,
        multitask::KernelHandle::VfsDirectory(_) => 8,
        multitask::KernelHandle::DisplaySurface(_) => 9,
    }
}

const CONSOLE_GRAPH_REFUSAL_NO_HANDLE: u64 = 1;
const CONSOLE_GRAPH_REFUSAL_WRONG_HANDLE_KIND: u64 = 2;
const CONSOLE_GRAPH_REFUSAL_NO_CAPABILITY: u64 = 3;

/// May this caller wait on the console graph?
///
/// The refusals are deliberately distinguishable, and a refused wait is
/// reported rather than merely returned. The single caller of this rail turns
/// a failed wait into a polling fallback, so a refusal that says nothing here
/// is indistinguishable from an idle console at every layer above - which is
/// exactly how a rail that had never once answered went unnoticed while the
/// compositor polled the broker roughly two hundred times a second.
fn console_graph_wait_authority(fd: u64) -> Result<(), i64> {
    let Some(handle) = current_kernel_handle(fd) else {
        report_console_graph_wait_refusal(CONSOLE_GRAPH_REFUSAL_NO_HANDLE, 0);
        return Err(LINUX_EBADF);
    };
    if !is_console_device_handle(&handle) {
        report_console_graph_wait_refusal(
            CONSOLE_GRAPH_REFUSAL_WRONG_HANDLE_KIND,
            kernel_handle_kind_code(&handle),
        );
        return Err(LINUX_ENOTTY);
    }
    // The graph is every session at once, which is the display policy owner's
    // subject and no single session's. A shell holds a console handle for its
    // own session and is served by the per-session readiness subject instead.
    if !ipc_ops::current_process_has_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_UI_POLICY,
    ) {
        report_console_graph_wait_refusal(CONSOLE_GRAPH_REFUSAL_NO_CAPABILITY, 0);
        return Err(LINUX_EPERM);
    }
    Ok(())
}

/// Reports the first few graph-wait refusals, then falls silent. A refusal
/// repeats at the caller's polling rate, so this must never be unbounded.
///
/// `arg0` carries the refusing process, `arg1` packs the reason in its high
/// half and, for a wrong handle kind, the kind that was actually found in its
/// low half.
fn report_console_graph_wait_refusal(reason: u64, handle_kind: u64) {
    use core::sync::atomic::{AtomicU64, Ordering};
    const REPORTED_REFUSAL_BUDGET: u64 = 4;
    // ORDERING: Relaxed is exact; this counter owns diagnostics only.
    static REPORTED_REFUSALS: AtomicU64 = AtomicU64::new(0);
    if REPORTED_REFUSALS.fetch_add(1, Ordering::Relaxed) >= REPORTED_REFUSAL_BUDGET {
        return;
    }
    let process_id = multitask::current_user_snapshot()
        .map(|snapshot| snapshot.process_id())
        .unwrap_or(0);
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "console-graph-wait-refused",
        process_id,
        (reason << 32) | (handle_kind & 0xffff_ffff),
    );
}

fn ioctl_is_direct_display_present(request_number: u64) -> bool {
    matches!(
        request_number,
        rustos_user_abi::device::DISPLAY_IOCTL_PRESENT
            | rustos_user_abi::device::DISPLAY_IOCTL_PRESENT_RECT
            | rustos_user_abi::device::DISPLAY_IOCTL_GPU_SUBMIT
            | rustos_user_abi::device::DISPLAY_IOCTL_GPU_QUERY_COMPLETION
    )
}

fn ioctl_is_display_policy_request(request_number: u64) -> bool {
    matches!(
        request_number,
        rustos_user_abi::device::DISPLAY_IOCTL_GET_INFO
            | rustos_user_abi::device::DISPLAY_IOCTL_CREATE_SURFACE
            | rustos_user_abi::device::DISPLAY_IOCTL_PRESENT
            | rustos_user_abi::device::DISPLAY_IOCTL_PRESENT_RECT
            | rustos_user_abi::device::DISPLAY_IOCTL_GPU_GET_INFO
            | rustos_user_abi::device::DISPLAY_IOCTL_GPU_SUBMIT
            | rustos_user_abi::device::DISPLAY_IOCTL_GPU_QUERY_COMPLETION
    )
}

const NETD_MAX_IOVEC_COUNT: usize = 16;

/// Whether `O_NONBLOCK` on the descriptor forbids this operation from waiting.
///
/// `poll` and `connect` are deliberately absent: waiting is the entire point of
/// one, and the other answers a non-blocking caller with `EINPROGRESS` rather
/// than by returning early.
fn socket_op_honours_nonblock(op: u16) -> bool {
    matches!(
        op,
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO
            | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
            | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
            | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
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
    syscall_linux_net6_with_timeout(op, arg0, arg1, arg2, arg3, arg4, arg5, None)
}

/// The same offload with an explicit completion bound.
///
/// Without one every netd call takes the bulk-data rail, 30 seconds, including
/// a receive on a socket the caller opened non-blocking. uiserver's Wayland
/// dispatch reads its client sockets that way, and the instrumented run named
/// the exact call: "phase=wayland step=dispatch elapsed_ms=3165", the
/// compositor watchdog killing the process inside libwayland's per-client
/// dispatch. `V5-WAYLAND-HOL-013` is that block.
///
/// A caller that declared O_NONBLOCK has already accepted `EAGAIN` as an
/// answer, so bounding its receive costs it nothing it did not agree to - but
/// only if the bound reports `EAGAIN`. The first attempt at this let
/// `ETIMEDOUT` reach the caller and the run came back with a 30-second callback
/// gap and two windows, because the whole Wayland dispatch model is built on
/// `EAGAIN` being the ordinary, non-fatal answer on a ready-but-empty socket
/// and treats anything else as a broken client.
///
/// Callers do not have to ask for the bound. Bounding it at the two `read`/
/// `write` call sites left `sendmsg` and `recvmsg` on the bulk rail, and those
/// are the calls libwayland actually makes - it needs the control message for
/// fd passing, so every compositor flush and every client read is a `sendmsg`
/// or a `recvmsg`, never a `sendto` or a `recvfrom`. The run said so directly:
/// "phase=wayland step=callback-flush elapsed_ms=3075", the watchdog killing
/// uiserver inside `wl_display_flush_clients` at t=33.6s, and WayClick took a
/// broken pipe one line later. So the rail is chosen here, once, from the
/// descriptor's own `O_NONBLOCK` - the single place that already knows both
/// the operation and the flags the caller opened the socket with.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors the six-argument Linux socket offload plus its completion bound"
)]
pub fn syscall_linux_net6_with_timeout(
    op: u16,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    timeout_ms: Option<u64>,
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
    let mut pending_transfers = PendingNetdTransfers::default();
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
    populate_netd_socket_token(&mut request);
    let nonblocking_bound = timeout_ms.is_none()
        && socket_op_honours_nonblock(op)
        && request.status_flags & linux_abi::O_NONBLOCK != 0;
    let timeout_ms = if nonblocking_bound {
        Some(rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS)
    } else {
        timeout_ms
    };
    // The wire deadline is fixed before payload preparation and is shared by
    // the service-side admission and our reply-cap wait. In particular, the
    // nonblocking rail must not enqueue with a fresh 16 ms wait after copying
    // a large control message.
    let deadline_class = timeout_ms.map_or(ipc_ops::ServiceIpcClass::BulkData, |_| {
        super::ipc_helpers::deadline::netd_timeout_class(op)
    });
    request.deadline_ns = netd_deadline_after_class(deadline_class, timeout_ms);
    if let Err(errno) = populate_netd_request_payload(&mut request, &mut pending_transfers) {
        pending_transfers.drop_pending();
        return linux_errno(errno);
    }
    if !pending_transfers.is_receive()
        && let Err(errno) = pending_transfers.publish_send()
    {
        pending_transfers.drop_pending();
        return linux_errno(errno);
    }
    let result = match timeout_ms {
        Some(timeout_ms) => call_netd_ipc_request_with_timeout(&request, timeout_ms),
        None => call_netd_ipc_request(&request),
    };
    let outcome = match result {
        Ok(response) => {
            let consumed = usize::try_from(response.value).ok();
            let result = consume_netd_response_payload(
                &request,
                &response,
                pending_transfers.receive_context(),
            );
            if pending_transfers.is_receive() {
                if !consumed.is_some_and(|len| pending_transfers.commit_receive(len)) {
                    // netd already dequeued these bytes from the peer's stream;
                    // no other copy of them exists anywhere in the system.
                    record_stream_break(
                        STREAM_BREAK_RECEIVE_UNCOMMITTED,
                        &request,
                        consumed.unwrap_or(0),
                    );
                    pending_transfers.drop_pending();
                    return linux_errno(LINUX_EIO);
                }
                if let Err(errno) = result.as_ref() {
                    // The bytes left netd's queue and then failed to reach the
                    // caller's buffer. The caller sees an error and reads again
                    // from the byte after the hole.
                    record_stream_break(
                        STREAM_BREAK_RECEIVE_COPYOUT,
                        &request,
                        errno.unsigned_abs() as usize,
                    );
                }
            } else if result.is_ok() {
                let Some(accepted) = consumed else {
                    record_stream_break(STREAM_BREAK_SEND_UNREPORTED, &request, 0);
                    pending_transfers.drop_pending();
                    return linux_errno(LINUX_EIO);
                };
                if pending_transfers.commit_send(accepted).is_err() {
                    // netd accepted `accepted` bytes onto the peer's queue, but
                    // this caller is about to be told the call failed, so a
                    // stream writer will resend the exact same bytes.
                    record_stream_break(STREAM_BREAK_SEND_UNCOMMITTED, &request, accepted);
                    pending_transfers.drop_pending();
                    return linux_errno(LINUX_ESTALE);
                }
            }
            match result {
                Ok(()) => response.value,
                Err(errno) => {
                    pending_transfers.drop_pending();
                    linux_errno(errno)
                }
            }
        }
        // A netd status reply is NOT recorded here. `EAGAIN` on an empty
        // non-blocking receive is the ordinary answer and arrives thousands of
        // times a second; counting it as a stream break buries the real events
        // under normal traffic. Only a reply that never arrived is ambiguous,
        // and only the transport can tell the two apart, so that record is
        // taken at the transport boundary in `call_netd_ipc_request_impl`.
        Err(errno) => {
            pending_transfers.drop_pending();
            linux_errno(errno)
        }
    };
    if nonblocking_bound && outcome == linux_errno(LINUX_ETIMEDOUT) {
        return linux_errno(LINUX_EAGAIN);
    }
    outcome
}

/// Operations whose completion moves bytes across a stream boundary.
///
/// These are exactly the operations for which "did it happen?" has an
/// observable answer that no later call can recover: a send that was accepted
/// cannot be un-accepted, and a receive that dequeued bytes cannot put them
/// back. `connect`, `bind`, `poll` and the option calls are all safely
/// repeatable and are deliberately absent.
pub(super) const fn socket_op_moves_stream_bytes(op: u16) -> bool {
    matches!(
        op,
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO
            | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
            | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
            | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
    )
}

const STREAM_BREAK_RECEIVE_UNCOMMITTED: u64 = 1;
const STREAM_BREAK_RECEIVE_COPYOUT: u64 = 2;
const STREAM_BREAK_SEND_UNCOMMITTED: u64 = 3;
const STREAM_BREAK_SEND_UNREPORTED: u64 = 4;
pub(super) const STREAM_BREAK_ABANDONED_IN_FLIGHT: u64 = 5;

/// Names a point where this syscall is about to report an outcome that does not
/// match what netd already did to the stream.
///
/// # Why this exists as its own milestone
///
/// Every site that calls this has the same shape: netd has *already* committed
/// the byte movement, and the compat layer then fails on the way back. A stream
/// caller cannot tell that apart from "nothing happened", so it does the only
/// thing a stream caller can do - it retries. A retried send duplicates bytes on
/// the wire; a failed receive drops them. Both land on the peer as a byte stream
/// that no longer parses, and the peer reports it as a protocol error against
/// whatever message straddles the damage. That report names the victim, never
/// the cause, which is why the cause has to name itself here.
///
/// This is a diagnostic, not a repair. The repair is a replay slot keyed by the
/// stream position that `SocketStreamGuard` already maintains, matching the
/// operation-ID replay slot netd keeps for reference mutations. Until that
/// exists, this milestone is the only evidence that distinguishes a corrupted
/// stream from a misbehaving peer.
///
/// # Why the sampling bound is mandatory, not a nicety
///
/// `record_milestone` writes a synchronous debugcon line, and a debugcon line
/// was measured at roughly 335 us fixed plus 11.1 us per byte. Stream breaks
/// arrive in bursts by their nature - a compositor flush loop that retries a
/// rejected write retries it immediately - so an unbounded record here would
/// spend milliseconds of CPU per event inside a socket syscall and convert a
/// data-integrity fault into a hang. `contracts-abi.md` states the same rule
/// for `ipc-reply-timeout`: a high-frequency milestone stays in the bounded
/// in-kernel ring and must not emit a line per occurrence.
///
/// Each class keeps its own counter so a common class cannot mask a rare one,
/// and `arg1` carries the running total for that class, so a suppressed line is
/// still countable from the lines that do appear.
pub(super) fn record_stream_break(class: u64, request: &NetdIpcRequest, detail: usize) {
    use core::sync::atomic::{AtomicU64, Ordering};
    // Indexed by class; slot 0 is unused so a class value maps directly.
    static STREAM_BREAKS: [AtomicU64; 6] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    const EARLY_STREAM_BREAK_SAMPLES: u64 = 8;

    let Some(counter) = STREAM_BREAKS.get(class as usize) else {
        return;
    };
    // ORDERING: Relaxed is exact; this counter owns diagnostics only and
    // orders nothing.
    let total = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if total > EARLY_STREAM_BREAK_SAMPLES && !total.is_power_of_two() {
        return;
    }
    // arg0=(class, offload op, running total for this class),
    // arg1=(socket token, class-specific detail: accepted bytes or errno).
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "socket-stream-break",
        (class << 48) | (u64::from(request.op) << 32) | (total & 0xffff_ffff),
        ((request.socket_token & 0xffff_ffff) << 32) | (detail as u64 & 0xffff_ffff),
    );
}

pub(super) const NETD_NANOS_PER_MILLI: u64 = 1_000_000;
const NETD_NANOS_PER_SECOND: u64 = 1_000_000_000;

/// The kernel's `CLOCK_MONOTONIC` conversion is the authority for compat
/// deadlines. It deliberately derives both request wire time and retry
/// accounting from the same RTC tick source that arms the reply waiter.
pub(super) fn netd_monotonic_nanos() -> u64 {
    let now = super::process_time::monotonic_timespec();
    u64::try_from(now.tv_sec)
        .unwrap_or(0)
        .saturating_mul(NETD_NANOS_PER_SECOND)
        .saturating_add(u64::try_from(now.tv_nsec).unwrap_or(0))
}

pub(super) fn netd_deadline_after_class(
    class: ipc_ops::ServiceIpcClass,
    requested_timeout_ms: Option<u64>,
) -> u64 {
    netd_deadline_after_ms(
        netd_monotonic_nanos(),
        requested_timeout_ms.map_or(class.timeout_ms(), |timeout_ms| {
            class.cap_timeout_ms(timeout_ms)
        }),
    )
}

pub(super) const fn netd_deadline_after_ms(now_ns: u64, timeout_ms: u64) -> u64 {
    now_ns.saturating_add(timeout_ms.saturating_mul(NETD_NANOS_PER_MILLI))
}

pub(super) fn netd_deadline_remaining_ms(deadline_ns: u64) -> Option<u64> {
    netd_deadline_remaining_ms_at(deadline_ns, netd_monotonic_nanos())
}

pub(super) const fn netd_deadline_remaining_ms_at(deadline_ns: u64, now_ns: u64) -> Option<u64> {
    if deadline_ns == 0 || now_ns >= deadline_ns {
        return None;
    }
    Some((deadline_ns - now_ns).div_ceil(NETD_NANOS_PER_MILLI))
}

struct PendingNetdTransfers {
    descriptors: Vec<kernel_ipc_runtime::api::KernelTransferredHandle>,
    tickets: Vec<kernel_ipc_runtime::api::KernelTransferTicket>,
    stream: Option<multitask::SocketStreamGuard>,
    channel_id: u64,
    channel_side: u8,
    socket_token: u64,
    receive: bool,
}

impl Default for PendingNetdTransfers {
    fn default() -> Self {
        Self {
            descriptors: Vec::new(),
            tickets: Vec::new(),
            stream: None,
            channel_id: 0,
            channel_side: 0,
            socket_token: 0,
            receive: false,
        }
    }
}

impl PendingNetdTransfers {
    fn extend(&mut self, descriptors: &[kernel_ipc_runtime::api::KernelTransferredHandle]) {
        self.descriptors.extend_from_slice(descriptors);
    }

    fn extend_tickets(&mut self, tickets: &[kernel_ipc_runtime::api::KernelTransferTicket]) {
        self.tickets.extend_from_slice(tickets);
    }

    fn begin_send(&mut self, socket: &multitask::SocketHandle, len: usize) -> Result<(), i64> {
        let stream = socket.begin_stream_send(len).ok_or(LINUX_EAGAIN)?;
        self.channel_id = socket.channel_id();
        self.channel_side = socket.channel_side();
        self.socket_token = socket.token_id();
        self.stream = Some(stream);
        Ok(())
    }

    fn begin_receive(&mut self, socket: &multitask::SocketHandle) -> Result<(), i64> {
        let stream = socket.begin_stream_receive().ok_or(LINUX_EAGAIN)?;
        self.channel_id = socket.channel_id();
        self.channel_side = socket.channel_side();
        self.socket_token = socket.token_id();
        self.receive = true;
        self.stream = Some(stream);
        Ok(())
    }

    fn commit_send(&mut self, accepted: usize) -> Result<(), i64> {
        if let Some(stream) = self.stream.take() {
            let reserved = stream
                .end()
                .checked_sub(stream.start())
                .and_then(|len| usize::try_from(len).ok())
                .ok_or(LINUX_ESTALE)?;
            if (!self.tickets.is_empty() && accepted != reserved) || !stream.commit_send(accepted) {
                return Err(LINUX_ESTALE);
            }
        }
        self.descriptors.clear();
        self.tickets.clear();
        Ok(())
    }

    fn publish_send(&mut self) -> Result<(), i64> {
        if self.tickets.is_empty() {
            return Ok(());
        }
        super::super::ipc_ops::commit_transfer_tickets_enqueue(self.tickets.as_slice())
    }

    fn commit_receive(&mut self, len: usize) -> bool {
        self.stream
            .take()
            .is_some_and(|stream| stream.commit_receive(len))
    }

    fn is_receive(&self) -> bool {
        self.receive
    }

    fn receive_context(&self) -> Option<(u64, u64, u8, u64)> {
        if !self.receive {
            return None;
        }
        Some((
            self.stream.as_ref()?.start(),
            self.channel_id,
            self.channel_side,
            self.socket_token,
        ))
    }

    fn drop_pending(&mut self) {
        if !self.descriptors.is_empty() {
            super::super::ipc_ops::drop_transfer_descriptors(self.descriptors.as_slice());
        }
        self.descriptors.clear();
        self.tickets.clear();
        self.stream.take();
    }
}

fn populate_netd_socket_token(request: &mut NetdIpcRequest) {
    let fd = match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_BIND
        | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
        | SYSCALL_OFFLOAD_OP_LINUX_DUP
        | SYSCALL_OFFLOAD_OP_LINUX_LISTEN
        | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT
        | SYSCALL_OFFLOAD_OP_LINUX_CONNECT
        | SYSCALL_OFFLOAD_OP_LINUX_SENDTO
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME
        | SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME
        | SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN
        | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
        | SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET => request.arg0,
        _ => return,
    };
    let Some((token, status_flags)) =
        multitask::with_current_user_process_state(|_, _, process_state| {
            let entry = process_state.handles().get_entry(fd)?;
            let token = match entry.handle() {
                multitask::KernelHandle::Socket(socket) => socket.token_id(),
                multitask::KernelHandle::InetSocket(socket) => socket.token_id(),
                _ => return None,
            };
            Some((token, entry.status_flags()))
        })
        .flatten()
    else {
        return;
    };
    request.socket_token = token;
    request.status_flags = status_flags;
}

fn populate_netd_request_payload(
    request: &mut NetdIpcRequest,
    pending_transfers: &mut PendingNetdTransfers,
) -> Result<(), i64> {
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_BIND | SYSCALL_OFFLOAD_OP_LINUX_CONNECT => {
            copy_current_payload(request, request.arg1, request.arg2)
        }
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO => {
            copy_current_payload(request, request.arg1, request.arg2)?;
            let socket = current_unix_socket_handle(request.arg0).ok_or(LINUX_ENOTSOCK)?;
            pending_transfers.begin_send(&socket, request.payload_len as usize)
        }
        SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT => {
            copy_current_payload(request, request.arg3, request.arg4)
        }
        SYSCALL_OFFLOAD_OP_LINUX_SENDMSG => {
            let header = usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(request.arg1)
                .map_err(address_space_error_to_linux_errno)?;
            let mut bytes = read_current_iovec_payload(header.msg_iov, header.msg_iovlen)?;
            let socket = current_unix_socket_handle(request.arg0).ok_or(LINUX_ENOTSOCK)?;
            pending_transfers.begin_send(&socket, bytes.len())?;
            let raw_control =
                read_current_control_bytes(header.msg_control, header.msg_controllen)?;
            let context = send_transfer_context(&socket, pending_transfers, bytes.len())?;
            let control = encode_transfer_control_payload(raw_control.as_slice(), context)?;
            pending_transfers.extend(control.descriptors.as_slice());
            pending_transfers.extend_tickets(control.tickets.as_slice());
            // One offload payload is a bounded buffer, but that bound belongs to
            // this transport and not to the caller's ABI. Linux `sendmsg` on a
            // stream socket never answers `EINVAL` because a buffer inside the
            // kernel was smaller than the request; it takes a prefix and returns
            // how much it took, which is what every stream writer is built to
            // handle. Returning `EINVAL` made a legal call look like a
            // programming error to a caller that had done nothing wrong.
            //
            // The reservation above only fixes an upper bound, so committing
            // fewer bytes than were reserved is already the supported partial
            // path. Descriptors are the exception: they attach to the message,
            // so a prefix would deliver them against the wrong bytes. That case
            // is a message this transport cannot carry atomically, which is what
            // `EMSGSIZE` means.
            let capacity = sendmsg_data_capacity(control.bytes.len())?;
            if bytes.len() > capacity {
                if !control.bytes.is_empty() {
                    return Err(LINUX_EMSGSIZE);
                }
                bytes.truncate(capacity);
            }
            let payload_len = NETD_SENDMSG_PAYLOAD_HEADER_SIZE
                .checked_add(bytes.len())
                .and_then(|len| len.checked_add(control.bytes.len()))
                .ok_or(LINUX_EINVAL)?;
            request.payload[0..4].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
            request.payload[4..8].copy_from_slice(&(control.bytes.len() as u32).to_ne_bytes());
            request.payload[8..12].copy_from_slice(&0_u32.to_ne_bytes());
            request.payload[12..16].copy_from_slice(&0_u32.to_ne_bytes());
            let data_start = NETD_SENDMSG_PAYLOAD_HEADER_SIZE;
            let control_start = data_start + bytes.len();
            request.payload[data_start..control_start].copy_from_slice(bytes.as_slice());
            request.payload[control_start..payload_len].copy_from_slice(control.bytes.as_slice());
            request.payload_len = payload_len as u32;
            request.arg4 = header.msg_controllen;
            Ok(())
        }
        SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => {
            let header = usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(request.arg1)
                .map_err(address_space_error_to_linux_errno)?;
            let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
            let total = iovec_payload_total_len(&iovecs)?;
            request.payload_len = total as u32;
            request.arg4 = header.msg_controllen;
            let socket = current_unix_socket_handle(request.arg0).ok_or(LINUX_ENOTSOCK)?;
            pending_transfers.begin_receive(&socket)?;
            Ok(())
        }
        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => {
            let socket = current_unix_socket_handle(request.arg0).ok_or(LINUX_ENOTSOCK)?;
            pending_transfers.begin_receive(&socket)
        }
        _ => Ok(()),
    }
}

fn current_unix_socket_handle(fd: u64) -> Option<multitask::SocketHandle> {
    match current_kernel_handle(fd)? {
        multitask::KernelHandle::Socket(socket) => Some(socket),
        _ => None,
    }
}

fn send_transfer_context(
    socket: &multitask::SocketHandle,
    pending: &PendingNetdTransfers,
    len: usize,
) -> Result<kernel_ipc_runtime::api::TransferContext, i64> {
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_ESRCH)?;
    let stream = pending.stream.as_ref().ok_or(LINUX_ESTALE)?;
    let len = u64::try_from(len).map_err(|_| LINUX_EOVERFLOW)?;
    if stream.end() != stream.start().checked_add(len).ok_or(LINUX_EOVERFLOW)? {
        return Err(LINUX_ESTALE);
    }
    let receiver_side = match socket.channel_side() {
        1 => 2,
        2 => 1,
        _ => return Err(LINUX_ENOTSOCK),
    };
    Ok(kernel_ipc_runtime::api::TransferContext {
        source: kernel_ipc_runtime::api::ProcessIdentity {
            pid: snapshot.process_id(),
            generation: snapshot.process_generation(),
        },
        service: kernel_ipc_runtime::api::ServiceIdentity {
            service_id: linux_abi::IPC_SERVICE_NETD,
            epoch: super::super::ipc_ops::service_endpoint_epoch(linux_abi::IPC_SERVICE_NETD)
                .ok_or(LINUX_ENOSYS)?,
        },
        channel: kernel_ipc_runtime::api::ChannelIdentity {
            channel_id: socket.channel_id(),
            generation: socket.channel_id(),
            receiver_side,
        },
        stream_start: stream.start(),
        stream_end: stream.end(),
        intended_receiver: None,
        receiver_open_description: 0,
    })
}

fn copy_current_payload(request: &mut NetdIpcRequest, ptr: u64, len: u64) -> Result<(), i64> {
    let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
    if len > NETD_IPC_PAYLOAD_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    if len == 0 {
        return Ok(());
    }
    usermem::copy_from_current_user_exact(ptr, &mut request.payload[..len])
        .map_err(address_space_error_to_linux_errno)?;
    request.payload_len = len as u32;
    Ok(())
}

fn consume_netd_response_payload(
    request: &NetdIpcRequest,
    response: &NetdIpcResponse,
    receive_context: Option<(u64, u64, u8, u64)>,
) -> Result<(), i64> {
    let payload_len = response.payload_len as usize;
    if payload_len > NETD_IPC_PAYLOAD_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => {
            let max_len = usize::try_from(request.arg2).map_err(|_| LINUX_EINVAL)?;
            if payload_len > max_len {
                return Err(LINUX_EINVAL);
            }
            if payload_len != 0 {
                usermem::write_current_user_bytes(request.arg1, &response.payload[..payload_len])
                    .map_err(address_space_error_to_linux_errno)?;
            }
            Ok(())
        }
        SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => {
            let mut header =
                usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(request.arg1)
                    .map_err(address_space_error_to_linux_errno)?;
            if payload_len < NETD_RECVMSG_PAYLOAD_HEADER_SIZE {
                return Err(LINUX_EINVAL);
            }
            let data_len = u32::from_ne_bytes(
                response.payload[0..4]
                    .try_into()
                    .map_err(|_| LINUX_EINVAL)?,
            ) as usize;
            let control_len = u32::from_ne_bytes(
                response.payload[4..8]
                    .try_into()
                    .map_err(|_| LINUX_EINVAL)?,
            ) as usize;
            let msg_flags = u32::from_ne_bytes(
                response.payload[8..12]
                    .try_into()
                    .map_err(|_| LINUX_EINVAL)?,
            );
            let data_start = NETD_RECVMSG_PAYLOAD_HEADER_SIZE;
            let control_start = data_start.checked_add(data_len).ok_or(LINUX_EINVAL)?;
            let end = control_start.checked_add(control_len).ok_or(LINUX_EINVAL)?;
            if end > payload_len || data_len != response.value as usize {
                return Err(LINUX_EINVAL);
            }
            let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
            write_current_iovec_payload(&iovecs, &response.payload[data_start..control_start])?;
            header.msg_namelen = 0;
            let receive_context = receive_context.ok_or(LINUX_ESTALE)?;
            let (control_written, control_flags, prepared) = write_current_control_payload(
                header.msg_control,
                header.msg_controllen,
                &response.payload[control_start..end],
                receive_context,
            )?;
            header.msg_controllen = control_written as u64;
            header.msg_flags = msg_flags | control_flags;
            usermem::write_current_user_struct(request.arg1, &header)
                .map_err(address_space_error_to_linux_errno)?;
            if let Some(prepared) = prepared {
                prepared.commit()?;
            }
            Ok(())
        }
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME
        | SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME => write_current_sockaddr_payload(
            request.arg1,
            request.arg2,
            &response.payload[..payload_len],
        ),
        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT => write_current_sockopt_payload(
            request.arg3,
            request.arg4,
            &response.payload[..payload_len],
        ),
        _ => Ok(()),
    }
}

pub(super) fn consume_netd_release_payload(response: &mut NetdIpcResponse) -> Result<(), i64> {
    let release_len = response.reserved0 as usize;
    if release_len == 0 {
        return Ok(());
    }
    let payload_len = response.payload_len as usize;
    let visible_len = payload_len.checked_sub(release_len).ok_or(LINUX_EINVAL)?;
    drop_encoded_transfer_descriptors(&response.payload[visible_len..payload_len])?;
    response.payload_len = visible_len as u32;
    response.reserved0 = 0;
    Ok(())
}

/// Stream bytes one offload payload can carry beside its control block.
///
/// Zero room is not a short write, it is a message with no prefix to take, so
/// it reports the same `EMSGSIZE` as a control block that cannot fit at all.
fn sendmsg_data_capacity(control_len: usize) -> Result<usize, i64> {
    NETD_IPC_PAYLOAD_CAPACITY
        .checked_sub(NETD_SENDMSG_PAYLOAD_HEADER_SIZE)
        .and_then(|room| room.checked_sub(control_len))
        .filter(|room| *room != 0)
        .ok_or(LINUX_EMSGSIZE)
}

fn read_current_iovec_payload(iov_ptr: u64, iov_len: u64) -> Result<Vec<u8>, i64> {
    let iovecs = read_current_iovecs_payload(iov_ptr, iov_len)?;
    let total = iovec_payload_total_len(&iovecs)?;
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

fn read_current_iovecs_payload(
    iov_ptr: u64,
    iov_len: u64,
) -> Result<Vec<linux_abi::LinuxIovec>, i64> {
    let iov_len = usize::try_from(iov_len).map_err(|_| LINUX_EINVAL)?;
    if iov_ptr == 0 || iov_len == 0 || iov_len > NETD_MAX_IOVEC_COUNT {
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

fn iovec_payload_total_len(iovecs: &[linux_abi::LinuxIovec]) -> Result<usize, i64> {
    let mut total = 0usize;
    for iov in iovecs {
        let len = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?;
        total = total.checked_add(len).ok_or(LINUX_EINVAL)?;
        if total > NETD_IPC_PAYLOAD_CAPACITY {
            return Err(LINUX_EINVAL);
        }
    }
    Ok(total)
}

fn write_current_iovec_payload(iovecs: &[linux_abi::LinuxIovec], bytes: &[u8]) -> Result<(), i64> {
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

fn write_current_control_payload(
    control_ptr: u64,
    control_capacity: u64,
    control: &[u8],
    receive_context: (u64, u64, u8, u64),
) -> Result<
    (
        usize,
        u32,
        Option<super::super::ipc_ops::PreparedTransferInstall>,
    ),
    i64,
> {
    if control.is_empty() {
        return Ok((0, 0, None));
    }
    let decoded_len = decoded_transfer_control_len(control)?;
    let capacity = usize::try_from(control_capacity).map_err(|_| LINUX_EINVAL)?;
    if control_ptr == 0 || capacity == 0 {
        drop_encoded_transfer_descriptors(control)?;
        return Ok((0, linux_abi::MSG_CTRUNC as u32, None));
    }
    if capacity < decoded_len {
        drop_encoded_transfer_descriptors(control)?;
        return Ok((0, linux_abi::MSG_CTRUNC as u32, None));
    }
    let (control, prepared) = decode_transfer_control_payload(control, receive_context)?;
    usermem::write_current_user_bytes(control_ptr, control.as_slice())
        .map_err(address_space_error_to_linux_errno)?;
    Ok((control.len(), 0, prepared))
}

struct EncodedControlPayload {
    bytes: Vec<u8>,
    descriptors: Vec<kernel_ipc_runtime::api::KernelTransferredHandle>,
    tickets: Vec<kernel_ipc_runtime::api::KernelTransferTicket>,
}

fn read_current_control_bytes(control_ptr: u64, control_len: u64) -> Result<Vec<u8>, i64> {
    let control_len = usize::try_from(control_len).map_err(|_| LINUX_EINVAL)?;
    if control_ptr == 0 || control_len == 0 {
        return Ok(Vec::new());
    }
    if control_len > NETD_IPC_PAYLOAD_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let mut control = alloc::vec![0_u8; control_len];
    usermem::copy_from_current_user_exact(control_ptr, &mut control)
        .map_err(address_space_error_to_linux_errno)?;
    Ok(control)
}

fn encode_transfer_control_payload(
    control: &[u8],
    context: kernel_ipc_runtime::api::TransferContext,
) -> Result<EncodedControlPayload, i64> {
    let mut all_descriptors = Vec::new();
    let skeleton = rewrite_scm_rights_control(control, |fd_bytes| {
        if fd_bytes.len() % core::mem::size_of::<i32>() != 0 {
            return Err(LINUX_EINVAL);
        }
        let mut fds = Vec::with_capacity(fd_bytes.len() / core::mem::size_of::<i32>());
        for chunk in fd_bytes.chunks_exact(core::mem::size_of::<i32>()) {
            fds.push(i32::from_ne_bytes(
                chunk.try_into().map_err(|_| LINUX_EINVAL)?,
            ));
        }
        let descriptors = super::super::ipc_ops::export_current_fds_for_transfer(fds.as_slice())?;
        let placeholder = alloc::vec![0_u8; descriptors.len() * TRANSFER_TICKET_WIRE_BYTES];
        all_descriptors.extend_from_slice(descriptors.as_slice());
        Ok(placeholder)
    });
    let skeleton = match skeleton {
        Ok(bytes) => bytes,
        Err(errno) => {
            super::super::ipc_ops::drop_transfer_descriptors(all_descriptors.as_slice());
            return Err(errno);
        }
    };
    let tickets = if all_descriptors.is_empty() {
        Vec::new()
    } else {
        match super::super::ipc_ops::transfer_tickets_for_descriptors(
            all_descriptors.as_slice(),
            context,
        ) {
            Ok(tickets) => tickets,
            Err(errno) => {
                super::super::ipc_ops::drop_transfer_descriptors(all_descriptors.as_slice());
                return Err(errno);
            }
        }
    };
    let mut cursor = 0usize;
    let bytes = rewrite_scm_rights_control(skeleton.as_slice(), |placeholder| {
        let count = placeholder.len() / TRANSFER_TICKET_WIRE_BYTES;
        let end = cursor.checked_add(count).ok_or(LINUX_EINVAL)?;
        let encoded = encode_transfer_tickets(tickets.get(cursor..end).ok_or(LINUX_EINVAL)?)?;
        cursor = end;
        Ok(encoded)
    });
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(errno) => {
            super::super::ipc_ops::drop_transfer_descriptors(all_descriptors.as_slice());
            return Err(errno);
        }
    };
    if cursor != tickets.len() {
        super::super::ipc_ops::drop_transfer_descriptors(all_descriptors.as_slice());
        return Err(LINUX_EINVAL);
    }
    Ok(EncodedControlPayload {
        bytes,
        descriptors: all_descriptors,
        tickets,
    })
}

fn decode_transfer_control_payload(
    control: &[u8],
    receive_context: (u64, u64, u8, u64),
) -> Result<
    (
        Vec<u8>,
        Option<super::super::ipc_ops::PreparedTransferInstall>,
    ),
    i64,
> {
    let mut tickets = Vec::new();
    let mut counts = Vec::new();
    rewrite_scm_rights_control(control, |ticket_bytes| {
        if !ticket_bytes
            .len()
            .is_multiple_of(TRANSFER_TICKET_WIRE_BYTES)
        {
            return Err(LINUX_EINVAL);
        }
        let count = ticket_bytes.len() / TRANSFER_TICKET_WIRE_BYTES;
        counts.push(count);
        for chunk in ticket_bytes.chunks_exact(TRANSFER_TICKET_WIRE_BYTES) {
            tickets.push(read_transfer_ticket(chunk)?);
        }
        Ok(alloc::vec![0_u8; count * core::mem::size_of::<i32>()])
    })?;
    if tickets.is_empty() {
        return Ok((control.to_vec(), None));
    }
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_ESRCH)?;
    let (stream_pos, channel_id, channel_side, receiver_open_description) = receive_context;
    let prepared = super::super::ipc_ops::prepare_transfer_tickets_for_current_process(
        tickets.as_slice(),
        kernel_ipc_runtime::api::ProcessIdentity {
            pid: snapshot.process_id(),
            generation: snapshot.process_generation(),
        },
        kernel_ipc_runtime::api::ServiceIdentity {
            service_id: linux_abi::IPC_SERVICE_NETD,
            epoch: super::super::ipc_ops::service_endpoint_epoch(linux_abi::IPC_SERVICE_NETD)
                .ok_or(LINUX_ENOSYS)?,
        },
        kernel_ipc_runtime::api::ChannelIdentity {
            channel_id,
            generation: channel_id,
            receiver_side: channel_side,
        },
        stream_pos,
        receiver_open_description,
    )?;
    let fds = prepared.fds()?;
    let mut record = 0usize;
    let mut fd_offset = 0usize;
    let bytes = rewrite_scm_rights_control(control, |_ticket_bytes| {
        let count = *counts.get(record).ok_or(LINUX_EINVAL)?;
        record += 1;
        let end = fd_offset.checked_add(count).ok_or(LINUX_EINVAL)?;
        let fd_slice = fds.get(fd_offset..end).ok_or(LINUX_EINVAL)?;
        let bytes = unsafe {
            core::slice::from_raw_parts(
                fd_slice.as_ptr().cast::<u8>(),
                fd_slice.len() * core::mem::size_of::<i32>(),
            )
        };
        fd_offset = end;
        Ok(bytes.to_vec())
    })?;
    if record != counts.len() || fd_offset != fds.len() {
        return Err(LINUX_EINVAL);
    }
    Ok((bytes, Some(prepared)))
}

fn decoded_transfer_control_len(control: &[u8]) -> Result<usize, i64> {
    let mut total = 0usize;
    let mut offset = 0usize;
    while offset + core::mem::size_of::<linux_abi::LinuxCmsghdr>() <= control.len() {
        let header = read_control_header(&control[offset..])?;
        let cmsg_len = usize::try_from(header.cmsg_len).map_err(|_| LINUX_EINVAL)?;
        if cmsg_len < core::mem::size_of::<linux_abi::LinuxCmsghdr>()
            || offset + cmsg_len > control.len()
        {
            return Err(LINUX_EINVAL);
        }
        let next = next_control_offset(control.len(), offset, cmsg_len)?;
        let data_len = cmsg_len - core::mem::size_of::<linux_abi::LinuxCmsghdr>();
        let visible_len = if header.cmsg_level == linux_abi::SOL_SOCKET as u32
            && header.cmsg_type == linux_abi::SCM_RIGHTS as u32
        {
            if !data_len.is_multiple_of(TRANSFER_TICKET_WIRE_BYTES) {
                return Err(LINUX_EINVAL);
            }
            let fd_bytes = (data_len / TRANSFER_TICKET_WIRE_BYTES)
                .checked_mul(core::mem::size_of::<i32>())
                .ok_or(LINUX_EINVAL)?;
            core::mem::size_of::<linux_abi::LinuxCmsghdr>()
                .checked_add(fd_bytes)
                .ok_or(LINUX_EINVAL)?
        } else {
            cmsg_len
        };
        total = total
            .checked_add(cmsg_align(visible_len))
            .ok_or(LINUX_EINVAL)?;
        offset = next;
    }
    if offset != control.len() {
        return Err(LINUX_EINVAL);
    }
    Ok(total)
}

fn drop_encoded_transfer_descriptors(control: &[u8]) -> Result<(), i64> {
    let mut tickets = Vec::new();
    let mut offset = 0usize;
    while offset + core::mem::size_of::<linux_abi::LinuxCmsghdr>() <= control.len() {
        let header = read_control_header(&control[offset..])?;
        let cmsg_len = usize::try_from(header.cmsg_len).map_err(|_| LINUX_EINVAL)?;
        if cmsg_len < core::mem::size_of::<linux_abi::LinuxCmsghdr>()
            || offset + cmsg_len > control.len()
        {
            return Err(LINUX_EINVAL);
        }
        let next = next_control_offset(control.len(), offset, cmsg_len)?;
        if header.cmsg_level == linux_abi::SOL_SOCKET as u32
            && header.cmsg_type == linux_abi::SCM_RIGHTS as u32
        {
            let data_start = offset + core::mem::size_of::<linux_abi::LinuxCmsghdr>();
            let data = &control[data_start..offset + cmsg_len];
            if !data.len().is_multiple_of(TRANSFER_TICKET_WIRE_BYTES) {
                return Err(LINUX_EINVAL);
            }
            for chunk in data.chunks_exact(TRANSFER_TICKET_WIRE_BYTES) {
                tickets.push(read_transfer_ticket(chunk)?);
            }
        }
        offset = next;
    }
    if offset != control.len() {
        return Err(LINUX_EINVAL);
    }
    super::super::ipc_ops::drop_transfer_tickets(tickets.as_slice());
    Ok(())
}

fn rewrite_scm_rights_control<F>(control: &[u8], mut rewrite: F) -> Result<Vec<u8>, i64>
where
    F: FnMut(&[u8]) -> Result<Vec<u8>, i64>,
{
    let mut out = Vec::with_capacity(control.len());
    let mut offset = 0usize;
    while offset + core::mem::size_of::<linux_abi::LinuxCmsghdr>() <= control.len() {
        let header = read_control_header(&control[offset..])?;
        let cmsg_len = usize::try_from(header.cmsg_len).map_err(|_| LINUX_EINVAL)?;
        if cmsg_len < core::mem::size_of::<linux_abi::LinuxCmsghdr>()
            || offset + cmsg_len > control.len()
        {
            return Err(LINUX_EINVAL);
        }
        let next = next_control_offset(control.len(), offset, cmsg_len)?;
        if header.cmsg_level == linux_abi::SOL_SOCKET as u32
            && header.cmsg_type == linux_abi::SCM_RIGHTS as u32
        {
            let data_start = offset + core::mem::size_of::<linux_abi::LinuxCmsghdr>();
            let rewritten = rewrite(&control[data_start..offset + cmsg_len])?;
            let new_len = core::mem::size_of::<linux_abi::LinuxCmsghdr>()
                .checked_add(rewritten.len())
                .ok_or(LINUX_EINVAL)?;
            let mut new_header = header;
            new_header.cmsg_len = new_len as u64;
            let start = out.len();
            out.resize(start + cmsg_align(new_len), 0);
            write_control_header(
                &mut out[start..start + core::mem::size_of::<linux_abi::LinuxCmsghdr>()],
                new_header,
            );
            out[start + core::mem::size_of::<linux_abi::LinuxCmsghdr>()..start + new_len]
                .copy_from_slice(rewritten.as_slice());
        } else {
            out.extend_from_slice(&control[offset..next]);
        }
        offset = next;
    }
    if offset != control.len() {
        return Err(LINUX_EINVAL);
    }
    Ok(out)
}

fn read_control_header(bytes: &[u8]) -> Result<linux_abi::LinuxCmsghdr, i64> {
    if bytes.len() < core::mem::size_of::<linux_abi::LinuxCmsghdr>() {
        return Err(LINUX_EINVAL);
    }
    Ok(linux_abi::LinuxCmsghdr {
        cmsg_len: u64::from_ne_bytes(bytes[0..8].try_into().map_err(|_| LINUX_EINVAL)?),
        cmsg_level: u32::from_ne_bytes(bytes[8..12].try_into().map_err(|_| LINUX_EINVAL)?),
        cmsg_type: u32::from_ne_bytes(bytes[12..16].try_into().map_err(|_| LINUX_EINVAL)?),
    })
}

fn write_control_header(dest: &mut [u8], header: linux_abi::LinuxCmsghdr) {
    dest[0..8].copy_from_slice(&header.cmsg_len.to_ne_bytes());
    dest[8..12].copy_from_slice(&header.cmsg_level.to_ne_bytes());
    dest[12..16].copy_from_slice(&header.cmsg_type.to_ne_bytes());
}

const TRANSFER_TICKET_WIRE_BYTES: usize = rustos_user_abi::syscall::IPC_TRANSFER_TICKET_WIRE_BYTES;

fn encode_transfer_tickets(
    tickets: &[kernel_ipc_runtime::api::KernelTransferTicket],
) -> Result<Vec<u8>, i64> {
    let byte_len = tickets
        .len()
        .checked_mul(TRANSFER_TICKET_WIRE_BYTES)
        .ok_or(LINUX_EINVAL)?;
    let mut bytes = Vec::with_capacity(byte_len);
    for ticket in tickets {
        let wire = rustos_user_abi::syscall::IpcTransferTicketWire::new(
            ticket.transfer_id(),
            ticket.nonce(),
            ticket.batch_generation(),
        )
        .ok_or(LINUX_EINVAL)?;
        bytes.extend_from_slice(&wire.encode());
    }
    Ok(bytes)
}

fn read_transfer_ticket(
    bytes: &[u8],
) -> Result<kernel_ipc_runtime::api::KernelTransferTicket, i64> {
    let wire =
        rustos_user_abi::syscall::IpcTransferTicketWire::decode(bytes).ok_or(LINUX_EINVAL)?;
    kernel_ipc_runtime::api::KernelTransferTicket::new(
        wire.transfer_id(),
        wire.nonce(),
        wire.batch_generation(),
    )
    .ok_or(LINUX_EINVAL)
}

fn next_control_offset(total_len: usize, offset: usize, cmsg_len: usize) -> Result<usize, i64> {
    let aligned_next = offset
        .checked_add(cmsg_align(cmsg_len))
        .ok_or(LINUX_EINVAL)?;
    let unaligned_next = offset.checked_add(cmsg_len).ok_or(LINUX_EINVAL)?;
    let next = if aligned_next <= total_len {
        aligned_next
    } else if unaligned_next == total_len {
        unaligned_next
    } else {
        return Err(LINUX_EINVAL);
    };
    if next <= offset {
        return Err(LINUX_EINVAL);
    }
    Ok(next)
}

fn cmsg_align(len: usize) -> usize {
    let align = core::mem::size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

fn write_current_sockaddr_payload(addr_ptr: u64, len_ptr: u64, payload: &[u8]) -> Result<(), i64> {
    if len_ptr == 0 {
        return if payload.is_empty() {
            Ok(())
        } else {
            Err(LINUX_EINVAL)
        };
    }
    if addr_ptr != 0 && !payload.is_empty() {
        let capacity = usermem::read_current_user_struct::<u32>(len_ptr)
            .map_err(address_space_error_to_linux_errno)? as usize;
        if capacity < payload.len() {
            return Err(LINUX_EINVAL);
        }
        usermem::write_current_user_bytes(addr_ptr, payload)
            .map_err(address_space_error_to_linux_errno)?;
    }
    let len = payload.len() as u32;
    usermem::write_current_user_struct(len_ptr, &len).map_err(address_space_error_to_linux_errno)
}

fn write_current_sockopt_payload(
    optval_ptr: u64,
    optlen_ptr: u64,
    payload: &[u8],
) -> Result<(), i64> {
    if optval_ptr == 0 || optlen_ptr == 0 {
        return Err(LINUX_EINVAL);
    }
    let capacity = usermem::read_current_user_struct::<u32>(optlen_ptr)
        .map_err(address_space_error_to_linux_errno)? as usize;
    if capacity < payload.len() {
        return Err(LINUX_EINVAL);
    }
    usermem::write_current_user_bytes(optval_ptr, payload)
        .map_err(address_space_error_to_linux_errno)?;
    let len = payload.len() as u32;
    usermem::write_current_user_struct(optlen_ptr, &len).map_err(address_space_error_to_linux_errno)
}

#[cfg(test)]
mod tests;
