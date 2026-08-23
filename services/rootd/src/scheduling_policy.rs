//! Rootd-owned launch scheduling policy and grant publication.
//!
//! Executable paths select only immutable manifest policy. Ring0 seals the
//! exact requester, rootd epoch, path, and policy into a bounded one-shot
//! authority; this module never accepts caller-selected budget enlargement.

use super::service_manifest::{
    BOOTSTRAP_MANIFEST, POST_INIT_MANIFEST, USER_WORKLOAD_SCHEDULING_POLICY,
};
use super::*;

pub(super) fn issue_scheduling_context_authority(
    request: &CommercialMaxProtocolRequest,
    sender: IpcSenderIdentity,
) -> Result<RustosSchedulingContextAuthority, i32> {
    let path_len = request.path_len as usize;
    if path_len == 0 || path_len > request.path.len() || request.path[..path_len].contains(&0) {
        return Err(22);
    }
    let policy = scheduling_context_policy_for_exec(&request.path[..path_len]);
    register_scheduling_context_authority(&request.path[..path_len], sender.pid, policy)
}

pub(super) fn register_scheduling_context_authority(
    exec_path: &[u8],
    requester_pid: u64,
    policy: RustosSchedulingContextPolicy,
) -> Result<RustosSchedulingContextAuthority, i32> {
    let args = RustosSchedulingContextGrantBrokerArgs {
        abi_version: rustos_user_abi::syscall::SCHEDULING_CONTEXT_POLICY_ABI_VERSION,
        requester_pid,
        exec_path_ptr: exec_path.as_ptr() as u64,
        exec_path_len: exec_path.len() as u64,
        policy,
        ..RustosSchedulingContextGrantBrokerArgs::default()
    };
    let token = syscall1(
        SYS_RUSTOS_SCHEDULING_CONTEXT_GRANT_BROKER,
        (&args as *const RustosSchedulingContextGrantBrokerArgs) as u64,
    );
    if token <= 0 {
        return Err(if token < 0 { (-token) as i32 } else { 5 });
    }
    Ok(RustosSchedulingContextAuthority {
        token: token as u64,
        policy,
    })
}

pub(super) fn scheduling_context_policy_for_exec(path: &[u8]) -> RustosSchedulingContextPolicy {
    let normalized = trim_nul(path.strip_prefix(b"/").unwrap_or(path));
    BOOTSTRAP_MANIFEST
        .iter()
        .find(|service| normalized == trim_nul(service.exec_path))
        .map(|service| service.scheduling)
        .or_else(|| {
            POST_INIT_MANIFEST
                .iter()
                .find(|service| normalized == trim_nul(service.exec_path))
                .map(|service| service.scheduling)
        })
        // Generic applications share the immutable user workload domain.
        // No package, desktop entry, or caller argument may enlarge it.
        .unwrap_or(USER_WORKLOAD_SCHEDULING_POLICY)
}
