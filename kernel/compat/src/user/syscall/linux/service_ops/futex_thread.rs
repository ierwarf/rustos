use super::*;
use lazy_static::lazy_static;
use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_PROCESS_POLICY, PROCD_OP_THREAD_PLAN,
    SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY, SYSCALL_OFFLOAD_OP_LINUX_FUTEX_POLICY,
};
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

// RING3-MIGRATION-REFERENCE START: scheduler-thread substrate exception:
// syscalld owns Linux futex opcode admission policy. Ring0 keeps the scheduler
// wait/wake queue substrate.
pub fn futex_impl(uaddr: u64, op: u64, val: u64, timeout_ptr: u64, uaddr2: u64, val3: u64) -> u64 {
    if (uaddr & 0x3) != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if current_process_is_syscalld_policy_owner() {
        if let Err(errno) = validate_futex_policy_locally(op, val3) {
            return linux_errno(errno);
        }
    } else {
        let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_FUTEX_POLICY);
        request.arg0 = op;
        request.arg1 = val3;
        match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response))
        {
            Ok(()) => {}
            Err(errno) => return linux_errno(errno),
        }
    }
    let cmd = op & linux_abi::FUTEX_CMD_MASK;
    let result = match cmd {
        c if c == linux_abi::FUTEX_WAIT => futex_wait(
            uaddr,
            val as u32,
            timeout_ptr,
            linux_abi::FUTEX_BITSET_MATCH_ANY,
        ),
        c if c == linux_abi::FUTEX_WAIT_BITSET => {
            let bitset = val3 as u32;
            futex_wait(uaddr, val as u32, timeout_ptr, bitset)
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
// RING3-MIGRATION-REFERENCE END: syscalld-owned futex opcode substrate exception.

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
    const OPTIONAL_THREAD_FLAGS: u64 = linux_abi::CLONE_SYSVSEM
        | linux_abi::CLONE_SETTLS
        | linux_abi::CLONE_PARENT_SETTID
        | linux_abi::CLONE_CHILD_CLEARTID
        | linux_abi::CLONE_CHILD_SETTID;

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

fn futex_wait(uaddr: u64, expected: u32, timeout_ptr: u64, bitset: u32) -> Result<u64, i64> {
    if timeout_ptr != 0 {
        return Err(LINUX_ENOSYS);
    }
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
            clear_futex_waiter(task_id, key);
            let _ = multitask::cancel_block_current_task();
            return Err(address_space_error_to_linux_errno(err));
        }
    };
    if actual != expected {
        clear_futex_waiter(task_id, key);
        let _ = multitask::cancel_block_current_task();
        return Err(LINUX_EAGAIN);
    }
    match multitask::commit_block_current_task() {
        Some(true) => multitask::yield_now(),
        Some(false) => {}
        None => {
            clear_futex_waiter(task_id, key);
            return Err(LINUX_EINVAL);
        }
    }
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

pub fn cleanup_linux_thread_exit() {
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
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST);
    request.arg0 = head_ptr;
    request.arg1 = len;
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
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
    if let Err(err) = usermem::write_current_user_bytes(head_ptr_ptr, &response.payload[..8]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = usermem::write_current_user_bytes(len_ptr, &response.payload[8..16]) {
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
