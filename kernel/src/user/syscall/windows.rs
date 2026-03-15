use crate::debug;
use crate::user::sysops::win32 as win32_ops;

use super::SyscallFrame;

const SYSCALL_BASE: u64 = 0x1000;
const IMPORT_THUNK_BYTES: usize = 34;

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

pub(super) fn dispatch_syscall(frame: &SyscallFrame) -> u64 {
    let api = match syscall_check(frame) {
        Ok(api) => api,
        Err(error) => return error,
    };

    let arg0 = frame.rdi;
    let arg1 = frame.rsi;
    let arg2 = frame.rdx;
    let arg3 = frame.r8;
    let arg4 = frame.r9;

    match api {
        Api::ExitProcess | Api::RtlExitUserProcess => win32_ops::exit_process(arg0),
        Api::GetStdHandle => win32_ops::get_std_handle(arg0),
        Api::WriteFile | Api::WriteConsoleA => win32_ops::write_file(arg0, arg1, arg2, arg3, arg4),
        Api::ReadFile | Api::ReadConsoleA => win32_ops::read_file(arg0, arg1, arg2, arg3, arg4),
        Api::Sleep => win32_ops::sleep(arg0),
        Api::CloseHandle => win32_ops::close_handle(arg0),
        Api::GetLastError => win32_ops::get_last_error(),
        Api::SetLastError => win32_ops::set_last_error(arg0 as u32),
        Api::GetFileType => win32_ops::get_file_type(arg0),
        Api::GetConsoleMode => win32_ops::get_console_mode(arg0, arg1),
        Api::SetConsoleMode => win32_ops::set_console_mode(arg0, arg1),
    }
}

fn syscall_check(frame: &SyscallFrame) -> Result<Api, u64> {
    let Some(api) = api_from_syscall_number(frame.rax) else {
        win32_ops::set_last_error(win32_ops::ERROR_INVALID_FUNCTION);
        return Err(super::SYSCALL_ERR_INVALID);
    };

    if !super::syscall_frame_security_check(frame) {
        debug::println!(
            "rejected unsafe windows syscall: nr={:#x} rip={:#x} rsp={:#x} rflags={:#x}",
            frame.rax,
            frame.user_rip,
            frame.user_rsp,
            frame.user_rflags,
        );
        win32_ops::set_last_error(win32_ops::ERROR_INVALID_PARAMETER);
        return Err(super::SYSCALL_ERR_INVALID);
    }

    Ok(api)
}

fn api_from_syscall_number(syscall_number: u64) -> Option<Api> {
    Some(match syscall_number.checked_sub(SYSCALL_BASE) {
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
    })
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
