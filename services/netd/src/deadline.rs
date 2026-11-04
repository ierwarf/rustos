//! Request deadline arithmetic: the caller owns one absolute monotonic end; netd only ever tightens it.
//!
//! - **Owner:** The caller supplies the absolute end; `netd` clamps it once at admission by request class.
//! - **Boundary:** `deadline_ns` is untrusted and must be nonzero and in the future.
//! - **Lifecycle:** Stamped at admission, subdivided for retries and provider waits, never extended.
//! - **Concurrency:** Pure functions over an immutable request; no shared state.
//! - **Failure:** Expired or zero deadlines return `ETIMEDOUT` before any reservation or provider start.
//! - **Forbidden:** No service-local timeout that outlives the caller's end; no widening by class cap.
//! - **Evidence:** `local_socket_poll_tests` deadline cases and the `netd-admission-deadline-freshen` mutants.

use super::*;

/// The caller owns one absolute `CLOCK_MONOTONIC` end instant.  Netd may
/// subdivide the remaining time for a provider operation or retry, but never
/// begins a fresh service-local timeout for a request that has already waited
/// in one of its bounded queues.
pub(super) fn request_deadline_remaining_ns_at(request: &NetdIpcRequest, now_ns: u64) -> Result<u64, i32> {
    if request.deadline_ns == 0 || now_ns >= request.deadline_ns {
        return Err(libc::ETIMEDOUT);
    }
    Ok(request.deadline_ns - now_ns)
}

pub(super) fn request_deadline_remaining_ns(request: &NetdIpcRequest) -> Result<u64, i32> {
    request_deadline_remaining_ns_at(request, rustos_svc_runtime::syscall::monotonic_nanos())
}

/// Netd derives its local authority once, at admission. The caller's wire end
/// stays immutable except for this tightening: a class cap never gives a
/// queued operation a later deadline than the caller originally supplied.
pub(super) fn request_deadline_class_cap(request: &NetdIpcRequest) -> Duration {
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET | NETD_IPC_OP_POLL_BATCH => {
            Duration::from_millis(rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS)
        }
        NETD_IPC_OP_DVM_SESSION => Duration::from_millis(
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
        ),
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET
        | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR
        | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT
        | SYSCALL_OFFLOAD_OP_LINUX_DUP
        | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
        | NETD_IPC_OP_REF_ACK => Duration::from_millis(
            rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
        ),
        _ => Duration::from_millis(rustos_user_abi::performance::IPC_BULK_DATA_HARD_LIMIT_MS),
    }
}

pub(super) fn clamp_request_deadline_at(request: &mut NetdIpcRequest, now_ns: u64) -> Result<(), i32> {
    request_deadline_remaining_ns_at(request, now_ns)?;
    let cap_ns = request_deadline_class_cap(request)
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64;
    request.deadline_ns = request.deadline_ns.min(now_ns.saturating_add(cap_ns));
    request_deadline_remaining_ns_at(request, now_ns).map(|_| ())
}

pub(super) fn capped_request_wait_at(
    request: &NetdIpcRequest,
    now_ns: u64,
    cap: Duration,
) -> Result<Duration, i32> {
    let remaining_ns = request_deadline_remaining_ns_at(request, now_ns)?;
    let cap_ns = cap.as_nanos().min(u128::from(u64::MAX)) as u64;
    Ok(Duration::from_nanos(remaining_ns.min(cap_ns)))
}

pub(super) fn capped_request_wait(request: &NetdIpcRequest, cap: Duration) -> Result<Duration, i32> {
    capped_request_wait_at(request, rustos_svc_runtime::syscall::monotonic_nanos(), cap)
}

pub(super) fn sleep_within_request_deadline(request: &NetdIpcRequest, cap: Duration) -> Result<(), i32> {
    thread::sleep(capped_request_wait(request, cap)?);
    Ok(())
}
