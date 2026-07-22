use super::*;

const MAX_POLL_FDS: usize = 1024;
const POLLFD_SIZE: usize = size_of::<linux_abi::LinuxPollFd>();
const EPOLL_EVENT_SIZE: usize = size_of::<linux_abi::LinuxEpollEvent>();
const WAITSET_INTEREST_SIZE: usize = size_of::<WaitSetInterestWire>();
const WAITSET_PROVIDER_QUERY_TIMEOUT_MS: u64 = 16;

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

pub fn syscall_linux_poll(fds_ptr: u64, nfds: u64, timeout_ms: i64) -> u64 {
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let deadline_tick = (timeout_ms >= 0).then(|| {
        let timeout_ticks = (timeout_ms as u64)
            .saturating_mul(ticks_per_second)
            .saturating_add(999)
            / 1000;
        crate::arch::rtc::ticks().saturating_add(timeout_ticks)
    });
    loop {
        let first = match collect_poll_readiness(fds_ptr, nfds, deadline_tick) {
            Ok(state) => state,
            Err(LINUX_ETIMEDOUT)
                if deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) =>
            {
                return 0;
            }
            Err(errno) => return linux_errno(errno),
        };
        if first.ready != 0 {
            return first.ready;
        }
        if timeout_ms == 0
            || deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline)
        {
            return 0;
        }
        if current_wait_was_interrupted() {
            return linux_errno(LINUX_EINTR);
        }
        let Some(task_id) = multitask::current_task_id() else {
            return linux_errno(LINUX_EINVAL);
        };
        let Some(process_id) = multitask::current_user_process_id() else {
            return linux_errno(LINUX_EINVAL);
        };
        if !first.observations.is_empty()
            && let Err(errno) =
                super::super::broker_ops::waitset_broker_ops::register_waitset_waiters(
                    task_id,
                    process_id,
                    &first.observations,
                )
        {
            return linux_errno(errno);
        }

        let second = match collect_poll_readiness(fds_ptr, nfds, deadline_tick) {
            Ok(state) => state,
            Err(LINUX_ETIMEDOUT)
                if deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) =>
            {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                return 0;
            }
            Err(errno) => {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                return linux_errno(errno);
            }
        };
        if second.ready != 0
            || second.observations != first.observations
            || deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline)
        {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            if second.ready != 0 {
                return second.ready;
            }
            continue;
        }
        if !multitask::arm_block_current_task() {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            return linux_errno(LINUX_EINVAL);
        }
        if !super::super::broker_ops::waitset_broker_ops::waitset_waiters_match(
            task_id,
            process_id,
            &second.observations,
        ) {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            let _ = multitask::cancel_block_current_task();
            continue;
        }
        if current_wait_was_interrupted() {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_EINTR);
        }
        if let Some(deadline) = deadline_tick
            && !crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, deadline)
        {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_EBUSY);
        }
        match multitask::commit_block_current_task() {
            Some(true) => multitask::yield_now(),
            Some(false) => {}
            None => {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                crate::arch::rtc::disarm_sleep_waiter(task_id);
                return linux_errno(LINUX_EINVAL);
            }
        }
        super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
        crate::arch::rtc::disarm_sleep_waiter(task_id);
        if current_wait_was_interrupted() {
            return linux_errno(LINUX_EINTR);
        }
    }
}

#[derive(Debug)]
struct PollReadinessState {
    ready: u64,
    observations: Vec<super::super::broker_ops::waitset_broker_ops::ProviderObservation>,
}

fn collect_poll_readiness(
    fds_ptr: u64,
    nfds: u64,
    deadline_tick: Option<u64>,
) -> Result<PollReadinessState, i64> {
    let nfds = usize::try_from(nfds).map_err(|_| LINUX_EINVAL)?;
    if nfds > MAX_POLL_FDS {
        return Err(LINUX_EINVAL);
    }
    let mut state = PollReadinessState {
        ready: 0,
        observations: Vec::with_capacity(WAITSET_PROVIDER_MAX as usize),
    };
    for index in 0..nfds {
        let entry_ptr = fds_ptr
            .checked_add((index * POLLFD_SIZE) as u64)
            .ok_or(LINUX_EFAULT)?;
        let mut entry = [0_u8; POLLFD_SIZE];
        usermem::copy_from_current_user_exact(entry_ptr, &mut entry)
            .map_err(address_space_error_to_linux_errno)?;
        let (fd, events) = decode_pollfd(&entry)?;
        let revents = if fd < 0 {
            0
        } else {
            collect_one_poll_fd(fd as u64, events, deadline_tick, &mut state)?
        };
        if revents != 0 {
            state.ready += 1;
        }
        entry[6..8].copy_from_slice(&(revents as i16).to_le_bytes());
        usermem::write_current_user_bytes(entry_ptr, &entry)
            .map_err(address_space_error_to_linux_errno)?;
    }
    Ok(state)
}

fn collect_one_poll_fd(
    fd: u64,
    events: u32,
    deadline_tick: Option<u64>,
    state: &mut PollReadinessState,
) -> Result<u32, i64> {
    let Some(handle) = current_kernel_handle(fd) else {
        return Ok(linux_abi::POLLNVAL as u32);
    };
    let ready_mask =
        poll_ready_bits(events | linux_abi::POLLERR as u32 | linux_abi::POLLHUP as u32);
    match handle {
        multitask::KernelHandle::Socket(socket) => {
            let (revents, generation) = poll_netd_socket_token(
                socket.token_id(),
                events,
                waitset_provider_query_timeout_ms(deadline_tick),
            )?;
            note_poll_observation(state, WAITSET_PROVIDER_NETD, generation);
            Ok(revents & ready_mask)
        }
        multitask::KernelHandle::InetSocket(socket) => {
            let (revents, generation) = poll_netd_socket_token(
                socket.token_id(),
                events,
                waitset_provider_query_timeout_ms(deadline_tick),
            )?;
            note_poll_observation(state, WAITSET_PROVIDER_NETD, generation);
            Ok(revents & ready_mask)
        }
        multitask::KernelHandle::Epoll(epoll) => {
            let nested = collect_epoll_readiness(epoll.token_id(), 1, deadline_tick)?;
            for observation in nested.observations {
                note_poll_observation_for_provider(state, observation);
            }
            Ok(if nested.ready.is_empty() {
                0
            } else {
                events & linux_abi::POLLIN as u32
            })
        }
        multitask::KernelHandle::Device(_) => {
            let Some((token, access, _)) = current_input_device_description(fd) else {
                return poll_vfs_revents(fd, events, deadline_tick);
            };
            if super::super::broker_ops::waitset_broker_ops::input_open_description_access(token)
                != Some(access)
            {
                return Err(LINUX_EBADF);
            }
            let (ready, generation) = input_device_readiness_for_access_with_timeout(
                access,
                waitset_provider_query_timeout_ms(deadline_tick),
            )?;
            note_poll_observation(state, WAITSET_PROVIDER_INPUTD, generation);
            Ok(if ready {
                events & (linux_abi::POLLIN as u32 | linux_abi::POLLPRI as u32)
            } else {
                0
            })
        }
        multitask::KernelHandle::Console(multitask::ConsoleStreamKind::Input)
            if !current_console_session_is_system() =>
        {
            let session = multitask::current_console_session()
                .ok_or(LINUX_EBADF)?
                .raw();
            let (ready, live, generation) = console_readiness_via_sessiond_with_timeout(
                session,
                waitset_provider_query_timeout_ms(deadline_tick),
            )?;
            note_poll_observation(state, WAITSET_PROVIDER_SESSIOND, generation);
            Ok(if !live {
                linux_abi::POLLHUP as u32
            } else if ready {
                events & linux_abi::POLLIN as u32
            } else {
                0
            })
        }
        multitask::KernelHandle::Console(multitask::ConsoleStreamKind::Output)
        | multitask::KernelHandle::Console(multitask::ConsoleStreamKind::Error) => {
            Ok(events & linux_abi::POLLOUT as u32)
        }
        _ => poll_vfs_revents(fd, events, deadline_tick),
    }
}

fn waitset_provider_query_timeout_ms(deadline_tick: Option<u64>) -> u64 {
    deadline_tick.map_or(WAITSET_PROVIDER_QUERY_TIMEOUT_MS, |deadline| {
        waitset_provider_query_timeout_ms_from_ticks(
            crate::arch::rtc::ticks(),
            deadline,
            crate::arch::rtc::ticks_per_second().max(1),
        )
    })
}

fn waitset_provider_query_timeout_ms_from_ticks(
    now: u64,
    deadline: u64,
    ticks_per_second: u64,
) -> u64 {
    let remaining_ticks = deadline.saturating_sub(now);
    let milliseconds = remaining_ticks
        .saturating_mul(1000)
        .saturating_add(ticks_per_second.saturating_sub(1))
        / ticks_per_second.max(1);
    milliseconds.clamp(1, WAITSET_PROVIDER_QUERY_TIMEOUT_MS)
}

fn note_poll_observation(state: &mut PollReadinessState, provider: u16, generation: u64) {
    note_poll_observation_for_provider(
        state,
        super::super::broker_ops::waitset_broker_ops::ProviderObservation {
            provider,
            object_id: WAITSET_GLOBAL_OBJECT_ID,
            generation,
        },
    );
}

fn note_poll_observation_for_provider(
    state: &mut PollReadinessState,
    observation: super::super::broker_ops::waitset_broker_ops::ProviderObservation,
) {
    if let Some(previous) = state
        .observations
        .iter_mut()
        .find(|previous| previous.provider == observation.provider)
    {
        if observation.generation > previous.generation {
            *previous = observation;
        }
    } else {
        state.observations.push(observation);
        state.observations.sort_by_key(|entry| entry.provider);
    }
}

fn current_wait_was_interrupted() -> bool {
    multitask::current_linux_thread_state()
        .is_some_and(|state| state.pending_signals & !state.signal_mask != 0)
}

fn poll_vfs_revents(fd: u64, events: u32, deadline_tick: Option<u64>) -> Result<u32, i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = VFS_POLL_QUERY_POLL;
    request.payload_len = POLLFD_SIZE as u32;
    request.payload[0..4].copy_from_slice(&(fd as i32).to_le_bytes());
    request.payload[4..6].copy_from_slice(&(events as i16).to_le_bytes());
    request.payload[6..8].copy_from_slice(&0_i16.to_le_bytes());
    let response = call_vfs_ipc_request_with_timeout(
        &request,
        waitset_provider_query_timeout_ms(deadline_tick),
    )?;
    ensure_vfs_status(&response)?;
    if response.payload_len as usize != POLLFD_SIZE {
        return Err(LINUX_EINVAL);
    }
    let revents = i16::from_le_bytes(
        response.payload[6..8]
            .try_into()
            .map_err(|_| LINUX_EINVAL)?,
    );
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

pub fn syscall_linux_ppoll(
    fds_ptr: u64,
    nfds: u64,
    timeout_ptr: u64,
    sigmask_ptr: u64,
    sigset_size: u64,
) -> u64 {
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
    with_temporary_wait_signal_mask(sigmask_ptr, sigset_size, || {
        syscall_linux_poll(fds_ptr, nfds, timeout_ms)
    })
}

pub fn syscall_linux_epoll_pwait(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    timeout_ms: i64,
    sigmask_ptr: u64,
    sigset_size: u64,
) -> u64 {
    with_temporary_wait_signal_mask(sigmask_ptr, sigset_size, || {
        syscall_linux_epoll_wait(epfd, events_ptr, maxevents, timeout_ms)
    })
}

fn with_temporary_wait_signal_mask(
    sigmask_ptr: u64,
    sigset_size: u64,
    wait: impl FnOnce() -> u64,
) -> u64 {
    if sigmask_ptr == 0 {
        return wait();
    }
    if sigset_size != size_of::<u64>() as u64 {
        return linux_errno(LINUX_EINVAL);
    }
    let requested = match usermem::read_current_user_struct::<u64>(sigmask_ptr) {
        Ok(mask) => sanitize_wait_signal_mask(mask),
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let previous = multitask::with_current_user_process_and_linux_thread_state_mut(
        |_, _, _, _, thread_state| {
            thread_state.as_mut().map(|state| {
                let previous = state.signal_mask;
                state.signal_mask = requested;
                previous
            })
        },
    )
    .flatten();
    let Some(previous) = previous else {
        return linux_errno(LINUX_EINVAL);
    };
    let result = wait();
    let restored = multitask::with_current_user_process_and_linux_thread_state_mut(
        |_, _, _, _, thread_state| {
            thread_state
                .as_mut()
                .map(|state| state.signal_mask = previous)
        },
    )
    .flatten()
    .is_some();
    if restored {
        result
    } else {
        linux_errno(LINUX_EINVAL)
    }
}

fn sanitize_wait_signal_mask(mask: u64) -> u64 {
    let unblockable = (1_u64 << (linux_abi::SIGKILL - 1)) | (1_u64 << (linux_abi::SIGSTOP - 1));
    mask & !unblockable
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
        Some(None) => {
            let _ = update_vfs_epoll_ref(epoll.token_id(), false);
            linux_errno(LINUX_EMFILE)
        }
        None => {
            let _ = update_vfs_epoll_ref(epoll.token_id(), false);
            linux_errno(LINUX_ENOSYS)
        }
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

    let (provider, object_id, provider_epoch) = match epoll_interest_target(fd) {
        Ok(target) => target,
        Err(errno) => return linux_errno(errno),
    };
    let mut wire = WaitSetInterestWire {
        abi_version: WAITSET_ABI_VERSION,
        provider,
        flags: 0,
        target_fd: fd,
        object_id,
        provider_epoch,
        ..WaitSetInterestWire::default()
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
            if events & !poll_ready_bits(u32::MAX) != 0 {
                return linux_errno(LINUX_EOPNOTSUPP);
            }
            wire.events = events;
            wire.data = data;
        }
        linux_abi::EPOLL_CTL_DEL => {}
        _ => return linux_errno(LINUX_EINVAL),
    }

    request.payload_len = WAITSET_INTEREST_SIZE as u32;
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&wire as *const WaitSetInterestWire).cast::<u8>(),
            WAITSET_INTEREST_SIZE,
        )
    };
    request.payload[..WAITSET_INTEREST_SIZE].copy_from_slice(bytes);

    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_epoll_wait(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    timeout_ms: i64,
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
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let deadline_tick = (timeout_ms >= 0).then(|| {
        let timeout_ticks = (timeout_ms as u64)
            .saturating_mul(ticks_per_second)
            .saturating_add(999)
            / 1000;
        crate::arch::rtc::ticks().saturating_add(timeout_ticks)
    });

    loop {
        let first = match collect_epoll_readiness(epoll.token_id(), maxevents, deadline_tick) {
            Ok(state) => state,
            Err(LINUX_ETIMEDOUT)
                if deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) =>
            {
                return 0;
            }
            Err(errno) => return linux_errno(errno),
        };
        if !first.ready.is_empty() {
            return write_epoll_ready_events(events_ptr, &first.ready);
        }
        if timeout_ms == 0
            || deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline)
        {
            return 0;
        }
        if current_wait_was_interrupted() {
            return linux_errno(LINUX_EINTR);
        }

        let Some(task_id) = multitask::current_task_id() else {
            return linux_errno(LINUX_EINVAL);
        };
        let Some(process_id) = multitask::current_user_process_id() else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(errno) = super::super::broker_ops::waitset_broker_ops::register_waitset_waiters(
            task_id,
            process_id,
            &first.observations,
        ) {
            return linux_errno(errno);
        }

        let second = match collect_epoll_readiness(epoll.token_id(), maxevents, deadline_tick) {
            Ok(state) => state,
            Err(LINUX_ETIMEDOUT)
                if deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) =>
            {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                return 0;
            }
            Err(errno) => {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                return linux_errno(errno);
            }
        };
        if !second.ready.is_empty()
            || second.observations != first.observations
            || deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline)
        {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            if !second.ready.is_empty() {
                return write_epoll_ready_events(events_ptr, &second.ready);
            }
            continue;
        }
        if !multitask::arm_block_current_task() {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            return linux_errno(LINUX_EINVAL);
        }
        if !super::super::broker_ops::waitset_broker_ops::waitset_waiters_match(
            task_id,
            process_id,
            &second.observations,
        ) {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            let _ = multitask::cancel_block_current_task();
            continue;
        }
        if current_wait_was_interrupted() {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_EINTR);
        }
        if let Some(deadline) = deadline_tick
            && !crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, deadline)
        {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            let _ = multitask::cancel_block_current_task();
            return linux_errno(LINUX_EBUSY);
        }
        match multitask::commit_block_current_task() {
            Some(true) => multitask::yield_now(),
            Some(false) => {}
            None => {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                crate::arch::rtc::disarm_sleep_waiter(task_id);
                return linux_errno(LINUX_EINVAL);
            }
        }
        super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
        crate::arch::rtc::disarm_sleep_waiter(task_id);
        if current_wait_was_interrupted() {
            return linux_errno(LINUX_EINTR);
        }
    }
}

#[derive(Debug)]
struct EpollReadinessState {
    ready: Vec<(u32, u64)>,
    observations: Vec<super::super::broker_ops::waitset_broker_ops::ProviderObservation>,
}

fn epoll_interest_target(fd: u64) -> Result<(u16, u64, u64), i64> {
    let handle = current_kernel_handle(fd).ok_or(LINUX_EBADF)?;
    match handle {
        multitask::KernelHandle::Socket(socket) => Ok((
            WAITSET_PROVIDER_NETD,
            socket.token_id(),
            waitset_provider_epoch(WAITSET_PROVIDER_NETD)?,
        )),
        multitask::KernelHandle::InetSocket(socket) => Ok((
            WAITSET_PROVIDER_NETD,
            socket.token_id(),
            waitset_provider_epoch(WAITSET_PROVIDER_NETD)?,
        )),
        multitask::KernelHandle::Device(_) => current_input_device_description(fd)
            .map(|(token, access, _)| {
                if super::super::broker_ops::waitset_broker_ops::input_open_description_access(
                    token,
                ) != Some(access)
                {
                    return Err(LINUX_EBADF);
                }
                Ok((
                    WAITSET_PROVIDER_INPUTD,
                    token,
                    waitset_provider_epoch(WAITSET_PROVIDER_INPUTD)?,
                ))
            })
            .ok_or(LINUX_EPERM)?,
        _ => Err(LINUX_EPERM),
    }
}

fn waitset_provider_epoch(provider: u16) -> Result<u64, i64> {
    let service_id = match provider {
        WAITSET_PROVIDER_NETD => IPC_SERVICE_NETD,
        WAITSET_PROVIDER_INPUTD => IPC_SERVICE_INPUTD,
        _ => return Err(LINUX_EINVAL),
    };
    ipc_ops::service_endpoint_epoch(service_id).ok_or(LINUX_ENOSYS)
}

fn collect_epoll_readiness(
    epoll_token: u64,
    maxevents: usize,
    deadline_tick: Option<u64>,
) -> Result<EpollReadinessState, i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = VFS_POLL_QUERY_EPOLL_SNAPSHOT;
    request.arg1 = WAITSET_MAX_INTERESTS as u64;
    request.remote_id = epoll_token;
    let response = call_vfs_ipc_request_with_timeout(
        &request,
        waitset_provider_query_timeout_ms(deadline_tick),
    )?;
    ensure_vfs_status(&response).map_err(|errno| {
        if errno == LINUX_ENOENT {
            LINUX_EIO
        } else {
            errno
        }
    })?;
    let count = response.value as usize;
    if response.aux == 0
        || count > WAITSET_MAX_INTERESTS
        || response.payload_len as usize != count.saturating_mul(WAITSET_INTEREST_SIZE)
    {
        return Err(LINUX_EINVAL);
    }
    let mut interests = Vec::with_capacity(count);
    for index in 0..count {
        let offset = index * WAITSET_INTEREST_SIZE;
        let wire = unsafe {
            core::ptr::read_unaligned(
                response.payload[offset..]
                    .as_ptr()
                    .cast::<WaitSetInterestWire>(),
            )
        };
        if wire.abi_version != WAITSET_ABI_VERSION
            || wire.flags != 0
            || wire.reserved0 != 0
            || wire.provider == 0
            || wire.object_id == 0
            || wire.provider_epoch == 0
        {
            return Err(LINUX_EINVAL);
        }
        if waitset_provider_epoch(wire.provider)? != wire.provider_epoch {
            return Err(LINUX_EIO);
        }
        interests.push(wire);
    }

    let mut ready = Vec::new();
    let mut netd_generation = None;
    let mut input_generation = None;
    for interest in interests {
        let revents =
            match interest.provider {
                WAITSET_PROVIDER_NETD => {
                    let (revents, generation) = poll_netd_socket_token(
                        interest.object_id,
                        interest.events,
                        waitset_provider_query_timeout_ms(deadline_tick),
                    )?;
                    netd_generation = Some(generation);
                    revents
                }
                WAITSET_PROVIDER_INPUTD => {
                    let access =
                    super::super::broker_ops::waitset_broker_ops::input_open_description_access(
                        interest.object_id,
                    )
                    .ok_or(LINUX_EBADF)?;
                    let (is_ready, generation) = input_device_readiness_for_access_with_timeout(
                        access,
                        waitset_provider_query_timeout_ms(deadline_tick),
                    )?;
                    input_generation = Some(generation);
                    if is_ready {
                        interest.events & (linux_abi::EPOLLIN | linux_abi::EPOLLPRI)
                    } else {
                        0
                    }
                }
                _ => return Err(LINUX_EIO),
            } & poll_ready_bits(interest.events | linux_abi::EPOLLERR | linux_abi::EPOLLHUP);
        if revents != 0 && ready.len() < maxevents {
            ready.push((revents, interest.data));
        }
    }

    let mut observations = Vec::with_capacity(3);
    observations.push(
        super::super::broker_ops::waitset_broker_ops::ProviderObservation {
            provider: WAITSET_PROVIDER_VFSD,
            object_id: WAITSET_GLOBAL_OBJECT_ID,
            generation: response.aux,
        },
    );
    if let Some(generation) = netd_generation {
        observations.push(
            super::super::broker_ops::waitset_broker_ops::ProviderObservation {
                provider: WAITSET_PROVIDER_NETD,
                object_id: WAITSET_GLOBAL_OBJECT_ID,
                generation,
            },
        );
    }
    if let Some(generation) = input_generation {
        observations.push(
            super::super::broker_ops::waitset_broker_ops::ProviderObservation {
                provider: WAITSET_PROVIDER_INPUTD,
                object_id: WAITSET_GLOBAL_OBJECT_ID,
                generation,
            },
        );
    }
    Ok(EpollReadinessState {
        ready,
        observations,
    })
}

fn write_epoll_ready_events(events_ptr: u64, ready: &[(u32, u64)]) -> u64 {
    for (slot, (events, data)) in ready.iter().copied().enumerate() {
        let Some(entry_ptr) = events_ptr.checked_add((slot * EPOLL_EVENT_SIZE) as u64) else {
            return linux_errno(LINUX_EFAULT);
        };
        if let Err(err) = write_linux_epoll_event(entry_ptr, events, data) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    ready.len() as u64
}

pub fn update_vfs_epoll_ref(token: u64, acquire: bool) -> Result<(), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = if acquire {
        VFS_POLL_QUERY_EPOLL_REF
    } else {
        VFS_POLL_QUERY_EPOLL_UNREF
    };
    request.remote_id = token;
    call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response))
}

pub fn purge_vfs_epoll_object(provider: u16, object_id: u64) -> Result<(), i64> {
    purge_vfs_epoll_object_with_timeout(provider, object_id, None)
}

pub fn purge_vfs_epoll_object_bounded(provider: u16, object_id: u64) -> Result<(), i64> {
    purge_vfs_epoll_object_with_timeout(provider, object_id, Some(16))
}

fn purge_vfs_epoll_object_with_timeout(
    provider: u16,
    object_id: u64,
    timeout_ms: Option<u64>,
) -> Result<(), i64> {
    if provider == 0 || object_id == 0 {
        return Err(LINUX_EINVAL);
    }
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = VFS_POLL_QUERY_EPOLL_PURGE_OBJECT;
    request.arg1 = provider as u64;
    request.arg2 = object_id;
    let response = match timeout_ms {
        Some(timeout_ms) => call_vfs_ipc_request_with_timeout(&request, timeout_ms),
        None => call_vfs_ipc_request(&request),
    }?;
    ensure_vfs_status(&response)
}

#[cfg(test)]
mod tests {
    use super::{
        PollReadinessState, note_poll_observation, sanitize_wait_signal_mask,
        waitset_provider_query_timeout_ms_from_ticks,
    };
    use alloc::vec::Vec;
    use rustos_user_abi::linux::{SIGKILL, SIGSTOP};
    use rustos_user_abi::syscall::{WAITSET_PROVIDER_INPUTD, WAITSET_PROVIDER_NETD};

    #[test]
    fn provider_observations_are_deduplicated_and_keep_the_newest_generation() {
        let mut state = PollReadinessState {
            ready: 0,
            observations: Vec::new(),
        };
        note_poll_observation(&mut state, WAITSET_PROVIDER_NETD, 7);
        note_poll_observation(&mut state, WAITSET_PROVIDER_INPUTD, 11);
        note_poll_observation(&mut state, WAITSET_PROVIDER_NETD, 7);
        assert_eq!(state.observations.len(), 2);
        assert_eq!(state.observations[0].provider, WAITSET_PROVIDER_NETD);
        assert_eq!(state.observations[1].provider, WAITSET_PROVIDER_INPUTD);
        note_poll_observation(&mut state, WAITSET_PROVIDER_NETD, 8);
        assert_eq!(state.observations[0].generation, 8);
    }

    #[test]
    fn temporary_wait_mask_cannot_block_kill_or_stop() {
        let kill = 1_u64 << (SIGKILL - 1);
        let stop = 1_u64 << (SIGSTOP - 1);
        let ordinary = 1_u64 << (2 - 1);
        assert_eq!(sanitize_wait_signal_mask(kill | stop | ordinary), ordinary);
    }

    #[test]
    fn provider_query_timeout_never_exceeds_the_wait_deadline_or_service_cap() {
        assert_eq!(
            waitset_provider_query_timeout_ms_from_ticks(10, 10, 1000),
            1
        );
        assert_eq!(
            waitset_provider_query_timeout_ms_from_ticks(10, 15, 1000),
            5
        );
        assert_eq!(
            waitset_provider_query_timeout_ms_from_ticks(10, 100, 1000),
            16
        );
        assert_eq!(
            waitset_provider_query_timeout_ms_from_ticks(10, 11, 100),
            10
        );
    }
}
