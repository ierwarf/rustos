use x86_64::VirtAddr;

use crate::multitask;
use crate::user::abi::UserAbi;
use crate::user::process_state::{UserProcessState, WindowsProcessRuntimeState};

use super::constants::ERROR_INVALID_FUNCTION;

pub(crate) fn set_last_error(value: u32) -> u64 {
    multitask::set_current_last_error(value);
    let _ = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Windows {
            return;
        }
        let Some(runtime) = process_state.windows_runtime() else {
            return;
        };
        let _ = process_state
            .address_space()
            .copy_into_user(VirtAddr::new(runtime.last_error_ptr), &value.to_le_bytes());
    });
    0
}

pub(super) fn with_windows_runtime_mut<R>(
    f: impl FnOnce(&mut UserProcessState, &mut WindowsProcessRuntimeState) -> Result<R, u32>,
) -> Result<R, u32> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Windows {
            return Err(ERROR_INVALID_FUNCTION);
        }
        let Some(mut runtime) = process_state.windows_runtime() else {
            return Err(ERROR_INVALID_FUNCTION);
        };
        let result = f(process_state, &mut runtime)?;
        *process_state
            .windows_runtime_mut()
            .ok_or(ERROR_INVALID_FUNCTION)? = runtime;
        Ok(result)
    }) else {
        return Err(ERROR_INVALID_FUNCTION);
    };
    result
}
