//! Thin convenience wrappers around the RustOS native IPC syscalls. Services
//! can call these directly without going through libc.

use alloc::{format, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use rustos_user_abi::syscall::{
    IpcReplyRecvResultKind, IpcReplyRecvWithSenderArgs, ProductExecutableSnapshotEvidence,
    RustosIpcValidateServiceOwnerArgs, IPC_ABI_VERSION, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_IPC_CALL, SYS_RUSTOS_IPC_CALL_BOUNDED, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_RECV_WITH_SENDER,
    SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER,
    SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER, SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER,
    SYS_RUSTOS_PRODUCT_EXECUTABLE_SNAPSHOT_EVIDENCE, SYS_RUSTOS_PRODUCT_MILESTONE,
};

use crate::syscall::{syscall0, syscall1, syscall2, syscall3, syscall5, syscall6};

const EARLY_REPLY_FAILURE_SAMPLES: u64 = 4;

/// Lifetime-bounded reporting for replies whose caller has already cancelled
/// or retired its one-shot capability.
///
/// A failure remains observable immediately, but a cancellation storm cannot
/// turn the synchronous debug port into a second workload.  Each independent
/// service lane emits the first four failures and then only power-of-two
/// cumulative totals: at most 68 records even if its counter reaches `u64::MAX`.
pub struct ReplyFailureDiagnostics {
    total: AtomicU64,
}

impl ReplyFailureDiagnostics {
    pub const fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
        }
    }

    pub fn record(&self, service: &str, operation: &str, errno: i64) {
        let total = self
            .total
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_or(u64::MAX, |previous| previous + 1);
        if reply_failure_diagnostic_due(total) {
            debug_line(&format!(
                "{service}: {operation} reply failed errno={errno} total={total}"
            ));
        }
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Acquire)
    }
}

impl Default for ReplyFailureDiagnostics {
    fn default() -> Self {
        Self::new()
    }
}

fn reply_failure_diagnostic_due(total: u64) -> bool {
    total <= EARLY_REPLY_FAILURE_SAMPLES || total.is_power_of_two()
}

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

/// Receive immediately if a request is queued, otherwise return `-EAGAIN`.
/// Services with an independent event source use this before a capability
/// wait, avoiding a poll timer while still servicing endpoint traffic.
#[inline]
pub unsafe fn try_recv_with_sender(
    endpoint: u64,
    request_buf: *mut u8,
    request_capacity: usize,
    reply_cap_out: *mut u64,
    sender_pid_out: *mut u64,
    sender_tid_out: *mut u64,
) -> i64 {
    syscall6(
        SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER,
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

/// Complete `reply_cap` and enter the next authenticated receive without an
/// intermediate return to ring3.
///
/// A non-negative result is the next request length.  An ordinary negative
/// errno means the reply was not consumed.  Use
/// [`reply_recv_committed_errno`] to identify a receive failure after the
/// reply commit; the old reply capability must never be retried in that case.
#[inline]
pub unsafe fn reply_recv_with_sender(
    endpoint: u64,
    reply_cap: u64,
    response: *const u8,
    response_len: usize,
    request_buf: *mut u8,
    request_capacity: usize,
    next_reply_cap_out: *mut u64,
    sender_pid_out: *mut u64,
    sender_tid_out: *mut u64,
) -> i64 {
    let args = IpcReplyRecvWithSenderArgs {
        abi_version: IPC_ABI_VERSION,
        endpoint,
        reply_cap,
        response_ptr: response as u64,
        response_len: response_len as u64,
        request_ptr: request_buf as u64,
        request_capacity: request_capacity as u64,
        next_reply_cap_ptr: next_reply_cap_out as u64,
        sender_pid_ptr: sender_pid_out as u64,
        sender_tid_ptr: sender_tid_out as u64,
        ..IpcReplyRecvWithSenderArgs::default()
    };
    syscall1(
        SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER,
        (&args as *const IpcReplyRecvWithSenderArgs) as u64,
    )
}

/// Returns the underlying receive errno only when `result` proves that the
/// reply phase committed.  `None` means either success or a pre-commit errno.
#[inline]
pub fn reply_recv_committed_errno(result: i64) -> Option<i64> {
    match reply_recv_result_kind(result) {
        IpcReplyRecvResultKind::PostCommitError(errno) => Some(errno),
        _ => None,
    }
}

/// Classify success, retry-safe pre-commit failure, completed-reply failure,
/// and impossible wire values without collapsing a protocol violation into a
/// retryable result.
#[inline]
pub const fn reply_recv_result_kind(result: i64) -> IpcReplyRecvResultKind {
    rustos_user_abi::syscall::ipc_reply_recv_result_kind(result)
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

/// Emit a fixed-name, kernel-timestamped product acceptance milestone.
///
/// The marker is diagnostic evidence only. It does not grant or transfer any
/// service authority.
#[inline]
pub fn product_milestone(milestone: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { syscall3(SYS_RUSTOS_PRODUCT_MILESTONE, milestone, arg0, arg1) }
}

/// Ask ring0 to stamp a fully identified DVM executable snapshot witness.
/// The kernel accepts it only from the live Vfsd endpoint and independently
/// supplies the provider identity and endpoint generation.
#[inline]
pub fn product_executable_snapshot_evidence(evidence: &ProductExecutableSnapshotEvidence) -> i64 {
    unsafe {
        syscall1(
            SYS_RUSTOS_PRODUCT_EXECUTABLE_SNAPSHOT_EVIDENCE,
            evidence as *const ProductExecutableSnapshotEvidence as u64,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{reply_failure_diagnostic_due, reply_recv_committed_errno, reply_recv_result_kind};
    use rustos_user_abi::syscall::IpcReplyRecvResultKind;

    #[test]
    fn reply_failure_diagnostics_are_lifetime_bounded() {
        for total in 1..=4 {
            assert!(reply_failure_diagnostic_due(total));
        }
        assert!(!reply_failure_diagnostic_due(5));
        assert!(reply_failure_diagnostic_due(8));
        assert!(!reply_failure_diagnostic_due(9));
        assert!(reply_failure_diagnostic_due(1_u64 << 63));
        assert!(!reply_failure_diagnostic_due(u64::MAX));
    }

    #[test]
    fn reply_recv_phase_tag_cannot_alias_linux_errno() {
        assert_eq!(reply_recv_committed_errno(0), None);
        assert_eq!(reply_recv_committed_errno(-1), None);
        assert_eq!(reply_recv_committed_errno(-4095), None);
        assert_eq!(reply_recv_committed_errno(-4096), None);
        assert_eq!(reply_recv_committed_errno(-4097), Some(1));
        assert_eq!(reply_recv_committed_errno(-4221), Some(125));
        assert_eq!(reply_recv_committed_errno(-8191), Some(4095));
        assert_eq!(reply_recv_committed_errno(-8192), None);
        assert_eq!(reply_recv_committed_errno(i64::MIN), None);
        assert_eq!(
            reply_recv_result_kind(-1),
            IpcReplyRecvResultKind::PreCommitError(1)
        );
        assert_eq!(
            reply_recv_result_kind(-4096),
            IpcReplyRecvResultKind::Invalid
        );
    }
}
