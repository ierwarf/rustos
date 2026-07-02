// RING3-MIGRATION-REFERENCE START: vfsd/netd should own poll/epoll readiness
// policy. Ring0 keeps fd validation, epoll token handles, user-copy, and bounded
// timeout sleep substrate.
use super::*;

const MAX_POLL_FDS: usize = 1024;
const POLLFD_SIZE: usize = size_of::<linux_abi::LinuxPollFd>();
const EPOLL_EVENT_SIZE: usize = size_of::<linux_abi::LinuxEpollEvent>();

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
    let Ok(nfds_usize) = usize::try_from(nfds) else {
        return linux_errno(LINUX_EINVAL);
    };
    if nfds_usize > MAX_POLL_FDS {
        return linux_errno(LINUX_EINVAL);
    }

    let max_chunk = VFS_IPC_REQUEST_PAYLOAD_CAPACITY / POLLFD_SIZE;
    let mut ready = 0_u64;
    let mut index = 0usize;
    while index < nfds_usize {
        let count = (nfds_usize - index).min(max_chunk);
        let len = count * POLLFD_SIZE;
        let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
        request.arg0 = VFS_POLL_QUERY_POLL;
        request.payload_len = len as u32;
        let mut originals: Vec<[u8; POLLFD_SIZE]> = Vec::new();
        let mut invalid: Vec<bool> = Vec::new();

        for slot in 0..count {
            let Some(entry_ptr) = fds_ptr.checked_add(((index + slot) * POLLFD_SIZE) as u64) else {
                return linux_errno(LINUX_EFAULT);
            };
            let mut entry = [0_u8; POLLFD_SIZE];
            if let Err(err) = usermem::copy_from_current_user_exact(entry_ptr, &mut entry) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            let fd = i32::from_le_bytes(entry[0..4].try_into().unwrap_or([0; 4]));
            let bad_fd = fd >= 0 && current_kernel_handle(fd as u64).is_none();
            if bad_fd {
                request.payload[slot * POLLFD_SIZE..slot * POLLFD_SIZE + 4]
                    .copy_from_slice(&(-1_i32).to_le_bytes());
                request.payload[slot * POLLFD_SIZE + 4..slot * POLLFD_SIZE + POLLFD_SIZE]
                    .copy_from_slice(&entry[4..POLLFD_SIZE]);
            } else {
                request.payload[slot * POLLFD_SIZE..slot * POLLFD_SIZE + POLLFD_SIZE]
                    .copy_from_slice(&entry);
            }
            originals.push(entry);
            invalid.push(bad_fd);
        }

        let response = match call_vfs_ipc_request(&request) {
            Ok(response) => response,
            Err(errno) => return linux_errno(errno),
        };
        if let Err(errno) = ensure_vfs_status(&response) {
            return linux_errno(errno);
        }
        if response.payload_len as usize != len {
            return linux_errno(LINUX_EINVAL);
        }

        for slot in 0..count {
            let offset = slot * POLLFD_SIZE;
            let revents = if invalid[slot] {
                linux_abi::POLLNVAL as i16
            } else {
                i16::from_le_bytes(
                    response.payload[offset + 6..offset + 8]
                        .try_into()
                        .unwrap_or([0; 2]),
                )
            };
            if revents != 0 {
                ready += 1;
            }
            let mut entry = originals[slot];
            entry[6..8].copy_from_slice(&revents.to_le_bytes());
            let Some(entry_ptr) = fds_ptr.checked_add(((index + slot) * POLLFD_SIZE) as u64) else {
                return linux_errno(LINUX_EFAULT);
            };
            if let Err(err) = usermem::write_current_user_bytes(entry_ptr, &entry) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
        }

        index += count;
    }
    ready
}

pub fn syscall_linux_ppoll(fds_ptr: u64, nfds: u64, timeout_ptr: u64, _sigmask_ptr: u64) -> u64 {
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
    let epoll = multitask::EpollHandle::new();
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = VFS_POLL_QUERY_EPOLL_CREATE;
    request.remote_id = epoll.token_id();
    if let Err(errno) =
        call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response))
    {
        return linux_errno(errno);
    }
    let fd_flags = if flags & linux_abi::EPOLL_CLOEXEC != 0 {
        multitask::FD_CLOEXEC
    } else {
        0
    };
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_entry(multitask::HandleEntry::new(
                multitask::KernelHandle::Epoll(epoll),
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
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = VFS_POLL_QUERY_EPOLL_CTL;
    request.arg1 = op;
    request.fd = fd;
    request.remote_id = epoll.token_id();

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
            request.payload_len = EPOLL_EVENT_SIZE as u32;
            request.payload[0..4].copy_from_slice(&events.to_le_bytes());
            request.payload[4..12].copy_from_slice(&data.to_le_bytes());
        }
        linux_abi::EPOLL_CTL_DEL => {}
        _ => return linux_errno(LINUX_EINVAL),
    }

    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
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
    if maxevents == 0 || maxevents > MAX_POLL_FDS {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(err) =
        usermem::validate_current_user_write_buffer(events_ptr, maxevents * EPOLL_EVENT_SIZE)
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let Some(epoll) = current_epoll_handle(epfd) else {
        return linux_errno(LINUX_EINVAL);
    };
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = VFS_POLL_QUERY_EPOLL_WAIT;
    request.arg1 = maxevents as u64;
    request.remote_id = epoll.token_id();
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let written = response.value as usize;
    if written > maxevents || response.payload_len as usize != written * EPOLL_EVENT_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    for slot in 0..written {
        let offset = slot * EPOLL_EVENT_SIZE;
        let events = u32::from_le_bytes(
            response.payload[offset..offset + 4]
                .try_into()
                .unwrap_or([0; 4]),
        );
        let data = u64::from_le_bytes(
            response.payload[offset + 4..offset + 12]
                .try_into()
                .unwrap_or([0; 8]),
        );
        let Some(entry_ptr) = events_ptr.checked_add((slot * EPOLL_EVENT_SIZE) as u64) else {
            return linux_errno(LINUX_EFAULT);
        };
        if let Err(err) = write_linux_epoll_event(entry_ptr, events, data) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    written as u64
}
// RING3-MIGRATION-REFERENCE END: vfsd/netd-owned poll/epoll policy.
