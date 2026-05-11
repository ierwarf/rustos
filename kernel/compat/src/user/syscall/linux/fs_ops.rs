use super::*;
use rustos_user_abi::syscall::SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT;
pub(super) fn syscall_linux_write(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    debug_log_secondary_linux_syscall(|| {
        alloc::format!("write fd={} ptr={:#x} len={}", fd, user_ptr, user_len)
    });
    match offload_ops::call_remote_vfs_write(fd, user_ptr, user_len) {
        Ok(Some(written)) => return written,
        Ok(None) => {}
        Err(errno) => return linux_errno(errno),
    }
    match linux_ops::write(fd, user_ptr, user_len) {
        Ok(written) => written as u64,
        Err(err) => {
            debug::println!(
                "linux write rejected: fd={} user_ptr={:#x} len={} err={:?}",
                fd,
                user_ptr,
                user_len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_unlink(path_ptr: u64) -> u64 {
    if let Err(errno) = vfs_path_policy(
        SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT,
        linux_abi::AT_FDCWD as u64,
        path_ptr,
        0,
        0,
    ) {
        return linux_errno(errno);
    }
    match linux_ops::unlink(path_ptr) {
        Ok(()) => 0,
        Err(err) => {
            if !matches!(err, linux_ops::LinuxSysopError::NotFound) {
                debug::println!(
                    "linux unlink rejected: path_ptr={:#x} path={} err={:?}",
                    path_ptr,
                    debug_user_path(path_ptr),
                    err,
                );
            }
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_read(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    debug_log_secondary_linux_syscall(|| {
        alloc::format!("read fd={} ptr={:#x} len={}", fd, user_ptr, user_len)
    });
    match offload_ops::call_remote_vfs_read(fd, user_ptr, user_len) {
        Ok(Some(read)) => return read,
        Ok(None) => {}
        Err(errno) => return linux_errno(errno),
    }
    match linux_ops::read(fd, user_ptr, user_len) {
        Ok(read) => read as u64,
        Err(err) => {
            debug::println!(
                "linux read rejected: fd={} user_ptr={:#x} len={} err={:?}",
                fd,
                user_ptr,
                user_len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_close(fd: u64) -> u64 {
    if offload_ops::current_process_may_bootstrap_policy_service() {
        return match linux_ops::close(fd) {
            Ok(()) => 0,
            Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
        };
    }
    match offload_ops::call_vfs_close_fd(fd) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_ftruncate(fd: u64, len: u64) -> u64 {
    match offload_ops::call_remote_vfs_ftruncate(fd, len) {
        Ok(Some(())) => return 0,
        Ok(None) => {}
        Err(errno) => return linux_errno(errno),
    }
    match linux_ops::ftruncate(fd, len) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_fstat(fd: u64, stat_ptr: u64) -> u64 {
    match offload_ops::call_remote_vfs_fstat(fd, stat_ptr) {
        Ok(Some(())) => return 0,
        Ok(None) => {}
        Err(errno) => return linux_errno(errno),
    }
    match linux_ops::fstat(fd, stat_ptr) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_poll(pollfds_ptr: u64, nfds: u64, timeout_millis: i32) -> u64 {
    debug_log_secondary_linux_syscall(|| {
        alloc::format!(
            "poll pollfds={:#x} nfds={} timeout_ms={}",
            pollfds_ptr,
            nfds,
            timeout_millis
        )
    });
    match linux_ops::poll(pollfds_ptr, nfds, timeout_millis) {
        Ok(ready) => ready,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_ppoll(
    pollfds_ptr: u64,
    nfds: u64,
    timeout_ptr: u64,
    sigmask_ptr: u64,
    sigset_size: u64,
) -> u64 {
    match linux_ops::ppoll(pollfds_ptr, nfds, timeout_ptr, sigmask_ptr, sigset_size) {
        Ok(ready) => ready,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_epoll_create1(flags: u64) -> u64 {
    match linux_ops::epoll_create1(flags) {
        Ok(fd) => fd,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_epoll_ctl(epfd: u64, op: u64, fd: u64, event_ptr: u64) -> u64 {
    match linux_ops::epoll_ctl(epfd, op, fd, event_ptr) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_epoll_wait(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    timeout_millis: i32,
) -> u64 {
    match linux_ops::epoll_wait(epfd, events_ptr, maxevents, timeout_millis) {
        Ok(ready) => ready,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_epoll_pwait(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    timeout_millis: i32,
    sigmask_ptr: u64,
    sigset_size: u64,
) -> u64 {
    match linux_ops::epoll_pwait(
        epfd,
        events_ptr,
        maxevents,
        timeout_millis,
        sigmask_ptr,
        sigset_size,
    ) {
        Ok(ready) => ready,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_dup(fd: u64) -> u64 {
    if offload_ops::current_process_may_bootstrap_policy_service() {
        return match linux_ops::dup(fd) {
            Ok(new_fd) => new_fd,
            Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
        };
    }
    match offload_ops::call_vfs_dup_fd(fd, 0, 0, offload_ops::VFSD_DUP_MODE_DUP) {
        Ok(new_fd) => new_fd,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_dup2(oldfd: u64, newfd: u64) -> u64 {
    if offload_ops::current_process_may_bootstrap_policy_service() {
        return match linux_ops::dup2(oldfd, newfd) {
            Ok(fd) => fd,
            Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
        };
    }
    match offload_ops::call_vfs_dup_fd(oldfd, newfd, 0, offload_ops::VFSD_DUP_MODE_DUP2) {
        Ok(fd) => fd,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    match offload_ops::call_remote_vfs_lseek(fd, offset, whence) {
        Ok(Some(position)) => return position,
        Ok(None) => {}
        Err(errno) => return linux_errno(errno),
    }
    match linux_ops::lseek(fd, offset, whence) {
        Ok(position) => position,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_writev(fd: u64, iov_ptr: u64, iov_count: u64) -> u64 {
    debug_log_secondary_linux_syscall(|| {
        alloc::format!("writev fd={} iov={:#x} iovcnt={}", fd, iov_ptr, iov_count)
    });
    match linux_ops::writev(fd, iov_ptr, iov_count) {
        Ok(written) => written as u64,
        Err(err) => {
            debug::println!(
                "linux writev rejected: fd={} iov_ptr={:#x} iov_count={} err={:?}",
                fd,
                iov_ptr,
                iov_count,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_mount(
    source_ptr: u64,
    target_ptr: u64,
    fstype_ptr: u64,
    flags: u64,
    data_ptr: u64,
) -> u64 {
    if source_ptr == 0 || target_ptr == 0 || fstype_ptr == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(err) = usermem::read_current_user_c_string(source_ptr, 256) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = usermem::read_current_user_c_string(fstype_ptr, 64) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if data_ptr != 0 {
        if let Err(err) = usermem::read_current_user_c_string(data_ptr, 256) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    let target_path = match linux_ops::resolve_readlinkat_absolute_path_for_current_process(
        linux_abi::AT_FDCWD as u64,
        target_ptr,
    ) {
        Ok(path) => path,
        Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
    };
    match offload_ops::call_vfs_mount(source_ptr, &target_path, fstype_ptr, flags, data_ptr) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_openat(dirfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    if offload_ops::current_process_may_bootstrap_policy_service() {
        return match linux_ops::openat(dirfd, path_ptr, flags, mode) {
            Ok(fd) => fd,
            Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
        };
    }
    let absolute_path =
        match linux_ops::resolve_readlinkat_absolute_path_for_current_process(dirfd, path_ptr) {
            Ok(path) => path,
            Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
        };
    if absolute_path.starts_with("/dev/") {
        return match linux_ops::openat(dirfd, path_ptr, flags, mode) {
            Ok(fd) => fd,
            Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
        };
    }
    let mode = u32::try_from(mode).unwrap_or(u32::MAX);
    match offload_ops::call_vfs_openat_with_fd(dirfd, flags, mode, absolute_path.as_str()) {
        Ok(fd) => fd,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_unlinkat(dirfd: u64, path_ptr: u64, flags: u64) -> u64 {
    if let Err(errno) =
        vfs_path_policy(SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT, dirfd, path_ptr, flags, 0)
    {
        return linux_errno(errno);
    }
    match linux_ops::unlinkat(dirfd, path_ptr, flags) {
        Ok(()) => 0,
        Err(err) => {
            if !matches!(err, linux_ops::LinuxSysopError::NotFound) {
                debug::println!(
                    "linux unlinkat rejected: dirfd={} path_ptr={:#x} path={} flags={:#x} err={:?}",
                    dirfd,
                    path_ptr,
                    debug_user_path(path_ptr),
                    flags,
                    err,
                );
            }
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_getdents64(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if offload_ops::current_process_may_bootstrap_policy_service() {
        return match linux_ops::getdents64(fd, user_ptr, user_len) {
            Ok(read) => read as u64,
            Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
        };
    }
    match offload_ops::call_vfs_getdents64(fd, user_ptr, user_len) {
        Ok(read) => read,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_umount2(target_ptr: u64, flags: u64) -> u64 {
    let target_path = match linux_ops::resolve_readlinkat_absolute_path_for_current_process(
        linux_abi::AT_FDCWD as u64,
        target_ptr,
    ) {
        Ok(path) => path,
        Err(err) => return linux_errno(linux_sysop_error_to_errno(err)),
    };
    match offload_ops::call_vfs_umount2(&target_path, flags) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    if offload_ops::current_process_may_bootstrap_policy_service() {
        return match linux_ops::fcntl(fd, cmd, arg) {
            Ok(result) => result,
            Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
        };
    }
    match offload_ops::call_vfs_fcntl(fd, cmd, arg) {
        Ok(result) => result,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_pread64(fd: u64, user_ptr: u64, user_len: u64, offset: u64) -> u64 {
    match offload_ops::call_remote_vfs_pread64(fd, user_ptr, user_len, offset) {
        Ok(Some(read)) => return read,
        Ok(None) => {}
        Err(errno) => return linux_errno(errno),
    }
    match linux_ops::pread64(fd, user_ptr, user_len, offset) {
        Ok(read) => read as u64,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_ioctl(fd: u64, request: u64, arg: u64) -> u64 {
    match offload_ops::call_device_ioctl(fd, request, arg) {
        Ok(value) => value,
        Err(errno) => {
            debug::println!(
                "linux ioctl rejected: fd={} request={:#x} arg={:#x} errno={}",
                fd,
                request,
                arg,
                errno,
            );
            linux_errno(errno)
        }
    }
}

pub(super) fn syscall_linux_dup3(oldfd: u64, newfd: u64, flags: u64) -> u64 {
    if offload_ops::current_process_may_bootstrap_policy_service() {
        return match linux_ops::dup3(oldfd, newfd, flags) {
            Ok(fd) => fd,
            Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
        };
    }
    match offload_ops::call_vfs_dup_fd(oldfd, newfd, flags, offload_ops::VFSD_DUP_MODE_DUP3) {
        Ok(fd) => fd,
        Err(errno) => linux_errno(errno),
    }
}

fn vfs_path_policy(op: u16, dirfd: u64, path_ptr: u64, flags: u64, arg0: u32) -> Result<(), i64> {
    let absolute_path =
        linux_ops::resolve_readlinkat_absolute_path_for_current_process(dirfd, path_ptr)
            .map_err(linux_sysop_error_to_errno)?;
    offload_ops::call_vfs_path_policy(op, dirfd, flags, arg0, absolute_path.as_str())
}
