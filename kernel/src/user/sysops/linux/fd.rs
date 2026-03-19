use alloc::vec;

use super::*;

pub(crate) fn write(fd: u64, user_ptr: u64, user_len: u64) -> Result<usize, LinuxSysopError> {
    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Ok(0);
    }

    if is_console_output_fd(fd)? {
        return console::write_from_current_process(user_ptr, len).map_err(Into::into);
    }

    if let Some(written) = file::write_current_process_file(fd, user_ptr, user_len)? {
        return Ok(written);
    }

    Err(LinuxSysopError::BadFileDescriptor)
}

pub(crate) fn read(fd: u64, user_ptr: u64, user_len: u64) -> Result<usize, LinuxSysopError> {
    if is_console_input_fd(fd)? {
        let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        if len == 0 {
            return Ok(0);
        }

        return console::read_into_current_process(user_ptr, len).map_err(Into::into);
    }

    if let Some(read) = file::read_current_process_file(fd, user_ptr, user_len)? {
        return Ok(read);
    }

    device::read_current_process_handle(fd, user_ptr, user_len).map_err(Into::into)
}

pub(crate) fn writev(fd: u64, iov_ptr: u64, iov_count: u64) -> Result<usize, LinuxSysopError> {
    let iov_count = usize::try_from(iov_count).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if iov_count > MAX_IOV_COUNT {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if iov_count == 0 {
        return Ok(0);
    }

    let mut total_written = 0usize;
    for index in 0..iov_count {
        let iovec_ptr = iov_ptr
            .checked_add((index * size_of::<linux_abi::LinuxIovec>()) as u64)
            .ok_or(LinuxSysopError::InvalidArgument)?;
        let mut iovec = linux_abi::LinuxIovec::default();
        let iovec_bytes = unsafe {
            slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(iovec).cast::<u8>(),
                size_of::<linux_abi::LinuxIovec>(),
            )
        };
        usermem::copy_from_current_user_exact(iovec_ptr, iovec_bytes)?;
        if iovec.iov_len == 0 {
            continue;
        }

        let written = write(fd, iovec.iov_base, iovec.iov_len)?;
        total_written = total_written
            .checked_add(written)
            .ok_or(LinuxSysopError::InvalidArgument)?;
        if written < usize::try_from(iovec.iov_len).map_err(|_| LinuxSysopError::InvalidArgument)? {
            break;
        }
    }

    Ok(total_written)
}

pub(crate) fn poll(
    pollfds_ptr: u64,
    nfds: u64,
    timeout_millis: i32,
) -> Result<u64, LinuxSysopError> {
    let nfds = usize::try_from(nfds).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if nfds > MAX_POLL_FDS {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if nfds == 0 {
        if timeout_millis > 0 {
            rtc::sleep(timeout_millis as u64);
        }
        return Ok(0);
    }

    let bytes_len = nfds
        .checked_mul(size_of::<linux_abi::LinuxPollFd>())
        .ok_or(LinuxSysopError::InvalidArgument)?;
    let mut pollfds = vec![linux_abi::LinuxPollFd::default(); nfds];
    let pollfd_bytes =
        unsafe { slice::from_raw_parts_mut(pollfds.as_mut_ptr().cast::<u8>(), bytes_len) };
    usermem::copy_from_current_user_exact(pollfds_ptr, pollfd_bytes)?;

    let deadline_tick = if timeout_millis < 0 {
        None
    } else {
        Some(rtc::ticks().saturating_add(
            ((timeout_millis as u64).saturating_mul(rtc::ticks_per_second().max(1)) + 999) / 1000,
        ))
    };

    loop {
        let ready = update_pollfd_revents(&mut pollfds)?;
        let pollfd_bytes =
            unsafe { slice::from_raw_parts(pollfds.as_ptr().cast::<u8>(), bytes_len) };
        usermem::write_current_user_bytes(pollfds_ptr, pollfd_bytes)?;

        if ready != 0 || timeout_millis == 0 {
            return Ok(ready as u64);
        }

        if let Some(deadline_tick) = deadline_tick {
            if rtc::ticks() >= deadline_tick {
                return Ok(0);
            }
        }

        rtc::sleep(1);
    }
}

pub(crate) fn close(fd: u64) -> Result<(), LinuxSysopError> {
    if fd <= 2 {
        return Ok(());
    }

    device::close_current_process_handle(fd).map_err(Into::into)
}

pub(crate) fn dup(fd: u64) -> Result<u64, LinuxSysopError> {
    duplicate_fd(fd, FIRST_DYNAMIC_FD as u64, false)
}

pub(crate) fn dup2(oldfd: u64, newfd: u64) -> Result<u64, LinuxSysopError> {
    duplicate_fd_to(oldfd, newfd, false, false)
}

pub(crate) fn dup3(oldfd: u64, newfd: u64, flags: u64) -> Result<u64, LinuxSysopError> {
    if flags & !linux_abi::O_CLOEXEC != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    duplicate_fd_to(oldfd, newfd, flags & linux_abi::O_CLOEXEC != 0, true)
}

pub(crate) fn openat(
    dirfd: u64,
    path_ptr: u64,
    flags: u64,
    mode: u64,
) -> Result<u64, LinuxSysopError> {
    let path = usermem::read_current_user_c_string(path_ptr, 128)?;
    let absolute_path = file::resolve_path_for_current_process(dirfd, &path)?;
    file::open_path_for_current_process(absolute_path.as_str(), flags, mode).map_err(Into::into)
}

pub(crate) fn fcntl(fd: u64, cmd: u64, arg: u64) -> Result<u64, LinuxSysopError> {
    match cmd {
        linux_abi::F_DUPFD => duplicate_fd(fd, arg, false),
        linux_abi::F_DUPFD_CLOEXEC => duplicate_fd(fd, arg, true),
        linux_abi::F_GETFD => Ok(get_fd_flags(fd)? as u64),
        linux_abi::F_SETFD => {
            set_fd_flags(fd, (arg as u32) & FD_CLOEXEC)?;
            Ok(0)
        }
        linux_abi::F_GETFL => Ok(get_status_flags(fd)?),
        linux_abi::F_SETFL => {
            set_status_flags(fd, arg)?;
            Ok(0)
        }
        _ => Err(LinuxSysopError::Unsupported),
    }
}

fn duplicate_fd(fd: u64, min_newfd: u64, close_on_exec: bool) -> Result<u64, LinuxSysopError> {
    if fd < 3 {
        let handle = console_stream_handle_for_fd(fd)?;
        let status_flags = console_stream_status_flags(fd)?;
        let fd_flags = if close_on_exec { FD_CLOEXEC } else { 0 };
        let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
            Ok(process_state.handles_mut().install_entry_min(
                crate::user::handles::HandleEntry::new(handle, fd_flags, status_flags),
                min_newfd,
            ))
        }) else {
            return Err(LinuxSysopError::Unsupported);
        };
        return result;
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .duplicate_min(fd, min_newfd, close_on_exec)
            .ok_or(LinuxSysopError::BadFileDescriptor)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn duplicate_fd_to(
    oldfd: u64,
    newfd: u64,
    close_on_exec: bool,
    reject_same_fd: bool,
) -> Result<u64, LinuxSysopError> {
    if oldfd == newfd {
        return if reject_same_fd {
            Err(LinuxSysopError::InvalidArgument)
        } else {
            Ok(newfd)
        };
    }
    if newfd < 3 {
        return Err(LinuxSysopError::Unsupported);
    }

    if oldfd < 3 {
        let handle = console_stream_handle_for_fd(oldfd)?;
        let status_flags = console_stream_status_flags(oldfd)?;
        let fd_flags = if close_on_exec { FD_CLOEXEC } else { 0 };
        let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
            process_state.handles_mut().close(newfd);
            let Some(index) = newfd
                .checked_sub(FIRST_DYNAMIC_FD as u64)
                .and_then(|value| usize::try_from(value).ok())
            else {
                return Err(LinuxSysopError::InvalidArgument);
            };
            process_state.handles_mut().ensure_entry_capacity(index);
            process_state
                .handles_mut()
                .replace_entry(
                    newfd,
                    Some(HandleEntry::new(handle, fd_flags, status_flags)),
                )
                .ok_or(LinuxSysopError::InvalidArgument)?;
            Ok(newfd)
        }) else {
            return Err(LinuxSysopError::Unsupported);
        };
        return result;
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().close(newfd);
        process_state
            .handles_mut()
            .duplicate_exact(oldfd, newfd, close_on_exec)
            .ok_or(LinuxSysopError::BadFileDescriptor)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn get_fd_flags(fd: u64) -> Result<u32, LinuxSysopError> {
    if fd < 3 {
        return Ok(0);
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles()
            .get_entry(fd)
            .map(|entry| entry.fd_flags())
            .ok_or(LinuxSysopError::BadFileDescriptor)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn set_fd_flags(fd: u64, flags: u32) -> Result<(), LinuxSysopError> {
    if fd < 3 {
        return Ok(());
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let Some(entry) = process_state.handles_mut().get_entry_mut(fd) else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };
        entry.set_fd_flags(flags);
        Ok(())
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn get_status_flags(fd: u64) -> Result<u64, LinuxSysopError> {
    if fd < 3 {
        return console_stream_status_flags(fd);
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles()
            .get_entry(fd)
            .map(|entry| entry.status_flags())
            .ok_or(LinuxSysopError::BadFileDescriptor)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn set_status_flags(fd: u64, flags: u64) -> Result<(), LinuxSysopError> {
    if fd < 3 {
        return Ok(());
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let Some(entry) = process_state.handles_mut().get_entry_mut(fd) else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };
        entry.set_status_flags(flags);
        Ok(())
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(super) fn current_handle(fd: u64) -> Result<KernelHandle, LinuxSysopError> {
    if fd < 3 {
        return console_stream_handle_for_fd(fd);
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles()
            .get(fd)
            .cloned()
            .ok_or(LinuxSysopError::BadFileDescriptor)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn is_console_input_fd(fd: u64) -> Result<bool, LinuxSysopError> {
    Ok(matches!(
        current_handle(fd)?,
        KernelHandle::Console(ConsoleStreamKind::Input)
    ))
}

fn is_console_output_fd(fd: u64) -> Result<bool, LinuxSysopError> {
    Ok(matches!(
        current_handle(fd)?,
        KernelHandle::Console(ConsoleStreamKind::Output | ConsoleStreamKind::Error)
    ))
}

fn console_stream_handle_for_fd(fd: u64) -> Result<KernelHandle, LinuxSysopError> {
    match fd {
        0 => Ok(KernelHandle::Console(ConsoleStreamKind::Input)),
        1 => Ok(KernelHandle::Console(ConsoleStreamKind::Output)),
        2 => Ok(KernelHandle::Console(ConsoleStreamKind::Error)),
        _ => Err(LinuxSysopError::BadFileDescriptor),
    }
}

fn console_stream_status_flags(fd: u64) -> Result<u64, LinuxSysopError> {
    match fd {
        0 => Ok(linux_abi::O_RDONLY),
        1 | 2 => Ok(linux_abi::O_WRONLY),
        _ => Err(LinuxSysopError::BadFileDescriptor),
    }
}

fn update_pollfd_revents(pollfds: &mut [linux_abi::LinuxPollFd]) -> Result<usize, LinuxSysopError> {
    let mut ready_count = 0usize;
    for pollfd in pollfds.iter_mut() {
        let revents = if pollfd.fd < 0 {
            0
        } else {
            poll_revents_for_fd(pollfd.fd as u64, pollfd.events)?
        };
        pollfd.revents = revents;
        if revents != 0 {
            ready_count += 1;
        }
    }
    Ok(ready_count)
}

fn poll_revents_for_fd(fd: u64, requested: i16) -> Result<i16, LinuxSysopError> {
    let console_session = multitask::current_console_session();
    if fd == 0 {
        let mut ready = 0_i16;
        if tty::has_pending_input_for_session(console_session) {
            ready |= requested & (linux_abi::POLLIN | linux_abi::POLLPRI);
        }
        return Ok(ready);
    }
    if matches!(fd, 1 | 2) {
        return Ok(requested & linux_abi::POLLOUT);
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let Some(entry) = process_state.handles().get_entry(fd) else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };
        Ok(poll_revents_for_handle(
            entry.handle().clone(),
            requested,
            console_session,
        ))
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    match result {
        Ok(revents) => Ok(revents),
        Err(LinuxSysopError::BadFileDescriptor) => Ok(linux_abi::POLLNVAL),
        Err(err) => Err(err),
    }
}

fn poll_revents_for_handle(
    handle: KernelHandle,
    requested: i16,
    console_session: crate::session::ConsoleSessionId,
) -> i16 {
    match handle {
        KernelHandle::Console(ConsoleStreamKind::Input) => {
            if tty::has_pending_input_for_session(console_session) {
                requested & (linux_abi::POLLIN | linux_abi::POLLPRI)
            } else {
                0
            }
        }
        KernelHandle::Console(ConsoleStreamKind::Output | ConsoleStreamKind::Error) => {
            requested & linux_abi::POLLOUT
        }
        KernelHandle::VfsFile(_) | KernelHandle::VfsDirectory(_) => {
            requested & (linux_abi::POLLIN | linux_abi::POLLOUT)
        }
        KernelHandle::DisplaySurface(_) => requested & linux_abi::POLLOUT,
        KernelHandle::Device(handle) => match handle.device_id() {
            device_ns::DeviceId::Input => {
                if event_queue::has_pending_input_events() {
                    requested & (linux_abi::POLLIN | linux_abi::POLLPRI)
                } else {
                    0
                }
            }
            device_ns::DeviceId::Console
            | device_ns::DeviceId::Display
            | device_ns::DeviceId::Runtime => requested & linux_abi::POLLOUT,
        },
    }
}
