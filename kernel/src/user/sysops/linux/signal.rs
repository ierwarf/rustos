use super::*;

pub(crate) fn rt_sigaction(
    signal: u64,
    action_ptr: u64,
    old_action_ptr: u64,
    sigset_size: u64,
) -> Result<(), LinuxSysopError> {
    if sigset_size != LINUX_SIGSET_SIZE {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if signal == 0 || signal > linux_abi::MAX_SIGNAL_NUMBER as u64 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if action_ptr != 0 && matches!(signal, linux_abi::SIGKILL | linux_abi::SIGSTOP) {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let mut new_action = linux_abi::LinuxSigAction::default();
    if action_ptr != 0 {
        let new_action_bytes = unsafe {
            slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(new_action).cast::<u8>(),
                size_of::<linux_abi::LinuxSigAction>(),
            )
        };
        usermem::copy_from_current_user_exact(action_ptr, new_action_bytes)?;
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        let current = process_state
            .linux_signal_action(signal)
            .ok_or(LinuxSysopError::InvalidArgument)?;
        if old_action_ptr != 0 {
            let current_bytes = unsafe {
                slice::from_raw_parts(
                    core::ptr::addr_of!(current).cast::<u8>(),
                    size_of::<linux_abi::LinuxSigAction>(),
                )
            };
            usermem::write_current_user_bytes(old_action_ptr, current_bytes)?;
        }
        if action_ptr != 0 {
            process_state
                .set_linux_signal_action(signal, new_action)
                .ok_or(LinuxSysopError::InvalidArgument)?;
        }
        Ok(())
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn sigaltstack(stack_ptr: u64, old_stack_ptr: u64) -> Result<(), LinuxSysopError> {
    let mut requested = linux_abi::LinuxSignalStack::default();
    if stack_ptr != 0 {
        let requested_bytes = unsafe {
            slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(requested).cast::<u8>(),
                size_of::<linux_abi::LinuxSignalStack>(),
            )
        };
        usermem::copy_from_current_user_exact(stack_ptr, requested_bytes)?;
    }

    let Some(result) = multitask::with_current_user_linux_state_mut(
        |_, _, abi, address_space, _, linux_thread_state| {
            if abi != UserAbi::Linux {
                return Err(LinuxSysopError::Unsupported);
            }

            let Some(state) = linux_thread_state.as_mut() else {
                return Err(LinuxSysopError::Unsupported);
            };

            if old_stack_ptr != 0 {
                let current = state.signal_stack;
                let current_bytes = unsafe {
                    slice::from_raw_parts(
                        core::ptr::addr_of!(current).cast::<u8>(),
                        size_of::<linux_abi::LinuxSignalStack>(),
                    )
                };
                address_space.copy_into_user(VirtAddr::new(old_stack_ptr), current_bytes)?;
            }

            if stack_ptr == 0 {
                return Ok(());
            }

            if requested.flags & !(linux_abi::SS_ONSTACK | linux_abi::SS_DISABLE) != 0 {
                return Err(LinuxSysopError::InvalidArgument);
            }
            if requested.flags & linux_abi::SS_ONSTACK != 0 {
                return Err(LinuxSysopError::InvalidArgument);
            }

            if requested.flags & linux_abi::SS_DISABLE != 0 {
                state.signal_stack = disabled_signal_stack();
                return Ok(());
            }

            if requested.sp == 0 || requested.size == 0 {
                return Err(LinuxSysopError::InvalidArgument);
            }
            let stack_len =
                usize::try_from(requested.size).map_err(|_| LinuxSysopError::InvalidArgument)?;
            address_space
                .validate_user_write_buffer(VirtAddr::new(requested.sp), stack_len)
                .map_err(LinuxSysopError::AddressSpace)?;

            state.signal_stack = linux_abi::LinuxSignalStack {
                sp: requested.sp,
                flags: 0,
                _pad: 0,
                size: requested.size,
            };
            Ok(())
        },
    ) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn rt_sigprocmask(
    how: u64,
    set_ptr: u64,
    oldset_ptr: u64,
    sigset_size: u64,
) -> Result<(), LinuxSysopError> {
    let _ = how;
    if sigset_size != LINUX_SIGSET_SIZE {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if set_ptr != 0 {
        let mut incoming = [0_u8; LINUX_SIGSET_SIZE as usize];
        usermem::copy_from_current_user_exact(set_ptr, &mut incoming)?;
    }
    if oldset_ptr != 0 {
        usermem::write_current_user_bytes(oldset_ptr, &0_u64.to_le_bytes())?;
    }
    Ok(())
}

pub(crate) fn tgkill(tgid: u64, tid: u64, signal: u64) -> Result<(), LinuxSysopError> {
    if signal > linux_abi::MAX_SIGNAL_NUMBER as u64 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let current_pid = super::process::getpid();
    let current_tid = super::process::gettid();
    if tgid != current_pid || tid != current_tid {
        return Err(LinuxSysopError::NoSuchProcess);
    }

    if signal == 0 {
        return Ok(());
    }

    if multitask::current_user_id() == Some(1) {
        debug::println!(
            "linux tgkill: self-targeted signal delivery deferred pid={} tid={} sig={}",
            current_pid,
            current_tid,
            signal,
        );
    }
    Ok(())
}
