use super::*;
use core::sync::atomic::{AtomicUsize, Ordering};

const SECONDARY_FUTEX_DEBUG_LIMIT: usize = 128;

static SECONDARY_FUTEX_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);

fn debug_log_secondary_futex(message: impl FnOnce() -> alloc::string::String) {
    if multitask::current_console_session().is_system() {
        return;
    }

    if SECONDARY_FUTEX_DEBUG_LOGS.fetch_add(1, Ordering::Relaxed) >= SECONDARY_FUTEX_DEBUG_LIMIT {
        return;
    }

    let pid = multitask::current_user_id().unwrap_or(0);
    let session = multitask::current_console_session();
    debug::println!(
        "secondary futex: pid={} session={} {}",
        pid,
        session.raw(),
        message(),
    );
}

pub(crate) fn futex(
    uaddr: u64,
    op: u64,
    val: u64,
    timeout_ptr: u64,
    _uaddr2: u64,
    val3: u64,
) -> Result<u64, LinuxSysopError> {
    if (uaddr & 0x3) != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let cmd = op & linux_abi::FUTEX_CMD_MASK;
    let supported_flags = linux_abi::FUTEX_PRIVATE_FLAG | linux_abi::FUTEX_CLOCK_REALTIME;
    if (op & !linux_abi::FUTEX_CMD_MASK) & !supported_flags != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    match cmd {
        linux_abi::FUTEX_WAIT => futex_wait(
            uaddr,
            val as u32,
            timeout_ptr,
            linux_abi::FUTEX_BITSET_MATCH_ANY,
        ),
        linux_abi::FUTEX_WAIT_BITSET => {
            let bitset = u32::try_from(val3).map_err(|_| LinuxSysopError::InvalidArgument)?;
            if bitset == 0 {
                return Err(LinuxSysopError::InvalidArgument);
            }
            futex_wait(uaddr, val as u32, timeout_ptr, bitset)
        }
        linux_abi::FUTEX_WAKE => futex_wake(uaddr, val, linux_abi::FUTEX_BITSET_MATCH_ANY),
        linux_abi::FUTEX_WAKE_BITSET => {
            let bitset = u32::try_from(val3).map_err(|_| LinuxSysopError::InvalidArgument)?;
            if bitset == 0 {
                return Err(LinuxSysopError::InvalidArgument);
            }
            futex_wake(uaddr, val, bitset)
        }
        _ => Err(LinuxSysopError::Unsupported),
    }
}

pub(crate) fn set_robust_list(head_ptr: u64, len: u64) -> Result<(), LinuxSysopError> {
    if head_ptr != 0 {
        let head_len = usize::try_from(len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        let Some(result) = multitask::with_current_user_linux_state_mut(
            |_, _, abi, address_space, _, linux_thread_state| {
                if abi != UserAbi::Linux {
                    return Err(LinuxSysopError::Unsupported);
                }
                let Some(state) = linux_thread_state.as_mut() else {
                    return Err(LinuxSysopError::Unsupported);
                };
                if head_len != 0 {
                    address_space
                        .validate_user_read_buffer(VirtAddr::new(head_ptr), head_len)
                        .map_err(LinuxSysopError::AddressSpace)?;
                }
                state.robust_list_head = head_ptr;
                state.robust_list_len = len;
                Ok(())
            },
        ) else {
            return Err(LinuxSysopError::Unsupported);
        };

        return result;
    }

    let Some(result) =
        multitask::with_current_user_linux_state_mut(|_, _, abi, _, _, linux_thread_state| {
            if abi != UserAbi::Linux {
                return Err(LinuxSysopError::Unsupported);
            }
            let Some(state) = linux_thread_state.as_mut() else {
                return Err(LinuxSysopError::Unsupported);
            };
            state.robust_list_head = 0;
            state.robust_list_len = len;
            Ok(())
        })
    else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn rseq(
    area_ptr: u64,
    len: u64,
    flags: u64,
    signature: u64,
) -> Result<(), LinuxSysopError> {
    if flags & !RSEQ_FLAG_UNREGISTER != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    let signature = u32::try_from(signature).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let len_u32 = u32::try_from(len).map_err(|_| LinuxSysopError::InvalidArgument)?;

    let Some(result) = multitask::with_current_user_linux_state_mut(
        |_, _, abi, address_space, _, linux_thread_state| {
            if abi != UserAbi::Linux {
                return Err(LinuxSysopError::Unsupported);
            }
            let Some(state) = linux_thread_state.as_mut() else {
                return Err(LinuxSysopError::Unsupported);
            };

            if flags & RSEQ_FLAG_UNREGISTER != 0 {
                state.rseq_area = 0;
                state.rseq_len = 0;
                state.rseq_signature = 0;
                return Ok(());
            }

            let area_len = usize::try_from(len).map_err(|_| LinuxSysopError::InvalidArgument)?;
            if area_len != 0 {
                address_space
                    .validate_user_write_buffer(VirtAddr::new(area_ptr), area_len)
                    .map_err(LinuxSysopError::AddressSpace)?;
            }
            state.rseq_area = area_ptr;
            state.rseq_len = len_u32;
            state.rseq_signature = signature;
            Ok(())
        },
    ) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn arch_prctl(code: u64, arg: u64) -> Result<u64, LinuxSysopError> {
    match code {
        linux_abi::ARCH_SET_FS => {
            if arg != 0
                && !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&arg)
            {
                return Err(LinuxSysopError::InvalidArgument);
            }

            let Some(result) = multitask::with_current_user_linux_state_mut(
                |_, _, abi, _, _, linux_thread_state| {
                    if abi != UserAbi::Linux {
                        return Err(LinuxSysopError::Unsupported);
                    }

                    let Some(state) = linux_thread_state.as_mut() else {
                        return Err(LinuxSysopError::Unsupported);
                    };
                    state.fs_base = arg;
                    FsBase::write(VirtAddr::new(arg));
                    Ok(0)
                },
            ) else {
                return Err(LinuxSysopError::Unsupported);
            };

            result
        }
        linux_abi::ARCH_GET_FS => {
            usermem::write_current_user_bytes(arg, &FsBase::read().as_u64().to_le_bytes())?;
            Ok(0)
        }
        _ => Err(LinuxSysopError::InvalidArgument),
    }
}

pub(crate) fn set_tid_address(user_ptr: u64) -> Result<u64, LinuxSysopError> {
    let Some(result) =
        multitask::with_current_user_linux_state_mut(|_, tid, abi, _, _, linux_thread_state| {
            if abi != UserAbi::Linux {
                return Err(LinuxSysopError::Unsupported);
            }

            let Some(state) = linux_thread_state.as_mut() else {
                return Err(LinuxSysopError::Unsupported);
            };
            state.clear_child_tid = user_ptr;
            Ok(tid)
        })
    else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn clone(
    frame: LinuxCloneFrame,
    flags: u64,
    child_stack: u64,
    parent_tid_ptr: u64,
    child_tid_ptr: u64,
    tls: u64,
) -> Result<u64, LinuxSysopError> {
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
        return Err(LinuxSysopError::InvalidArgument);
    }
    if flags & REQUIRED_THREAD_FLAGS != REQUIRED_THREAD_FLAGS {
        return Err(LinuxSysopError::Unsupported);
    }
    if child_stack == 0
        || !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&child_stack)
    {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if flags & linux_abi::CLONE_SETTLS != 0
        && tls != 0
        && !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&tls)
    {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let console_session = multitask::current_console_session();
    let Some(child_thread_state) = multitask::with_current_user_linux_state_mut(
        |_, _, abi, address_space, _, linux_thread_state| {
            if abi != UserAbi::Linux {
                return Err(LinuxSysopError::Unsupported);
            }

            let Some(parent_thread_state) = linux_thread_state.as_ref() else {
                return Err(LinuxSysopError::Unsupported);
            };

            if flags & linux_abi::CLONE_PARENT_SETTID != 0 {
                address_space
                    .validate_user_write_buffer(VirtAddr::new(parent_tid_ptr), size_of::<u32>())
                    .map_err(LinuxSysopError::AddressSpace)?;
            }
            if flags & (linux_abi::CLONE_CHILD_SETTID | linux_abi::CLONE_CHILD_CLEARTID) != 0 {
                address_space
                    .validate_user_write_buffer(VirtAddr::new(child_tid_ptr), size_of::<u32>())
                    .map_err(LinuxSysopError::AddressSpace)?;
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
            child_thread_state.signal_stack = disabled_signal_stack();
            Ok(child_thread_state)
        },
    ) else {
        return Err(LinuxSysopError::Unsupported);
    };
    let child_thread_state = child_thread_state?;

    let mut bootstrap = multitask::UserTaskBootstrap::new(
        UserAbi::Linux,
        VirtAddr::new(frame.user_rip),
        VirtAddr::new(child_stack),
    );
    bootstrap.console_session = console_session;
    bootstrap.linux_thread_state = Some(child_thread_state);
    bootstrap.registers = frame.registers;
    bootstrap.registers.rax = 0;
    bootstrap.registers.rcx = frame.user_rip;
    bootstrap.registers.r11 = frame.user_rflags;

    let child_tid =
        multitask::spawn_user_thread(bootstrap, multitask::DEFAULT_USER_TASK_WEIGHT_MICROS)
            .map_err(|err| match err {
                multitask::SpawnTaskError::InvalidWeightMicros => LinuxSysopError::InvalidArgument,
                multitask::SpawnTaskError::NoFreeTaskSlot => LinuxSysopError::TryAgain,
            })?;
    let child_tid_bytes = (child_tid as u32).to_le_bytes();

    if flags & (linux_abi::CLONE_PARENT_SETTID | linux_abi::CLONE_CHILD_SETTID) != 0 {
        let Some(result) =
            multitask::with_current_user_process_state_mut(|_, abi, process_state| {
                if abi != UserAbi::Linux {
                    return Err(LinuxSysopError::Unsupported);
                }
                let address_space = process_state.address_space();
                if flags & linux_abi::CLONE_PARENT_SETTID != 0 {
                    address_space
                        .copy_into_user(VirtAddr::new(parent_tid_ptr), &child_tid_bytes)
                        .map_err(LinuxSysopError::AddressSpace)?;
                }
                if flags & linux_abi::CLONE_CHILD_SETTID != 0 {
                    address_space
                        .copy_into_user(VirtAddr::new(child_tid_ptr), &child_tid_bytes)
                        .map_err(LinuxSysopError::AddressSpace)?;
                }
                Ok(())
            })
        else {
            return Err(LinuxSysopError::Unsupported);
        };
        result?;
    }

    Ok(child_tid)
}

pub(crate) fn clone3(
    frame: LinuxCloneFrame,
    args_ptr: u64,
    args_size: u64,
) -> Result<u64, LinuxSysopError> {
    let expected_size = size_of::<linux_abi::LinuxCloneArgs>();
    let provided_size = usize::try_from(args_size).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if provided_size == 0 || provided_size > expected_size {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let mut args = linux_abi::LinuxCloneArgs::default();
    let args_bytes = unsafe {
        slice::from_raw_parts_mut(core::ptr::addr_of_mut!(args).cast::<u8>(), provided_size)
    };
    usermem::copy_from_current_user_exact(args_ptr, args_bytes)?;

    let requests_pidfd = args.flags & linux_abi::CLONE_PIDFD != 0;
    let requests_set_tid = args.set_tid != 0 || args.set_tid_size != 0;
    let requests_cgroup = args.flags & linux_abi::CLONE_INTO_CGROUP != 0;
    if requests_pidfd || requests_set_tid || requests_cgroup {
        debug::println!(
            "linux clone3 unsupported fields: flags={:#x} pidfd={:#x} child_tid={:#x} parent_tid={:#x} exit_signal={:#x} stack={:#x} stack_size={:#x} tls={:#x} set_tid={:#x} set_tid_size={} cgroup={:#x}",
            args.flags,
            args.pidfd,
            args.child_tid,
            args.parent_tid,
            args.exit_signal,
            args.stack,
            args.stack_size,
            args.tls,
            args.set_tid,
            args.set_tid_size,
            args.cgroup,
        );
        return Err(LinuxSysopError::Unsupported);
    }
    if args.flags & linux_abi::CLONE_PIDFD == 0 {
        args.pidfd = 0;
    }
    if args.flags & linux_abi::CLONE_INTO_CGROUP == 0 {
        args.cgroup = 0;
    }
    if args.exit_signal & !linux_abi::CSIGNAL != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let child_stack = if args.stack == 0 && args.stack_size == 0 {
        0
    } else {
        args.stack
            .checked_add(args.stack_size)
            .ok_or(LinuxSysopError::InvalidArgument)?
    };
    let combined_flags = args.flags | (args.exit_signal & linux_abi::CSIGNAL);

    clone(
        frame,
        combined_flags,
        child_stack,
        args.parent_tid,
        args.child_tid,
        args.tls,
    )
}

pub(crate) fn exit_current_process(status: u64) -> ! {
    if status != 0 {
        debug::println!("user process exited with status {}", status);
    }
    let clear_child_tid = multitask::with_current_user_linux_state_mut(
        |_, _, abi, address_space, _, linux_thread_state| {
            if abi != UserAbi::Linux {
                return None;
            }

            let Some(state) = linux_thread_state.as_mut() else {
                return None;
            };
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
    multitask::exit_current_user_task()
}

fn futex_wait(
    uaddr: u64,
    expected: u32,
    timeout_ptr: u64,
    bitset: u32,
) -> Result<u64, LinuxSysopError> {
    if timeout_ptr != 0 {
        return Err(LinuxSysopError::Unsupported);
    }

    let actual = usermem::read_current_user_u32(uaddr)?;
    debug_log_secondary_futex(|| {
        alloc::format!(
            "wait uaddr={:#x} expected={:#x} actual={:#x} timeout_ptr={:#x} bitset={:#x}",
            uaddr,
            expected,
            actual,
            timeout_ptr,
            bitset
        )
    });
    if actual != expected {
        return Err(LinuxSysopError::TryAgain);
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
        return Err(LinuxSysopError::Unsupported);
    }

    debug_log_secondary_futex(|| alloc::format!("blocked task_id={} uaddr={:#x}", task_id, uaddr));
    multitask::yield_now();
    clear_futex_waiter(task_id, key);
    debug_log_secondary_futex(|| alloc::format!("resumed task_id={} uaddr={:#x}", task_id, uaddr));
    Ok(0)
}

fn futex_wake(uaddr: u64, max_wake: u64, bitset: u32) -> Result<u64, LinuxSysopError> {
    let max_wake = usize::try_from(max_wake).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if max_wake == 0 {
        return Ok(0);
    }

    let (_, mut key) = current_futex_waiter_context()?;
    key.uaddr = uaddr;
    let woke = wake_futex_waiters(key, max_wake, bitset) as u64;
    debug_log_secondary_futex(|| {
        alloc::format!(
            "wake uaddr={:#x} max_wake={} bitset={:#x} woke={}",
            uaddr,
            max_wake,
            bitset,
            woke
        )
    });
    Ok(woke)
}

fn current_futex_waiter_context() -> Result<(u64, FutexKey), LinuxSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|pid, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        Ok((
            pid,
            FutexKey {
                address_space_root: process_state.address_space_root(),
                uaddr: 0,
            },
        ))
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn register_futex_waiter(waiter: FutexWaiter) -> Result<(), LinuxSysopError> {
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
        return Err(LinuxSysopError::Busy);
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
