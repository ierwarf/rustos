//! Thin convenience wrappers around the RustOS native IPC syscalls. Services
//! can call these directly without going through libc.

use alloc::vec::Vec;
use rustos_user_abi::syscall::{
    RustosIpcValidateServiceOwnerArgs, IPC_ABI_VERSION, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_IPC_CALL, SYS_RUSTOS_IPC_CALL_BOUNDED, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_RECV_WITH_SENDER,
    SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER,
};

use crate::syscall::{syscall0, syscall1, syscall2, syscall3, syscall5, syscall6};

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

/// Prove that a kernel-stamped sender PID currently owns `service_id`.
///
/// This is the only accepted shortcut for a delegated service-to-service
/// request whose subject differs from the immediate sender. Callers must not
/// cache a successful result across requests or service restart.
#[inline]
pub fn validate_service_owner(service_id: u64, sender_pid: u64) -> i64 {
    let args = RustosIpcValidateServiceOwnerArgs {
        abi_version: IPC_ABI_VERSION,
        service_id,
        process_id: sender_pid,
        ..RustosIpcValidateServiceOwnerArgs::default()
    };
    unsafe {
        syscall1(
            SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER,
            (&args as *const RustosIpcValidateServiceOwnerArgs) as u64,
        )
    }
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

/// Call one endpoint with an explicit finite deadline. `timeout_ms` must be
/// non-zero and no larger than the kernel's global service ceiling.
#[inline]
pub unsafe fn call_bounded(
    endpoint: u64,
    request: *const u8,
    request_len: usize,
    reply: *mut u8,
    reply_capacity: usize,
    timeout_ms: u64,
) -> i64 {
    syscall6(
        SYS_RUSTOS_IPC_CALL_BOUNDED,
        endpoint,
        request as u64,
        request_len as u64,
        reply as u64,
        reply_capacity as u64,
        timeout_ms,
    )
}

/// Receive one request together with the kernel-stamped sender identity.
/// Policy services must use this form whenever request fields claim a PID/TID.
#[inline]
pub unsafe fn recv_with_sender(
    endpoint: u64,
    request_buf: *mut u8,
    request_capacity: usize,
    reply_cap_out: *mut u64,
    sender_pid_out: *mut u64,
    sender_tid_out: *mut u64,
) -> i64 {
    syscall6(
        SYS_RUSTOS_IPC_RECV_WITH_SENDER,
        endpoint,
        request_buf as u64,
        request_capacity as u64,
        reply_cap_out as u64,
        sender_pid_out as u64,
        sender_tid_out as u64,
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
    let mut line = Vec::from(message.as_bytes());
    line.push(b'\n');
    unsafe {
        syscall2(
            SYS_RUSTOS_DEBUG_PRINT,
            line.as_ptr() as u64,
            line.len() as u64,
        );
    }
}
