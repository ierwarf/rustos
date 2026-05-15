use super::*;
use lazy_static::lazy_static;
use spin::Mutex;
use x86_64::VirtAddr;

const FUTEX_WAITERS_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FutexKey {
    address_space_root: u64,
    uaddr: u64,
}

#[derive(Clone, Copy, Debug)]
struct FutexWaiter {
    key: FutexKey,
    task_id: u64,
    bitset: u32,
}

lazy_static! {
    static ref FUTEX_WAITERS: Mutex<[Option<FutexWaiter>; FUTEX_WAITERS_CAPACITY]> =
        Mutex::new([None; FUTEX_WAITERS_CAPACITY]);
}

pub(super) fn syscall_linux_poll(fds_ptr: u64, nfds: u64, _timeout_ms: i64) -> u64 {
    const POLLFD_SIZE: u64 = 8;
    let Ok(nfds_usize) = usize::try_from(nfds) else {
        return linux_errno(LINUX_EINVAL);
    };
    if nfds_usize > 1024 {
        return linux_errno(LINUX_EINVAL);
    }
    let mut ready = 0_u64;
    for i in 0..nfds {
        let Some(entry_ptr) = fds_ptr.checked_add(i.saturating_mul(POLLFD_SIZE)) else {
            return linux_errno(LINUX_EFAULT);
        };
        let mut entry = [0_u8; 8];
        if let Err(err) = usermem::copy_from_current_user_exact(entry_ptr, &mut entry) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let fd = i32::from_le_bytes(entry[0..4].try_into().unwrap());
        let events = u16::from_le_bytes(entry[4..6].try_into().unwrap());
        let revents: u16 = if fd < 0 {
            0
        } else if fd <= 2 {
            let ready_bits = events & (linux_abi::POLLIN | linux_abi::POLLOUT) as u16;
            if ready_bits != 0 {
                ready += 1;
            }
            ready_bits
        } else {
            let has_handle = multitask::with_current_user_process_state(|_, _, ps| {
                ps.handles().get(fd as u64).is_some()
            })
            .unwrap_or(false);
            if has_handle {
                let ready_bits = events & (linux_abi::POLLIN | linux_abi::POLLOUT) as u16;
                if ready_bits != 0 {
                    ready += 1;
                }
                ready_bits
            } else {
                ready += 1;
                linux_abi::POLLNVAL as u16
            }
        };
        entry[6..8].copy_from_slice(&revents.to_le_bytes());
        if let Err(err) = usermem::write_current_user_bytes(entry_ptr, &entry) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }
    ready
}

pub(super) fn syscall_linux_ppoll(
    fds_ptr: u64,
    nfds: u64,
    timeout_ptr: u64,
    _sigmask_ptr: u64,
) -> u64 {
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

pub(super) fn syscall_linux_epoll_create1(flags: u64) -> u64 {
    if flags & !linux_abi::EPOLL_CLOEXEC != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let fd_flags = if flags & linux_abi::EPOLL_CLOEXEC != 0 {
        multitask::FD_CLOEXEC
    } else {
        0
    };
    let handle = multitask::KernelHandle::Epoll(multitask::EpollHandle::new());
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().install_entry(multitask::HandleEntry::new(
            handle,
            fd_flags,
            linux_abi::O_RDONLY,
        ))
    }) {
        Some(fd) => fd,
        None => linux_errno(LINUX_ENOSYS),
    }
}

pub(super) fn syscall_linux_epoll_ctl(epfd: u64, op: u64, fd: u64, event_ptr: u64) -> u64 {
    if epfd == fd {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(epoll) = current_epoll_handle(epfd) else {
        return linux_errno(LINUX_EINVAL);
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
            let result = if op == linux_abi::EPOLL_CTL_ADD {
                epoll.add(fd, handle, events, data)
            } else {
                epoll.modify(fd, handle, events, data)
            };
            match result {
                Ok(()) => 0,
                Err(err) => linux_errno(epoll_error_to_linux_errno(err)),
            }
        }
        linux_abi::EPOLL_CTL_DEL => match epoll.delete(fd) {
            Ok(()) => 0,
            Err(err) => linux_errno(epoll_error_to_linux_errno(err)),
        },
        _ => linux_errno(LINUX_EINVAL),
    }
}

pub(super) fn syscall_linux_epoll_wait(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    _timeout_ms: i64,
) -> u64 {
    let Ok(maxevents) = usize::try_from(maxevents) else {
        return linux_errno(LINUX_EINVAL);
    };
    if maxevents == 0 || maxevents > 1024 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(
        events_ptr,
        maxevents.saturating_mul(size_of::<linux_abi::LinuxEpollEvent>()),
    ) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let Some(epoll) = current_epoll_handle(epfd) else {
        return linux_errno(LINUX_EINVAL);
    };
    let mut written = 0usize;
    for interest in epoll.snapshot().into_iter().take(maxevents) {
        let events = interest.events
            & (linux_abi::EPOLLIN
                | linux_abi::EPOLLPRI
                | linux_abi::EPOLLOUT
                | linux_abi::EPOLLERR
                | linux_abi::EPOLLHUP);
        if events == 0 {
            continue;
        }
        let Some(entry_ptr) =
            events_ptr.checked_add((written * size_of::<linux_abi::LinuxEpollEvent>()) as u64)
        else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = write_linux_epoll_event(entry_ptr, events, interest.data) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        written += 1;
    }
    written as u64
}

pub(super) fn syscall_linux_sched_yield() -> u64 {
    multitask::yield_now();
    0
}

fn current_process_is_linux_syscall_policy_owner() -> bool {
    super::ipc_ops::current_process_has_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY,
    )
}

pub(super) fn syscall_linux_getpid() -> u64 {
    multitask::current_user_process_id().unwrap_or(0)
}

pub(super) fn syscall_linux_gettid() -> u64 {
    multitask::current_user_thread_id().unwrap_or(0)
}

pub(super) fn syscall_linux_rt_sigaction(
    signal: u64,
    action_ptr: u64,
    old_action_ptr: u64,
    _sigset_size: u64,
) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_RT_SIGACTION);
    request.arg0 = signal;
    if action_ptr != 0 {
        let action = match usermem::read_current_user_struct::<LinuxSigActionWire>(action_ptr) {
            Ok(action) => action,
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
        request.mask |= 0x1;
        request.path_len = LINUX_SIGACTION_SIZE as u32;
        request.path[..LINUX_SIGACTION_SIZE].copy_from_slice(as_bytes(&action));
    }
    if old_action_ptr != 0 {
        if let Err(err) =
            usermem::validate_current_user_write_buffer(old_action_ptr, LINUX_SIGACTION_SIZE)
        {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        request.mask |= 0x2;
    }
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if old_action_ptr != 0 {
        if let Err(errno) = ensure_syscalld_payload(&response, LINUX_SIGACTION_SIZE) {
            return linux_errno(errno);
        }
        match usermem::write_current_user_bytes(
            old_action_ptr,
            &response.payload[..LINUX_SIGACTION_SIZE],
        ) {
            Ok(()) => 0,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        }
    } else {
        match ensure_empty_syscalld_response(&response) {
            Ok(()) => 0,
            Err(errno) => linux_errno(errno),
        }
    }
}

pub(super) fn syscall_linux_rt_sigprocmask(
    how: u64,
    set_ptr: u64,
    oldset_ptr: u64,
    _sigset_size: u64,
) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_RT_SIGPROCMASK);
    request.arg0 = how;
    if set_ptr != 0 {
        let mut set = [0_u8; 8];
        if let Err(err) = usermem::copy_from_current_user_exact(set_ptr, &mut set) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        request.mask |= 0x1;
        request.path_len = 8;
        request.path[..8].copy_from_slice(&set);
    }
    if oldset_ptr != 0 {
        if let Err(err) = usermem::validate_current_user_write_buffer(oldset_ptr, 8) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        request.mask |= 0x2;
    }
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if oldset_ptr != 0 {
        if let Err(errno) = ensure_syscalld_payload(&response, 8) {
            return linux_errno(errno);
        }
        match usermem::write_current_user_bytes(oldset_ptr, &response.payload[..8]) {
            Ok(()) => 0,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        }
    } else {
        match ensure_empty_syscalld_response(&response) {
            Ok(()) => 0,
            Err(errno) => linux_errno(errno),
        }
    }
}

pub(super) fn syscall_linux_nanosleep(request_ptr: u64, _remaining_ptr: u64) -> u64 {
    if current_process_is_linux_syscall_policy_owner() {
        return syscall_linux_policy_owner_nanosleep(request_ptr);
    }

    let ts = match usermem::read_current_user_struct::<LinuxTimespecWire>(request_ptr) {
        Ok(ts) => ts,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP);
    request.path_len = LINUX_TIMESPEC_SIZE as u32;
    request.path[..LINUX_TIMESPEC_SIZE].copy_from_slice(as_bytes(&ts));
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_clock_gettime(clock_id: u64, timespec_ptr: u64) -> u64 {
    if current_process_is_linux_syscall_policy_owner() {
        return syscall_linux_policy_owner_clock_gettime(clock_id, timespec_ptr);
    }

    if let Err(err) = usermem::validate_current_user_write_buffer(timespec_ptr, LINUX_TIMESPEC_SIZE)
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME);
    request.arg0 = clock_id;
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_syscalld_payload(&response, LINUX_TIMESPEC_SIZE) {
        return linux_errno(errno);
    }
    match usermem::write_current_user_bytes(timespec_ptr, &response.payload[..LINUX_TIMESPEC_SIZE])
    {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

fn syscall_linux_policy_owner_nanosleep(request_ptr: u64) -> u64 {
    let ts = match usermem::read_current_user_struct::<LinuxTimespecWire>(request_ptr) {
        Ok(ts) => ts,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return linux_errno(LINUX_EINVAL);
    }
    let nanos = (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64);
    if nanos != 0 {
        let millis = nanos.saturating_add(999_999) / 1_000_000;
        crate::arch::rtc::sleep(millis.max(1));
    }
    0
}

fn syscall_linux_policy_owner_clock_gettime(clock_id: u64, timespec_ptr: u64) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(timespec_ptr, LINUX_TIMESPEC_SIZE)
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }

    let ts = match clock_id {
        id if id == linux_abi::CLOCK_MONOTONIC as u64 => monotonic_timespec(),
        id if id == linux_abi::CLOCK_REALTIME as u64 => realtime_timespec(),
        _ => return linux_errno(LINUX_EINVAL),
    };
    match usermem::write_current_user_bytes(timespec_ptr, as_bytes(&ts)) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

fn monotonic_timespec() -> LinuxTimespecWire {
    let ticks = crate::arch::rtc::ticks();
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    LinuxTimespecWire {
        tv_sec: (ticks / ticks_per_second) as i64,
        tv_nsec: ((ticks % ticks_per_second).saturating_mul(1_000_000_000) / ticks_per_second)
            as i64,
    }
}

fn realtime_timespec() -> LinuxTimespecWire {
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

pub(super) fn syscall_linux_clock_nanosleep(
    clock_id: u64,
    flags: u64,
    request_ptr: u64,
    _remaining_ptr: u64,
) -> u64 {
    let ts = match usermem::read_current_user_struct::<LinuxTimespecWire>(request_ptr) {
        Ok(ts) => ts,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP);
    request.arg0 = clock_id;
    request.arg1 = flags;
    request.path_len = LINUX_TIMESPEC_SIZE as u32;
    request.path[..LINUX_TIMESPEC_SIZE].copy_from_slice(as_bytes(&ts));
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_futex_minimal(
    uaddr: u64,
    op: u64,
    val: u64,
    timeout_ptr: u64,
    uaddr2: u64,
    val3: u64,
) -> u64 {
    futex_impl(uaddr, op, val, timeout_ptr, uaddr2, val3)
}

pub(super) fn syscall_linux_clone(frame: &SyscallFrame) -> u64 {
    clone_linux_thread(frame, frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
}

pub(super) fn syscall_linux_futex(
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

pub(super) fn syscall_linux_clone3(frame: &SyscallFrame) -> u64 {
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

fn futex_impl(uaddr: u64, op: u64, val: u64, timeout_ptr: u64, uaddr2: u64, val3: u64) -> u64 {
    if (uaddr & 0x3) != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let cmd = op & linux_abi::FUTEX_CMD_MASK;
    let supported_flags = linux_abi::FUTEX_PRIVATE_FLAG | linux_abi::FUTEX_CLOCK_REALTIME;
    if (op & !linux_abi::FUTEX_CMD_MASK) & !supported_flags != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let result = match cmd {
        c if c == linux_abi::FUTEX_WAIT => futex_wait(
            uaddr,
            val as u32,
            timeout_ptr,
            linux_abi::FUTEX_BITSET_MATCH_ANY,
        ),
        c if c == linux_abi::FUTEX_WAIT_BITSET => {
            let Ok(bitset) = u32::try_from(val3) else {
                return linux_errno(LINUX_EINVAL);
            };
            if bitset == 0 {
                return linux_errno(LINUX_EINVAL);
            }
            futex_wait(uaddr, val as u32, timeout_ptr, bitset)
        }
        c if c == linux_abi::FUTEX_WAKE => {
            futex_wake(uaddr, val, linux_abi::FUTEX_BITSET_MATCH_ANY)
        }
        c if c == linux_abi::FUTEX_WAKE_BITSET => {
            let Ok(bitset) = u32::try_from(val3) else {
                return linux_errno(LINUX_EINVAL);
            };
            if bitset == 0 {
                return linux_errno(LINUX_EINVAL);
            }
            futex_wake(uaddr, val, bitset)
        }
        c if c == linux_abi::FUTEX_REQUEUE => futex_requeue(uaddr, val, timeout_ptr, uaddr2),
        c if c == linux_abi::FUTEX_CMP_REQUEUE => {
            futex_cmp_requeue(uaddr, val, timeout_ptr, uaddr2, val3)
        }
        _ => Err(LINUX_ENOSYS),
    };
    match result {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

fn clone_linux_thread(
    frame: &SyscallFrame,
    flags: u64,
    child_stack: u64,
    parent_tid_ptr: u64,
    child_tid_ptr: u64,
    tls: u64,
) -> u64 {
    const REQUIRED_THREAD_FLAGS: u64 = linux_abi::CLONE_VM
        | linux_abi::CLONE_FS
        | linux_abi::CLONE_FILES
        | linux_abi::CLONE_SIGHAND
        | linux_abi::CLONE_THREAD;
    const OPTIONAL_THREAD_FLAGS: u64 = linux_abi::CLONE_SYSVSEM
        | linux_abi::CLONE_SETTLS
        | linux_abi::CLONE_PARENT_SETTID
        | linux_abi::CLONE_CHILD_CLEARTID
        | linux_abi::CLONE_CHILD_SETTID;

    let exit_signal = flags & linux_abi::CSIGNAL;
    let supported_flags = REQUIRED_THREAD_FLAGS | OPTIONAL_THREAD_FLAGS | linux_abi::CSIGNAL;
    if exit_signal != 0 || flags & !supported_flags != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if flags & REQUIRED_THREAD_FLAGS != REQUIRED_THREAD_FLAGS {
        return linux_errno(LINUX_ENOSYS);
    }
    if child_stack == 0
        || !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&child_stack)
    {
        return linux_errno(LINUX_EINVAL);
    }
    if flags & linux_abi::CLONE_SETTLS != 0
        && tls != 0
        && !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&tls)
    {
        return linux_errno(LINUX_EINVAL);
    }

    let console_session = multitask::current_console_session();
    let child_thread_state = match multitask::with_current_user_linux_state_mut(
        |_, _, abi, address_space, _, linux_thread_state| {
            if abi != crate::user::abi::UserAbi::Linux {
                return Err(LINUX_ENOSYS);
            }
            let Some(parent_thread_state) = linux_thread_state.as_ref() else {
                return Err(LINUX_ENOSYS);
            };
            if flags & linux_abi::CLONE_PARENT_SETTID != 0 {
                address_space
                    .validate_user_write_buffer(VirtAddr::new(parent_tid_ptr), size_of::<u32>())
                    .map_err(address_space_error_to_linux_errno)?;
            }
            if flags & (linux_abi::CLONE_CHILD_SETTID | linux_abi::CLONE_CHILD_CLEARTID) != 0 {
                address_space
                    .validate_user_write_buffer(VirtAddr::new(child_tid_ptr), size_of::<u32>())
                    .map_err(address_space_error_to_linux_errno)?;
            }

            let mut child_thread_state = *parent_thread_state;
            if flags & linux_abi::CLONE_SETTLS != 0 {
                child_thread_state.fs_base = tls;
            }
            child_thread_state.clear_child_tid = if flags & linux_abi::CLONE_CHILD_CLEARTID != 0 {
                child_tid_ptr
            } else {
                0
            };
            child_thread_state.robust_list_head = 0;
            child_thread_state.robust_list_len = 0;
            child_thread_state.rseq_area = 0;
            child_thread_state.rseq_len = 0;
            child_thread_state.rseq_signature = 0;
            child_thread_state.pending_signals = 0;
            child_thread_state.signal_stack = linux_abi::LinuxSignalStack {
                sp: 0,
                flags: linux_abi::SS_DISABLE,
                _pad: 0,
                size: 0,
            };
            Ok(child_thread_state)
        },
    ) {
        Some(Ok(state)) => state,
        Some(Err(errno)) => return linux_errno(errno),
        None => return linux_errno(LINUX_ENOSYS),
    };

    let mut bootstrap = multitask::UserTaskBootstrap::new(
        crate::user::abi::UserAbi::Linux,
        VirtAddr::new(frame.user_rip),
        VirtAddr::new(child_stack),
    );
    bootstrap.console_session = console_session;
    bootstrap.linux_thread_state = Some(child_thread_state);
    bootstrap.registers = multitask::UserTaskRegisters {
        rax: 0,
        rbx: frame.rbx,
        rcx: frame.user_rip,
        rdx: frame.rdx,
        rsi: frame.rsi,
        rdi: frame.rdi,
        rbp: frame.rbp,
        r8: frame.r8,
        r9: frame.r9,
        r10: frame.r10,
        r11: frame.user_rflags,
        r12: frame.r12,
        r13: frame.r13,
        r14: frame.r14,
        r15: frame.r15,
    };

    let child_tid =
        match multitask::spawn_user_thread(bootstrap, multitask::DEFAULT_USER_TASK_WEIGHT_MICROS) {
            Ok(tid) => tid,
            Err(multitask::SpawnTaskError::InvalidWeightMicros) => {
                return linux_errno(LINUX_EINVAL);
            }
            Err(multitask::SpawnTaskError::NoFreeTaskSlot) => return linux_errno(LINUX_EAGAIN),
        };
    let child_tid_bytes = (child_tid as u32).to_le_bytes();
    if flags & (linux_abi::CLONE_PARENT_SETTID | linux_abi::CLONE_CHILD_SETTID) != 0 {
        let result = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
            if abi != crate::user::abi::UserAbi::Linux {
                return Err(LINUX_ENOSYS);
            }
            let address_space = process_state.address_space();
            if flags & linux_abi::CLONE_PARENT_SETTID != 0 {
                address_space
                    .copy_into_user(VirtAddr::new(parent_tid_ptr), &child_tid_bytes)
                    .map_err(address_space_error_to_linux_errno)?;
            }
            if flags & linux_abi::CLONE_CHILD_SETTID != 0 {
                address_space
                    .copy_into_user(VirtAddr::new(child_tid_ptr), &child_tid_bytes)
                    .map_err(address_space_error_to_linux_errno)?;
            }
            Ok(())
        });
        match result {
            Some(Ok(())) => {}
            Some(Err(errno)) => return linux_errno(errno),
            None => return linux_errno(LINUX_ENOSYS),
        }
    }
    child_tid
}

fn futex_wait(uaddr: u64, expected: u32, timeout_ptr: u64, bitset: u32) -> Result<u64, i64> {
    if timeout_ptr != 0 {
        return Err(LINUX_ENOSYS);
    }
    let actual =
        usermem::read_current_user_u32(uaddr).map_err(address_space_error_to_linux_errno)?;
    if actual != expected {
        return Err(LINUX_EAGAIN);
    }
    let (task_id, mut key) = current_futex_waiter_context()?;
    key.uaddr = uaddr;
    register_futex_waiter(FutexWaiter {
        key,
        task_id,
        bitset,
    })?;
    if !multitask::block_current_user_task() {
        clear_futex_waiter(task_id, key);
        return Err(LINUX_ENOSYS);
    }
    multitask::yield_now();
    clear_futex_waiter(task_id, key);
    Ok(0)
}

fn futex_wake(uaddr: u64, max_wake: u64, bitset: u32) -> Result<u64, i64> {
    let max_wake = usize::try_from(max_wake).map_err(|_| LINUX_EINVAL)?;
    if max_wake == 0 {
        return Ok(0);
    }
    let (_, mut key) = current_futex_waiter_context()?;
    key.uaddr = uaddr;
    Ok(wake_futex_waiters(key, max_wake, bitset) as u64)
}

fn futex_requeue(uaddr: u64, max_wake: u64, max_requeue: u64, uaddr2: u64) -> Result<u64, i64> {
    futex_requeue_inner(uaddr, max_wake, max_requeue, uaddr2)
}

fn futex_cmp_requeue(
    uaddr: u64,
    max_wake: u64,
    max_requeue: u64,
    uaddr2: u64,
    expected: u64,
) -> Result<u64, i64> {
    let actual =
        usermem::read_current_user_u32(uaddr).map_err(address_space_error_to_linux_errno)?;
    if actual as u64 != expected {
        return Err(LINUX_EAGAIN);
    }
    futex_requeue_inner(uaddr, max_wake, max_requeue, uaddr2)
}

fn futex_requeue_inner(
    uaddr: u64,
    max_wake: u64,
    max_requeue: u64,
    uaddr2: u64,
) -> Result<u64, i64> {
    if (uaddr2 & 0x3) != 0 {
        return Err(LINUX_EINVAL);
    }
    let max_wake = usize::try_from(max_wake).map_err(|_| LINUX_EINVAL)?;
    let max_requeue = usize::try_from(max_requeue).map_err(|_| LINUX_EINVAL)?;
    let (_, mut from_key) = current_futex_waiter_context()?;
    from_key.uaddr = uaddr;
    let mut to_key = from_key;
    to_key.uaddr = uaddr2;
    let (woke, requeued) = requeue_futex_waiters(
        from_key,
        to_key,
        max_wake,
        max_requeue,
        linux_abi::FUTEX_BITSET_MATCH_ANY,
    );
    Ok((woke + requeued) as u64)
}

fn current_futex_waiter_context() -> Result<(u64, FutexKey), i64> {
    multitask::with_current_user_process_state_mut(|pid, abi, process_state| {
        if abi != crate::user::abi::UserAbi::Linux {
            return Err(LINUX_ENOSYS);
        }
        Ok((
            pid,
            FutexKey {
                address_space_root: process_state.address_space_root(),
                uaddr: 0,
            },
        ))
    })
    .unwrap_or(Err(LINUX_ENOSYS))
}

fn register_futex_waiter(waiter: FutexWaiter) -> Result<(), i64> {
    let mut waiters = FUTEX_WAITERS.lock();
    let mut free_slot = None;
    for slot in 0..waiters.len() {
        match waiters[slot] {
            Some(existing) if !multitask::is_user_task_alive(existing.task_id) => {
                waiters[slot] = None;
                if free_slot.is_none() {
                    free_slot = Some(slot);
                }
            }
            None if free_slot.is_none() => free_slot = Some(slot),
            _ => {}
        }
    }
    let Some(slot) = free_slot else {
        return Err(LINUX_EBUSY);
    };
    waiters[slot] = Some(waiter);
    Ok(())
}

fn clear_futex_waiter(task_id: u64, key: FutexKey) {
    let mut waiters = FUTEX_WAITERS.lock();
    for slot in 0..waiters.len() {
        if waiters[slot]
            .map(|waiter| waiter.task_id == task_id && waiter.key == key)
            .unwrap_or(false)
        {
            waiters[slot] = None;
        }
    }
}

fn wake_futex_waiters(key: FutexKey, max_wake: usize, bitset: u32) -> usize {
    let mut task_ids = [0_u64; FUTEX_WAITERS_CAPACITY];
    let mut wake_count = 0usize;
    {
        let mut waiters = FUTEX_WAITERS.lock();
        for slot in 0..waiters.len() {
            if wake_count == max_wake {
                break;
            }
            let Some(waiter) = waiters[slot] else {
                continue;
            };
            if waiter.key != key || (waiter.bitset & bitset) == 0 {
                continue;
            }
            task_ids[wake_count] = waiter.task_id;
            waiters[slot] = None;
            wake_count += 1;
        }
    }
    let mut woken = 0usize;
    for task_id in task_ids.into_iter().take(wake_count) {
        if multitask::wake_user_task(task_id) {
            woken += 1;
        }
    }
    woken
}

fn requeue_futex_waiters(
    from_key: FutexKey,
    to_key: FutexKey,
    max_wake: usize,
    max_requeue: usize,
    bitset: u32,
) -> (usize, usize) {
    let mut task_ids = [0_u64; FUTEX_WAITERS_CAPACITY];
    let mut wake_count = 0usize;
    let mut requeue_count = 0usize;
    {
        let mut waiters = FUTEX_WAITERS.lock();
        for slot in 0..waiters.len() {
            let Some(mut waiter) = waiters[slot] else {
                continue;
            };
            if waiter.key != from_key || (waiter.bitset & bitset) == 0 {
                continue;
            }
            if wake_count < max_wake {
                task_ids[wake_count] = waiter.task_id;
                waiters[slot] = None;
                wake_count += 1;
                continue;
            }
            if requeue_count < max_requeue {
                waiter.key = to_key;
                waiters[slot] = Some(waiter);
                requeue_count += 1;
                continue;
            }
            break;
        }
    }
    let mut woken = 0usize;
    for task_id in task_ids.into_iter().take(wake_count) {
        if multitask::wake_user_task(task_id) {
            woken += 1;
        }
    }
    (woken, requeue_count)
}

pub(super) fn cleanup_linux_thread_exit() {
    let clear_child_tid = multitask::with_current_user_linux_state_mut(
        |_, _, abi, address_space, _, linux_thread_state| {
            if abi != crate::user::abi::UserAbi::Linux {
                return None;
            }
            let state = linux_thread_state.as_mut()?;
            if state.clear_child_tid == 0 {
                return None;
            }
            let clear_child_tid = state.clear_child_tid;
            let _ =
                address_space.copy_into_user(VirtAddr::new(clear_child_tid), &0_u32.to_le_bytes());
            state.clear_child_tid = 0;
            Some(clear_child_tid)
        },
    )
    .flatten();
    if let Some(clear_child_tid) = clear_child_tid {
        let _ = futex_wake(clear_child_tid, 1, linux_abi::FUTEX_BITSET_MATCH_ANY);
    }
}

pub(super) fn syscall_linux_arch_prctl(_code: u64, _arg: u64) -> u64 {
    match _code {
        linux_abi::ARCH_SET_FS => {
            if _arg != 0
                && !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&_arg)
            {
                return linux_errno(LINUX_EINVAL);
            }
            let Some(result) =
                multitask::with_current_user_linux_state_mut(|_, _, abi, _, _, thread_state| {
                    if abi != crate::user::abi::UserAbi::Linux {
                        return false;
                    }
                    let Some(state) = thread_state.as_mut() else {
                        return false;
                    };
                    state.fs_base = _arg;
                    x86_64::registers::model_specific::FsBase::write(x86_64::VirtAddr::new(_arg));
                    true
                })
            else {
                return linux_errno(LINUX_ENOSYS);
            };
            if result { 0 } else { linux_errno(LINUX_ENOSYS) }
        }
        linux_abi::ARCH_GET_FS => {
            let fs = x86_64::registers::model_specific::FsBase::read().as_u64();
            match usermem::write_current_user_bytes(_arg, &fs.to_le_bytes()) {
                Ok(()) => 0,
                Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
            }
        }
        _ => linux_errno(LINUX_EINVAL),
    }
}

pub(super) fn syscall_linux_set_tid_address(user_ptr: u64) -> u64 {
    let Some(result) =
        multitask::with_current_user_linux_state_mut(|_, tid, abi, _, _, thread_state| {
            if abi != crate::user::abi::UserAbi::Linux {
                return None;
            }
            let state = thread_state.as_mut()?;
            state.clear_child_tid = user_ptr;
            Some(tid)
        })
    else {
        return linux_errno(LINUX_ENOSYS);
    };
    result.unwrap_or_else(|| linux_errno(LINUX_ENOSYS))
}

pub(super) fn syscall_linux_tgkill(tgid: u64, tid: u64, signal: u64) -> u64 {
    if signal > linux_abi::MAX_SIGNAL_NUMBER as u64 {
        return linux_errno(LINUX_EINVAL);
    }
    if multitask::queue_linux_signal(tgid, tid, signal) {
        0
    } else {
        linux_errno(LINUX_ESRCH)
    }
}

pub(super) fn syscall_linux_set_robust_list(head_ptr: u64, len: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST);
    request.arg0 = head_ptr;
    request.arg1 = len;
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_get_robust_list(pid: u64, head_ptr_ptr: u64, len_ptr: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST);
    request.arg0 = pid;
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_syscalld_payload(&response, 16) {
        return linux_errno(errno);
    }
    if let Err(err) = usermem::write_current_user_bytes(head_ptr_ptr, &response.payload[..8]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = usermem::write_current_user_bytes(len_ptr, &response.payload[8..16]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    0
}

pub(super) fn syscall_linux_rseq(area_ptr: u64, len: u64, flags: u64, signature: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_RSEQ);
    request.arg0 = area_ptr;
    request.arg1 = len;
    request.arg2 = flags;
    request.arg3 = signature;
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_openat(dirfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_OPENAT);
    request.dirfd = dirfd;
    request.arg0 = flags;
    request.arg1 = mode;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_VFSD) => {
            return bootstrap_openat(path.as_str(), flags);
        }
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let kind = match response.handle_kind {
        VFS_IPC_HANDLE_KIND_FILE => multitask::RemoteVfsHandleKind::File,
        VFS_IPC_HANDLE_KIND_DIR => multitask::RemoteVfsHandleKind::Directory,
        VFS_IPC_HANDLE_KIND_DEVICE => multitask::RemoteVfsHandleKind::Device,
        _ => return linux_errno(LINUX_EINVAL),
    };
    let handle = multitask::KernelHandle::RemoteVfs(multitask::RemoteVfsHandle::new(
        response.remote_id,
        kind,
        String::from(path.as_str()),
        response.value,
    ));
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_with_open_flags(handle, flags)
    }) {
        Some(fd) => fd,
        None => linux_errno(LINUX_EINVAL),
    }
}

pub(super) fn syscall_linux_vfs_close(fd: u64) -> u64 {
    if let Some(remote) = current_remote_vfs_handle(fd) {
        let mut request = new_vfs_request(VFS_IPC_OP_CLOSE);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        if let Err(errno) =
            call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response))
        {
            return linux_errno(errno);
        }
    }
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().close(fd).is_some()
    }) {
        Some(true) => 0,
        _ => linux_errno(LINUX_EBADF),
    }
}

pub(super) fn syscall_linux_vfs_dup(oldfd: u64, newfd: u64, flags: u64, mode: VfsDupMode) -> u64 {
    if let Some(remote) = current_remote_vfs_handle(oldfd) {
        let mut request = new_vfs_request(VFS_IPC_OP_DUP);
        request.fd = oldfd;
        request.remote_id = remote.remote_id();
        if let Err(errno) =
            call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response))
        {
            return linux_errno(errno);
        }
    }
    let close_on_exec = flags & linux_abi::O_CLOEXEC != 0;
    let result = multitask::with_current_user_process_state_mut(|_, _, process_state| match mode {
        VfsDupMode::Dup => process_state.handles_mut().duplicate_min(oldfd, 0, false),
        VfsDupMode::Dup2 if oldfd == newfd => {
            process_state.handles().get_entry(oldfd).map(|_| oldfd)
        }
        VfsDupMode::Dup2 => process_state
            .handles_mut()
            .duplicate_exact(oldfd, newfd, false),
        VfsDupMode::Dup3 => {
            if oldfd == newfd || flags & !linux_abi::O_CLOEXEC != 0 {
                None
            } else {
                process_state
                    .handles_mut()
                    .duplicate_exact(oldfd, newfd, close_on_exec)
            }
        }
    });
    match result.flatten() {
        Some(fd) => fd,
        None => linux_errno(LINUX_EBADF),
    }
}

pub(super) fn syscall_linux_vfs_read(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if fd == 0 {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        return match crate::user::sysops::console::read_into_current_process(user_ptr, user_len) {
            Ok(read) => read as u64,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    if let Some(mut file) = current_vfs_file_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let mut copied = 0usize;
        let mut chunk = [0_u8; 256];
        while copied < user_len {
            let chunk_len = (user_len - copied).min(chunk.len());
            let read = file.read_into(&mut chunk[..chunk_len]);
            if read == 0 {
                break;
            }
            let Some(dest) = user_ptr.checked_add(copied as u64) else {
                return linux_errno(LINUX_EINVAL);
            };
            if let Err(err) = usermem::write_current_user_bytes(dest, &chunk[..read]) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            copied += read;
            if read < chunk_len {
                break;
            }
        }
        return copied as u64;
    }
    if let Some(mut memfd) = current_memfd_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let mut copied = 0usize;
        let mut chunk = [0_u8; 256];
        while copied < user_len {
            let chunk_len = (user_len - copied).min(chunk.len());
            let read = memfd.read_into(&mut chunk[..chunk_len]);
            if read == 0 {
                break;
            }
            let Some(dest) = user_ptr.checked_add(copied as u64) else {
                return linux_errno(LINUX_EINVAL);
            };
            if let Err(err) = usermem::write_current_user_bytes(dest, &chunk[..read]) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            copied += read;
            if read < chunk_len {
                break;
            }
        }
        return copied as u64;
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut copied = 0usize;
    while copied < user_len {
        let chunk_len = (user_len - copied).min(VFS_IPC_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(VFS_IPC_OP_READ);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        request.arg1 = chunk_len as u64;
        let response = match call_vfs_ipc_request(&request) {
            Ok(response) => response,
            Err(errno) => return linux_errno(errno),
        };
        if let Err(errno) = ensure_vfs_status(&response) {
            return linux_errno(errno);
        }
        let read = response.payload_len as usize;
        if read > chunk_len {
            return linux_errno(LINUX_EINVAL);
        }
        if read == 0 {
            break;
        }
        let Some(dest) = user_ptr.checked_add(copied as u64) else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::write_current_user_bytes(dest, &response.payload[..read]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        copied += read;
        if read < chunk_len {
            break;
        }
    }
    copied as u64
}

pub(super) fn syscall_linux_vfs_pread64(fd: u64, user_ptr: u64, user_len: u64, offset: u64) -> u64 {
    if let Some(memfd) = current_memfd_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        let Ok(offset) = usize::try_from(offset) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let mut copied = 0usize;
        let mut chunk = [0_u8; 256];
        while copied < user_len {
            let chunk_len = (user_len - copied).min(chunk.len());
            let read = memfd.read_at(offset.saturating_add(copied), &mut chunk[..chunk_len]);
            if read == 0 {
                break;
            }
            let Some(dest) = user_ptr.checked_add(copied as u64) else {
                return linux_errno(LINUX_EINVAL);
            };
            if let Err(err) = usermem::write_current_user_bytes(dest, &chunk[..read]) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            copied += read;
            if read < chunk_len {
                break;
            }
        }
        return copied as u64;
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut copied = 0usize;
    while copied < user_len {
        let chunk_len = (user_len - copied).min(VFS_IPC_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(VFS_IPC_OP_PREAD64);
        request.fd = fd;
        request.remote_id = remote.remote_id();
        request.arg0 = offset.saturating_add(copied as u64);
        request.arg1 = chunk_len as u64;
        let response = match call_vfs_ipc_request(&request) {
            Ok(response) => response,
            Err(errno) => return linux_errno(errno),
        };
        if let Err(errno) = ensure_vfs_status(&response) {
            return linux_errno(errno);
        }
        let read = response.payload_len as usize;
        if read > chunk_len {
            return linux_errno(LINUX_EINVAL);
        }
        if read == 0 {
            break;
        }
        let Some(dest) = user_ptr.checked_add(copied as u64) else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::write_current_user_bytes(dest, &response.payload[..read]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        copied += read;
        if read < chunk_len {
            break;
        }
    }
    copied as u64
}

pub(super) fn syscall_linux_vfs_write(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    if fd <= 2 {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        return match crate::user::sysops::console::write_from_current_process(user_ptr, user_len) {
            Ok(written) => written as u64,
            Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
        };
    }
    if let Some(mut memfd) = current_memfd_handle(fd) {
        let Ok(user_len) = usize::try_from(user_len) else {
            return linux_errno(LINUX_EINVAL);
        };
        if user_len == 0 {
            return 0;
        }
        let mut copied = 0usize;
        let mut chunk = [0_u8; 256];
        while copied < user_len {
            let chunk_len = (user_len - copied).min(chunk.len());
            let Some(src) = user_ptr.checked_add(copied as u64) else {
                return linux_errno(LINUX_EINVAL);
            };
            if let Err(err) = usermem::copy_from_current_user_exact(src, &mut chunk[..chunk_len]) {
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            let written = match memfd.write_from(&chunk[..chunk_len]) {
                Ok(written) => written,
                Err(err) => return linux_errno(memfd_error_to_linux_errno(err)),
            };
            copied += written;
            if written < chunk_len {
                break;
            }
        }
        return copied as u64;
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    let chunk_len = user_len.min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
    let mut request = new_vfs_request(VFS_IPC_OP_WRITE);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.payload_len = chunk_len as u32;
    if let Err(err) =
        usermem::copy_from_current_user_exact(user_ptr, &mut request.payload[..chunk_len])
    {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    match call_vfs_ipc_request(&request).and_then(|response| {
        ensure_vfs_status(&response)?;
        Ok(response.value)
    }) {
        Ok(written) => written,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_writev(fd: u64, iov_ptr: u64, iovcnt: u64) -> u64 {
    const LINUX_UIO_MAXIOV: u64 = 1024;
    if iovcnt > LINUX_UIO_MAXIOV {
        return linux_errno(LINUX_EINVAL);
    }
    let mut total = 0_u64;
    for index in 0..iovcnt {
        let Some(entry_ptr) = iov_ptr.checked_add(index.saturating_mul(16)) else {
            return if total == 0 {
                linux_errno(LINUX_EFAULT)
            } else {
                total
            };
        };
        let mut entry = [0_u8; 16];
        if let Err(err) = usermem::copy_from_current_user_exact(entry_ptr, &mut entry) {
            return if total == 0 {
                linux_errno(address_space_error_to_linux_errno(err))
            } else {
                total
            };
        }
        let base = u64::from_le_bytes(entry[..8].try_into().unwrap_or([0; 8]));
        let len = u64::from_le_bytes(entry[8..16].try_into().unwrap_or([0; 8]));
        let mut written_for_iov = 0_u64;
        while written_for_iov < len {
            let Some(chunk_ptr) = base.checked_add(written_for_iov) else {
                return if total == 0 {
                    linux_errno(LINUX_EFAULT)
                } else {
                    total
                };
            };
            let chunk_len = (len - written_for_iov).min(VFS_IPC_REQUEST_PAYLOAD_CAPACITY as u64);
            let result = syscall_linux_vfs_write(fd, chunk_ptr, chunk_len);
            if is_linux_error(result) {
                return if total == 0 { result } else { total };
            }
            if result == 0 {
                return total;
            }
            total = match total.checked_add(result) {
                Some(value) => value,
                None => return linux_errno(LINUX_EINVAL),
            };
            written_for_iov = match written_for_iov.checked_add(result) {
                Some(value) => value,
                None => return linux_errno(LINUX_EINVAL),
            };
            if result < chunk_len {
                return total;
            }
        }
    }
    total
}

fn is_linux_error(result: u64) -> bool {
    let signed = result as i64;
    (-4095..0).contains(&signed)
}

pub(super) fn syscall_linux_vfs_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    if let Some(mut file) = current_vfs_file_handle(fd) {
        let whence = match whence {
            value if value == linux_abi::SEEK_SET => multitask::FileHandleSeekWhence::Start,
            value if value == linux_abi::SEEK_CUR => multitask::FileHandleSeekWhence::Current,
            value if value == linux_abi::SEEK_END => multitask::FileHandleSeekWhence::End,
            _ => return linux_errno(LINUX_EINVAL),
        };
        return match file.seek(offset, whence) {
            Ok(pos) => pos,
            Err(_) => linux_errno(LINUX_EINVAL),
        };
    }
    if let Some(mut memfd) = current_memfd_handle(fd) {
        let whence = match whence {
            value if value == linux_abi::SEEK_SET => multitask::FileHandleSeekWhence::Start,
            value if value == linux_abi::SEEK_CUR => multitask::FileHandleSeekWhence::Current,
            value if value == linux_abi::SEEK_END => multitask::FileHandleSeekWhence::End,
            _ => return linux_errno(LINUX_EINVAL),
        };
        return match memfd.seek(offset, whence) {
            Ok(pos) => pos,
            Err(_) => linux_errno(LINUX_EINVAL),
        };
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let mut request = new_vfs_request(VFS_IPC_OP_LSEEK);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg0 = offset as u64;
    request.arg1 = whence;
    match call_vfs_ipc_request(&request).and_then(|response| {
        ensure_vfs_status(&response)?;
        Ok(response.value)
    }) {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_fstat(fd: u64, stat_ptr: u64) -> u64 {
    if fd <= 2 {
        return write_bootstrap_stat(stat_ptr, fd + 1, 0);
    }
    if let Some(file) = current_vfs_file_handle(fd) {
        return write_bootstrap_stat(
            stat_ptr,
            crate::vfs_core::path_inode(file.path().as_bytes()),
            file.len() as u64,
        );
    }
    if let Some(memfd) = current_memfd_handle(fd) {
        return write_bootstrap_stat(
            stat_ptr,
            crate::vfs_core::path_inode(memfd.path().as_bytes()),
            memfd.len() as u64,
        );
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(stat_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_FSTAT);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    if response.payload_len as usize != LINUX_STAT_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(stat_ptr, &response.payload[..LINUX_STAT_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_ftruncate(fd: u64, len: u64) -> u64 {
    if let Some(memfd) = current_memfd_handle(fd) {
        let Ok(len) = usize::try_from(len) else {
            return linux_errno(LINUX_EINVAL);
        };
        return match memfd.truncate(len) {
            Ok(()) => 0,
            Err(err) => linux_errno(memfd_error_to_linux_errno(err)),
        };
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let mut request = new_vfs_request(VFS_IPC_OP_FTRUNCATE);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg0 = len;
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_getdents64(fd: u64, user_ptr: u64, user_len: u64) -> u64 {
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_GETDENTS64);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg1 = user_len as u64;
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let len = response.payload_len as usize;
    if len > user_len || len > response.payload.len() {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(user_ptr, &response.payload[..len]) {
        Ok(()) => len as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    if fd <= 2 {
        return match cmd {
            linux_abi::F_GETFD => 0,
            linux_abi::F_SETFD => 0,
            linux_abi::F_GETFL => (linux_abi::O_RDWR as u64),
            linux_abi::F_SETFL => 0,
            _ => linux_errno(LINUX_EINVAL),
        };
    }
    if let Some(memfd) = current_memfd_handle(fd) {
        return match cmd {
            linux_abi::F_GET_SEALS => memfd.seals() as u64,
            linux_abi::F_ADD_SEALS => match memfd.add_seals(arg as u32) {
                Ok(()) => 0,
                Err(err) => linux_errno(memfd_error_to_linux_errno(err)),
            },
            _ => linux_errno(LINUX_EINVAL),
        };
    }
    let Some(remote) = current_remote_vfs_handle(fd) else {
        return linux_errno(LINUX_EBADF);
    };
    let mut request = new_vfs_request(VFS_IPC_OP_FCNTL);
    request.fd = fd;
    request.remote_id = remote.remote_id();
    request.arg0 = cmd;
    request.arg1 = arg;
    match call_vfs_ipc_request(&request).and_then(|response| {
        ensure_vfs_status(&response)?;
        Ok(response.value)
    }) {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_statx(
    dirfd: u64,
    path_ptr: u64,
    flags: u64,
    mask: u64,
    statx_ptr: u64,
) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(statx_ptr, LINUX_STATX_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_STATX);
    request.dirfd = dirfd;
    request.flags = flags as u32;
    request.arg1 = mask;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_VFSD) => {
            return bootstrap_stat_path(path.as_str(), statx_ptr, true);
        }
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    if response.payload_len as usize != LINUX_STATX_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(statx_ptr, &response.payload[..LINUX_STATX_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_newfstatat(
    dirfd: u64,
    path_ptr: u64,
    stat_ptr: u64,
    flags: u64,
) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(stat_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_NEWFSTATAT);
    request.dirfd = dirfd;
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_VFSD) => {
            return bootstrap_stat_path(path.as_str(), stat_ptr, false);
        }
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    if response.payload_len as usize != LINUX_STAT_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    match usermem::write_current_user_bytes(stat_ptr, &response.payload[..LINUX_STAT_SIZE]) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_readlinkat(
    dirfd: u64,
    path_ptr: u64,
    user_ptr: u64,
    user_len: u64,
) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut request = new_vfs_request(VFS_IPC_OP_READLINKAT);
    request.dirfd = dirfd;
    request.arg0 = user_len as u64;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let len = (response.payload_len as usize).min(user_len);
    match usermem::write_current_user_bytes(user_ptr, &response.payload[..len]) {
        Ok(()) => len as u64,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_access(dirfd: u64, path_ptr: u64, mode: u64, flags: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_ACCESS);
    request.dirfd = dirfd;
    request.flags = flags as u32;
    request.arg0 = mode;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_VFSD) => {
            match bootstrap_read_file(path.as_str()) {
                Ok(_) => 0,
                Err(errno) => linux_errno(errno),
            }
        }
        Err(errno) => linux_errno(errno),
    }
}

fn current_vfs_file_handle(fd: u64) -> Option<multitask::VfsFileHandle> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::VfsFile(file)) => Some(file.clone()),
            _ => None,
        }
    })
    .flatten()
}

fn current_memfd_handle(fd: u64) -> Option<multitask::MemfdHandle> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::Memfd(memfd)) => Some(memfd.clone()),
            _ => None,
        }
    })
    .flatten()
}

fn current_epoll_handle(fd: u64) -> Option<multitask::EpollHandle> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::Epoll(epoll)) => Some(epoll.clone()),
            _ => None,
        }
    })
    .flatten()
}

fn read_linux_epoll_event(user_ptr: u64) -> Result<(u32, u64), paging::AddressSpaceError> {
    let mut bytes = [0_u8; size_of::<linux_abi::LinuxEpollEvent>()];
    usermem::copy_from_current_user_exact(user_ptr, &mut bytes)?;
    Ok((
        u32::from_le_bytes(bytes[0..4].try_into().unwrap_or([0; 4])),
        u64::from_le_bytes(bytes[4..12].try_into().unwrap_or([0; 8])),
    ))
}

fn write_linux_epoll_event(
    user_ptr: u64,
    events: u32,
    data: u64,
) -> Result<(), paging::AddressSpaceError> {
    let mut bytes = [0_u8; size_of::<linux_abi::LinuxEpollEvent>()];
    bytes[0..4].copy_from_slice(&events.to_le_bytes());
    bytes[4..12].copy_from_slice(&data.to_le_bytes());
    usermem::write_current_user_bytes(user_ptr, &bytes)
}

fn current_kernel_handle(fd: u64) -> Option<multitask::KernelHandle> {
    if fd == 0 {
        return Some(multitask::KernelHandle::Console(
            multitask::ConsoleStreamKind::Input,
        ));
    }
    if fd == 1 {
        return Some(multitask::KernelHandle::Console(
            multitask::ConsoleStreamKind::Output,
        ));
    }
    if fd == 2 {
        return Some(multitask::KernelHandle::Console(
            multitask::ConsoleStreamKind::Error,
        ));
    }
    multitask::with_current_user_process_state(|_, _, process_state| {
        process_state.handles().get(fd).cloned()
    })
    .flatten()
}

fn epoll_error_to_linux_errno(err: multitask::EpollError) -> i64 {
    match err {
        multitask::EpollError::Busy => LINUX_EEXIST,
        multitask::EpollError::InvalidArgument => LINUX_EINVAL,
        multitask::EpollError::NotFound => LINUX_ENOENT,
    }
}

fn memfd_error_to_linux_errno(err: multitask::MemfdError) -> i64 {
    match err {
        multitask::MemfdError::Busy => LINUX_EBUSY,
        multitask::MemfdError::InvalidArgument => LINUX_EINVAL,
        multitask::MemfdError::NoMemory => LINUX_ENOMEM,
        multitask::MemfdError::PermissionDenied => LINUX_EACCES,
    }
}

fn bootstrap_openat(path: &str, flags: u64) -> u64 {
    if flags & linux_abi::O_DIRECTORY != 0 || flags & linux_abi::O_ACCMODE != linux_abi::O_RDONLY {
        return linux_errno(LINUX_ENOSYS);
    }
    let bytes = match bootstrap_read_file(path) {
        Ok(bytes) => bytes,
        Err(errno) => return linux_errno(errno),
    };
    let boot_path = bootstrap_path(path).unwrap_or(path);
    let handle = multitask::KernelHandle::VfsFile(multitask::VfsFileHandle::read_only_memory(
        String::from(boot_path),
        bytes,
    ));
    multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_with_open_flags(handle, flags)
    })
    .unwrap_or_else(|| linux_errno(LINUX_EINVAL))
}

fn bootstrap_stat_path(path: &str, user_ptr: u64, statx: bool) -> u64 {
    let bytes = match bootstrap_read_file(path) {
        Ok(bytes) => bytes,
        Err(errno) => return linux_errno(errno),
    };
    let inode = crate::vfs_core::path_inode(path.as_bytes());
    if statx {
        write_bootstrap_statx(user_ptr, inode, bytes.len() as u64)
    } else {
        write_bootstrap_stat(user_ptr, inode, bytes.len() as u64)
    }
}

fn bootstrap_read_file(path: &str) -> Result<alloc::vec::Vec<u8>, i64> {
    let path = bootstrap_path(path).ok_or(LINUX_ENOSYS)?;
    crate::storage::boot_volume::read_file_to_vec(path).map_err(|_| LINUX_ENOENT)
}

fn bootstrap_path(path: &str) -> Option<&str> {
    let path = path.strip_prefix('/').unwrap_or(path);
    if path.is_empty() || path.contains("..") {
        return None;
    }
    (path.starts_with("services/")
        || path.starts_with("applications/")
        || path.starts_with("lib/")
        || path.starts_with("lib64/"))
    .then_some(path)
}

fn write_bootstrap_stat(user_ptr: u64, inode: u64, len: u64) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, LINUX_STAT_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut out = [0_u8; LINUX_STAT_SIZE];
    out[8..16].copy_from_slice(&inode.max(1).to_ne_bytes());
    out[16..24].copy_from_slice(&1_u64.to_ne_bytes());
    out[24..28].copy_from_slice(&(0o100000_u32 | 0o555).to_ne_bytes());
    out[48..56].copy_from_slice(&(len as i64).to_ne_bytes());
    out[56..64].copy_from_slice(&4096_i64.to_ne_bytes());
    out[64..72].copy_from_slice(&(len.div_ceil(512) as i64).to_ne_bytes());
    match usermem::write_current_user_bytes(user_ptr, &out) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

fn write_bootstrap_statx(user_ptr: u64, inode: u64, len: u64) -> u64 {
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, LINUX_STATX_SIZE) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut out = [0_u8; LINUX_STATX_SIZE];
    out[0..4].copy_from_slice(&0x17ff_u32.to_ne_bytes());
    out[4..8].copy_from_slice(&4096_u32.to_ne_bytes());
    out[16..20].copy_from_slice(&1_u32.to_ne_bytes());
    out[28..30].copy_from_slice(&(0o100000_u16 | 0o555).to_ne_bytes());
    out[40..48].copy_from_slice(&inode.max(1).to_ne_bytes());
    out[48..56].copy_from_slice(&len.to_ne_bytes());
    out[56..64].copy_from_slice(&len.div_ceil(512).to_ne_bytes());
    match usermem::write_current_user_bytes(user_ptr, &out) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_vfs_getcwd(user_ptr: u64, user_len: u64) -> u64 {
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let request = new_vfs_request(VFS_IPC_OP_GETCWD);
    let response = match call_vfs_ipc_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_vfs_status(&response) {
        return linux_errno(errno);
    }
    let len = response.payload_len as usize;
    if len.checked_add(1).map_or(true, |needed| needed > user_len) {
        return linux_errno(LINUX_ERANGE);
    }
    if let Err(err) = usermem::write_current_user_bytes(user_ptr, &response.payload[..len]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = usermem::write_current_user_bytes(user_ptr + len as u64, &[0]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    (len + 1) as u64
}

pub(super) fn syscall_linux_vfs_chdir(dirfd: u64, path_ptr: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_CHDIR);
    request.dirfd = dirfd;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_mkdir(dirfd: u64, path_ptr: u64, mode: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_MKDIR);
    request.dirfd = dirfd;
    request.arg0 = mode;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_unlinkat(dirfd: u64, path_ptr: u64, flags: u64) -> u64 {
    let path = match copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_UNLINKAT);
    request.dirfd = dirfd;
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_mount(
    _source_ptr: u64,
    target_ptr: u64,
    _fstype_ptr: u64,
    flags: u64,
) -> u64 {
    let path = match copy_current_user_path(target_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_MOUNT);
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_vfs_umount2(target_ptr: u64, flags: u64) -> u64 {
    let path = match copy_current_user_path(target_ptr, VFS_IPC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_vfs_request(VFS_IPC_OP_UMOUNT2);
    request.flags = flags as u32;
    if let Err(errno) = populate_vfs_path(&mut request, &path) {
        return linux_errno(errno);
    }
    match call_vfs_ipc_request(&request).and_then(|response| ensure_vfs_status(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_ioctl(fd: u64, request_number: u64, arg: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_IOCTL);
    request.dirfd = fd;
    request.flags = request_number;
    request.arg1 = arg;
    match call_service_offload_request(IPC_SERVICE_DEVMGRD, &request).and_then(|response| {
        ensure_service_response(&response, SYSCALL_OFFLOAD_OP_LINUX_IOCTL)?;
        ensure_syscalld_payload(&response, size_of::<u64>())?;
        let mut bytes = [0_u8; size_of::<u64>()];
        bytes.copy_from_slice(&response.payload[..size_of::<u64>()]);
        Ok(u64::from_le_bytes(bytes))
    }) {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_net4(op: u16, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    syscall_linux_net6(op, arg0, arg1, arg2, arg3, 0, 0)
}

pub(super) fn syscall_linux_net6(
    op: u16,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> u64 {
    let mut request = new_syscalld_request(op);
    request.dirfd = arg0;
    request.flags = arg1;
    request.arg0 = arg2;
    request.arg1 = arg3;
    request.arg2 = arg4;
    request.arg3 = arg5;
    match call_service_offload_request(IPC_SERVICE_NETD, &request).and_then(|response| {
        ensure_service_response(&response, op)?;
        ensure_syscalld_payload(&response, size_of::<u64>())?;
        let mut bytes = [0_u8; size_of::<u64>()];
        bytes.copy_from_slice(&response.payload[..size_of::<u64>()]);
        Ok(u64::from_le_bytes(bytes))
    }) {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_loader_spawn_exec(
    path_ptr: u64,
    argv_ptr: u64,
    envp_ptr: u64,
    flags: u64,
    console_session: u64,
    weight_micros: u64,
) -> u64 {
    let exec_path = match copy_current_user_path(path_ptr, LOADER_SPAWN_EXEC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = LoaderSpawnRequest {
        flags: flags as u32,
        console_session,
        weight_micros,
        ..LoaderSpawnRequest::default()
    };
    if exec_path.len() > request.exec_path.len() {
        return linux_errno(LINUX_EINVAL);
    }
    request.exec_path_len = exec_path.len() as u32;
    request.exec_path[..exec_path.len()].copy_from_slice(exec_path.as_bytes());
    if let Err(errno) = copy_string_vector(
        argv_ptr,
        LOADER_SPAWN_MAX_ARG_COUNT,
        &mut request.argv_bytes,
        &mut request.argv_bytes_len,
        &mut request.argv_count,
    ) {
        return linux_errno(errno);
    }
    if let Err(errno) = copy_string_vector(
        envp_ptr,
        LOADER_SPAWN_MAX_ENV_COUNT,
        &mut request.env_bytes,
        &mut request.env_bytes_len,
        &mut request.env_count,
    ) {
        return linux_errno(errno);
    }
    // foundation 서비스(syscalld/vfsd/loaderd)는 loaderd를 경유하지 않고 항상 kernel
    // bootstrap path를 사용한다. loaderd가 VFS(vfsd)에 의존하므로, 이 서비스들을 loaderd로
    // respawn 하면 vfsd 크래시 시 의존 사이클이 발생해 복구 불가 상태가 된다.
    if can_bootstrap_spawn_direct(exec_path.as_str()) {
        return match spawn_bootstrap_exec_direct(
            exec_path.as_str(),
            flags,
            console_session,
            weight_micros,
        ) {
            Ok(pid) => pid,
            Err(errno) => linux_errno(errno),
        };
    }

    let response = match call_loaderd(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.version != LOADER_REQUEST_ABI_VERSION
        || response.op != LOADER_OP_SPAWN_EXEC
        || response.reserved0 != 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    response.pid as u64
}

fn can_bootstrap_spawn_direct(exec_path: &str) -> bool {
    let path = exec_path.strip_prefix('/').unwrap_or(exec_path);
    matches!(
        path,
        "services/syscalld/syscalld.elf"
            | "services/vfsd/vfsd.elf"
            | "services/loaderd/loaderd.elf"
    )
}

fn spawn_bootstrap_exec_direct(
    exec_path: &str,
    flags: u64,
    console_session: u64,
    weight_micros: u64,
) -> Result<u64, i64> {
    let current_is_admin =
        multitask::with_current_process_credentials(|security| security.is_logical_admin())
            .unwrap_or(false);
    if !current_is_admin {
        return Err(LINUX_EPERM);
    }

    let loaded = crate::user::console_host::load_executable_image_by_path(exec_path, None)
        .map_err(console_host_error_to_linux_errno)?;
    let session = if console_session == 0 {
        multitask::current_user_snapshot()
            .map(|snapshot| snapshot.console_session())
            .unwrap_or(crate::io::session::ConsoleSessionHandle::SYSTEM)
    } else {
        crate::io::session::ConsoleSessionHandle::from_raw(console_session)
    };
    let logical_admin = flags & 0x1 != 0;
    let program = crate::user::console_host::ConsoleProgramSpec::new(
        &loaded.bytes,
        loaded.path,
        weight_micros,
    )
    .with_logical_admin(logical_admin);
    crate::user::console_host::spawn_program_in_session(session, program)
        .map(|spawned| spawned.pid)
        .map_err(console_host_error_to_linux_errno)
}

fn console_host_error_to_linux_errno(error: crate::user::console_host::ConsoleHostError) -> i64 {
    match error {
        crate::user::console_host::ConsoleHostError::BootstrapBlocked => LINUX_EAGAIN,
        crate::user::console_host::ConsoleHostError::Load { error, .. } => match error {
            crate::vfs::VfsError::BadFileDescriptor => LINUX_EBADF,
            crate::vfs::VfsError::InvalidArgument => LINUX_EINVAL,
            crate::vfs::VfsError::NotFound => LINUX_ENOENT,
            crate::vfs::VfsError::NotDirectory => LINUX_ENOTDIR,
            crate::vfs::VfsError::PermissionDenied => LINUX_EACCES,
            crate::vfs::VfsError::ReadOnlyFilesystem => LINUX_EROFS,
            crate::vfs::VfsError::Unsupported => LINUX_ENOSYS,
        },
        crate::user::console_host::ConsoleHostError::Spawn { .. } => LINUX_ENOEXEC,
    }
}

pub(super) fn copy_string_vector(
    vector_ptr: u64,
    max_count: usize,
    dest: &mut [u8],
    dest_len: &mut u32,
    dest_count: &mut u16,
) -> Result<(), i64> {
    *dest_len = 0;
    *dest_count = 0;
    if vector_ptr == 0 {
        return Ok(());
    }
    let mut cursor = vector_ptr;
    let mut offset = 0usize;
    for count in 0..max_count {
        let mut ptr_bytes = [0_u8; size_of::<u64>()];
        usermem::copy_from_current_user_exact(cursor, &mut ptr_bytes)
            .map_err(address_space_error_to_linux_errno)?;
        let value_ptr = u64::from_ne_bytes(ptr_bytes);
        if value_ptr == 0 {
            *dest_len = offset as u32;
            *dest_count = count as u16;
            return Ok(());
        }
        let value = usermem::read_current_user_c_string(value_ptr, dest.len())
            .map_err(address_space_error_to_linux_errno)?;
        let needed = value.len().checked_add(1).ok_or(LINUX_E2BIG)?;
        if offset.checked_add(needed).ok_or(LINUX_E2BIG)? > dest.len() {
            return Err(LINUX_E2BIG);
        }
        dest[offset..offset + value.len()].copy_from_slice(value.as_bytes());
        offset += value.len();
        dest[offset] = 0;
        offset += 1;
        cursor = cursor
            .checked_add(size_of::<u64>() as u64)
            .ok_or(LINUX_EINVAL)?;
    }
    Err(LINUX_E2BIG)
}

pub(super) fn new_vfs_request(op: u16) -> VfsIpcRequest {
    let mut request = VfsIpcRequest {
        op,
        ..VfsIpcRequest::default()
    };
    populate_vfs_identity(&mut request);
    request
}

pub(super) fn populate_vfs_identity(request: &mut VfsIpcRequest) {
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.session_handle = snapshot.console_session().raw();
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }
}

pub(super) fn populate_vfs_path(request: &mut VfsIpcRequest, path: &str) -> Result<(), i64> {
    let bytes = path.as_bytes();
    if bytes.len() > VFS_IPC_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    request.path_len = bytes.len() as u32;
    request.path[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

pub(super) fn call_vfs_ipc_request(request: &VfsIpcRequest) -> Result<VfsIpcResponse, i64> {
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_VFSD, as_bytes(request))?;
    if response.len() != size_of::<VfsIpcResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<VfsIpcResponse>(response.as_slice());
    if response.version != VFS_IPC_ABI_VERSION
        || response.op != request.op
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    Ok(response)
}

pub(super) fn call_service_offload_request(
    service_id: u64,
    request: &LinuxSyscallOffloadRequest,
) -> Result<LinuxSyscallOffloadResponse, i64> {
    let response = ipc_ops::call_service_endpoint(service_id, as_bytes(request))?;
    if response.len() != size_of::<LinuxSyscallOffloadResponse>() {
        return Err(LINUX_EINVAL);
    }
    Ok(read_unaligned::<LinuxSyscallOffloadResponse>(
        response.as_slice(),
    ))
}

pub(super) fn call_loaderd(request: &LoaderSpawnRequest) -> Result<LoaderSpawnResponse, i64> {
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_LOADERD, as_bytes(request))?;
    if response.len() != size_of::<LoaderSpawnResponse>() {
        return Err(LINUX_EINVAL);
    }
    Ok(read_unaligned::<LoaderSpawnResponse>(response.as_slice()))
}

pub(super) fn ensure_vfs_status(response: &VfsIpcResponse) -> Result<(), i64> {
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(())
}

pub(super) fn ensure_service_response(
    response: &LinuxSyscallOffloadResponse,
    op: u16,
) -> Result<(), i64> {
    if response.version != SYSCALL_OFFLOAD_ABI_VERSION
        || response.op != op
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    ensure_syscalld_status(response)
}

pub(super) fn current_remote_vfs_handle(fd: u64) -> Option<multitask::RemoteVfsHandle> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::RemoteVfs(remote)) => Some(remote.clone()),
            _ => None,
        }
    })
    .flatten()
}

pub(super) fn copy_current_user_path(ptr: u64, capacity: usize) -> Result<String, i64> {
    if ptr == 0 {
        return Err(LINUX_EFAULT);
    }
    let path = usermem::read_current_user_c_string(ptr, capacity)
        .map_err(address_space_error_to_linux_errno)?;
    if path.is_empty() || path.len() > capacity {
        return Err(LINUX_EINVAL);
    }
    Ok(path)
}
