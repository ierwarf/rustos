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
use alloc::vec;

// Every provider-backed descriptor in one poll arm must fit in the exact
// object observation set installed in ring0.  Reject larger scans instead of
// silently collapsing them to a provider-wide wake bucket.
const MAX_POLL_FDS: usize = WAITSET_MAX_INTERESTS;
const POLLFD_SIZE: usize = size_of::<linux_abi::LinuxPollFd>();
const EPOLL_EVENT_SIZE: usize = size_of::<linux_abi::LinuxEpollEvent>();
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
        u32::from(u16::from_le_bytes(event_bytes)),
    ))
}

const SELECT_MAX_FDS: usize = 4096;
const SELECT_POLLIN_EVENTS: u32 = (linux_abi::POLLIN as u32)
    | linux_abi::EPOLLRDNORM
    | linux_abi::EPOLLRDBAND
    | (linux_abi::POLLHUP as u32)
    | (linux_abi::POLLERR as u32);
const SELECT_POLLOUT_EVENTS: u32 = (linux_abi::POLLOUT as u32)
    | linux_abi::EPOLLWRNORM
    | linux_abi::EPOLLWRBAND
    | (linux_abi::POLLERR as u32);
const SELECT_POLLEX_EVENTS: u32 = linux_abi::POLLPRI as u32;

pub fn syscall_linux_poll(fds_ptr: u64, nfds: u64, timeout_ms: i64) -> u64 {
    linux_result(poll_fds_timeout(fds_ptr, nfds, timeout_ms))
}

fn poll_fds_timeout(fds_ptr: u64, nfds: u64, timeout_ms: i64) -> LinuxResult<u64> {
    let deadline_tick = (timeout_ms >= 0).then(|| deadline_tick_from_ms(timeout_ms as u64));
    wait_until_poll_ready(timeout_ms == 0, deadline_tick, || {
        collect_poll_readiness(fds_ptr, nfds, deadline_tick)
    })
}

fn deadline_tick_from_ms_at(timeout_ms: u64, now_ticks: u64, ticks_per_second: u64) -> u64 {
    let tps = u128::from(ticks_per_second.max(1));
    let timeout_ticks = (u128::from(timeout_ms) * tps).div_ceil(1000);
    let ticks = u64::try_from(timeout_ticks).unwrap_or(u64::MAX);
    now_ticks.saturating_add(ticks)
}

fn deadline_tick_from_ms(timeout_ms: u64) -> u64 {
    deadline_tick_from_ms_at(
        timeout_ms,
        crate::arch::rtc::ticks(),
        crate::arch::rtc::ticks_per_second(),
    )
}

fn deadline_tick_from_nanos_at(nanos: u128, now_ticks: u64, ticks_per_second: u64) -> u64 {
    if nanos == 0 {
        return now_ticks;
    }
    let tps = u128::from(ticks_per_second.max(1));
    let timeout_ticks = nanos.saturating_mul(tps).div_ceil(1_000_000_000);
    let ticks = u64::try_from(timeout_ticks).unwrap_or(u64::MAX);
    now_ticks.saturating_add(ticks.max(1))
}

fn deadline_tick_from_nanos(nanos: u128) -> u64 {
    deadline_tick_from_nanos_at(
        nanos,
        crate::arch::rtc::ticks(),
        crate::arch::rtc::ticks_per_second(),
    )
}

fn wait_until_poll_ready(
    immediate: bool,
    deadline_tick: Option<u64>,
    mut collect: impl FnMut() -> Result<PollReadinessState, i64>,
) -> Result<u64, i64> {
    loop {
        let first = match collect() {
            Ok(state) => state,
            Err(LINUX_ETIMEDOUT)
                if deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) =>
            {
                return Ok(0);
            }
            Err(LINUX_ETIMEDOUT) => {
                if current_wait_interrupted() {
                    return Err(LINUX_EINTR);
                }
                multitask::cond_resched();
                continue;
            }
            Err(errno) => return Err(errno),
        };
        if first.ready != 0 {
            return Ok(first.ready);
        }
        if immediate
            || deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline)
        {
            return Ok(0);
        }
        if current_wait_interrupted() {
            return Err(LINUX_EINTR);
        }
        let Some(task_id) = multitask::current_task_id() else {
            return Err(LINUX_EINVAL);
        };
        let Some(process_id) = multitask::current_user_process_id() else {
            return Err(LINUX_EINVAL);
        };
        if !first.observations.is_empty() {
            super::super::broker_ops::waitset_broker_ops::register_waitset_waiters(
                task_id,
                process_id,
                &first.observations,
            )?;
        }

        let second = match collect() {
            Ok(state) => state,
            Err(LINUX_ETIMEDOUT)
                if deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) =>
            {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                return Ok(0);
            }
            Err(LINUX_ETIMEDOUT) => {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                if current_wait_interrupted() {
                    return Err(LINUX_EINTR);
                }
                multitask::cond_resched();
                continue;
            }
            Err(errno) => {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                return Err(errno);
            }
        };
        if second.ready != 0
            || second.observations != first.observations
            || deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline)
        {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            if second.ready != 0 {
                return Ok(second.ready);
            }
            continue;
        }
        if !multitask::arm_block_current_task() {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            if multitask::current_task_id() != Some(task_id) {
                return Err(LINUX_EINVAL);
            }
            multitask::cond_resched();
            continue;
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
        if has_unmasked_pending_signal() {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            let _ = multitask::cancel_block_current_task();
            if current_wait_interrupted() {
                return Err(LINUX_EINTR);
            }
            continue;
        }
        if let Some(deadline) = deadline_tick
            && !crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, deadline)
        {
            super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
            let _ = multitask::cancel_block_current_task();
            return Err(LINUX_EBUSY);
        }
        match multitask::commit_block_current_task_and_yield() {
            Some(true) | Some(false) => {}
            None => {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                crate::arch::rtc::disarm_sleep_waiter(task_id);
                return Err(LINUX_EINVAL);
            }
        }
        super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
        crate::arch::rtc::disarm_sleep_waiter(task_id);
        if current_wait_interrupted() {
            return Err(LINUX_EINTR);
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
    VfsKeyed { remote_id: u64 },
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
    let mut vfs_keyed_slots = Vec::new();
    let mut vfs_keyed_queries = Vec::new();
    for index in 0..nfds {
        let entry_ptr = fds_ptr
            .checked_add((index * POLLFD_SIZE) as u64)
            .ok_or(LINUX_EFAULT)?;
        let mut raw = [0_u8; POLLFD_SIZE];
        read_user_bytes(entry_ptr, &mut raw)?;
        let (fd, events) = decode_pollfd(&raw)?;
        push_poll_entry(
            &mut entries,
            &mut netd_slots,
            &mut netd_queries,
            &mut vfs_slots,
            &mut vfs_keyed_slots,
            &mut vfs_keyed_queries,
            entry_ptr,
            raw,
            fd,
            events,
            deadline_tick,
            &mut state,
        )?;
    }
    evaluate_poll_entries(
        &mut entries,
        &netd_slots,
        &netd_queries,
        &vfs_slots,
        &vfs_keyed_slots,
        &vfs_keyed_queries,
        deadline_tick,
        &mut state,
    )?;
    for entry in &entries {
        if entry.revents != 0 {
            state.ready += 1;
        }
        // Linux stores only `revents`; rewriting fd/events would clobber a
        // concurrent edit of those fields, and an unchanged value needs no
        // store (the page may not even be writable then).
        let revents_bytes = (entry.revents as i16).to_le_bytes();
        if entry.raw[6..8] != revents_bytes {
            write_user_bytes(entry.ptr + 6, &revents_bytes)?;
        }
    }
    Ok(state)
}

#[allow(clippy::too_many_arguments)]
fn push_poll_entry(
    entries: &mut Vec<PollEntry>,
    netd_slots: &mut Vec<usize>,
    netd_queries: &mut Vec<(u64, u32)>,
    vfs_slots: &mut Vec<usize>,
    vfs_keyed_slots: &mut Vec<usize>,
    vfs_keyed_queries: &mut Vec<(i32, u32, u64)>,
    ptr: u64,
    raw: [u8; POLLFD_SIZE],
    fd: i32,
    events: u32,
    deadline_tick: Option<u64>,
    state: &mut PollReadinessState,
) -> Result<u32, i64> {
    let index = entries.len();
    let route = if fd < 0 {
        PollRoute::Done(0)
    } else {
        match route_one_poll_fd(fd as u64, events, deadline_tick, state) {
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
        PollRoute::VfsKeyed { remote_id } => {
            vfs_keyed_slots.push(index);
            vfs_keyed_queries.push((fd, events, remote_id));
            0
        }
    };
    entries.push(PollEntry {
        ptr,
        raw,
        fd,
        events,
        revents,
    });
    Ok(revents)
}

#[allow(clippy::too_many_arguments)]
fn evaluate_poll_entries(
    entries: &mut [PollEntry],
    netd_slots: &[usize],
    netd_queries: &[(u64, u32)],
    vfs_slots: &[usize],
    vfs_keyed_slots: &[usize],
    vfs_keyed_queries: &[(i32, u32, u64)],
    deadline_tick: Option<u64>,
    state: &mut PollReadinessState,
) -> Result<(), i64> {
    resolve_poll_netd_batch(entries, netd_slots, netd_queries, deadline_tick, state)?;
    resolve_poll_vfs_batch(entries, vfs_slots, deadline_tick, state)?;
    resolve_poll_vfs_keyed_batch(entries, vfs_keyed_slots, vfs_keyed_queries, deadline_tick, state)?;
    if state.observations.len()
        > super::super::broker_ops::waitset_broker_ops::WAITSET_MAX_OBSERVATIONS
    {
        return Err(LINUX_EINVAL);
    }
    let has_ready = entries.iter().any(|entry| entry.revents != 0);
    require_completed_provider_scan(state.provider_timed_out, has_ready)
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

fn resolve_poll_vfs_keyed_batch(
    entries: &mut [PollEntry],
    slots: &[usize],
    queries: &[(i32, u32, u64)],
    deadline_tick: Option<u64>,
    state: &mut PollReadinessState,
) -> Result<(), i64> {
    for (slot_chunk, query_chunk) in slots
        .chunks(rustos_user_abi::syscall::VFS_POLL_KEYED_BATCH_MAX)
        .zip(queries.chunks(rustos_user_abi::syscall::VFS_POLL_KEYED_BATCH_MAX))
    {
        match poll_vfs_keyed_batch(query_chunk, deadline_tick) {
            Ok((revents, generation)) => {
                for ((slot, &(_, _, remote_id)), revents) in
                    slot_chunk.iter().zip(query_chunk.iter()).zip(revents)
                {
                    note_poll_observation(state, WAITSET_PROVIDER_VFSD, remote_id, generation);
                    entries[*slot].revents = revents;
                }
            }
            Err(LINUX_ETIMEDOUT) => state.provider_timed_out = true,
            Err(errno) if provider_revoke_events(errno).is_some() => {
                for slot in slot_chunk {
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
        multitask::KernelHandle::Pipe(_)
        | multitask::KernelHandle::EventFd(_)
        | multitask::KernelHandle::UnixSocket(_) => {
            // Kernel-local object: readiness is read directly, with no
            // provider round trip, and its generation arms the waiter.
            let token = super::local_fd::waitset_object(&handle).ok_or(LINUX_EBADF)?;
            let Some(readiness) = kernel_ps::api::local_fd_readiness_of(token) else {
                return Ok(PollRoute::Done(linux_abi::POLLNVAL as u32));
            };
            note_poll_observation(state, WAITSET_PROVIDER_KERNEL, token, readiness.generation);
            Ok(PollRoute::Done(epoll_report_events(readiness.events, events)))
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
        multitask::KernelHandle::RemoteVfs(remote)
            if remote.kind() == multitask::RemoteVfsHandleKind::Inotify =>
        {
            Ok(PollRoute::VfsKeyed {
                remote_id: remote.remote_id(),
            })
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

pub(super) fn has_unmasked_pending_signal() -> bool {
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
        let value = u16::from_le_bytes(
            response.payload[offset + 6..offset + 8]
                .try_into()
                .map_err(|_| LINUX_EINVAL)?,
        );
        revents.push(u32::from(value));
    }
    Ok(revents)
}

fn poll_vfs_keyed_batch(
    queries: &[(i32, u32, u64)],
    deadline_tick: Option<u64>,
) -> Result<(Vec<u32>, u64), i64> {
    const ENTRY_SIZE: usize = 16;
    if queries.is_empty() || queries.len() > rustos_user_abi::syscall::VFS_POLL_KEYED_BATCH_MAX {
        return Err(LINUX_EINVAL);
    }
    let mut request = new_vfs_request(VFS_IPC_OP_POLL_QUERY);
    request.arg0 = rustos_user_abi::syscall::VFS_POLL_QUERY_POLL_KEYED;
    request.payload_len = (queries.len() * ENTRY_SIZE) as u32;
    for (index, (fd, events, remote_id)) in queries.iter().copied().enumerate() {
        let offset = index * ENTRY_SIZE;
        request.payload[offset..offset + 4].copy_from_slice(&fd.to_le_bytes());
        request.payload[offset + 4..offset + 6].copy_from_slice(&(events as i16).to_le_bytes());
        request.payload[offset + 6..offset + 8].copy_from_slice(&0_i16.to_le_bytes());
        request.payload[offset + 8..offset + 16].copy_from_slice(&remote_id.to_le_bytes());
    }
    let response = call_vfs_ipc_request_with_timeout(
        &request,
        waitset_provider_query_timeout_ms(deadline_tick),
    )?;
    ensure_vfs_status(&response)?;
    if response.payload_len as usize != queries.len() * ENTRY_SIZE {
        return Err(LINUX_EINVAL);
    }
    let generation = response.aux.max(1);
    let mut revents = Vec::with_capacity(queries.len());
    for index in 0..queries.len() {
        let offset = index * ENTRY_SIZE;
        let value = i16::from_le_bytes(
            response.payload[offset + 6..offset + 8]
                .try_into()
                .map_err(|_| LINUX_EINVAL)?,
        );
        revents.push(u32::from(value as u16));
    }
    Ok((revents, generation))
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

fn read_timespec_timeout_ms(timeout_ptr: u64) -> LinuxResult<i64> {
    let Some(ts) = read_user_optional_struct::<LinuxTimespecWire>(timeout_ptr)? else {
        return Ok(-1);
    };
    process_time::validate_relative_sleep_timespec_locally(ts)?;
    Ok(timespec_to_timeout_ms(ts))
}

fn timespec_to_timeout_ms(ts: LinuxTimespecWire) -> i64 {
    ts.tv_sec
        .saturating_mul(1000)
        .saturating_add((ts.tv_nsec + 999_999) / 1_000_000)
}

fn read_pselect6_timeout(timeout_ptr: u64) -> LinuxResult<(bool, Option<u64>)> {
    let Some(ts) = read_user_optional_struct::<LinuxTimespecWire>(timeout_ptr)? else {
        return Ok((false, None));
    };
    process_time::validate_relative_sleep_timespec_locally(ts)?;
    let total_nanos = (ts.tv_sec as u128)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u128);
    if total_nanos == 0 {
        Ok((true, Some(crate::arch::rtc::ticks())))
    } else {
        Ok((false, Some(deadline_tick_from_nanos(total_nanos))))
    }
}

fn parse_select_timeout(timeout_ptr: u64) -> LinuxResult<(bool, Option<u64>)> {
    let Some(tv) = read_user_optional_struct::<linux_abi::LinuxTimeval>(timeout_ptr)? else {
        return Ok((false, None));
    };
    if tv.tv_sec < 0 || tv.tv_usec < 0 {
        return Err(LINUX_EINVAL);
    }
    let total_nanos = (tv.tv_sec as u128)
        .saturating_mul(1_000_000_000)
        .saturating_add((tv.tv_usec as u128).saturating_mul(1_000));
    if total_nanos == 0 {
        Ok((true, Some(crate::arch::rtc::ticks())))
    } else {
        Ok((false, Some(deadline_tick_from_nanos(total_nanos))))
    }
}

fn remaining_timeval_from_ticks(
    remaining_ticks: u64,
    ticks_per_second: u64,
) -> linux_abi::LinuxTimeval {
    let tps = u128::from(ticks_per_second.max(1));
    let remaining_usec = (u128::from(remaining_ticks) * 1_000_000) / tps;
    match i64::try_from(remaining_usec / 1_000_000) {
        Ok(tv_sec) => linux_abi::LinuxTimeval {
            tv_sec,
            tv_usec: (remaining_usec % 1_000_000) as i64,
        },
        Err(_) => linux_abi::LinuxTimeval {
            tv_sec: i64::MAX,
            tv_usec: 999_999,
        },
    }
}

pub fn syscall_linux_ppoll(
    fds_ptr: u64,
    nfds: u64,
    timeout_ptr: u64,
    sigmask_ptr: u64,
    sigset_size: u64,
) -> u64 {
    linux_result((|| {
        let timeout_ms = read_timespec_timeout_ms(timeout_ptr)?;
        with_wait_signal_mask_result(sigmask_ptr, sigset_size, || {
            poll_fds_timeout(fds_ptr, nfds, timeout_ms)
        })
    })())
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SelectRequestEntry {
    fd: i32,
    events: u32,
    in_read: bool,
    in_write: bool,
    in_except: bool,
}

/// Linux `select(nfds, readfds, writefds, exceptfds, timeout)`.
pub fn syscall_linux_select(
    nfds: u64,
    readfds_ptr: u64,
    writefds_ptr: u64,
    exceptfds_ptr: u64,
    timeout_ptr: u64,
) -> u64 {
    linux_result((|| {
        let (immediate, deadline_tick) = parse_select_timeout(timeout_ptr)?;
        let ready = execute_select(
            nfds,
            readfds_ptr,
            writefds_ptr,
            exceptfds_ptr,
            immediate,
            deadline_tick,
        )?;
        if timeout_ptr != 0 {
            let remaining_ticks = deadline_tick
                .map(|deadline| deadline.saturating_sub(crate::arch::rtc::ticks()))
                .unwrap_or(0);
            let remaining = remaining_timeval_from_ticks(
                remaining_ticks,
                crate::arch::rtc::ticks_per_second(),
            );
            write_user_struct(timeout_ptr, &remaining)?;
        }
        Ok(ready)
    })())
}

/// Linux `pselect6(nfds, readfds, writefds, exceptfds, timeout, sigmask_arg)`.
pub fn syscall_linux_pselect6(
    nfds: u64,
    readfds_ptr: u64,
    writefds_ptr: u64,
    exceptfds_ptr: u64,
    timeout_ptr: u64,
    sigmask_arg_ptr: u64,
) -> u64 {
    linux_result((|| {
        let (immediate, deadline_tick) = read_pselect6_timeout(timeout_ptr)?;
        let [ss_ptr, ss_len] =
            read_user_optional_struct::<[u64; 2]>(sigmask_arg_ptr)?.unwrap_or([0, 0]);
        with_wait_signal_mask_result(ss_ptr, ss_len, || {
            execute_select(
                nfds,
                readfds_ptr,
                writefds_ptr,
                exceptfds_ptr,
                immediate,
                deadline_tick,
            )
        })
    })())
}

fn execute_select(
    nfds: u64,
    readfds_ptr: u64,
    writefds_ptr: u64,
    exceptfds_ptr: u64,
    immediate: bool,
    deadline_tick: Option<u64>,
) -> LinuxResult<u64> {
    let nfds_i64 = nfds as i64;
    if nfds_i64 < 0 || nfds_i64 as usize > SELECT_MAX_FDS {
        return Err(LINUX_EINVAL);
    }
    let nfds = nfds_i64 as usize;
    let word_count = nfds.div_ceil(64);
    let read_words = read_select_fd_set(readfds_ptr, word_count)?;
    let write_words = read_select_fd_set(writefds_ptr, word_count)?;
    let except_words = read_select_fd_set(exceptfds_ptr, word_count)?;

    let requests = decode_select_queries(nfds, &read_words, &write_words, &except_words);
    if requests.len() > MAX_POLL_FDS {
        return Err(LINUX_EINVAL);
    }
    for req in &requests {
        if current_kernel_handle(req.fd as u64).is_none() {
            return Err(LINUX_EBADF);
        }
    }

    let mut last_revents = vec![0_u32; requests.len()];
    wait_until_poll_ready(immediate, deadline_tick, || {
        collect_select_readiness(&requests, deadline_tick, &mut last_revents)
    })?;

    let (ready_bits, out_read, out_write, out_except) = encode_select_results(
        &requests,
        &last_revents,
        word_count,
        readfds_ptr != 0,
        writefds_ptr != 0,
        exceptfds_ptr != 0,
    );
    write_select_fd_set(readfds_ptr, &out_read)?;
    write_select_fd_set(writefds_ptr, &out_write)?;
    write_select_fd_set(exceptfds_ptr, &out_except)?;
    Ok(ready_bits as u64)
}

fn read_select_fd_set(ptr: u64, word_count: usize) -> Result<Vec<u64>, i64> {
    if ptr == 0 || word_count == 0 {
        return Ok(Vec::new());
    }
    let byte_len = word_count
        .checked_mul(size_of::<u64>())
        .ok_or(LINUX_EINVAL)?;
    let mut raw = vec![0_u8; byte_len];
    read_user_bytes(ptr, &mut raw)?;
    Ok(raw
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
        .collect())
}

fn write_select_fd_set(ptr: u64, words: &[u64]) -> Result<(), i64> {
    if ptr == 0 || words.is_empty() {
        return Ok(());
    }
    let mut raw = Vec::with_capacity(words.len() * 8);
    for word in words {
        raw.extend_from_slice(&word.to_le_bytes());
    }
    write_user_bytes(ptr, &raw)
}

fn decode_select_queries(
    nfds: usize,
    read_words: &[u64],
    write_words: &[u64],
    except_words: &[u64],
) -> Vec<SelectRequestEntry> {
    let mut requests = Vec::new();
    for fd in 0..nfds {
        let word_idx = fd / 64;
        let bit = 1_u64 << (fd % 64);
        let in_read = read_words.get(word_idx).is_some_and(|w| w & bit != 0);
        let in_write = write_words.get(word_idx).is_some_and(|w| w & bit != 0);
        let in_except = except_words.get(word_idx).is_some_and(|w| w & bit != 0);
        if !in_read && !in_write && !in_except {
            continue;
        }
        let mut events = 0_u32;
        if in_read {
            events |= SELECT_POLLIN_EVENTS;
        }
        if in_write {
            events |= SELECT_POLLOUT_EVENTS;
        }
        if in_except {
            events |= SELECT_POLLEX_EVENTS;
        }
        requests.push(SelectRequestEntry {
            fd: fd as i32,
            events,
            in_read,
            in_write,
            in_except,
        });
    }
    requests
}

fn select_matched_bit_count(req: SelectRequestEntry, revents: u32) -> usize {
    let mut bits = 0usize;
    if req.in_read && (revents & SELECT_POLLIN_EVENTS) != 0 {
        bits += 1;
    }
    if req.in_write && (revents & SELECT_POLLOUT_EVENTS) != 0 {
        bits += 1;
    }
    if req.in_except && (revents & SELECT_POLLEX_EVENTS) != 0 {
        bits += 1;
    }
    bits
}

fn encode_select_results(
    requests: &[SelectRequestEntry],
    revents: &[u32],
    word_count: usize,
    has_read: bool,
    has_write: bool,
    has_except: bool,
) -> (usize, Vec<u64>, Vec<u64>, Vec<u64>) {
    let mut out_read = if has_read { vec![0_u64; word_count] } else { Vec::new() };
    let mut out_write = if has_write { vec![0_u64; word_count] } else { Vec::new() };
    let mut out_except = if has_except { vec![0_u64; word_count] } else { Vec::new() };
    let mut ready_bits = 0usize;
    for (req, &rev) in requests.iter().zip(revents.iter()) {
        let fd = req.fd as usize;
        let word_idx = fd / 64;
        let bit = 1_u64 << (fd % 64);
        if has_read && req.in_read && (rev & SELECT_POLLIN_EVENTS) != 0 {
            out_read[word_idx] |= bit;
            ready_bits += 1;
        }
        if has_write && req.in_write && (rev & SELECT_POLLOUT_EVENTS) != 0 {
            out_write[word_idx] |= bit;
            ready_bits += 1;
        }
        if has_except && req.in_except && (rev & SELECT_POLLEX_EVENTS) != 0 {
            out_except[word_idx] |= bit;
            ready_bits += 1;
        }
    }
    (ready_bits, out_read, out_write, out_except)
}

fn collect_select_readiness(
    requests: &[SelectRequestEntry],
    deadline_tick: Option<u64>,
    last_revents: &mut [u32],
) -> Result<PollReadinessState, i64> {
    let mut state = PollReadinessState {
        ready: 0,
        provider_timed_out: false,
        observations: Vec::with_capacity(requests.len().min(WAITSET_MAX_INTERESTS)),
    };
    let mut entries = Vec::with_capacity(requests.len());
    let mut netd_slots = Vec::new();
    let mut netd_queries = Vec::new();
    let mut vfs_slots = Vec::new();
    let mut vfs_keyed_slots = Vec::new();
    let mut vfs_keyed_queries = Vec::new();
    for req in requests {
        let rev = push_poll_entry(
            &mut entries,
            &mut netd_slots,
            &mut netd_queries,
            &mut vfs_slots,
            &mut vfs_keyed_slots,
            &mut vfs_keyed_queries,
            0,
            [0_u8; POLLFD_SIZE],
            req.fd,
            req.events,
            deadline_tick,
            &mut state,
        )?;
        if rev & (linux_abi::POLLNVAL as u32) != 0 {
            return Err(LINUX_EBADF);
        }
    }
    evaluate_poll_entries(
        &mut entries,
        &netd_slots,
        &netd_queries,
        &vfs_slots,
        &vfs_keyed_slots,
        &vfs_keyed_queries,
        deadline_tick,
        &mut state,
    )?;
    for ((req, entry), out_rev) in requests.iter().zip(entries.iter()).zip(last_revents.iter_mut()) {
        if entry.revents & (linux_abi::POLLNVAL as u32) != 0 {
            return Err(LINUX_EBADF);
        }
        *out_rev = entry.revents;
        state.ready += select_matched_bit_count(*req, entry.revents) as u64;
    }
    Ok(state)
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

/// `epoll_pwait2`: `epoll_pwait` with a `timespec` timeout (`NULL` blocks).
/// The timeout is rounded up to whole milliseconds, the resolution of the wait.
pub fn syscall_linux_epoll_pwait2(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    timeout_ptr: u64,
    sigmask_ptr: u64,
    sigset_size: u64,
) -> u64 {
    let timeout_ms = match read_timespec_timeout_ms(timeout_ptr) {
        Ok(timeout_ms) => timeout_ms,
        Err(errno) => return linux_errno(errno),
    };
    syscall_linux_epoll_pwait(epfd, events_ptr, maxevents, timeout_ms, sigmask_ptr, sigset_size)
}

struct WaitSignalMaskGuard {
    previous: Option<u64>,
}

impl WaitSignalMaskGuard {
    fn arm(sigmask_ptr: u64, sigset_size: u64) -> LinuxResult<Self> {
        if sigmask_ptr == 0 {
            return Ok(Self { previous: None });
        }
        if sigset_size != size_of::<u64>() as u64 {
            return Err(LINUX_EINVAL);
        }
        let requested = sanitize_wait_signal_mask(read_user_struct::<u64>(sigmask_ptr)?);
        let previous = multitask::with_current_user_process_and_linux_thread_state_mut(
            |_, _, _, _, thread_state| {
                thread_state.as_mut().map(|state| {
                    let previous = state.signal_mask;
                    state.signal_mask = requested;
                    previous
                })
            },
        )
        .flatten()
        .ok_or(LINUX_EINVAL)?;
        Ok(Self {
            previous: Some(previous),
        })
    }

    fn restore(&mut self) -> LinuxResult<()> {
        let Some(previous) = self.previous.take() else {
            return Ok(());
        };
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
            Ok(())
        } else {
            Err(LINUX_EINVAL)
        }
    }
}

impl Drop for WaitSignalMaskGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn with_wait_signal_mask_result(
    sigmask_ptr: u64,
    sigset_size: u64,
    wait: impl FnOnce() -> LinuxResult<u64>,
) -> LinuxResult<u64> {
    let mut guard = WaitSignalMaskGuard::arm(sigmask_ptr, sigset_size)?;
    let result = wait();
    guard.restore()?;
    result
}

fn with_temporary_wait_signal_mask(
    sigmask_ptr: u64,
    sigset_size: u64,
    wait: impl FnOnce() -> u64,
) -> u64 {
    let mut guard = match WaitSignalMaskGuard::arm(sigmask_ptr, sigset_size) {
        Ok(guard) => guard,
        Err(errno) => return linux_errno(errno),
    };
    let result = wait();
    match guard.restore() {
        Ok(()) => result,
        Err(errno) => linux_errno(errno),
    }
}

fn sanitize_wait_signal_mask(mask: u64) -> u64 {
    let unblockable = (1_u64 << (linux_abi::SIGKILL - 1)) | (1_u64 << (linux_abi::SIGSTOP - 1));
    mask & !unblockable
}

fn epoll_error_to_linux_errno(err: multitask::EpollError) -> i64 {
    match err {
        multitask::EpollError::Busy => LINUX_EBUSY,
        multitask::EpollError::Capacity => LINUX_ENOSPC,
        multitask::EpollError::Exists => LINUX_EEXIST,
        multitask::EpollError::InvalidArgument => LINUX_EINVAL,
        multitask::EpollError::NotFound => LINUX_ENOENT,
    }
}

pub fn syscall_linux_epoll_create1(flags: u64) -> u64 {
    linux_result((|| {
        if flags & !linux_abi::EPOLL_CLOEXEC != 0 {
            return Err(LINUX_EINVAL);
        }
        let epoll = multitask::EpollHandle::create().map_err(|err| match err {
            multitask::EpollError::Capacity => LINUX_EMFILE,
            other => epoll_error_to_linux_errno(other),
        })?;
        let epoll_token = epoll.token_id();
        let fd_flags = if flags & linux_abi::EPOLL_CLOEXEC != 0 {
            multitask::FD_CLOEXEC
        } else {
            0
        };
        let installed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
            process_state
                .handles_mut()
                .install_entry(multitask::HandleEntry::new(
                    multitask::KernelHandle::Epoll(epoll),
                    fd_flags,
                    linux_abi::O_RDONLY,
                ))
        });
        match installed {
            Some(Some(fd)) => Ok(fd),
            Some(None) => {
                retire_kernel_epoll(epoll_token);
                Err(LINUX_EMFILE)
            }
            None => {
                retire_kernel_epoll(epoll_token);
                Err(LINUX_ENOSYS)
            }
        }
    })())
}

pub fn syscall_linux_epoll_ctl(epfd: u64, op: u64, fd: u64, event_ptr: u64) -> u64 {
    linux_unit_result((|| {
        if epfd == fd {
            return Err(LINUX_EINVAL);
        }
        let epoll = epoll_handle_or_errno(epfd)?;

        if !matches!(
            op,
            linux_abi::EPOLL_CTL_ADD | linux_abi::EPOLL_CTL_MOD | linux_abi::EPOLL_CTL_DEL
        ) {
            return Err(LINUX_EINVAL);
        }
        let target = epoll_interest_target(fd)?;
        let provider_epoch = if epoll_ctl_requires_live_provider_epoch(op) {
            waitset_provider_epoch(target.provider)?
        } else {
            // DEL is keyed only by target fd plus open-description identity. Keep
            // the record structurally valid without making cleanup depend on a live
            // provider or its replacement epoch.
            1
        };

        let (events, data) = match op {
            linux_abi::EPOLL_CTL_ADD | linux_abi::EPOLL_CTL_MOD => {
                if event_ptr == 0 {
                    return Err(LINUX_EINVAL);
                }
                let (events, data) =
                    read_linux_epoll_event(event_ptr).map_err(user_pointer_error_to_linux_errno)?;
                // Linux: EPOLLEXCLUSIVE is ADD-only and combines only with the
                // bits that make a wake-one hint meaningful. An interest added
                // that way is later immutable (the kernel registry rejects MOD too).
                if events & linux_abi::EPOLLEXCLUSIVE != 0
                    && (op == linux_abi::EPOLL_CTL_MOD
                        || events & !linux_abi::EPOLLEXCLUSIVE_OK_BITS != 0)
                {
                    return Err(LINUX_EINVAL);
                }
                // EPOLLWAKEUP needs CAP_BLOCK_SUSPEND; RustOS has no suspend
                // blockers, so it is dropped exactly as for an unprivileged caller.
                (events & !linux_abi::EPOLLWAKEUP, data)
            }
            linux_abi::EPOLL_CTL_DEL => (0, 0),
            _ => unreachable!(),
        };

        let record = multitask::EpollInterestRecord {
            key: multitask::EpollInterestKey {
                target_fd: fd,
                provider: target.provider,
                object_id: target.object_id,
            },
            provider_epoch,
            events,
            data,
        };

        let guard = if epoll_ctl_requires_live_provider_epoch(op) {
            Some(acquire_epoll_target_guard(&target.handle)?)
        } else {
            None
        };
        let result = multitask::epoll_ctl(epoll.token_id(), op, record);
        if result.is_ok() && op != linux_abi::EPOLL_CTL_DEL {
            super::local_fd::note_epoll_registered(&target.handle);
        }
        if let Ok(new_gen) = result {
            super::super::broker_ops::waitset_broker_ops::wake_local_fd_object(
                epoll.token_id(),
                new_gen,
            );
        }
        if let Some(guard) = guard {
            release_epoll_target_guard(guard);
        }
        result.map(|_| ()).map_err(epoll_error_to_linux_errno)
    })())
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
fn epoll_wait_failed(site: u64, errno: i64) -> i64 {
    debug::record_milestone(debug::LogCategory::Compat, "epoll-wait-failed", site, errno.unsigned_abs());
    errno
}

fn parse_epoll_maxevents(maxevents: u64) -> LinuxResult<usize> {
    // `maxevents` is a C `int`; the register's upper half is not ABI.
    let Ok(maxevents) = usize::try_from(maxevents as u32 as i32) else {
        return Err(LINUX_EINVAL);
    };
    // Linux's EP_MAX_EVENTS bound. Larger-than-interest-table counts are legal
    // (libuv passes 1024) and are served in full by clamping: one wait can
    // never report more than the table's interests.
    if maxevents == 0 || maxevents > i32::MAX as usize / EPOLL_EVENT_SIZE {
        return Err(LINUX_EINVAL);
    }
    Ok(maxevents.min(MAX_POLL_FDS))
}

pub fn syscall_linux_epoll_wait(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    timeout_ms: i64,
) -> u64 {
    linux_result((|| {
        let maxevents = parse_epoll_maxevents(maxevents)?;
        validate_user_write(events_ptr, maxevents * EPOLL_EVENT_SIZE)?;
        let epoll = epoll_handle_or_errno(epfd)?;
        let deadline_tick = (timeout_ms >= 0).then(|| deadline_tick_from_ms(timeout_ms as u64));

        loop {
            let signal_epoch_before = super::super::broker_ops::waitset_broker_ops::signal_epoch();
            let first = match collect_epoll_readiness(
                epoll.token_id(),
                maxevents,
                deadline_tick,
                EpollScanMode::Consume,
            ) {
                Ok(state) => state,
                Err(LINUX_ETIMEDOUT)
                    if deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) =>
                {
                    return Ok(0);
                }
                Err(LINUX_ETIMEDOUT) => {
                    if current_wait_interrupted() {
                        return Err(LINUX_EINTR);
                    }
                    multitask::cond_resched();
                    continue;
                }
                Err(errno) if transient_waitset_scan_error(errno) => {
                    multitask::cond_resched();
                    continue;
                }
                Err(errno) => return Err(epoll_wait_failed(1, errno)),
            };
            if !first.ready.is_empty() {
                return write_epoll_ready_events(events_ptr, &first.ready);
            }
            if timeout_ms == 0
                || deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline)
            {
                return Ok(0);
            }
            if current_wait_interrupted() {
                return Err(LINUX_EINTR);
            }

            let task_id = multitask::current_task_id().ok_or(LINUX_EINVAL)?;
            let process_id = multitask::current_user_process_id().ok_or(LINUX_EINVAL)?;
            if let Err(errno) = super::super::broker_ops::waitset_broker_ops::register_waitset_waiters(
                task_id,
                process_id,
                &first.observations,
            ) {
                return Err(epoll_wait_failed(2, errno));
            }

            // If no waitset signal or provider revocation occurred between the
            // epoch sample before `first` and the `SeqCst` fence closing
            // `register_waitset_waiters`, the second readiness scan cannot observe
            // a newly ready object and can be skipped without losing wakeups.
            let armed_observations = if super::super::broker_ops::waitset_broker_ops::signal_epoch()
                == signal_epoch_before
            {
                if deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) {
                    super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                    continue;
                }
                first.observations
            } else {
                let second = match collect_epoll_readiness(
                    epoll.token_id(),
                    maxevents,
                    deadline_tick,
                    EpollScanMode::Consume,
                ) {
                    Ok(state) => state,
                    Err(LINUX_ETIMEDOUT)
                        if deadline_tick
                            .is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) =>
                    {
                        super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                        return Ok(0);
                    }
                    Err(LINUX_ETIMEDOUT) => {
                        super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                        if current_wait_interrupted() {
                            return Err(LINUX_EINTR);
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
                        return Err(epoll_wait_failed(3, errno));
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
                second.observations
            };
            if !multitask::arm_block_current_task() {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                // A prior typed reply wake from the readiness scan can still own
                // this slot's pending wake phase on another CPU; yield and rescan
                // rather than turning a transient cross-CPU phase into EINVAL.
                if multitask::current_task_id() != Some(task_id) {
                    return Err(epoll_wait_failed(4, LINUX_EINVAL));
                }
                multitask::cond_resched();
                continue;
            }
            if !super::super::broker_ops::waitset_broker_ops::waitset_waiters_match(
                task_id,
                process_id,
                &armed_observations,
            ) {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                let _ = multitask::cancel_block_current_task();
                continue;
            }
            if has_unmasked_pending_signal() {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                let _ = multitask::cancel_block_current_task();
                if current_wait_interrupted() {
                    return Err(LINUX_EINTR);
                }
                continue;
            }
            if let Some(deadline) = deadline_tick
                && !crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, deadline)
            {
                super::super::broker_ops::waitset_broker_ops::remove_waitset_waiters(task_id);
                let _ = multitask::cancel_block_current_task();
                return Err(LINUX_EBUSY);
            }
            match multitask::commit_block_current_task_and_yield() {
                Some(true) => {}
                Some(false) => {}
                None => {
                    acknowledge_epoll_wait_block(task_id);
                    return Err(epoll_wait_failed(5, LINUX_EINVAL));
                }
            }
            acknowledge_epoll_wait_block(task_id);
            if current_wait_interrupted() {
                return Err(LINUX_EINTR);
            }
        }
    })())
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
        multitask::KernelHandle::RemoteVfs(remote)
            if remote.kind() == multitask::RemoteVfsHandleKind::Inotify =>
        {
            (WAITSET_PROVIDER_VFSD, remote.remote_id())
        }
        multitask::KernelHandle::Pipe(_)
        | multitask::KernelHandle::EventFd(_)
        | multitask::KernelHandle::UnixSocket(_) => (
            WAITSET_PROVIDER_KERNEL,
            super::local_fd::waitset_object(&handle).ok_or(LINUX_EBADF)?,
        ),
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
                purge_kernel_epoll_object(WAITSET_PROVIDER_SESSIOND, console.token_id());
            }
        }
    }
}

fn waitset_provider_epoch(provider: u16) -> Result<u64, i64> {
    let service_id = match provider {
        WAITSET_PROVIDER_VFSD => IPC_SERVICE_VFSD,
        WAITSET_PROVIDER_NETD => IPC_SERVICE_NETD,
        WAITSET_PROVIDER_INPUTD => IPC_SERVICE_INPUTD,
        WAITSET_PROVIDER_SESSIOND => IPC_SERVICE_SESSIOND,
        // Ring0 is its own provider: there is no service endpoint to restart,
        // so the epoch is constant.
        WAITSET_PROVIDER_KERNEL => return Ok(1),
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

fn collect_epoll_readiness(
    epoll_token: u64,
    maxevents: usize,
    deadline_tick: Option<u64>,
    mode: EpollScanMode,
) -> Result<EpollReadinessState, i64> {
    let (snapshot_generation, entries) =
        multitask::epoll_snapshot(epoll_token, WAITSET_MAX_INTERESTS).map_err(|err| {
            if err == multitask::EpollError::NotFound {
                LINUX_EIO
            } else {
                epoll_error_to_linux_errno(err)
            }
        })?;
    let mut interests = Vec::with_capacity(entries.len());
    for (record, report) in entries {
        // A provider restart advances its endpoint epoch. Report the interest
        // as ERR|HUP, keyed by the new epoch so an edge-triggered consumer
        // sees the revoke once rather than on every scan.
        let (revoked, epoch_generation) = match waitset_provider_epoch(record.key.provider) {
            Ok(current_epoch) => (
                current_epoch != record.provider_epoch,
                current_epoch.max(1),
            ),
            Err(errno) if provider_revoke_events(errno).is_some() => (true, 1),
            Err(errno) => return Err(errno),
        };
        interests.push((record, report, revoked, epoch_generation));
    }

    let mut observations = Vec::with_capacity(interests.len().saturating_add(1));
    observations.push(
        super::super::broker_ops::waitset_broker_ops::ProviderObservation {
            provider: WAITSET_PROVIDER_KERNEL,
            object_id: epoll_token,
            generation: snapshot_generation,
        },
    );
    let revoke_events = linux_abi::EPOLLERR | linux_abi::EPOLLHUP;
    let mut scans = Vec::with_capacity(interests.len());
    let mut netd_slots = Vec::new();
    let mut netd_queries = Vec::new();
    let mut vfs_keyed_slots = Vec::new();
    let mut vfs_keyed_queries = Vec::new();
    for (index, (record, report, revoked, epoch_generation)) in interests.iter().enumerate() {
        if report.disabled {
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
        let provider_events = epoll_provider_events(record.events);
        if record.key.provider == WAITSET_PROVIDER_NETD {
            netd_slots.push(index);
            netd_queries.push((record.key.object_id, provider_events));
            scans.push(InterestScan::Netd);
            continue;
        }
        if record.key.provider == WAITSET_PROVIDER_VFSD {
            vfs_keyed_slots.push(index);
            vfs_keyed_queries.push((
                record.key.target_fd as i32,
                provider_events,
                record.key.object_id,
            ));
            scans.push(InterestScan::Netd);
            continue;
        }
        let queried = (|| -> Result<(u32, u64), i64> {
            Ok(match record.key.provider {
                WAITSET_PROVIDER_INPUTD => {
                    let access =
                        super::super::broker_ops::waitset_broker_ops::input_open_description_access(
                            record.key.object_id,
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
                    let stream = multitask::ConsoleHandle::stream_for_token(record.key.object_id)
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
                WAITSET_PROVIDER_KERNEL => match kernel_ps::api::local_fd_readiness_of(
                    record.key.object_id,
                ) {
                    Some(readiness) => {
                        merge_provider_observation(
                            &mut observations,
                            super::super::broker_ops::waitset_broker_ops::ProviderObservation {
                                provider: WAITSET_PROVIDER_KERNEL,
                                object_id: record.key.object_id,
                                generation: readiness.generation,
                            },
                        );
                        (readiness.events, readiness.edge_generation(provider_events))
                    }
                    // The object closed while an interest survived (a purge
                    // that has not landed yet): report it like a revoked
                    // provider so the waiter can drop the descriptor.
                    None => (revoke_events, 1),
                },
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
                    record.key.provider,
                    record.key.object_id,
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

    if !vfs_keyed_queries.is_empty() {
        for (slot_chunk, query_chunk) in vfs_keyed_slots
            .chunks(rustos_user_abi::syscall::VFS_POLL_KEYED_BATCH_MAX)
            .zip(vfs_keyed_queries.chunks(rustos_user_abi::syscall::VFS_POLL_KEYED_BATCH_MAX))
        {
            match poll_vfs_keyed_batch(query_chunk, deadline_tick) {
                Ok((outcomes, generation)) => {
                    for ((slot, &(_, _, remote_id)), revents) in
                        slot_chunk.iter().zip(query_chunk.iter()).zip(outcomes)
                    {
                        merge_provider_observation(
                            &mut observations,
                            super::super::broker_ops::waitset_broker_ops::ProviderObservation {
                                provider: WAITSET_PROVIDER_VFSD,
                                object_id: remote_id,
                                generation,
                            },
                        );
                        scans[*slot] = InterestScan::Ready { revents, generation };
                    }
                }
                Err(LINUX_ETIMEDOUT) => {
                    for slot in slot_chunk {
                        scans[*slot] = InterestScan::TimedOut;
                    }
                }
                Err(errno) if provider_revoke_events(errno).is_some() => {
                    for slot in slot_chunk {
                        scans[*slot] = InterestScan::Ready {
                            revents: revoke_events,
                            generation: 1,
                        };
                    }
                }
                Err(errno) => return Err(errno),
            }
        }
    }

    let mut provider_timed_out = false;
    let mut candidates = Vec::new();
    for ((record, report, _, _), scan) in interests.iter().zip(scans) {
        let (revents, generation) = match scan {
            InterestScan::Skip | InterestScan::Netd => continue,
            InterestScan::TimedOut => {
                provider_timed_out = true;
                continue;
            }
            InterestScan::Ready { revents, generation } => (revents, generation),
        };
        let reported = epoll_report_events(revents, record.events);
        if reported == 0 {
            continue;
        }
        let edge = record.events & linux_abi::EPOLLET != 0;
        let oneshot = record.events & linux_abi::EPOLLONESHOT != 0;
        // An edge was already reported when no newer provider generation exists.
        if edge && generation <= report.edge_generation {
            continue;
        }
        candidates.push(EpollCandidate {
            events: reported,
            data: record.data,
            claim: WaitSetClaimWire {
                provider: record.key.provider,
                target_fd: record.key.target_fd,
                object_id: record.key.object_id,
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
/// are taken as they are; edge-triggered and one-shot ones are claimed in the
/// kernel epoll registry first, and a candidate another waiter already claimed
/// is replaced by the next one so a lost race never turns a ready set into an
/// empty result.
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

/// Atomically claims edge-triggered and one-shot candidates in the kernel epoll
/// registry. The answer is one flag per gated candidate, in order.
fn claim_epoll_reports(
    epoll_token: u64,
    candidates: &[EpollCandidate],
) -> Result<Vec<bool>, i64> {
    let mut owned = Vec::new();
    for candidate in candidates.iter().filter(|candidate| candidate.gated) {
        let key = multitask::EpollInterestKey {
            target_fd: candidate.claim.target_fd,
            provider: candidate.claim.provider,
            object_id: candidate.claim.object_id,
        };
        let won = multitask::epoll_claim(epoll_token, key, candidate.claim.generation)
            .map_err(epoll_error_to_linux_errno)?;
        owned.push(won);
    }
    Ok(owned)
}

fn write_epoll_ready_events(events_ptr: u64, ready: &[(u32, u64)]) -> LinuxResult<u64> {
    for (slot, (events, data)) in ready.iter().copied().enumerate() {
        let entry_ptr = events_ptr
            .checked_add((slot * EPOLL_EVENT_SIZE) as u64)
            .ok_or(LINUX_EFAULT)?;
        write_linux_epoll_event(entry_ptr, events, data)
            .map_err(user_pointer_error_to_linux_errno)?;
    }
    Ok(ready.len() as u64)
}

pub fn retire_kernel_epoll(token: u64) {
    if let Some(new_gen) = multitask::epoll_retire(token) {
        super::super::broker_ops::waitset_broker_ops::wake_local_fd_object(token, new_gen);
    }
}

pub fn purge_kernel_epoll_object(provider: u16, object_id: u64) {
    if provider == 0 || object_id == 0 {
        return;
    }
    let wakes = multitask::epoll_purge_object(provider, object_id);
    for (epoll_token, new_gen) in wakes {
        super::super::broker_ops::waitset_broker_ops::wake_local_fd_object(epoll_token, new_gen);
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    #[test]
    fn decode_pollfd_does_not_sign_extend_high_event_bit() {
        let raw = [5, 0, 0, 0, 0x00, 0x80, 0, 0];
        assert_eq!(decode_pollfd(&raw), Ok((5, 0x8000)));
        assert_eq!(decode_pollfd(&[1, 2, 3, 4, 5]), Err(LINUX_EINVAL));
    }

    #[test]
    fn deadline_tick_helpers_avoid_zero_bias_and_u64_overflow() {
        assert_eq!(deadline_tick_from_nanos_at(0, 100, 1000), 100);
        assert_eq!(deadline_tick_from_nanos_at(1, 100, 1000), 101);
        assert_eq!(deadline_tick_from_ms_at(0, 100, 1000), 100);
        assert_eq!(deadline_tick_from_ms_at(u64::MAX, 100, 1000), u64::MAX);
        assert_eq!(
            deadline_tick_from_ms_at(1_000_000_000_000, 0, 100_000),
            100_000_000_000_000
        );
    }

    #[test]
    fn parse_epoll_maxevents_validates_bounds_and_clamps() {
        assert_eq!(parse_epoll_maxevents(0), Err(LINUX_EINVAL));
        assert_eq!(
            parse_epoll_maxevents((-1_i32 as i64) as u64),
            Err(LINUX_EINVAL)
        );
        assert_eq!(parse_epoll_maxevents(i32::MAX as u64), Err(LINUX_EINVAL));
        assert_eq!(parse_epoll_maxevents(1), Ok(1));
        assert_eq!(parse_epoll_maxevents((1_u64 << 32) | 4), Ok(4));
        assert_eq!(parse_epoll_maxevents(100_000), Ok(MAX_POLL_FDS));
    }

    #[test]
    fn remaining_timeval_saturates_above_i64_max() {
        let huge = remaining_timeval_from_ticks(u64::MAX, 1);
        assert_eq!(huge.tv_sec, i64::MAX);
        assert_eq!(huge.tv_usec, 999_999);

        let exact = remaining_timeval_from_ticks(1500, 1000);
        assert_eq!(exact.tv_sec, 1);
        assert_eq!(exact.tv_usec, 500_000);
    }

    #[test]
    fn timespec_to_timeout_ms_rounds_up_subms_and_saturates() {
        assert_eq!(
            timespec_to_timeout_ms(LinuxTimespecWire {
                tv_sec: 0,
                tv_nsec: 0
            }),
            0
        );
        assert_eq!(
            timespec_to_timeout_ms(LinuxTimespecWire {
                tv_sec: 0,
                tv_nsec: 1
            }),
            1
        );
        assert_eq!(
            timespec_to_timeout_ms(LinuxTimespecWire {
                tv_sec: i64::MAX,
                tv_nsec: 999_999_999
            }),
            i64::MAX
        );
    }
}

#[cfg(test)]
mod tests;
