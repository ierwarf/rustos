//! Scheduler-local Linux futex wait, wake, requeue, and owner-death substrate.
//!
//! - **Owner:** Compat owns fixed Linux futex semantics; `kernel-ps` owns task
//!   blocking and process-generation user memory.
//! - **Boundary:** User addresses, futex words, flags, counts, robust lists, and
//!   deadlines are untrusted.
//! - **Lifecycle:** Validate word/key, register exact waiter, recheck, arm,
//!   commit, wake/requeue/timeout/signal, remove, and exit cleanup.
//! - **Concurrency:** Admission avoids allocation, process-state locking, and
//!   policy-service IPC before waiter/deadline publication.
//! - **Failure:** Lost-wake races, timeout, signal, requeue overlap, exec,
//!   task exit, and bounded owner-death traversal converge without stale waiters.
//! - **Forbidden:** No syscalld hot-path call, unbounded robust-list walk,
//!   address-only waiter identity, or removal before wake ownership settles.
//! - **Evidence:** `futex-wait-lifecycle`.
use super::*;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_PROCESS_POLICY, PROCD_OP_THREAD_PLAN,
    SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY,
};
use x86_64::VirtAddr;

const FUTEX_WAITERS_CAPACITY: usize = 256;
const ROBUST_LIST_HEAD_SIZE: u64 = 24;
const ROBUST_LIST_LIMIT: usize = 2_048;
const ROBUST_CMPXCHG_RETRY_LIMIT: usize = 64;
const FUTEX_WAITERS_BIT: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED_BIT: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
const ROBUST_LIST_PI_BIT: u64 = 1;

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

static FUTEX_WAITERS: TrackedSpinLock<
    [Option<FutexWaiter>; FUTEX_WAITERS_CAPACITY],
    { LockClass::FutexWaiter as u8 },
> = TrackedSpinLock::new([None; FUTEX_WAITERS_CAPACITY]);

// Futex opcode/flag admission is part of the scheduler wait/wake substrate.
// Keep it local and allocation-free: putting a synchronous policy-service
// call before waiter/deadline registration can stall every userspace mutex and
// lets an unpark race ahead of the target's waiter installation.
pub fn futex_impl(uaddr: u64, op: u64, val: u64, timeout_ptr: u64, uaddr2: u64, val3: u64) -> u64 {
    if (uaddr & 0x3) != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(errno) = validate_futex_policy_locally(op, val3) {
        return linux_errno(errno);
    }
    let cmd = op & linux_abi::FUTEX_CMD_MASK;
    let result = match cmd {
        c if c == linux_abi::FUTEX_WAIT => futex_wait(
            uaddr,
            val as u32,
            timeout_ptr,
            linux_abi::FUTEX_BITSET_MATCH_ANY,
            op,
        ),
        c if c == linux_abi::FUTEX_WAIT_BITSET => {
            let bitset = val3 as u32;
            futex_wait(uaddr, val as u32, timeout_ptr, bitset, op)
        }
        c if c == linux_abi::FUTEX_WAKE => {
            futex_wake(uaddr, val, linux_abi::FUTEX_BITSET_MATCH_ANY)
        }
        c if c == linux_abi::FUTEX_WAKE_BITSET => {
            let bitset = val3 as u32;
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

fn validate_futex_policy_locally(op: u64, val3: u64) -> Result<(), i64> {
    let cmd = op & linux_abi::FUTEX_CMD_MASK;
    let supported_flags = linux_abi::FUTEX_PRIVATE_FLAG | linux_abi::FUTEX_CLOCK_REALTIME;
    if (op & !linux_abi::FUTEX_CMD_MASK) & !supported_flags != 0 {
        return Err(LINUX_EINVAL);
    }
    match cmd {
        c if c == linux_abi::FUTEX_WAIT
            || c == linux_abi::FUTEX_WAKE
            || c == linux_abi::FUTEX_REQUEUE
            || c == linux_abi::FUTEX_CMP_REQUEUE =>
        {
            Ok(())
        }
        c if c == linux_abi::FUTEX_WAIT_BITSET || c == linux_abi::FUTEX_WAKE_BITSET => {
            if val3 == 0 { Err(LINUX_EINVAL) } else { Ok(()) }
        }
        _ => Err(LINUX_ENOSYS),
    }
}

fn current_process_is_syscalld_policy_owner() -> bool {
    ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY)
}

fn current_process_is_procd_policy_owner() -> bool {
    ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_PROCESS_POLICY)
}

// RING3-MIGRATION-REFERENCE START: scheduler-thread substrate exception: procd
// owns Linux clone/thread admission policy. Ring0 keeps final
// same-address-space thread spawn and register substrate.
pub fn clone_linux_thread(
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
    if flags & REQUIRED_THREAD_FLAGS != REQUIRED_THREAD_FLAGS {
        return procd_fork(
            frame,
            flags,
            child_stack,
            parent_tid_ptr,
            child_tid_ptr,
            tls,
        );
    }
    if current_process_is_procd_policy_owner() {
        if !valid_thread_plan_locally(flags, child_stack, tls) {
            return linux_errno(LINUX_EINVAL);
        }
    } else {
        let mut request = new_procd_request(PROCD_OP_THREAD_PLAN);
        request.arg0 = flags;
        request.arg1 = child_stack;
        request.arg2 = parent_tid_ptr;
        request.arg3 = child_tid_ptr;
        request.arg4 = tls;
        match call_procd(&request).and_then(|response| ensure_empty_procd_response(&response)) {
            Ok(()) => {}
            Err(errno) => return linux_errno(errno),
        }
    }

    let Some(console_session) = multitask::current_console_session() else {
        return linux_errno(LINUX_EINVAL);
    };
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
            if flags & linux_abi::CLONE_CHILD_SETTID != 0 {
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
            child_thread_state.pending_sigchld_events = 0;
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

    let child_tid = match multitask::spawn_user_thread_suspended(bootstrap) {
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
            Some(Err(errno)) => {
                let _ = multitask::terminate_user_task(child_tid);
                return linux_errno(errno);
            }
            None => {
                let _ = multitask::terminate_user_task(child_tid);
                return linux_errno(LINUX_ENOSYS);
            }
        }
    }
    if !multitask::activate_suspended_user_task(child_tid) {
        let _ = multitask::terminate_user_task(child_tid);
        return linux_errno(LINUX_EAGAIN);
    }
    // Publish the runnable child only after every shared-memory clone field is
    // committed. The one-shot handoff is now safe: the child cannot observe a
    // zero/stale TID or race its parent's rollback path.
    multitask::set_next_spawn_pick_hint(child_tid);
    multitask::request_deferred_reschedule();
    child_tid
}
// RING3-MIGRATION-REFERENCE END: procd-owned Linux clone/thread substrate exception.

fn valid_thread_plan_locally(flags: u64, child_stack: u64, tls: u64) -> bool {
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
    let supported_flags = REQUIRED_THREAD_FLAGS | OPTIONAL_THREAD_FLAGS | linux_abi::CSIGNAL;
    flags & REQUIRED_THREAD_FLAGS == REQUIRED_THREAD_FLAGS
        && flags & linux_abi::CSIGNAL == 0
        && flags & !supported_flags == 0
        && child_stack != 0
        && (paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&child_stack)
        && (flags & linux_abi::CLONE_SETTLS == 0
            || tls == 0
            || (paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&tls))
}

fn futex_wait(
    uaddr: u64,
    expected: u32,
    timeout_ptr: u64,
    bitset: u32,
    op: u64,
) -> Result<u64, i64> {
    let deadline_tick = futex_wait_deadline_tick(op, timeout_ptr)?;
    let (task_id, mut key) = current_futex_waiter_context()?;
    key.uaddr = uaddr;
    if !multitask::arm_block_current_task() {
        return Err(LINUX_ENOSYS);
    }
    let actual = match usermem::read_current_user_u32(uaddr) {
        Ok(actual) => actual,
        Err(err) => {
            let _ = multitask::cancel_block_current_task();
            return Err(address_space_error_to_linux_errno(err));
        }
    };
    if actual != expected {
        let _ = multitask::cancel_block_current_task();
        return Err(LINUX_EAGAIN);
    }
    if let Err(errno) = register_futex_waiter(FutexWaiter {
        key,
        task_id,
        bitset,
    }) {
        let _ = multitask::cancel_block_current_task();
        return Err(errno);
    }
    let actual = match usermem::read_current_user_u32(uaddr) {
        Ok(actual) => actual,
        Err(err) => {
            clear_futex_waiter(task_id);
            let _ = multitask::cancel_block_current_task();
            return Err(address_space_error_to_linux_errno(err));
        }
    };
    if actual != expected {
        clear_futex_waiter(task_id);
        let _ = multitask::cancel_block_current_task();
        return Err(LINUX_EAGAIN);
    }
    if deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline) {
        clear_futex_waiter(task_id);
        let _ = multitask::cancel_block_current_task();
        return Err(LINUX_ETIMEDOUT);
    }
    if let Some(deadline) = deadline_tick
        && !crate::arch::rtc::arm_sleep_waiter_until_tick(task_id, deadline)
    {
        clear_futex_waiter(task_id);
        let _ = multitask::cancel_block_current_task();
        return Err(LINUX_EBUSY);
    }
    match multitask::commit_block_current_task_and_yield() {
        Some(true) => {}
        Some(false) => {}
        None => {
            clear_futex_waiter(task_id);
            crate::arch::rtc::disarm_sleep_waiter(task_id);
            return Err(LINUX_EINVAL);
        }
    }
    // REQUEUE may have changed the key while this task slept. Cleanup is tied
    // to task identity, not the key captured before blocking, so timeout and
    // spurious-wake paths cannot strand a waiter in the bounded table. Keep
    // the deadline notification owned until this cleanup completes: removing
    // timer authority before the waiter-table transaction would make a stall
    // in the resumed kernel path indistinguishable from a completed wait.
    let still_waiting = take_futex_waiter(task_id);
    let timed_out = still_waiting
        && deadline_tick.is_some_and(|deadline| crate::arch::rtc::ticks() >= deadline);
    crate::arch::rtc::disarm_sleep_waiter(task_id);
    if timed_out {
        Err(LINUX_ETIMEDOUT)
    } else {
        // Linux permits spurious futex wakeups. A waiter removed by
        // FUTEX_WAKE and a task woken for an unrelated reason both return
        // success; userspace must re-check its atomic predicate.
        Ok(0)
    }
}

fn futex_wait_deadline_tick(op: u64, timeout_ptr: u64) -> Result<Option<u64>, i64> {
    if timeout_ptr == 0 {
        return Ok(None);
    }
    let timeout = usermem::read_current_user_struct::<LinuxTimespecWire>(timeout_ptr)
        .map_err(address_space_error_to_linux_errno)?;
    validate_futex_timespec(timeout)?;

    let now_tick = crate::arch::rtc::ticks();
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let timeout_ticks = if op & linux_abi::FUTEX_CMD_MASK == linux_abi::FUTEX_WAIT_BITSET {
        let clock_id = if op & linux_abi::FUTEX_CLOCK_REALTIME != 0 {
            linux_abi::CLOCK_REALTIME
        } else {
            linux_abi::CLOCK_MONOTONIC
        };
        let now = process_time::current_clock_timespec_substrate(clock_id);
        timespec_delta_ticks(timeout, now, ticks_per_second)
    } else {
        timespec_duration_ticks(timeout, ticks_per_second)
    };
    Ok(Some(now_tick.saturating_add(timeout_ticks)))
}

fn validate_futex_timespec(timespec: LinuxTimespecWire) -> Result<(), i64> {
    if timespec.tv_sec < 0 || !(0..1_000_000_000).contains(&timespec.tv_nsec) {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

fn timespec_duration_ticks(timespec: LinuxTimespecWire, ticks_per_second: u64) -> u64 {
    let nanos = (timespec.tv_sec as u128)
        .saturating_mul(1_000_000_000)
        .saturating_add(timespec.tv_nsec as u128);
    nanos
        .saturating_mul(ticks_per_second as u128)
        .saturating_add(999_999_999)
        .saturating_div(1_000_000_000)
        .min(u64::MAX as u128) as u64
}

fn timespec_delta_ticks(
    deadline: LinuxTimespecWire,
    now: LinuxTimespecWire,
    ticks_per_second: u64,
) -> u64 {
    if deadline.tv_sec < now.tv_sec
        || deadline.tv_sec == now.tv_sec && deadline.tv_nsec <= now.tv_nsec
    {
        return 0;
    }
    let mut seconds = deadline.tv_sec.saturating_sub(now.tv_sec);
    let nanos = if deadline.tv_nsec >= now.tv_nsec {
        deadline.tv_nsec - now.tv_nsec
    } else {
        seconds = seconds.saturating_sub(1);
        deadline.tv_nsec + 1_000_000_000 - now.tv_nsec
    };
    timespec_duration_ticks(
        LinuxTimespecWire {
            tv_sec: seconds,
            tv_nsec: nanos,
        },
        ticks_per_second,
    )
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
    let Some((task_id, abi, address_space_root)) = multitask::current_user_wait_binding() else {
        return Err(LINUX_ENOSYS);
    };
    if abi != crate::user::abi::UserAbi::Linux {
        return Err(LINUX_ENOSYS);
    }
    Ok((
        task_id,
        FutexKey {
            address_space_root,
            uaddr: 0,
        },
    ))
}

fn register_futex_waiter(waiter: FutexWaiter) -> Result<(), i64> {
    let mut waiters = FUTEX_WAITERS.lock();
    let mut free_slot = None;
    for slot in 0..waiters.len() {
        match waiters[slot] {
            Some(existing) if existing.task_id == waiter.task_id => {
                // A task can own at most one scheduler wait. Treat a second
                // registration as an invariant violation rather than hiding
                // the older entry and consuming another bounded slot.
                return Err(LINUX_EBUSY);
            }
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

fn clear_futex_waiter(task_id: u64) {
    let _ = take_futex_waiter(task_id);
}

pub(crate) fn cleanup_retired_task_waiter(task_id: u64) -> bool {
    take_futex_waiter(task_id)
}

fn take_futex_waiter(task_id: u64) -> bool {
    let mut waiters = FUTEX_WAITERS.lock();
    take_futex_waiter_from(&mut waiters[..], task_id)
}

fn take_futex_waiter_from(waiters: &mut [Option<FutexWaiter>], task_id: u64) -> bool {
    let mut removed = false;
    for slot in waiters.iter_mut() {
        if slot
            .map(|waiter| waiter.task_id == task_id)
            .unwrap_or(false)
        {
            *slot = None;
            removed = true;
        }
    }
    removed
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
        } else {
            crate::debug::println!("futex: selected waiter had no live task task={task_id}");
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

fn robust_owner_death_value(value: u32, task_id: u64) -> Option<(u32, bool)> {
    let task_tid = u32::try_from(task_id).ok()? & FUTEX_TID_MASK;
    if value & FUTEX_TID_MASK != task_tid {
        return None;
    }
    Some((
        (value & FUTEX_WAITERS_BIT) | FUTEX_OWNER_DIED_BIT,
        value & FUTEX_WAITERS_BIT != 0,
    ))
}

fn robust_futex_address(entry: u64, offset: i64) -> Option<u64> {
    let address = if offset >= 0 {
        entry.checked_add(offset as u64)?
    } else {
        entry.checked_sub(offset.unsigned_abs())?
    };
    let end = address.checked_add(core::mem::size_of::<u32>() as u64)?;
    ((address & 0x3) == 0
        && (paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&address)
        && end <= paging::USER_SPACE_END_EXCLUSIVE)
        .then_some(address)
}

fn read_user_u64(address_space: &paging::ProcessAddressSpace, address: u64) -> Option<u64> {
    let mut bytes = [0_u8; 8];
    address_space
        .copy_from_user(VirtAddr::new(address), &mut bytes)
        .ok()?;
    Some(u64::from_le_bytes(bytes))
}

fn cleanup_robust_futex(
    address_space: &paging::ProcessAddressSpace,
    address_space_root: u64,
    task_id: u64,
    entry: u64,
    futex_offset: i64,
) -> bool {
    let Some(futex_address) = robust_futex_address(entry, futex_offset) else {
        return false;
    };
    let Ok(mut value) = address_space.atomic_load_user_u32(futex_address) else {
        return false;
    };
    for _ in 0..ROBUST_CMPXCHG_RETRY_LIMIT {
        let Some((owner_died, had_waiters)) = robust_owner_death_value(value, task_id) else {
            return false;
        };
        match address_space.atomic_compare_exchange_user_u32(futex_address, value, owner_died) {
            Ok(Ok(_)) => {
                if had_waiters {
                    let _ = wake_futex_waiters(
                        FutexKey {
                            address_space_root,
                            uaddr: futex_address,
                        },
                        1,
                        linux_abi::FUTEX_BITSET_MATCH_ANY,
                    );
                }
                return true;
            }
            Ok(Err(observed)) => value = observed,
            Err(_) => return false,
        }
    }
    false
}

fn cleanup_robust_list(
    address_space: &paging::ProcessAddressSpace,
    address_space_root: u64,
    task_id: u64,
    robust_list_head: u64,
    robust_list_len: u64,
) -> usize {
    if robust_list_head == 0 || robust_list_len != ROBUST_LIST_HEAD_SIZE {
        return 0;
    }
    let Some(mut entry) = read_user_u64(address_space, robust_list_head) else {
        return 0;
    };
    let Some(futex_offset) =
        read_user_u64(address_space, robust_list_head.saturating_add(8)).map(|value| value as i64)
    else {
        return 0;
    };
    let Some(pending) = read_user_u64(address_space, robust_list_head.saturating_add(16)) else {
        return 0;
    };
    let pending_entry = pending & !ROBUST_LIST_PI_BIT;
    let mut cleaned = 0usize;

    for _ in 0..ROBUST_LIST_LIMIT {
        let current_entry = entry & !ROBUST_LIST_PI_BIT;
        if current_entry == robust_list_head {
            break;
        }
        if current_entry == 0 || entry & ROBUST_LIST_PI_BIT != 0 {
            return cleaned;
        }
        let Some(next) = read_user_u64(address_space, current_entry) else {
            return cleaned;
        };
        if current_entry != pending_entry
            && cleanup_robust_futex(
                address_space,
                address_space_root,
                task_id,
                current_entry,
                futex_offset,
            )
        {
            cleaned += 1;
        }
        entry = next;
    }

    if pending_entry != 0
        && pending & ROBUST_LIST_PI_BIT == 0
        && cleanup_robust_futex(
            address_space,
            address_space_root,
            task_id,
            pending_entry,
            futex_offset,
        )
    {
        cleaned += 1;
    }
    cleaned
}

pub(crate) fn cleanup_retired_linux_thread_state(
    task_id: u64,
    process_id: u64,
    clear_child_tid: u64,
    robust_list_head: u64,
    robust_list_len: u64,
) -> usize {
    if process_id == 0 {
        return 0;
    }
    multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        let address_space_root = process_state.address_space_root();
        let address_space = process_state.address_space();
        let mut cleaned = cleanup_robust_list(
            address_space,
            address_space_root,
            task_id,
            robust_list_head,
            robust_list_len,
        );
        if clear_child_tid != 0
            && (clear_child_tid & 0x3) == 0
            && address_space
                .atomic_store_user_u32_release(clear_child_tid, 0)
                .is_ok()
        {
            let _ = wake_futex_waiters(
                FutexKey {
                    address_space_root,
                    uaddr: clear_child_tid,
                },
                1,
                linux_abi::FUTEX_BITSET_MATCH_ANY,
            );
            cleaned += 1;
        }
        cleaned
    })
    .unwrap_or(0)
}

pub fn cleanup_linux_thread_exit() {
    if let Some(task_id) = multitask::current_user_thread_id() {
        clear_futex_waiter(task_id);
        crate::arch::rtc::disarm_sleep_waiter(task_id);
    }
    let cleanup = multitask::with_current_user_linux_state_mut(
        |process_id, task_id, abi, _, _, linux_thread_state| {
            if abi != crate::user::abi::UserAbi::Linux {
                return None;
            }
            let state = linux_thread_state.as_mut()?;
            let cleanup = (
                process_id,
                task_id,
                state.clear_child_tid,
                state.robust_list_head,
                state.robust_list_len,
            );
            state.clear_child_tid = 0;
            state.robust_list_head = 0;
            state.robust_list_len = 0;
            Some(cleanup)
        },
    )
    .flatten();
    if let Some((process_id, task_id, clear_child_tid, robust_list_head, robust_list_len)) = cleanup
    {
        let _ = cleanup_retired_linux_thread_state(
            task_id,
            process_id,
            clear_child_tid,
            robust_list_head,
            robust_list_len,
        );
    }
}

// RING3-MIGRATION-REFERENCE START: scheduler-thread substrate exception:
// syscalld/procd own Linux thread metadata admission policy. Ring0 keeps
// FS-base mutation and clear_child_tid substrate.
pub fn syscall_linux_arch_prctl(_code: u64, _arg: u64) -> u64 {
    if current_process_is_syscalld_policy_owner() {
        if !valid_arch_prctl_locally(_code, _arg) {
            return linux_errno(LINUX_EINVAL);
        }
    } else {
        let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY);
        request.arg0 = _code;
        request.arg1 = _arg;
        match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response))
        {
            Ok(()) => {}
            Err(errno) => return linux_errno(errno),
        }
    }
    match _code {
        linux_abi::ARCH_SET_FS => {
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

pub fn syscall_linux_set_tid_address(user_ptr: u64) -> u64 {
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
// RING3-MIGRATION-REFERENCE END: syscalld/procd-owned Linux thread metadata substrate exception.

fn valid_arch_prctl_locally(code: u64, arg: u64) -> bool {
    match code {
        linux_abi::ARCH_SET_FS => {
            arg == 0 || (paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&arg)
        }
        linux_abi::ARCH_GET_FS => true,
        _ => false,
    }
}

pub fn syscall_linux_kill(pid: u64, signal: u64) -> u64 {
    let pid_i64 = pid as i64;
    if pid_i64 == 0 || pid_i64 < -1 {
        return linux_errno(LINUX_ENOSYS);
    }
    let target_pid = if pid_i64 == -1 {
        match multitask::current_user_process_id() {
            Some(id) => id,
            None => return linux_errno(LINUX_ENOSYS),
        }
    } else {
        pid
    };
    procd_tgkill(target_pid, target_pid, signal)
}

pub fn syscall_linux_tkill(tid: u64, signal: u64) -> u64 {
    let pid = match multitask::current_user_process_id() {
        Some(id) => id,
        None => return linux_errno(LINUX_ENOSYS),
    };
    procd_tgkill(pid, tid, signal)
}

pub fn syscall_linux_tgkill(tgid: u64, tid: u64, signal: u64) -> u64 {
    procd_tgkill(tgid, tid, signal)
}

fn procd_tgkill(tgid: u64, tid: u64, signal: u64) -> u64 {
    let mut request = new_procd_request(rustos_user_abi::syscall::PROCD_OP_TGKILL);
    request.arg0 = tgid;
    request.arg1 = tid;
    request.arg2 = signal;
    match call_procd(&request).and_then(|response| ensure_empty_procd_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_set_robust_list(head_ptr: u64, len: u64) -> u64 {
    if len != ROBUST_LIST_HEAD_SIZE {
        return linux_errno(LINUX_EINVAL);
    }
    if head_ptr != 0
        && (!(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&head_ptr)
            || head_ptr
                .checked_add(ROBUST_LIST_HEAD_SIZE)
                .is_none_or(|end| end > paging::USER_SPACE_END_EXCLUSIVE))
    {
        return linux_errno(LINUX_EFAULT);
    }
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST);
    request.arg0 = head_ptr;
    request.arg1 = len;
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => {
            let updated =
                multitask::with_current_user_linux_state_mut(|_, _, abi, _, _, thread_state| {
                    if abi != crate::user::abi::UserAbi::Linux {
                        return false;
                    }
                    let Some(state) = thread_state.as_mut() else {
                        return false;
                    };
                    state.robust_list_head = head_ptr;
                    state.robust_list_len = len;
                    true
                })
                .unwrap_or(false);
            if updated {
                0
            } else {
                linux_errno(LINUX_ENOSYS)
            }
        }
        Err(errno) => linux_errno(errno),
    }
}

pub fn syscall_linux_get_robust_list(pid: u64, head_ptr_ptr: u64, len_ptr: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST);
    request.arg0 = pid;
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(errno) = ensure_syscalld_payload(&response, 16) {
        return linux_errno(errno);
    }
    let Some(current_process_id) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_ENOSYS);
    };
    let Some(current_task_id) = multitask::current_user_thread_id() else {
        return linux_errno(LINUX_ENOSYS);
    };
    let target_task_id = if pid == 0 { current_task_id } else { pid };
    let Some(snapshot) =
        multitask::linux_thread_snapshot_by_ids(current_process_id, target_task_id)
    else {
        return linux_errno(LINUX_ESRCH);
    };
    let head = snapshot.thread_state.robust_list_head.to_le_bytes();
    let len = snapshot.thread_state.robust_list_len.to_le_bytes();
    if let Err(err) = usermem::write_current_user_bytes(head_ptr_ptr, &head) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = usermem::write_current_user_bytes(len_ptr, &len) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    0
}

pub fn syscall_linux_rseq(area_ptr: u64, len: u64, flags: u64, signature: u64) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_identity_cleanup_removes_a_requeued_waiter() {
        let original = FutexKey {
            address_space_root: 0x1000,
            uaddr: 0x2000,
        };
        let requeued = FutexKey {
            uaddr: 0x3000,
            ..original
        };
        let mut waiters = [
            Some(FutexWaiter {
                key: requeued,
                task_id: 7,
                bitset: linux_abi::FUTEX_BITSET_MATCH_ANY,
            }),
            Some(FutexWaiter {
                key: original,
                task_id: 8,
                bitset: linux_abi::FUTEX_BITSET_MATCH_ANY,
            }),
        ];

        assert!(take_futex_waiter_from(&mut waiters, 7));
        assert!(waiters[0].is_none());
        assert_eq!(waiters[1].unwrap().task_id, 8);
    }

    #[test]
    fn supported_futex_admission_is_local_and_complete() {
        for op in [
            linux_abi::FUTEX_WAIT,
            linux_abi::FUTEX_WAKE,
            linux_abi::FUTEX_REQUEUE,
            linux_abi::FUTEX_CMP_REQUEUE,
            linux_abi::FUTEX_WAIT | linux_abi::FUTEX_PRIVATE_FLAG,
            linux_abi::FUTEX_WAIT_BITSET | linux_abi::FUTEX_CLOCK_REALTIME,
            linux_abi::FUTEX_WAKE_BITSET | linux_abi::FUTEX_PRIVATE_FLAG,
        ] {
            assert_eq!(
                validate_futex_policy_locally(op, linux_abi::FUTEX_BITSET_MATCH_ANY as u64),
                Ok(())
            );
        }
        assert_eq!(
            validate_futex_policy_locally(linux_abi::FUTEX_WAIT_BITSET, 0),
            Err(LINUX_EINVAL)
        );
        assert_eq!(
            validate_futex_policy_locally(u64::MAX, linux_abi::FUTEX_BITSET_MATCH_ANY as u64),
            Err(LINUX_ENOSYS)
        );
    }

    #[test]
    fn retired_task_cleanup_is_exact_and_idempotent() {
        let task_id = u64::MAX - 501;
        register_futex_waiter(FutexWaiter {
            key: FutexKey {
                address_space_root: 0x4000,
                uaddr: 0x5000,
            },
            task_id,
            bitset: linux_abi::FUTEX_BITSET_MATCH_ANY,
        })
        .expect("register retired-task waiter");

        assert!(cleanup_retired_task_waiter(task_id));
        assert!(!cleanup_retired_task_waiter(task_id));
    }

    #[test]
    fn robust_owner_death_preserves_waiters_and_rejects_foreign_owner() {
        let task_id = 77_u64;
        assert_eq!(
            robust_owner_death_value(FUTEX_WAITERS_BIT | task_id as u32, task_id),
            Some((FUTEX_WAITERS_BIT | FUTEX_OWNER_DIED_BIT, true))
        );
        assert_eq!(
            robust_owner_death_value(task_id as u32, task_id),
            Some((FUTEX_OWNER_DIED_BIT, false))
        );
        assert_eq!(robust_owner_death_value(78, task_id), None);
    }

    #[test]
    fn robust_futex_offset_is_checked_before_user_access() {
        let entry = paging::USER_SPACE_BASE + 0x100;
        assert_eq!(robust_futex_address(entry, 16), Some(entry + 16));
        assert_eq!(robust_futex_address(entry, -16), Some(entry - 16));
        assert_eq!(robust_futex_address(entry, 2), None);
        assert_eq!(robust_futex_address(u64::MAX - 3, 8), None);
        assert_eq!(robust_futex_address(paging::USER_SPACE_BASE, -4), None);
    }
}
