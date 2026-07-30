//! Exact bounded loader activation-batch wire contract.
//!
//! - **Owner:** rustos-user-abi owns the wire; loaderd and kernel-compat own
//!   their respective sender binding and one-shot capability transitions.
//! - **Boundary:** initd-selected PIDs cross loaderd into the process broker.
//! - **Lifecycle:** validate one unique cohort, bind its requester, atomically
//!   publish every member, and consume every matching capability.
//! - **Concurrency:** the wire is immutable during one bounded IPC/broker call.
//! - **Failure:** bad shape, sender, target, or partial admission changes none.
//! - **Forbidden:** no partial activation, zero-tail alias, retry widening, or
//!   requester identity supplied without the kernel-stamped sender.
//! - **Evidence:** `atomic-process-activation-batch` and its focused tests.

/// Loaderd-only atomic publication of a bounded set of exact suspended
/// children. The kernel validates every one-shot activation capability and
/// every scheduler target before making any member runnable.
pub const SYS_RUSTOS_PROC_ACTIVATE_BATCH_BROKER: u64 = 0x5255_0047;
pub const PRODUCT_MILESTONE_INIT_IDENTITY_READY: u64 = 6;
pub const LOADER_OP_ACTIVATE: u16 = 3;
pub const LOADER_OP_ACTIVATE_BATCH: u16 = 4;
pub const LOADER_ACTIVATE_BATCH_ABI_VERSION: u16 = 1;
pub const LOADER_ACTIVATE_BATCH_MAX_TARGETS: usize = 8;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcActivateBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub target_pid: u64,
    /// Exact process that requested the corresponding deferred spawn.
    pub requester_pid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcActivateBatchBrokerArgs {
    pub abi_version: u16,
    pub target_count: u16,
    pub flags: u32,
    /// Exact process that requested every corresponding deferred spawn.
    pub requester_pid: u64,
    pub target_pids: [u64; LOADER_ACTIVATE_BATCH_MAX_TARGETS],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoaderActivateBatchRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    /// Immediate caller PID. Loaderd must bind this to the kernel-stamped IPC
    /// sender before forwarding any target identity.
    pub requester_pid: u64,
    pub target_count: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub target_pids: [u64; LOADER_ACTIVATE_BATCH_MAX_TARGETS],
}

impl Default for LoaderActivateBatchRequest {
    fn default() -> Self {
        Self {
            version: LOADER_ACTIVATE_BATCH_ABI_VERSION,
            op: LOADER_OP_ACTIVATE_BATCH,
            flags: 0,
            requester_pid: 0,
            target_count: 0,
            reserved0: 0,
            reserved1: 0,
            target_pids: [0; LOADER_ACTIVATE_BATCH_MAX_TARGETS],
        }
    }
}

impl LoaderActivateBatchRequest {
    pub const fn requester_is_exact_sender(&self, sender_pid: u64) -> bool {
        self.requester_pid != 0 && self.requester_pid == sender_pid
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoaderActivateBatchResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub activated_count: u32,
    pub reserved0: u32,
}

#[cfg(test)]
mod tests {
    use super::LoaderActivateBatchRequest;

    #[test]
    fn requester_identity_is_bound_to_the_kernel_sender() {
        let mut request = LoaderActivateBatchRequest::default();
        assert!(!request.requester_is_exact_sender(23));
        request.requester_pid = 23;
        assert!(request.requester_is_exact_sender(23));
        assert!(!request.requester_is_exact_sender(29));
    }
}
