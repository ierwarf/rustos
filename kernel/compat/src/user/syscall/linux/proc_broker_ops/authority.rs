//! Process-broker authority capture across service and raw-lock handoffs.
//!
//! - **Owner:** `kernel-compat` owns the capability snapshot; `procd` owns
//!   process-prepare policy.
//! - **Boundary:** scheduler current identity is sampled once before direct
//!   service IPC and before any prepare-registry raw critical section.
//! - **Lifecycle:** capture subject → request exact procd policy → validate the
//!   reply → retain the original PID through prepare mutation or rejection.
//! - **Concurrency:** a blocking service handoff may schedule another task, so
//!   post-handoff authority never re-reads transient scheduler current state.
//! - **Failure:** missing capability, malformed reply, subject mismatch, or
//!   owner mismatch returns a bounded error without publishing authority.
//! - **Forbidden:** no wildcard PID, post-lock current-task lookup, or policy
//!   implementation in ring0.
//! - **Evidence:** `loader-request-authority`,
//!   `deferred-process-activation`, and source conformance.

use super::*;
use rustos_user_abi::syscall::{
    COMMERCIAL_MAX_PROCD_OP_PROCESS_PREPARE, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_PROCD, CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
};

/// Capture the exact loader subject before any prepare-registry raw lock.
///
/// The capability snapshot and returned PID are one lookup transaction.
/// Callers retain the PID across allocation, nested service work, and raw
/// critical sections; scheduler observation is intentionally unavailable
/// while preemption is disabled.
pub(super) fn current_loader_process_id() -> Option<u64> {
    ipc_ops::current_process_with_service_capability(IPC_SERVICE_CAP_PROCESS_LOADER)
}

pub(super) fn prepare_owned_by(state: &ProcPrepareState, loader_pid: u64) -> bool {
    state.owner_pid == loader_pid
}

// RING3-MIGRATION-REFERENCE START: capability-broker exception: procd owns
// process-prepare admission policy. Ring0 keeps the capability-gated broker
// handle table and calls procd before allocating privileged prepare state.
pub(super) fn procd_process_prepare_policy(format: u16) -> Result<u64, i64> {
    let Some(snapshot) = multitask::current_user_snapshot() else {
        return Err(LINUX_EPERM);
    };
    // Retain the exact syscall-entry subject across the cross-service policy
    // round trip. Re-reading "current" after a direct IPC handoff can observe
    // the policy server's transient scheduler slot and must never transfer
    // ownership of a privileged prepare handle.
    let owner_pid = snapshot.process_id();
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_PROCD;
    request.header.op = COMMERCIAL_MAX_PROCD_OP_PROCESS_PREPARE;
    request.header.service_id = rustos_user_abi::syscall::IPC_SERVICE_PROCD;
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    request.arg0 = u64::from(format);
    let response = ipc_ops::call_service_endpoint_with_class(
        rustos_user_abi::syscall::IPC_SERVICE_PROCD,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::BootControl,
    )?;
    if response.len() != core::mem::size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    ipc_ops::validate_commercial_response_envelope(&request, &response)?;
    if response.payload_len != 0
        || response.descriptor_count != 1
        || response.value0 != u64::from(format)
        || response.value1 != PROC_BROKER_ABI_VERSION as u64
    {
        return Err(LINUX_EINVAL);
    }
    if response.status == 0 {
        Ok(owner_pid)
    } else {
        Err(response.status.unsigned_abs() as i64)
    }
}
// RING3-MIGRATION-REFERENCE END: procd-owned process-prepare admission substrate exception.
