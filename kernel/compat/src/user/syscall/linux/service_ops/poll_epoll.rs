use super::*;

pub fn syscall_linux_poll(fds_ptr: u64, nfds: u64, timeout_ms: i64) -> u64 {
    let start_tick = crate::arch::rtc::ticks();
    let ticks_per_second = crate::arch::rtc::ticks_per_second();
    let timeout_ticks = if timeout_ms < 0 {
        None
    } else {
        Some(
            (timeout_ms as u64)
                .saturating_mul(ticks_per_second)
                .saturating_add(999)
                / 1000,
        )
    };
    loop {
        let result = syscall_linux_poll_once(fds_ptr, nfds);
        if is_linux_error(result) || result != 0 || timeout_ms == 0 {
            return result;
        }
        if let Some(timeout_ticks) = timeout_ticks {
            let elapsed = crate::arch::rtc::ticks().saturating_sub(start_tick);
            if elapsed >= timeout_ticks {
                return 0;
            }
        }
        crate::arch::rtc::sleep(1);
    }
}

fn syscall_linux_poll_once(fds_ptr: u64, nfds: u64) -> u64 {
    const POLLFD_SIZE: u64 = 8;
    let Ok(nfds_usize) = usize::try_from(nfds) else {
        return linux_errno(LINUX_EINVAL);
    };
    if nfds_usize > 1024 {
        return linux_errno(LINUX_EINVAL);
    }
    let mut ready = 0_u64;
    for i in 0..nfds {
        let Some(entry_ptr) = fds_ptr.checked_add(i.saturating_mul(POLLFD_SIZE)) else {
            return linux_errno(LINUX_EFAULT);
        };
        let mut entry = [0_u8; 8];
        if let Err(err) = usermem::copy_from_current_user_exact(entry_ptr, &mut entry) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let fd = i32::from_le_bytes(entry[0..4].try_into().unwrap());
        let events = u16::from_le_bytes(entry[4..6].try_into().unwrap());
        let revents: u16 = if fd < 0 {
            0
        } else if fd <= 2 {
            let ready_bits = events & (linux_abi::POLLIN | linux_abi::POLLOUT) as u16;
            if ready_bits != 0 {
                ready += 1;
            }
            ready_bits
        } else {
            let handle = multitask::with_current_user_process_state(|_, _, ps| {
                ps.handles().get(fd as u64).cloned()
            })
            .flatten();
            if let Some(handle) = handle {
                let ready_bits = kernel_handle_poll_revents(&handle, events as u32) as u16;
                if ready_bits != 0 {
                    ready += 1;
                }
                ready_bits
            } else {
                ready += 1;
                linux_abi::POLLNVAL as u16
            }
        };
        entry[6..8].copy_from_slice(&revents.to_le_bytes());
        if let Err(err) = usermem::write_current_user_bytes(entry_ptr, &entry) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    ready
}

pub fn syscall_linux_ppoll(
    fds_ptr: u64,
    nfds: u64,
    timeout_ptr: u64,
    _sigmask_ptr: u64,
) -> u64 {
    let timeout_ms = if timeout_ptr == 0 {
        -1
    } else {
        let ts = match usermem::read_current_user_struct::<LinuxTimespecWire>(timeout_ptr) {
            Ok(ts) => ts,
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
        if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
            return linux_errno(LINUX_EINVAL);
        }
        ts.tv_sec
            .saturating_mul(1000)
            .saturating_add((ts.tv_nsec + 999_999) / 1_000_000)
    };
    syscall_linux_poll(fds_ptr, nfds, timeout_ms)
}

pub fn syscall_linux_epoll_create1(flags: u64) -> u64 {
    if flags & !linux_abi::EPOLL_CLOEXEC != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let fd_flags = if flags & linux_abi::EPOLL_CLOEXEC != 0 {
        multitask::FD_CLOEXEC
    } else {
        0
    };
    let handle = multitask::KernelHandle::Epoll(multitask::EpollHandle::new());
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_entry(multitask::HandleEntry::new(
                handle,
                fd_flags,
                linux_abi::O_RDONLY,
            ))
    }) {
        Some(fd) => fd,
        None => linux_errno(LINUX_ENOSYS),
    }
}

pub fn syscall_linux_epoll_ctl(epfd: u64, op: u64, fd: u64, event_ptr: u64) -> u64 {
    if epfd == fd {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(epoll) = current_epoll_handle(epfd) else {
        return linux_errno(LINUX_EINVAL);
    };
    match op {
        linux_abi::EPOLL_CTL_ADD | linux_abi::EPOLL_CTL_MOD => {
            if event_ptr == 0 {
                return linux_errno(LINUX_EINVAL);
            }
            let Some(handle) = current_kernel_handle(fd) else {
                return linux_errno(LINUX_EBADF);
            };
            if matches!(handle, multitask::KernelHandle::Epoll(_)) {
                return linux_errno(LINUX_EOPNOTSUPP);
            }
            let (events, data) = match read_linux_epoll_event(event_ptr) {
                Ok(event) => event,
                Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
            };
            let result = if op == linux_abi::EPOLL_CTL_ADD {
                epoll.add(fd, handle, events, data)
            } else {
                epoll.modify(fd, handle, events, data)
            };
            match result {
                Ok(()) => 0,
                Err(err) => linux_errno(epoll_error_to_linux_errno(err)),
            }
        }
        linux_abi::EPOLL_CTL_DEL => match epoll.delete(fd) {
            Ok(()) => 0,
            Err(err) => linux_errno(epoll_error_to_linux_errno(err)),
        },
        _ => linux_errno(LINUX_EINVAL),
    }
}

pub fn syscall_linux_epoll_wait(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    _timeout_ms: i64,
) -> u64 {
    let Ok(maxevents) = usize::try_from(maxevents) else {
        return linux_errno(LINUX_EINVAL);
    };
    if maxevents == 0 || maxevents > 1024 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(
        events_ptr,
        maxevents.saturating_mul(size_of::<linux_abi::LinuxEpollEvent>()),
    ) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let Some(epoll) = current_epoll_handle(epfd) else {
        return linux_errno(LINUX_EINVAL);
    };
    let mut written = 0usize;
    for interest in epoll.snapshot().into_iter().take(maxevents) {
        let requested = interest.events
            & (linux_abi::EPOLLIN
                | linux_abi::EPOLLPRI
                | linux_abi::EPOLLOUT
                | linux_abi::EPOLLERR
                | linux_abi::EPOLLHUP);
        let events = kernel_handle_poll_revents(&interest.handle, requested);
        if events == 0 {
            continue;
        }
        let Some(entry_ptr) =
            events_ptr.checked_add((written * size_of::<linux_abi::LinuxEpollEvent>()) as u64)
        else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = write_linux_epoll_event(entry_ptr, events, interest.data) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        written += 1;
    }
    written as u64
}

pub fn kernel_handle_poll_revents(handle: &multitask::KernelHandle, requested: u32) -> u32 {
    match handle {
        multitask::KernelHandle::Socket(socket) => {
            socket.poll_revents(requested as i16).max(0) as u32
        }
        _ => requested & (linux_abi::EPOLLIN | linux_abi::EPOLLOUT),
    }
}
