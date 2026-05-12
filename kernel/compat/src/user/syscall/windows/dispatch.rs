use crate::user::sysops::win32 as win32_ops;

use super::super::SyscallFrame;
use super::Api;

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
