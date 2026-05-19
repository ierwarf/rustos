//! Thin convenience wrappers around the RustOS native IPC syscalls. Services
//! can call these directly without going through libc.

use rustos_user_abi::syscall::{
    SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_IPC_CALL, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_RECV,
    SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REPLY,
};

use crate::syscall::{syscall0, syscall1, syscall2, syscall3, syscall4, syscall5};

#[inline]
pub fn endpoint_create() -> i64 {
    unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) }
}

#[inline]
pub fn register_linux_syscall_endpoint(endpoint: u64) -> i64 {
    unsafe { syscall1(SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT, endpoint) }
}

#[inline]
pub fn register_service_endpoint(service_id: u64, endpoint: u64) -> i64 {
    unsafe {
        syscall2(
            SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
            service_id,
            endpoint,
        )
    }
}

#[inline]
pub fn lookup_service_endpoint(service_id: u64) -> i64 {
    unsafe { syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, service_id) }
}

#[inline]
pub unsafe fn call(
    endpoint: u64,
    request: *const u8,
    request_len: usize,
    reply: *mut u8,
    reply_capacity: usize,
) -> i64 {
    syscall5(
        SYS_RUSTOS_IPC_CALL,
        endpoint,
        request as u64,
        request_len as u64,
        reply as u64,
        reply_capacity as u64,
    )
}

/// Block until a request arrives on `endpoint`. On success, the request bytes
/// are placed into `request_buf` (truncated to `request_capacity`) and
/// `reply_cap_out` receives the reply capability the service must hand back to
/// `reply()`.
///
/// Returns the number of request bytes received, or a negative errno.
#[inline]
pub unsafe fn recv(
    endpoint: u64,
    request_buf: *mut u8,
    request_capacity: usize,
    reply_cap_out: *mut u64,
) -> i64 {
    syscall4(
        SYS_RUSTOS_IPC_RECV,
        endpoint,
        request_buf as u64,
        request_capacity as u64,
        reply_cap_out as u64,
    )
}

/// Send a reply to the caller identified by `reply_cap`.
#[inline]
pub unsafe fn reply(reply_cap: u64, response: *const u8, response_len: usize) -> i64 {
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        response as u64,
        response_len as u64,
    )
}

/// Emit a debug line via `SYS_RUSTOS_DEBUG_PRINT`. Always-on; equivalent to
/// the per-service `debug_line()` helpers that previously called
/// `libc::syscall`.
pub fn debug_line(message: &str) {
    let bytes = message.as_bytes();
    unsafe {
        syscall2(
            SYS_RUSTOS_DEBUG_PRINT,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
        );
        syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
    }
}
