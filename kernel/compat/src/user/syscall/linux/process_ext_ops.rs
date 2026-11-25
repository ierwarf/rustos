//! Linux process/signal/limit/message syscalls built on existing machinery:
//! `rt_sigtimedwait`, `getrlimit`/`setrlimit`, the `sched_set*` and
//! priority-range calls, and `sendmmsg`/`recvmmsg`.
//!
//! - **Owner:** `kernel-compat` owns the argument envelope. rlimit storage is
//!   `syscalld`'s (`prlimit64`); socket data movement is the single-message
//!   `sendmsg`/`recvmsg` path; pending-signal bits are the ring0 thread state
//!   that `procd` already selects from.
//! - **Boundary:** every pointer is validated before an effect, and a fault is
//!   `EFAULT`.
//! - **Lifecycle:** signal consumption and message progress belong to the caller.
//! - **Concurrency:** signal waits sleep without holding a process-state guard.
//! - **Forbidden:** dereference a guest pointer directly or report an unprocessed message.
//! - **Evidence:** focused signal mask, scheduler and message argument tests.
//! - **Failure:** a batch that moved at least one message reports the count and
//!   drops the later error, exactly as Linux does.

use rustos_user_abi::syscall::LinuxTimespecWire;

use super::*;

const SIGINFO_SIZE: usize = 128;

// ---- rt_sigtimedwait ---------------------------------------------------------

/// Lowest-numbered signal of `pending & wanted`, as a signal number.
pub(super) fn first_wanted_signal(pending: u64, wanted: u64) -> Option<u64> {
    let hit = pending & wanted;
    (hit != 0).then(|| u64::from(hit.trailing_zeros()) + 1)
}

/// `SIGKILL` and `SIGSTOP` can never be waited for.
pub(super) fn waitable_signal_set(set: u64) -> u64 {
    set & !((1_u64 << (linux_abi::SIGKILL - 1)) | (1_u64 << (linux_abi::SIGSTOP - 1)))
}

fn timespec_nanos(ts: LinuxTimespecWire) -> Result<u64, i64> {
    if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
        return Err(LINUX_EINVAL);
    }
    Ok((ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64))
}

/// `rt_sigtimedwait(set, info, timeout, sigsetsize)`.
///
/// A pending signal of `set` is consumed whether or not it is blocked; any
/// other deliverable signal ends the wait with `EINTR`. The wait polls the
/// pending bits at 1 ms granularity: a blocked signal has no waiter to wake, so
/// the latency to notice one is up to a millisecond. `siginfo` carries
/// `si_signo` and `si_code = SI_USER`; the sender's pid/uid are not recorded
/// with a pending bit and read as zero.
pub(super) fn syscall_linux_rt_sigtimedwait(
    set_ptr: u64,
    info_ptr: u64,
    timeout_ptr: u64,
    sigset_size: u64,
) -> u64 {
    linux_result((|| {
        if sigset_size != size_of::<u64>() as u64 {
            return Err(LINUX_EINVAL);
        }
        let wanted = waitable_signal_set(read_user_struct::<u64>(set_ptr)?);
        let timeout_ns = read_user_optional_struct::<LinuxTimespecWire>(timeout_ptr)?
            .map(timespec_nanos)
            .transpose()?;
        if info_ptr != 0 {
            validate_user_write(info_ptr, SIGINFO_SIZE)?;
        }
        let deadline = timeout_ns.map(|ns| crate::arch::rtc::monotonic_nanos().saturating_add(ns));
        loop {
            let state = multitask::current_linux_thread_state().ok_or(LINUX_EINVAL)?;
            if let Some(signal) = first_wanted_signal(state.pending_signals, wanted) {
                clear_current_pending_signal(signal, state.pending_sigchld_events);
                if info_ptr != 0 {
                    let mut info = [0_u8; SIGINFO_SIZE];
                    info[0..4].copy_from_slice(&(signal as i32).to_le_bytes());
                    write_user_bytes(info_ptr, &info)?;
                }
                return Ok(signal);
            }
            if current_wait_interrupted() {
                return Err(LINUX_EINTR);
            }
            if deadline.is_some_and(|deadline| crate::arch::rtc::monotonic_nanos() >= deadline) {
                return Err(LINUX_EAGAIN);
            }
            crate::arch::rtc::sleep(1);
        }
    })())
}

// ---- getrlimit / setrlimit -------------------------------------------------------

/// Legacy `getrlimit(resource, rlim)`: `prlimit64(0, resource, NULL, rlim)`.
pub(super) fn syscall_linux_getrlimit(resource: u64, rlim_ptr: u64) -> u64 {
    syscall_linux_syscalld_prlimit64(0, resource, 0, rlim_ptr)
}

/// Legacy `setrlimit(resource, rlim)`: `prlimit64(0, resource, rlim, NULL)`.
pub(super) fn syscall_linux_setrlimit(resource: u64, rlim_ptr: u64) -> u64 {
    syscall_linux_syscalld_prlimit64(0, resource, rlim_ptr, 0)
}

// ---- scheduling policy -----------------------------------------------------------
//
// The scheduler has one policy class for a Linux task (`SCHED_OTHER`) and
// derives weights from the launch class, not from a per-task priority, so a
// task's policy cannot change. Calls that would leave it unchanged succeed;
// valid requests for another class are `EPERM` (a class this kernel does not
// grant); malformed ones are `EINVAL`.

/// Priority range of `policy`, or `None` for an unknown policy.
pub(super) fn sched_priority_range(policy: i64) -> Option<(u64, u64)> {
    match policy {
        linux_abi::SCHED_FIFO | linux_abi::SCHED_RR => {
            Some((linux_abi::SCHED_RT_PRIORITY_MIN, linux_abi::SCHED_RT_PRIORITY_MAX))
        }
        linux_abi::SCHED_OTHER | linux_abi::SCHED_BATCH | linux_abi::SCHED_IDLE => Some((0, 0)),
        _ => None,
    }
}

pub(super) fn syscall_linux_sched_get_priority(policy: i64, max: bool) -> u64 {
    match sched_priority_range(policy) {
        Some((min_prio, max_prio)) => if max { max_prio } else { min_prio },
        None => linux_errno(LINUX_EINVAL),
    }
}

/// Resolves a scheduling-call target. `Ok(false)` is the caller's own process
/// or one of its threads, `Ok(true)` another live process, and a target that
/// does not exist is `ESRCH`. Every task has the same fixed policy, so reading
/// another process's scheduling attributes needs no more than its existence.
pub(super) fn sched_target(pid: i64) -> Result<bool, i64> {
    if pid < 0 {
        return Err(LINUX_EINVAL);
    }
    if pid == 0 {
        return Ok(false);
    }
    let target = pid as u64;
    let own_process = multitask::current_user_process_id();
    if own_process == Some(target)
        || multitask::current_user_thread_id() == Some(target)
        || own_process
            .is_some_and(|process| multitask::linux_thread_snapshot_by_ids(process, target).is_some())
    {
        return Ok(false);
    }
    if multitask::with_process_state_by_pid(target, |_| ()).is_some() {
        return Ok(true);
    }
    Err(LINUX_ESRCH)
}

/// A scheduling-attribute write may name another process only when the caller
/// is privileged: unprivileged callers may change their own tasks only.
fn sched_write_target(pid: i64) -> Result<(), i64> {
    if sched_target(pid)? && multitask::with_current_process_credentials(|s| s.euid()) != Some(0) {
        return Err(LINUX_EPERM);
    }
    Ok(())
}

/// `SCHED_RESET_ON_FORK` rides in the policy word; it changes only what a
/// fork child inherits, and every task has the same fixed policy anyway.
const SCHED_RESET_ON_FORK: i64 = 0x4000_0000;

/// The result of `sched_setscheduler(policy, priority)` for this kernel.
pub(super) fn sched_set_outcome(policy: i64, priority: i32) -> Result<(), i64> {
    let Some((min_prio, max_prio)) = sched_priority_range(policy) else {
        return Err(LINUX_EINVAL);
    };
    if !(min_prio as i64..=max_prio as i64).contains(&i64::from(priority)) {
        return Err(LINUX_EINVAL);
    }
    if policy == linux_abi::SCHED_OTHER {
        Ok(())
    } else {
        Err(LINUX_EPERM)
    }
}

/// The policy of a `policy` argument (a C `int`) with `SCHED_RESET_ON_FORK`
/// stripped; a negative word is `EINVAL`.
fn sched_policy_without_flags(word: i64) -> Result<i64, i64> {
    let word = word as i32 as i64;
    if word < 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(word & !SCHED_RESET_ON_FORK)
}

/// The order Linux checks `sched_setscheduler`/`sched_setparam` arguments in:
/// a negative pid or a NULL `param` is `EINVAL` (not `EFAULT`), then the
/// `param` copy, then the target, then the policy.
fn read_sched_param(pid: i64, param_ptr: u64) -> Result<linux_abi::LinuxSchedParam, i64> {
    if param_ptr == 0 || pid < 0 {
        return Err(LINUX_EINVAL);
    }
    read_user_struct(param_ptr)
}

pub(super) fn syscall_linux_sched_setscheduler(pid: i64, policy: i64, param_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let policy = sched_policy_without_flags(policy)?;
        let param = read_sched_param(pid, param_ptr)?;
        sched_write_target(pid)?;
        sched_set_outcome(policy, param.sched_priority)
    })())
}

pub(super) fn syscall_linux_sched_setparam(pid: i64, param_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let param = read_sched_param(pid, param_ptr)?;
        sched_write_target(pid)?;
        // The task's policy is always SCHED_OTHER, whose only priority is 0.
        sched_set_outcome(linux_abi::SCHED_OTHER, param.sched_priority)
    })())
}

// ---- sendmmsg / recvmmsg ---------------------------------------------------------

fn mmsg_entry(msgvec: u64, index: u64) -> u64 {
    msgvec + index * linux_abi::MMSGHDR_SIZE as u64
}

fn is_errno_result(value: u64) -> bool {
    (value as i64) < 0 && (value as i64) >= -4095
}

fn write_msg_len(entry: u64, len: u64) -> Result<(), i64> {
    write_user_bytes(
        entry + linux_abi::MMSGHDR_MSG_LEN_OFFSET as u64,
        &(len as u32).to_le_bytes(),
    )
}

fn mmsg_vector_ok(fd: u64, msgvec: u64, vlen: u64) -> Result<(), i64> {
    if vlen == 0 {
        let exists = multitask::with_current_user_process_state(|_, _, state| {
            state.handles().get(fd).is_some()
        })
        .unwrap_or(false);
        return if exists { Ok(()) } else { Err(LINUX_EBADF) };
    }
    validate_user_write(msgvec, vlen as usize * linux_abi::MMSGHDR_SIZE)
}

/// A batch's outcome: the count if any message moved, else the first error.
pub(super) fn mmsg_batch_result(done: u64, error: Option<i64>) -> u64 {
    match (done, error) {
        (0, Some(errno)) => linux_errno(errno),
        _ => done,
    }
}

pub(super) fn syscall_linux_sendmsg(fd: u64, msg_ptr: u64, flags: u64) -> u64 {
    service_ops::local_unix::try_sendmsg(fd, msg_ptr, flags).unwrap_or_else(|| {
        syscall_linux_net4(SYSCALL_OFFLOAD_OP_LINUX_SENDMSG, fd, msg_ptr, flags, 0)
    })
}

pub(super) fn syscall_linux_recvmsg(fd: u64, msg_ptr: u64, flags: u64) -> u64 {
    service_ops::local_unix::try_recvmsg(fd, msg_ptr, flags).unwrap_or_else(|| {
        syscall_linux_net4(SYSCALL_OFFLOAD_OP_LINUX_RECVMSG, fd, msg_ptr, flags, 0)
    })
}

pub(super) fn syscall_linux_sendmmsg(fd: u64, msgvec: u64, vlen: u64, flags: u64) -> u64 {
    let vlen = vlen.min(linux_abi::UIO_MAXIOV);
    if let Err(errno) = mmsg_vector_ok(fd, msgvec, vlen) {
        return linux_errno(errno);
    }
    let mut done = 0_u64;
    let mut error = None;
    while done < vlen {
        let entry = mmsg_entry(msgvec, done);
        let sent = syscall_linux_sendmsg(fd, entry, flags);
        if is_errno_result(sent) {
            error = Some(-(sent as i64));
            break;
        }
        if let Err(errno) = write_msg_len(entry, sent) {
            error = Some(errno);
            break;
        }
        done += 1;
    }
    mmsg_batch_result(done, error)
}

pub(super) fn syscall_linux_recvmmsg(
    fd: u64,
    msgvec: u64,
    vlen: u64,
    flags: u64,
    timeout_ptr: u64,
) -> u64 {
    let vlen = vlen.min(linux_abi::UIO_MAXIOV);
    if let Err(errno) = mmsg_vector_ok(fd, msgvec, vlen) {
        return linux_errno(errno);
    }
    let deadline = match read_user_optional_struct::<LinuxTimespecWire>(timeout_ptr) {
        Ok(None) => None,
        Ok(Some(ts)) => match timespec_nanos(ts) {
            Ok(ns) => Some(crate::arch::rtc::monotonic_nanos().saturating_add(ns)),
            Err(errno) => return linux_errno(errno),
        },
        Err(errno) => return linux_errno(errno),
    };
    let wait_for_one = flags & linux_abi::MSG_WAITFORONE != 0;
    let mut flags = flags & !linux_abi::MSG_WAITFORONE;
    let mut done = 0_u64;
    let mut error = None;
    while done < vlen {
        let entry = mmsg_entry(msgvec, done);
        let received = syscall_linux_recvmsg(fd, entry, flags);
        if is_errno_result(received) {
            error = Some(-(received as i64));
            break;
        }
        if let Err(errno) = write_msg_len(entry, received) {
            error = Some(errno);
            break;
        }
        done += 1;
        if wait_for_one {
            flags |= linux_abi::MSG_DONTWAIT;
        }
        // As on Linux, the timeout is judged between datagrams only; a blocking
        // receive in flight is not cut short by it.
        if deadline.is_some_and(|deadline| crate::arch::rtc::monotonic_nanos() >= deadline) {
            break;
        }
    }
    mmsg_batch_result(done, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigtimedwait_picks_the_lowest_wanted_signal() {
        let bit = |sig: u64| 1_u64 << (sig - 1);
        assert_eq!(first_wanted_signal(bit(10) | bit(3), bit(10) | bit(3)), Some(3));
        assert_eq!(first_wanted_signal(bit(10), bit(3)), None);
        assert_eq!(first_wanted_signal(0, u64::MAX), None);
        assert_eq!(first_wanted_signal(bit(64), u64::MAX), Some(64));
        let kill_stop = bit(linux_abi::SIGKILL) | bit(linux_abi::SIGSTOP);
        assert_eq!(waitable_signal_set(u64::MAX) & kill_stop, 0);
        assert_eq!(waitable_signal_set(bit(10)), bit(10));
    }

    #[test]
    fn timeout_timespec_is_validated() {
        let ts = |tv_sec, tv_nsec| LinuxTimespecWire { tv_sec, tv_nsec };
        assert_eq!(timespec_nanos(ts(1, 5)), Ok(1_000_000_005));
        assert_eq!(timespec_nanos(ts(0, 0)), Ok(0));
        assert_eq!(timespec_nanos(ts(-1, 0)), Err(LINUX_EINVAL));
        assert_eq!(timespec_nanos(ts(0, 1_000_000_000)), Err(LINUX_EINVAL));
        assert_eq!(timespec_nanos(ts(0, -1)), Err(LINUX_EINVAL));
        assert_eq!(timespec_nanos(ts(i64::MAX, 0)), Ok(u64::MAX));
    }

    #[test]
    fn priority_ranges_and_set_outcomes() {
        assert_eq!(syscall_linux_sched_get_priority(linux_abi::SCHED_FIFO, false), 1);
        assert_eq!(syscall_linux_sched_get_priority(linux_abi::SCHED_FIFO, true), 99);
        assert_eq!(syscall_linux_sched_get_priority(linux_abi::SCHED_RR, true), 99);
        for policy in [linux_abi::SCHED_OTHER, linux_abi::SCHED_BATCH, linux_abi::SCHED_IDLE] {
            assert_eq!(syscall_linux_sched_get_priority(policy, false), 0);
            assert_eq!(syscall_linux_sched_get_priority(policy, true), 0);
        }
        assert_eq!(syscall_linux_sched_get_priority(4, true), linux_errno(LINUX_EINVAL));
        assert_eq!(syscall_linux_sched_get_priority(6, false), linux_errno(LINUX_EINVAL));
        assert_eq!(syscall_linux_sched_get_priority(-1, false), linux_errno(LINUX_EINVAL));

        assert_eq!(sched_set_outcome(linux_abi::SCHED_OTHER, 0), Ok(()));
        assert_eq!(sched_set_outcome(linux_abi::SCHED_OTHER, 1), Err(LINUX_EINVAL));
        assert_eq!(sched_set_outcome(linux_abi::SCHED_FIFO, 0), Err(LINUX_EINVAL));
        assert_eq!(sched_set_outcome(linux_abi::SCHED_FIFO, 100), Err(LINUX_EINVAL));
        assert_eq!(sched_set_outcome(linux_abi::SCHED_FIFO, 50), Err(LINUX_EPERM));
        assert_eq!(sched_set_outcome(linux_abi::SCHED_RR, 1), Err(LINUX_EPERM));
        assert_eq!(sched_set_outcome(linux_abi::SCHED_BATCH, 0), Err(LINUX_EPERM));
        assert_eq!(sched_set_outcome(6, 0), Err(LINUX_EINVAL));
    }

    #[test]
    fn setscheduler_policy_word_strips_reset_on_fork_and_rejects_negatives() {
        assert_eq!(sched_policy_without_flags(0), Ok(0));
        assert_eq!(sched_policy_without_flags(SCHED_RESET_ON_FORK), Ok(0));
        assert_eq!(sched_policy_without_flags(SCHED_RESET_ON_FORK | 2), Ok(2));
        assert_eq!(sched_policy_without_flags(-1), Err(LINUX_EINVAL));
        // The upper register half of a C `int` is not ABI.
        assert_eq!(sched_policy_without_flags(0xFFFF_FFFF), Err(LINUX_EINVAL));
        assert_eq!(sched_policy_without_flags(0x5_0000_0001), Ok(1));
        // A NULL param or negative pid is EINVAL before anything is read.
        assert_eq!(read_sched_param(0, 0).err(), Some(LINUX_EINVAL));
        assert_eq!(read_sched_param(-1, 0x1000).err(), Some(LINUX_EINVAL));
    }

    #[test]
    fn mmsg_batches_report_count_before_error() {
        assert_eq!(mmsg_batch_result(3, Some(LINUX_EAGAIN)), 3);
        assert_eq!(mmsg_batch_result(0, Some(LINUX_EAGAIN)), linux_errno(LINUX_EAGAIN));
        assert_eq!(mmsg_batch_result(0, None), 0);
        assert_eq!(mmsg_batch_result(5, None), 5);
        assert_eq!(mmsg_entry(0x1000, 2), 0x1000 + 128);
        assert_eq!(linux_abi::UIO_MAXIOV, 1024);
    }
}
