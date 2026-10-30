//! Common service IPC over the opt-in fastcall boundary, without libc.
//! Exact short shapes use registers; other messages/deadlines/handles retain
//! the original buffer handlers in the same syscall, never a post-commit retry.

mod fastcall;

use alloc::{format, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use rustos_user_abi::syscall::{
    IPC_ABI_VERSION, IpcReplyRecvResultKind, IpcReplyRecvWithSenderArgs,
    IpcRecvWithHandlesAndSenderArgs, IpcReplyRecvWithHandlesAndSenderArgs,
    SYS_RUSTOS_IPC_RECV_WITH_HANDLES_AND_SENDER, SYS_RUSTOS_IPC_REPLY_RECV_WITH_HANDLES_AND_SENDER,
    ProductExecutableSnapshotEvidence, RustosIpcValidateServiceOwnerArgs, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_IPC_CALL, SYS_RUSTOS_IPC_CALL_BOUNDED, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_RECV_WITH_SENDER,
    SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER,
    SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER, SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER,
    SYS_RUSTOS_PRODUCT_EXECUTABLE_SNAPSHOT_EVIDENCE, SYS_RUSTOS_PRODUCT_MILESTONE,
};

use crate::syscall::{syscall0, syscall1, syscall2, syscall3, syscall5, syscall6};

const EARLY_REPLY_FAILURE_SAMPLES: u64 = 4;
static DEBUG_OUTPUT_ABANDONED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Diagnostic suffixes abandoned after bounded transport retries. This is not
/// kernel admission loss: rejected bytes remain owned by the userspace writer.
pub fn debug_output_abandoned_bytes() -> u64 {
    DEBUG_OUTPUT_ABANDONED_BYTES.load(Ordering::Relaxed)
}

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

/// # Safety
/// Input/output ranges must remain valid for the call and obey the endpoint's
/// one-shot protocol. The reply may alias the request after it has been packed.
#[inline]
pub unsafe fn call(
    endpoint: u64,
    request: *const u8,
    request_len: usize,
    reply: *mut u8,
    reply_capacity: usize,
) -> i64 {
    if let Some(result) =
        unsafe { fastcall::call(endpoint, request, request_len, reply, reply_capacity) }
    {
        return result;
    }
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
///
/// # Safety
/// The receive range and three disjoint output words must remain writable
/// throughout the call; the sender identity is kernel-authenticated.
#[inline]
pub unsafe fn recv_with_sender(
    endpoint: u64,
    request_buf: *mut u8,
    request_capacity: usize,
    reply_cap_out: *mut u64,
    sender_pid_out: *mut u64,
    sender_tid_out: *mut u64,
) -> i64 {
    if let Some(result) = unsafe {
        fastcall::receive(
            endpoint,
            request_buf,
            request_capacity,
            reply_cap_out,
            sender_pid_out,
            sender_tid_out,
        )
    } {
        return result;
    }
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

/// Receive descriptors and the kernel-authenticated sender in one transaction.
/// This explicit form never falls back to a handle-unaware register receive.
///
/// # Safety
/// All outputs named by `args` must remain writable for the blocking call and
/// disjoint from the live argument block. The fd array holds i32 values and its
/// returned count is a u16. On error no descriptor is published.
#[inline]
// SAFETY: callers own the live output buffers described by this raw ABI block.
pub unsafe fn recv_with_handles_and_sender(args: &IpcRecvWithHandlesAndSenderArgs) -> i64 {
    syscall1(SYS_RUSTOS_IPC_RECV_WITH_HANDLES_AND_SENDER, args as *const _ as u64)
}

/// Commit a byte reply, then receive bytes, descriptors and stamped sender.
/// The result uses the same explicit precommit/postcommit error partition as
/// `reply_recv_with_sender`; a postcommit failure never permits reply retry.
///
/// # Safety
/// Response bytes stay readable, receive outputs stay writable and disjoint,
/// and the ABI argument block stays live for the entire blocking call.
#[inline]
// SAFETY: callers retain the response and every receive output for the call.
pub unsafe fn reply_recv_with_handles_and_sender(args: &IpcReplyRecvWithHandlesAndSenderArgs) -> i64 {
    syscall1(SYS_RUSTOS_IPC_REPLY_RECV_WITH_HANDLES_AND_SENDER, args as *const _ as u64)
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
///
/// # Safety
/// `response` is readable for `response_len` bytes (may be null when empty),
/// and the caller owns the one-shot reply capability.
#[inline]
pub unsafe fn reply(reply_cap: u64, response: *const u8, response_len: usize) -> i64 {
    if let Some(result) = unsafe { fastcall::reply(reply_cap, response, response_len) } {
        return result;
    }
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
///
/// # Safety
/// The response and request ranges remain live and disjoint; the three output
/// words remain exclusively writable and the caller owns `reply_cap`.
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
    if let Some(result) = unsafe {
        fastcall::reply_receive(
            endpoint,
            reply_cap,
            response,
            response_len,
            request_buf,
            request_capacity,
            next_reply_cap_out,
            sender_pid_out,
            sender_tid_out,
        )
    } {
        return result;
    }
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

/// One turn of a single-endpoint service loop: reply to `reply_cap` and receive
/// the next request in a single syscall.
///
/// Returns the next request length, or a negative value when no request was
/// received (the caller then falls back to a plain receive). The reply is
/// retried once through the plain reply path only when the fused call proved it
/// did not consume the capability; a failure after the reply committed is never
/// retried. A result outside the native partition is a protocol violation.
///
/// # Safety
/// `response` and `request_buf` must stay live and disjoint for the call, and
/// every output pointer must be exclusively borrowed by the caller.
#[inline]
pub unsafe fn reply_then_recv(
    endpoint: u64,
    reply_cap: u64,
    response: *const u8,
    response_len: usize,
    request_buf: *mut u8,
    request_capacity: usize,
    next_reply_cap_out: *mut u64,
    sender_pid_out: *mut u64,
    sender_tid_out: *mut u64,
    on_failure: fn(&'static str),
) -> i64 {
    let result = unsafe {
        reply_recv_with_sender(
            endpoint,
            reply_cap,
            response,
            response_len,
            request_buf,
            request_capacity,
            next_reply_cap_out,
            sender_pid_out,
            sender_tid_out,
        )
    };
    match reply_recv_result_kind(result) {
        IpcReplyRecvResultKind::Success => {}
        IpcReplyRecvResultKind::PostCommitError(_) => on_failure("reply-recv receive failed"),
        IpcReplyRecvResultKind::PreCommitError(_) => {
            on_failure("reply-recv reply failed");
            // Only the pre-commit partition reaches here, so the old capability
            // and the immutable response are live for exactly one recovery try.
            if unsafe { reply(reply_cap, response, response_len) } < 0 {
                on_failure("reply-recv recovery failed");
            }
        }
        IpcReplyRecvResultKind::Invalid => {
            panic!("invalid reply-recv result outside native ABI partition")
        }
    }
    result
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

/// `debug_line` for a `fn(&'static str)` callback slot (see [`reply_then_recv`]).
pub fn debug_line_static(message: &'static str) {
    debug_line(message);
}

/// Emit a debug line via `SYS_RUSTOS_DEBUG_PRINT`. Always-on; equivalent to
/// the per-service `debug_line()` helpers that previously called
/// `libc::syscall`.
pub fn debug_line(message: &str) {
    let mut line = Vec::from(message.as_bytes());
    line.push(b'\n');
    debug_bytes(&line);
}

/// Bounded suffix-only diagnostic output; abandoned bytes are counted.
/// Unlike `debug_line`, preserves caller framing and does not allocate.
pub fn debug_bytes(bytes: &[u8]) {
    let _ = write_debug_bytes_with(
        bytes,
        |remaining| unsafe {
            syscall2(
                SYS_RUSTOS_DEBUG_PRINT,
                remaining.as_ptr() as u64,
                remaining.len() as u64,
            )
        },
        || unsafe {
            syscall0(crate::syscall::SYS_SCHED_YIELD);
        },
    );
}

fn write_debug_bytes_with(
    bytes: &[u8],
    mut write: impl FnMut(&[u8]) -> i64,
    mut yield_now: impl FnMut(),
) -> usize {
    let mut written = 0;
    for _ in 0..64 {
        if written == bytes.len() {
            break;
        }
        let result = write(&bytes[written..]);
        if result > 0 && result as usize <= bytes.len() - written {
            written += result as usize;
        } else if result != -11 {
            break;
        }
        if written != bytes.len() {
            yield_now();
        }
    }
    DEBUG_OUTPUT_ABANDONED_BYTES.fetch_add((bytes.len() - written) as u64, Ordering::Relaxed);
    written
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
    #[test]
    fn debug_output_retries_only_unaccepted_suffix_and_bounds_backpressure() {
        let mut responses = [2, -11, 3].into_iter();
        let mut offsets = alloc::vec::Vec::new();
        let mut yields = 0;
        let written = super::write_debug_bytes_with(
            b"hello",
            |remaining| {
                offsets.push(remaining.len());
                responses.next().unwrap()
            },
            || yields += 1,
        );
        assert_eq!(written, 5);
        assert_eq!(offsets, [5, 3, 3]);
        assert_eq!(yields, 2);
        assert_eq!(super::write_debug_bytes_with(b"x", |_| -11, || {}), 0);
        assert_eq!(
            super::write_debug_bytes_with(
                b"x",
                |_| 2,
                || panic!("invalid result is not retryable")
            ),
            0
        );
    }

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
    fn authenticated_handle_wrappers_do_not_use_handle_unaware_fallbacks() {
        let source = include_str!("ipc.rs").split("#[cfg(test)]").next().unwrap();
        let wrappers = source.split_once("pub unsafe fn recv_with_handles_and_sender(").unwrap().1
            .split_once("pub unsafe fn try_recv_with_sender(").unwrap().0;
        assert!(wrappers.contains("syscall1(SYS_RUSTOS_IPC_RECV_WITH_HANDLES_AND_SENDER"));
        assert!(wrappers.contains("syscall1(SYS_RUSTOS_IPC_REPLY_RECV_WITH_HANDLES_AND_SENDER"));
        assert!(!wrappers.contains("fastcall::"), "a handle-unaware fallback would lose attached authority");
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
