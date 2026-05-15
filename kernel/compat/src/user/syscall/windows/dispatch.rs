use crate::user::sysops::win32 as win32_ops;

use super::super::SyscallFrame;
use super::Api;
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

pub(crate) fn dispatch_syscall(frame: &mut SyscallFrame) -> u64 {
    let api = match syscall_check(frame) {
        Ok(api) => api,
        Err(error) => return error,
    };
    let arg0 = frame.rdi;
    let arg1 = frame.rsi;
    let arg2 = frame.rdx;
    let arg3 = frame.r8;
    let arg4 = frame.r9;
    if let Err(response) = call_win32_policy(api, arg0, arg1, arg2, arg3, arg4, 0) {
        win32_ops::set_last_error(response.status);
        return response.result;
    }

    match api {
        Api::RtlExitUserProcess => win32_ops::exit_process(arg0),
        Api::NtWriteFile => win32_ops::write_file(arg0, arg1, arg2, arg3, arg4),
        Api::NtReadFile => win32_ops::read_file(arg0, arg1, arg2, arg3, arg4),
        Api::NtDelayExecution => win32_ops::sleep(arg0),
        Api::NtClose => win32_ops::close_handle(arg0),
        Api::NtGetConsoleMode => win32_ops::get_console_mode(arg0, arg1),
        Api::NtSetConsoleMode => win32_ops::set_console_mode(arg0, arg1),
        Api::NtAllocateVirtualMemory => win32_ops::virtual_alloc(arg0, arg1, arg2, arg3),
        Api::NtFreeVirtualMemory => win32_ops::virtual_free(arg0, arg1, arg2),
        Api::NtProtectVirtualMemory => win32_ops::virtual_protect(arg0, arg1, arg2, arg3),
        Api::NtQueryVirtualMemory => win32_ops::virtual_query(arg0, arg1, arg2),
    }
}

fn call_win32_policy(
    api: Api,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> Result<(), Win32SyscallOffloadResponse> {
    let mut request = Win32SyscallOffloadRequest {
        version: WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
        op: win32_offload_op(api),
        arg0,
        arg1,
        arg2,
        arg3,
        arg4,
        arg5,
        ..Win32SyscallOffloadRequest::default()
    };
    if let Some(snapshot) = crate::multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.session_handle = snapshot.console_session().raw();
    }
    let response = match super::super::linux::call_syscalld_raw(as_bytes(&request)) {
        Ok(bytes) if bytes.len() == size_of::<Win32SyscallOffloadResponse>() => {
            read_unaligned::<Win32SyscallOffloadResponse>(bytes.as_slice())
        }
        _ => {
            return Err(Win32SyscallOffloadResponse {
                version: WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
                op: request.op,
                status: win32_ops::ERROR_INVALID_FUNCTION,
                result: 0,
                reserved0: 0,
            });
        }
    };
    if response.version != WIN32_SYSCALL_OFFLOAD_ABI_VERSION
        || response.op != request.op
        || response.reserved0 != 0
    {
        return Err(Win32SyscallOffloadResponse {
            version: WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
            op: request.op,
            status: win32_ops::ERROR_INVALID_FUNCTION,
            result: 0,
            reserved0: 0,
        });
    }
    if response.status != 0 {
        return Err(response);
    }
    Ok(())
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
        win32_ops::set_last_error(win32_ops::ERROR_INVALID_FUNCTION);
        return Err(super::super::SYSCALL_ERR_INVALID);
    };
    if !super::super::syscall_frame_security_check(frame) {
        super::super::validate_syscall_entry_or_terminate(frame);
    }
    Ok(api)
}
