//! Raw syscall bridge and debug output.

pub(super) fn syscall1(number: u64, arg0: u64) -> i64 {
    // SAFETY: The bridge owns its descriptor; callers retain IPC buffers until return.
    if let Some(result) =
        unsafe { rustos_svc_runtime::syscall::try_fast_ipc(number, [arg0, 0, 0, 0, 0, 0]) }
    {
        return result;
    }
    unsafe { libc::syscall(number as libc::c_long, arg0) as i64 }
}

pub(super) fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    // SAFETY: The bridge owns its descriptor; callers retain IPC buffers until return.
    if let Some(result) =
        unsafe { rustos_svc_runtime::syscall::try_fast_ipc(number, [arg0, arg1, 0, 0, 0, 0]) }
    {
        return result;
    }
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1) as i64 }
}

pub(super) fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    // SAFETY: The bridge owns its descriptor; callers retain IPC buffers until return.
    if let Some(result) =
        unsafe { rustos_svc_runtime::syscall::try_fast_ipc(number, [arg0, arg1, arg2, 0, 0, 0]) }
    {
        return result;
    }
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2) as i64 }
}

pub(super) fn syscall6(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i64 {
    // SAFETY: The bridge owns its descriptor; callers retain IPC buffers until return.
    if let Some(result) = unsafe {
        rustos_svc_runtime::syscall::try_fast_ipc(number, [arg0, arg1, arg2, arg3, arg4, arg5])
    } {
        return result;
    }
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3, arg4, arg5) as i64 }
}

pub(super) fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

pub(super) fn debug_line(message: &str) {
    let bytes = message.as_bytes();
    let len = bytes.len().min(1023);
    let mut line = [0_u8; 1024];
    line[..len].copy_from_slice(&bytes[..len]);
    line[len] = b'\n';
    rustos_svc_runtime::ipc::debug_bytes(&line[..len + 1]);
}
