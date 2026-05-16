use rustos_user_abi::syscall::{
    Win32SyscallOffloadRequest, Win32SyscallOffloadResponse,
    SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY, SYSCALL_OFFLOAD_OP_WIN32_CLOSE,
    SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION, SYSCALL_OFFLOAD_OP_WIN32_EXIT_PROCESS,
    SYSCALL_OFFLOAD_OP_WIN32_FREE_VIRTUAL_MEMORY, SYSCALL_OFFLOAD_OP_WIN32_GET_CONSOLE_MODE,
    SYSCALL_OFFLOAD_OP_WIN32_PROTECT_VIRTUAL_MEMORY, SYSCALL_OFFLOAD_OP_WIN32_QUERY_VIRTUAL_MEMORY,
    SYSCALL_OFFLOAD_OP_WIN32_READ_FILE, SYSCALL_OFFLOAD_OP_WIN32_SET_CONSOLE_MODE,
    SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE,
};

pub(crate) const ERROR_INVALID_FUNCTION: u32 = 1;
pub(crate) const ERROR_INVALID_HANDLE: u32 = 6;
pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;

const HANDLE_STDIN: u64 = 0x1000_0001;
const HANDLE_STDOUT: u64 = 0x1000_0002;
const HANDLE_STDERR: u64 = 0x1000_0003;
const HANDLE_CURRENT_PROCESS: u64 = u64::MAX;

const BOOL_FALSE: u64 = 0;
const PAGE_SIZE: u64 = 4096;
const PAGE_NOACCESS: u64 = 0x0001;
const PAGE_READONLY: u64 = 0x0002;
const PAGE_READWRITE: u64 = 0x0004;
const PAGE_EXECUTE_READ: u64 = 0x0020;
const PAGE_EXECUTE_READWRITE: u64 = 0x0040;
const MEM_COMMIT: u64 = 0x1000;
const MEM_RESERVE: u64 = 0x2000;
const MEM_RELEASE: u64 = 0x8000;
const MAX_CONSOLE_IO_BYTES: u64 = 1024 * 1024;

pub(crate) fn handle_request(
    request: &Win32SyscallOffloadRequest,
    response: &mut Win32SyscallOffloadResponse,
) {
    let denied = match request.op {
        SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE => validate_write_file(request),
        SYSCALL_OFFLOAD_OP_WIN32_READ_FILE => validate_read_file(request),
        SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION => Ok(()),
        SYSCALL_OFFLOAD_OP_WIN32_CLOSE => validate_close(request),
        SYSCALL_OFFLOAD_OP_WIN32_GET_CONSOLE_MODE => validate_get_console_mode(request),
        SYSCALL_OFFLOAD_OP_WIN32_SET_CONSOLE_MODE => validate_set_console_mode(request),
        SYSCALL_OFFLOAD_OP_WIN32_EXIT_PROCESS => Ok(()),
        SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY => validate_virtual_alloc(request),
        SYSCALL_OFFLOAD_OP_WIN32_FREE_VIRTUAL_MEMORY => validate_virtual_free(request),
        SYSCALL_OFFLOAD_OP_WIN32_PROTECT_VIRTUAL_MEMORY => validate_virtual_protect(request),
        SYSCALL_OFFLOAD_OP_WIN32_QUERY_VIRTUAL_MEMORY => validate_virtual_query(request),
        _ => Err(ERROR_INVALID_FUNCTION),
    };
    if let Err(error) = denied {
        response.status = error;
        response.result = BOOL_FALSE;
    }
}

fn validate_write_file(request: &Win32SyscallOffloadRequest) -> Result<(), u32> {
    if !matches!(request.arg0, HANDLE_STDOUT | HANDLE_STDERR) {
        return Err(ERROR_INVALID_HANDLE);
    }
    validate_console_io(request.arg1, request.arg2, request.arg4)
}

fn validate_read_file(request: &Win32SyscallOffloadRequest) -> Result<(), u32> {
    if request.arg0 != HANDLE_STDIN {
        return Err(ERROR_INVALID_HANDLE);
    }
    validate_console_io(request.arg1, request.arg2, request.arg4)
}

fn validate_console_io(buffer: u64, len: u64, overlapped: u64) -> Result<(), u32> {
    if overlapped != 0 || len > MAX_CONSOLE_IO_BYTES || len != 0 && buffer == 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    Ok(())
}

fn validate_close(request: &Win32SyscallOffloadRequest) -> Result<(), u32> {
    if matches!(
        request.arg0,
        HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR | HANDLE_CURRENT_PROCESS
    ) {
        Ok(())
    } else {
        Err(ERROR_INVALID_HANDLE)
    }
}

fn validate_get_console_mode(request: &Win32SyscallOffloadRequest) -> Result<(), u32> {
    if !matches!(request.arg0, HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR) {
        return Err(ERROR_INVALID_HANDLE);
    }
    if request.arg1 == 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    Ok(())
}

fn validate_set_console_mode(request: &Win32SyscallOffloadRequest) -> Result<(), u32> {
    if matches!(request.arg0, HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR) {
        Ok(())
    } else {
        Err(ERROR_INVALID_HANDLE)
    }
}

fn validate_virtual_alloc(request: &Win32SyscallOffloadRequest) -> Result<(), u32> {
    let allocation_type = request.arg2;
    let protect = request.arg3;
    if request.arg1 == 0
        || allocation_type & !(MEM_COMMIT | MEM_RESERVE) != 0
        || allocation_type & (MEM_COMMIT | MEM_RESERVE) == 0
        || !valid_protect(protect)
        || request.arg0 != 0 && request.arg0 & (PAGE_SIZE - 1) != 0
    {
        return Err(ERROR_INVALID_PARAMETER);
    }
    Ok(())
}

fn validate_virtual_free(request: &Win32SyscallOffloadRequest) -> Result<(), u32> {
    if request.arg0 == 0 || request.arg1 != 0 || request.arg2 != MEM_RELEASE {
        return Err(ERROR_INVALID_PARAMETER);
    }
    Ok(())
}

fn validate_virtual_protect(request: &Win32SyscallOffloadRequest) -> Result<(), u32> {
    if request.arg0 == 0 || request.arg1 == 0 || !valid_protect(request.arg2) || request.arg3 == 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    Ok(())
}

fn validate_virtual_query(request: &Win32SyscallOffloadRequest) -> Result<(), u32> {
    if request.arg1 == 0 || request.arg2 == 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    Ok(())
}

fn valid_protect(protect: u64) -> bool {
    matches!(
        protect,
        PAGE_NOACCESS | PAGE_READONLY | PAGE_READWRITE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE
    )
}
