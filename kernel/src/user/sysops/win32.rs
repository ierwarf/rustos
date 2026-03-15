use core::convert::TryFrom;

use x86_64::VirtAddr;

use crate::multitask;
use crate::paging;

use super::console;
use super::usermem;

const STD_INPUT_HANDLE_ID: u32 = 0xffff_fff6;
const STD_OUTPUT_HANDLE_ID: u32 = 0xffff_fff5;
const STD_ERROR_HANDLE_ID: u32 = 0xffff_fff4;

const HANDLE_STDIN: u64 = 0x1000_0001;
const HANDLE_STDOUT: u64 = 0x1000_0002;
const HANDLE_STDERR: u64 = 0x1000_0003;

const BOOL_FALSE: u64 = 0;
const BOOL_TRUE: u64 = 1;
const INVALID_HANDLE_VALUE: u64 = u64::MAX;

const FILE_TYPE_CHAR: u64 = 0x0002;
const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
const ENABLE_LINE_INPUT: u32 = 0x0002;
const ENABLE_ECHO_INPUT: u32 = 0x0004;
const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;

pub(crate) const ERROR_SUCCESS: u32 = 0;
pub(crate) const ERROR_INVALID_FUNCTION: u32 = 1;
pub(crate) const ERROR_INVALID_HANDLE: u32 = 6;
pub(crate) const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
pub(crate) const ERROR_NOT_READY: u32 = 21;
pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;

pub(crate) fn exit_process(_exit_code: u64) -> u64 {
    multitask::exit_current_user_task()
}

pub(crate) fn get_std_handle(std_handle: u64) -> u64 {
    set_last_error(ERROR_SUCCESS);
    match std_handle as u32 {
        STD_INPUT_HANDLE_ID => HANDLE_STDIN,
        STD_OUTPUT_HANDLE_ID => HANDLE_STDOUT,
        STD_ERROR_HANDLE_ID => HANDLE_STDERR,
        _ => {
            set_last_error(ERROR_INVALID_PARAMETER);
            INVALID_HANDLE_VALUE
        }
    }
}

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

    let Some(address_space) = multitask::current_user_address_space() else {
        set_last_error(ERROR_INVALID_FUNCTION);
        return BOOL_FALSE;
    };

    if bytes_written_ptr != 0
        && address_space
            .validate_user_write_buffer(VirtAddr::new(bytes_written_ptr), 4)
            .is_err()
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    match console::write_from_current_process(buffer, len) {
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

    let Some(address_space) = multitask::current_user_address_space() else {
        set_last_error(ERROR_INVALID_FUNCTION);
        return BOOL_FALSE;
    };

    if address_space
        .validate_user_write_buffer(VirtAddr::new(buffer), len)
        .is_err()
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    if bytes_read_ptr != 0
        && address_space
            .validate_user_write_buffer(VirtAddr::new(bytes_read_ptr), 4)
            .is_err()
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    match console::read_into_current_process(buffer, len) {
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
    crate::rtc::sleep(milliseconds);
    set_last_error(ERROR_SUCCESS);
    0
}

pub(crate) fn close_handle(handle: u64) -> u64 {
    if matches!(handle, HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR) {
        set_last_error(ERROR_SUCCESS);
        return BOOL_TRUE;
    }

    set_last_error(ERROR_INVALID_HANDLE);
    BOOL_FALSE
}

pub(crate) fn get_last_error() -> u64 {
    multitask::current_last_error() as u64
}

pub(crate) fn set_last_error(value: u32) -> u64 {
    multitask::set_current_last_error(value);
    0
}

pub(crate) fn get_file_type(handle: u64) -> u64 {
    if matches!(handle, HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR) {
        set_last_error(ERROR_SUCCESS);
        return FILE_TYPE_CHAR;
    }

    set_last_error(ERROR_INVALID_HANDLE);
    0
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

fn address_space_error_to_win32(err: paging::AddressSpaceError) -> u32 {
    match err {
        paging::AddressSpaceError::ZeroSizedAllocation => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::AddressOverflow => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::AddressOutOfRange => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::AddressNotPageAligned => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::AlreadyMapped => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::NotMapped => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::ProtectionViolation => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::HugePageConflict => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::OutOfFrames => ERROR_NOT_ENOUGH_MEMORY,
    }
}
