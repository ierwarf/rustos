use super::*;

pub fn syscall_linux_sched_yield() -> u64 {
    multitask::request_deferred_reschedule();
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
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
        request.flags |= 0x1;
        request.payload_len = LINUX_SIGACTION_SIZE as u32;
        request.payload[..LINUX_SIGACTION_SIZE].copy_from_slice(as_bytes(&action));
    }
    if old_action_ptr != 0 {
        if let Err(err) =
            usermem::validate_current_user_write_buffer(old_action_ptr, LINUX_SIGACTION_SIZE)
        {
            return linux_errno(address_space_error_to_linux_errno(err));
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
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        }
    } else {
        match ensure_empty_procd_response(&response) {
            Ok(()) => 0,
            Err(errno) => linux_errno(errno),
        }
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
    let mut request = new_procd_request(PROCD_OP_RT_SIGPROCMASK);
    request.arg0 = how;
    let mut requested_mask = 0_u64;
    if set_ptr != 0 {
        let mut set = [0_u8; 8];
        if let Err(err) = usermem::copy_from_current_user_exact(set_ptr, &mut set) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        requested_mask = u64::from_ne_bytes(set);
        request.flags |= 0x1;
        request.payload_len = 8;
        request.payload[..8].copy_from_slice(&set);
    }
    if oldset_ptr != 0 {
        if let Err(err) = usermem::validate_current_user_write_buffer(oldset_ptr, 8) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        request.flags |= 0x2;
    }
    let response = match call_procd(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if set_ptr != 0 {
        sync_current_linux_signal_mask(how, requested_mask);
    }
    if oldset_ptr != 0 {
        if response.payload_len as usize != 8 {
            return linux_errno(LINUX_EINVAL);
        }
        match usermem::write_current_user_bytes(oldset_ptr, &response.payload[..8]) {
            Ok(()) => 0,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        }
    } else {
        match ensure_empty_procd_response(&response) {
            Ok(()) => 0,
            Err(errno) => linux_errno(errno),
        }
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

pub fn syscall_linux_nanosleep(request_ptr: u64, _remaining_ptr: u64) -> u64 {
    let ts = match usermem::read_current_user_struct::<LinuxTimespecWire>(request_ptr) {
        Ok(ts) => ts,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if let Err(errno) =
        request_syscalld_timespec_admission(SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP, 0, 0, ts)
    {
        return linux_errno(errno);
    }
    sleep_relative_timespec_substrate(ts);
    0
}

pub fn syscall_linux_clock_gettime(clock_id: u64, timespec_ptr: u64) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(timespec_ptr, LINUX_TIMESPEC_SIZE)
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(errno) = request_syscalld_clock_gettime_admission(clock_id) {
        return linux_errno(errno);
    }
    write_clock_timespec(clock_id, timespec_ptr)
}

fn write_clock_timespec(clock_id: u64, timespec_ptr: u64) -> u64 {
    let ts = current_clock_timespec_substrate(clock_id);
    match usermem::write_current_user_bytes(timespec_ptr, as_bytes(&ts)) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
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
    match clock_id {
        id if id == linux_abi::CLOCK_REALTIME as u64 => realtime_timespec(),
        _ => monotonic_timespec(),
    }
}

fn timespec_lte(lhs: LinuxTimespecWire, rhs: LinuxTimespecWire) -> bool {
    lhs.tv_sec < rhs.tv_sec || lhs.tv_sec == rhs.tv_sec && lhs.tv_nsec <= rhs.tv_nsec
}

pub fn sleep_until_timespec_substrate(clock_id: u64, deadline: LinuxTimespecWire) {
    let now = current_clock_timespec_substrate(clock_id);
    if timespec_lte(deadline, now) {
        return;
    }
    let mut tv_sec = deadline.tv_sec.saturating_sub(now.tv_sec);
    let mut tv_nsec = deadline.tv_nsec - now.tv_nsec;
    if tv_nsec < 0 {
        tv_sec = tv_sec.saturating_sub(1);
        tv_nsec += 1_000_000_000;
    }
    sleep_relative_timespec_substrate(LinuxTimespecWire { tv_sec, tv_nsec });
}

pub fn monotonic_timespec() -> LinuxTimespecWire {
    let ticks = crate::arch::rtc::ticks();
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    LinuxTimespecWire {
        tv_sec: (ticks / ticks_per_second) as i64,
        tv_nsec: ((ticks % ticks_per_second).saturating_mul(1_000_000_000) / ticks_per_second)
            as i64,
    }
}

pub fn realtime_timespec() -> LinuxTimespecWire {
    let now = crate::arch::rtc::now();
    let seconds = rtc_datetime_to_unix_seconds(
        now.year, now.month, now.day, now.hour, now.minute, now.second,
    )
    .unwrap_or(0);
    LinuxTimespecWire {
        tv_sec: seconds as i64,
        tv_nsec: 0,
    }
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
    _remaining_ptr: u64,
) -> u64 {
    let ts = match usermem::read_current_user_struct::<LinuxTimespecWire>(request_ptr) {
        Ok(ts) => ts,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if let Err(errno) = request_syscalld_timespec_admission(
        SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP,
        clock_id,
        flags,
        ts,
    ) {
        return linux_errno(errno);
    }
    sleep_clock_nanosleep_substrate(clock_id, flags, ts)
}

fn sleep_clock_nanosleep_substrate(clock_id: u64, flags: u64, ts: LinuxTimespecWire) -> u64 {
    if flags & linux_abi::TIMER_ABSTIME as u64 != 0 {
        sleep_until_timespec_substrate(clock_id, ts);
    } else {
        sleep_relative_timespec_substrate(ts);
    }
    0
}

// RING3-MIGRATION-REFERENCE START: scheduler-thread substrate exception:
// procd/syscalld own clone/futex/time admission policy. Ring0 keeps scheduler
// mutation, futex wait queues, and task creation substrate.
pub fn syscall_linux_clone(frame: &SyscallFrame) -> u64 {
    clone_linux_thread(frame, frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
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
    let provided_size = match usize::try_from(frame.rsi) {
        Ok(size) if size != 0 && size <= expected_size => size,
        _ => return linux_errno(LINUX_EINVAL),
    };
    let mut args = linux_abi::LinuxCloneArgs::default();
    let args_bytes = unsafe {
        slice::from_raw_parts_mut(core::ptr::addr_of_mut!(args).cast::<u8>(), provided_size)
    };
    if let Err(err) = usermem::copy_from_current_user_exact(frame.rdi, args_bytes) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if args.flags & linux_abi::CLONE_PIDFD != 0
        || args.set_tid != 0
        || args.set_tid_size != 0
        || args.flags & linux_abi::CLONE_INTO_CGROUP != 0
        || args.exit_signal & !linux_abi::CSIGNAL != 0
    {
        return linux_errno(LINUX_ENOSYS);
    }
    let child_stack = if args.stack == 0 && args.stack_size == 0 {
        0
    } else {
        match args.stack.checked_add(args.stack_size) {
            Some(value) => value,
            None => return linux_errno(LINUX_EINVAL),
        }
    };
    clone_linux_thread(
        frame,
        args.flags | (args.exit_signal & linux_abi::CSIGNAL),
        child_stack,
        args.parent_tid,
        args.child_tid,
        args.tls,
    )
}
// RING3-MIGRATION-REFERENCE END: procd/syscalld-owned clone/futex/time substrate exception.
