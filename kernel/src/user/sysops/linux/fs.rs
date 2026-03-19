use super::*;

pub(crate) fn access(path_ptr: u64, mode: u64) -> Result<(), LinuxSysopError> {
    let path = usermem::read_current_user_c_string(path_ptr, 128)?;
    check_access_path(linux_abi::AT_FDCWD as u64, &path, mode, 0)
}

pub(crate) fn getcwd(user_ptr: u64, user_len: u64) -> Result<usize, LinuxSysopError> {
    let user_len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let cwd = b"/\0";
    if user_len < cwd.len() {
        return Err(LinuxSysopError::InvalidArgument);
    }
    usermem::write_current_user_bytes(user_ptr, cwd)?;
    Ok(cwd.len())
}

pub(crate) fn faccessat(
    dirfd: u64,
    path_ptr: u64,
    mode: u64,
    flags: u64,
) -> Result<(), LinuxSysopError> {
    let path = usermem::read_current_user_c_string(path_ptr, 128)?;
    check_access_path(dirfd, &path, mode, flags)
}

pub(crate) fn readlink(
    path_ptr: u64,
    user_ptr: u64,
    user_len: u64,
) -> Result<usize, LinuxSysopError> {
    readlinkat(linux_abi::AT_FDCWD as u64, path_ptr, user_ptr, user_len)
}

pub(crate) fn readlinkat(
    dirfd: u64,
    path_ptr: u64,
    user_ptr: u64,
    user_len: u64,
) -> Result<usize, LinuxSysopError> {
    let path = usermem::read_current_user_c_string(path_ptr, 256)?;
    let absolute_path = file::resolve_path_for_current_process(dirfd, &path)?;
    let target = crate::vfs::readlink_for_current_process(absolute_path.as_str())
        .map_err(file::FileSysopError::from)
        .map_err(LinuxSysopError::from)?;
    if user_len == 0 {
        return Ok(0);
    }

    let user_len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let bytes = target.as_bytes();
    let copy_len = bytes.len().min(user_len);
    usermem::write_current_user_bytes(user_ptr, &bytes[..copy_len])?;
    Ok(copy_len)
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

pub(crate) fn newfstatat(
    dirfd: u64,
    path_ptr: u64,
    stat_ptr: u64,
    flags: u64,
) -> Result<(), LinuxSysopError> {
    let supported_flags = linux_abi::AT_EMPTY_PATH;
    if flags & !supported_flags != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let path = usermem::read_current_user_c_string(path_ptr, 128)?;
    let stat = stat_for_path_or_fd(dirfd, Some(path.as_str()), flags)?;

    write_linux_stat(stat_ptr, &stat)
}

pub(crate) fn statx(
    dirfd: u64,
    path_ptr: u64,
    flags: u64,
    mask: u32,
    statx_ptr: u64,
) -> Result<(), LinuxSysopError> {
    let supported_flags = linux_abi::AT_EMPTY_PATH
        | linux_abi::AT_SYMLINK_NOFOLLOW
        | linux_abi::AT_NO_AUTOMOUNT
        | linux_abi::AT_STATX_SYNC_TYPE;
    if flags & !supported_flags != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let path = if path_ptr == 0 {
        None
    } else {
        Some(usermem::read_current_user_c_string(path_ptr, 128)?)
    };
    let stat = stat_for_path_or_fd(dirfd, path.as_deref(), flags)?;
    let statx = linux_stat_to_statx(&stat, mask);
    write_linux_statx(statx_ptr, &statx)
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

fn write_linux_statx(statx_ptr: u64, statx: &linux_abi::LinuxStatx) -> Result<(), LinuxSysopError> {
    let bytes = unsafe {
        slice::from_raw_parts(
            (statx as *const linux_abi::LinuxStatx).cast::<u8>(),
            size_of::<linux_abi::LinuxStatx>(),
        )
    };
    usermem::write_current_user_bytes(statx_ptr, bytes)?;
    Ok(())
}

fn stat_for_descriptor(fd: u64) -> Result<linux_abi::LinuxStat, LinuxSysopError> {
    let stat = stat::for_fd(fd).map_err(stat_lookup_error_to_linux)?;
    Ok(kernel_stat_to_linux_stat(&stat))
}

fn stat_for_path_or_fd(
    dirfd: u64,
    path: Option<&str>,
    flags: u64,
) -> Result<linux_abi::LinuxStat, LinuxSysopError> {
    let Some(path) = path else {
        if flags & linux_abi::AT_EMPTY_PATH == 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }
        return stat_for_descriptor(dirfd);
    };

    if path.is_empty() {
        if flags & linux_abi::AT_EMPTY_PATH == 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }
        return stat_for_descriptor(dirfd);
    }

    let absolute_path = file::resolve_path_for_current_process(dirfd, path)?;
    let stat =
        stat::for_absolute_path(absolute_path.as_str()).map_err(stat_lookup_error_to_linux)?;
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

fn check_access_path(dirfd: u64, path: &str, mode: u64, flags: u64) -> Result<(), LinuxSysopError> {
    if flags & !linux_abi::AT_EACCESS != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let absolute_path = file::resolve_path_for_current_process(dirfd, path)?;
    file::check_access_for_current_process(absolute_path.as_str(), mode).map_err(Into::into)
}
