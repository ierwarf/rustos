//! Versioned Linux and Windows CPU topology and affinity wire constants.
//!
//! - **Owner:** rustos-user-abi owns numeric operation and topology fields.
//! - **Boundary:** kernel-compat, syscalld, winsys, and ABI probes must agree.
//! - **Lifecycle:** values are immutable within the offload ABI generation.
//! - **Concurrency:** constants carry snapshots only and own no mutable state.
//! - **Failure:** reserved, stale, or duplicated values fail ABI validation.
//! - **Forbidden:** no raw APIC identity, implicit CPU-zero, or value reuse.
//! - **Evidence:** `cpu-affinity-observation`, `task-affinity-lifecycle`, and
//!   `formal/run-abi-differential.sh`.

/// Operation-specific topology stamp carried in `arg0..=arg3` for Linux
/// affinity operations:
/// `arg0=online_mask`, `arg1=online_count`, `arg2=version`,
/// `arg3=target_process_id`. `mask` carries the kernel-resolved effective
/// task mask; syscalld must not infer target ownership from a thread ID.
pub const CPU_TOPOLOGY_OBSERVATION_ABI_VERSION: u64 = 2;
pub const CPU_TOPOLOGY_MAX_LOGICAL_CPUS: u64 = 8;

pub const SYSCALL_OFFLOAD_OP_LINUX_SCHED_SETAFFINITY: u16 = 69;
pub const SYSCALL_OFFLOAD_OP_WIN32_QUERY_SYSTEM_INFORMATION: u16 = 91;
pub const SYSCALL_OFFLOAD_OP_WIN32_QUERY_PROCESS_AFFINITY: u16 = 92;
pub const SYSCALL_OFFLOAD_OP_WIN32_SET_PROCESS_AFFINITY: u16 = 93;
pub const SYSCALL_OFFLOAD_OP_WIN32_SET_THREAD_AFFINITY: u16 = 94;
pub const SYSCALL_OFFLOAD_OP_WIN32_GET_CURRENT_PROCESSOR_NUMBER: u16 = 95;

pub const WIN32_SYSTEM_INFORMATION_CLASS_BASIC: u64 = 0;
pub const WIN32_TOPOLOGY_OBSERVATION_VERSION_SHIFT: u64 = 8;
pub const WIN32_TOPOLOGY_OBSERVATION_RESERVED_SHIFT: u64 = 16;
pub const WIN32_TOPOLOGY_OBSERVATION_FIELD_MASK: u64 = 0xff;

#[cfg(test)]
mod tests {
    use super::SYSCALL_OFFLOAD_OP_LINUX_SCHED_SETAFFINITY;
    use crate::syscall::SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET;

    #[test]
    fn affinity_mutation_owns_the_next_unique_policy_operation() {
        assert_eq!(
            SYSCALL_OFFLOAD_OP_LINUX_SCHED_SETAFFINITY,
            SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET + 1
        );
    }
}
