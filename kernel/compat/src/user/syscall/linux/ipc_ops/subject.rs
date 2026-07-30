//! Syscall-subject capture for service capability decisions.
//!
//! - **Owner:** `kernel-compat` owns the live service-endpoint capability
//!   snapshot; policy remains in the named service.
//! - **Boundary:** scheduler current identity is sampled before the registry
//!   raw lock and returned only when the same PID owns the live capability.
//! - **Lifecycle:** validate capability → capture PID → lock registry →
//!   validate live owner → retain or reject the exact subject.
//! - **Concurrency:** callers carry the returned PID across blocking IPC and
//!   later raw critical sections instead of re-reading scheduler current state.
//! - **Failure:** zero capability, missing user subject, or stale/foreign
//!   endpoint returns `None` without manufacturing authority.
//! - **Forbidden:** no wildcard identity and no scheduler lookup while a raw
//!   service registry guard is live.
//! - **Evidence:** `service-call-authority` and `loader-request-authority`.

use super::*;

pub(crate) fn current_process_has_service_capability(capability: u64) -> bool {
    current_process_with_service_capability(capability).is_some()
}

/// Return the exact syscall subject whose live endpoint owns `capability`.
pub(crate) fn current_process_with_service_capability(capability: u64) -> Option<u64> {
    if capability == 0 {
        return None;
    }
    let process_id = multitask::current_user_process_id()?;
    let _registry = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    current_process_has_service_capability_locked(capability, process_id).then_some(process_id)
}
