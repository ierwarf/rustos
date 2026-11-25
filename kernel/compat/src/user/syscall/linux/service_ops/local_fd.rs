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
    PipeReadError, PipeRole, UnixSocketHandle,
};

use super::*;
use super::super::broker_ops::waitset_broker_ops::{
    register_waitset_waiters, remove_waitset_waiters, wake_local_fd_object,
    waitset_waiters_match, ProviderObservation,
};
use rustos_user_abi::syscall::WAITSET_PROVIDER_KERNEL;

const LINUX_ENFILE: i64 = 23;
const LINUX_ESPIPE: i64 = 29;
/// Largest single bounce buffer; matches the memfd copy chunk and the pipe
/// capacity, so one chunk never needs more than the buffer can ever hold.
const LOCAL_FD_IO_CHUNK_BYTES: usize = 64 * 1024;
const PIPE2_ACCEPTED_FLAGS: u64 = linux_abi::O_CLOEXEC | linux_abi::O_NONBLOCK;
const EFD_ACCEPTED_FLAGS: u64 =
    linux_abi::EFD_SEMAPHORE | linux_abi::EFD_CLOEXEC | linux_abi::EFD_NONBLOCK;
const FIONREAD_RESULT_BYTES: usize = 4;

/// A kernel-local fd object pinned by an active descriptor reference.
#[derive(Debug)]
pub(in crate::user::syscall::linux) enum LocalFd {
    Pipe(PipeHandle),
    EventFd(EventFdHandle),
    UnixSocket(UnixSocketHandle),
}

impl Drop for LocalFd {
    fn drop(&mut self) {
        match self {
            Self::Pipe(pipe) => release_pipe(pipe, true),
            Self::EventFd(eventfd) => release_eventfd(eventfd, true),
            Self::UnixSocket(sock) => {
                release_service_handle_refs_bounded(&[ServiceHandleRef::UnixSocket(sock.clone())]);
            }
        }
    }
}

impl LocalFd {
    fn status_flags(&self) -> u64 {
        match self {
            Self::Pipe(pipe) => pipe.status_flags(),
            Self::EventFd(eventfd) => eventfd.status_flags(),
            Self::UnixSocket(sock) => sock.status_flags(),
        }
    }

    fn set_nonblocking(&self, value: bool) {
        match self {
            Self::Pipe(pipe) => pipe.set_nonblocking(value),
            Self::EventFd(eventfd) => eventfd.set_nonblocking(value),
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
    settle_release(pipe.release_descriptor_reference(), pipe.epoll_registered(), pipe.token_id(), bounded);
}

pub(super) fn release_eventfd(eventfd: &EventFdHandle, bounded: bool) {
    settle_release(
        eventfd.release_descriptor_reference(),
        eventfd.epoll_registered(),
        eventfd.token_id(),
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

fn release_handle(handle: &multitask::KernelHandle) {
    match handle {
        multitask::KernelHandle::Pipe(pipe) => release_pipe(pipe, true),
        multitask::KernelHandle::EventFd(eventfd) => release_eventfd(eventfd, true),
        _ => {}
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
/// arrives, or the object closes. `Ok` means "retry the operation".
fn wait_for_generation(token: u64, generation: u64) -> Result<(), i64> {
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
    if multitask::current_linux_thread_state()
        .is_some_and(|state| state.pending_signals & !state.signal_mask != 0)
    {
        remove_waitset_waiters(task_id);
        let _ = multitask::cancel_block_current_task();
        multitask::cond_resched();
        return Ok(());
    }
    let committed = multitask::commit_block_current_task_and_yield();
    remove_waitset_waiters(task_id);
    if committed.is_none() {
        return Err(LINUX_EINVAL);
    }
    if current_wait_interrupted() {
        return Err(LINUX_EINTR);
    }
    Ok(())
}

// ---- creation -------------------------------------------------------------

fn install_pair(
    read: PipeHandle,
    write: PipeHandle,
    fd_flags: u32,
) -> Result<(u64, u64), i64> {
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
                first.iter().chain(second.iter()).for_each(release_handle);
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
    // Both are C `int`/`unsigned int`: ignore the register's upper half.
    let initval = initval as u32;
    let flags = u64::from(flags as u32);
    if flags & !EFD_ACCEPTED_FLAGS != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let eventfd = match EventFdHandle::new(
        initval,
        flags & linux_abi::EFD_SEMAPHORE != 0,
        flags & linux_abi::EFD_NONBLOCK != 0,
    ) {
        Ok(eventfd) => eventfd,
        Err(EventFdError::TooManyObjects) => return linux_errno(LINUX_ENFILE),
        Err(_) => return linux_errno(LINUX_EINVAL),
    };
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
        Some(Some(fd)) => fd,
        Some(None) => {
            release_eventfd(&eventfd, true);
            linux_errno(LINUX_EMFILE)
        }
        None => {
            release_eventfd(&eventfd, true);
            linux_errno(LINUX_ENOSYS)
        }
    }
}

pub fn syscall_linux_eventfd(initval: u64) -> u64 {
    syscall_linux_eventfd2(initval, 0)
}

// ---- read / write ---------------------------------------------------------

fn completed_or(progress: usize, errno: i64) -> u64 {
    if progress != 0 {
        progress as u64
    } else {
        linux_errno(errno)
    }
}

/// `read(2)` on a local fd; `None` when `fd` is not one.
pub(in crate::user::syscall::linux) fn read(fd: u64, user_ptr: u64, user_len: u64) -> Option<u64> {
    let local = current_local_fd(fd)?;
    Some(match &local {
        LocalFd::Pipe(pipe) => pipe_read(pipe, user_ptr, user_len),
        LocalFd::EventFd(eventfd) => eventfd_read(eventfd, user_ptr, user_len),
        LocalFd::UnixSocket(sock) => super::local_unix::read_unix_socket(sock, user_ptr, user_len),
    })
}

/// `write(2)` on a local fd; `None` when `fd` is not one.
pub(in crate::user::syscall::linux) fn write(fd: u64, user_ptr: u64, user_len: u64) -> Option<u64> {
    let local = current_local_fd(fd)?;
    Some(match &local {
        LocalFd::Pipe(pipe) => pipe_write(pipe, user_ptr, user_len),
        LocalFd::EventFd(eventfd) => eventfd_write(eventfd, user_ptr, user_len),
        LocalFd::UnixSocket(sock) => super::local_unix::write_unix_socket(sock, user_ptr, user_len),
    })
}

const LOCAL_FD_STACK_SCRATCH_BYTES: usize = 4096;

fn pipe_read(pipe: &PipeHandle, user_ptr: u64, user_len: u64) -> u64 {
    if pipe.role() != PipeRole::Read {
        return linux_errno(LINUX_EBADF);
    }
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
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
        if let Err(err) = usermem::validate_current_user_write_buffer(
            user_ptr,
            want.min(LOCAL_FD_STACK_SCRATCH_BYTES),
        ) {
            return linux_errno(user_pointer_error_to_linux_errno(err));
        }
        let scratch = if want <= LOCAL_FD_STACK_SCRATCH_BYTES {
            &mut stack[..want]
        } else {
            if want > heap.len() {
                if heap.try_reserve_exact(want - heap.len()).is_err() {
                    return linux_errno(LINUX_ENOMEM);
                }
                heap.resize(want, 0);
            }
            &mut heap[..want]
        };
        let copied = pipe.shared().read_with(scratch, |bytes| {
            usermem::write_current_user_bytes(user_ptr, bytes)
        });
        match copied {
            Ok(progress) => {
                publish(progress.wake);
                return progress.bytes as u64;
            }
            // The user copy faulted; nothing was consumed from the pipe.
            Err(PipeReadError::CopyOut(err)) => {
                return linux_errno(user_pointer_error_to_linux_errno(err));
            }
            Err(PipeReadError::Pipe(PipeError::WouldBlock { generation })) => {
                if pipe.nonblocking() {
                    return linux_errno(LINUX_EAGAIN);
                }
                if current_wait_interrupted() {
                    return linux_errno(LINUX_EINTR);
                }
                if let Err(errno) = wait_for_generation(pipe.token_id(), generation) {
                    return linux_errno(errno);
                }
            }
            Err(PipeReadError::Pipe(err)) => return linux_errno(pipe_error_errno(err)),
        }
    }
}

/// Pushes `chunk` into a pipe, blocking per the descriptor's mode. `done` is
/// what the caller already delivered in this call. `Err` carries the final
/// syscall result: the bytes moved so far when any, else the negative errno.
fn pipe_push_chunk(pipe: &PipeHandle, chunk: &[u8], done: usize) -> Result<(), u64> {
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
                if pipe.nonblocking() {
                    return Err(completed_or(done + sent, LINUX_EAGAIN));
                }
                if current_wait_interrupted() {
                    return Err(completed_or(done + sent, LINUX_EINTR));
                }
                if let Err(errno) = wait_for_generation(pipe.token_id(), generation) {
                    return Err(completed_or(done + sent, errno));
                }
            }
            Err(PipeError::BrokenPipe) => {
                raise_sigpipe();
                return Err(completed_or(done + sent, LINUX_EPIPE));
            }
            Err(err) => return Err(completed_or(done + sent, pipe_error_errno(err))),
        }
    }
    Ok(())
}

/// Checks whether `fd` is a pipe and whether it is writable (`EBADF` if read end).
pub(in crate::user::syscall::linux) fn check_pipe_write_fd(fd: u64) -> Option<Result<(), i64>> {
    let local = current_local_fd(fd)?;
    let LocalFd::Pipe(pipe) = &local else {
        return None;
    };
    Some(if pipe.role() == PipeRole::Write {
        Ok(())
    } else {
        Err(LINUX_EBADF)
    })
}

/// Kernel-buffer `write(2)` to a pipe write end, for `sendfile`. `None` when
/// `fd` is not a pipe; otherwise the bytes delivered, or the errno when none
/// were. A short count is real progress (nonblocking pipe full, signal).
pub(in crate::user::syscall::linux) fn pipe_write_bytes(
    fd: u64,
    data: &[u8],
) -> Option<Result<usize, i64>> {
    let local = current_local_fd(fd)?;
    let LocalFd::Pipe(pipe) = &local else {
        return None;
    };
    if pipe.role() != PipeRole::Write {
        return Some(Err(LINUX_EBADF));
    }
    if data.is_empty() {
        return Some(Ok(0));
    }
    Some(match pipe_push_chunk(pipe, data, 0) {
        Ok(()) => Ok(data.len()),
        Err(raw) if is_linux_error(raw) => Err(-(raw as i64)),
        Err(progress) => Ok(progress as usize),
    })
}

fn pipe_write(pipe: &PipeHandle, user_ptr: u64, user_len: u64) -> u64 {
    if pipe.role() != PipeRole::Write {
        return linux_errno(LINUX_EBADF);
    }
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    // Linux completes an empty write without looking at the readers.
    if user_len == 0 {
        return 0;
    }
    let mut stack = [0_u8; LOCAL_FD_STACK_SCRATCH_BYTES];
    let mut heap = Vec::new();
    if user_len > LOCAL_FD_STACK_SCRATCH_BYTES
        && heap.try_reserve_exact(user_len.min(LOCAL_FD_IO_CHUNK_BYTES)).is_err()
    {
        return linux_errno(LINUX_ENOMEM);
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
            return completed_or(total, LINUX_EFAULT);
        };
        if let Err(err) = usermem::copy_from_current_user_exact(source, chunk) {
            return completed_or(total, user_pointer_error_to_linux_errno(err));
        }
        if let Err(raw) = pipe_push_chunk(pipe, chunk, total) {
            return raw;
        }
        total += chunk_len;
    }
    total as u64
}

fn eventfd_read(eventfd: &EventFdHandle, user_ptr: u64, user_len: u64) -> u64 {
    if user_len < size_of::<u64>() as u64 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, size_of::<u64>()) {
        return linux_errno(user_pointer_error_to_linux_errno(err));
    }
    loop {
        let generation = match eventfd.peek_read() {
            Ok(value) => {
                if let Err(err) = usermem::write_current_user_bytes(user_ptr, &value.to_ne_bytes()) {
                    return linux_errno(user_pointer_error_to_linux_errno(err));
                }
                match eventfd.commit_read(value) {
                    Ok(wake) => {
                        publish(Some(wake));
                        return size_of::<u64>() as u64;
                    }
                    Err(EventFdError::WouldBlock { .. }) => continue,
                    Err(_) => return linux_errno(LINUX_EINVAL),
                }
            }
            Err(EventFdError::WouldBlock { generation }) => generation,
            Err(_) => return linux_errno(LINUX_EINVAL),
        };
        if eventfd.nonblocking() {
            return linux_errno(LINUX_EAGAIN);
        }
        if current_wait_interrupted() {
            return linux_errno(LINUX_EINTR);
        }
        if let Err(errno) = wait_for_generation(eventfd.token_id(), generation) {
            return linux_errno(errno);
        }
    }
}

fn eventfd_write(eventfd: &EventFdHandle, user_ptr: u64, user_len: u64) -> u64 {
    if user_len < size_of::<u64>() as u64 {
        return linux_errno(LINUX_EINVAL);
    }
    let mut bytes = [0_u8; size_of::<u64>()];
    if let Err(err) = usermem::copy_from_current_user_exact(user_ptr, &mut bytes) {
        return linux_errno(user_pointer_error_to_linux_errno(err));
    }
    let value = u64::from_ne_bytes(bytes);
    loop {
        match eventfd.try_write(value) {
            Ok(wake) => {
                publish(Some(wake));
                return size_of::<u64>() as u64;
            }
            Err(EventFdError::InvalidValue) => return linux_errno(LINUX_EINVAL),
            Err(EventFdError::WouldBlock { generation }) => {
                if eventfd.nonblocking() {
                    return linux_errno(LINUX_EAGAIN);
                }
                if current_wait_interrupted() {
                    return linux_errno(LINUX_EINTR);
                }
                if let Err(errno) = wait_for_generation(eventfd.token_id(), generation) {
                    return linux_errno(errno);
                }
            }
            Err(EventFdError::TooManyObjects) => return linux_errno(LINUX_ENFILE),
        }
    }
}

// ---- fcntl / ioctl / stat / seek -----------------------------------------

/// `fcntl` commands whose state lives on the open description. `None` leaves
/// dup/`F_GETFD`/`F_SETFD` (fd-table state) to the generic implementation.
pub(in crate::user::syscall::linux) fn fcntl(local: &LocalFd, cmd: u64, arg: u64) -> Option<u64> {
    match cmd {
        linux_abi::F_DUPFD | linux_abi::F_DUPFD_CLOEXEC | linux_abi::F_GETFD | linux_abi::F_SETFD => {
            None
        }
        linux_abi::F_GETFL => Some(local.status_flags()),
        linux_abi::F_SETFL => {
            // Only O_NONBLOCK is stored. Turning on O_DIRECT would select
            // packet mode, which does not exist here (Linux: EINVAL on a
            // description that has no direct I/O).
            if arg & linux_abi::O_DIRECT != 0 {
                return Some(linux_errno(LINUX_EINVAL));
            }
            local.set_nonblocking(arg & linux_abi::O_NONBLOCK != 0);
            Some(0)
        }
        linux_abi::F_GETPIPE_SZ => Some(match local {
            LocalFd::Pipe(pipe) => pipe.shared().capacity() as u64,
            LocalFd::EventFd(_) | LocalFd::UnixSocket(_) => linux_errno(LINUX_EBADF),
        }),
        linux_abi::F_SETPIPE_SZ => Some(match local {
            LocalFd::Pipe(pipe) => {
                // `arg` is a C `int`; a negative size is invalid.
                let Ok(requested) = usize::try_from(arg as u32 as i32) else {
                    return Some(linux_errno(LINUX_EINVAL));
                };
                match pipe.shared().set_capacity(requested) {
                    Ok((bytes, wake)) => {
                        publish(wake);
                        bytes as u64
                    }
                    // Above the (64 KiB) system pipe maximum: Linux's EPERM
                    // for an unprivileged caller past pipe-max-size.
                    Err(PipeCapacityError::TooLarge) => linux_errno(LINUX_EPERM),
                    Err(PipeCapacityError::Busy) => linux_errno(LINUX_EBUSY),
                }
            }
            LocalFd::EventFd(_) | LocalFd::UnixSocket(_) => linux_errno(LINUX_EBADF),
        }),
        _ => Some(linux_errno(LINUX_EINVAL)),
    }
}

pub(in crate::user::syscall::linux) fn ioctl(local: &LocalFd, request: u64, arg: u64) -> u64 {
    linux_unit_result((|| match request {
        linux_abi::FIONREAD => match local {
            LocalFd::Pipe(pipe) => {
                write_user_bytes(arg, &(pipe.shared().bytes_available() as u32).to_ne_bytes())
            }
            LocalFd::UnixSocket(sock) => {
                write_user_bytes(arg, &(sock.bytes_available() as u32).to_ne_bytes())
            }
            LocalFd::EventFd(_) => Err(LINUX_ENOTTY),
        },
        linux_abi::FIONBIO => {
            let mut bytes = [0_u8; FIONREAD_RESULT_BYTES];
            read_user_bytes(arg, &mut bytes)?;
            local.set_nonblocking(i32::from_ne_bytes(bytes) != 0);
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
        multitask::KernelHandle::UnixSocket(sock) => sock.mark_epoll_registered(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe2_flag_masks_match_the_documented_contract() {
        assert_eq!(PIPE2_ACCEPTED_FLAGS & linux_abi::O_DIRECT, 0, "packet mode is rejected");
        assert_eq!(EFD_ACCEPTED_FLAGS & linux_abi::O_DIRECT, 0);
        assert_eq!(linux_abi::SYS_PIPE, 22);
        assert_eq!(linux_abi::SYS_EVENTFD, 284);
        assert_eq!(linux_abi::SYS_EVENTFD2, 290);
        assert_eq!(linux_abi::SYS_PIPE2, 293);
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
}
