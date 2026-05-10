use super::*;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;

use linux_raw_sys::general::linux_dirent64;

use crate::memory::paging;
use crate::multitask;
use crate::user::handles::{KernelHandle, VfsDirectoryEntry, VfsDirectoryEntryKind};

const USER_C_STRING_COPY_CHUNK: usize = 256;
const USER_PAGE_SIZE: usize = 4096;

pub(crate) fn unlink(path_ptr: u64) -> Result<(), LinuxSysopError> {
    unlinkat(linux_abi::AT_FDCWD as u64, path_ptr, 0)
}

pub(crate) fn unlinkat(dirfd: u64, path_ptr: u64, flags: u64) -> Result<(), LinuxSysopError> {
    if flags != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let path = usermem::read_current_user_c_string(path_ptr, 256)?;
    if path.is_empty() {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let absolute_path = file::resolve_path_for_current_process(dirfd, &path)?;
    crate::user::socket::unlink_bound_path(absolute_path.as_str(), current_socket_credentials()?)
        .map_err(Into::into)
}

fn current_socket_credentials() -> Result<crate::user::socket::SocketCredentials, LinuxSysopError> {
    multitask::with_current_user_process_state(|pid, _, process_state| {
        crate::user::socket::SocketCredentials::new(
            i32::try_from(pid).unwrap_or(i32::MAX),
            process_state.security().euid(),
            process_state.security().egid(),
        )
    })
    .ok_or(LinuxSysopError::Unsupported)
}

pub(crate) fn getdents64(fd: u64, user_ptr: u64, user_len: u64) -> Result<usize, LinuxSysopError> {
    let process_id = multitask::current_user_process_id().ok_or(LinuxSysopError::Unsupported)?;
    getdents64_for_process(process_id, fd, user_ptr, user_len)
}

pub(crate) fn getdents64_for_process(
    process_id: u64,
    fd: u64,
    user_ptr: u64,
    user_len: u64,
) -> Result<usize, LinuxSysopError> {
    let user_len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if user_len < size_of::<linux_dirent64>() + 2 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let Some(result) = multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        process_state
            .address_space()
            .validate_user_write_buffer(VirtAddr::new(user_ptr), user_len)?;

        let (records, consumed_entries, written) = {
            let Some(handle) = process_state.handles_mut().get_mut(fd) else {
                return Err(LinuxSysopError::BadFileDescriptor);
            };
            let KernelHandle::VfsDirectory(directory) = handle else {
                return Err(LinuxSysopError::NotDirectory);
            };

            let mut records = Vec::new();
            let mut consumed_entries = 0usize;
            let mut written = 0usize;
            let start_cursor = directory.cursor();
            let entries = directory.entries();
            while let Some(entry) = entries.get(start_cursor + consumed_entries) {
                let record_len = linux_dirent_record_len(entry.name())?;
                if written + record_len > user_len {
                    if written == 0 {
                        return Err(LinuxSysopError::InvalidArgument);
                    }
                    break;
                }

                let record = encode_linux_dirent64(entry, start_cursor + consumed_entries + 1)?;
                written += record.len();
                consumed_entries += 1;
                records.push(record);
            }

            directory.advance_cursor(consumed_entries);
            (records, consumed_entries, written)
        };

        let mut copied = 0usize;
        for record in records {
            let dest_ptr = user_ptr
                .checked_add(copied as u64)
                .ok_or(LinuxSysopError::InvalidArgument)?;
            process_state
                .address_space()
                .copy_into_user(VirtAddr::new(dest_ptr), record.as_slice())?;
            copied += record.len();
        }

        debug_assert_eq!(copied, written);
        let _ = consumed_entries;
        Ok(written)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn resolve_faccessat_absolute_path_for_current_process(
    dirfd: u64,
    path_ptr: u64,
    flags: u64,
) -> Result<String, LinuxSysopError> {
    if flags & !linux_abi::AT_EACCESS != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    resolve_nonempty_user_path_for_current_process(dirfd, path_ptr, 128)
}

pub(crate) fn mount_for_process(
    process_id: u64,
    source_ptr: u64,
    target_path: &str,
    fstype_ptr: u64,
    flags: u64,
    data_ptr: u64,
) -> Result<(), LinuxSysopError> {
    if source_ptr == 0 || fstype_ptr == 0 || target_path.is_empty() {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let source = read_user_c_string_for_process(process_id, source_ptr, 256)?;
    let filesystem_type = read_user_c_string_for_process(process_id, fstype_ptr, 64)?;
    if source.is_empty() || filesystem_type.is_empty() {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let source = file::resolve_path_for_process(process_id, &source)?;
    let options = if data_ptr == 0 {
        None
    } else {
        Some(read_user_c_string_for_process(process_id, data_ptr, 256)?)
    };

    crate::vfs::mount_for_current_process(
        source.as_str(),
        target_path,
        filesystem_type.as_str(),
        flags,
        options.as_deref(),
    )
    .map_err(LinuxSysopError::from)
}

pub(crate) fn umount2_for_process(
    _process_id: u64,
    target_path: &str,
    flags: u64,
) -> Result<(), LinuxSysopError> {
    if target_path.is_empty() || flags != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    crate::vfs::umount_for_current_process(target_path).map_err(LinuxSysopError::from)
}

pub(crate) fn pread64(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    offset: u64,
) -> Result<usize, LinuxSysopError> {
    if fd <= 2 {
        return Err(LinuxSysopError::IllegalSeek);
    }

    match file::pread_current_process_file(fd, user_ptr, user_len, offset)? {
        Some(read) => Ok(read),
        None => Err(LinuxSysopError::IllegalSeek),
    }
}

pub(crate) fn lseek(fd: u64, offset: i64, whence: u64) -> Result<u64, LinuxSysopError> {
    if fd <= 2 {
        return Err(LinuxSysopError::IllegalSeek);
    }

    let whence = match whence {
        linux_abi::SEEK_SET => FileHandleSeekWhence::Start,
        linux_abi::SEEK_CUR => FileHandleSeekWhence::Current,
        linux_abi::SEEK_END => FileHandleSeekWhence::End,
        _ => return Err(LinuxSysopError::InvalidArgument),
    };

    match file::seek_current_process_file(fd, offset, whence)? {
        Some(position) => Ok(position),
        None => Err(LinuxSysopError::IllegalSeek),
    }
}

pub(crate) fn fstat(fd: u64, stat_ptr: u64) -> Result<(), LinuxSysopError> {
    let stat = stat_for_descriptor(fd)?;
    write_linux_stat(stat_ptr, &stat)
}

pub(crate) fn resolve_statx_absolute_path_for_current_process(
    dirfd: u64,
    path_ptr: u64,
    flags: u64,
) -> Result<String, LinuxSysopError> {
    let supported_flags = linux_abi::AT_EMPTY_PATH
        | linux_abi::AT_SYMLINK_NOFOLLOW
        | linux_abi::AT_NO_AUTOMOUNT
        | linux_abi::AT_STATX_SYNC_TYPE;
    if flags & !supported_flags != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if path_ptr == 0 {
        return Err(LinuxSysopError::OperationNotSupported);
    }

    let path = usermem::read_current_user_c_string(path_ptr, 128)?;
    if path.is_empty() {
        return Err(LinuxSysopError::OperationNotSupported);
    }
    file::resolve_path_for_current_process(dirfd, &path).map_err(LinuxSysopError::from)
}

pub(crate) fn resolve_newfstatat_absolute_path_for_current_process(
    dirfd: u64,
    path_ptr: u64,
    flags: u64,
) -> Result<String, LinuxSysopError> {
    let supported_flags = linux_abi::AT_EMPTY_PATH;
    if flags & !supported_flags != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    resolve_nonempty_user_path_for_current_process(dirfd, path_ptr, 128)
}

pub(crate) fn resolve_readlinkat_absolute_path_for_current_process(
    dirfd: u64,
    path_ptr: u64,
) -> Result<String, LinuxSysopError> {
    resolve_nonempty_user_path_for_current_process(dirfd, path_ptr, 256)
}

pub(crate) fn statx_for_absolute_path(
    absolute_path: &str,
    mask: u32,
) -> Result<linux_abi::LinuxStatx, LinuxSysopError> {
    if !absolute_path.starts_with('/') {
        return Err(LinuxSysopError::InvalidArgument);
    }
    let stat = stat::for_absolute_path(absolute_path).map_err(stat_lookup_error_to_linux)?;
    let stat = kernel_stat_to_linux_stat(&stat);
    Ok(linux_stat_to_statx(&stat, mask))
}

pub(crate) fn stat_for_absolute_path(
    absolute_path: &str,
) -> Result<linux_abi::LinuxStat, LinuxSysopError> {
    if !absolute_path.starts_with('/') {
        return Err(LinuxSysopError::InvalidArgument);
    }
    let stat = stat::for_absolute_path(absolute_path).map_err(stat_lookup_error_to_linux)?;
    Ok(kernel_stat_to_linux_stat(&stat))
}

pub(crate) fn readlink_for_absolute_path(
    absolute_path: &str,
    max_len: usize,
) -> Result<Vec<u8>, LinuxSysopError> {
    if !absolute_path.starts_with('/') {
        return Err(LinuxSysopError::InvalidArgument);
    }
    let target = crate::vfs::readlink_for_current_process(absolute_path)
        .map_err(file::FileSysopError::from)
        .map_err(LinuxSysopError::from)?;
    Ok(target.as_bytes()[..target.len().min(max_len)].to_vec())
}

pub(crate) fn check_access_for_absolute_path_and_process(
    process_id: u64,
    absolute_path: &str,
    mode: u64,
) -> Result<(), LinuxSysopError> {
    if !absolute_path.starts_with('/') {
        return Err(LinuxSysopError::InvalidArgument);
    }
    multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        crate::vfs::check_access_for_user_process(
            absolute_path,
            mode,
            crate::user::abi::UserAbi::Linux,
            process_state,
        )
        .map_err(file::FileSysopError::from)
        .map_err(LinuxSysopError::from)
    })
    .ok_or(LinuxSysopError::NoSuchProcess)?
}

pub(crate) fn cwd_for_process(process_id: u64) -> Result<String, LinuxSysopError> {
    multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        String::from(process_state.cwd())
    })
    .ok_or(LinuxSysopError::NoSuchProcess)
}

pub(crate) fn chdir_absolute_path_for_process(
    process_id: u64,
    absolute_path: &str,
) -> Result<(), LinuxSysopError> {
    if !absolute_path.starts_with('/') {
        return Err(LinuxSysopError::InvalidArgument);
    }
    multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        crate::vfs::check_access_for_user_process(
            absolute_path,
            0,
            crate::user::abi::UserAbi::Linux,
            process_state,
        )
        .map_err(file::FileSysopError::from)
        .map_err(LinuxSysopError::from)?;
        let stat = stat::for_absolute_path(absolute_path).map_err(stat_lookup_error_to_linux)?;
        if stat.kind != stat::KernelNodeKind::Directory {
            return Err(LinuxSysopError::NotDirectory);
        }
        process_state.set_cwd(absolute_path);
        Ok(())
    })
    .ok_or(LinuxSysopError::NoSuchProcess)?
}

fn resolve_nonempty_user_path_for_current_process(
    dirfd: u64,
    path_ptr: u64,
    max_len: usize,
) -> Result<String, LinuxSysopError> {
    if path_ptr == 0 {
        return Err(LinuxSysopError::OperationNotSupported);
    }
    let path = usermem::read_current_user_c_string(path_ptr, max_len)?;
    if path.is_empty() {
        return Err(LinuxSysopError::OperationNotSupported);
    }
    file::resolve_path_for_current_process(dirfd, &path).map_err(LinuxSysopError::from)
}

fn read_user_c_string_for_process(
    process_id: u64,
    user_ptr: u64,
    max_len: usize,
) -> Result<String, LinuxSysopError> {
    if user_ptr == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    let Some(result) =
        multitask::with_process_state_by_pid_mut(process_id, |process_state| {
            let address_space = process_state.address_space();
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; USER_C_STRING_COPY_CHUNK];

            while bytes.len() < max_len {
                let ptr = user_ptr.checked_add(bytes.len() as u64).ok_or(
                    LinuxSysopError::AddressSpace(paging::AddressSpaceError::AddressOverflow),
                )?;
                let page_remaining = USER_PAGE_SIZE - ((ptr as usize) & (USER_PAGE_SIZE - 1));
                let chunk_len = (max_len - bytes.len()).min(chunk.len()).min(page_remaining);
                address_space.copy_from_user(VirtAddr::new(ptr), &mut chunk[..chunk_len])?;
                if let Some(nul_index) = chunk[..chunk_len].iter().position(|byte| *byte == 0) {
                    bytes.extend_from_slice(&chunk[..nul_index]);
                    return Ok(String::from_utf8_lossy(&bytes).into_owned());
                }
                bytes.extend_from_slice(&chunk[..chunk_len]);
            }

            Err(LinuxSysopError::AddressSpace(
                paging::AddressSpaceError::AddressOverflow,
            ))
        })
    else {
        return Err(LinuxSysopError::NoSuchProcess);
    };
    result
}

fn write_linux_stat(stat_ptr: u64, stat: &linux_abi::LinuxStat) -> Result<(), LinuxSysopError> {
    let bytes = unsafe {
        slice::from_raw_parts(
            (stat as *const linux_abi::LinuxStat).cast::<u8>(),
            size_of::<linux_abi::LinuxStat>(),
        )
    };
    usermem::write_current_user_bytes(stat_ptr, bytes)?;
    Ok(())
}

fn stat_for_descriptor(fd: u64) -> Result<linux_abi::LinuxStat, LinuxSysopError> {
    let stat = stat::for_fd(fd).map_err(stat_lookup_error_to_linux)?;
    Ok(kernel_stat_to_linux_stat(&stat))
}

fn linux_stat_to_statx(stat: &linux_abi::LinuxStat, _requested_mask: u32) -> linux_abi::LinuxStatx {
    let supported_mask = linux_abi::STATX_BASIC_STATS;
    linux_abi::LinuxStatx {
        stx_mask: supported_mask,
        stx_blksize: stat.st_blksize.max(0).min(u32::MAX as i64) as u32,
        stx_nlink: stat.st_nlink.min(u32::MAX as u64) as u32,
        stx_uid: stat.st_uid,
        stx_gid: stat.st_gid,
        stx_mode: stat.st_mode.min(u16::MAX as u32) as u16,
        stx_ino: stat.st_ino,
        stx_size: stat.st_size.max(0) as u64,
        stx_blocks: stat.st_blocks.max(0) as u64,
        stx_atime: linux_timespec_to_statx_timestamp(stat.st_atim),
        stx_ctime: linux_timespec_to_statx_timestamp(stat.st_ctim),
        stx_mtime: linux_timespec_to_statx_timestamp(stat.st_mtim),
        ..linux_abi::LinuxStatx::default()
    }
}

fn linux_timespec_to_statx_timestamp(
    timespec: linux_abi::LinuxTimespec,
) -> linux_abi::LinuxStatxTimestamp {
    linux_abi::LinuxStatxTimestamp {
        tv_sec: timespec.tv_sec,
        tv_nsec: timespec.tv_nsec.max(0).min(u32::MAX as i64) as u32,
        __reserved: 0,
    }
}

fn kernel_stat_to_linux_stat(stat: &stat::KernelStat) -> linux_abi::LinuxStat {
    linux_abi::LinuxStat {
        st_ino: stat.inode,
        st_nlink: stat.link_count,
        st_mode: kernel_node_mode_bits(stat.kind),
        st_size: stat.size.min(i64::MAX as u64) as i64,
        st_blksize: stat.block_size.min(i64::MAX as u64) as i64,
        st_blocks: stat.blocks.min(i64::MAX as u64) as i64,
        st_atim: linux_abi::LinuxTimespec {
            tv_sec: stat.atime.sec,
            tv_nsec: stat.atime.nsec,
        },
        st_mtim: linux_abi::LinuxTimespec {
            tv_sec: stat.mtime.sec,
            tv_nsec: stat.mtime.nsec,
        },
        st_ctim: linux_abi::LinuxTimespec {
            tv_sec: stat.ctime.sec,
            tv_nsec: stat.ctime.nsec,
        },
        ..linux_abi::LinuxStat::default()
    }
}

fn kernel_node_mode_bits(kind: stat::KernelNodeKind) -> u32 {
    match kind {
        stat::KernelNodeKind::File => linux_abi::BOOT_FILE_MODE_BITS,
        stat::KernelNodeKind::Directory => linux_abi::BOOT_DIRECTORY_MODE_BITS,
        stat::KernelNodeKind::Device => linux_abi::DEVICE_FILE_MODE_BITS,
    }
}

fn stat_lookup_error_to_linux(err: stat::StatLookupError) -> LinuxSysopError {
    match err {
        stat::StatLookupError::BadFileDescriptor => LinuxSysopError::BadFileDescriptor,
        stat::StatLookupError::InvalidArgument => LinuxSysopError::InvalidArgument,
        stat::StatLookupError::NotFound => LinuxSysopError::NotFound,
        stat::StatLookupError::NotDirectory => LinuxSysopError::NotDirectory,
        stat::StatLookupError::PermissionDenied => LinuxSysopError::PermissionDenied,
        stat::StatLookupError::Unsupported => LinuxSysopError::Unsupported,
    }
}

fn linux_dirent_record_len(name: &str) -> Result<usize, LinuxSysopError> {
    let base_len = size_of::<linux_dirent64>();
    let len = base_len
        .checked_add(name.len())
        .and_then(|value| value.checked_add(1))
        .ok_or(LinuxSysopError::InvalidArgument)?;
    Ok((len + 7) & !7)
}

fn encode_linux_dirent64(
    entry: &VfsDirectoryEntry,
    next_offset: usize,
) -> Result<Vec<u8>, LinuxSysopError> {
    let record_len = linux_dirent_record_len(entry.name())?;
    let mut bytes = vec![0_u8; record_len];
    bytes[..8].copy_from_slice(&entry.inode().to_le_bytes());
    bytes[8..16].copy_from_slice(&(next_offset as i64).to_le_bytes());
    bytes[16..18].copy_from_slice(&(record_len as u16).to_le_bytes());
    bytes[18] = linux_dirent_type(entry.kind());
    let name_bytes = entry.name().as_bytes();
    bytes[19..19 + name_bytes.len()].copy_from_slice(name_bytes);
    bytes[19 + name_bytes.len()] = 0;
    Ok(bytes)
}

fn linux_dirent_type(kind: VfsDirectoryEntryKind) -> u8 {
    match kind {
        VfsDirectoryEntryKind::File => linux_abi::DT_REG as u8,
        VfsDirectoryEntryKind::Directory => linux_abi::DT_DIR as u8,
        VfsDirectoryEntryKind::Device => linux_abi::DT_CHR as u8,
    }
}
