use core::convert::TryFrom;

use x86_64::VirtAddr;

use crate::{multitask, paging, syscall};

const SYSCALL_BASE: u64 = 0x1000;
const IMPORT_THUNK_BYTES: usize = 34;

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

const ERROR_SUCCESS: u32 = 0;
const ERROR_INVALID_FUNCTION: u32 = 1;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_INVALID_PARAMETER: u32 = 87;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Api {
    ExitProcess = 1,
    GetStdHandle = 2,
    WriteFile = 3,
    ReadFile = 4,
    Sleep = 5,
    CloseHandle = 6,
    GetLastError = 7,
    SetLastError = 8,
    WriteConsoleA = 9,
    ReadConsoleA = 10,
    GetFileType = 11,
    GetConsoleMode = 12,
    SetConsoleMode = 13,
    RtlExitUserProcess = 14,
}

impl Api {
    pub const fn syscall_number(self) -> u64 {
        SYSCALL_BASE + self as u64
    }
}

pub fn resolve_import(dll_name: &[u8], function_name: &[u8]) -> Option<Api> {
    if dll_name_eq(dll_name, b"kernel32.dll") {
        return match function_name {
            b"ExitProcess" => Some(Api::ExitProcess),
            b"GetStdHandle" => Some(Api::GetStdHandle),
            b"WriteFile" => Some(Api::WriteFile),
            b"ReadFile" => Some(Api::ReadFile),
            b"Sleep" => Some(Api::Sleep),
            b"CloseHandle" => Some(Api::CloseHandle),
            b"GetLastError" => Some(Api::GetLastError),
            b"SetLastError" => Some(Api::SetLastError),
            b"WriteConsoleA" => Some(Api::WriteConsoleA),
            b"ReadConsoleA" => Some(Api::ReadConsoleA),
            b"GetFileType" => Some(Api::GetFileType),
            b"GetConsoleMode" => Some(Api::GetConsoleMode),
            b"SetConsoleMode" => Some(Api::SetConsoleMode),
            _ => None,
        };
    }

    if dll_name_eq(dll_name, b"ntdll.dll") {
        return match function_name {
            b"RtlExitUserProcess" => Some(Api::RtlExitUserProcess),
            _ => None,
        };
    }

    None
}

pub fn import_thunk_len() -> usize {
    IMPORT_THUNK_BYTES
}

pub fn encode_import_thunk(api: Api, dest: &mut [u8]) -> usize {
    assert!(dest.len() >= IMPORT_THUNK_BYTES);

    let syscall_number = api.syscall_number() as u32;
    // Win64 callers treat RDI/RSI as non-volatile, so preserve them while
    // remapping Windows x64 arguments onto the kernel's syscall ABI.
    let bytes = [
        0x57,
        0x56,
        0xB8,
        (syscall_number & 0xff) as u8,
        ((syscall_number >> 8) & 0xff) as u8,
        ((syscall_number >> 16) & 0xff) as u8,
        ((syscall_number >> 24) & 0xff) as u8,
        0x48,
        0x89,
        0xCF,
        0x48,
        0x89,
        0xD6,
        0x4C,
        0x89,
        0xC2,
        0x4D,
        0x89,
        0xC8,
        0x4C,
        0x8B,
        0x4C,
        0x24,
        0x38,
        0x4C,
        0x8B,
        0x54,
        0x24,
        0x40,
        0x0F,
        0x05,
        0x5E,
        0x5F,
        0xC3,
    ];

    dest[..IMPORT_THUNK_BYTES].copy_from_slice(&bytes);
    IMPORT_THUNK_BYTES
}

pub fn dispatch_syscall(
    syscall_number: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    _arg5: u64,
) -> Option<u64> {
    let api = match syscall_number.checked_sub(SYSCALL_BASE) {
        Some(1) => Api::ExitProcess,
        Some(2) => Api::GetStdHandle,
        Some(3) => Api::WriteFile,
        Some(4) => Api::ReadFile,
        Some(5) => Api::Sleep,
        Some(6) => Api::CloseHandle,
        Some(7) => Api::GetLastError,
        Some(8) => Api::SetLastError,
        Some(9) => Api::WriteConsoleA,
        Some(10) => Api::ReadConsoleA,
        Some(11) => Api::GetFileType,
        Some(12) => Api::GetConsoleMode,
        Some(13) => Api::SetConsoleMode,
        Some(14) => Api::RtlExitUserProcess,
        _ => return None,
    };

    Some(match api {
        Api::ExitProcess | Api::RtlExitUserProcess => exit_process(arg0),
        Api::GetStdHandle => get_std_handle(arg0),
        Api::WriteFile | Api::WriteConsoleA => write_file(arg0, arg1, arg2, arg3, arg4),
        Api::ReadFile | Api::ReadConsoleA => read_file(arg0, arg1, arg2, arg3, arg4),
        Api::Sleep => sleep(arg0),
        Api::CloseHandle => close_handle(arg0),
        Api::GetLastError => get_last_error(),
        Api::SetLastError => set_last_error(arg0 as u32),
        Api::GetFileType => get_file_type(arg0),
        Api::GetConsoleMode => get_console_mode(arg0, arg1),
        Api::SetConsoleMode => set_console_mode(arg0, arg1),
    })
}

fn exit_process(_exit_code: u64) -> u64 {
    multitask::exit_current_user_task()
}

fn get_std_handle(std_handle: u64) -> u64 {
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

fn write_file(
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

    match syscall::console_write_from_user(buffer, len) {
        Ok(written) => {
            if bytes_written_ptr != 0
                && syscall::write_user_u32(bytes_written_ptr, written as u32).is_err()
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

fn read_file(
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

    match syscall::console_read_into_user(buffer, len) {
        Ok(read) => {
            if bytes_read_ptr != 0 && syscall::write_user_u32(bytes_read_ptr, read as u32).is_err()
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

fn sleep(milliseconds: u64) -> u64 {
    syscall::sleep_ms(milliseconds);
    set_last_error(ERROR_SUCCESS);
    0
}

fn close_handle(handle: u64) -> u64 {
    if matches!(handle, HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR) {
        set_last_error(ERROR_SUCCESS);
        return BOOL_TRUE;
    }

    set_last_error(ERROR_INVALID_HANDLE);
    BOOL_FALSE
}

fn get_last_error() -> u64 {
    multitask::current_last_error() as u64
}

fn set_last_error(value: u32) -> u64 {
    multitask::set_current_last_error(value);
    0
}

fn get_file_type(handle: u64) -> u64 {
    if matches!(handle, HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR) {
        set_last_error(ERROR_SUCCESS);
        return FILE_TYPE_CHAR;
    }

    set_last_error(ERROR_INVALID_HANDLE);
    0
}

fn get_console_mode(handle: u64, mode_ptr: u64) -> u64 {
    let mode = match handle {
        HANDLE_STDIN => ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT,
        HANDLE_STDOUT | HANDLE_STDERR => ENABLE_PROCESSED_OUTPUT | ENABLE_WRAP_AT_EOL_OUTPUT,
        _ => {
            set_last_error(ERROR_INVALID_HANDLE);
            return BOOL_FALSE;
        }
    };

    if mode_ptr == 0 || syscall::write_user_u32(mode_ptr, mode).is_err() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    set_last_error(ERROR_SUCCESS);
    BOOL_TRUE
}

fn set_console_mode(handle: u64, _mode: u64) -> u64 {
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

fn dll_name_eq(actual: &[u8], expected_ascii_lower: &[u8]) -> bool {
    if actual.len() != expected_ascii_lower.len() {
        return false;
    }

    actual
        .iter()
        .zip(expected_ascii_lower.iter())
        .all(|(&lhs, &rhs)| lhs.to_ascii_lowercase() == rhs)
}
