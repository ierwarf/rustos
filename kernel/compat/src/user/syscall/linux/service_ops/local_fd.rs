//! Linux `pipe`/`pipe2`/`eventfd`/`eventfd2` over kernel-local fd objects.
//!
//! - **Owner:** Compat owns fd install, flag validation, user copy, blocking
//!   and wakeups; `kernel-ps` owns each object's exact state, descriptor
//!   lifetime and readiness (`user/pipe.rs`, `user/eventfd.rs`). See
//!   `docs/ai/abi/local-fd.md` for the owner decision and its evidence.
//! - **Boundary:** User pointers, lengths, flags, counter values and fds are
//!   untrusted; every user copy completes before an object guard is taken.
//! - **Lifecycle:** Create object, install both descriptors in one
//!   process-state transaction, copy the fd pair out (rolling both back on
//!   fault). Dup/fork/exec/exit settle through `ServiceHandleRef::{Pipe,
//!   EventFd}`; the final release closes the end, wakes the peer and purges any
//!   epoll interest.
//! - **Concurrency:** No object guard is held across a user copy, a sleep or a
//!   wake. A blocked task registers an observation of the object's own
//!   generation, rechecks it, arms, and only then commits the block, exactly
//!   like `epoll_wait`; `wake_local_fd_object` removes the waiter before
//!   waking it, so a wake between recheck and arm is detected by presence.
//! - **Failure:** Partial progress is returned in preference to a later error;
//!   `EPIPE` always queues `SIGPIPE`; `EINTR` only when nothing was moved.
//! - **Forbidden:** Polling loops, packet mode, descriptor passing, unbounded
//!   bounce buffers, and a wake issued while an object guard is held.
//! - **Evidence:** `vfs-open-description`, `userspace-wait-set`.
use alloc::vec::Vec;
use kernel_ps::api::{
    EventFdError, EventFdHandle, LocalRelease, PipeCapacityError, PipeError, PipeHandle,
    PipeReadError, PipeRole, TimerFdError, TimerFdHandle, TimerFdSpec, UnixSocketHandle,
};

use super::super::broker_ops::waitset_broker_ops::{
    ProviderObservation, register_waitset_waiters, remove_waitset_waiters, waitset_waiters_match,
    wake_local_fd_object,
};
use super::*;
use rustos_user_abi::syscall::WAITSET_PROVIDER_KERNEL;

const LINUX_ENFILE: i64 = 23;
const LINUX_ESPIPE: i64 = 29;
/// Largest single bounce buffer; matches the memfd copy chunk and the pipe
/// capacity, so one chunk never needs more than the buffer can ever hold.
const LOCAL_FD_IO_CHUNK_BYTES: usize = 64 * 1024;
const PIPE2_ACCEPTED_FLAGS: u64 = linux_abi::O_CLOEXEC | linux_abi::O_NONBLOCK;
const EFD_ACCEPTED_FLAGS: u64 =
    linux_abi::EFD_SEMAPHORE | linux_abi::EFD_CLOEXEC | linux_abi::EFD_NONBLOCK;

const TFD_ACCEPTED_FLAGS: u64 = linux_abi::TFD_CLOEXEC | linux_abi::TFD_NONBLOCK;
const TFD_SETTIME_ACCEPTED_FLAGS: u64 =
    linux_abi::TFD_TIMER_ABSTIME | linux_abi::TFD_TIMER_CANCEL_ON_SET;

/// A kernel-local fd object pinned by an active descriptor reference.
#[derive(Debug)]
pub(in crate::user::syscall::linux) enum LocalFd {
    Pipe(PipeHandle),
    EventFd(EventFdHandle),
    TimerFd(TimerFdHandle),
    UnixSocket(UnixSocketHandle),
}

impl Drop for LocalFd {
    fn drop(&mut self) {
        match self {
            Self::Pipe(pipe) => release_pipe(pipe, true),
            Self::EventFd(eventfd) => release_eventfd(eventfd, true),
            Self::TimerFd(timerfd) => release_timerfd(timerfd, true),
            Self::UnixSocket(sock) => {
                release_service_handle_refs_bounded(&[ServiceHandleRef::UnixSocket(sock.clone())]);
            }
        }
    }
}

impl LocalFd {
    pub(super) fn token_id(&self) -> u64 {
        match self {
            Self::Pipe(pipe) => pipe.token_id(),
            Self::EventFd(eventfd) => eventfd.token_id(),
            Self::TimerFd(timerfd) => timerfd.token_id(),
            Self::UnixSocket(sock) => sock.token_id(),
        }
    }

    fn status_flags(&self) -> u64 {
        match self {
            Self::Pipe(pipe) => pipe.status_flags(),
            Self::EventFd(eventfd) => eventfd.status_flags(),
            Self::TimerFd(timerfd) => timerfd.status_flags(),
            Self::UnixSocket(sock) => sock.status_flags(),
        }
    }

    fn set_nonblocking(&self, value: bool) {
        match self {
            Self::Pipe(pipe) => pipe.set_nonblocking(value),
            Self::EventFd(eventfd) => eventfd.set_nonblocking(value),
            Self::TimerFd(timerfd) => timerfd.set_nonblocking(value),
            Self::UnixSocket(sock) => sock.set_nonblocking(value),
        }
    }
}

pub(in crate::user::syscall::linux) fn current_local_fd(fd: u64) -> Option<LocalFd> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::Pipe(pipe))
                if pipe.try_acquire_descriptor_reference() =>
            {
                Some(LocalFd::Pipe(pipe.clone()))
            }
            Some(multitask::KernelHandle::EventFd(eventfd))
                if eventfd.try_acquire_descriptor_reference() =>
            {
                Some(LocalFd::EventFd(eventfd.clone()))
            }
            Some(multitask::KernelHandle::TimerFd(timerfd))
                if timerfd.try_acquire_descriptor_reference() =>
            {
                Some(LocalFd::TimerFd(timerfd.clone()))
            }
            Some(multitask::KernelHandle::UnixSocket(sock))
                if sock.try_acquire_descriptor_reference() =>
            {
                Some(LocalFd::UnixSocket(sock.clone()))
            }
            _ => None,
        }
    })
    .flatten()
}

fn publish(wake: Option<kernel_ps::api::LocalWake>) {
    if let Some(wake) = wake {
        wake_local_fd_object(wake.token, wake.generation);
    }
}

/// Settles the descriptor reference of a pipe end that left a descriptor
/// table: wakes the peer on a final close and drops any epoll interest.
pub(super) fn release_pipe(pipe: &PipeHandle, bounded: bool) {
    settle_release(
        pipe.release_descriptor_reference(),
        pipe.epoll_registered(),
        pipe.token_id(),
        bounded,
    );
}

pub(super) fn release_eventfd(eventfd: &EventFdHandle, bounded: bool) {
    settle_release(
        eventfd.release_descriptor_reference(),
        eventfd.epoll_registered(),
        eventfd.token_id(),
        bounded,
    );
}

pub(super) fn release_timerfd(timerfd: &TimerFdHandle, bounded: bool) {
    settle_release(
        timerfd.release_descriptor_reference(),
        timerfd.epoll_registered(),
        timerfd.token_id(),
        bounded,
    );
}

pub(super) fn settle_release(
    release: LocalRelease,
    epoll_registered: bool,
    token: u64,
    _bounded: bool,
) {
    publish(release.wake);
    if release.last && epoll_registered {
        // Linux drops an object's epoll interests when its last descriptor
        // closes; the kernel epoll registry keeps them keyed by (fd, provider, object).
        purge_kernel_epoll_object(WAITSET_PROVIDER_KERNEL, token);
    }
}

fn pipe_error_errno(err: PipeError) -> i64 {
    match err {
        PipeError::NoMemory => LINUX_ENOMEM,
        PipeError::TooManyObjects => LINUX_ENFILE,
        PipeError::BrokenPipe => LINUX_EPIPE,
        PipeError::WouldBlock { .. } => LINUX_EAGAIN,
    }
}

/// Queues `SIGPIPE` on the writing thread; procd decides its disposition when
/// the pending signal is delivered at syscall return.
fn raise_sigpipe() {
    let (Some(process_id), Some(thread_id)) = (
        multitask::current_user_process_id(),
        multitask::current_user_thread_id(),
    ) else {
        return;
    };
    // `Err` is transient contention, never a missing recipient: retry briefly.
    for _ in 0..8 {
        if multitask::queue_linux_signal(process_id, thread_id, linux_abi::SIGPIPE).is_ok() {
            return;
        }
        multitask::cond_resched();
    }
}

/// Sleeps until `token`'s generation moves past `generation`, a signal
/// arrives, `deadline_tick` elapses, or the object closes. `Ok(())` means
/// "retry the operation".
pub(super) fn wait_for_local_generation(
    token: u64,
    generation: u64,
    deadline_tick: Option<u64>,
) -> Result<(), i64> {
    if deadline_tick.is_some_and(|d| crate::arch::rtc::ticks() >= d) {
        return Err(LINUX_EAGAIN);
    }
    let Some(task_id) = multitask::current_task_id() else {
        return Err(LINUX_EINVAL);
    };
    let Some(process_id) = multitask::current_user_process_id() else {
        return Err(LINUX_EINVAL);
    };
    let observation = [ProviderObservation {
        provider: WAITSET_PROVIDER_KERNEL,
        object_id: token,
        generation,
    }];
    register_waitset_waiters(task_id, process_id, &observation)?;
    // The state may have changed between the failed attempt and registration.
    match kernel_ps::api::local_fd_readiness_of(token) {
        Some(current) if current.generation == generation => {}
        _ => {
            remove_waitset_waiters(task_id);
            return Ok(());
        }
    }
    if !multitask::arm_block_current_task() {
        remove_waitset_waiters(task_id);
        if multitask::current_task_id() != Some(task_id) {
            return Err(LINUX_EINVAL);
        }
        // A pending wake phase owned by another CPU: yield and retry.
        multitask::cond_resched();
        return Ok(());
    }
    if !waitset_waiters_match(task_id, process_id, &observation) {
        // A wake removed the observation after the recheck.
        remove_waitset_waiters(task_id);
        let _ = multitask::cancel_block_current_task();
        return Ok(());
    }
    // Armed to block: no procd round trip is allowed here, so only the raw
    // pending bits are consulted. A hit cancels the block and returns to the
    // caller's loop, whose `current_wait_interrupted` classifies the signal
    // (ignored ones are consumed there) before waiting again.
    if super::poll_epoll::has_unmasked_pending_signal() {
        remove_waitset_waiters(task_id);
        let _ = multitask::cancel_block_current_task();
        multitask::cond_resched();
        return Ok(());
    }
    if let Some(deadline) = deadline_tick {
        if !crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, deadline) {
            remove_waitset_waiters(task_id);
            let _ = multitask::cancel_block_current_task();
            return Err(LINUX_EAGAIN);
        }
        if crate::arch::rtc::ticks() >= deadline {
            remove_waitset_waiters(task_id);
            crate::arch::rtc::disarm_sleep_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            return Err(LINUX_EAGAIN);
        }
    }
    let committed = multitask::commit_block_current_task_and_yield();
    remove_waitset_waiters(task_id);
    if deadline_tick.is_some() {
        crate::arch::rtc::disarm_sleep_waiter(task_id);
    }
    if committed.is_none() {
        return Err(LINUX_EINVAL);
    }
    if current_wait_interrupted() {
        return Err(LINUX_EINTR);
    }
    if deadline_tick.is_some_and(|d| crate::arch::rtc::ticks() >= d) {
        return Err(LINUX_EAGAIN);
    }
    Ok(())
}

/// Handles a `WouldBlock { generation }` response for a local fd operation:
/// fails immediately with `EAGAIN` if `nonblocking` or `EINTR` if interrupted
/// by a deliverable signal, otherwise waits for `generation` to advance or
/// `deadline_tick` to elapse.
pub(super) fn wait_local_wouldblock(
    token: u64,
    generation: u64,
    nonblocking: bool,
    deadline_tick: Option<u64>,
) -> Result<(), i64> {
    if nonblocking {
        return Err(LINUX_EAGAIN);
    }
    if current_wait_interrupted() {
        return Err(LINUX_EINTR);
    }
    wait_for_local_generation(token, generation, deadline_tick)
}

// ---- creation -------------------------------------------------------------

fn install_pair(read: PipeHandle, write: PipeHandle, fd_flags: u32) -> Result<(u64, u64), i64> {
    let read_entry = multitask::HandleEntry::new(
        multitask::KernelHandle::Pipe(read.clone()),
        fd_flags,
        read.status_flags(),
    );
    let write_entry = multitask::HandleEntry::new(
        multitask::KernelHandle::Pipe(write.clone()),
        fd_flags,
        write.status_flags(),
    );
    let installed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let handles = process_state.handles_mut();
        let first = handles.install_entry(read_entry)?;
        let Some(second) = handles.install_entry(write_entry) else {
            let _ = handles.close(first);
            return None;
        };
        Some((first, second))
    });
    match installed {
        Some(Some(fds)) => Ok(fds),
        Some(None) => Err(LINUX_EMFILE),
        None => Err(LINUX_ENOSYS),
    }
}

pub(crate) fn kernel_create_pipe_pair(nonblocking: bool, cloexec: bool) -> Result<(u64, u64), i64> {
    let (read, write) =
        PipeHandle::new_pair(multitask::current_user_job_charge_group(), nonblocking)
            .map_err(pipe_error_errno)?;
    let fd_flags = if cloexec { multitask::FD_CLOEXEC } else { 0 };
    match install_pair(read.clone(), write.clone(), fd_flags) {
        Ok(fds) => Ok(fds),
        Err(errno) => {
            release_pipe(&read, true);
            release_pipe(&write, true);
            Err(errno)
        }
    }
}

pub fn syscall_linux_pipe2(pipefd_ptr: u64, flags: u64) -> u64 {
    linux_unit_result((|| {
        // `flags` is a C `int`: the register's upper half is not ABI.
        let flags = u64::from(flags as u32);
        if flags & !(PIPE2_ACCEPTED_FLAGS | linux_abi::O_DIRECT) != 0 {
            return Err(LINUX_EINVAL);
        }
        if flags & linux_abi::O_DIRECT != 0 {
            // Packet mode is not implemented; accepting the flag would silently
            // return stream semantics to a caller that depends on message bounds.
            return Err(LINUX_EINVAL);
        }
        validate_user_write(pipefd_ptr, 2 * size_of::<i32>())?;
        let nonblocking = flags & linux_abi::O_NONBLOCK != 0;
        let cloexec = flags & linux_abi::O_CLOEXEC != 0;
        let (read_fd, write_fd) = kernel_create_pipe_pair(nonblocking, cloexec)?;
        let mut pair = [0_u8; 2 * size_of::<i32>()];
        pair[..4].copy_from_slice(&(read_fd as i32).to_ne_bytes());
        pair[4..].copy_from_slice(&(write_fd as i32).to_ne_bytes());
        if let Err(errno) = write_user_bytes(pipefd_ptr, &pair) {
            // The buffer was validated, so this is a racing unmap: Linux closes
            // both descriptors and reports EFAULT.
            let closed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
                let handles = process_state.handles_mut();
                (handles.close(read_fd), handles.close(write_fd))
            });
            if let Some((first, second)) = closed {
                release_closed_handles(first.into_iter().chain(second));
            }
            return Err(errno);
        }
        Ok(())
    })())
}

pub fn syscall_linux_pipe(pipefd_ptr: u64) -> u64 {
    syscall_linux_pipe2(pipefd_ptr, 0)
}

pub fn syscall_linux_eventfd2(initval: u64, flags: u64) -> u64 {
    linux_result((|| {
        // Both are C `int`/`unsigned int`: ignore the register's upper half.
        let initval = initval as u32;
        let flags = u64::from(flags as u32);
        if flags & !EFD_ACCEPTED_FLAGS != 0 {
            return Err(LINUX_EINVAL);
        }
        let eventfd = EventFdHandle::new_in_group(
            initval,
            flags & linux_abi::EFD_SEMAPHORE != 0,
            flags & linux_abi::EFD_NONBLOCK != 0,
            multitask::current_user_job_charge_group(),
        )
        .map_err(|err| match err {
            EventFdError::TooManyObjects => LINUX_ENFILE,
            _ => LINUX_EINVAL,
        })?;
        let fd_flags = if flags & linux_abi::EFD_CLOEXEC != 0 {
            multitask::FD_CLOEXEC
        } else {
            0
        };
        let entry = multitask::HandleEntry::new(
            multitask::KernelHandle::EventFd(eventfd.clone()),
            fd_flags,
            eventfd.status_flags(),
        );
        match multitask::with_current_user_process_state_mut(|_, _, process_state| {
            process_state.handles_mut().install_entry(entry)
        }) {
            Some(Some(fd)) => Ok(fd),
            Some(None) => {
                release_eventfd(&eventfd, true);
                Err(LINUX_EMFILE)
            }
            None => {
                release_eventfd(&eventfd, true);
                Err(LINUX_ENOSYS)
            }
        }
    })())
}

pub fn syscall_linux_eventfd(initval: u64) -> u64 {
    syscall_linux_eventfd2(initval, 0)
}

pub fn syscall_linux_timerfd_create(clock_id: u64, flags: u64) -> u64 {
    linux_result((|| {
        if flags & !TFD_ACCEPTED_FLAGS != 0 {
            return Err(LINUX_EINVAL);
        }
        let clock = match clock_id {
            id if id == linux_abi::CLOCK_REALTIME
                || id == linux_abi::CLOCK_MONOTONIC
                || id == linux_abi::CLOCK_BOOTTIME =>
            {
                id as u32
            }
            _ => return Err(LINUX_EINVAL),
        };
        let timerfd = TimerFdHandle::new_in_group(
            clock,
            flags & linux_abi::TFD_NONBLOCK != 0,
            multitask::current_user_job_charge_group(),
        )
        .map_err(|_| LINUX_ENFILE)?;
        let fd_flags = if flags & linux_abi::TFD_CLOEXEC != 0 {
            multitask::FD_CLOEXEC
        } else {
            0
        };
        let entry = multitask::HandleEntry::new(
            multitask::KernelHandle::TimerFd(timerfd.clone()),
            fd_flags,
            timerfd.status_flags(),
        );
        match multitask::with_current_user_process_state_mut(|_, _, process_state| {
            process_state.handles_mut().install_entry(entry)
        }) {
            Some(Some(fd)) => Ok(fd),
            Some(None) => {
                release_timerfd(&timerfd, true);
                Err(LINUX_EMFILE)
            }
            None => {
                release_timerfd(&timerfd, true);
                Err(LINUX_ENOSYS)
            }
        }
    })())
}

fn validate_timerfd_timespec(ts: linux_abi::LinuxTimespec) -> Result<u64, i64> {
    if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
        return Err(LINUX_EINVAL);
    }
    Ok((ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64))
}

fn encode_timerfd_itimerspec(spec: TimerFdSpec) -> linux_abi::LinuxItimerspec {
    let to_ts = |ns: u64| linux_abi::LinuxTimespec {
        tv_sec: (ns / 1_000_000_000) as i64,
        tv_nsec: (ns % 1_000_000_000) as i64,
    };
    linux_abi::LinuxItimerspec {
        it_interval: to_ts(spec.interval_ns),
        it_value: to_ts(spec.value_ns),
    }
}

// Translate an absolute wall-clock deadline without losing the expirations
// or the phase of a periodic timer whose first deadline has already passed.
fn realtime_timer_schedule(
    now_ns: u64,
    realtime_now: u64,
    value_ns: u64,
    interval_ns: u64,
) -> (u64, u64) {
    if value_ns > realtime_now {
        return (now_ns.saturating_add(value_ns - realtime_now), 0);
    }
    if interval_ns == 0 {
        return (0, 1);
    }
    let elapsed = realtime_now - value_ns;
    let remaining = interval_ns - elapsed % interval_ns;
    (
        now_ns.checked_add(remaining).unwrap_or(0),
        1_u64.saturating_add(elapsed / interval_ns),
    )
}

fn with_timerfd<T>(fd: u64, f: impl FnOnce(&TimerFdHandle) -> Result<T, i64>) -> Result<T, i64> {
    match current_local_fd(fd) {
        Some(local @ LocalFd::TimerFd(_)) => {
            let LocalFd::TimerFd(ref timerfd) = local else {
                unreachable!()
            };
            f(timerfd)
        }
        Some(_) => Err(LINUX_EINVAL),
        None => {
            let exists = multitask::with_current_user_process_state(|_, _, state| {
                state.handles().get_entry(fd).is_some()
            })
            .unwrap_or(false);
            Err(if exists { LINUX_EINVAL } else { LINUX_EBADF })
        }
    }
}

pub fn syscall_linux_timerfd_settime(
    fd: u64,
    flags: u64,
    new_value_ptr: u64,
    old_value_ptr: u64,
) -> u64 {
    linux_unit_result((|| {
        if flags & !TFD_SETTIME_ACCEPTED_FLAGS != 0 {
            return Err(LINUX_EINVAL);
        }
        with_timerfd(fd, |timerfd| {
            let new_spec: linux_abi::LinuxItimerspec = read_user_struct(new_value_ptr)?;
            let value_ns = validate_timerfd_timespec(new_spec.it_value)?;
            let interval_ns = validate_timerfd_timespec(new_spec.it_interval)?;
            if old_value_ptr != 0 {
                validate_user_write(old_value_ptr, size_of::<linux_abi::LinuxItimerspec>())?;
            }
            let now_ns = crate::arch::rtc::monotonic_nanos();
            let (next_expiry_ns, expirations) = if value_ns == 0 {
                (0, 0)
            } else if flags & linux_abi::TFD_TIMER_ABSTIME != 0 {
                if u64::from(timerfd.clock_id()) == linux_abi::CLOCK_REALTIME {
                    let rt = super::process_time::realtime_timespec();
                    let realtime_now = validate_timerfd_timespec(linux_abi::LinuxTimespec {
                        tv_sec: rt.tv_sec,
                        tv_nsec: rt.tv_nsec,
                    })?;
                    realtime_timer_schedule(now_ns, realtime_now, value_ns, interval_ns)
                } else {
                    (value_ns, 0)
                }
            } else {
                (now_ns.saturating_add(value_ns), 0)
            };
            let (old_spec, wake) =
                timerfd.settime_at(now_ns, next_expiry_ns, interval_ns, expirations);
            publish(Some(wake));
            if old_value_ptr != 0 {
                write_user_struct(old_value_ptr, &encode_timerfd_itimerspec(old_spec))?;
            }
            Ok(())
        })
    })())
}

pub fn syscall_linux_timerfd_gettime(fd: u64, curr_value_ptr: u64) -> u64 {
    linux_unit_result((|| {
        with_timerfd(fd, |timerfd| {
            validate_user_write(curr_value_ptr, size_of::<linux_abi::LinuxItimerspec>())?;
            let spec = timerfd.gettime_at(crate::arch::rtc::monotonic_nanos());
            write_user_struct(curr_value_ptr, &encode_timerfd_itimerspec(spec))
        })
    })())
}

// ---- read / write ---------------------------------------------------------

fn completed_or_result(progress: usize, errno: i64) -> LinuxResult<u64> {
    if progress != 0 {
        Ok(progress as u64)
    } else {
        Err(errno)
    }
}

#[cfg(test)]
fn completed_or(progress: usize, errno: i64) -> u64 {
    linux_result(completed_or_result(progress, errno))
}

/// `read(2)` on a local fd; `None` when `fd` is not one.
pub(in crate::user::syscall::linux) fn read(fd: u64, user_ptr: u64, user_len: u64) -> Option<u64> {
    let local = current_local_fd(fd)?;
    Some(match &local {
        LocalFd::Pipe(pipe) => pipe_read(pipe, user_ptr, user_len),
        LocalFd::EventFd(eventfd) => eventfd_read(eventfd, user_ptr, user_len),
        LocalFd::TimerFd(timerfd) => timerfd_read(timerfd, user_ptr, user_len),
        LocalFd::UnixSocket(sock) => super::local_unix::read_unix_socket(sock, user_ptr, user_len),
    })
}

/// `write(2)` on a local fd; `None` when `fd` is not one.
pub(in crate::user::syscall::linux) fn write(fd: u64, user_ptr: u64, user_len: u64) -> Option<u64> {
    let local = current_local_fd(fd)?;
    Some(match &local {
        LocalFd::Pipe(pipe) => pipe_write(pipe, user_ptr, user_len),
        LocalFd::EventFd(eventfd) => eventfd_write(eventfd, user_ptr, user_len),
        LocalFd::TimerFd(_) => linux_errno(LINUX_EINVAL),
        LocalFd::UnixSocket(sock) => super::local_unix::write_unix_socket(sock, user_ptr, user_len),
    })
}

const LOCAL_FD_STACK_SCRATCH_BYTES: usize = 4096;

fn pipe_read(pipe: &PipeHandle, user_ptr: u64, user_len: u64) -> u64 {
    linux_result((|| {
        if pipe.role() != PipeRole::Read {
            return Err(LINUX_EBADF);
        }
        let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
        if user_len == 0 {
            return Ok(0);
        }
        let mut stack = [0_u8; LOCAL_FD_STACK_SCRATCH_BYTES];
        let mut heap = Vec::new();
        loop {
            // Size scratch by the queued bytes, sampling again after each wake.
            // The one-byte minimum lets read_with distinguish EOF from an empty
            // live pipe and return the wait generation under its own lock.
            let want = user_len
                .min(LOCAL_FD_IO_CHUNK_BYTES)
                .min(pipe.shared().bytes_available().max(1));
            validate_user_write(user_ptr, want.min(LOCAL_FD_STACK_SCRATCH_BYTES))?;
            let scratch = if want <= LOCAL_FD_STACK_SCRATCH_BYTES {
                &mut stack[..want]
            } else {
                if want > heap.len() {
                    heap.try_reserve_exact(want - heap.len())
                        .map_err(|_| LINUX_ENOMEM)?;
                    heap.resize(want, 0);
                }
                &mut heap[..want]
            };
            let copied = pipe
                .shared()
                .read_with(scratch, |bytes| write_user_bytes(user_ptr, bytes));
            match copied {
                Ok(progress) => {
                    publish(progress.wake);
                    return Ok(progress.bytes as u64);
                }
                // The user copy faulted; nothing was consumed from the pipe.
                Err(PipeReadError::CopyOut(errno)) => return Err(errno),
                Err(PipeReadError::Pipe(PipeError::WouldBlock { generation })) => {
                    wait_local_wouldblock(pipe.token_id(), generation, pipe.nonblocking(), None)?;
                }
                Err(PipeReadError::Pipe(err)) => return Err(pipe_error_errno(err)),
            }
        }
    })())
}

/// Pushes `chunk` into a pipe, blocking per the descriptor's mode. Returns
/// `Ok(chunk.len())` when all bytes were queued, or `Err((sent, errno))`
/// carrying any partial progress before the terminal error.
fn pipe_push_chunk(pipe: &PipeHandle, chunk: &[u8]) -> Result<usize, (usize, i64)> {
    // A write of at most PIPE_BUF bytes is one atomic chunk; a larger
    // request stays a sequence of partial writes.
    let mut sent = 0_usize;
    while sent < chunk.len() {
        match pipe.shared().try_write(&chunk[sent..]) {
            Ok(progress) => {
                sent += progress.bytes;
                publish(progress.wake);
            }
            Err(PipeError::WouldBlock { generation }) => {
                if let Err(errno) =
                    wait_local_wouldblock(pipe.token_id(), generation, pipe.nonblocking(), None)
                {
                    return Err((sent, errno));
                }
            }
            Err(PipeError::BrokenPipe) => {
                raise_sigpipe();
                return Err((sent, LINUX_EPIPE));
            }
            Err(err) => return Err((sent, pipe_error_errno(err))),
        }
    }
    Ok(sent)
}

/// Kernel-buffer `write(2)` to a pinned pipe or Unix socket description, for
/// `sendfile`. Returns the bytes delivered, or an errno when none were.
pub(in crate::user::syscall::linux) fn write_pinned_local(
    local: &LocalFd,
    data: &[u8],
) -> Result<usize, i64> {
    if data.is_empty() {
        return Ok(0);
    }
    match local {
        LocalFd::Pipe(pipe) => {
            if pipe.role() != PipeRole::Write {
                return Err(LINUX_EBADF);
            }
            match pipe_push_chunk(pipe, data) {
                Ok(n) => Ok(n),
                Err((sent, errno)) => {
                    if sent > 0 {
                        Ok(sent)
                    } else {
                        Err(errno)
                    }
                }
            }
        }
        LocalFd::UnixSocket(sock) => super::local_unix::unix_socket_write_bytes_handle(sock, data),
        LocalFd::EventFd(_) | LocalFd::TimerFd(_) => Err(LINUX_EINVAL),
    }
}

fn pipe_write(pipe: &PipeHandle, user_ptr: u64, user_len: u64) -> u64 {
    linux_result((|| {
        if pipe.role() != PipeRole::Write {
            return Err(LINUX_EBADF);
        }
        let user_len = usize::try_from(user_len)
            .map_err(|_| LINUX_EINVAL)?
            .min(isize::MAX as usize);
        // Linux completes an empty write without looking at the readers.
        if user_len == 0 {
            return Ok(0);
        }
        let mut stack = [0_u8; LOCAL_FD_STACK_SCRATCH_BYTES];
        let mut heap = Vec::new();
        if user_len > LOCAL_FD_STACK_SCRATCH_BYTES {
            heap.try_reserve_exact(user_len.min(LOCAL_FD_IO_CHUNK_BYTES))
                .map_err(|_| LINUX_ENOMEM)?;
        }
        let mut total = 0_usize;
        while total < user_len {
            let chunk_len = (user_len - total).min(LOCAL_FD_IO_CHUNK_BYTES);
            let chunk: &mut [u8] = if chunk_len <= LOCAL_FD_STACK_SCRATCH_BYTES {
                &mut stack[..chunk_len]
            } else {
                heap.resize(chunk_len, 0);
                &mut heap[..chunk_len]
            };
            let Some(source) = user_ptr.checked_add(total as u64) else {
                return completed_or_result(total, LINUX_EFAULT);
            };
            if let Err(errno) = read_user_bytes(source, chunk) {
                return completed_or_result(total, errno);
            }
            match pipe_push_chunk(pipe, chunk) {
                Ok(n) => total += n,
                Err((sent, errno)) => return completed_or_result(total + sent, errno),
            }
        }
        Ok(total as u64)
    })())
}

fn eventfd_read(eventfd: &EventFdHandle, user_ptr: u64, user_len: u64) -> u64 {
    linux_result((|| {
        if user_len < size_of::<u64>() as u64 {
            return Err(LINUX_EINVAL);
        }
        validate_user_write(user_ptr, size_of::<u64>())?;
        loop {
            let generation = match eventfd.peek_read() {
                Ok(value) => {
                    write_user_bytes(user_ptr, &value.to_ne_bytes())?;
                    match eventfd.commit_read(value) {
                        Ok(wake) => {
                            publish(Some(wake));
                            return Ok(size_of::<u64>() as u64);
                        }
                        Err(EventFdError::WouldBlock { .. }) => continue,
                        Err(_) => return Err(LINUX_EINVAL),
                    }
                }
                Err(EventFdError::WouldBlock { generation }) => generation,
                Err(_) => return Err(LINUX_EINVAL),
            };
            wait_local_wouldblock(eventfd.token_id(), generation, eventfd.nonblocking(), None)?;
        }
    })())
}

fn timerfd_read(timerfd: &TimerFdHandle, user_ptr: u64, user_len: u64) -> u64 {
    linux_result((|| {
        if user_len < size_of::<u64>() as u64 {
            return Err(LINUX_EINVAL);
        }
        validate_user_write(user_ptr, size_of::<u64>())?;
        loop {
            let now_ns = crate::arch::rtc::monotonic_nanos();
            let (generation, deadline_ns) = match timerfd.peek_read_at(now_ns) {
                Ok(read) => {
                    write_user_bytes(user_ptr, &read.count.to_ne_bytes())?;
                    match timerfd.commit_read(read) {
                        Ok(wake) => {
                            publish(Some(wake));
                            return Ok(size_of::<u64>() as u64);
                        }
                        Err(TimerFdError::Retry) => continue,
                        Err(_) => return Err(LINUX_EINVAL),
                    }
                }
                Err(TimerFdError::WouldBlock {
                    generation,
                    deadline_ns,
                }) => (generation, deadline_ns),
                Err(_) => return Err(LINUX_EINVAL),
            };
            if timerfd.nonblocking() {
                return Err(LINUX_EAGAIN);
            }
            let deadline_tick = deadline_ns.map(super::poll_epoll::monotonic_deadline_tick_from_ns);
            match wait_for_local_generation(timerfd.token_id(), generation, deadline_tick) {
                Ok(()) | Err(LINUX_EAGAIN) => {}
                Err(errno) => return Err(errno),
            }
        }
    })())
}

fn eventfd_write(eventfd: &EventFdHandle, user_ptr: u64, user_len: u64) -> u64 {
    linux_result((|| {
        if user_len < size_of::<u64>() as u64 {
            return Err(LINUX_EINVAL);
        }
        let mut bytes = [0_u8; size_of::<u64>()];
        read_user_bytes(user_ptr, &mut bytes)?;
        let value = u64::from_ne_bytes(bytes);
        loop {
            match eventfd.try_write(value) {
                Ok(wake) => {
                    publish(Some(wake));
                    return Ok(size_of::<u64>() as u64);
                }
                Err(EventFdError::InvalidValue) => return Err(LINUX_EINVAL),
                Err(EventFdError::WouldBlock { generation }) => {
                    wait_local_wouldblock(
                        eventfd.token_id(),
                        generation,
                        eventfd.nonblocking(),
                        None,
                    )?;
                }
                Err(EventFdError::TooManyObjects) => return Err(LINUX_ENFILE),
            }
        }
    })())
}

// ---- fcntl / ioctl / stat / seek -----------------------------------------

fn parse_pipe_size_arg(arg: u64) -> Result<usize, i64> {
    if (arg as i64) < 0 {
        return Err(LINUX_EINVAL);
    }
    if arg <= u64::from(u32::MAX) && (arg as u32 as i32) < 0 {
        return Err(LINUX_EINVAL);
    }
    usize::try_from(arg).map_err(|_| LINUX_EINVAL)
}

/// `fcntl` commands whose state lives on the open description. `None` leaves
/// dup/`F_GETFD`/`F_SETFD` (fd-table state) to the generic implementation.
pub(in crate::user::syscall::linux) fn fcntl(local: &LocalFd, cmd: u64, arg: u64) -> Option<u64> {
    if matches!(
        cmd,
        linux_abi::F_DUPFD | linux_abi::F_DUPFD_CLOEXEC | linux_abi::F_GETFD | linux_abi::F_SETFD
    ) {
        return None;
    }
    Some(linux_result((|| match cmd {
        linux_abi::F_GETFL => Ok(local.status_flags()),
        linux_abi::F_SETFL => {
            // Only O_NONBLOCK is stored. Turning on O_DIRECT would select
            // packet mode, which does not exist here (Linux: EINVAL on a
            // description that has no direct I/O).
            if arg & linux_abi::O_DIRECT != 0 {
                return Err(LINUX_EINVAL);
            }
            local.set_nonblocking(arg & linux_abi::O_NONBLOCK != 0);
            Ok(0)
        }
        linux_abi::F_GETPIPE_SZ => match local {
            LocalFd::Pipe(pipe) => Ok(pipe.shared().capacity() as u64),
            LocalFd::EventFd(_) | LocalFd::TimerFd(_) | LocalFd::UnixSocket(_) => Err(LINUX_EBADF),
        },
        linux_abi::F_SETPIPE_SZ => match local {
            LocalFd::Pipe(pipe) => {
                let requested = parse_pipe_size_arg(arg)?;
                let (bytes, wake) =
                    pipe.shared()
                        .set_capacity(requested)
                        .map_err(|err| match err {
                            // Above the (64 KiB) system pipe maximum: Linux's EPERM
                            // for an unprivileged caller past pipe-max-size.
                            PipeCapacityError::TooLarge => LINUX_EPERM,
                            PipeCapacityError::Busy => LINUX_EBUSY,
                        })?;
                publish(wake);
                Ok(bytes as u64)
            }
            LocalFd::EventFd(_) | LocalFd::TimerFd(_) | LocalFd::UnixSocket(_) => Err(LINUX_EBADF),
        },
        _ => Err(LINUX_EINVAL),
    })()))
}

pub(in crate::user::syscall::linux) fn ioctl(local: &LocalFd, request: u64, arg: u64) -> u64 {
    linux_unit_result((|| match request {
        linux_abi::FIONREAD => match local {
            LocalFd::Pipe(pipe) => write_user_struct(
                arg,
                &u32::try_from(pipe.shared().bytes_available()).unwrap_or(u32::MAX),
            ),
            LocalFd::UnixSocket(sock) => write_user_struct(
                arg,
                &u32::try_from(sock.bytes_available()).unwrap_or(u32::MAX),
            ),
            LocalFd::EventFd(_) | LocalFd::TimerFd(_) => Err(LINUX_ENOTTY),
        },
        linux_abi::FIONBIO => {
            let value = read_user_struct::<i32>(arg)?;
            local.set_nonblocking(value != 0);
            Ok(())
        }
        _ => Err(LINUX_ENOTTY),
    })())
}

/// `lseek`/`pread`/`pwrite` on a pipe or eventfd: not seekable.
pub(in crate::user::syscall::linux) fn not_seekable(fd: u64) -> Option<u64> {
    current_local_fd(fd).map(|_| linux_errno(LINUX_ESPIPE))
}

/// `fstat` on a kernel-owned descriptor object (pipe, eventfd, epoll, socket,
/// memfd); the identity rules live in `anon_stat`.
pub(in crate::user::syscall::linux) fn fstat(fd: u64, stat_ptr: u64) -> Option<u64> {
    super::anon_stat::fstat(fd, stat_ptr)
}

/// `fstat` spelled as `newfstatat(fd, "", ..., AT_EMPTY_PATH)`, which current
/// glibc emits. `None` unless that exact shape names a local fd.
pub(in crate::user::syscall::linux) fn fstat_at_empty_path(
    dirfd: u64,
    path: &str,
    flags: u64,
    stat_ptr: u64,
) -> Option<u64> {
    if !path.is_empty() || flags & linux_abi::AT_EMPTY_PATH == 0 {
        return None;
    }
    fstat(dirfd, stat_ptr)
}

pub(in crate::user::syscall::linux) fn statx_at_empty_path(
    dirfd: u64,
    path: &str,
    flags: u64,
    statx_ptr: u64,
) -> Option<u64> {
    if !path.is_empty() || flags & linux_abi::AT_EMPTY_PATH == 0 {
        return None;
    }
    super::anon_stat::statx(dirfd, statx_ptr)
}

// ---- poll / epoll integration ---------------------------------------------

/// The object an epoll/poll scan should observe for `handle`, if it is local.
pub(in crate::user::syscall::linux) fn waitset_object(
    handle: &multitask::KernelHandle,
) -> Option<u64> {
    match handle {
        multitask::KernelHandle::Pipe(pipe) => Some(pipe.token_id()),
        multitask::KernelHandle::EventFd(eventfd) => Some(eventfd.token_id()),
        multitask::KernelHandle::TimerFd(timerfd) => Some(timerfd.token_id()),
        multitask::KernelHandle::UnixSocket(sock) => Some(sock.token_id()),
        _ => None,
    }
}

/// Records that an epoll interest now names this object, so its final close
/// purges the interest from vfsd.
pub(in crate::user::syscall::linux) fn note_epoll_registered(handle: &multitask::KernelHandle) {
    match handle {
        multitask::KernelHandle::Pipe(pipe) => pipe.mark_epoll_registered(),
        multitask::KernelHandle::EventFd(eventfd) => eventfd.mark_epoll_registered(),
        multitask::KernelHandle::TimerFd(timerfd) => timerfd.mark_epoll_registered(),
        multitask::KernelHandle::UnixSocket(sock) => sock.mark_epoll_registered(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_realtime_timer_preserves_elapsed_periods_and_phase() {
        // The first expiry is even before the monotonic origin; four periods
        // are already readable and the next is 50 ns in the future.
        assert_eq!(realtime_timer_schedule(100, 10_000, 9_650, 100), (150, 4));
        assert_eq!(realtime_timer_schedule(100, 10_000, 9_650, 0), (0, 1));
        assert_eq!(realtime_timer_schedule(100, 10_000, 10_500, 100), (600, 0));
        assert_eq!(realtime_timer_schedule(100, 10_000, 10_000, 100), (200, 1));
    }

    #[test]
    fn pipe2_flag_masks_match_the_documented_contract() {
        assert_eq!(
            PIPE2_ACCEPTED_FLAGS & linux_abi::O_DIRECT,
            0,
            "packet mode is rejected"
        );
        assert_eq!(EFD_ACCEPTED_FLAGS & linux_abi::O_DIRECT, 0);
        assert_eq!(
            TFD_ACCEPTED_FLAGS,
            linux_abi::TFD_CLOEXEC | linux_abi::TFD_NONBLOCK
        );
        assert_eq!(linux_abi::SYS_PIPE, 22);
        assert_eq!(linux_abi::SYS_EVENTFD, 284);
        assert_eq!(linux_abi::SYS_TIMERFD_CREATE, 283);
        assert_eq!(linux_abi::SYS_TIMERFD_SETTIME, 286);
        assert_eq!(linux_abi::SYS_TIMERFD_GETTIME, 287);
        assert_eq!(linux_abi::SYS_EVENTFD2, 290);
        assert_eq!(linux_abi::SYS_PIPE2, 293);
        assert_eq!(linux_abi::SYS_NAME_TO_HANDLE_AT, 303);
        assert_eq!(linux_abi::SYS_PKEY_MPROTECT, 329);
        assert_eq!(linux_abi::SYS_PKEY_ALLOC, 330);
        assert_eq!(linux_abi::SYS_PKEY_FREE, 331);
    }

    #[test]
    fn timerfd_timespec_validation_and_encoding_match_linux_contract() {
        assert_eq!(
            validate_timerfd_timespec(linux_abi::LinuxTimespec {
                tv_sec: 2,
                tv_nsec: 250_000_000,
            }),
            Ok(2_250_000_000)
        );
        assert_eq!(
            validate_timerfd_timespec(linux_abi::LinuxTimespec {
                tv_sec: -1,
                tv_nsec: 0,
            }),
            Err(LINUX_EINVAL)
        );
        assert_eq!(
            validate_timerfd_timespec(linux_abi::LinuxTimespec {
                tv_sec: 0,
                tv_nsec: 1_000_000_000,
            }),
            Err(LINUX_EINVAL)
        );
        let encoded = encode_timerfd_itimerspec(TimerFdSpec {
            interval_ns: 1_000_000_050,
            value_ns: 500_000_000,
        });
        assert_eq!(encoded.it_interval.tv_sec, 1);
        assert_eq!(encoded.it_interval.tv_nsec, 50);
        assert_eq!(encoded.it_value.tv_sec, 0);
        assert_eq!(encoded.it_value.tv_nsec, 500_000_000);
    }

    #[test]
    fn chunk_is_the_pipe_capacity() {
        assert_eq!(LOCAL_FD_IO_CHUNK_BYTES, kernel_ps::api::PIPE_MAX_CAPACITY);
    }

    #[test]
    fn errno_mapping_distinguishes_exhaustion_from_a_missing_reader() {
        assert_eq!(pipe_error_errno(PipeError::BrokenPipe), LINUX_EPIPE);
        assert_eq!(pipe_error_errno(PipeError::NoMemory), LINUX_ENOMEM);
        assert_eq!(pipe_error_errno(PipeError::TooManyObjects), LINUX_ENFILE);
    }

    #[test]
    fn completed_bytes_win_over_a_later_error() {
        assert_eq!(completed_or(0, LINUX_EPIPE), linux_errno(LINUX_EPIPE));
        assert_eq!(completed_or(7, LINUX_EPIPE), 7);
    }

    #[test]
    fn parse_pipe_size_arg_rejects_negative_and_does_not_truncate_upper_bits() {
        assert_eq!(parse_pipe_size_arg(0), Ok(0));
        assert_eq!(parse_pipe_size_arg(4096), Ok(4096));
        assert_eq!(parse_pipe_size_arg((-1_i64) as u64), Err(LINUX_EINVAL));
        assert_eq!(parse_pipe_size_arg(0xffff_ffff), Err(LINUX_EINVAL));
        assert_eq!(parse_pipe_size_arg(0x8000_0000), Err(LINUX_EINVAL));
        assert_eq!(parse_pipe_size_arg(0x1_0000_0000), Ok(0x1_0000_0000));
        assert_eq!(parse_pipe_size_arg(0x1_0000_1000), Ok(0x1_0000_1000));
    }

    #[test]
    fn pipe_push_chunk_returns_partial_progress_before_wouldblock() {
        let (_read, write) = PipeHandle::new_pair(None, true).unwrap();
        let fill = alloc::vec![0x5a_u8; kernel_ps::api::PIPE_MAX_CAPACITY - 8];
        assert_eq!(pipe_push_chunk(&write, &fill), Ok(fill.len()));
        // Non-atomic write (> PIPE_BUF) writes the remaining 8 bytes, then hits WouldBlock.
        let tail = alloc::vec![0x3c_u8; kernel_ps::api::PIPE_BUF + 16];
        assert_eq!(pipe_push_chunk(&write, &tail), Err((8, LINUX_EAGAIN)));
    }
}
