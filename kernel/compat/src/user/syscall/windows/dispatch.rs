// RING3-MIGRATION-REFERENCE START: decode exception: syscalld owns Win32
// syscall policy. Ring0 keeps syscall frame validation, current-process
// user-copy, and dispatch substrate.
use core::mem::size_of;
use core::slice;

use rustos_user_abi::syscall::{
    SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY, SYSCALL_OFFLOAD_OP_WIN32_CLOSE,
    SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION, SYSCALL_OFFLOAD_OP_WIN32_EXIT_PROCESS,
    SYSCALL_OFFLOAD_OP_WIN32_FREE_VIRTUAL_MEMORY, SYSCALL_OFFLOAD_OP_WIN32_GET_CONSOLE_MODE,
    SYSCALL_OFFLOAD_OP_WIN32_PROTECT_VIRTUAL_MEMORY, SYSCALL_OFFLOAD_OP_WIN32_QUERY_VIRTUAL_MEMORY,
    SYSCALL_OFFLOAD_OP_WIN32_READ_FILE, SYSCALL_OFFLOAD_OP_WIN32_SET_CONSOLE_MODE,
    SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE, WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
    Win32SyscallOffloadRequest, Win32SyscallOffloadResponse,
};

use super::super::SyscallFrame;
use super::Api;

const WIN32_ERROR_INVALID_FUNCTION: u32 = 1;
const WIN32_ERROR_INVALID_HANDLE: u32 = 6;
const WIN32_ERROR_INVALID_PARAMETER: u32 = 87;
const STATUS_INVALID_HANDLE: u64 = 0xc000_0008;
const STATUS_INVALID_PARAMETER: u64 = 0xc000_000d;
const STATUS_INVALID_SYSTEM_SERVICE: u64 = 0xc000_001c;

pub(crate) fn dispatch_syscall(frame: &mut SyscallFrame) -> u64 {
    let api = match syscall_check(frame) {
        Ok(api) => api,
        Err(error) => return error,
    };
    let request = Win32SyscallOffloadRequest {
        version: WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
        op: win32_offload_op(api),
        arg0: frame.rdi,
        arg1: frame.rsi,
        arg2: frame.rdx,
        arg3: frame.r8,
        arg4: frame.r9,
        arg5: 0,
        pid: crate::multitask::current_user_snapshot()
            .map(|s| s.process_id())
            .unwrap_or(0),
        tid: crate::multitask::current_user_snapshot()
            .map(|s| s.thread_id())
            .unwrap_or(0),
        session_handle: crate::multitask::current_user_snapshot()
            .map(|s| s.console_session().raw())
            .unwrap_or(0),
        ..Win32SyscallOffloadRequest::default()
    };
    match super::super::linux::call_syscalld_raw(as_bytes(&request)) {
        Ok(bytes) if bytes.len() == size_of::<Win32SyscallOffloadResponse>() => {
            let response = read_unaligned::<Win32SyscallOffloadResponse>(bytes.as_slice());
            if response.version != WIN32_SYSCALL_OFFLOAD_ABI_VERSION
                || response.op != request.op
                || response.reserved0 != 0
            {
                return STATUS_INVALID_SYSTEM_SERVICE;
            }
            if response.status == 0 {
                response.result
            } else {
                policy_status_to_ntstatus(response.status)
            }
        }
        _ => STATUS_INVALID_SYSTEM_SERVICE,
    }
}

fn policy_status_to_ntstatus(status: u32) -> u64 {
    match status {
        WIN32_ERROR_INVALID_FUNCTION => STATUS_INVALID_SYSTEM_SERVICE,
        WIN32_ERROR_INVALID_HANDLE => STATUS_INVALID_HANDLE,
        WIN32_ERROR_INVALID_PARAMETER => STATUS_INVALID_PARAMETER,
        _ => STATUS_INVALID_PARAMETER,
    }
}

fn win32_offload_op(api: Api) -> u16 {
    match api {
        Api::NtWriteFile => SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE,
        Api::NtReadFile => SYSCALL_OFFLOAD_OP_WIN32_READ_FILE,
        Api::NtDelayExecution => SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION,
        Api::NtClose => SYSCALL_OFFLOAD_OP_WIN32_CLOSE,
        Api::NtGetConsoleMode => SYSCALL_OFFLOAD_OP_WIN32_GET_CONSOLE_MODE,
        Api::NtSetConsoleMode => SYSCALL_OFFLOAD_OP_WIN32_SET_CONSOLE_MODE,
        Api::RtlExitUserProcess => SYSCALL_OFFLOAD_OP_WIN32_EXIT_PROCESS,
        Api::NtAllocateVirtualMemory => SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY,
        Api::NtFreeVirtualMemory => SYSCALL_OFFLOAD_OP_WIN32_FREE_VIRTUAL_MEMORY,
        Api::NtProtectVirtualMemory => SYSCALL_OFFLOAD_OP_WIN32_PROTECT_VIRTUAL_MEMORY,
        Api::NtQueryVirtualMemory => SYSCALL_OFFLOAD_OP_WIN32_QUERY_VIRTUAL_MEMORY,
    }
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    debug_assert!(bytes.len() >= size_of::<T>());
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
}

fn syscall_check(frame: &SyscallFrame) -> Result<Api, u64> {
    let Some(api) = Api::from_syscall_number(frame.rax) else {
        return Err(super::super::SYSCALL_ERR_INVALID);
    };
    if !super::super::syscall_frame_security_check(frame) {
        super::super::validate_syscall_entry_or_terminate(frame);
    }
    Ok(api)
}
// RING3-MIGRATION-REFERENCE END: syscalld-owned Win32 syscall dispatch decode exception.
