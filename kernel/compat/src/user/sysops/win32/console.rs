use x86_64::VirtAddr;

use crate::multitask;
use crate::user::sysops::console as host_console;
use crate::user::sysops::usermem;

use super::constants::{
    BOOL_FALSE, BOOL_TRUE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_PROCESSED_OUTPUT, ENABLE_WRAP_AT_EOL_OUTPUT, ERROR_INVALID_FUNCTION,
    ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_SUCCESS, HANDLE_CURRENT_PROCESS,
    HANDLE_STDERR, HANDLE_STDIN, HANDLE_STDOUT,
};
use super::runtime::set_last_error;
use super::util::address_space_error_to_win32;

pub(crate) fn write_file(
    handle: u64,
    buffer: u64,
    len: u64,
    bytes_written_ptr: u64,
    overlapped: u64,
) -> u64 {
    if overlapped != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }
    if !matches!(handle, HANDLE_STDOUT | HANDLE_STDERR) {
        set_last_error(ERROR_INVALID_HANDLE);
        return BOOL_FALSE;
    }
    let Ok(len) = usize::try_from(len) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    };
    let Some(retained) = multitask::current_user_address_space() else {
        set_last_error(ERROR_INVALID_FUNCTION);
        return BOOL_FALSE;
    };
    let address_space = retained.address_space();
    if bytes_written_ptr != 0
        && address_space
            .validate_user_write_buffer(VirtAddr::new(bytes_written_ptr), 4)
            .is_err()
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }
    match host_console::write_from_current_process(buffer, len) {
        Ok(written) => {
            if bytes_written_ptr != 0
                && usermem::write_current_user_u32(bytes_written_ptr, written as u32).is_err()
            {
                set_last_error(ERROR_INVALID_PARAMETER);
                return BOOL_FALSE;
            }
            set_last_error(ERROR_SUCCESS);
            BOOL_TRUE
        }
        Err(err) => {
            set_last_error(address_space_error_to_win32(err));
            BOOL_FALSE
        }
    }
}

pub(crate) fn read_file(
    handle: u64,
    buffer: u64,
    len: u64,
    bytes_read_ptr: u64,
    overlapped: u64,
) -> u64 {
    if overlapped != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }
    if handle != HANDLE_STDIN {
        set_last_error(ERROR_INVALID_HANDLE);
        return BOOL_FALSE;
    }
    let Ok(len) = usize::try_from(len) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    };
    let Some(retained) = multitask::current_user_address_space() else {
        set_last_error(ERROR_INVALID_FUNCTION);
        return BOOL_FALSE;
    };
    let address_space = retained.address_space();
    if address_space
        .validate_user_write_buffer(VirtAddr::new(buffer), len)
        .is_err()
        || bytes_read_ptr != 0
            && address_space
                .validate_user_write_buffer(VirtAddr::new(bytes_read_ptr), 4)
                .is_err()
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }
    match host_console::read_into_current_process(buffer, len) {
        Ok(read) => {
            if bytes_read_ptr != 0
                && usermem::write_current_user_u32(bytes_read_ptr, read as u32).is_err()
            {
                set_last_error(ERROR_INVALID_PARAMETER);
                return BOOL_FALSE;
            }
            set_last_error(ERROR_SUCCESS);
            BOOL_TRUE
        }
        Err(err) => {
            set_last_error(address_space_error_to_win32(err));
            BOOL_FALSE
        }
    }
}

pub(crate) fn sleep(milliseconds: u64) -> u64 {
    crate::arch::rtc::sleep(milliseconds);
    set_last_error(ERROR_SUCCESS);
    0
}

pub(crate) fn close_handle(handle: u64) -> u64 {
    if matches!(
        handle,
        HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR | HANDLE_CURRENT_PROCESS
    ) {
        set_last_error(ERROR_SUCCESS);
        return BOOL_TRUE;
    }
    set_last_error(ERROR_INVALID_HANDLE);
    BOOL_FALSE
}

pub(crate) fn get_console_mode(handle: u64, mode_ptr: u64) -> u64 {
    let mode = match handle {
        HANDLE_STDIN => ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT,
        HANDLE_STDOUT | HANDLE_STDERR => ENABLE_PROCESSED_OUTPUT | ENABLE_WRAP_AT_EOL_OUTPUT,
        _ => {
            set_last_error(ERROR_INVALID_HANDLE);
            return BOOL_FALSE;
        }
    };
    if mode_ptr == 0 || usermem::write_current_user_u32(mode_ptr, mode).is_err() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }
    set_last_error(ERROR_SUCCESS);
    BOOL_TRUE
}

pub(crate) fn set_console_mode(handle: u64, _mode: u64) -> u64 {
    if matches!(handle, HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR) {
        set_last_error(ERROR_SUCCESS);
        return BOOL_TRUE;
    }
    set_last_error(ERROR_INVALID_HANDLE);
    BOOL_FALSE
}
