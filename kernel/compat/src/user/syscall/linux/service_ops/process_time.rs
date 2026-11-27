//! Linux process-time and sleep ABI over scheduler-local monotonic substrate.
//!
//! - **Owner:** Compat owns Linux argument/result semantics; HAL and
//!   `kernel-ps` own clocks, deadlines, and task blocking.
//! - **Boundary:** User timespecs and clock IDs are fully validated before a
//!   waiter or copyout is published.
//! - **Lifecycle:** Query or finite sleep follows register, arm, commit,
//!   resume, disarm, and terminal-result ordering.
//! - **Concurrency:** The hot path performs no policy-service IPC, allocation,
//!   or process-state locking before waiter publication.
//! - **Failure:** Signal, timeout, invalid clock, and task retirement return
//!   exact Linux outcomes without leaking a waiter.
//! - **Forbidden:** No syscalld round trip, calendar-time timeout, busy loop,
//!   or unbounded sleep record.
//! - **Evidence:** `monotonic-deadline-lifecycle`.
use super::*;

pub fn syscall_linux_sched_yield() -> u64 {
    multitask::request_yield_reschedule();
    0
}

pub fn syscall_linux_getpid() -> u64 {
    multitask::current_user_process_id().unwrap_or(0)
}

pub fn syscall_linux_gettid() -> u64 {
    multitask::current_user_thread_id().unwrap_or(0)
}

pub fn syscall_linux_execve(
    frame: &mut SyscallFrame,
    path_ptr: u64,
    argv_ptr: u64,
    envp_ptr: u64,
) -> u64 {
    procd_exec(
        frame,
        PROCD_OP_EXECVE,
        linux_abi::AT_FDCWD as u64,
        path_ptr,
        argv_ptr,
        envp_ptr,
        0,
    )
}

pub fn syscall_linux_execveat(
    frame: &mut SyscallFrame,
    dirfd: u64,
    path_ptr: u64,
    argv_ptr: u64,
    envp_ptr: u64,
    flags: u64,
) -> u64 {
    procd_exec(
        frame,
        PROCD_OP_EXECVEAT,
        dirfd,
        path_ptr,
        argv_ptr,
        envp_ptr,
        flags,
    )
}

pub fn syscall_linux_fork(frame: &SyscallFrame) -> u64 {
    procd_fork(frame, 0, 0, 0, 0, 0)
}

pub fn syscall_linux_vfork(frame: &SyscallFrame) -> u64 {
    procd_fork(
        frame,
        linux_abi::CLONE_VM | linux_abi::CLONE_VFORK | linux_abi::SIGCHLD,
        0,
        0,
        0,
        0,
    )
}

pub fn syscall_linux_rt_sigaction(
    signal: u64,
    action_ptr: u64,
    old_action_ptr: u64,
    sigset_size: u64,
) -> u64 {
    if sigset_size != size_of::<u64>() as u64 {
        return linux_errno(LINUX_EINVAL);
    }
    let mut request = new_procd_request(PROCD_OP_RT_SIGACTION);
    request.arg0 = signal;
    if action_ptr != 0 {
        let action = match usermem::read_current_user_struct::<LinuxSigActionWire>(action_ptr) {
            Ok(action) => action,
            Err(err) => return linux_errno(user_pointer_error_to_linux_errno(err)),
        };
        request.flags |= 0x1;
        request.payload_len = LINUX_SIGACTION_SIZE as u32;
        request.payload[..LINUX_SIGACTION_SIZE].copy_from_slice(as_bytes(&action));
    }
    if old_action_ptr != 0 {
        if let Err(err) =
            usermem::validate_current_user_write_buffer(old_action_ptr, LINUX_SIGACTION_SIZE)
        {
            return linux_errno(user_pointer_error_to_linux_errno(err));
        }
        request.flags |= 0x2;
    }
    let response = match call_procd(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if old_action_ptr != 0 {
        if response.status != 0 {
            return linux_errno(response.status.unsigned_abs() as i64);
        }
        if response.payload_len as usize != LINUX_SIGACTION_SIZE {
            return linux_errno(LINUX_EINVAL);
        }
        match usermem::write_current_user_bytes(
            old_action_ptr,
            &response.payload[..LINUX_SIGACTION_SIZE],
        ) {
            Ok(()) => 0,
            Err(err) => linux_errno(user_pointer_error_to_linux_errno(err)),
        }
    } else {
        match ensure_empty_procd_response(&response) {
            Ok(()) => 0,
            Err(errno) => linux_errno(errno),
        }
    }
}

/// Replaces the calling thread's signal mask with `mask` in procd's policy
/// copy and the ring0 copy, returning the mask that was in force.
pub(super) fn swap_signal_mask(mask: u64) -> Result<u64, i64> {
    update_procd_signal_mask(linux_abi::SIG_SETMASK, Some(mask), true)?.ok_or(LINUX_EINVAL)
}

/// Puts `original` back after a wait that ran under a temporary mask.
///
/// When the wait delivered a handler, the signal frame already carries
/// `original` for `sigreturn` and the ring0 mask is the handler's (`original`
/// plus `sa_mask` plus the signal), which must survive; only procd's policy
/// copy still holds the temporary mask.
pub(super) fn restore_signal_mask_after_wait(original: u64, handler_installed: bool) {
    let handler_mask = handler_installed
        .then(|| multitask::current_linux_thread_state().map(|state| state.signal_mask))
        .flatten();
    let _ = update_procd_signal_mask(linux_abi::SIG_SETMASK, Some(original), false);
    if let Some(mask) = handler_mask {
        sync_current_linux_signal_mask(linux_abi::SIG_SETMASK, mask);
    }
}

fn update_procd_signal_mask(
    how: u64,
    requested_mask: Option<u64>,
    wants_old: bool,
) -> Result<Option<u64>, i64> {
    let mut request = new_procd_request(PROCD_OP_RT_SIGPROCMASK);
    request.arg0 = how;
    if let Some(mask) = requested_mask {
        request.flags |= 0x1;
        request.payload_len = 8;
        request.payload[..8].copy_from_slice(&mask.to_ne_bytes());
    }
    if wants_old {
        request.flags |= 0x2;
    }
    let response = call_procd(&request)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if let Some(mask) = requested_mask {
        sync_current_linux_signal_mask(how, mask);
    }
    if wants_old {
        if response.payload_len as usize != 8 {
            return Err(LINUX_EINVAL);
        }
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(&response.payload[..8]);
        Ok(Some(u64::from_ne_bytes(bytes)))
    } else {
        ensure_empty_procd_response(&response)?;
        Ok(None)
    }
}

pub fn syscall_linux_rt_sigprocmask(
    how: u64,
    set_ptr: u64,
    oldset_ptr: u64,
    sigset_size: u64,
) -> u64 {
    if sigset_size != size_of::<u64>() as u64 {
        return linux_errno(LINUX_EINVAL);
    }
    let requested_mask = if set_ptr != 0 {
        let mut set = [0_u8; 8];
        if let Err(err) = usermem::copy_from_current_user_exact(set_ptr, &mut set) {
            return linux_errno(user_pointer_error_to_linux_errno(err));
        }
        Some(u64::from_ne_bytes(set))
    } else {
        None
    };
    if oldset_ptr != 0
        && let Err(err) = usermem::validate_current_user_write_buffer(oldset_ptr, 8)
    {
        return linux_errno(user_pointer_error_to_linux_errno(err));
    }
    let old = match update_procd_signal_mask(how, requested_mask, oldset_ptr != 0) {
        Ok(old) => old,
        Err(errno) => return linux_errno(errno),
    };
    if let Some(old_mask) = old {
        match usermem::write_current_user_bytes(oldset_ptr, &old_mask.to_ne_bytes()) {
            Ok(()) => 0,
            Err(err) => linux_errno(user_pointer_error_to_linux_errno(err)),
        }
    } else {
        0
    }
}

pub fn syscall_linux_rt_sigpending(set_ptr: u64, sigset_size: u64) -> u64 {
    linux_unit_result((|| {
        if sigset_size != size_of::<u64>() as u64 {
            return Err(LINUX_EINVAL);
        }
        let pending = multitask::current_linux_thread_state()
            .map(|state| state.pending_signals & state.signal_mask)
            .unwrap_or(0);
        write_user_bytes(set_ptr, &pending.to_ne_bytes())
    })())
}

fn wait_for_interruptible_signal() -> Result<(), i64> {
    loop {
        if current_wait_interrupted() {
            return Err(LINUX_EINTR);
        }
        while !multitask::arm_block_current_task() {
            multitask::cond_resched();
            if current_wait_interrupted() {
                return Err(LINUX_EINTR);
            }
        }
        if multitask::current_linux_thread_state()
            .is_some_and(|state| state.pending_signals & !state.signal_mask != 0)
        {
            let _ = multitask::cancel_block_current_task();
            multitask::cond_resched();
            continue;
        }
        if multitask::commit_block_current_task_and_yield().is_none() {
            return Err(LINUX_EINVAL);
        }
    }
}

pub fn syscall_linux_pause() -> u64 {
    linux_errno(wait_for_interruptible_signal().unwrap_err())
}

pub(in crate::user::syscall::linux) fn syscall_linux_rt_sigsuspend(frame: &mut SyscallFrame) -> u64 {
    let mask_ptr = frame.rdi;
    let sigset_size = frame.rsi;
    if sigset_size != size_of::<u64>() as u64 {
        return linux_errno(LINUX_EINVAL);
    }
    let mut set = [0_u8; 8];
    if let Err(errno) = read_user_bytes(mask_ptr, &mut set) {
        return linux_errno(errno);
    }
    let temp_mask = u64::from_ne_bytes(set);
    let old_mask = match update_procd_signal_mask(linux_abi::SIG_SETMASK, Some(temp_mask), true) {
        Ok(Some(old)) => old,
        Ok(None) => return linux_errno(LINUX_EINVAL),
        Err(errno) => return linux_errno(errno),
    };
    let err = wait_for_interruptible_signal().unwrap_err();
    frame.rax = linux_errno(err);
    let delivered = deliver_pending_signals_if_needed_with_saved_mask(frame, Some(old_mask));
    restore_signal_mask_after_wait(old_mask, delivered);
    if delivered {
        frame.rax
    } else {
        linux_errno(err)
    }
}

fn sync_current_linux_signal_mask(how: u64, requested_mask: u64) {
    const SIG_BLOCK: u64 = 0;
    const SIG_UNBLOCK: u64 = 1;
    const SIG_SETMASK: u64 = 2;
    let unblockable = (1_u64 << (9 - 1)) | (1_u64 << (19 - 1));
    let _ = multitask::with_current_user_process_and_linux_thread_state_mut(
        |_, _, _, _, linux_thread_state| {
            let Some(state) = linux_thread_state.as_mut() else {
                return;
            };
            match how {
                SIG_BLOCK => state.signal_mask |= requested_mask & !unblockable,
                SIG_UNBLOCK => state.signal_mask &= !requested_mask,
                SIG_SETMASK => state.signal_mask = requested_mask & !unblockable,
                _ => {}
            }
        },
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimeHotPath {
    ClockGettime,
    Nanosleep,
    ClockNanosleep,
}

/// Validate the fixed Linux clock/sleep ABI envelope without acquiring shared
/// process state or synchronously entering a policy service. Clock reads and
/// bounded sleeps are scheduler/timer substrate and must establish deadline
/// recovery even while another same-process thread mutates unrelated state.
fn validate_time_hot_path_locally(
    kind: TimeHotPath,
    clock_id: u64,
    flags: u64,
    timespec: Option<LinuxTimespecWire>,
) -> Result<(), i64> {
    match kind {
        TimeHotPath::ClockGettime => {
            if flags != 0 || timespec.is_some() || !is_readable_clock_id(clock_id) {
                return Err(LINUX_EINVAL);
            }
        }
        TimeHotPath::Nanosleep => {
            if clock_id != 0 || flags != 0 {
                return Err(LINUX_EINVAL);
            }
            validate_sleep_timespec(timespec.ok_or(LINUX_EINVAL)?)?;
        }
        TimeHotPath::ClockNanosleep => {
            if !is_supported_clock_id(clock_id) || flags != 0 && flags != linux_abi::TIMER_ABSTIME {
                return Err(LINUX_EINVAL);
            }
            validate_sleep_timespec(timespec.ok_or(LINUX_EINVAL)?)?;
        }
    }
    Ok(())
}

// Linux clock ids that are views of the two clocksources this kernel owns.
// RAW/COARSE/BOOTTIME differ from MONOTONIC/REALTIME only in slewing,
// resolution and suspend accounting, none of which exist here, and libc,
// glib and runtimes read them routinely (a spurious EINVAL there panics
// std-style `Instant::now` callers). The TAI clock stays unsupported; process
// and thread CPU-time clocks are readable (see `is_readable_clock_id`) from
// scheduler per-thread accounting.
const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;
const CLOCK_THREAD_CPUTIME_ID: u64 = 3;
const CLOCK_MONOTONIC_RAW: u64 = 4;
const CLOCK_REALTIME_COARSE: u64 = 5;
const CLOCK_MONOTONIC_COARSE: u64 = 6;
const CLOCK_BOOTTIME: u64 = 7;

fn is_realtime_clock_id(clock_id: u64) -> bool {
    clock_id == linux_abi::CLOCK_REALTIME || clock_id == CLOCK_REALTIME_COARSE
}

/// Clocks `clock_gettime`/`clock_getres` serve. `CLOCK_PROCESS_CPUTIME_ID` and
/// `CLOCK_THREAD_CPUTIME_ID` are readable but are not instants a sleep can wait
/// on, so `clock_nanosleep` keeps using `is_supported_clock_id`, as Linux does.
fn is_readable_clock_id(clock_id: u64) -> bool {
    is_supported_clock_id(clock_id)
        || clock_id == CLOCK_PROCESS_CPUTIME_ID
        || clock_id == CLOCK_THREAD_CPUTIME_ID
}

fn is_supported_clock_id(clock_id: u64) -> bool {
    is_realtime_clock_id(clock_id)
        || matches!(
            clock_id,
            id if id == linux_abi::CLOCK_MONOTONIC
                || id == CLOCK_MONOTONIC_RAW
                || id == CLOCK_MONOTONIC_COARSE
                || id == CLOCK_BOOTTIME
        )
}

fn validate_sleep_timespec(timespec: LinuxTimespecWire) -> Result<(), i64> {
    if timespec.tv_sec < 0 || !(0..1_000_000_000).contains(&timespec.tv_nsec) {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub(super) fn validate_relative_sleep_timespec_locally(
    timespec: LinuxTimespecWire,
) -> Result<(), i64> {
    validate_time_hot_path_locally(TimeHotPath::Nanosleep, 0, 0, Some(timespec))
}

/// Nanoseconds a `timespec` spans, saturating at `u64::MAX`.
fn timespec_total_nanos(ts: LinuxTimespecWire) -> u64 {
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

/// The monotonic instant an absolute `target` on a clock reading `clock_now`
/// corresponds to when the monotonic clock reads `mono_now`.
fn absolute_deadline_on_monotonic(mono_now: u64, clock_now: u64, target: u64) -> u64 {
    mono_now.saturating_add(target.saturating_sub(clock_now))
}

/// Sleeps until the monotonic instant `deadline_ns` or until a signal that
/// will be delivered arrives. `Some(remaining_ns)` reports the interruption,
/// `None` a completed sleep. The wait itself has the millisecond resolution
/// of the timer substrate, so a short wake re-arms for the remainder.
fn sleep_until_monotonic_or_signal(deadline_ns: u64) -> Option<u64> {
    loop {
        let now = crate::arch::rtc::monotonic_nanos();
        if now >= deadline_ns {
            return None;
        }
        let millis = (deadline_ns - now).saturating_add(999_999) / 1_000_000;
        crate::arch::rtc::sleep_unless_with_post_arm(
            millis.max(1),
            current_wait_interrupted,
            || {
                multitask::current_linux_thread_state()
                    .is_some_and(|state| state.pending_signals & !state.signal_mask != 0)
            },
        );
        let now = crate::arch::rtc::monotonic_nanos();
        if now >= deadline_ns {
            return None;
        }
        if current_wait_interrupted() {
            return Some(deadline_ns - now);
        }
    }
}

/// Finishes an interruptible sleep: `EINTR`, with the unslept time stored in
/// `remaining_ptr` when the caller asked for it (relative sleeps only).
fn sleep_interruption_result(remaining_ns: u64, remaining_ptr: u64) -> Result<(), i64> {
    if remaining_ptr != 0 {
        write_user_bytes(remaining_ptr, as_bytes(&timespec_from_nanos(remaining_ns)))?;
    }
    Err(LINUX_EINTR)
}

pub fn syscall_linux_nanosleep(request_ptr: u64, remaining_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let ts: LinuxTimespecWire = read_user_struct(request_ptr)?;
        validate_time_hot_path_locally(TimeHotPath::Nanosleep, 0, 0, Some(ts))?;
        let deadline =
            crate::arch::rtc::monotonic_nanos().saturating_add(timespec_total_nanos(ts));
        match sleep_until_monotonic_or_signal(deadline) {
            None => Ok(()),
            Some(remaining) => sleep_interruption_result(remaining, remaining_ptr),
        }
    })())
}

fn clock_gettime_with_writer(
    clock_id: u64,
    timespec_ptr: u64,
    write: impl FnOnce(u64, &[u8]) -> LinuxResult<()>,
) -> LinuxResult<()> {
    validate_time_hot_path_locally(TimeHotPath::ClockGettime, clock_id, 0, None)?;
    let ts = read_clock_timespec(clock_id)?;
    write(timespec_ptr, as_bytes(&ts))
}

pub fn syscall_linux_clock_gettime(clock_id: u64, timespec_ptr: u64) -> u64 {
    linux_unit_result(clock_gettime_with_writer(
        clock_id,
        timespec_ptr,
        write_user_bytes,
    ))
}

/// Linux `time(tloc)`: returns wall-clock seconds since the Unix epoch and
/// stores them into `tloc` when non-null.
pub fn syscall_linux_time(tloc_ptr: u64) -> u64 {
    linux_result((|| {
        let sec = realtime_timespec().tv_sec;
        if tloc_ptr != 0 {
            write_user_bytes(tloc_ptr, &sec.to_le_bytes())?;
        }
        Ok(sec as u64)
    })())
}

fn read_clock_timespec(clock_id: u64) -> LinuxResult<LinuxTimespecWire> {
    if clock_id == CLOCK_THREAD_CPUTIME_ID || clock_id == CLOCK_PROCESS_CPUTIME_ID {
        Ok(timespec_from_nanos(
            multitask::current_thread_cpu_time_ns().ok_or(LINUX_EINVAL)?,
        ))
    } else {
        Ok(current_clock_timespec_substrate(clock_id))
    }
}

pub fn syscall_linux_clock_getres(clock_id: u64, res_ptr: u64) -> u64 {
    linux_unit_result((|| {
        if !is_readable_clock_id(clock_id) {
            return Err(LINUX_EINVAL);
        }
        if res_ptr == 0 {
            return Ok(());
        }
        let res = LinuxTimespecWire {
            tv_sec: 0,
            tv_nsec: 1,
        };
        write_user_bytes(res_ptr, as_bytes(&res))
    })())
}

pub fn syscall_linux_gettimeofday(tv_ptr: u64, tz_ptr: u64) -> u64 {
    linux_unit_result((|| {
        if tv_ptr != 0 {
            let now = realtime_timespec();
            let tv = linux_abi::LinuxTimeval {
                tv_sec: now.tv_sec,
                tv_usec: now.tv_nsec / 1000,
            };
            write_user_struct(tv_ptr, &tv)?;
        }
        if tz_ptr != 0 {
            write_user_bytes(tz_ptr, &[0_u8; 8])?;
        }
        Ok(())
    })())
}

pub fn syscall_linux_getrusage(who: i64, rusage_ptr: u64) -> u64 {
    const RUSAGE_SELF: i64 = 0;
    const RUSAGE_CHILDREN: i64 = -1;
    const RUSAGE_THREAD: i64 = 1;
    linux_unit_result((|| {
        if !matches!(who, RUSAGE_SELF | RUSAGE_CHILDREN | RUSAGE_THREAD) {
            return Err(LINUX_EINVAL);
        }
        let mut usage = linux_abi::LinuxRusage::default();
        if matches!(who, RUSAGE_SELF | RUSAGE_THREAD) {
            if let Some(snapshot) = multitask::current_scheduling_context_runtime_snapshot() {
                let consumed_us = snapshot.context_consumed_ns / 1000;
                usage.ru_utime = linux_abi::LinuxTimeval {
                    tv_sec: (consumed_us / 1_000_000) as i64,
                    tv_usec: (consumed_us % 1_000_000) as i64,
                };
            }
        }
        write_user_struct(rusage_ptr, &usage)
    })())
}

pub fn syscall_linux_times(tms_ptr: u64) -> u64 {
    const USER_HZ: u64 = 100;
    const NS_PER_TICK: u64 = 1_000_000_000 / USER_HZ;
    linux_result((|| {
        if tms_ptr != 0 {
            let utime = multitask::current_scheduling_context_runtime_snapshot()
                .map(|s| (s.context_consumed_ns / NS_PER_TICK) as i64)
                .unwrap_or(0);
            let tms = linux_abi::LinuxTms {
                tms_utime: utime,
                tms_stime: 0,
                tms_cutime: 0,
                tms_cstime: 0,
            };
            write_user_struct(tms_ptr, &tms)?;
        }
        Ok(crate::arch::rtc::monotonic_nanos() / NS_PER_TICK)
    })())
}

pub fn syscall_linux_sysinfo(info_ptr: u64) -> u64 {
    let info = linux_abi::LinuxSysinfo {
        uptime: monotonic_timespec().tv_sec,
        loads: [0; 3],
        totalram: crate::memory::usable_bytes(),
        freeram: crate::memory::free_bytes(),
        sharedram: 0,
        bufferram: 0,
        totalswap: 0,
        freeswap: 0,
        procs: (multitask::live_process_count().min(u16::MAX as usize) as u16).max(1),
        pad: 0,
        _pad2: 0,
        totalhigh: 0,
        freehigh: 0,
        mem_unit: 1,
        _pad3: 0,
    };
    linux_unit_result(write_user_struct(info_ptr, &info))
}

pub fn syscall_linux_getcpu(cpu_ptr: u64, node_ptr: u64) -> u64 {
    linux_unit_result((|| {
        if cpu_ptr != 0 {
            let cpu = nucleus_core::util::lockdep::current_cpu_index() as u32;
            write_user_struct(cpu_ptr, &cpu)?;
        }
        if node_ptr != 0 {
            let node = 0_u32;
            write_user_struct(node_ptr, &node)?;
        }
        Ok(())
    })())
}

pub fn syscall_linux_sched_getscheduler(pid: i64) -> u64 {
    match super::super::process_ext_ops::sched_target(pid) {
        Ok(_) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_sched_getparam(pid: i64, param_ptr: u64) -> u64 {
    linux_unit_result((|| {
        // Linux reports a NULL `param` as EINVAL, before it looks at the target.
        if param_ptr == 0 || pid < 0 {
            return Err(LINUX_EINVAL);
        }
        super::super::process_ext_ops::sched_target(pid)?;
        let param = linux_abi::LinuxSchedParam { sched_priority: 0 };
        write_user_struct(param_ptr, &param)
    })())
}

pub fn syscall_linux_sched_rr_get_interval(pid: i64, ts_ptr: u64) -> u64 {
    linux_unit_result((|| {
        super::super::process_ext_ops::sched_target(pid)?;
        let ts = LinuxTimespecWire {
            tv_sec: 0,
            tv_nsec: 0,
        };
        write_user_bytes(ts_ptr, as_bytes(&ts))
    })())
}

pub fn sleep_relative_timespec_substrate(ts: LinuxTimespecWire) {
    let nanos = (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64);
    if nanos != 0 {
        let millis = nanos.saturating_add(999_999) / 1_000_000;
        crate::arch::rtc::sleep(millis.max(1));
    }
}

pub fn current_clock_timespec_substrate(clock_id: u64) -> LinuxTimespecWire {
    if is_realtime_clock_id(clock_id) {
        realtime_timespec()
    } else {
        monotonic_timespec()
    }
}

/// Reports the validated clocksource directly, in its own resolution.
///
/// `CLOCK_MONOTONIC` is the only instant userspace can read, so it is what
/// every service measures an interval with. Routing it through the
/// tick-numbered deadline wheel rounded every answer to 1/1024 s, and a
/// millisecond is longer than most of what ring 3 measures: inputd reported
/// `drain_us=0 decode_us=0` for work it had certainly done, and the transport
/// turn it could see came back as a choice between 0, 977, and 1953
/// microseconds. The tick domain remains the authority for *arming* a
/// deadline; it was never the authority for reporting an instant.
pub fn monotonic_timespec() -> LinuxTimespecWire {
    timespec_from_nanos(crate::arch::rtc::monotonic_nanos())
}

fn timespec_from_nanos(nanos: u64) -> LinuxTimespecWire {
    LinuxTimespecWire {
        tv_sec: (nanos / 1_000_000_000) as i64,
        tv_nsec: (nanos % 1_000_000_000) as i64,
    }
}

/// Wall-clock nanoseconds at monotonic zero, latched on the first successful
/// calendar read and never re-read afterwards.
///
/// Reading the CMOS calendar is not a cheap query. `rtc::now` disables
/// interrupts, spins until the chip is not mid-update, and then reads seven
/// registers twice over to prove the value did not change underneath it. Every
/// one of those is a port access, and under hardware virtualization every port
/// access is a VM exit, so a `clock_gettime(CLOCK_REALTIME)` cost upwards of
/// thirty exits with interrupts off - to answer with a value whose resolution
/// was one second.
///
/// Anchoring the calendar to the validated monotonic clocksource once is what
/// every general-purpose kernel does with its wall clock, and it is what makes
/// the answer both cheap and finer than a second. The absolute error is
/// whatever the one-second calendar read carried at latch time, which is the
/// same error the repeated read had; what changes is that the value now
/// advances continuously and cannot be observed going backwards.
static REALTIME_EPOCH_NANOS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static REALTIME_EPOCH_LATCHED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub fn realtime_timespec() -> LinuxTimespecWire {
    use core::sync::atomic::Ordering;
    // ORDERING: acquire pairs with the release that publishes the latch, so a
    // CPU that sees the flag also sees the epoch the winning CPU wrote.
    if REALTIME_EPOCH_LATCHED.load(Ordering::Acquire) {
        return timespec_from_nanos(
            REALTIME_EPOCH_NANOS
                .load(Ordering::Relaxed)
                .saturating_add(crate::arch::rtc::monotonic_nanos()),
        );
    }
    latch_realtime_epoch()
}

#[cold]
fn latch_realtime_epoch() -> LinuxTimespecWire {
    use core::sync::atomic::Ordering;
    let now = crate::arch::rtc::now();
    let Some(seconds) = rtc_datetime_to_unix_seconds(
        now.year, now.month, now.day, now.hour, now.minute, now.second,
    ) else {
        // An unreadable calendar stays unreadable rather than latching a
        // fabricated epoch that every later query would inherit.
        return LinuxTimespecWire {
            tv_sec: 0,
            tv_nsec: 0,
        };
    };
    let wall_nanos = seconds.saturating_mul(1_000_000_000);
    if crate::arch::clock::current_source().is_none() {
        // No admitted elapsed-time domain to anchor against yet. Answer from
        // the calendar and try again on the next query.
        return timespec_from_nanos(wall_nanos);
    }
    let monotonic = crate::arch::rtc::monotonic_nanos();
    let epoch = wall_nanos.saturating_sub(monotonic);
    // ORDERING: exactly one CPU may install the epoch, so the write is a
    // compare-exchange against the never-latched zero rather than a store.
    if REALTIME_EPOCH_NANOS
        .compare_exchange(0, epoch, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        // ORDERING: release keeps the epoch write ahead of the flag that
        // advertises it, so the acquiring fast path cannot read a zero epoch.
        REALTIME_EPOCH_LATCHED.store(true, Ordering::Release);
    }
    // A CPU that lost the race reads the winner's epoch, so both return the
    // same time domain from this call onwards.
    timespec_from_nanos(
        REALTIME_EPOCH_NANOS
            .load(Ordering::Relaxed)
            .saturating_add(monotonic),
    )
}

fn rtc_datetime_to_unix_seconds(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
) -> Option<u64> {
    if year < 1970 || month == 0 || month > 12 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let month_days = days_in_month(year, month)?;
    if day == 0 || day > month_days {
        return None;
    }

    let mut days = 0_u64;
    let mut y = 1970_u16;
    while y < year {
        days += if is_leap_year(y) { 366 } else { 365 };
        y += 1;
    }
    let mut m = 1_u8;
    while m < month {
        days += u64::from(days_in_month(year, m)?);
        m += 1;
    }
    days += u64::from(day - 1);

    Some(
        days.saturating_mul(86_400)
            .saturating_add(u64::from(hour) * 3_600)
            .saturating_add(u64::from(minute) * 60)
            .saturating_add(u64::from(second)),
    )
}

fn days_in_month(year: u16, month: u8) -> Option<u8> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 if is_leap_year(year) => Some(29),
        2 => Some(28),
        _ => None,
    }
}

fn is_leap_year(year: u16) -> bool {
    let year = u64::from(year);
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn syscall_linux_clock_nanosleep(
    clock_id: u64,
    flags: u64,
    request_ptr: u64,
    remaining_ptr: u64,
) -> u64 {
    linux_unit_result((|| {
        let ts: LinuxTimespecWire = read_user_struct(request_ptr)?;
        validate_time_hot_path_locally(TimeHotPath::ClockNanosleep, clock_id, flags, Some(ts))?;
        let mono_now = crate::arch::rtc::monotonic_nanos();
        let absolute = flags & linux_abi::TIMER_ABSTIME != 0;
        let deadline = if absolute {
            let clock_now = timespec_total_nanos(current_clock_timespec_substrate(clock_id));
            absolute_deadline_on_monotonic(mono_now, clock_now, timespec_total_nanos(ts))
        } else {
            mono_now.saturating_add(timespec_total_nanos(ts))
        };
        match sleep_until_monotonic_or_signal(deadline) {
            None => Ok(()),
            // An absolute sleep is restarted by the caller with the same
            // deadline, so Linux reports no remaining time for it.
            Some(remaining) => {
                sleep_interruption_result(remaining, if absolute { 0 } else { remaining_ptr })
            }
        }
    })())
}

// RING3-MIGRATION-REFERENCE START: scheduler-thread substrate exception:
// procd owns clone/process admission policy. Ring0 keeps task creation plus
// fixed futex/time ABI validation and scheduler wait/deadline substrate.
const CLASSIC_CLONE_FLAGS_MASK: u64 = 0xffff_ffff;

pub fn syscall_linux_clone(frame: &SyscallFrame) -> u64 {
    // Linux `clone(2)` takes only the low 32 bits as flags; bit 32 and up
    // (`CLONE_CLEAR_SIGHAND`, ...) exist for `clone3` alone and are ignored.
    record_clone_result(
        1,
        clone_linux_thread(
            frame,
            frame.rdi & CLASSIC_CLONE_FLAGS_MASK,
            frame.rsi,
            frame.rdx,
            frame.r10,
            frame.r8,
        ),
    )
}

pub fn syscall_linux_futex(
    _frame: &SyscallFrame,
    uaddr: u64,
    op: u64,
    val: u64,
    timeout_ptr: u64,
    uaddr2: u64,
    val3: u64,
) -> u64 {
    futex_impl(uaddr, op, val, timeout_ptr, uaddr2, val3)
}

pub fn syscall_linux_clone3(frame: &SyscallFrame) -> u64 {
    let expected_size = size_of::<linux_abi::LinuxCloneArgs>();
    // CLONE_ARGS_SIZE_VER0: anything smaller cannot carry the mandatory fields
    // (flags through tls) and is EINVAL, not a zero-padded request.
    let requested_size = match clone3_args_size(frame.rsi) {
        Ok(size) => size,
        Err(errno) => return linux_errno(errno),
    };
    let provided_size = requested_size.min(expected_size);
    let mut args = linux_abi::LinuxCloneArgs::default();
    // SAFETY: `args` is a live, zero-initialized `#[repr(C)]` LinuxCloneArgs of u64
    // fields (no padding, any byte pattern valid) and `provided_size` was checked
    // above to be <= size_of it, so the exclusive byte view stays inside `args`.
    // It is only used for the user copy below, before `args` is read.
    let args_bytes = unsafe {
        slice::from_raw_parts_mut(core::ptr::addr_of_mut!(args).cast::<u8>(), provided_size)
    };
    if let Err(errno) = read_user_bytes(frame.rdi, args_bytes) {
        return linux_errno(errno);
    }
    // A newer, larger struct is accepted while every field this kernel does not
    // know is zero; a set unknown field is E2BIG, never silently ignored.
    if requested_size > expected_size {
        let mut offset = expected_size;
        while offset < requested_size {
            let mut tail = [0_u8; 64];
            let len = (requested_size - offset).min(tail.len());
            if let Err(errno) = read_user_bytes(frame.rdi + offset as u64, &mut tail[..len]) {
                return linux_errno(errno);
            }
            if tail[..len].iter().any(|&byte| byte != 0) {
                return linux_errno(LINUX_E2BIG);
            }
            offset += len;
        }
    }
    if let Err(errno) = validate_clone3_args(&args) {
        return linux_errno(errno);
    }
    let child_stack = if args.stack == 0 && args.stack_size == 0 {
        0
    } else {
        match args.stack.checked_add(args.stack_size) {
            Some(value) => value,
            None => return linux_errno(LINUX_EINVAL),
        }
    };
    record_clone_result(
        3,
        clone_linux_thread(
            frame,
            args.flags | (args.exit_signal & linux_abi::CSIGNAL),
            child_stack,
            args.parent_tid,
            args.child_tid,
            args.tls,
        ),
    )
}

/// `copy_struct_from_user` admission of the `clone3` size: below
/// `CLONE_ARGS_SIZE_VER0` is `EINVAL`, above a page `E2BIG`; anything between
/// is admitted (a size beyond this kernel's struct is checked for zero fields).
fn clone3_args_size(size: u64) -> Result<usize, i64> {
    const CLONE_ARGS_SIZE_VER0: u64 = 64;
    const CLONE_ARGS_SIZE_MAX: u64 = 4096;
    if size < CLONE_ARGS_SIZE_VER0 {
        return Err(LINUX_EINVAL);
    }
    if size > CLONE_ARGS_SIZE_MAX {
        return Err(LINUX_E2BIG);
    }
    Ok(size as usize)
}

fn validate_clone3_args(args: &linux_abi::LinuxCloneArgs) -> Result<(), i64> {
    if args.flags & linux_abi::CSIGNAL != 0 || args.exit_signal & !linux_abi::CSIGNAL != 0 {
        return Err(LINUX_EINVAL);
    }
    if args.flags & linux_abi::CLONE_PIDFD != 0
        || args.set_tid != 0
        || args.set_tid_size != 0
        || args.flags & linux_abi::CLONE_INTO_CGROUP != 0
    {
        return Err(LINUX_ENOSYS);
    }
    Ok(())
}

fn record_clone_result(kind: u64, result: u64) -> u64 {
    let signed = result as i64;
    if signed < 0 {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "linux-thread-clone-rejected",
            kind,
            signed.unsigned_abs(),
        );
    }
    result
}
// RING3-MIGRATION-REFERENCE END: procd-owned clone policy and scheduler substrate exception.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_monotonic_instant_between_two_ticks_keeps_its_own_resolution() {
        let ticks_per_second = crate::arch::rtc::ticks_per_second();
        let tick_nanos = 1_000_000_000 / ticks_per_second;
        // Two instants inside one tick. The tick-numbered deadline wheel cannot
        // tell them apart, which is exactly why reporting an instant must not
        // be routed through it.
        let early = 7 * tick_nanos + 1_000;
        let late = 7 * tick_nanos + tick_nanos - 1_000;

        assert_eq!(
            timespec_from_nanos(early).tv_nsec as u64,
            early,
            "a sub-tick instant must survive the conversion",
        );
        assert!(timespec_from_nanos(early).tv_nsec < timespec_from_nanos(late).tv_nsec);

        // The seconds split stays exact across a boundary that is not a whole
        // number of ticks.
        let crossing = 3 * 1_000_000_000 + 12_345;
        assert_eq!(
            timespec_from_nanos(crossing),
            LinuxTimespecWire {
                tv_sec: 3,
                tv_nsec: 12_345,
            }
        );
    }

    #[test]
    fn sleep_deadlines_and_unslept_time_are_exact() {
        let ts = |tv_sec, tv_nsec| LinuxTimespecWire { tv_sec, tv_nsec };
        assert_eq!(timespec_total_nanos(ts(2, 5)), 2_000_000_005);
        assert_eq!(timespec_total_nanos(ts(i64::MAX, 0)), u64::MAX);
        // An absolute realtime deadline 5 s ahead of the realtime clock lies
        // 5 s ahead of the monotonic clock; one already passed is due now.
        assert_eq!(absolute_deadline_on_monotonic(100, 1_000, 6_000), 5_100);
        assert_eq!(absolute_deadline_on_monotonic(100, 1_000, 1_000), 100);
        assert_eq!(absolute_deadline_on_monotonic(100, 1_000, 10), 100);
        assert_eq!(absolute_deadline_on_monotonic(u64::MAX - 1, 0, 9), u64::MAX);
        // An interrupted sleep is EINTR, and a caller that passed no
        // remaining-time pointer gets exactly that and nothing else.
        assert_eq!(sleep_interruption_result(1_500_000_000, 0), Err(LINUX_EINTR));
    }

    #[test]
    fn clone3_size_admission_follows_copy_struct_from_user() {
        let known = size_of::<linux_abi::LinuxCloneArgs>();
        assert_eq!(clone3_args_size(63), Err(LINUX_EINVAL));
        assert_eq!(clone3_args_size(0), Err(LINUX_EINVAL));
        assert_eq!(clone3_args_size(64), Ok(64));
        assert_eq!(clone3_args_size(known as u64), Ok(known));
        // A larger (newer) struct is admitted for the zero-tail check, up to a page.
        assert_eq!(clone3_args_size(known as u64 + 8), Ok(known + 8));
        assert_eq!(clone3_args_size(4096), Ok(4096));
        assert_eq!(clone3_args_size(4097), Err(LINUX_E2BIG));
    }

    #[test]
    fn monotonic_and_realtime_clock_aliases_are_admitted() {
        for id in [CLOCK_MONOTONIC_RAW, CLOCK_MONOTONIC_COARSE, CLOCK_BOOTTIME] {
            assert!(is_supported_clock_id(id));
            assert!(!is_realtime_clock_id(id));
        }
        assert!(is_supported_clock_id(CLOCK_REALTIME_COARSE));
        assert!(is_realtime_clock_id(CLOCK_REALTIME_COARSE));
        // Process/thread CPU clocks (2, 3) are readable but never a sleep clock,
        // and TAI (11) has no backing source here.
        for id in [2, 3, 8, 11, 12] {
            assert!(!is_supported_clock_id(id));
        }
        assert!(is_readable_clock_id(CLOCK_PROCESS_CPUTIME_ID));
        assert!(is_readable_clock_id(CLOCK_THREAD_CPUTIME_ID));
        assert!(!is_readable_clock_id(11));
    }

    #[test]
    fn time_hot_path_admission_is_local_and_complete() {
        let valid = LinuxTimespecWire {
            tv_sec: 0,
            tv_nsec: 16_000_000,
        };
        assert_eq!(
            validate_time_hot_path_locally(
                TimeHotPath::ClockGettime,
                linux_abi::CLOCK_MONOTONIC,
                0,
                None,
            ),
            Ok(())
        );
        assert_eq!(
            validate_time_hot_path_locally(TimeHotPath::Nanosleep, 0, 0, Some(valid)),
            Ok(())
        );
        assert_eq!(
            validate_time_hot_path_locally(
                TimeHotPath::ClockNanosleep,
                linux_abi::CLOCK_REALTIME,
                linux_abi::TIMER_ABSTIME,
                Some(valid),
            ),
            Ok(())
        );
        assert_eq!(
            validate_time_hot_path_locally(TimeHotPath::ClockGettime, u64::MAX, 0, None,),
            Err(LINUX_EINVAL)
        );
        assert_eq!(
            validate_relative_sleep_timespec_locally(LinuxTimespecWire {
                tv_sec: 0,
                tv_nsec: 1_000_000_000,
            }),
            Err(LINUX_EINVAL)
        );
        assert_eq!(
            validate_time_hot_path_locally(
                TimeHotPath::ClockNanosleep,
                linux_abi::CLOCK_MONOTONIC,
                u64::MAX,
                Some(valid),
            ),
            Err(LINUX_EINVAL)
        );
    }

    #[test]
    fn clone3_rejects_csignal_bits_or_bad_exit_signal_with_einval() {
        let mut args = linux_abi::LinuxCloneArgs::default();
        args.flags = linux_abi::SIGCHLD;
        assert_eq!(validate_clone3_args(&args), Err(LINUX_EINVAL));
        args.flags = linux_abi::CLONE_VM | linux_abi::CLONE_VFORK | linux_abi::CLONE_CLEAR_SIGHAND;
        args.exit_signal = u64::MAX;
        assert_eq!(validate_clone3_args(&args), Err(LINUX_EINVAL));
        args.exit_signal = linux_abi::SIGCHLD;
        assert_eq!(validate_clone3_args(&args), Ok(()));
    }

    #[test]
    fn clock_gettime_validates_clock_id_before_copyout_and_writes_directly() {
        // Invalid clock_id must return EINVAL before touching user memory
        // (even when timespec_ptr is NULL).
        assert_eq!(
            syscall_linux_clock_gettime(u64::MAX, 0),
            linux_errno(LINUX_EINVAL)
        );
        assert_eq!(
            syscall_linux_clock_gettime(11, 0),
            linux_errno(LINUX_EINVAL)
        );
        let mut called = false;
        assert_eq!(
            clock_gettime_with_writer(u64::MAX, 0, |_, _| {
                called = true;
                Ok(())
            }),
            Err(LINUX_EINVAL)
        );
        assert!(!called, "invalid clock_id must fail before copyout");

        // Supported monotonic clocks write a 16-byte LinuxTimespecWire directly
        // without pre-validating via ProcessStateLock.
        for id in [
            linux_abi::CLOCK_MONOTONIC,
            CLOCK_MONOTONIC_RAW,
            CLOCK_MONOTONIC_COARSE,
            CLOCK_BOOTTIME,
        ] {
            let mut captured = [0_u8; 16];
            let mut written_ptr = 0_u64;
            assert_eq!(
                clock_gettime_with_writer(id, 0x7fff_0000_1000, |ptr, bytes| {
                    written_ptr = ptr;
                    assert_eq!(bytes.len(), size_of::<LinuxTimespecWire>());
                    captured.copy_from_slice(bytes);
                    Ok(())
                }),
                Ok(())
            );
            assert_eq!(written_ptr, 0x7fff_0000_1000);
            let ts: LinuxTimespecWire = read_unaligned(&captured);
            assert!(ts.tv_sec >= 0);
            assert!((0..1_000_000_000).contains(&ts.tv_nsec));
        }

        // Faulting / NULL output buffer propagates EFAULT from copyout.
        assert_eq!(
            clock_gettime_with_writer(linux_abi::CLOCK_MONOTONIC, 0, |ptr, _| {
                if ptr == 0 { Err(LINUX_EFAULT) } else { Ok(()) }
            }),
            Err(LINUX_EFAULT)
        );
    }
}
