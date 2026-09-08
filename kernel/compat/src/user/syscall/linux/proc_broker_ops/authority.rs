//! Process-broker authority capture across service and raw-lock handoffs.
//!
//! - **Owner:** `loaderd` owns image admission; `kernel-compat` snapshots its
//!   exact `PROCESS_LOADER` capability for prepare-handle mechanism.
//! - **Boundary:** scheduler current identity is sampled once before direct
//!   service IPC and before any prepare-registry raw critical section.
//! - **Lifecycle:** capture subject → validate the loader-owned request → retain
//!   the original PID through prepare mutation or rejection.
//! - **Concurrency:** prepare performs no reverse service handoff, so a racing
//!   procd exec cannot close a `procd -> loaderd -> procd` wait cycle.
//! - **Failure:** missing capability or owner mismatch returns a bounded error
//!   without publishing authority.
//! - **Forbidden:** no wildcard PID, reverse procd call, post-lock current-task
//!   lookup, or image-format policy implementation in ring0.
//! - **Evidence:** `loader-request-authority`,
//!   `deferred-process-activation`, and source conformance.

use super::*;

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
