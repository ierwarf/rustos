use super::*;

const MAX_POLL_FDS: usize = 1024;
const POLLFD_SIZE: usize = size_of::<linux_abi::LinuxPollFd>();
const EPOLL_EVENT_SIZE: usize = size_of::<linux_abi::LinuxEpollEvent>();

fn decode_pollfd(bytes: &[u8]) -> Result<(i32, u32), i64> {
    let fd_bytes: [u8; 4] = bytes
        .get(0..4)
        .ok_or(LINUX_EINVAL)?
        .try_into()
        .map_err(|_| LINUX_EINVAL)?;
    let event_bytes: [u8; 2] = bytes
        .get(4..6)
        .ok_or(LINUX_EINVAL)?
        .try_into()
        .map_err(|_| LINUX_EINVAL)?;
    Ok((
        i32::from_le_bytes(fd_bytes),
        i16::from_le_bytes(event_bytes) as u32,
    ))
}

fn decode_epoll_event(bytes: &[u8]) -> Result<(u32, u64), i64> {
    let event_bytes: [u8; 4] = bytes
        .get(0..4)
        .ok_or(LINUX_EINVAL)?
        .try_into()
        .map_err(|_| LINUX_EINVAL)?;
    let data_bytes: [u8; 8] = bytes
        .get(4..12)
        .ok_or(LINUX_EINVAL)?
        .try_into()
        .map_err(|_| LINUX_EINVAL)?;
    Ok((
        u32::from_le_bytes(event_bytes),
        u64::from_le_bytes(data_bytes),
    ))
}

pub fn syscall_linux_poll(fds_ptr: u64, nfds: u64, timeout_ms: i64) -> u64 {
    let start_tick = crate::arch::rtc::ticks();
    let ticks_per_second = crate::arch::rtc::ticks_per_second();
    let can_block_on_input = timeout_ms < 0;
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
        // A single indefinite socket wait does not need a separate QUERY:
        // netd's WAIT operation first checks current readiness, then arms its
        // bounded waiter only when no event is ready. Preserve that result
        // directly instead of paying QUERY -> WAIT -> QUERY IPC roundtrips.
        if can_block_on_input {
            match block_current_poll_on_single_socket(fds_ptr, nfds) {
                PollSocketBlockResult::Ready(ready) => return ready,
                PollSocketBlockResult::BlockedOrRaced => continue,
                PollSocketBlockResult::NoSingleSocket => {}
                PollSocketBlockResult::Err(errno) => return linux_errno(errno),
            }
        }
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
        if can_block_on_input {
            match block_current_poll_on_input(fds_ptr, nfds) {
                PollInputBlockResult::BlockedOrRaced => continue,
                PollInputBlockResult::NoInputInterest => {}
                PollInputBlockResult::Err(errno) => return linux_errno(errno),
            }
        }
        // A finite poll already owns a hard RTC deadline. Recheck once per
        // tick instead of registering the same task simultaneously in the
        // input-waiter and RTC-waiter tables. The dual registration could
        // lose both wake paths under an MSI-X/deadline race and strand an 8 ms
        // UI poll indefinitely. Indefinite polls above remain event-driven;
        // finite polls add at most one 1024 Hz tick of readiness latency.
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

    let mut ready = 0_u64;
    for index in 0..nfds_usize {
        let Some(entry_ptr) = fds_ptr.checked_add((index * POLLFD_SIZE) as u64) else {
            return linux_errno(LINUX_EFAULT);
        };
        let mut entry = [0_u8; POLLFD_SIZE];
        if let Err(err) = usermem::copy_from_current_user_exact(entry_ptr, &mut entry) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let (fd, events) = match decode_pollfd(&entry) {
            Ok(values) => values,
            Err(errno) => return linux_errno(errno),
        };
        let revents = if fd < 0 {
            0
        } else {
            let fd = fd as u64;
            match current_kernel_handle(fd as u64) {
                None => {
                    entry[0..4].copy_from_slice(&(-1_i32).to_le_bytes());
                    linux_abi::POLLNVAL as i16
                }
                Some(
                    multitask::KernelHandle::Socket(_) | multitask::KernelHandle::InetSocket(_),
                ) => match poll_socket_revents(fd, events, NETD_POLL_MODE_QUERY) {
                    Ok(revents) => revents as i16,
                    Err(errno) => return linux_errno(errno),
                },
                Some(_) if current_input_device_access(fd).is_some() => {
                    match poll_input_device_revents(fd, events) {
                        Ok(revents) => revents as i16,
                        Err(errno) => return linux_errno(errno),
                    }
                }
                Some(_) => match poll_vfs_revents(fd, events) {
                    Ok(revents) => revents as i16,
                    Err(errno) => return linux_errno(errno),
                },
            }
        };
        if revents != 0 {
            ready += 1;
        }
        entry[6..8].copy_from_slice(&revents.to_le_bytes());
        if let Err(err) = usermem::write_current_user_bytes(entry_ptr, &entry) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    ready
}

fn poll_socket_revents(fd: u64, events: u32, mode: u64) -> Result<u32, i64> {
    let result = syscall_linux_net4(
        SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
        fd,
        events as u64,
        mode,
        0,
    );
    if is_linux_error(result) {
        Err(-(result as i64))
    } else {
        Ok((result as u32)
            & poll_ready_bits(events | linux_abi::POLLERR as u32 | linux_abi::POLLHUP as u32))
    }
}

fn poll_input_device_revents(fd: u64, events: u32) -> Result<u32, i64> {
    let mut revents = 0_u32;
    if events & (linux_abi::POLLIN as u32 | linux_abi::POLLPRI as u32) != 0 {
        let pending = input_device_has_pending_events(fd)?;
        if pending {
            revents |= events & (linux_abi::POLLIN as u32 | linux_abi::POLLPRI as u32);
        }
    }
    Ok(revents)
}

fn poll_vfs_revents(fd: u64, events: u32) -> Result<u32, i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = VFS_POLL_QUERY_POLL;
    request.payload_len = POLLFD_SIZE as u32;
    request.payload[0..4].copy_from_slice(&(fd as i32).to_le_bytes());
    request.payload[4..6].copy_from_slice(&(events as i16).to_le_bytes());
    request.payload[6..8].copy_from_slice(&0_i16.to_le_bytes());

    let response = call_vfs_ipc_request(&request)?;
    ensure_vfs_status(&response)?;
    if response.payload_len as usize != POLLFD_SIZE {
        return Err(LINUX_EINVAL);
    }
    let revents_bytes: [u8; 2] = response.payload[6..8]
        .try_into()
        .map_err(|_| LINUX_EINVAL)?;
    let revents = i16::from_le_bytes(revents_bytes);
    Ok(revents as u32)
}

fn poll_ready_bits(requested: u32) -> u32 {
    requested
        & (linux_abi::POLLIN as u32
            | linux_abi::POLLPRI as u32
            | linux_abi::POLLOUT as u32
            | linux_abi::POLLERR as u32
            | linux_abi::POLLHUP as u32)
}

enum PollInputBlockResult {
    BlockedOrRaced,
    NoInputInterest,
    Err(i64),
}

enum PollSocketBlockResult {
    Ready(u64),
    BlockedOrRaced,
    NoSingleSocket,
    Err(i64),
}

/// A blocking Wayland dispatch waits on exactly one AF_UNIX socket. Route that
/// common case through netd's bounded readiness waiter instead of issuing one
/// synchronous readiness RPC per RTC tick. Multi-fd polls retain the bounded
/// compatibility loop until a kernel-wide wait set can arm all providers
/// atomically.
fn block_current_poll_on_single_socket(fds_ptr: u64, nfds: u64) -> PollSocketBlockResult {
    if nfds != 1 {
        return PollSocketBlockResult::NoSingleSocket;
    }
    let mut entry = [0_u8; POLLFD_SIZE];
    if let Err(err) = usermem::copy_from_current_user_exact(fds_ptr, &mut entry) {
        return PollSocketBlockResult::Err(address_space_error_to_linux_errno(err));
    }
    let (fd, events) = match decode_pollfd(&entry) {
        Ok(values) => values,
        Err(errno) => return PollSocketBlockResult::Err(errno),
    };
    if fd < 0
        || !matches!(
            current_kernel_handle(fd as u64),
            Some(multitask::KernelHandle::Socket(_) | multitask::KernelHandle::InetSocket(_))
        )
    {
        return PollSocketBlockResult::NoSingleSocket;
    }
    match poll_socket_revents(fd as u64, events, NETD_POLL_MODE_WAIT) {
        Ok(0) | Err(LINUX_EAGAIN) => PollSocketBlockResult::BlockedOrRaced,
        Ok(revents) => {
            entry[6..8].copy_from_slice(&(revents as i16).to_le_bytes());
            if let Err(err) = usermem::write_current_user_bytes(fds_ptr, &entry) {
                return PollSocketBlockResult::Err(address_space_error_to_linux_errno(err));
            }
            PollSocketBlockResult::Ready(1)
        }
        Err(errno) => PollSocketBlockResult::Err(errno),
    }
}

fn block_current_poll_on_input(fds_ptr: u64, nfds: u64) -> PollInputBlockResult {
    match poll_has_input_read_interest(fds_ptr, nfds) {
        Ok(true) => {}
        Ok(false) => return PollInputBlockResult::NoInputInterest,
        Err(errno) => return PollInputBlockResult::Err(errno),
    }
    let Some(task_id) = multitask::current_task_id() else {
        return PollInputBlockResult::NoInputInterest;
    };
    if !multitask::arm_block_current_task() {
        return PollInputBlockResult::NoInputInterest;
    }
    if !kernel_io_manager::api::input::event_queue::arm_input_waiter(task_id) {
        let _ = multitask::wake_task(task_id);
        let _ = multitask::commit_block_current_task();
        return PollInputBlockResult::NoInputInterest;
    }
    if kernel_io_manager::api::input::event_queue::has_pending_input_events() {
        kernel_io_manager::api::input::event_queue::disarm_input_waiter(task_id);
        let _ = multitask::wake_task(task_id);
        let _ = multitask::commit_block_current_task();
        return PollInputBlockResult::BlockedOrRaced;
    }
    match multitask::commit_block_current_task() {
        Some(true) => {
            multitask::yield_now();
            PollInputBlockResult::BlockedOrRaced
        }
        Some(false) => {
            kernel_io_manager::api::input::event_queue::disarm_input_waiter(task_id);
            PollInputBlockResult::BlockedOrRaced
        }
        None => {
            kernel_io_manager::api::input::event_queue::disarm_input_waiter(task_id);
            PollInputBlockResult::Err(LINUX_EINVAL)
        }
    }
}

fn poll_has_input_read_interest(fds_ptr: u64, nfds: u64) -> Result<bool, i64> {
    let nfds_usize = usize::try_from(nfds).map_err(|_| LINUX_EINVAL)?;
    if nfds_usize > MAX_POLL_FDS {
        return Err(LINUX_EINVAL);
    }
    for index in 0..nfds_usize {
        let entry_ptr = fds_ptr
            .checked_add((index * POLLFD_SIZE) as u64)
            .ok_or(LINUX_EFAULT)?;
        let mut entry = [0_u8; POLLFD_SIZE];
        usermem::copy_from_current_user_exact(entry_ptr, &mut entry)
            .map_err(address_space_error_to_linux_errno)?;
        let (fd, events) = decode_pollfd(&entry)?;
        if fd < 0 {
            continue;
        }
        if events & (linux_abi::POLLIN as u32 | linux_abi::POLLPRI as u32) == 0 {
            continue;
        }
        if current_input_device_access(fd as u64).is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn syscall_linux_ppoll(fds_ptr: u64, nfds: u64, timeout_ptr: u64, _sigmask_ptr: u64) -> u64 {
    let timeout_ms = if timeout_ptr == 0 {
        -1
    } else {
        let ts = match usermem::read_current_user_struct::<LinuxTimespecWire>(timeout_ptr) {
            Ok(ts) => ts,
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
        if let Err(errno) =
            request_syscalld_timespec_admission(SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP, 0, 0, ts)
        {
            return linux_errno(errno);
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
        Some(Some(fd)) => fd,
        Some(None) => linux_errno(LINUX_EMFILE),
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
        let Some(wire_event) = response.payload.get(offset..offset + EPOLL_EVENT_SIZE) else {
            return linux_errno(LINUX_EINVAL);
        };
        let (events, data) = match decode_epoll_event(wire_event) {
            Ok(values) => values,
            Err(errno) => return linux_errno(errno),
        };
        let Some(entry_ptr) = events_ptr.checked_add((slot * EPOLL_EVENT_SIZE) as u64) else {
            return linux_errno(LINUX_EFAULT);
        };
        if let Err(err) = write_linux_epoll_event(entry_ptr, events, data) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    written as u64
}
