//! Linux poll/epoll wait ABI over cross-service readiness generations.
//!
//! - **Owner:** Compat owns fd-set semantics; provider services own readiness
//!   generations and `kernel-ps` owns task blocking.
//! - **Boundary:** User arrays, epoll registrations, signal masks, service
//!   replies, and provider generations are untrusted.
//! - **Lifecycle:** Query, register, requery, arm, presence-check, commit,
//!   resume, remove, disarm, and requery return one terminal outcome.
//! - **Concurrency:** Waiter identity binds task/process/open-description and
//!   provider epochs; provider calls are capped by the remaining app deadline.
//! - **Failure:** Signal, timeout, close, dup/exec, provider revoke/restart,
//!   peer exit, and late reply cannot lose a wake or revive a stale waiter.
//! - **Forbidden:** No periodic polling, one-shot timeout ignoring, fd-only
//!   registration, unbounded provider call, per-descriptor provider round trip,
//!   unclaimed edge/one-shot delivery, or app-specific readiness path.
//! - **Evidence:** `waitset` and `input-delivery-lifecycle`.
mod control;

use super::*;

// Every provider-backed descriptor in one poll arm must fit in the exact
// object observation set installed in ring0.  Reject larger scans instead of
// silently collapsing them to a provider-wide wake bucket.
const MAX_POLL_FDS: usize = WAITSET_MAX_INTERESTS;
const POLLFD_SIZE: usize = size_of::<linux_abi::LinuxPollFd>();
const EPOLL_EVENT_SIZE: usize = size_of::<linux_abi::LinuxEpollEvent>();
const WAITSET_INTEREST_SIZE: usize = size_of::<WaitSetInterestWire>();
const WAITSET_SNAPSHOT_SIZE: usize = size_of::<WaitSetSnapshotWire>();
const WAITSET_CLAIM_SIZE: usize = size_of::<WaitSetClaimWire>();
const _: () = assert!(WAITSET_MAX_INTERESTS * WAITSET_SNAPSHOT_SIZE <= VFS_IPC_PAYLOAD_CAPACITY);
const _: () = assert!(NETD_POLL_BATCH_MAX >= MAX_POLL_FDS);
const WAITSET_PROVIDER_QUERY_TIMEOUT_MS: u64 =
    rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS;

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
            Err(LINUX_ETIMEDOUT) => {
                if current_wait_was_interrupted() {
                    return linux_errno(LINUX_EINTR);
                }
                multitask::cond_resched();
                continue;
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
            Err(LINUX_ETIMEDOUT) => {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                if current_wait_was_interrupted() {
                    return linux_errno(LINUX_EINTR);
                }
                multitask::cond_resched();
                continue;
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
        match multitask::commit_block_current_task_and_yield() {
            Some(true) => {}
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
    provider_timed_out: bool,
    observations: Vec<super::super::broker_ops::waitset_broker_ops::ProviderObservation>,
}

/// Where one `pollfd` gets its readiness from. Provider-backed descriptors are
/// resolved together, one batched exchange per provider, after the whole array
/// has been classified.
enum PollRoute {
    Done(u32),
    Netd { token: u64 },
    Vfs,
}

struct PollEntry {
    ptr: u64,
    raw: [u8; POLLFD_SIZE],
    fd: i32,
    events: u32,
    revents: u32,
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
        provider_timed_out: false,
        observations: Vec::with_capacity(nfds.min(WAITSET_MAX_INTERESTS)),
    };
    let mut entries = Vec::with_capacity(nfds);
    let mut netd_slots = Vec::new();
    let mut netd_queries = Vec::new();
    let mut vfs_slots = Vec::new();
    for index in 0..nfds {
        let entry_ptr = fds_ptr
            .checked_add((index * POLLFD_SIZE) as u64)
            .ok_or(LINUX_EFAULT)?;
        let mut raw = [0_u8; POLLFD_SIZE];
        usermem::copy_from_current_user_exact(entry_ptr, &mut raw)
            .map_err(address_space_error_to_linux_errno)?;
        let (fd, events) = decode_pollfd(&raw)?;
        let route = if fd < 0 {
            PollRoute::Done(0)
        } else {
            match route_one_poll_fd(fd as u64, events, deadline_tick, &mut state) {
                Ok(route) => route,
                Err(LINUX_ETIMEDOUT) => {
                    state.provider_timed_out = true;
                    PollRoute::Done(0)
                }
                Err(errno) => return Err(errno),
            }
        };
        let revents = match route {
            PollRoute::Done(revents) => revents,
            PollRoute::Netd { token } => {
                netd_slots.push(index);
                netd_queries.push((token, events));
                0
            }
            PollRoute::Vfs => {
                vfs_slots.push(index);
                0
            }
        };
        entries.push(PollEntry {
            ptr: entry_ptr,
            raw,
            fd,
            events,
            revents,
        });
    }
    resolve_poll_netd_batch(&mut entries, &netd_slots, &netd_queries, deadline_tick, &mut state)?;
    resolve_poll_vfs_batch(&mut entries, &vfs_slots, deadline_tick, &mut state)?;
    for entry in &entries {
        if entry.revents != 0 {
            state.ready += 1;
        }
        // Linux stores only `revents`; rewriting fd/events would clobber a
        // concurrent edit of those fields, and an unchanged value needs no
        // store (the page may not even be writable then).
        let revents_bytes = (entry.revents as i16).to_le_bytes();
        if entry.raw[6..8] != revents_bytes {
            usermem::write_current_user_bytes(entry.ptr + 6, &revents_bytes)
                .map_err(address_space_error_to_linux_errno)?;
        }
    }
    if state.observations.len()
        > super::super::broker_ops::waitset_broker_ops::WAITSET_MAX_OBSERVATIONS
    {
        return Err(LINUX_EINVAL);
    }
    require_completed_provider_scan(state.provider_timed_out, state.ready != 0)?;
    Ok(state)
}

fn resolve_poll_netd_batch(
    entries: &mut [PollEntry],
    slots: &[usize],
    queries: &[(u64, u32)],
    deadline_tick: Option<u64>,
    state: &mut PollReadinessState,
) -> Result<(), i64> {
    if queries.is_empty() {
        return Ok(());
    }
    let outcomes = match poll_netd_socket_batch(queries, waitset_provider_query_timeout_ms(deadline_tick))
    {
        Ok(outcomes) => outcomes,
        Err(LINUX_ETIMEDOUT) => {
            state.provider_timed_out = true;
            return Ok(());
        }
        Err(errno) if provider_revoke_events(errno).is_some() => {
            for slot in slots {
                entries[*slot].revents = provider_revoke_events(errno).unwrap();
            }
            return Ok(());
        }
        Err(errno) => return Err(errno),
    };
    for (slot, ((token, events), outcome)) in slots.iter().zip(queries.iter().zip(outcomes)) {
        let ready_mask = poll_ready_bits(*events | linux_abi::POLLERR as u32 | linux_abi::POLLHUP as u32);
        entries[*slot].revents = match outcome {
            Ok((revents, generation)) => {
                note_poll_observation(state, WAITSET_PROVIDER_NETD, *token, generation);
                revents & ready_mask
            }
            Err(errno) if provider_revoke_events(errno).is_some() => {
                provider_revoke_events(errno).unwrap()
            }
            Err(errno) => return Err(errno),
        };
    }
    Ok(())
}

fn resolve_poll_vfs_batch(
    entries: &mut [PollEntry],
    slots: &[usize],
    deadline_tick: Option<u64>,
    state: &mut PollReadinessState,
) -> Result<(), i64> {
    for chunk in slots.chunks(VFS_POLL_BATCH_MAX) {
        let queries = chunk
            .iter()
            .map(|slot| (entries[*slot].fd, entries[*slot].events))
            .collect::<Vec<_>>();
        match poll_vfs_batch(&queries, deadline_tick) {
            Ok(revents) => {
                for (slot, revents) in chunk.iter().zip(revents) {
                    entries[*slot].revents = revents;
                }
            }
            Err(LINUX_ETIMEDOUT) => state.provider_timed_out = true,
            Err(errno) if provider_revoke_events(errno).is_some() => {
                for slot in chunk {
                    entries[*slot].revents = provider_revoke_events(errno).unwrap();
                }
            }
            Err(errno) => return Err(errno),
        }
    }
    Ok(())
}

fn require_completed_provider_scan(timed_out: bool, has_ready: bool) -> Result<(), i64> {
    if timed_out && !has_ready {
        Err(LINUX_ETIMEDOUT)
    } else {
        Ok(())
    }
}

fn route_one_poll_fd(
    fd: u64,
    events: u32,
    deadline_tick: Option<u64>,
    state: &mut PollReadinessState,
) -> Result<PollRoute, i64> {
    let Some(handle) = current_kernel_handle(fd) else {
        return Ok(PollRoute::Done(linux_abi::POLLNVAL as u32));
    };
    match handle {
        multitask::KernelHandle::Socket(socket) => Ok(PollRoute::Netd {
            token: socket.token_id(),
        }),
        multitask::KernelHandle::InetSocket(socket) => Ok(PollRoute::Netd {
            token: socket.token_id(),
        }),
        multitask::KernelHandle::Epoll(epoll) => {
            let nested = collect_epoll_readiness(
                epoll.token_id(),
                1,
                deadline_tick,
                EpollScanMode::Peek,
            )?;
            for observation in nested.observations {
                note_poll_observation_for_provider(state, observation);
            }
            Ok(PollRoute::Done(if nested.ready.is_empty() {
                0
            } else {
                events & linux_abi::POLLIN as u32
            }))
        }
        multitask::KernelHandle::Device(_) => {
            let Some((token, access, _)) = current_input_device_description(fd) else {
                return Ok(PollRoute::Vfs);
            };
            if super::super::broker_ops::waitset_broker_ops::input_open_description_access(token)
                != Some(access)
            {
                return Err(LINUX_EBADF);
            }
            let (ready, generation) = match input_readiness_for_access(access, deadline_tick) {
                Ok(result) => result,
                Err(errno) if provider_revoke_events(errno).is_some() => {
                    return Ok(PollRoute::Done(provider_revoke_events(errno).unwrap()));
                }
                Err(errno) => return Err(errno),
            };
            note_poll_observation(
                state,
                WAITSET_PROVIDER_INPUTD,
                waitset_input_object_id(access),
                generation,
            );
            Ok(PollRoute::Done(if ready {
                events & (linux_abi::POLLIN as u32 | linux_abi::POLLPRI as u32)
            } else {
                0
            }))
        }
        multitask::KernelHandle::Console(console)
            if console.stream() == multitask::ConsoleStreamKind::Input
                && !current_console_session_is_system() =>
        {
            let session = multitask::current_console_session()
                .ok_or(LINUX_EBADF)?
                .raw();
            let (ready, live, generation) = match console_readiness_via_sessiond_with_timeout(
                session,
                waitset_provider_query_timeout_ms(deadline_tick),
            ) {
                Ok(result) => result,
                Err(errno) if provider_revoke_events(errno).is_some() => {
                    return Ok(PollRoute::Done(provider_revoke_events(errno).unwrap()));
                }
                Err(errno) => return Err(errno),
            };
            note_poll_observation(state, WAITSET_PROVIDER_SESSIOND, session, generation);
            Ok(PollRoute::Done(console_ready_events(
                console.stream(),
                live,
                ready,
                events,
            )))
        }
        multitask::KernelHandle::Console(console)
            if matches!(
                console.stream(),
                multitask::ConsoleStreamKind::Output | multitask::ConsoleStreamKind::Error
            ) && !current_console_session_is_system() =>
        {
            let session = multitask::current_console_session()
                .ok_or(LINUX_EBADF)?
                .raw();
            let (_, live, generation) = match console_readiness_via_sessiond_with_timeout(
                session,
                waitset_provider_query_timeout_ms(deadline_tick),
            ) {
                Ok(result) => result,
                Err(errno) if provider_revoke_events(errno).is_some() => {
                    return Ok(PollRoute::Done(provider_revoke_events(errno).unwrap()));
                }
                Err(errno) => return Err(errno),
            };
            note_poll_observation(state, WAITSET_PROVIDER_SESSIOND, session, generation);
            Ok(PollRoute::Done(console_ready_events(
                console.stream(),
                live,
                false,
                events,
            )))
        }
        multitask::KernelHandle::Console(console)
            if matches!(
                console.stream(),
                multitask::ConsoleStreamKind::Output | multitask::ConsoleStreamKind::Error
            ) =>
        {
            Ok(PollRoute::Done(events & linux_abi::POLLOUT as u32))
        }
        _ => Ok(PollRoute::Vfs),
    }
}

fn provider_revoke_events(errno: i64) -> Option<u32> {
    matches!(
        errno,
        LINUX_EBADF | LINUX_ENODEV | LINUX_EPIPE | LINUX_ENOSYS
    )
    .then_some(linux_abi::POLLERR as u32 | linux_abi::POLLHUP as u32)
}

fn transient_waitset_scan_error(errno: i64) -> bool {
    // A VFS reply capability can be revoked after a bounded provider query
    // times out even while vfsd and the epoll object remain live. EPIPE is an
    // internal transport outcome, not a Linux epoll_wait(2) result. Re-scan
    // under the original application deadline so a late reply cannot kill an
    // otherwise valid wait set.
    errno == LINUX_EPIPE
}

/// inputd's readiness for one access ABI, from what it has published if it has
/// published anything, and from a bounded query only until it has.
///
/// The query is the fallback, not the rule. It is a synchronous round trip to
/// a service that also does bulk decode work, and it runs on the wait-set scan
/// path - every `epoll_wait` and every `poll`. One answer arriving past
/// `WAITSET_PROVIDER_QUERY_TIMEOUT_MS` desynchronises caller and service
/// permanently, because the caller cancels its reply and the service's next
/// reply always answers the question before the one being asked. A measured
/// 90-second run froze the pointer that way at t=21.3s: sixteen
/// `ipc-service-reply-timeout` records, then ninety-eight consecutive rejected
/// inputd replies, and an `epoll_wait` that never returned again.
fn input_readiness_for_access(access: u16, deadline_tick: Option<u64>) -> Result<(bool, u64), i64> {
    if let Some(published) =
        super::super::broker_ops::waitset_broker_ops::published_provider_readiness(
            WAITSET_PROVIDER_INPUTD,
            waitset_input_object_id(access),
        )
    {
        return Ok(published);
    }
    input_device_readiness_for_access_with_timeout(
        access,
        waitset_provider_query_timeout_ms(deadline_tick),
    )
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

fn note_poll_observation(
    state: &mut PollReadinessState,
    provider: u16,
    object_id: u64,
    generation: u64,
) {
    note_poll_observation_for_provider(
        state,
        super::super::broker_ops::waitset_broker_ops::ProviderObservation {
            provider,
            object_id,
            generation,
        },
    );
}

fn note_poll_observation_for_provider(
    state: &mut PollReadinessState,
    observation: super::super::broker_ops::waitset_broker_ops::ProviderObservation,
) {
    merge_provider_observation(&mut state.observations, observation);
}

fn merge_provider_observation(
    observations: &mut Vec<super::super::broker_ops::waitset_broker_ops::ProviderObservation>,
    observation: super::super::broker_ops::waitset_broker_ops::ProviderObservation,
) {
    // The vector is kept sorted by (provider, object); a binary search keeps a
    // scan of n descriptors O(n log n) instead of re-sorting on every insert.
    let key = (observation.provider, observation.object_id);
    match observations.binary_search_by_key(&key, |entry| (entry.provider, entry.object_id)) {
        Ok(index) => {
            if observation.generation > observations[index].generation {
                observations[index] = observation;
            }
        }
        Err(index) => observations.insert(index, observation),
    }
}

fn waitset_input_object_id(access: u16) -> u64 {
    match access {
        INPUTD_ACCESS_NATIVE => WAITSET_INPUT_NATIVE_OBJECT_ID,
        INPUTD_ACCESS_EVDEV => WAITSET_INPUT_EVDEV_OBJECT_ID,
        _ => 0,
    }
}

fn current_wait_was_interrupted() -> bool {
    multitask::current_linux_thread_state()
        .is_some_and(|state| state.pending_signals & !state.signal_mask != 0)
}

fn acknowledge_epoll_wait_block(task_id: u64) {
    super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
    crate::arch::rtc::disarm_sleep_waiter(task_id);
}

/// Readiness of descriptors vfsd answers for, `VFS_POLL_BATCH_MAX` per round
/// trip. The reply echoes every entry in order with `revents` filled in.
fn poll_vfs_batch(queries: &[(i32, u32)], deadline_tick: Option<u64>) -> Result<Vec<u32>, i64> {
    if queries.is_empty() || queries.len() > VFS_POLL_BATCH_MAX {
        return Err(LINUX_EINVAL);
    }
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = VFS_POLL_QUERY_POLL;
    request.payload_len = (queries.len() * POLLFD_SIZE) as u32;
    for (index, (fd, events)) in queries.iter().copied().enumerate() {
        let offset = index * POLLFD_SIZE;
        request.payload[offset..offset + 4].copy_from_slice(&fd.to_le_bytes());
        request.payload[offset + 4..offset + 6].copy_from_slice(&(events as i16).to_le_bytes());
        request.payload[offset + 6..offset + 8].copy_from_slice(&0_i16.to_le_bytes());
    }
    let response = call_vfs_ipc_request_with_timeout(
        &request,
        waitset_provider_query_timeout_ms(deadline_tick),
    )?;
    ensure_vfs_status(&response)?;
    if response.payload_len as usize != queries.len() * POLLFD_SIZE {
        return Err(LINUX_EINVAL);
    }
    let mut revents = Vec::with_capacity(queries.len());
    for index in 0..queries.len() {
        let offset = index * POLLFD_SIZE;
        let value = i16::from_le_bytes(
            response.payload[offset + 6..offset + 8]
                .try_into()
                .map_err(|_| LINUX_EINVAL)?,
        );
        revents.push(value as u32);
    }
    Ok(revents)
}

fn poll_ready_bits(requested: u32) -> u32 {
    requested
        & (linux_abi::POLLIN as u32
            | linux_abi::POLLPRI as u32
            | linux_abi::POLLOUT as u32
            | linux_abi::POLLERR as u32
            | linux_abi::POLLHUP as u32
            | linux_abi::POLLRDHUP as u32)
}

fn console_ready_events(
    stream: multitask::ConsoleStreamKind,
    live: bool,
    input_ready: bool,
    requested: u32,
) -> u32 {
    if !live {
        return linux_abi::POLLHUP as u32;
    }
    match stream {
        multitask::ConsoleStreamKind::Input if input_ready => requested & linux_abi::POLLIN as u32,
        multitask::ConsoleStreamKind::Output | multitask::ConsoleStreamKind::Error => {
            requested & linux_abi::POLLOUT as u32
        }
        _ => 0,
    }
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
        if let Err(errno) = process_time::validate_relative_sleep_timespec_locally(ts) {
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
    let epoll_token = epoll.token_id();
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = VFS_POLL_QUERY_EPOLL_CREATE;
    request.remote_id = epoll_token;
    if let Err(errno) = call_vfs_ipc_request_with_timeout(&request, control::deadline_ms())
        .and_then(|response| ensure_vfs_status(&response))
    {
        // A timeout can race the service commit. The token is globally unique
        // and not yet published in the fd table, so an idempotent bounded
        // retire safely removes a possibly-created orphan and otherwise gets
        // ENOENT without affecting another epoll object.
        let _ = retire_vfs_epoll_bounded(epoll_token);
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
            let _ = retire_vfs_epoll(epoll_token);
            linux_errno(LINUX_EMFILE)
        }
        None => {
            let _ = retire_vfs_epoll(epoll_token);
            linux_errno(LINUX_ENOSYS)
        }
    }
}

pub fn syscall_linux_epoll_ctl(epfd: u64, op: u64, fd: u64, event_ptr: u64) -> u64 {
    if epfd == fd {
        return linux_errno(LINUX_EINVAL);
    }
    let epoll = match epoll_handle_or_errno(epfd) {
        Ok(epoll) => epoll,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = VFS_POLL_QUERY_EPOLL_CTL;
    request.arg1 = op;
    request.fd = fd;
    request.remote_id = epoll.token_id();

    if !matches!(
        op,
        linux_abi::EPOLL_CTL_ADD | linux_abi::EPOLL_CTL_MOD | linux_abi::EPOLL_CTL_DEL
    ) {
        return linux_errno(LINUX_EINVAL);
    }
    let target = match epoll_interest_target(fd) {
        Ok(target) => target,
        Err(errno) => return linux_errno(errno),
    };
    let provider_epoch = if epoll_ctl_requires_live_provider_epoch(op) {
        match waitset_provider_epoch(target.provider) {
            Ok(epoch) => epoch,
            Err(errno) => return linux_errno(errno),
        }
    } else {
        // DEL is keyed only by target fd plus open-description identity. Keep
        // the wire structurally valid without making cleanup depend on a live
        // provider or its replacement epoch.
        1
    };
    let mut wire = WaitSetInterestWire {
        abi_version: WAITSET_ABI_VERSION,
        provider: target.provider,
        flags: 0,
        target_fd: fd,
        object_id: target.object_id,
        provider_epoch,
        ..WaitSetInterestWire::default()
    };

    match op {
        linux_abi::EPOLL_CTL_ADD | linux_abi::EPOLL_CTL_MOD => {
            if event_ptr == 0 {
                return linux_errno(LINUX_EINVAL);
            }
            let (events, data) = match read_linux_epoll_event(event_ptr) {
                Ok(event) => event,
                Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
            };
            // Linux: EPOLLEXCLUSIVE is ADD-only and combines only with the
            // bits that make a wake-one hint meaningful. An interest added
            // that way is later immutable (vfsd rejects the MOD too).
            if events & linux_abi::EPOLLEXCLUSIVE != 0
                && (op == linux_abi::EPOLL_CTL_MOD
                    || events & !linux_abi::EPOLLEXCLUSIVE_OK_BITS != 0)
            {
                return linux_errno(LINUX_EINVAL);
            }
            // EPOLLWAKEUP needs CAP_BLOCK_SUSPEND; RustOS has no suspend
            // blockers, so it is dropped exactly as for an unprivileged caller.
            wire.events = events & !linux_abi::EPOLLWAKEUP;
            wire.data = data;
        }
        linux_abi::EPOLL_CTL_DEL => {}
        _ => unreachable!(),
    }

    request.payload_len = WAITSET_INTEREST_SIZE as u32;
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&wire as *const WaitSetInterestWire).cast::<u8>(),
            WAITSET_INTEREST_SIZE,
        )
    };
    request.payload[..WAITSET_INTEREST_SIZE].copy_from_slice(bytes);

    let guard = if epoll_ctl_requires_live_provider_epoch(op) {
        match acquire_epoll_target_guard(&target.handle) {
            Ok(guard) => Some(guard),
            Err(errno) => return linux_errno(errno),
        }
    } else {
        None
    };
    let result = call_vfs_ipc_request_with_timeout(&request, control::deadline_ms())
        .and_then(|response| ensure_vfs_status(&response));
    if let Some(guard) = guard {
        release_epoll_target_guard(guard);
    }
    match result {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

/// Linux reports a descriptor that is not open as EBADF and one that is open
/// but not an epoll instance as EINVAL; runtimes branch on the difference.
fn epoll_handle_or_errno(epfd: u64) -> Result<multitask::EpollHandle, i64> {
    if let Some(epoll) = current_epoll_handle(epfd) {
        return Ok(epoll);
    }
    let open = multitask::with_current_user_process_state(|_, _, process_state| {
        process_state.handles().get(epfd).is_some()
    })
    .unwrap_or(false);
    Err(if open { LINUX_EINVAL } else { LINUX_EBADF })
}

/// Names which `epoll_wait` exit produced an error: the same errno comes from
/// several scan, registration and block-commit sites.
fn epoll_wait_failed(site: u64, errno: i64) -> u64 {
    debug::record_milestone(debug::LogCategory::Compat, "epoll-wait-failed", site, errno.unsigned_abs());
    linux_errno(errno)
}

pub fn syscall_linux_epoll_wait(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    timeout_ms: i64,
) -> u64 {
    // `maxevents` is a C `int`; the register's upper half is not ABI.
    let Ok(maxevents) = usize::try_from(maxevents as u32 as i32) else {
        return linux_errno(LINUX_EINVAL);
    };
    // Linux's EP_MAX_EVENTS bound. Larger-than-interest-table counts are legal
    // (libuv passes 1024) and are served in full by clamping: one wait can
    // never report more than the table's interests.
    if maxevents == 0 || maxevents > i32::MAX as usize / EPOLL_EVENT_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    let maxevents = maxevents.min(MAX_POLL_FDS);
    if let Err(err) =
        usermem::validate_current_user_write_buffer(events_ptr, maxevents * EPOLL_EVENT_SIZE)
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let epoll = match epoll_handle_or_errno(epfd) {
        Ok(epoll) => epoll,
        Err(errno) => return linux_errno(errno),
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
        let first = match collect_epoll_readiness(epoll.token_id(), maxevents, deadline_tick, EpollScanMode::Consume) {
            Ok(state) => state,
            Err(LINUX_ETIMEDOUT)
                if deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) =>
            {
                return 0;
            }
            Err(LINUX_ETIMEDOUT) => {
                if current_wait_was_interrupted() {
                    return linux_errno(LINUX_EINTR);
                }
                multitask::cond_resched();
                continue;
            }
            Err(errno) if transient_waitset_scan_error(errno) => {
                multitask::cond_resched();
                continue;
            }
            Err(errno) => return epoll_wait_failed(1, errno),
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
            return epoll_wait_failed(2, errno);
        }

        let second = match collect_epoll_readiness(epoll.token_id(), maxevents, deadline_tick, EpollScanMode::Consume) {
            Ok(state) => state,
            Err(LINUX_ETIMEDOUT)
                if deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) =>
            {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                return 0;
            }
            Err(LINUX_ETIMEDOUT) => {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                if current_wait_was_interrupted() {
                    return linux_errno(LINUX_EINTR);
                }
                multitask::cond_resched();
                continue;
            }
            Err(errno) if transient_waitset_scan_error(errno) => {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                multitask::cond_resched();
                continue;
            }
            Err(errno) => {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                return epoll_wait_failed(3, errno);
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
            return epoll_wait_failed(4, LINUX_EINVAL);
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
        match multitask::commit_block_current_task_and_yield() {
            Some(true) => {}
            Some(false) => {}
            None => {
                acknowledge_epoll_wait_block(task_id);
                return epoll_wait_failed(5, LINUX_EINVAL);
            }
        }
        acknowledge_epoll_wait_block(task_id);
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

fn epoll_ctl_requires_live_provider_epoch(op: u64) -> bool {
    op != linux_abi::EPOLL_CTL_DEL
}

struct EpollInterestTarget {
    provider: u16,
    object_id: u64,
    handle: multitask::KernelHandle,
}

enum EpollTargetGuard {
    Service(ServiceHandleRef),
    Console(multitask::ConsoleHandle),
}

fn epoll_interest_target(fd: u64) -> Result<EpollInterestTarget, i64> {
    let entry = multitask::with_current_user_process_state(|_, _, process_state| {
        process_state.handles().get_entry(fd).cloned()
    })
    .flatten()
    .ok_or(LINUX_EBADF)?;
    let handle = entry.handle().clone();
    let (provider, object_id) = match &handle {
        multitask::KernelHandle::Socket(socket) => (WAITSET_PROVIDER_NETD, socket.token_id()),
        multitask::KernelHandle::InetSocket(socket) => (WAITSET_PROVIDER_NETD, socket.token_id()),
        multitask::KernelHandle::Device(device)
            if entry.rights().allows_read()
                && device.device_id() == kernel_object::api::device::DeviceId::Input =>
        {
            let access = match device.access_kind() {
                kernel_object::api::device::DeviceAccessKind::Native => INPUTD_ACCESS_NATIVE,
                kernel_object::api::device::DeviceAccessKind::Evdev => INPUTD_ACCESS_EVDEV,
            };
            if super::super::broker_ops::waitset_broker_ops::input_open_description_access(
                device.token_id(),
            ) != Some(access)
            {
                return Err(LINUX_EBADF);
            }
            (WAITSET_PROVIDER_INPUTD, device.token_id())
        }
        multitask::KernelHandle::Console(console) if !current_console_session_is_system() => {
            (WAITSET_PROVIDER_SESSIOND, console.token_id())
        }
        _ => return Err(LINUX_EPERM),
    };
    Ok(EpollInterestTarget {
        provider,
        object_id,
        handle,
    })
}

fn acquire_epoll_target_guard(handle: &multitask::KernelHandle) -> Result<EpollTargetGuard, i64> {
    if let Some(handle_ref) = service_handle_ref_for_handle(handle) {
        acquire_service_handle_ref(&handle_ref)?;
        return Ok(EpollTargetGuard::Service(handle_ref));
    }
    if let multitask::KernelHandle::Console(console) = handle {
        if console.try_acquire_descriptor_reference() {
            return Ok(EpollTargetGuard::Console(console.clone()));
        }
        return Err(LINUX_EBADF);
    }
    Err(LINUX_EPERM)
}

fn release_epoll_target_guard(guard: EpollTargetGuard) {
    match guard {
        EpollTargetGuard::Service(handle_ref) => {
            release_service_handle_refs_bounded(&[handle_ref]);
        }
        EpollTargetGuard::Console(console) => {
            if console.release_descriptor_reference() {
                let _ =
                    purge_vfs_epoll_object_bounded(WAITSET_PROVIDER_SESSIOND, console.token_id());
            }
        }
    }
}

fn waitset_provider_epoch(provider: u16) -> Result<u64, i64> {
    let service_id = match provider {
        WAITSET_PROVIDER_NETD => IPC_SERVICE_NETD,
        WAITSET_PROVIDER_INPUTD => IPC_SERVICE_INPUTD,
        WAITSET_PROVIDER_SESSIOND => IPC_SERVICE_SESSIOND,
        _ => return Err(LINUX_EINVAL),
    };
    ipc_ops::service_endpoint_epoch(service_id).ok_or(LINUX_ENOSYS)
}

/// Whether a scan consumes edge-triggered and one-shot reports.
///
/// `epoll_wait` consumes: the events it returns are claimed in vfsd first, so
/// two waiters on one epoll object never both receive the same edge or the same
/// one-shot report. Nested readiness (`poll`/`epoll` on an epoll fd) only
/// peeks; it has to answer "would `epoll_wait` return something" without
/// stealing the report from the real waiter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EpollScanMode {
    Consume,
    Peek,
}

/// One interest whose provider reported readiness the application asked for.
struct EpollCandidate {
    events: u32,
    data: u64,
    claim: WaitSetClaimWire,
    /// Edge-triggered or one-shot: the report must be claimed in vfsd.
    gated: bool,
}

enum InterestScan {
    Skip,
    Netd,
    TimedOut,
    Ready { revents: u32, generation: u64 },
}

/// Provider-level event bits that satisfy the Linux event mask `events`.
fn epoll_provider_events(events: u32) -> u32 {
    let mut provider = events & (linux_abi::EPOLLPRI | linux_abi::EPOLLRDHUP);
    if events & (linux_abi::EPOLLIN | linux_abi::EPOLLRDNORM) != 0 {
        provider |= linux_abi::EPOLLIN;
    }
    if events & (linux_abi::EPOLLOUT | linux_abi::EPOLLWRNORM) != 0 {
        provider |= linux_abi::EPOLLOUT;
    }
    provider
}

/// The bits `epoll_wait` reports for provider readiness `provider_revents`
/// against the registered mask `requested`. `EPOLLERR` and `EPOLLHUP` are
/// always reported; the normal-data aliases appear only if they were asked for.
fn epoll_report_events(provider_revents: u32, requested: u32) -> u32 {
    let mut out = provider_revents
        & (linux_abi::EPOLLPRI | linux_abi::EPOLLRDHUP | linux_abi::EPOLLERR | linux_abi::EPOLLHUP)
        & (requested | linux_abi::EPOLLERR | linux_abi::EPOLLHUP);
    if provider_revents & linux_abi::EPOLLIN != 0 {
        out |= requested & (linux_abi::EPOLLIN | linux_abi::EPOLLRDNORM);
    }
    if provider_revents & linux_abi::EPOLLOUT != 0 {
        out |= requested & (linux_abi::EPOLLOUT | linux_abi::EPOLLWRNORM);
    }
    out
}

fn decode_snapshot_entry(bytes: &[u8]) -> WaitSetSnapshotWire {
    debug_assert!(bytes.len() >= WAITSET_SNAPSHOT_SIZE);
    // SAFETY: the caller sliced at least `WAITSET_SNAPSHOT_SIZE` bytes and
    // `WaitSetSnapshotWire` is plain `repr(C)` data valid for any bit pattern;
    // `read_unaligned` tolerates the byte buffer's alignment.
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<WaitSetSnapshotWire>()) }
}

fn collect_epoll_readiness(
    epoll_token: u64,
    maxevents: usize,
    deadline_tick: Option<u64>,
    mode: EpollScanMode,
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
        || response.payload_len as usize != count.saturating_mul(WAITSET_SNAPSHOT_SIZE)
    {
        debug::record_milestone(debug::LogCategory::Compat, "epoll-snapshot-bad-header", response.aux, ((count as u64) << 32) | u64::from(response.payload_len));
        return Err(LINUX_EINVAL);
    }
    let mut interests = Vec::with_capacity(count);
    for index in 0..count {
        let offset = index * WAITSET_SNAPSHOT_SIZE;
        let wire = decode_snapshot_entry(&response.payload[offset..offset + WAITSET_SNAPSHOT_SIZE]);
        if !waitset_snapshot_shape_valid(&wire) {
            debug::record_milestone(debug::LogCategory::Compat, "epoll-snapshot-bad-entry", index as u64, (u64::from(wire.state) << 32) | u64::from(wire.interest.events));
            return Err(LINUX_EINVAL);
        }
        // A provider restart advances its endpoint epoch. Report the interest
        // as ERR|HUP, keyed by the new epoch so an edge-triggered consumer
        // sees the revoke once rather than on every scan.
        let (revoked, epoch_generation) = match waitset_provider_epoch(wire.interest.provider) {
            Ok(current_epoch) => (
                current_epoch != wire.interest.provider_epoch,
                current_epoch.max(1),
            ),
            Err(errno) if provider_revoke_events(errno).is_some() => (true, 1),
            Err(errno) => return Err(errno),
        };
        interests.push((wire, revoked, epoch_generation));
    }

    let mut observations = Vec::with_capacity(interests.len().saturating_add(1));
    observations.push(
        super::super::broker_ops::waitset_broker_ops::ProviderObservation {
            provider: WAITSET_PROVIDER_VFSD,
            object_id: epoll_token,
            generation: response.aux,
        },
    );
    let revoke_events = linux_abi::EPOLLERR | linux_abi::EPOLLHUP;
    let mut scans = Vec::with_capacity(interests.len());
    let mut netd_slots = Vec::new();
    let mut netd_queries = Vec::new();
    for (index, (wire, revoked, epoch_generation)) in interests.iter().enumerate() {
        let interest = &wire.interest;
        if wire.state & WAITSET_SNAPSHOT_STATE_DISABLED != 0 {
            // A reported one-shot stays silent until EPOLL_CTL_MOD re-arms it,
            // and that mutation advances the epoll object's own generation.
            scans.push(InterestScan::Skip);
            continue;
        }
        if *revoked {
            scans.push(InterestScan::Ready {
                revents: revoke_events,
                generation: *epoch_generation,
            });
            continue;
        }
        let provider_events = epoll_provider_events(interest.events);
        if interest.provider == WAITSET_PROVIDER_NETD {
            netd_slots.push(index);
            netd_queries.push((interest.object_id, provider_events));
            scans.push(InterestScan::Netd);
            continue;
        }
        let queried = (|| -> Result<(u32, u64), i64> {
            Ok(match interest.provider {
                WAITSET_PROVIDER_INPUTD => {
                    let access =
                        super::super::broker_ops::waitset_broker_ops::input_open_description_access(
                            interest.object_id,
                        )
                        .ok_or(LINUX_EBADF)?;
                    let (is_ready, generation) = input_readiness_for_access(access, deadline_tick)?;
                    merge_provider_observation(
                        &mut observations,
                        super::super::broker_ops::waitset_broker_ops::ProviderObservation {
                            provider: WAITSET_PROVIDER_INPUTD,
                            object_id: waitset_input_object_id(access),
                            generation,
                        },
                    );
                    let revents = if is_ready {
                        provider_events & (linux_abi::EPOLLIN | linux_abi::EPOLLPRI)
                    } else {
                        0
                    };
                    (revents, generation)
                }
                WAITSET_PROVIDER_SESSIOND => {
                    let stream = multitask::ConsoleHandle::stream_for_token(interest.object_id)
                        .ok_or(LINUX_EBADF)?;
                    let session = multitask::current_console_session()
                        .ok_or(LINUX_EBADF)?
                        .raw();
                    let (is_ready, live, generation) = console_readiness_via_sessiond_with_timeout(
                        session,
                        waitset_provider_query_timeout_ms(deadline_tick),
                    )?;
                    merge_provider_observation(
                        &mut observations,
                        super::super::broker_ops::waitset_broker_ops::ProviderObservation {
                            provider: WAITSET_PROVIDER_SESSIOND,
                            object_id: session,
                            generation,
                        },
                    );
                    (
                        console_ready_events(stream, live, is_ready, provider_events),
                        generation,
                    )
                }
                _ => return Err(LINUX_EIO),
            })
        })();
        scans.push(match queried {
            Ok((revents, generation)) => InterestScan::Ready { revents, generation },
            Err(LINUX_ETIMEDOUT) => InterestScan::TimedOut,
            Err(errno) if provider_revoke_events(errno).is_some() => InterestScan::Ready {
                revents: revoke_events,
                generation: 1,
            },
            Err(errno) => {
                crate::debug::info!(
                    compat,
                    "epoll provider query failed: provider={} object={:#x} errno={}",
                    interest.provider,
                    interest.object_id,
                    errno
                );
                return Err(errno);
            }
        });
    }

    // Every netd-backed interest is answered by one exchange, not one each.
    if !netd_queries.is_empty() {
        match poll_netd_socket_batch(&netd_queries, waitset_provider_query_timeout_ms(deadline_tick)) {
            Ok(outcomes) => {
                for (slot, ((token, _), outcome)) in
                    netd_slots.iter().zip(netd_queries.iter().zip(outcomes))
                {
                    scans[*slot] = match outcome {
                        Ok((revents, generation)) => {
                            merge_provider_observation(
                                &mut observations,
                                super::super::broker_ops::waitset_broker_ops::ProviderObservation {
                                    provider: WAITSET_PROVIDER_NETD,
                                    object_id: *token,
                                    generation,
                                },
                            );
                            InterestScan::Ready { revents, generation }
                        }
                        Err(errno) if provider_revoke_events(errno).is_some() => {
                            InterestScan::Ready {
                                revents: revoke_events,
                                generation: 1,
                            }
                        }
                        Err(LINUX_ETIMEDOUT) => InterestScan::TimedOut,
                        Err(errno) => return Err(errno),
                    };
                }
            }
            Err(LINUX_ETIMEDOUT) => {
                for slot in &netd_slots {
                    scans[*slot] = InterestScan::TimedOut;
                }
            }
            Err(errno) if provider_revoke_events(errno).is_some() => {
                for slot in &netd_slots {
                    scans[*slot] = InterestScan::Ready {
                        revents: revoke_events,
                        generation: 1,
                    };
                }
            }
            Err(errno) => return Err(errno),
        }
    }

    let mut provider_timed_out = false;
    let mut candidates = Vec::new();
    for ((wire, _, _), scan) in interests.iter().zip(scans) {
        let interest = &wire.interest;
        let (revents, generation) = match scan {
            InterestScan::Skip | InterestScan::Netd => continue,
            InterestScan::TimedOut => {
                provider_timed_out = true;
                continue;
            }
            InterestScan::Ready { revents, generation } => (revents, generation),
        };
        let reported = epoll_report_events(revents, interest.events);
        if reported == 0 {
            continue;
        }
        let edge = interest.events & linux_abi::EPOLLET != 0;
        let oneshot = interest.events & linux_abi::EPOLLONESHOT != 0;
        // An edge was already reported when no newer provider generation exists.
        if edge && generation <= wire.edge_generation {
            continue;
        }
        candidates.push(EpollCandidate {
            events: reported,
            data: interest.data,
            claim: WaitSetClaimWire {
                provider: interest.provider,
                target_fd: interest.target_fd,
                object_id: interest.object_id,
                generation,
                ..WaitSetClaimWire::default()
            },
            gated: edge || oneshot,
        });
    }

    let ready = select_epoll_events(epoll_token, &candidates, maxevents, mode)?;
    require_completed_provider_scan(provider_timed_out, !ready.is_empty())?;
    Ok(EpollReadinessState {
        ready,
        observations,
    })
}

/// Pick up to `maxevents` events in snapshot order. Level-triggered candidates
/// are taken as they are; edge-triggered and one-shot ones are claimed in
/// vfsd first, and a candidate another waiter already claimed is replaced by
/// the next one so a lost race never turns a ready set into an empty result.
fn select_epoll_events(
    epoll_token: u64,
    candidates: &[EpollCandidate],
    maxevents: usize,
    mode: EpollScanMode,
) -> Result<Vec<(u32, u64)>, i64> {
    let mut ready = Vec::new();
    let mut cursor = 0;
    while ready.len() < maxevents && cursor < candidates.len() {
        let take = (maxevents - ready.len()).min(candidates.len() - cursor);
        let round = &candidates[cursor..cursor + take];
        cursor += take;
        let claims = if mode == EpollScanMode::Consume {
            claim_epoll_reports(epoll_token, round)?
        } else {
            alloc::vec![true; round.iter().filter(|candidate| candidate.gated).count()]
        };
        let mut claimed = claims.into_iter();
        for candidate in round {
            if candidate.gated && !claimed.next().unwrap_or(false) {
                continue;
            }
            ready.push((candidate.events, candidate.data));
        }
    }
    Ok(ready)
}

/// Ask vfsd which of the gated candidates this caller owns. The answer is one
/// flag per gated candidate, in order. The exchange is not replayed: a lost
/// reply after vfsd consumed the report can drop one edge, which is why it
/// rides the interactive rail instead of the 16 ms readiness rail.
fn claim_epoll_reports(
    epoll_token: u64,
    candidates: &[EpollCandidate],
) -> Result<Vec<bool>, i64> {
    let gated = candidates
        .iter()
        .filter(|candidate| candidate.gated)
        .map(|candidate| candidate.claim)
        .collect::<Vec<_>>();
    let mut owned = Vec::with_capacity(gated.len());
    for chunk in gated.chunks(VFS_EPOLL_CLAIM_MAX) {
        let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
        request.arg0 = VFS_POLL_QUERY_EPOLL_CLAIM;
        request.remote_id = epoll_token;
        request.payload_len = (chunk.len() * WAITSET_CLAIM_SIZE) as u32;
        for (index, claim) in chunk.iter().enumerate() {
            let offset = index * WAITSET_CLAIM_SIZE;
            request.payload[offset..offset + 2].copy_from_slice(&claim.provider.to_le_bytes());
            request.payload[offset + 2..offset + 4].copy_from_slice(&claim.reserved0.to_le_bytes());
            request.payload[offset + 4..offset + 8].copy_from_slice(&claim.reserved1.to_le_bytes());
            request.payload[offset + 8..offset + 16].copy_from_slice(&claim.target_fd.to_le_bytes());
            request.payload[offset + 16..offset + 24].copy_from_slice(&claim.object_id.to_le_bytes());
            request.payload[offset + 24..offset + 32].copy_from_slice(&claim.generation.to_le_bytes());
        }
        let response = call_vfs_ipc_request_with_timeout(&request, control::deadline_ms())?;
        ensure_vfs_status(&response)?;
        if response.payload_len as usize != chunk.len() || response.value > chunk.len() as u64 {
            debug::record_milestone(debug::LogCategory::Compat, "epoll-claim-bad-reply", u64::from(response.payload_len), response.value);
            return Err(LINUX_EINVAL);
        }
        owned.extend(response.payload[..chunk.len()].iter().map(|flag| *flag != 0));
    }
    Ok(owned)
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

pub fn retire_vfs_epoll(token: u64) -> Result<(), i64> {
    retire_vfs_epoll_bounded(token)
}

pub fn retire_vfs_epoll_bounded(token: u64) -> Result<(), i64> {
    retire_vfs_epoll_with_timeout(token, Some(control::deadline_ms()))
}

fn retire_vfs_epoll_with_timeout(token: u64, timeout_ms: Option<u64>) -> Result<(), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = VFS_POLL_QUERY_EPOLL_RETIRE;
    request.remote_id = token;
    let response = match timeout_ms {
        Some(timeout_ms) => call_vfs_ipc_request_with_timeout(&request, timeout_ms),
        None => call_vfs_ipc_request(&request),
    }?;
    ensure_vfs_status(&response)
}

pub fn purge_vfs_epoll_object(provider: u16, object_id: u64) -> Result<(), i64> {
    purge_vfs_epoll_object_bounded(provider, object_id)
}

pub fn purge_vfs_epoll_object_bounded(provider: u16, object_id: u64) -> Result<(), i64> {
    purge_vfs_epoll_object_with_timeout(provider, object_id, Some(control::deadline_ms()))
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
mod tests;
