use super::*;
use rustos_user_abi::syscall::{
    VFS_IPC_OP_RENAMEAT2, VFS_IPC_OP_STATFS, VFS_IPC_OP_SYMLINKAT,
};

mod inet_datagram;
pub fn syscall_linux_vfs_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    if let Some(result) = super::local_fd::not_seekable(fd) {
        return result;
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
    let _cursor = match acquire_vfs_cursor_gate(remote.remote_id()) {
        Ok(gate) => gate,
        Err(errno) => return linux_errno(errno),
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

pub fn kernel_vfs_fstat(fd: u64) -> Result<linux_abi::LinuxStat, i64> {
    if let Some(memfd) = current_memfd_handle(fd) {
        return Ok(super::anon_stat::memfd_stat(&memfd));
    }
    let remote = current_remote_vfs_handle(fd).ok_or(LINUX_EBADF)?;
    let mut request = new_vfs_request(VFS_IPC_OP_FSTAT);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    let response = call_pinned_remote_vfs_request(&request)?;
    ensure_vfs_status(&response)?;
    if response.payload_len as usize != LINUX_STAT_SIZE {
        return Err(LINUX_EINVAL);
    }
    Ok(read_unaligned::<linux_abi::LinuxStat>(&response.payload[..LINUX_STAT_SIZE]))
}

pub fn kernel_vfs_statx_path(path: &str) -> Result<linux_abi::LinuxStatx, i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_STATX);
    proc_identity::attach_caller_exec_path(&mut request);
    request.arg1 = 0x7ff;
    populate_vfs_path_base(&mut request, linux_abi::AT_FDCWD as u64, path)?;
    let response = call_pinned_remote_vfs_request(&request)?;
    ensure_vfs_status(&response)?;
    if response.payload_len as usize != LINUX_STATX_SIZE {
        return Err(LINUX_EINVAL);
    }
    Ok(read_unaligned::<linux_abi::LinuxStatx>(&response.payload[..LINUX_STATX_SIZE]))
}

pub fn syscall_linux_vfs_fstat(fd: u64, stat_ptr: u64) -> u64 {
    if let Some(result) = super::local_fd::fstat(fd, stat_ptr) {
        return result;
    }
    if let Some(multitask::KernelHandle::Console(console)) = current_kernel_handle(fd) {
        return write_bootstrap_stat(stat_ptr, console.token_id(), 0);
    }
    if let Some(multitask::KernelHandle::DeviceBuffer(buffer)) = current_kernel_handle(fd) {
        return write_device_buffer_stat(stat_ptr, &buffer);
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

/// `ftruncate` on a descriptor without write access: `EINVAL` for a readable
/// description, `EBADF` for one that grants neither read nor write (`O_PATH`).
fn ftruncate_unwritable_errno(rights: kernel_object::api::HandleRights) -> i64 {
    if rights.allows_read() {
        LINUX_EINVAL
    } else {
        LINUX_EBADF
    }
}

pub fn syscall_linux_vfs_ftruncate(fd: u64, len: u64) -> u64 {
    // Linux rejects a negative `off_t` before it looks at the descriptor.
    if len > i64::MAX as u64 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Some((memfd, rights)) = current_memfd_handle_with_rights(fd) {
        if !rights.allows_write() {
            return linux_errno(ftruncate_unwritable_errno(rights));
        }
        let Ok(len) = usize::try_from(len) else {
            return linux_errno(LINUX_EINVAL);
        };
        return match memfd.truncate(len) {
            Ok(()) => 0,
            Err(err) => linux_errno(memfd_error_to_linux_errno(err)),
        };
    }
    let Some((remote, rights)) = current_remote_vfs_handle_with_rights(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    if !rights.allows_write() {
        return linux_errno(ftruncate_unwritable_errno(rights));
    }
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

pub(crate) fn kernel_vfs_getdents_raw(fd: u64, dest: &mut [u8]) -> Result<usize, i64> {
    let remote = current_remote_vfs_handle(fd).ok_or(LINUX_EBADF)?;
    let remote_id = remote.remote_id();
    acquire_current_remote_vfs_ref(fd, remote_id)?;
    let result = (|| {
        let _cursor = acquire_vfs_cursor_gate(remote_id)?;
        let mut request = new_vfs_request(VFS_IPC_OP_GETDENTS64);
        request.fd = fd;
        request.remote_id = remote_id;
        request.arg1 = dest.len() as u64;
        let response = match call_vfs_ipc_request(&request) {
            Ok(response) => response,
            Err(errno) => {
                let _ = settle_vfs_cursor_mutation(&request, false);
                return Err(errno);
            }
        };
        vfs_getdents_status(&request, &response)?;
        let len = response.payload_len as usize;
        if len > dest.len() || len > response.payload.len() {
            let _ = settle_vfs_cursor_mutation(&request, false);
            return Err(LINUX_EINVAL);
        }
        dest[..len].copy_from_slice(&response.payload[..len]);
        match settle_vfs_cursor_mutation(&request, true) {
            Ok(()) => Ok(len),
            Err(errno) => {
                if len > 0 {
                    Ok(len)
                } else {
                    Err(errno)
                }
            }
        }
    })();
    release_service_handle_refs_bounded(&[ServiceHandleRef::RemoteVfs(remote_id)]);
    result
}

/// EIO may be a prepared replay whose directory page changed. Exact cancel
/// is harmless for a fresh error and prevents wedging a prepared cursor.
fn vfs_getdents_status(request: &VfsIpcRequest, response: &VfsIpcResponse) -> Result<(), i64> {
    let status = ensure_vfs_status(response);
    if status == Err(LINUX_EIO) {
        let _ = settle_vfs_cursor_mutation(request, false);
    }
    status
}

pub fn syscall_linux_vfs_getdents64(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if let Some(result) = proc_identity::getdents_snapshot(fd, user_ptr, user_len) {
        return result;
    }
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
        let _cursor = match acquire_vfs_cursor_gate(remote_id) {
            Ok(gate) => gate,
            Err(errno) => return linux_errno(errno),
        };
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
        if let Err(errno) = vfs_getdents_status(&request, &response) {
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
    let path = match copy_current_user_path_allow_empty(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    syscall_linux_vfs_statx_str(dirfd, &path, flags, mask, statx_ptr)
}

pub fn syscall_linux_vfs_statx_str(
    dirfd: u64,
    path: &str,
    flags: u64,
    mask: u64,
    statx_ptr: u64,
) -> u64 {
    if path.contains("startup-programs") || path.contains("runtime-env") {
        crate::debug::info!(
            compat,
            "bootstrap path probe: op=statx dirfd={:#x} flags={:#x} path={}",
            dirfd,
            flags,
            path
        );
    }
    if let Some(result) = super::local_fd::statx_at_empty_path(dirfd, &path, flags, statx_ptr) {
        return result;
    }
    if path.is_empty() && flags & linux_abi::AT_EMPTY_PATH == 0 {
        return linux_errno(LINUX_ENOENT);
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(statx_ptr, LINUX_STATX_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_STATX);
    proc_identity::attach_caller_exec_path(&mut request);
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
    if let Some(result) = proc_identity::complete_stat(
        response.aux,
        flags,
        proc_identity::StatOut::Statx { ptr: statx_ptr, mask },
    ) {
        return result;
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
    let path = match copy_current_user_path_allow_empty(path_ptr, VFS_IPC_PATH_CAPACITY) {
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
    if let Some(result) = super::local_fd::fstat_at_empty_path(dirfd, &path, flags, stat_ptr) {
        return result;
    }
    if path.is_empty() && flags & linux_abi::AT_EMPTY_PATH == 0 {
        return linux_errno(LINUX_ENOENT);
    }
    if path.is_empty()
        && flags & linux_abi::AT_EMPTY_PATH != 0
        && matches!(
            current_kernel_handle(dirfd),
            Some(
                multitask::KernelHandle::Console(_) | multitask::KernelHandle::DeviceBuffer(_)
            )
        )
    {
        return syscall_linux_vfs_fstat(dirfd, stat_ptr);
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(stat_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_NEWFSTATAT);
    proc_identity::attach_caller_exec_path(&mut request);
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
    if let Some(result) = proc_identity::complete_stat(
        response.aux,
        flags,
        proc_identity::StatOut::Stat { ptr: stat_ptr },
    ) {
        return result;
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
    // `bufsiz` is a C `int` (sign-extended by `int_args`): zero or negative is
    // EINVAL, not an empty success and not a huge buffer that faults.
    if (user_len as i64) <= 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_READLINKAT);
    proc_identity::attach_caller_exec_path(&mut request);
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
    if let Some(resolved) = proc_identity::complete_readlink(response.aux) {
        return match resolved {
            Ok(target) => {
                let bytes = target.as_bytes();
                let len = bytes.len().min(user_len);
                match usermem::write_current_user_bytes(user_ptr, &bytes[..len]) {
                    Ok(()) => len as u64,
                    Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
                }
            }
            Err(errno) => linux_errno(errno),
        };
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
    let response = match call_pinned_remote_vfs_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    if let Some(result) = proc_identity::complete_access(response.aux, mode) {
        return result;
    }
    0
}

pub fn current_memfd_handle(fd: u64) -> Option<multitask::MemfdHandle> {
    current_memfd_handle_with_rights(fd).map(|(memfd, _)| memfd)
}

pub fn current_memfd_handle_with_rights(
    fd: u64,
) -> Option<(
    multitask::MemfdHandle,
    kernel_object::api::handle::HandleRights,
)> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let entry = process_state.handles().get_entry(fd)?;
        match entry.handle() {
            multitask::KernelHandle::Memfd(memfd) => Some((memfd.clone(), entry.rights())),
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
/// App DMA-BUF identity for clients that must authenticate a received fd:
/// character device `rdev` = `APP_BUFFER_STAT_RDEV`, `st_dev` = provider
/// generation, `st_ino` = allocation id, `st_size` = capability length. These
/// are kernel-synthesised from the held capability, never from a descriptor.
fn write_device_buffer_stat(user_ptr: u64, buffer: &kernel_ps::api::handles::BufferCapability) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let id = buffer.identity();
    let mut out = [0_u8; LINUX_STAT_SIZE];
    out[0..8].copy_from_slice(&id.owner.generation.to_ne_bytes());
    out[8..16].copy_from_slice(&id.allocation.to_ne_bytes());
    out[16..24].copy_from_slice(&1_u64.to_ne_bytes());
    out[24..28].copy_from_slice(&(0o020000_u32 | 0o600).to_ne_bytes());
    out[40..48].copy_from_slice(&rustos_user_abi::syscall::APP_BUFFER_STAT_RDEV.to_ne_bytes());
    out[48..56].copy_from_slice(&(buffer.len() as i64).to_ne_bytes());
    out[56..64].copy_from_slice(&4096_i64.to_ne_bytes());
    match usermem::write_current_user_bytes(user_ptr, &out) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
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

pub(crate) fn kernel_vfs_unlink_path(path: &str, flags: u64) -> Result<(), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_UNLINKAT);
    request.flags = flags as u32;
    populate_vfs_path_base(&mut request, linux_abi::AT_FDCWD as u64, path)?;
    let response = call_pinned_remote_vfs_request(&request)?;
    ensure_vfs_status(&response)
}

pub fn syscall_linux_vfs_truncate(path_ptr: u64, len: u64) -> u64 {
    if len > i64::MAX as u64 {
        return linux_errno(LINUX_EINVAL);
    }
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut open_req = new_vfs_request(VFS_IPC_OP_OPENAT);
    open_req.arg0 = linux_abi::O_WRONLY as u64;
    open_req.arg1 = 0;
    open_req.arg3 = mint_service_object_id();
    if let Err(errno) = populate_vfs_path_base(&mut open_req, linux_abi::AT_FDCWD as u64, &path) {
        return linux_errno(errno);
    }
    let open_resp = match call_vfs_ipc_request(&open_req) {
        Ok(resp) => resp,
        Err(errno) => {
            let _ = call_vfs_remote_close_bounded(open_req.arg3);
            return linux_errno(errno);
        }
    };
    if let Err(errno) = ensure_vfs_status(&open_resp) {
        let _ = call_vfs_remote_close_bounded(open_req.arg3);
        return linux_errno(errno);
    }
    let remote_id = open_resp.remote_id;
    if remote_id == 0 || remote_id != open_req.arg3 {
        let _ = call_vfs_remote_close_bounded(open_req.arg3);
        return linux_errno(LINUX_EINVAL);
    }
    let mut trunc_req = new_vfs_request(VFS_IPC_OP_FTRUNCATE);
    trunc_req.remote_id = remote_id;
    trunc_req.arg0 = len;
    let res = call_vfs_ipc_request(&trunc_req).and_then(|resp| ensure_vfs_status(&resp));
    let _ = call_vfs_remote_close_bounded(remote_id);
    match res {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(crate) fn kernel_vfs_renameat2(
    olddirfd: u64,
    old_path: &str,
    newdirfd: u64,
    new_path: &str,
    flags: u64,
) -> Result<(), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_RENAMEAT2);
    request.dirfd = olddirfd;
    request.remote_id = resolve_vfs_dirfd_remote_id(olddirfd, old_path)?;
    request.arg0 = newdirfd;
    request.arg1 = flags;
    request.arg2 = resolve_vfs_dirfd_remote_id(newdirfd, new_path)?;
    let encoded_len = rustos_user_abi::syscall::encode_two_path_payload(
        old_path.as_bytes(),
        new_path.as_bytes(),
        &mut request.payload,
    )
    .ok_or(36_i64 /* ENAMETOOLONG */)?;
    request.payload_len = encoded_len as u32;
    let response = call_pinned_remote_vfs_request(&request)?;
    ensure_vfs_status(&response)
}

pub(crate) fn kernel_vfs_rename_paths(
    old_path: &str,
    new_path: &str,
    flags: u64,
) -> Result<(), i64> {
    kernel_vfs_renameat2(
        linux_abi::AT_FDCWD as u64,
        old_path,
        linux_abi::AT_FDCWD as u64,
        new_path,
        flags,
    )
}

pub fn syscall_linux_vfs_renameat2(
    olddirfd: u64,
    oldpath_ptr: u64,
    newdirfd: u64,
    newpath_ptr: u64,
    flags: u64,
) -> u64 {
    let old_path = match copy_current_user_path(oldpath_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let new_path = match copy_current_user_path(newpath_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    match kernel_vfs_renameat2(olddirfd, &old_path, newdirfd, &new_path, flags) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_vfs_symlinkat(
    target_ptr: u64,
    newdirfd: u64,
    linkpath_ptr: u64,
) -> u64 {
    let target = match copy_current_user_path(target_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let linkpath = match copy_current_user_path(linkpath_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_SYMLINKAT);
    request.dirfd = newdirfd;
    request.remote_id = match resolve_vfs_dirfd_remote_id(newdirfd, &linkpath) {
        Ok(id) => id,
        Err(errno) => return linux_errno(errno),
    };
    let Some(encoded_len) = rustos_user_abi::syscall::encode_two_path_payload(
        target.as_bytes(),
        linkpath.as_bytes(),
        &mut request.payload,
    ) else {
        return linux_errno(36 /* ENAMETOOLONG */);
    };
    request.payload_len = encoded_len as u32;
    match call_pinned_remote_vfs_request(&request).and_then(|response| ensure_vfs_status(&response))
    {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(crate) fn kernel_vfs_statfs_path(
    path: &str,
) -> Result<rustos_user_abi::syscall::VfsStatFsWire, i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_STATFS);
    request.arg0 = 0;
    populate_vfs_path_base(&mut request, linux_abi::AT_FDCWD as u64, path)?;
    let response = call_pinned_remote_vfs_request(&request)?;
    ensure_vfs_status(&response)?;
    let bytes = response
        .payload
        .get(..response.payload_len as usize)
        .ok_or(LINUX_EINVAL)?;
    rustos_user_abi::syscall::VfsStatFsWire::from_bytes(bytes).ok_or(LINUX_EINVAL)
}

pub(crate) fn kernel_vfs_fstatfs(
    fd: u64,
) -> Result<rustos_user_abi::syscall::VfsStatFsWire, i64> {
    if let Some(remote) = current_remote_vfs_handle(fd) {
        let mut request = new_vfs_request(VFS_IPC_OP_STATFS);
        request.arg0 = 1;
        request.fd = fd;
        request.remote_id = remote.remote_id();
        let response = call_pinned_remote_vfs_request(&request)?;
        ensure_vfs_status(&response)?;
        let bytes = response
            .payload
            .get(..response.payload_len as usize)
            .ok_or(LINUX_EINVAL)?;
        return rustos_user_abi::syscall::VfsStatFsWire::from_bytes(bytes).ok_or(LINUX_EINVAL);
    }
    if current_fd_exists(fd) {
        return Ok(rustos_user_abi::syscall::VfsStatFsWire {
            f_type: 0x0102_1994,
            f_bsize: 4096,
            f_namelen: 255,
            f_frsize: 4096,
            ..Default::default()
        });
    }
    Err(LINUX_EBADF)
}

pub fn syscall_linux_vfs_statfs(path_ptr: u64, buf_ptr: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let wire = match kernel_vfs_statfs_path(&path) {
        Ok(wire) => wire,
        Err(errno) => return linux_errno(errno),
    };
    match usermem::write_current_user_bytes(buf_ptr, &wire.to_bytes()) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub fn syscall_linux_vfs_fstatfs(fd: u64, buf_ptr: u64) -> u64 {
    let wire = match kernel_vfs_fstatfs(fd) {
        Ok(wire) => wire,
        Err(errno) => return linux_errno(errno),
    };
    match usermem::write_current_user_bytes(buf_ptr, &wire.to_bytes()) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
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
    // Pipes and eventfds are kernel-local: devmgrd has nothing to route.
    if let Some(local) = super::local_fd::current_local_fd(fd) {
        return super::local_fd::ioctl(&local, request_number, arg);
    }
    if request_number == linux_abi::FIONREAD
        && let Some(result) = super::vfs_socket::try_inotify_fionread(fd, arg)
    {
        return result;
    }
    if request_number == linux_abi::FIONREAD
        && current_kernel_handle(fd).is_some_and(|handle| {
            matches!(
                handle,
                multitask::KernelHandle::Socket(_) | multitask::KernelHandle::InetSocket(_)
            )
        })
    {
        return match kernel_netd_raw_socket_op(
            rustos_user_abi::syscall::SYSCALL_OFFLOAD_OP_LINUX_IOCTL,
            fd,
            [request_number, 0, 0, 0, 0],
            &[],
            None,
        ) {
            Ok((_, payload)) => {
                let Some(val_bytes) = payload.get(..core::mem::size_of::<i32>()) else {
                    return linux_errno(LINUX_EINVAL);
                };
                let val = i32::from_ne_bytes(val_bytes.try_into().unwrap_or([0; 4]));
                match usermem::write_current_user_struct(arg, &val) {
                    Ok(()) => 0,
                    Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
                }
            }
            Err(errno) => linux_errno(errno),
        };
    }
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
                ServiceCapability::UiPolicy,
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
        multitask::KernelHandle::Console(_) => rustos_user_abi::handle_kind::HANDLE_KIND_CONSOLE,
        multitask::KernelHandle::Device(_) => rustos_user_abi::handle_kind::HANDLE_KIND_DEVICE,
        multitask::KernelHandle::Epoll(_) => rustos_user_abi::handle_kind::HANDLE_KIND_EPOLL,
        multitask::KernelHandle::InetSocket(_) => rustos_user_abi::handle_kind::HANDLE_KIND_INET_SOCKET,
        multitask::KernelHandle::Memfd(_) => rustos_user_abi::handle_kind::HANDLE_KIND_MEMFD,
        multitask::KernelHandle::RemoteVfs(_) => rustos_user_abi::handle_kind::HANDLE_KIND_REMOTE_VFS,
        multitask::KernelHandle::Socket(_) => rustos_user_abi::handle_kind::HANDLE_KIND_SOCKET,
        multitask::KernelHandle::VfsDirectory(_) => rustos_user_abi::handle_kind::HANDLE_KIND_VFS_DIRECTORY,
        multitask::KernelHandle::DisplaySurface(_) => rustos_user_abi::handle_kind::HANDLE_KIND_DISPLAY_SURFACE,
        multitask::KernelHandle::DeviceBuffer(_) => rustos_user_abi::handle_kind::HANDLE_KIND_DEVICE_BUFFER,
        multitask::KernelHandle::Pipe(_) => rustos_user_abi::handle_kind::HANDLE_KIND_PIPE,
        multitask::KernelHandle::EventFd(_) => rustos_user_abi::handle_kind::HANDLE_KIND_EVENTFD,
        multitask::KernelHandle::Job(_) => rustos_user_abi::handle_kind::HANDLE_KIND_JOB,
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
        ServiceCapability::UiPolicy,
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

/// Linux `UIO_MAXIOV`. The payload copy below is bounded by the offload
/// capacity, not by the vector count, so the count is only a parse limit.
const NETD_MAX_IOVEC_COUNT: usize = 1024;

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

pub(crate) fn kernel_netd_poll_fd(fd: u64, events: u32, timeout_ms: u64) -> Result<u32, i64> {
    let snapshot = snapshot_socket(fd).ok_or(LINUX_ENOTSOCK)?;
    let outcomes = super::vfs_socket::poll_netd_socket_batch(&[(snapshot.token(), events)], timeout_ms)?;
    match outcomes.first() {
        Some(Ok((revents, _))) => Ok(*revents),
        Some(Err(errno)) => Err(*errno),
        None => Ok(0),
    }
}

pub(crate) fn kernel_netd_raw_socket_op(
    op: u16,
    fd: u64,
    args: [u64; 5],
    in_payload: &[u8],
    timeout_ms: Option<u64>,
) -> Result<(i64, Vec<u8>), i64> {
    let mut request = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op,
        arg0: fd,
        arg1: args[0],
        arg2: args[1],
        arg3: args[2],
        arg4: args[3],
        arg5: args[4],
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
    let _socket = populate_netd_socket_token(&mut request).ok_or(LINUX_ENOTSOCK)?;
    if in_payload.len() > NETD_IPC_PAYLOAD_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    if !in_payload.is_empty() {
        request.payload[..in_payload.len()].copy_from_slice(in_payload);
        request.payload_len = in_payload.len() as u32;
    }
    let deadline_class = timeout_ms.map_or(ipc_ops::ServiceIpcClass::BulkData, |_| {
        super::ipc_helpers::deadline::netd_timeout_class(op)
    });
    request.deadline_ns = netd_deadline_after_class(deadline_class, timeout_ms);
    let response = match timeout_ms {
        Some(ms) => call_netd_ipc_request_with_timeout(&request, ms)?,
        None => call_netd_ipc_request(&request)?,
    };
    let len = usize::try_from(response.payload_len).map_err(|_| LINUX_EINVAL)?;
    if len > response.payload.len() {
        return Err(LINUX_EINVAL);
    }
    Ok((response.value as i64, response.payload[..len].to_vec()))
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
    let result = syscall_linux_net6_once(op, arg0, arg1, arg2, arg3, arg4, arg5, timeout_ms);
    if result != linux_errno(LINUX_EAGAIN) || timeout_ms.is_some() {
        return result;
    }
    if op == SYSCALL_OFFLOAD_OP_LINUX_ACCEPT {
        let blocking_accept = snapshot_socket(arg0)
            .is_some_and(|snapshot| snapshot.status_flags & linux_abi::O_NONBLOCK == 0);
        if !blocking_accept {
            return result;
        }
        return wait_and_retry_local_receive(arg0, || {
            syscall_linux_net6_once(op, arg0, arg1, arg2, arg3, arg4, arg5, None)
        });
    }
    let io_flags = match op {
        SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => arg2,
        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => arg3,
        _ => return result,
    };
    if io_flags & (linux_abi::MSG_DONTWAIT | libc_msg_errqueue()) != 0 {
        return result;
    }
    // Only sockets whose queue netd owns locally wait here: INET receives
    // block inside netd's worker pool, so their `EAGAIN` is already final.
    let blocking_local = snapshot_socket(arg0).is_some_and(|snapshot| {
        snapshot.status_flags & linux_abi::O_NONBLOCK == 0
            && matches!(snapshot.handle, multitask::KernelHandle::Socket(_))
    });
    if !blocking_local {
        return result;
    }
    wait_and_retry_local_receive(arg0, || {
        syscall_linux_net6_once(op, arg0, arg1, arg2, arg3, arg4, arg5, None)
    })
}

/// `MSG_ERRQUEUE` (0x2000) never waits either; it is not in `linux_abi`.
const fn libc_msg_errqueue() -> u64 {
    0x2000
}

/// `SO_RCVTIMEO` (`struct timeval`), the only socket option the wait loop
/// reads from netd.
const LINUX_SO_RCVTIMEO: u64 = 20;
/// Longest single readiness wait: bounds how late a signal or a lapsed
/// `SO_RCVTIMEO` is noticed.
const LOCAL_RECEIVE_WAIT_SLICE_MS: u64 = 100;

/// Receive timeout in milliseconds from netd (`None` = wait forever).
fn local_receive_timeout_ms(fd: u64) -> Option<u64> {
    let args = [linux_abi::SOL_SOCKET, LINUX_SO_RCVTIMEO, 0, 0, 0];
    let (_, payload) = kernel_netd_raw_socket_op(SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT, fd, args, &[], Some(500)).ok()?;
    let seconds = i64::from_ne_bytes(payload.get(0..8)?.try_into().ok()?);
    let micros = i64::from_ne_bytes(payload.get(8..16)?.try_into().ok()?);
    let total_us = seconds.checked_mul(1_000_000)?.checked_add(micros)?;
    (total_us > 0).then(|| (total_us as u64).div_ceil(1000))
}

/// A blocking receive on a local (AF_UNIX / AF_NETLINK) socket that found its
/// queue empty: netd answers `EAGAIN`, so the *caller side* of the syscall owns
/// the wait. It parks on netd's deferred local-poll machinery (the same wait
/// `poll`/`epoll` use) in bounded slices and retries the receive, returning
/// `EINTR` on a deliverable signal and `EAGAIN` once `SO_RCVTIMEO` lapses.
fn wait_and_retry_local_receive(fd: u64, mut receive: impl FnMut() -> u64) -> u64 {
    let start = crate::arch::rtc::ticks();
    let timeout_ms = local_receive_timeout_ms(fd);
    loop {
        if super::poll_epoll::has_unmasked_pending_signal() {
            return linux_errno(LINUX_EINTR);
        }
        let elapsed = crate::arch::rtc::ticks()
            .saturating_sub(start)
            .saturating_mul(1000)
            .saturating_div(crate::arch::rtc::ticks_per_second().max(1));
        let slice = match timeout_ms {
            Some(limit) if elapsed >= limit => return linux_errno(LINUX_EAGAIN),
            Some(limit) => (limit - elapsed).min(LOCAL_RECEIVE_WAIT_SLICE_MS),
            None => LOCAL_RECEIVE_WAIT_SLICE_MS,
        };
        match kernel_netd_poll_fd(fd, linux_abi::POLLIN as u32, slice.max(1)) {
            // Readable, hung up, or the slice lapsed: either way the receive
            // below is the authority.
            Ok(_) => {}
            Err(errno) if errno == LINUX_ETIMEDOUT => {}
            Err(errno) => return linux_errno(errno),
        }
        let result = receive();
        if result != linux_errno(LINUX_EAGAIN) {
            return result;
        }
    }
}

pub(super) fn current_fd_allows_write(fd: u64) -> bool {
    multitask::with_current_user_process_state(|_, _, process_state| {
        process_state.handles().get_entry(fd).is_some_and(|entry| {
            let rights = entry.rights();
            match rights {
                kernel_object::api::HandleRights::Socket(socket_rights) => {
                    socket_rights.contains(kernel_object::api::SocketHandleRights::SEND)
                }
                _ => rights.allows_write(),
            }
        })
    })
    .unwrap_or(false)
}

pub(super) fn netd_socket_write_bytes(
    snapshot: &SocketSnapshot,
    fd: u64,
    data: &[u8],
) -> Result<usize, i64> {
    if data.is_empty() {
        return Ok(0);
    }
    let nonblocking = snapshot.status_flags & linux_abi::O_NONBLOCK != 0;
    let timeout = nonblocking
        .then_some(rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS);
    let class = timeout.map_or(ipc_ops::ServiceIpcClass::BulkData, |_| {
        super::ipc_helpers::deadline::netd_timeout_class(SYSCALL_OFFLOAD_OP_LINUX_SENDTO)
    });
    let mut operation = [0_u8; 16];
    nucleus_core::util::random::Random::new().fill_bytes(&mut operation);
    let mut request = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op: SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
        socket_token: snapshot.token(),
        deadline_ns: netd_deadline_after_class(class, timeout),
        operation_hi: u64::from_le_bytes(operation[..8].try_into().unwrap()),
        operation_lo: u64::from_le_bytes(operation[8..].try_into().unwrap()),
        arg0: fd,
        status_flags: snapshot.status_flags,
        ..NetdIpcRequest::default()
    };
    if request.operation_hi == 0 && request.operation_lo == 0 {
        request.operation_lo = 1;
    }
    if let Some(user) = multitask::current_user_snapshot() {
        request.pid = user.process_id();
        request.tid = user.thread_id();
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }
    let chunk = if snapshot.uses_netd_message() {
        let max_dgram = NETD_IPC_PAYLOAD_CAPACITY
            .saturating_sub(rustos_user_abi::syscall::netd_message::HEADER_SIZE);
        let chunk = &data[..data.len().min(max_dgram)];
        request.payload_len = rustos_user_abi::syscall::netd_message::encode(
            &mut request.payload,
            chunk,
            &[],
            0,
            &[],
        )
        .ok_or(LINUX_EMSGSIZE)? as u32;
        chunk
    } else {
        let chunk = &data[..data.len().min(NETD_IPC_PAYLOAD_CAPACITY)];
        request.payload[..chunk.len()].copy_from_slice(chunk);
        request.payload_len = chunk.len() as u32;
        chunk
    };
    let stream = match &snapshot.handle {
        multitask::KernelHandle::Socket(socket) if socket.channel_id() != 0 => loop {
            if let Some(stream) = socket.begin_stream_send(chunk.len()) {
                break Some(stream);
            }
            if nonblocking {
                return Err(LINUX_EAGAIN);
            }
            if super::poll_epoll::has_unmasked_pending_signal() {
                return Err(LINUX_EINTR);
            }
            match kernel_netd_poll_fd(fd, linux_abi::POLLOUT as u32, LOCAL_RECEIVE_WAIT_SLICE_MS) {
                Ok(_) => {}
                Err(errno) if errno == LINUX_ETIMEDOUT => {}
                Err(errno) => return Err(errno),
            }
        },
        _ => None,
    };
    let response = if nonblocking {
        call_netd_ipc_request_with_timeout(
            &request,
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
        )
        .map_err(|errno| if errno == LINUX_ETIMEDOUT { LINUX_EAGAIN } else { errno })?
    } else {
        call_netd_ipc_request(&request)?
    };
    if response.value > chunk.len() as u64 {
        return Err(LINUX_EIO);
    }
    if let Some(stream) = stream
        && !stream.commit_send(response.value as usize)
    {
        return Err(LINUX_ESTALE);
    }
    Ok(response.value as usize)
}

#[allow(
    clippy::too_many_arguments,
    reason = "mirrors the six-argument Linux socket offload plus its completion bound"
)]
fn syscall_linux_net6_once(
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
    let socket_snapshot = populate_netd_socket_token(&mut request);
    let io_flags = match op {
        SYSCALL_OFFLOAD_OP_LINUX_SENDMSG | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => arg2,
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => arg3,
        _ => 0,
    };
    let nonblocking_bound = timeout_ms.is_none()
        && socket_op_honours_nonblock(op)
        && (request.status_flags & linux_abi::O_NONBLOCK != 0
            || io_flags & (linux_abi::MSG_DONTWAIT | linux_abi::MSG_ERRQUEUE) != 0);
    // `O_NONBLOCK` bounds how long netd may wait for the *socket*, not how long
    // the reply may take: a send or receive whose reply deadline expires has
    // already been queued and still runs, so the caller cannot learn whether
    // bytes moved. Byte-moving ops therefore get the state-mutation lease, not
    // the interactive rail.
    let creates_socket=matches!(op,SYSCALL_OFFLOAD_OP_LINUX_SOCKET
        | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT);
    let timeout_ms = if creates_socket {
        Some(timeout_ms.unwrap_or(rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS))
    } else if nonblocking_bound {
        Some(if socket_op_moves_stream_bytes(op) {
            rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS
        } else {
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
        })
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
    if let Err(errno) = populate_netd_request_payload(&mut request, &mut pending_transfers, socket_snapshot.as_ref()) {
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
            let record_present = response.reserved1 & rustos_user_abi::syscall::NETD_IPC_RESPONSE_FLAG_RECORD_PRESENT != 0;
            if (response.discarded_tail != 0 || record_present)
                && !current_unix_socket_handle(socket_snapshot.as_ref()).is_some_and(|handle| {
                    matches!(
                        handle.type_(),
                        rustos_user_abi::syscall::NET_BROKER_PUBLICATION_SOCK_SEQPACKET
                            | linux_abi::SOCK_DGRAM
                    )
                })
            {
                pending_transfers.drop_pending();
                return linux_errno(LINUX_EINVAL);
            }
            let consumed = usize::try_from(response.value).ok();
            // A SEQPACKET receive that discarded a truncated tail returns the
            // copied length but dequeued the whole message: the stream
            // position must advance by what the sender's position advanced by.
            let stream_consumed = if pending_transfers.is_receive() {
                consumed.and_then(|len| len.checked_add(response.discarded_tail as usize))
                    .map(|len| if record_present { len.max(1) } else { len })
            } else {
                consumed
            };
            let result = consume_netd_response_payload(
                &request,
                &response,
                pending_transfers.receive_context(),
                socket_snapshot.as_ref(),
            );
            if pending_transfers.is_receive() && io_flags & linux_abi::MSG_PEEK != 0 {
                // A peek dequeues nothing: the stream position must not move.
                pending_transfers.drop_pending();
            } else if pending_transfers.is_receive() {
                if !stream_consumed.is_some_and(|len| pending_transfers.commit_receive(len)) {
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
    seqpacket: bool,
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
            seqpacket: false,
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
        if socket.channel_id() == 0 {
            return Ok(());
        }
        self.seqpacket = matches!(
            socket.type_(),
            rustos_user_abi::syscall::NET_BROKER_PUBLICATION_SOCK_SEQPACKET
                | linux_abi::SOCK_DGRAM
        );
        let stream = socket.begin_stream_send(if self.seqpacket { len.max(1) } else { len }).ok_or(LINUX_EAGAIN)?;
        self.channel_id = socket.channel_id();
        self.channel_side = socket.channel_side();
        self.socket_token = socket.token_id();
        self.stream = Some(stream);
        Ok(())
    }

    fn begin_receive(&mut self, socket: &multitask::SocketHandle) -> Result<(), i64> {
        if socket.channel_id() == 0 {
            return Ok(());
        }
        self.seqpacket = matches!(
            socket.type_(),
            rustos_user_abi::syscall::NET_BROKER_PUBLICATION_SOCK_SEQPACKET
                | linux_abi::SOCK_DGRAM
        );
        let stream = socket.begin_stream_receive().ok_or(LINUX_EAGAIN)?;
        self.channel_id = socket.channel_id();
        self.channel_side = socket.channel_side();
        self.socket_token = socket.token_id();
        self.receive = true;
        self.stream = Some(stream);
        Ok(())
    }

    fn commit_send(&mut self, accepted: usize) -> Result<(), i64> {
        let accepted = if self.seqpacket { accepted.max(1) } else { accepted };
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

pub(super) struct SocketSnapshot {
    pub(super) handle: multitask::KernelHandle,
    pub(super) status_flags: u64,
}

impl SocketSnapshot {
    pub(super) fn token(&self) -> u64 {
        match &self.handle {
            multitask::KernelHandle::Socket(s) => s.token_id(),
            multitask::KernelHandle::InetSocket(s) => s.token_id(),
            _ => unreachable!("socket snapshot must contain a socket"),
        }
    }
    fn is_netlink(&self) -> bool {
        matches!(&self.handle, multitask::KernelHandle::InetSocket(s)
            if s.domain() == rustos_user_abi::syscall::NET_BROKER_PUBLICATION_AF_NETLINK)
    }
    pub(super) fn is_datagram(&self) -> bool {
        // AF_NETLINK datagram sockets are netd-owned tokens with their own
        // wire; only internet datagrams use the sockaddr/packet-info envelope.
        matches!(&self.handle, multitask::KernelHandle::InetSocket(s)
            if s.type_() == linux_abi::SOCK_DGRAM && s.domain() == linux_abi::AF_INET)
    }
    pub(super) fn is_ipv6_datagram(&self) -> bool {
        matches!(&self.handle, multitask::KernelHandle::InetSocket(s)
            if s.type_() == linux_abi::SOCK_DGRAM && s.domain() == linux_abi::AF_INET6)
    }
    pub(super) fn is_unix_dgram(&self) -> bool {
        matches!(&self.handle, multitask::KernelHandle::Socket(s)
            if s.type_() == linux_abi::SOCK_DGRAM)
    }
    pub(super) fn uses_netd_message(&self) -> bool {
        self.is_datagram() || self.is_ipv6_datagram() || self.is_unix_dgram()
    }
}

pub(super) fn snapshot_socket(fd: u64) -> Option<SocketSnapshot> {
    multitask::with_current_user_process_state(|_, _, state| {
        let entry = state.handles().get_entry(fd)?;
        if !matches!(entry.handle(), multitask::KernelHandle::Socket(_) | multitask::KernelHandle::InetSocket(_)) { return None; }
        Some(SocketSnapshot { handle: entry.handle().clone(), status_flags: entry.status_flags() })
    }).flatten()
}

fn populate_netd_socket_token(request: &mut NetdIpcRequest) -> Option<SocketSnapshot> {
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
        | SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET
        | rustos_user_abi::syscall::SYSCALL_OFFLOAD_OP_LINUX_IOCTL => request.arg0,
        _ => return None,
    };
    let snapshot = snapshot_socket(fd)?;
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_ACCEPT {
        match &snapshot.handle {
            multitask::KernelHandle::Socket(listener) => {
                // The accepted socket has the listener's type; the reply validation
                // checks the published handle against it.
                request.arg4 = listener.type_();
            }
            multitask::KernelHandle::InetSocket(listener) => {
                request.arg4 = listener.type_();
                request.arg5 = listener.domain();
            }
            _ => {}
        }
    }
    request.socket_token = snapshot.token();
    request.status_flags = snapshot.status_flags;
    Some(snapshot)
}

fn populate_netd_request_payload(
    request: &mut NetdIpcRequest,
    pending_transfers: &mut PendingNetdTransfers,
    socket: Option<&SocketSnapshot>,
) -> Result<(), i64> {
    if socket.is_some_and(|s| s.is_ipv6_datagram() || s.is_unix_dgram()) {
        match request.op {
            SYSCALL_OFFLOAD_OP_LINUX_SENDTO | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG => {
                return populate_netd_msg_dgram_send(request, pending_transfers, socket);
            }
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => {
                if let Some(unix_sock) = current_unix_socket_handle(socket) {
                    pending_transfers.begin_receive(&unix_sock)?;
                }
                return Ok(());
            }
            SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => {
                let header = usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(request.arg1)
                    .map_err(address_space_error_to_linux_errno)?;
                let iovecs = if header.msg_iovlen == 0 {
                    Vec::new()
                } else {
                    read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?
                };
                request.payload_len =
                    iovec_payload_total_len(&iovecs)?.min(NETD_IPC_PAYLOAD_CAPACITY) as u32;
                request.arg4 = header.msg_controllen;
                if let Some(unix_sock) = current_unix_socket_handle(socket) {
                    pending_transfers.begin_receive(&unix_sock)?;
                }
                return Ok(());
            }
            _ => {}
        }
    }
    if socket.is_some_and(SocketSnapshot::is_datagram) {
        match request.op {
            SYSCALL_OFFLOAD_OP_LINUX_SENDTO | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG => {
                return inet_datagram::populate_send(request);
            }
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => return Ok(()),
            SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => {
                let header = usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(request.arg1)
                    .map_err(address_space_error_to_linux_errno)?;
                let iovecs = if header.msg_iovlen == 0 { Vec::new() } else {
                    read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?
                };
                request.payload_len = iovec_payload_total_len(&iovecs)?.min(NETD_IPC_PAYLOAD_CAPACITY) as u32;
                return Ok(());
            }
            _ => {}
        }
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_BIND | SYSCALL_OFFLOAD_OP_LINUX_CONNECT => {
            copy_current_payload(request, request.arg1, request.arg2)?;
            if request.op == SYSCALL_OFFLOAD_OP_LINUX_BIND
                && current_unix_socket_handle(socket).is_some()
                && request.payload_len > 2
                && u16::from_ne_bytes([request.payload[0], request.payload[1]]) as u64 == linux_abi::AF_UNIX
                && request.payload[2] != 0
            {
                let mut query = super::super::syscalld_ops::new_syscalld_request(rustos_user_abi::syscall::SYSCALL_OFFLOAD_OP_LINUX_UMASK);
                query.flags = rustos_user_abi::syscall::SYSCALL_OFFLOAD_UMASK_QUERY;
                let response = super::super::syscalld_ops::call_syscalld(query)?;
                super::super::syscalld_ops::ensure_syscalld_payload(&response, size_of::<u32>())?;
                let mask = u32::from_le_bytes(response.payload[..4].try_into().map_err(|_| LINUX_EINVAL)?);
                if mask & !0o777 != 0 { return Err(LINUX_EINVAL); }
                request.arg3 = u64::from(mask);
            }
            Ok(())
        }
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO => {
            // Stream sockets take a bounded prefix and report it, exactly as
            // `sendmsg` and `writev` do below; failing a large `write` with
            // `EINVAL` because one offload payload is smaller than the buffer
            // is a transport limit leaking into the caller's ABI. Datagram
            // sockets are never split and took the dedicated branch above.
            let seqpacket = current_unix_socket_handle(socket)
                .is_some_and(|handle| handle.type_() == rustos_user_abi::syscall::NET_BROKER_PUBLICATION_SOCK_SEQPACKET);
            if seqpacket && request.arg2 > NETD_IPC_PAYLOAD_CAPACITY as u64 {
                // A message is never split: a prefix would redefine its boundary.
                return Err(LINUX_EMSGSIZE);
            }
            let prefix_len = request.arg2.min(NETD_IPC_PAYLOAD_CAPACITY as u64);
            copy_current_payload(request, request.arg1, prefix_len)?;
            if current_inet_socket(socket) {
                // Internet sockets carry no kernel-side stream bookkeeping:
                // netd owns the whole byte stream.
                return Ok(());
            }
            let socket = current_unix_socket_handle(socket).ok_or(LINUX_ENOTSOCK)?;
            pending_transfers.begin_send(&socket, request.payload_len as usize)
        }
        SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT => {
            copy_current_payload(request, request.arg3, request.arg4)
        }
        SYSCALL_OFFLOAD_OP_LINUX_SENDMSG => {
            let header = usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(request.arg1)
                .map_err(address_space_error_to_linux_errno)?;
            let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
            let total = iovec_payload_total_len(&iovecs)?;
            let seqpacket = current_unix_socket_handle(socket)
                .is_some_and(|handle| handle.type_() == rustos_user_abi::syscall::NET_BROKER_PUBLICATION_SOCK_SEQPACKET);
            if total > NETD_IPC_PAYLOAD_CAPACITY && (seqpacket || header.msg_controllen != 0) {
                return Err(LINUX_EMSGSIZE);
            }
            let mut bytes = read_iovec_prefix(&iovecs, total.min(NETD_IPC_PAYLOAD_CAPACITY))?;
            let control = if current_inet_socket(socket) {
                // Ancillary data on an internet socket (IP_PKTINFO, ...) has no
                // transport here; refuse it instead of dropping it silently.
                if header.msg_controllen != 0 {
                    return Err(LINUX_EOPNOTSUPP);
                }
                EncodedControlPayload::default()
            } else {
                let socket = current_unix_socket_handle(socket).ok_or(LINUX_ENOTSOCK)?;
                pending_transfers.begin_send(&socket, bytes.len())?;
                let raw_control =
                    read_current_control_bytes(header.msg_control, header.msg_controllen)?;
                let context = send_transfer_context(&socket, pending_transfers, bytes.len())?;
                let control = encode_transfer_control_payload(raw_control.as_slice(), context)?;
                pending_transfers.extend(control.descriptors.as_slice());
                pending_transfers.extend_tickets(control.tickets.as_slice());
                control
            };
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
                if !control.bytes.is_empty() || seqpacket {
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
            request.payload_len = total.min(NETD_IPC_PAYLOAD_CAPACITY) as u32;
            request.arg4 = header.msg_controllen;
            if current_inet_socket(socket) {
                return Ok(());
            }
            let socket = current_unix_socket_handle(socket).ok_or(LINUX_ENOTSOCK)?;
            pending_transfers.begin_receive(&socket)?;
            Ok(())
        }
        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => {
            if current_inet_socket(socket) {
                return Ok(());
            }
            let socket = current_unix_socket_handle(socket).ok_or(LINUX_ENOTSOCK)?;
            pending_transfers.begin_receive(&socket)
        }
        _ => Ok(()),
    }
}

fn current_inet_socket(socket: Option<&SocketSnapshot>) -> bool {
    socket.is_some_and(|s| matches!(&s.handle, multitask::KernelHandle::InetSocket(_)))
}

fn current_unix_socket_handle(socket: Option<&SocketSnapshot>) -> Option<multitask::SocketHandle> {
    match &socket?.handle {
        multitask::KernelHandle::Socket(socket) => Some(socket.clone()),
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
    let width = if pending.seqpacket { len.max(1) } else { len };
    if stream.end() != stream.start().checked_add(width).ok_or(LINUX_EOVERFLOW)? {
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
    socket: Option<&SocketSnapshot>,
) -> Result<(), i64> {
    let payload_len = response.payload_len as usize;
    if payload_len > NETD_IPC_PAYLOAD_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    if socket.is_some_and(|s| s.is_ipv6_datagram() || s.is_unix_dgram())
        && matches!(
            request.op,
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
        )
    {
        return consume_netd_msg_dgram_receive(request, response, receive_context);
    }
    if socket.is_some_and(SocketSnapshot::is_datagram)
        && matches!(request.op, SYSCALL_OFFLOAD_OP_LINUX_RECVFROM | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG)
    {
        return inet_datagram::consume_receive(request, response);
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
            if socket.is_some_and(SocketSnapshot::is_netlink) && request.arg4 != 0 {
                write_current_sockaddr_payload(request.arg4, request.arg5, &netlink_kernel_address())?;
            } else if request.arg4 != 0 && request.arg5 != 0 {
                // No peer address is reported for this socket kind: say so
                // (`addrlen = 0`, as Linux does for connected stream sockets)
                // instead of leaving the caller's stale length in place. The
                // bytes were already dequeued, so a bad pointer is not an error.
                let _ = usermem::write_current_user_struct(request.arg5, &0_u32);
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
            // `MSG_TRUNC` in the receive flags makes the return value the real
            // message length, which exceeds what was copied.
            let value_matches = if msg_flags & linux_abi::MSG_TRUNC as u32 != 0 {
                response.value as usize >= data_len
            } else {
                response.value as usize == data_len
            };
            if end > payload_len || !value_matches {
                return Err(LINUX_EINVAL);
            }
            let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
            write_current_iovec_payload(&iovecs, &response.payload[data_start..control_start])?;
            if socket.is_some_and(SocketSnapshot::is_netlink) && header.msg_name != 0 {
                let address = netlink_kernel_address();
                let len = (header.msg_namelen as usize).min(address.len());
                if len != 0 {
                    usermem::write_current_user_bytes(header.msg_name, &address[..len])
                        .map_err(address_space_error_to_linux_errno)?;
                }
                header.msg_namelen = address.len() as u32;
            } else {
                header.msg_namelen = 0;
            }
            let (control_written, control_flags, prepared) = match receive_context {
                Some(receive_context) => write_current_control_payload(
                    header.msg_control,
                    header.msg_controllen,
                    &response.payload[control_start..end],
                    receive_context,
                    request.arg2 & linux_abi::MSG_CMSG_CLOEXEC != 0,
                )?,
                // Internet sockets have no receive stream context and never
                // carry ancillary data back.
                None if control_len == 0 => (0, 0, None),
                None => return Err(LINUX_ESTALE),
            };
            header.msg_controllen = control_written as u64;
            header.msg_flags = msg_flags | control_flags;
            usermem::write_current_user_struct(request.arg1, &header)
                .map_err(address_space_error_to_linux_errno)?;
            if let Some(prepared) = prepared {
                prepared.commit()?;
            }
            Ok(())
        }
        // Prepared socket publication copied accept/socketpair outputs before
        // committing the invisible descriptor reservations. Never copy twice.
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR => Ok(()),
        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME
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

fn read_optional_user_sockaddr(ptr: u64, len: u64) -> Result<Vec<u8>, i64> {
    if ptr == 0 || len == 0 {
        return Ok(Vec::new());
    }
    let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
    if len > rustos_user_abi::syscall::netd_message::MAX_UNIX_ADDRESS_SIZE {
        return Err(LINUX_EINVAL);
    }
    let mut addr = alloc::vec![0_u8; len];
    usermem::copy_from_current_user_exact(ptr, &mut addr)
        .map_err(address_space_error_to_linux_errno)?;
    Ok(addr)
}

fn control_has_scm_rights(control: &[u8]) -> Result<bool, i64> {
    let mut offset = 0usize;
    while offset + core::mem::size_of::<linux_abi::LinuxCmsghdr>() <= control.len() {
        let header = read_control_header(&control[offset..])?;
        let cmsg_len = usize::try_from(header.cmsg_len).map_err(|_| LINUX_EINVAL)?;
        if cmsg_len < core::mem::size_of::<linux_abi::LinuxCmsghdr>()
            || offset.checked_add(cmsg_len).is_none_or(|end| end > control.len())
        {
            return Err(LINUX_EINVAL);
        }
        if header.cmsg_level == linux_abi::SOL_SOCKET as u32
            && header.cmsg_type == linux_abi::SCM_RIGHTS as u32
        {
            return Ok(true);
        }
        offset = next_control_offset(control.len(), offset, cmsg_len)?;
    }
    if offset != control.len() {
        return Err(LINUX_EINVAL);
    }
    Ok(false)
}

fn populate_netd_msg_dgram_send(
    request: &mut NetdIpcRequest,
    pending_transfers: &mut PendingNetdTransfers,
    socket: Option<&SocketSnapshot>,
) -> Result<(), i64> {
    use rustos_user_abi::syscall::netd_message;

    let (bytes, control, address) = if request.op == SYSCALL_OFFLOAD_OP_LINUX_SENDTO {
        let addr = read_optional_user_sockaddr(request.arg4, request.arg5)?;
        let total = usize::try_from(request.arg2).map_err(|_| LINUX_EINVAL)?;
        let cap = NETD_IPC_PAYLOAD_CAPACITY
            .checked_sub(netd_message::HEADER_SIZE)
            .and_then(|r| r.checked_sub(addr.len()))
            .ok_or(LINUX_EMSGSIZE)?;
        if total > cap {
            return Err(LINUX_EMSGSIZE);
        }
        let mut bytes = alloc::vec![0_u8; total];
        if total != 0 {
            usermem::copy_from_current_user_exact(request.arg1, &mut bytes)
                .map_err(address_space_error_to_linux_errno)?;
        }
        if let Some(unix_sock) = current_unix_socket_handle(socket) {
            pending_transfers.begin_send(&unix_sock, bytes.len())?;
        }
        (bytes, Vec::new(), addr)
    } else {
        let header = usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(request.arg1)
            .map_err(address_space_error_to_linux_errno)?;
        let addr = read_optional_user_sockaddr(header.msg_name, u64::from(header.msg_namelen))?;
        let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
        let total = iovec_payload_total_len(&iovecs)?;
        if total > NETD_IPC_PAYLOAD_CAPACITY {
            return Err(LINUX_EMSGSIZE);
        }
        let bytes = read_iovec_prefix(&iovecs, total)?;
        let control = if current_inet_socket(socket) {
            if header.msg_controllen != 0 {
                return Err(LINUX_EOPNOTSUPP);
            }
            Vec::new()
        } else {
            let unix_sock = current_unix_socket_handle(socket).ok_or(LINUX_ENOTSOCK)?;
            if header.msg_controllen == 0 {
                pending_transfers.begin_send(&unix_sock, bytes.len())?;
                Vec::new()
            } else if unix_sock.channel_id() == 0 {
                let raw_control =
                    read_current_control_bytes(header.msg_control, header.msg_controllen)?;
                if control_has_scm_rights(&raw_control)? {
                    return Err(LINUX_EOPNOTSUPP);
                }
                raw_control
            } else {
                pending_transfers.begin_send(&unix_sock, bytes.len())?;
                let raw_control =
                    read_current_control_bytes(header.msg_control, header.msg_controllen)?;
                let context = send_transfer_context(&unix_sock, pending_transfers, bytes.len())?;
                let encoded = encode_transfer_control_payload(&raw_control, context)?;
                pending_transfers.extend(encoded.descriptors.as_slice());
                pending_transfers.extend_tickets(encoded.tickets.as_slice());
                encoded.bytes
            }
        };
        request.arg4 = header.msg_controllen;
        (bytes, control, addr)
    };
    let len = netd_message::encode(&mut request.payload, &bytes, &control, 0, &address)
        .ok_or(LINUX_EMSGSIZE)?;
    request.payload_len = len as u32;
    Ok(())
}

fn consume_netd_msg_dgram_receive(
    request: &NetdIpcRequest,
    response: &NetdIpcResponse,
    receive_context: Option<(u64, u64, u8, u64)>,
) -> Result<(), i64> {
    let payload_len = response.payload_len as usize;
    let message = rustos_user_abi::syscall::netd_message::decode(&response.payload[..payload_len])
        .ok_or(LINUX_EINVAL)?;
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_RECVFROM {
        let max_len = usize::try_from(request.arg2).map_err(|_| LINUX_EINVAL)?;
        if message.data.len() > max_len {
            return Err(LINUX_EINVAL);
        }
        if !message.data.is_empty() {
            usermem::write_current_user_bytes(request.arg1, message.data)
                .map_err(address_space_error_to_linux_errno)?;
        }
        if request.arg4 != 0 && request.arg5 != 0 {
            if message.address.is_empty() {
                let _ = usermem::write_current_user_struct(request.arg5, &0_u32);
            } else {
                write_current_sockaddr_payload(request.arg4, request.arg5, message.address)?;
            }
        }
        return Ok(());
    }
    let mut header = usermem::read_current_user_struct::<linux_abi::LinuxMsghdr>(request.arg1)
        .map_err(address_space_error_to_linux_errno)?;
    let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
    write_current_iovec_payload(&iovecs, message.data)?;
    if header.msg_name != 0 {
        let copy_len = (header.msg_namelen as usize).min(message.address.len());
        if copy_len != 0 {
            usermem::write_current_user_bytes(header.msg_name, &message.address[..copy_len])
                .map_err(address_space_error_to_linux_errno)?;
        }
        header.msg_namelen = message.address.len() as u32;
    } else {
        header.msg_namelen = 0;
    }
    let (control_written, control_flags, prepared) = if message.control.is_empty() {
        (0, 0, None)
    } else if let Some(ctx) = receive_context {
        write_current_control_payload(
            header.msg_control,
            header.msg_controllen,
            message.control,
            ctx,
            request.arg2 & linux_abi::MSG_CMSG_CLOEXEC != 0,
        )?
    } else {
        if control_has_scm_rights(message.control)? {
            return Err(LINUX_ESTALE);
        }
        let cap = usize::try_from(header.msg_controllen).map_err(|_| LINUX_EINVAL)?;
        if header.msg_control == 0 || cap < message.control.len() {
            (0, linux_abi::MSG_CTRUNC as u32, None)
        } else {
            usermem::write_current_user_bytes(header.msg_control, message.control)
                .map_err(address_space_error_to_linux_errno)?;
            (message.control.len(), 0, None)
        }
    };
    header.msg_controllen = control_written as u64;
    header.msg_flags = message.flags | control_flags;
    usermem::write_current_user_struct(request.arg1, &header)
        .map_err(address_space_error_to_linux_errno)?;
    if let Some(prepared) = prepared {
        prepared.commit()?;
    }
    Ok(())
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
    if total > NETD_IPC_PAYLOAD_CAPACITY { return Err(LINUX_EMSGSIZE); }
    read_iovec_prefix(&iovecs, total)
}

fn read_iovec_prefix(iovecs: &[linux_abi::LinuxIovec], total: usize) -> Result<Vec<u8>, i64> {
    let mut bytes = Vec::with_capacity(total);
    for iov in iovecs {
        let len = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?.min(total - bytes.len());
        if len == 0 { continue; }
        let start = bytes.len();
        bytes.resize(start + len, 0);
        usermem::copy_from_current_user_exact(iov.iov_base, &mut bytes[start..])
            .map_err(address_space_error_to_linux_errno)?;
    }
    Ok(bytes)
}

pub(super) fn read_current_iovecs_payload(
    iov_ptr: u64,
    iov_len: u64,
) -> Result<Vec<linux_abi::LinuxIovec>, i64> {
    let iov_len = usize::try_from(iov_len).map_err(|_| LINUX_EINVAL)?;
    if iov_len == 0 { return Ok(Vec::new()); }
    if iov_len > NETD_MAX_IOVEC_COUNT {
        return Err(LINUX_EINVAL);
    }
    if iov_ptr == 0 {
        return Err(LINUX_EFAULT);
    }
    let mut iovecs = Vec::with_capacity(iov_len);
    for index in 0..iov_len {
        let offset = index
            .checked_mul(size_of::<linux_abi::LinuxIovec>())
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(LINUX_EINVAL)?;
        let iov = usermem::read_current_user_struct::<linux_abi::LinuxIovec>(
            iov_ptr.checked_add(offset).ok_or(LINUX_EFAULT)?,
        )
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
    cloexec: bool,
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
    let (control, prepared) = decode_transfer_control_payload(control, receive_context, cloexec)?;
    usermem::write_current_user_bytes(control_ptr, control.as_slice())
        .map_err(address_space_error_to_linux_errno)?;
    Ok((control.len(), 0, prepared))
}

#[derive(Default)]
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
    cloexec: bool,
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
    // Received descriptors carry exactly the receiver's `MSG_CMSG_CLOEXEC`
    // choice, never the sender's own close-on-exec state.
    let mut prepared = prepared;
    prepared.set_close_on_exec(cloexec)?;
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
            || offset.checked_add(cmsg_len).is_none_or(|end| end > control.len())
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
    let tickets=decode_transfer_release_tickets(control)?;
    super::super::ipc_ops::drop_transfer_tickets(tickets.as_slice());
    Ok(())
}

pub(in crate::user::syscall::linux) fn decode_transfer_release_tickets(
    control:&[u8],
)->Result<Vec<kernel_ipc_runtime::api::KernelTransferTicket>,i64> {
    let mut tickets = Vec::new();
    let mut offset = 0usize;
    while offset + core::mem::size_of::<linux_abi::LinuxCmsghdr>() <= control.len() {
        let header = read_control_header(&control[offset..])?;
        let cmsg_len = usize::try_from(header.cmsg_len).map_err(|_| LINUX_EINVAL)?;
        if cmsg_len < core::mem::size_of::<linux_abi::LinuxCmsghdr>()
            || offset.checked_add(cmsg_len).is_none_or(|end| end > control.len())
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
    Ok(tickets)
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
            || offset.checked_add(cmsg_len).is_none_or(|end| end > control.len())
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

// Netlink currently produces only unicast replies from the kernel port.
fn netlink_kernel_address() -> [u8; 12] {
    let mut address = [0; 12];
    address[..2].copy_from_slice(&(rustos_user_abi::syscall::NET_BROKER_PUBLICATION_AF_NETLINK as u16).to_ne_bytes());
    address
}

pub(super) fn write_current_sockaddr_payload(addr_ptr: u64, len_ptr: u64, payload: &[u8]) -> Result<(), i64> {
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
        usermem::write_current_user_bytes(addr_ptr, &payload[..capacity.min(payload.len())])
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
    // Linux truncates an int-valued option to the caller's `optlen`
    // (`SO_ERROR` read through a 1-byte buffer); structured options with a
    // short buffer stay `EINVAL`.
    let payload = if capacity >= payload.len() {
        payload
    } else if payload.len() == size_of::<i32>() && capacity != 0 && capacity <= i32::MAX as usize {
        &payload[..capacity]
    } else {
        return Err(LINUX_EINVAL);
    };
    usermem::write_current_user_bytes(optval_ptr, payload)
        .map_err(address_space_error_to_linux_errno)?;
    let len = payload.len() as u32;
    usermem::write_current_user_struct(optlen_ptr, &len).map_err(address_space_error_to_linux_errno)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod netd_receive_shape_tests {
    use super::*;

    #[test]
    fn empty_iovecs_and_large_receive_capacity_are_valid() {
        assert!(read_current_iovecs_payload(0, 0).unwrap().is_empty());
        let iovecs = [linux_abi::LinuxIovec { iov_base: 0, iov_len: (NETD_IPC_PAYLOAD_CAPACITY as u64) * 2 }];
        assert_eq!(iovec_payload_total_len(&iovecs), Ok(NETD_IPC_PAYLOAD_CAPACITY * 2));
    }

    #[test]
    fn netlink_kernel_source_has_sockaddr_nl_shape() {
        let addr = netlink_kernel_address();
        assert_eq!(u16::from_ne_bytes([addr[0], addr[1]]) as u64, rustos_user_abi::syscall::NET_BROKER_PUBLICATION_AF_NETLINK);
        assert_eq!(&addr[2..], &[0; 10]);
    }
}
