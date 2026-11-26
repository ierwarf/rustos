use super::*;
use rustos_user_abi::syscall::{
    VFS_IPC_OP_RENAMEAT2, VFS_IPC_OP_STATFS, VFS_IPC_OP_SYMLINKAT,
};

mod inet_datagram;
pub fn syscall_linux_vfs_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    if let Some(result) = super::local_fd::not_seekable(fd) {
        return result;
    }
    linux_result((|| {
        if let Some(mut memfd) = current_memfd_handle(fd) {
            let whence = match whence {
                value if value == linux_abi::SEEK_SET => multitask::FileHandleSeekWhence::Start,
                value if value == linux_abi::SEEK_CUR => multitask::FileHandleSeekWhence::Current,
                value if value == linux_abi::SEEK_END => multitask::FileHandleSeekWhence::End,
                _ => return Err(LINUX_EINVAL),
            };
            return memfd.seek(offset, whence).map_err(|_| LINUX_EINVAL);
        }
        let remote = current_remote_vfs_handle(fd).ok_or(LINUX_EBADF)?;
        let _cursor = acquire_vfs_cursor_gate(remote.remote_id())?;
        let is_file = remote.kind() == multitask::RemoteVfsHandleKind::File;
        let observed_epoch = if is_file {
            ipc_ops::service_endpoint_epoch(IPC_SERVICE_VFSD).unwrap_or(0)
        } else {
            0
        };
        let mut request = new_vfs_request(VFS_IPC_OP_LSEEK);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        request.arg0 = offset as u64;
        request.arg1 = whence;
        if is_file {
            request.flags |= VFS_LSEEK_FLAG_SYNC_CURSOR;
            request.arg2 = remote.cursor();
        }
        let value = call_vfs_checked(&request)?.value;
        if is_file {
            remote.set_synced_cursor(value, observed_epoch);
        }
        Ok(value)
    })())
}

pub fn kernel_vfs_fstat(fd: u64) -> Result<linux_abi::LinuxStat, i64> {
    if let Some(memfd) = current_memfd_handle(fd) {
        return Ok(super::anon_stat::memfd_stat(&memfd));
    }
    let remote = current_remote_vfs_handle(fd).ok_or(LINUX_EBADF)?;
    let mut request = new_vfs_request(VFS_IPC_OP_FSTAT);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    let response = call_vfs_checked(&request)?;
    decode_exact_payload::<linux_abi::LinuxStat>(&response.payload, response.payload_len)
}

pub fn kernel_vfs_statx_path(path: &str) -> Result<linux_abi::LinuxStatx, i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_STATX);
    proc_identity::attach_caller_exec_path(&mut request);
    request.arg1 = 0x7ff;
    populate_vfs_path_base(&mut request, linux_abi::AT_FDCWD as u64, path)?;
    let response = call_vfs_checked(&request)?;
    decode_exact_payload::<linux_abi::LinuxStatx>(&response.payload, response.payload_len)
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
    linux_unit_result((|| {
        let remote = current_remote_vfs_handle(fd).ok_or(LINUX_EBADF)?;
        validate_user_write_as(stat_ptr, LINUX_STAT_SIZE)?;
        let mut request = new_vfs_request(VFS_IPC_OP_FSTAT);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        let response = call_vfs_checked(&request)?;
        copy_exact_payload_to_user_as(
            stat_ptr,
            &response.payload,
            response.payload_len,
            LINUX_STAT_SIZE,
        )
    })())
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
    linux_unit_result((|| {
        // Linux rejects a negative `off_t` before it looks at the descriptor.
        if len > i64::MAX as u64 {
            return Err(LINUX_EINVAL);
        }
        if let Some((memfd, rights)) = current_memfd_handle_with_rights(fd) {
            if !rights.allows_write() {
                return Err(ftruncate_unwritable_errno(rights));
            }
            let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
            return memfd.truncate(len).map_err(memfd_error_to_linux_errno);
        }
        let (remote, rights) =
            current_remote_vfs_handle_with_rights(fd).ok_or(LINUX_EBADF)?;
        if !rights.allows_write() {
            return Err(ftruncate_unwritable_errno(rights));
        }
        call_vfs_fd_status(VFS_IPC_OP_FTRUNCATE, fd, remote.remote_id(), |request| {
            request.arg0 = len;
        })
    })())
}

fn getdents_pinned(
    fd: u64,
    remote_id: u64,
    requested_len: usize,
    copy_out: impl FnOnce(&[u8]) -> Result<(), i64>,
) -> Result<usize, i64> {
    let _pin = super::vfs_socket::RemoteVfsRefGuard::acquire(fd, remote_id)?;
    let _cursor = acquire_vfs_cursor_gate(remote_id)?;
    let mut request = new_vfs_request(VFS_IPC_OP_GETDENTS64);
    request.fd = fd;
    request.remote_id = remote_id;
    request.arg1 = requested_len as u64;
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => {
            let _ = settle_vfs_cursor_mutation(&request, false);
            return Err(errno);
        }
    };
    vfs_getdents_status(&request, &response)?;
    let len = response.payload_len as usize;
    if len > requested_len || len > response.payload.len() {
        let _ = settle_vfs_cursor_mutation(&request, false);
        return Err(LINUX_EINVAL);
    }
    if let Err(errno) = copy_out(&response.payload[..len]) {
        let _ = settle_vfs_cursor_mutation(&request, false);
        return Err(errno);
    }
    match settle_vfs_cursor_mutation(&request, true) {
        Ok(()) => Ok(len),
        Err(errno) => {
            // Bytes are already visible; reconciliation retains the
            // prepared cursor until the exact COMMIT settles.
            if len > 0 {
                Ok(len)
            } else {
                Err(errno)
            }
        }
    }
}

pub(crate) fn kernel_vfs_getdents_raw(fd: u64, dest: &mut [u8]) -> Result<usize, i64> {
    let remote = current_remote_vfs_handle(fd).ok_or(LINUX_EBADF)?;
    getdents_pinned(fd, remote.remote_id(), dest.len(), |bytes| {
        dest[..bytes.len()].copy_from_slice(bytes);
        Ok(())
    })
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
    linux_result((|| {
        let (remote, rights) =
            current_remote_vfs_handle_with_rights(fd).ok_or(LINUX_EBADF)?;
        if !rights.allows_read() {
            return Err(LINUX_EBADF);
        }
        let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
        validate_user_write_as(user_ptr, user_len)?;
        let len = getdents_pinned(fd, remote.remote_id(), user_len, |bytes| {
            write_user_bytes_as(user_ptr, bytes)
        })?;
        Ok(len as u64)
    })())
}

pub fn syscall_linux_vfs_statx(
    dirfd: u64,
    path_ptr: u64,
    flags: u64,
    mask: u64,
    statx_ptr: u64,
) -> u64 {
    linux_result((|| {
        let path = copy_current_user_path_allow_empty(path_ptr, VFS_IPC_PATH_CAPACITY)?;
        Ok(syscall_linux_vfs_statx_str(dirfd, &path, flags, mask, statx_ptr))
    })())
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
    if let Some(result) = super::local_fd::statx_at_empty_path(dirfd, path, flags, statx_ptr) {
        return result;
    }
    linux_unit_result((|| {
        if path.is_empty() && flags & linux_abi::AT_EMPTY_PATH == 0 {
            return Err(LINUX_ENOENT);
        }
        validate_user_write_as(statx_ptr, LINUX_STATX_SIZE)?;
        if path.is_empty() && !is_linux_at_fdcwd_for_vfs(dirfd) {
            return match current_kernel_handle(dirfd) {
                Some(multitask::KernelHandle::Console(console)) => {
                    let stat = bootstrap_linux_stat(console.token_id(), 0);
                    write_user_bytes_as(statx_ptr, &encode_linux_statx_from_stat(&stat))
                }
                Some(multitask::KernelHandle::DeviceBuffer(buffer)) => {
                    let stat = device_buffer_linux_stat(&buffer);
                    write_user_bytes_as(statx_ptr, &encode_linux_statx_from_stat(&stat))
                }
                Some(multitask::KernelHandle::RemoteVfs(remote)) => {
                    let mut request = new_vfs_request(VFS_IPC_OP_FSTAT);
                    request.fd = dirfd;
                    request.remote_id = remote.remote_id();
                    let response = call_vfs_checked(&request)?;
                    let stat = decode_exact_payload::<linux_abi::LinuxStat>(
                        &response.payload,
                        response.payload_len,
                    )?;
                    write_user_bytes_as(statx_ptr, &encode_linux_statx_from_stat(&stat))
                }
                _ => Err(LINUX_EBADF),
            };
        }
        let effective_path = if path.is_empty() { "." } else { path };
        let mut request = new_vfs_request(VFS_IPC_OP_STATX);
        proc_identity::attach_caller_exec_path(&mut request);
        request.flags = flags as u32;
        request.arg1 = mask;
        populate_vfs_path_base(&mut request, dirfd, effective_path)?;
        let response = call_vfs_checked(&request)?;
        if let Some(result) = proc_identity::complete_stat(
            response.aux,
            flags,
            proc_identity::StatOut::Statx { ptr: statx_ptr, mask },
        ) {
            return super::vfs_socket::kernel_vfs_result(result).map(|_| ());
        }
        copy_exact_payload_to_user_as(
            statx_ptr,
            &response.payload,
            response.payload_len,
            LINUX_STATX_SIZE,
        )
    })())
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
        && !is_linux_at_fdcwd_for_vfs(dirfd)
        && matches!(
            current_kernel_handle(dirfd),
            Some(
                multitask::KernelHandle::Console(_)
                    | multitask::KernelHandle::DeviceBuffer(_)
                    | multitask::KernelHandle::RemoteVfs(_)
                    | multitask::KernelHandle::VfsDirectory(_)
            )
        )
    {
        return syscall_linux_vfs_fstat(dirfd, stat_ptr);
    }
    let effective_path = if path.is_empty() { "." } else { &path };
    linux_unit_result((|| {
        validate_user_write_as(stat_ptr, LINUX_STAT_SIZE)?;
        let mut request = new_vfs_request(VFS_IPC_OP_NEWFSTATAT);
        proc_identity::attach_caller_exec_path(&mut request);
        request.flags = flags as u32;
        populate_vfs_path_base(&mut request, dirfd, effective_path)?;
        let response = call_vfs_checked(&request)?;
        if let Some(result) = proc_identity::complete_stat(
            response.aux,
            flags,
            proc_identity::StatOut::Stat { ptr: stat_ptr },
        ) {
            return super::vfs_socket::kernel_vfs_result(result).map(|_| ());
        }
        copy_exact_payload_to_user_as(
            stat_ptr,
            &response.payload,
            response.payload_len,
            LINUX_STAT_SIZE,
        )
    })())
}

pub fn syscall_linux_vfs_readlinkat(
    dirfd: u64,
    path_ptr: u64,
    user_ptr: u64,
    user_len: u64,
) -> u64 {
    linux_result((|| {
        let path = copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY)?;
        // `bufsiz` is a C `int` (sign-extended by `int_args`): zero or negative is
        // EINVAL, not an empty success and not a huge buffer that faults.
        if (user_len as i64) <= 0 {
            return Err(LINUX_EINVAL);
        }
        let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
        validate_user_write_as(user_ptr, user_len)?;
        let mut request = new_vfs_request(VFS_IPC_OP_READLINKAT);
        proc_identity::attach_caller_exec_path(&mut request);
        request.arg0 = user_len as u64;
        populate_vfs_path_base(&mut request, dirfd, &path)?;
        let response = call_vfs_checked(&request)?;
        if let Some(resolved) = proc_identity::complete_readlink(response.aux) {
            let target = resolved?;
            let bytes = target.as_bytes();
            let len = bytes.len().min(user_len);
            write_user_bytes_as(user_ptr, &bytes[..len])?;
            return Ok(len as u64);
        }
        let payload = response
            .payload
            .get(..response.payload_len as usize)
            .ok_or(LINUX_EINVAL)?;
        let len = payload.len().min(user_len);
        write_user_bytes_as(user_ptr, &payload[..len])?;
        Ok(len as u64)
    })())
}

pub fn syscall_linux_vfs_access(dirfd: u64, path_ptr: u64, mode: u64, flags: u64) -> u64 {
    linux_result((|| {
        const ACCESS_ALLOWED_FLAGS: u64 =
            linux_abi::AT_EACCESS | linux_abi::AT_SYMLINK_NOFOLLOW | linux_abi::AT_EMPTY_PATH;
        let path = copy_current_user_path_allow_empty(path_ptr, VFS_IPC_PATH_CAPACITY)?;
        if path.is_empty() {
            if flags & linux_abi::AT_EMPTY_PATH == 0 {
                return Err(LINUX_ENOENT);
            }
            if flags & !ACCESS_ALLOWED_FLAGS != 0 || mode & !7 != 0 {
                return Err(LINUX_EINVAL);
            }
            if let Some(owner_bits) = super::anon_stat::current_owner_mode_bits(dirfd) {
                if (mode & u64::from(owner_bits)) != mode {
                    return Err(LINUX_EACCES);
                }
                return Ok(0);
            }
        }
        let mut request = new_vfs_request(VFS_IPC_OP_ACCESS);
        request.flags = flags as u32;
        request.arg0 = mode;
        populate_vfs_path_base(&mut request, dirfd, &path)?;
        let response = call_vfs_checked(&request)?;
        if let Some(result) = proc_identity::complete_access(response.aux, mode) {
            return super::vfs_socket::kernel_vfs_result(result);
        }
        Ok(0)
    })())
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
fn write_stat_bytes_checked(user_ptr: u64, out: &[u8; LINUX_STAT_SIZE]) -> u64 {
    linux_unit_result((|| {
        validate_user_write_as(user_ptr, LINUX_STAT_SIZE)?;
        write_user_bytes_as(user_ptr, out)
    })())
}

fn linux_dev_major(dev: u64) -> u32 {
    (((dev >> 8) & 0x0fff) | ((dev >> 32) & 0xffff_f000)) as u32
}

fn linux_dev_minor(dev: u64) -> u32 {
    ((dev & 0x00ff) | ((dev >> 12) & 0xffff_ff00)) as u32
}

fn encode_linux_stat_bytes(stat: &linux_abi::LinuxStat) -> [u8; LINUX_STAT_SIZE] {
    let mut out = [0_u8; LINUX_STAT_SIZE];
    out[0..8].copy_from_slice(&stat.st_dev.to_ne_bytes());
    out[8..16].copy_from_slice(&stat.st_ino.to_ne_bytes());
    out[16..24].copy_from_slice(&stat.st_nlink.to_ne_bytes());
    out[24..28].copy_from_slice(&stat.st_mode.to_ne_bytes());
    out[28..32].copy_from_slice(&stat.st_uid.to_ne_bytes());
    out[32..36].copy_from_slice(&stat.st_gid.to_ne_bytes());
    out[40..48].copy_from_slice(&stat.st_rdev.to_ne_bytes());
    out[48..56].copy_from_slice(&stat.st_size.to_ne_bytes());
    out[56..64].copy_from_slice(&stat.st_blksize.to_ne_bytes());
    out[64..72].copy_from_slice(&stat.st_blocks.to_ne_bytes());
    out[72..80].copy_from_slice(&stat.st_atim.tv_sec.to_ne_bytes());
    out[80..88].copy_from_slice(&stat.st_atim.tv_nsec.to_ne_bytes());
    out[88..96].copy_from_slice(&stat.st_mtim.tv_sec.to_ne_bytes());
    out[96..104].copy_from_slice(&stat.st_mtim.tv_nsec.to_ne_bytes());
    out[104..112].copy_from_slice(&stat.st_ctim.tv_sec.to_ne_bytes());
    out[112..120].copy_from_slice(&stat.st_ctim.tv_nsec.to_ne_bytes());
    out
}

fn encode_linux_statx_from_stat(stat: &linux_abi::LinuxStat) -> [u8; LINUX_STATX_SIZE] {
    const STATX_BASIC_STATS: u32 = 0x7ff;
    let mut out = [0_u8; LINUX_STATX_SIZE];
    out[0..4].copy_from_slice(&STATX_BASIC_STATS.to_ne_bytes());
    let blksize = u32::try_from(stat.st_blksize.max(0)).unwrap_or(4096);
    out[4..8].copy_from_slice(&blksize.to_ne_bytes());
    let nlink = u32::try_from(stat.st_nlink).unwrap_or(u32::MAX);
    out[16..20].copy_from_slice(&nlink.to_ne_bytes());
    out[20..24].copy_from_slice(&stat.st_uid.to_ne_bytes());
    out[24..28].copy_from_slice(&stat.st_gid.to_ne_bytes());
    out[28..30].copy_from_slice(&(stat.st_mode as u16).to_ne_bytes());
    out[32..40].copy_from_slice(&stat.st_ino.to_ne_bytes());
    out[40..48].copy_from_slice(&(stat.st_size.max(0) as u64).to_ne_bytes());
    out[48..56].copy_from_slice(&(stat.st_blocks.max(0) as u64).to_ne_bytes());
    out[64..72].copy_from_slice(&stat.st_atim.tv_sec.to_ne_bytes());
    out[72..76].copy_from_slice(&(stat.st_atim.tv_nsec as u32).to_ne_bytes());
    out[96..104].copy_from_slice(&stat.st_ctim.tv_sec.to_ne_bytes());
    out[104..108].copy_from_slice(&(stat.st_ctim.tv_nsec as u32).to_ne_bytes());
    out[112..120].copy_from_slice(&stat.st_mtim.tv_sec.to_ne_bytes());
    out[120..124].copy_from_slice(&(stat.st_mtim.tv_nsec as u32).to_ne_bytes());
    out[128..132].copy_from_slice(&linux_dev_major(stat.st_rdev).to_ne_bytes());
    out[132..136].copy_from_slice(&linux_dev_minor(stat.st_rdev).to_ne_bytes());
    out[136..140].copy_from_slice(&linux_dev_major(stat.st_dev).to_ne_bytes());
    out[140..144].copy_from_slice(&linux_dev_minor(stat.st_dev).to_ne_bytes());
    out[144..152].copy_from_slice(&stat.st_dev.to_ne_bytes());
    out
}

fn device_buffer_linux_stat(
    buffer: &kernel_ps::api::handles::BufferCapability,
) -> linux_abi::LinuxStat {
    let id = buffer.identity();
    linux_abi::LinuxStat {
        st_dev: id.owner.generation,
        st_ino: id.allocation,
        st_nlink: 1,
        st_mode: 0o020000 | 0o600,
        st_rdev: rustos_user_abi::syscall::APP_BUFFER_STAT_RDEV,
        st_size: buffer.len() as i64,
        st_blksize: 4096,
        ..linux_abi::LinuxStat::default()
    }
}

fn bootstrap_linux_stat(inode: u64, len: u64) -> linux_abi::LinuxStat {
    linux_abi::LinuxStat {
        st_ino: inode.max(1),
        st_nlink: 1,
        st_mode: 0o100000 | 0o555,
        st_size: len as i64,
        st_blksize: 4096,
        st_blocks: len.div_ceil(512) as i64,
        ..linux_abi::LinuxStat::default()
    }
}

fn write_device_buffer_stat(user_ptr: u64, buffer: &kernel_ps::api::handles::BufferCapability) -> u64 {
    let stat = device_buffer_linux_stat(buffer);
    write_stat_bytes_checked(user_ptr, &encode_linux_stat_bytes(&stat))
}

fn write_bootstrap_stat(user_ptr: u64, inode: u64, len: u64) -> u64 {
    let stat = bootstrap_linux_stat(inode, len);
    write_stat_bytes_checked(user_ptr, &encode_linux_stat_bytes(&stat))
}

// RING3-MIGRATION-REFERENCE END: kernel-owned handle stat substrate.

pub fn syscall_linux_vfs_getcwd(user_ptr: u64, user_len: u64) -> u64 {
    linux_result((|| {
        let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
        if user_len == 0 {
            return Err(LINUX_EINVAL);
        }
        validate_user_write_as(user_ptr, user_len)?;
        let response = call_vfs_checked(&new_vfs_request(VFS_IPC_OP_GETCWD))?;
        let len = response.payload_len as usize;
        let cwd = response.payload.get(..len).ok_or(LINUX_EINVAL)?;
        if len.checked_add(1).is_none_or(|needed| needed > user_len) {
            return Err(LINUX_ERANGE);
        }
        write_user_bytes_as(user_ptr, cwd)?;
        write_user_bytes_as(user_ptr + len as u64, &[0])?;
        Ok((len + 1) as u64)
    })())
}

pub fn syscall_linux_vfs_chdir(dirfd: u64, path_ptr: u64) -> u64 {
    linux_unit_result(call_vfs_path_status(VFS_IPC_OP_CHDIR, dirfd, path_ptr, |_| {}))
}

pub fn syscall_linux_vfs_mkdir(dirfd: u64, path_ptr: u64, mode: u64) -> u64 {
    linux_unit_result(call_vfs_path_status(VFS_IPC_OP_MKDIR, dirfd, path_ptr, |request| {
        request.arg0 = mode;
    }))
}

pub fn syscall_linux_vfs_unlinkat(dirfd: u64, path_ptr: u64, flags: u64) -> u64 {
    linux_unit_result(call_vfs_path_status(VFS_IPC_OP_UNLINKAT, dirfd, path_ptr, |request| {
        request.flags = flags as u32;
    }))
}

pub(crate) fn kernel_vfs_unlink_path(path: &str, flags: u64) -> Result<(), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_UNLINKAT);
    request.flags = flags as u32;
    populate_vfs_path_base(&mut request, linux_abi::AT_FDCWD as u64, path)?;
    call_vfs_status(&request)
}

pub fn syscall_linux_vfs_truncate(path_ptr: u64, len: u64) -> u64 {
    linux_unit_result((|| {
        if len > i64::MAX as u64 {
            return Err(LINUX_EINVAL);
        }
        let path = copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY)?;
        let mut open_req = new_vfs_request(VFS_IPC_OP_OPENAT);
        open_req.arg0 = linux_abi::O_WRONLY as u64;
        open_req.arg1 = 0;
        open_req.arg3 = mint_service_object_id();
        populate_vfs_path_base(&mut open_req, linux_abi::AT_FDCWD as u64, &path)?;
        let open_resp = match call_vfs_ipc_request(&open_req).and_then(|resp| {
            ensure_vfs_status(&resp)?;
            Ok(resp)
        }) {
            Ok(resp) => resp,
            Err(errno) => {
                let _ = call_vfs_remote_close_bounded(open_req.arg3);
                return Err(errno);
            }
        };
        let remote_id = open_resp.remote_id;
        if remote_id == 0 || remote_id != open_req.arg3 {
            let _ = call_vfs_remote_close_bounded(open_req.arg3);
            return Err(LINUX_EINVAL);
        }
        let mut trunc_req = new_vfs_request(VFS_IPC_OP_FTRUNCATE);
        trunc_req.remote_id = remote_id;
        trunc_req.arg0 = len;
        let res = call_vfs_ipc_request(&trunc_req).and_then(|resp| ensure_vfs_status(&resp));
        let _ = call_vfs_remote_close_bounded(remote_id);
        res
    })())
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
    linux_unit_result((|| {
        let old_path = copy_current_user_path(oldpath_ptr, VFS_IPC_PATH_CAPACITY)?;
        let new_path = copy_current_user_path(newpath_ptr, VFS_IPC_PATH_CAPACITY)?;
        kernel_vfs_renameat2(olddirfd, &old_path, newdirfd, &new_path, flags)
    })())
}

pub fn syscall_linux_vfs_symlinkat(
    target_ptr: u64,
    newdirfd: u64,
    linkpath_ptr: u64,
) -> u64 {
    linux_unit_result((|| {
        let target = copy_current_user_path(target_ptr, VFS_IPC_PATH_CAPACITY)?;
        let linkpath = copy_current_user_path(linkpath_ptr, VFS_IPC_PATH_CAPACITY)?;
        let mut request = new_vfs_request(VFS_IPC_OP_SYMLINKAT);
        request.dirfd = newdirfd;
        request.remote_id = resolve_vfs_dirfd_remote_id(newdirfd, &linkpath)?;
        let encoded_len = rustos_user_abi::syscall::encode_two_path_payload(
            target.as_bytes(),
            linkpath.as_bytes(),
            &mut request.payload,
        )
        .ok_or(36_i64 /* ENAMETOOLONG */)?;
        request.payload_len = encoded_len as u32;
        call_vfs_status(&request)
    })())
}

fn decode_vfs_statfs_response(response: &VfsIpcResponse) -> Result<rustos_user_abi::syscall::VfsStatFsWire, i64> {
    let bytes = response
        .payload
        .get(..response.payload_len as usize)
        .ok_or(LINUX_EINVAL)?;
    rustos_user_abi::syscall::VfsStatFsWire::from_bytes(bytes).ok_or(LINUX_EINVAL)
}

pub(crate) fn kernel_vfs_statfs_path(
    path: &str,
) -> Result<rustos_user_abi::syscall::VfsStatFsWire, i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_STATFS);
    request.arg0 = 0;
    populate_vfs_path_base(&mut request, linux_abi::AT_FDCWD as u64, path)?;
    decode_vfs_statfs_response(&*call_vfs_checked(&request)?)
}

pub(crate) fn kernel_vfs_fstatfs(
    fd: u64,
) -> Result<rustos_user_abi::syscall::VfsStatFsWire, i64> {
    if let Some(remote) = current_remote_vfs_handle(fd) {
        let mut request = new_vfs_request(VFS_IPC_OP_STATFS);
        request.arg0 = 1;
        request.fd = fd;
        request.remote_id = remote.remote_id();
        return decode_vfs_statfs_response(&*call_vfs_checked(&request)?);
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
    linux_unit_result((|| {
        let path = copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY)?;
        let wire = kernel_vfs_statfs_path(&path)?;
        write_user_bytes_as(buf_ptr, &wire.to_bytes())
    })())
}

pub fn syscall_linux_vfs_fstatfs(fd: u64, buf_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let wire = kernel_vfs_fstatfs(fd)?;
        write_user_bytes_as(buf_ptr, &wire.to_bytes())
    })())
}

pub fn syscall_linux_vfs_mount(
    _source_ptr: u64,
    target_ptr: u64,
    _fstype_ptr: u64,
    flags: u64,
) -> u64 {
    linux_unit_result((|| {
        let path = copy_current_user_path(target_ptr, VFS_IPC_PATH_CAPACITY)?;
        let mut request = new_vfs_request(VFS_IPC_OP_MOUNT);
        request.flags = flags as u32;
        populate_vfs_path(&mut request, &path)?;
        call_vfs_status(&request)
    })())
}

/// Copy the caller's wait request in, hold it in the console broker until the
/// graph moves past the generation it presented, and copy the answer back.
fn console_wait_graph(arg: u64) -> Result<u64, i64> {
    let mut request =
        read_user_struct_as::<rustos_user_abi::console::ConsoleWaitGraphRequest>(arg)?;
    if request.reserved != 0 {
        return Err(LINUX_EINVAL);
    }
    request.generation = super::ipc_helpers::console_graph_readiness_via_sessiond(
        request.generation,
        u64::from(request.wait_ms),
    )?;
    write_user_struct_as(arg, &request)?;
    Ok(0)
}

pub fn syscall_linux_vfs_umount2(target_ptr: u64, flags: u64) -> u64 {
    linux_unit_result((|| {
        let path = copy_current_user_path(target_ptr, VFS_IPC_PATH_CAPACITY)?;
        let mut request = new_vfs_request(VFS_IPC_OP_UMOUNT2);
        request.flags = flags as u32;
        populate_vfs_path(&mut request, &path)?;
        call_vfs_status(&request)
    })())
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
            matches!(handle, multitask::KernelHandle::InetSocket(_))
        })
    {
        return linux_unit_result((|| {
            let (_, payload) = kernel_netd_raw_socket_op(
                rustos_user_abi::syscall::SYSCALL_OFFLOAD_OP_LINUX_IOCTL,
                fd,
                [request_number, 0, 0, 0, 0],
                &[],
                None,
            )?;
            let val = decode_fixed_response::<i32>(&payload)?;
            write_user_struct_as(arg, &val)
        })());
    }
    // A console-graph wait is answered by the console broker holding the reply
    // until the graph moves. Asking devmgrd to authorize and forward it would
    // park devmgrd's single serving loop for the whole wait, stalling every
    // unrelated device ioctl behind a call whose entire purpose is to block.
    // Console read, write, and per-session readiness already take this direct
    // rail for the same reason. The console handle is the authority: a caller
    // without one is refused here, before the broker is reached.
    if request_number == rustos_user_abi::console::CONSOLE_IOCTL_WAIT_GRAPH {
        return linux_result(
            console_graph_wait_authority(fd).and_then(|()| console_wait_graph(arg)),
        );
    }
    linux_result((|| {
        let route = if ioctl_is_direct_display_present(request_number)
            || ioctl_is_display_policy_request(request_number)
                && ipc_ops::current_process_has_service_capability(ServiceCapability::UiPolicy)
        {
            rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_DIRECT
        } else {
            match ioctl_route_via_devmgrd(fd, request_number) {
                Ok(route) => route,
                Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_DEVMGRD) => {
                    rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_DIRECT
                }
                Err(errno) => return Err(errno),
            }
        };
        match route {
            rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_SESSIOND_TTY => {
                if !current_kernel_handle(fd).is_some_and(|handle| is_console_handle(&handle)) {
                    return Err(LINUX_ENOTTY);
                }
                ioctl_tty_via_sessiond(fd, request_number, arg)
            }
            rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_DEVMGRD
            | rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT => {
                ioctl_device_via_devmgrd(fd, request_number, arg)
            }
            rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_DIRECT => {
                crate::user::sysops::device::ioctl_current_process_fd(fd, request_number, arg)
                    .map_err(super::super::broker_ops::device_sysop_error_to_linux_errno)
            }
            _ => Err(LINUX_EINVAL),
        }
    })())
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
        multitask::KernelHandle::UnixSocket(_) => {
            rustos_user_abi::handle_kind::HANDLE_KIND_SOCKET
        }
        multitask::KernelHandle::VfsDirectory(_) => rustos_user_abi::handle_kind::HANDLE_KIND_VFS_DIRECTORY,
        multitask::KernelHandle::DisplaySurface(_) => rustos_user_abi::handle_kind::HANDLE_KIND_DISPLAY_SURFACE,
        multitask::KernelHandle::DeviceBuffer(_) => rustos_user_abi::handle_kind::HANDLE_KIND_DEVICE_BUFFER,
        multitask::KernelHandle::Pipe(_) => rustos_user_abi::handle_kind::HANDLE_KIND_PIPE,
        multitask::KernelHandle::EventFd(_) => rustos_user_abi::handle_kind::HANDLE_KIND_EVENTFD,
        multitask::KernelHandle::TimerFd(_) => rustos_user_abi::handle_kind::HANDLE_KIND_TIMERFD,
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
    let mut request = new_netd_request(op);
    request.arg0 = fd;
    request.arg1 = args[0];
    request.arg2 = args[1];
    request.arg3 = args[2];
    request.arg4 = args[3];
    request.arg5 = args[4];
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
    result
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
    let wait_res: LinuxResult<u64> = (|| loop {
        if super::poll_epoll::has_unmasked_pending_signal() {
            return Err(LINUX_EINTR);
        }
        let elapsed = crate::arch::rtc::ticks()
            .saturating_sub(start)
            .saturating_mul(1000)
            .saturating_div(crate::arch::rtc::ticks_per_second().max(1));
        let slice = match timeout_ms {
            Some(limit) if elapsed >= limit => return Err(LINUX_EAGAIN),
            Some(limit) => (limit - elapsed).min(LOCAL_RECEIVE_WAIT_SLICE_MS),
            None => LOCAL_RECEIVE_WAIT_SLICE_MS,
        };
        match kernel_netd_poll_fd(fd, linux_abi::POLLIN as u32, slice.max(1)) {
            // Readable, hung up, or the slice lapsed: either way the receive
            // below is the authority.
            Ok(_) | Err(LINUX_ETIMEDOUT) => {}
            Err(errno) => return Err(errno),
        }
        let result = receive();
        if result != linux_errno(LINUX_EAGAIN) {
            return Ok(result);
        }
    })();
    linux_result(wait_res)
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
    let mut request = new_netd_request(SYSCALL_OFFLOAD_OP_LINUX_SENDTO);
    request.socket_token = snapshot.token();
    request.deadline_ns = netd_deadline_after_class(class, timeout);
    request.operation_hi = u64::from_le_bytes(operation[..8].try_into().unwrap());
    request.operation_lo = u64::from_le_bytes(operation[8..].try_into().unwrap());
    request.arg0 = fd;
    request.status_flags = snapshot.status_flags;
    if request.operation_hi == 0 && request.operation_lo == 0 {
        request.operation_lo = 1;
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
    let mut request = new_netd_request(op);
    request.arg0 = arg0;
    request.arg1 = arg1;
    request.arg2 = arg2;
    request.arg3 = arg3;
    request.arg4 = arg4;
    request.arg5 = arg5;
    let socket_snapshot = populate_netd_socket_token(&mut request);
    if socket_snapshot.is_none()
        && !matches!(
            op,
            SYSCALL_OFFLOAD_OP_LINUX_SOCKET | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR
        )
    {
        let errno = multitask::with_current_user_process_state(|_, _, state| {
            missing_socket_snapshot_errno(state.handles(), arg0)
        })
        .unwrap_or(LINUX_EBADF);
        return linux_errno(errno);
    }
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
    linux_result((|| {
        populate_netd_request_payload(&mut request, socket_snapshot.as_ref())?;
        let response = match timeout_ms {
            Some(timeout_ms) => call_netd_ipc_request_with_timeout(&request, timeout_ms),
            None => call_netd_ipc_request(&request),
        }
        .map_err(|errno| {
            if nonblocking_bound && errno == LINUX_ETIMEDOUT {
                LINUX_EAGAIN
            } else {
                errno
            }
        })?;
        consume_netd_response_payload(&request, &response, socket_snapshot.as_ref())
            .map_err(|errno| {
                if nonblocking_bound && errno == LINUX_ETIMEDOUT {
                    LINUX_EAGAIN
                } else {
                    errno
                }
            })?;
        Ok(response.value)
    })())
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

pub(super) struct SocketSnapshot {
    pub(super) handle: multitask::KernelHandle,
    pub(super) status_flags: u64,
}

impl SocketSnapshot {
    pub(super) fn token(&self) -> u64 {
        match &self.handle {
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
    pub(super) fn uses_netd_message(&self) -> bool {
        self.is_datagram() || self.is_ipv6_datagram()
    }
}

pub(super) fn missing_socket_snapshot_errno(handles: &multitask::HandleTable, fd: u64) -> i64 {
    if handles.get_entry(fd).is_some() {
        LINUX_ENOTSOCK
    } else {
        LINUX_EBADF
    }
}

pub(super) fn snapshot_socket(fd: u64) -> Option<SocketSnapshot> {
    multitask::with_current_user_process_state(|_, _, state| {
        let entry = state.handles().get_entry(fd)?;
        if !matches!(entry.handle(), multitask::KernelHandle::InetSocket(_)) { return None; }
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
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_ACCEPT
        && let multitask::KernelHandle::InetSocket(listener) = &snapshot.handle
    {
        request.arg4 = listener.type_();
        request.arg5 = listener.domain();
    }
    request.socket_token = snapshot.token();
    request.status_flags = snapshot.status_flags;
    Some(snapshot)
}

fn populate_netd_request_payload(
    request: &mut NetdIpcRequest,
    socket: Option<&SocketSnapshot>,
) -> Result<(), i64> {
    if socket.is_some_and(SocketSnapshot::is_ipv6_datagram) {
        match request.op {
            SYSCALL_OFFLOAD_OP_LINUX_SENDTO | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG => {
                return populate_netd_msg_dgram_send(request, socket);
            }
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => return Ok(()),
            SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => {
                let header: linux_abi::LinuxMsghdr = read_user_struct_as(request.arg1)?;
                let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
                request.payload_len =
                    iovec_payload_total_len(&iovecs)?.min(NETD_IPC_PAYLOAD_CAPACITY) as u32;
                request.arg4 = header.msg_controllen;
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
                let header: linux_abi::LinuxMsghdr = read_user_struct_as(request.arg1)?;
                let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
                request.payload_len =
                    iovec_payload_total_len(&iovecs)?.min(NETD_IPC_PAYLOAD_CAPACITY) as u32;
                return Ok(());
            }
            _ => {}
        }
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_BIND | SYSCALL_OFFLOAD_OP_LINUX_CONNECT => {
            copy_current_payload(request, request.arg1, request.arg2)?;
            Ok(())
        }
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO => {
            let prefix_len = request.arg2.min(NETD_IPC_PAYLOAD_CAPACITY as u64);
            copy_current_payload(request, request.arg1, prefix_len)?;
            if current_inet_socket(socket) {
                return Ok(());
            }
            Err(LINUX_ENOTSOCK)
        }
        SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT => {
            copy_current_payload(request, request.arg3, request.arg4)
        }
        SYSCALL_OFFLOAD_OP_LINUX_SENDMSG => {
            let header: linux_abi::LinuxMsghdr = read_user_struct_as(request.arg1)?;
            let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
            let total = iovec_payload_total_len(&iovecs)?;
            if total > NETD_IPC_PAYLOAD_CAPACITY && header.msg_controllen != 0 {
                return Err(LINUX_EMSGSIZE);
            }
            if !current_inet_socket(socket) {
                return Err(LINUX_ENOTSOCK);
            }
            // Ancillary data on an internet socket (IP_PKTINFO, ...) has no
            // transport here; refuse it instead of dropping it silently.
            if header.msg_controllen != 0 {
                return Err(LINUX_EOPNOTSUPP);
            }
            let capacity = sendmsg_data_capacity(0)?;
            let bytes = read_iovec_prefix(&iovecs, total.min(capacity))?;
            let payload_len = NETD_SENDMSG_PAYLOAD_HEADER_SIZE
                .checked_add(bytes.len())
                .ok_or(LINUX_EINVAL)?;
            request.payload[0..4].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
            request.payload[4..8].copy_from_slice(&0_u32.to_ne_bytes());
            request.payload[8..12].copy_from_slice(&0_u32.to_ne_bytes());
            request.payload[12..16].copy_from_slice(&0_u32.to_ne_bytes());
            let data_start = NETD_SENDMSG_PAYLOAD_HEADER_SIZE;
            request.payload[data_start..payload_len].copy_from_slice(bytes.as_slice());
            request.payload_len = payload_len as u32;
            request.arg4 = header.msg_controllen;
            Ok(())
        }
        SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => {
            let header: linux_abi::LinuxMsghdr = read_user_struct_as(request.arg1)?;
            let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
            let total = iovec_payload_total_len(&iovecs)?;
            request.payload_len = total.min(NETD_IPC_PAYLOAD_CAPACITY) as u32;
            request.arg4 = header.msg_controllen;
            if current_inet_socket(socket) {
                return Ok(());
            }
            Err(LINUX_ENOTSOCK)
        }
        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => {
            if current_inet_socket(socket) {
                return Ok(());
            }
            Err(LINUX_ENOTSOCK)
        }
        _ => Ok(()),
    }
}

fn current_inet_socket(socket: Option<&SocketSnapshot>) -> bool {
    socket.is_some_and(|s| matches!(&s.handle, multitask::KernelHandle::InetSocket(_)))
}

fn copy_current_payload(request: &mut NetdIpcRequest, ptr: u64, len: u64) -> Result<(), i64> {
    let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
    if len > NETD_IPC_PAYLOAD_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    if len == 0 {
        return Ok(());
    }
    read_user_bytes_as(ptr, &mut request.payload[..len])?;
    request.payload_len = len as u32;
    Ok(())
}

fn consume_netd_response_payload(
    request: &NetdIpcRequest,
    response: &NetdIpcResponse,
    socket: Option<&SocketSnapshot>,
) -> Result<(), i64> {
    let payload_len = response.payload_len as usize;
    if payload_len > NETD_IPC_PAYLOAD_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    if socket.is_some_and(SocketSnapshot::is_ipv6_datagram)
        && matches!(
            request.op,
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
        )
    {
        return consume_netd_msg_dgram_receive(request, response);
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
                write_user_bytes_as(request.arg1, &response.payload[..payload_len])?;
            }
            if socket.is_some_and(SocketSnapshot::is_netlink) && request.arg4 != 0 {
                write_current_sockaddr_payload(request.arg4, request.arg5, &netlink_kernel_address())?;
            } else if request.arg4 != 0 && request.arg5 != 0 {
                // No peer address is reported for this socket kind: say so
                // (`addrlen = 0`, as Linux does for connected stream sockets)
                // instead of leaving the caller's stale length in place. The
                // bytes were already dequeued, so a bad pointer is not an error.
                let _ = write_user_struct_as(request.arg5, &0_u32);
            }
            Ok(())
        }
        SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => {
            let mut header: linux_abi::LinuxMsghdr = read_user_struct_as(request.arg1)?;
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
            if end > payload_len || !value_matches || control_len != 0 {
                return Err(LINUX_EINVAL);
            }
            let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
            write_current_iovec_payload(&iovecs, &response.payload[data_start..control_start])?;
            let addr = if socket.is_some_and(SocketSnapshot::is_netlink) {
                Some(netlink_kernel_address())
            } else {
                None
            };
            write_msghdr_name(
                &mut header,
                addr.as_ref().map_or(&[], |a| a.as_slice()),
                UserCopyMode::AddressSpace,
            )?;
            header.msg_controllen = 0;
            header.msg_flags = msg_flags;
            write_user_struct_as(request.arg1, &header)
        }
        // Prepared socket publication copied accept outputs before
        // committing the invisible descriptor reservations. Never copy twice.
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT => Ok(()),
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
    read_user_bytes_as(ptr, &mut addr)?;
    Ok(addr)
}

fn populate_netd_msg_dgram_send(
    request: &mut NetdIpcRequest,
    socket: Option<&SocketSnapshot>,
) -> Result<(), i64> {
    use rustos_user_abi::syscall::netd_message;

    let (bytes, address) = if request.op == SYSCALL_OFFLOAD_OP_LINUX_SENDTO {
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
            read_user_bytes_as(request.arg1, &mut bytes)?;
        }
        (bytes, addr)
    } else {
        let header: linux_abi::LinuxMsghdr = read_user_struct_as(request.arg1)?;
        let addr = read_optional_user_sockaddr(header.msg_name, u64::from(header.msg_namelen))?;
        let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
        let total = iovec_payload_total_len(&iovecs)?;
        if total > NETD_IPC_PAYLOAD_CAPACITY {
            return Err(LINUX_EMSGSIZE);
        }
        let bytes = read_iovec_prefix(&iovecs, total)?;
        if !current_inet_socket(socket) {
            return Err(LINUX_ENOTSOCK);
        }
        if header.msg_controllen != 0 {
            return Err(LINUX_EOPNOTSUPP);
        }
        request.arg4 = header.msg_controllen;
        (bytes, addr)
    };
    let len = netd_message::encode(&mut request.payload, &bytes, &[], 0, &address)
        .ok_or(LINUX_EMSGSIZE)?;
    request.payload_len = len as u32;
    Ok(())
}

fn consume_netd_msg_dgram_receive(
    request: &NetdIpcRequest,
    response: &NetdIpcResponse,
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
            write_user_bytes_as(request.arg1, message.data)?;
        }
        if request.arg4 != 0 && request.arg5 != 0 {
            if message.address.is_empty() {
                let _ = write_user_struct_as(request.arg5, &0_u32);
            } else {
                write_current_sockaddr_payload(request.arg4, request.arg5, message.address)?;
            }
        }
        return Ok(());
    }
    let mut header: linux_abi::LinuxMsghdr = read_user_struct_as(request.arg1)?;
    let iovecs = read_current_iovecs_payload(header.msg_iov, header.msg_iovlen)?;
    write_current_iovec_payload(&iovecs, message.data)?;
    write_msghdr_name(&mut header, message.address, UserCopyMode::AddressSpace)?;
    if !message.control.is_empty() {
        return Err(LINUX_ESTALE);
    }
    header.msg_controllen = 0;
    header.msg_flags = message.flags;
    write_user_struct_as(request.arg1, &header)
}

pub(super) fn consume_netd_release_payload(response: &mut NetdIpcResponse) -> Result<(), i64> {
    if response.reserved0 != 0 {
        return Err(LINUX_EINVAL);
    }
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

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum UserCopyMode {
    Pointer,
    AddressSpace,
}

impl UserCopyMode {
    pub(super) fn read_struct<T: Copy + Default>(self, ptr: u64) -> LinuxResult<T> {
        match self {
            Self::Pointer => read_user_struct(ptr),
            Self::AddressSpace => read_user_struct_as(ptr),
        }
    }

    pub(super) fn write_struct<T: rustos_user_abi::wire::UserWire>(
        self,
        ptr: u64,
        value: &T,
    ) -> LinuxResult<()> {
        match self {
            Self::Pointer => write_user_struct(ptr, value),
            Self::AddressSpace => write_user_struct_as(ptr, value),
        }
    }

    pub(super) fn read_bytes(self, ptr: u64, dest: &mut [u8]) -> LinuxResult<()> {
        match self {
            Self::Pointer => read_user_bytes(ptr, dest),
            Self::AddressSpace => read_user_bytes_as(ptr, dest),
        }
    }

    pub(super) fn write_bytes(self, ptr: u64, bytes: &[u8]) -> LinuxResult<()> {
        match self {
            Self::Pointer => write_user_bytes(ptr, bytes),
            Self::AddressSpace => write_user_bytes_as(ptr, bytes),
        }
    }
}

fn read_current_iovec_payload(iov_ptr: u64, iov_len: u64) -> Result<Vec<u8>, i64> {
    let iovecs = read_current_iovecs_payload(iov_ptr, iov_len)?;
    let total = iovec_payload_total_len(&iovecs)?;
    if total > NETD_IPC_PAYLOAD_CAPACITY {
        return Err(LINUX_EMSGSIZE);
    }
    read_iovec_prefix(&iovecs, total)
}

pub(super) fn read_iovec_prefix_with_mode(
    iovecs: &[linux_abi::LinuxIovec],
    total: usize,
    mode: UserCopyMode,
) -> Result<Vec<u8>, i64> {
    let mut bytes = Vec::new();
    if total > 0 {
        bytes.try_reserve_exact(total).map_err(|_| LINUX_ENOMEM)?;
    }
    for iov in iovecs {
        if bytes.len() == total {
            break;
        }
        let len = usize::try_from(iov.iov_len)
            .map_err(|_| LINUX_EINVAL)?
            .min(total - bytes.len());
        if len == 0 {
            continue;
        }
        let start = bytes.len();
        bytes.resize(start + len, 0);
        mode.read_bytes(iov.iov_base, &mut bytes[start..])?;
    }
    Ok(bytes)
}

fn read_iovec_prefix(iovecs: &[linux_abi::LinuxIovec], total: usize) -> Result<Vec<u8>, i64> {
    read_iovec_prefix_with_mode(iovecs, total, UserCopyMode::AddressSpace)
}

pub(super) fn read_current_iovecs_with_mode(
    iov_ptr: u64,
    iov_len: u64,
    mode: UserCopyMode,
) -> Result<Vec<linux_abi::LinuxIovec>, i64> {
    let iov_len = usize::try_from(iov_len).map_err(|_| LINUX_EINVAL)?;
    if iov_len == 0 {
        return Ok(Vec::new());
    }
    if iov_len > NETD_MAX_IOVEC_COUNT {
        return Err(LINUX_EINVAL);
    }
    if iov_ptr == 0 {
        return Err(LINUX_EFAULT);
    }
    let byte_len = iov_len
        .checked_mul(size_of::<linux_abi::LinuxIovec>())
        .ok_or(LINUX_EINVAL)?;
    let mut raw = alloc::vec![0_u8; byte_len];
    mode.read_bytes(iov_ptr, &mut raw)?;
    let mut iovecs = Vec::with_capacity(iov_len);
    for chunk in raw.chunks_exact(size_of::<linux_abi::LinuxIovec>()) {
        iovecs.push(linux_abi::LinuxIovec {
            iov_base: u64::from_ne_bytes(chunk[0..8].try_into().unwrap()),
            iov_len: u64::from_ne_bytes(chunk[8..16].try_into().unwrap()),
        });
    }
    Ok(iovecs)
}

pub(super) fn read_current_iovecs_payload(
    iov_ptr: u64,
    iov_len: u64,
) -> Result<Vec<linux_abi::LinuxIovec>, i64> {
    read_current_iovecs_with_mode(iov_ptr, iov_len, UserCopyMode::AddressSpace)
}

pub(super) fn iovec_payload_total_len(iovecs: &[linux_abi::LinuxIovec]) -> Result<usize, i64> {
    let mut total = 0usize;
    for iov in iovecs {
        let len = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?;
        total = total
            .checked_add(len)
            .filter(|sum| *sum <= isize::MAX as usize)
            .ok_or(LINUX_EINVAL)?;
    }
    Ok(total)
}

pub(super) fn write_iovec_payload_with_mode(
    iovecs: &[linux_abi::LinuxIovec],
    bytes: &[u8],
    mode: UserCopyMode,
) -> Result<usize, i64> {
    let mut written = 0usize;
    for iov in iovecs {
        if written >= bytes.len() {
            break;
        }
        let len = usize::try_from(iov.iov_len).map_err(|_| LINUX_EINVAL)?;
        let chunk_len = len.min(bytes.len() - written);
        if chunk_len == 0 {
            continue;
        }
        mode.write_bytes(iov.iov_base, &bytes[written..written + chunk_len])?;
        written += chunk_len;
    }
    Ok(written)
}

fn write_current_iovec_payload(iovecs: &[linux_abi::LinuxIovec], bytes: &[u8]) -> Result<(), i64> {
    write_iovec_payload_with_mode(iovecs, bytes, UserCopyMode::AddressSpace).map(|_| ())
}

pub(super) fn write_msghdr_name(
    header: &mut linux_abi::LinuxMsghdr,
    address: &[u8],
    mode: UserCopyMode,
) -> Result<(), i64> {
    if header.msg_name != 0 {
        let copy_len = (header.msg_namelen as usize).min(address.len());
        if copy_len != 0 {
            mode.write_bytes(header.msg_name, &address[..copy_len])?;
        }
        header.msg_namelen = u32::try_from(address.len()).map_err(|_| LINUX_EINVAL)?;
    } else {
        header.msg_namelen = 0;
    }
    Ok(())
}

// Netlink currently produces only unicast replies from the kernel port.
fn netlink_kernel_address() -> [u8; 12] {
    let mut address = [0; 12];
    address[..2].copy_from_slice(&(rustos_user_abi::syscall::NET_BROKER_PUBLICATION_AF_NETLINK as u16).to_ne_bytes());
    address
}

pub(super) fn write_sockaddr_payload_with_mode(
    addr_ptr: u64,
    len_ptr: u64,
    payload: &[u8],
    mode: UserCopyMode,
) -> Result<(), i64> {
    if len_ptr == 0 {
        return match mode {
            UserCopyMode::Pointer => Err(LINUX_EFAULT),
            UserCopyMode::AddressSpace => {
                if payload.is_empty() {
                    Ok(())
                } else {
                    Err(LINUX_EINVAL)
                }
            }
        };
    }
    if addr_ptr != 0 {
        let cap = mode.read_struct::<i32>(len_ptr)?;
        if cap < 0 {
            return Err(LINUX_EINVAL);
        }
        if !payload.is_empty() {
            mode.write_bytes(addr_ptr, &payload[..(cap as usize).min(payload.len())])?;
        }
    }
    let len = u32::try_from(payload.len()).map_err(|_| LINUX_EINVAL)?;
    mode.write_struct(len_ptr, &len)
}

pub(super) fn write_current_sockaddr_payload(addr_ptr: u64, len_ptr: u64, payload: &[u8]) -> Result<(), i64> {
    write_sockaddr_payload_with_mode(addr_ptr, len_ptr, payload, UserCopyMode::AddressSpace)
}

pub(super) fn write_sockopt_payload(
    optval_ptr: u64,
    optlen_ptr: u64,
    payload: &[u8],
    allow_struct_truncation: bool,
    mode: UserCopyMode,
) -> Result<(), i64> {
    if optval_ptr == 0 || optlen_ptr == 0 {
        return Err(match mode {
            UserCopyMode::Pointer => LINUX_EFAULT,
            UserCopyMode::AddressSpace => LINUX_EINVAL,
        });
    }
    let cap = mode.read_struct::<i32>(optlen_ptr)?;
    if cap < 0 {
        return Err(LINUX_EINVAL);
    }
    let capacity = cap as usize;
    // Linux truncates an int-valued option to the caller's `optlen`
    // (`SO_ERROR` read through a 1-byte buffer); structured netd options with
    // a short buffer stay `EINVAL`.
    let out = if capacity >= payload.len() {
        payload
    } else if allow_struct_truncation || (payload.len() == size_of::<i32>() && capacity != 0) {
        &payload[..capacity]
    } else {
        return Err(LINUX_EINVAL);
    };
    mode.write_bytes(optval_ptr, out)?;
    let len = u32::try_from(out.len()).map_err(|_| LINUX_EINVAL)?;
    mode.write_struct(optlen_ptr, &len)
}

fn write_current_sockopt_payload(
    optval_ptr: u64,
    optlen_ptr: u64,
    payload: &[u8],
) -> Result<(), i64> {
    write_sockopt_payload(
        optval_ptr,
        optlen_ptr,
        payload,
        false,
        UserCopyMode::AddressSpace,
    )
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
    fn iovec_payload_total_len_rejects_overflow_and_oversized_sum() {
        let overflowing = [
            linux_abi::LinuxIovec { iov_base: 0, iov_len: u64::MAX - 4 },
            linux_abi::LinuxIovec { iov_base: 0, iov_len: 16 },
        ];
        assert_eq!(iovec_payload_total_len(&overflowing), Err(LINUX_EINVAL));

        let too_large = [
            linux_abi::LinuxIovec { iov_base: 0, iov_len: (isize::MAX as u64) },
            linux_abi::LinuxIovec { iov_base: 0, iov_len: 1 },
        ];
        assert_eq!(iovec_payload_total_len(&too_large), Err(LINUX_EINVAL));
    }

    #[test]
    fn write_iovec_payload_skips_zero_len_null_base_iovecs() {
        let iovecs = [
            linux_abi::LinuxIovec { iov_base: 0, iov_len: 0 },
        ];
        assert_eq!(
            write_iovec_payload_with_mode(&iovecs, b"abc", UserCopyMode::Pointer),
            Ok(0)
        );
        assert_eq!(
            write_iovec_payload_with_mode(&iovecs, &[], UserCopyMode::AddressSpace),
            Ok(0)
        );
    }

    #[test]
    fn netlink_kernel_source_has_sockaddr_nl_shape() {
        let addr = netlink_kernel_address();
        assert_eq!(u16::from_ne_bytes([addr[0], addr[1]]) as u64, rustos_user_abi::syscall::NET_BROKER_PUBLICATION_AF_NETLINK);
        assert_eq!(&addr[2..], &[0; 10]);
    }
}
