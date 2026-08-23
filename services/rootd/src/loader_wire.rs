//! Const loader request/response templates and bounded byte helpers.
//!
//! Rootd initializes the complete fixed wire record before filling an exact
//! operation; no stale field from a prior supervisor turn may be published.

use super::*;

pub(super) const fn empty_loader_spawn_request() -> LoaderSpawnRequest {
    LoaderSpawnRequest {
        version: 0,
        op: 0,
        flags: 0,
        console_session: 0,
        weight_micros: 0,
        target_pid: 0,
        target_tid: 0,
        exec_ticket: 0,
        exec_path_len: 0,
        argv_count: 0,
        env_count: 0,
        argv_bytes_len: 0,
        env_bytes_len: 0,
        requester_pid: 0,
        scheduling_context: RustosSchedulingContextAuthority {
            token: 0,
            policy: RustosSchedulingContextPolicy {
                abi_version: SCHEDULING_CONTEXT_POLICY_ABI_UNSET,
                refill_capacity: 0,
                criticality: 0,
                flags: 0,
                cpu_mask: 0,
                budget_ns: 0,
                period_ns: 0,
                domain: 0,
                policy_epoch: 0,
                timeout_endpoint_cap: 0,
                reserved0: 0,
                reserved1: 0,
            },
        },
        exec_path: [0; LOADER_SPAWN_EXEC_PATH_CAPACITY],
        argv_bytes: [0; LOADER_SPAWN_ARG_BYTES],
        env_bytes: [0; LOADER_SPAWN_ENV_BYTES],
    }
}

pub(super) const fn empty_loader_spawn_response() -> LoaderSpawnResponse {
    LoaderSpawnResponse {
        version: 0,
        op: 0,
        status: 0,
        pid: 0,
        reserved0: 0,
    }
}

pub(super) fn contains_nul(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

pub(super) fn copy_bytes(src: &[u8], dest: &mut [u8]) {
    dest[..src.len()].copy_from_slice(src);
}
