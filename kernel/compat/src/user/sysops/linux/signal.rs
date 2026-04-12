use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingSignalAction {
    Ignore(u64),
    Terminate(u64),
    UnsupportedHandler(u64),
}

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
    let requested = if stack_ptr != 0 {
        usermem::read_current_user_struct::<linux_abi::LinuxSignalStack>(stack_ptr)?
    } else {
        linux_abi::LinuxSignalStack::default()
    };

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
                usermem::write_current_user_struct(old_stack_ptr, &current)
                    .map_err(LinuxSysopError::AddressSpace)?;
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
    if sigset_size != LINUX_SIGSET_SIZE {
        return Err(LinuxSysopError::InvalidArgument);
    }
    let requested_mask = if set_ptr != 0 {
        usermem::read_current_user_struct::<u64>(set_ptr)?
    } else {
        0
    };

    let Some(result) = multitask::with_current_user_process_and_linux_thread_state_mut(
        |_, _, abi, _, linux_thread_state| {
            if abi != UserAbi::Linux {
                return Err(LinuxSysopError::Unsupported);
            }
            let Some(state) = linux_thread_state.as_mut() else {
                return Err(LinuxSysopError::Unsupported);
            };

            let previous_mask = state.signal_mask;
            if oldset_ptr != 0 {
                usermem::write_current_user_struct(oldset_ptr, &previous_mask)?;
            }

            if set_ptr == 0 {
                return Ok(());
            }

            state.signal_mask = apply_rt_sigprocmask(previous_mask, how, requested_mask)?;
            Ok(())
        },
    ) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn tgkill(tgid: u64, tid: u64, signal: u64) -> Result<(), LinuxSysopError> {
    if signal > linux_abi::MAX_SIGNAL_NUMBER as u64 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if signal == 0 {
        return if multitask::queue_linux_signal(tgid, tid, 0) {
            Ok(())
        } else {
            Err(LinuxSysopError::NoSuchProcess)
        };
    }
    if multitask::queue_linux_signal(tgid, tid, signal) {
        Ok(())
    } else {
        Err(LinuxSysopError::NoSuchProcess)
    }
}

pub(crate) fn current_thread_has_unblocked_pending_signal() -> Result<bool, LinuxSysopError> {
    let Some(result) = multitask::with_current_user_process_and_linux_thread_state_mut(
        |_, _, abi, _, linux_thread_state| {
            if abi != UserAbi::Linux {
                return Err(LinuxSysopError::Unsupported);
            }
            let Some(state) = linux_thread_state.as_ref() else {
                return Err(LinuxSysopError::Unsupported);
            };
            Ok((state.pending_signals & !state.signal_mask) != 0)
        },
    ) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn deliver_pending_signals_for_current_thread() {
    loop {
        let Some(result) = multitask::with_current_user_process_and_linux_thread_state_mut(
            |_, _, abi, process_state, linux_thread_state| {
                if abi != UserAbi::Linux {
                    return None;
                }
                let Some(state) = linux_thread_state.as_mut() else {
                    return None;
                };

                let Some(signal) =
                    select_deliverable_signal(state.pending_signals, state.signal_mask)
                else {
                    return None;
                };
                let signal_bit = linux_signal_bit(signal).expect("deliverable signal bit");
                state.pending_signals &= !signal_bit;

                let action = process_state
                    .linux_signal_action(signal)
                    .unwrap_or_default();
                if action.handler == linux_abi::SIG_IGN || signal == linux_abi::SIGSTOP {
                    return Some(PendingSignalAction::Ignore(signal));
                }
                if action.handler != linux_abi::SIG_DFL {
                    return Some(PendingSignalAction::UnsupportedHandler(signal));
                }

                Some(PendingSignalAction::Terminate(signal))
            },
        ) else {
            return;
        };

        let Some(action) = result else {
            return;
        };
        match action {
            PendingSignalAction::Ignore(_) => {}
            PendingSignalAction::Terminate(signal) => {
                super::thread::exit_current_process(128 + signal);
            }
            PendingSignalAction::UnsupportedHandler(_signal) => {
                debug::println!(
                    "linux signal delivery remains partial: signal={} custom handlers are not installed yet",
                    signal
                );
            }
        }
    }
}

fn apply_rt_sigprocmask(
    previous_mask: u64,
    how: u64,
    requested_mask: u64,
) -> Result<u64, LinuxSysopError> {
    let mut updated_mask = match how {
        linux_abi::SIG_BLOCK => previous_mask | requested_mask,
        linux_abi::SIG_UNBLOCK => previous_mask & !requested_mask,
        linux_abi::SIG_SETMASK => requested_mask,
        _ => return Err(LinuxSysopError::InvalidArgument),
    };
    updated_mask &= !linux_unblockable_signal_mask();
    Ok(updated_mask)
}

fn select_deliverable_signal(pending_signals: u64, signal_mask: u64) -> Option<u64> {
    let deliverable = pending_signals & !signal_mask;
    if deliverable == 0 {
        return None;
    }
    Some(deliverable.trailing_zeros() as u64 + 1)
}

#[cfg(test)]
mod tests {
    use super::{apply_rt_sigprocmask, select_deliverable_signal};
    use crate::user::linux as linux_abi;

    #[test]
    fn sigprocmask_tracks_per_thread_mask_and_never_blocks_sigkill() {
        let mask = apply_rt_sigprocmask(0, linux_abi::SIG_BLOCK, u64::MAX).expect("mask");
        assert_eq!(mask & (1_u64 << (linux_abi::SIGKILL - 1)), 0);
        assert_eq!(mask & (1_u64 << (linux_abi::SIGSTOP - 1)), 0);
    }

    #[test]
    fn deliverable_signal_selection_skips_masked_entries() {
        let pending = (1_u64 << 1) | (1_u64 << 4);
        let masked = 1_u64 << 1;
        assert_eq!(select_deliverable_signal(pending, masked), Some(5));
        assert_eq!(select_deliverable_signal(masked, masked), None);
    }
}
