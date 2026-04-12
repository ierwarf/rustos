use super::*;

pub(super) fn syscall_linux_write(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    debug_log_secondary_linux_syscall(|| {
        alloc::format!("write fd={} ptr={:#x} len={}", fd, user_ptr, user_len)
    });
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
    match linux_ops::close(fd) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_ftruncate(fd: u64, len: u64) -> u64 {
    match linux_ops::ftruncate(fd, len) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_fstat(fd: u64, stat_ptr: u64) -> u64 {
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
    match linux_ops::dup(fd) {
        Ok(new_fd) => new_fd,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_dup2(oldfd: u64, newfd: u64) -> u64 {
    match linux_ops::dup2(oldfd, newfd) {
        Ok(fd) => fd,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
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

pub(super) fn syscall_linux_access(path_ptr: u64, mode: u64) -> u64 {
    match linux_ops::access(path_ptr, mode) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux access rejected: path_ptr={:#x} path={} mode={:#x} err={:?}",
                path_ptr,
                debug_user_path(path_ptr),
                mode,
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
    match linux_ops::mount(source_ptr, target_ptr, fstype_ptr, flags, data_ptr) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux mount rejected: source_ptr={:#x} target_ptr={:#x} fstype_ptr={:#x} flags={:#x} data_ptr={:#x} err={:?}",
                source_ptr,
                target_ptr,
                fstype_ptr,
                flags,
                data_ptr,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_chdir(path_ptr: u64) -> u64 {
    match linux_ops::chdir(path_ptr) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_mkdir(path_ptr: u64, mode: u64) -> u64 {
    match linux_ops::mkdir(path_ptr, mode) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_openat(dirfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    match linux_ops::openat(dirfd, path_ptr, flags, mode) {
        Ok(fd) => fd,
        Err(err) => {
            debug::println!(
                "linux openat rejected: dirfd={} path_ptr={:#x} path={} flags={:#x} mode={:#x} err={:?}",
                dirfd,
                path_ptr,
                debug_user_path(path_ptr),
                flags,
                mode,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_unlinkat(dirfd: u64, path_ptr: u64, flags: u64) -> u64 {
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
    match linux_ops::getdents64(fd, user_ptr, user_len) {
        Ok(read) => read as u64,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_getcwd(user_ptr: u64, user_len: u64) -> u64 {
    match linux_ops::getcwd(user_ptr, user_len) {
        Ok(read) => read as u64,
        Err(err) => {
            debug::println!(
                "linux getcwd rejected: user_ptr={:#x} len={} err={:?}",
                user_ptr,
                user_len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_umount2(target_ptr: u64, flags: u64) -> u64 {
    match linux_ops::umount2(target_ptr, flags) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    match linux_ops::fcntl(fd, cmd, arg) {
        Ok(result) => result,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_pread64(fd: u64, user_ptr: u64, user_len: u64, offset: u64) -> u64 {
    match linux_ops::pread64(fd, user_ptr, user_len, offset) {
        Ok(read) => read as u64,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_newfstatat(
    dirfd: u64,
    path_ptr: u64,
    stat_ptr: u64,
    flags: u64,
) -> u64 {
    match linux_ops::newfstatat(dirfd, path_ptr, stat_ptr, flags) {
        Ok(()) => 0,
        Err(err) => {
            if !matches!(err, linux_ops::LinuxSysopError::NotFound) {
                debug::println!(
                    "linux newfstatat rejected: dirfd={} path_ptr={:#x} path={} stat_ptr={:#x} flags={:#x} err={:?}",
                    dirfd,
                    path_ptr,
                    debug_user_path(path_ptr),
                    stat_ptr,
                    flags,
                    err,
                );
            }
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_readlink(path_ptr: u64, user_ptr: u64, user_len: u64) -> u64 {
    match linux_ops::readlink(path_ptr, user_ptr, user_len) {
        Ok(read) => read as u64,
        Err(err) => {
            debug::println!(
                "linux readlink rejected: path_ptr={:#x} path={} user_ptr={:#x} len={} err={:?}",
                path_ptr,
                debug_user_path(path_ptr),
                user_ptr,
                user_len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_readlinkat(
    dirfd: u64,
    path_ptr: u64,
    user_ptr: u64,
    user_len: u64,
) -> u64 {
    match linux_ops::readlinkat(dirfd, path_ptr, user_ptr, user_len) {
        Ok(read) => read as u64,
        Err(err) => {
            debug::println!(
                "linux readlinkat rejected: dirfd={} path_ptr={:#x} path={} user_ptr={:#x} len={} err={:?}",
                dirfd,
                path_ptr,
                debug_user_path(path_ptr),
                user_ptr,
                user_len,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_faccessat(dirfd: u64, path_ptr: u64, mode: u64, flags: u64) -> u64 {
    match linux_ops::faccessat(dirfd, path_ptr, mode, flags) {
        Ok(()) => 0,
        Err(err) => {
            debug::println!(
                "linux faccessat rejected: dirfd={} path_ptr={:#x} path={} mode={:#x} flags={:#x} err={:?}",
                dirfd,
                path_ptr,
                debug_user_path(path_ptr),
                mode,
                flags,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_ioctl(fd: u64, request: u64, arg: u64) -> u64 {
    match linux_ops::ioctl(fd, request, arg) {
        Ok(value) => value,
        Err(err) => {
            debug::println!(
                "linux ioctl rejected: fd={} request={:#x} arg={:#x} err={:?}",
                fd,
                request,
                arg,
                err,
            );
            linux_errno(linux_sysop_error_to_errno(err))
        }
    }
}

pub(super) fn syscall_linux_dup3(oldfd: u64, newfd: u64, flags: u64) -> u64 {
    match linux_ops::dup3(oldfd, newfd, flags) {
        Ok(fd) => fd,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}
