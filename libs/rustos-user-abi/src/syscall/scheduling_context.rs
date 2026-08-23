//! Canonical scheduling-context policy and one-shot launch authority wire ABI.
//!
//! Rootd authors policy, ring0 seals and consumes the grant, and intermediate
//! launch services may only forward these fixed-size values unchanged.

pub const SCHEDULING_CONTEXT_POLICY_ABI_VERSION: u16 = 1;
/// Zeroed loader-request storage has no scheduling-context authority until
/// rootd replaces the complete policy and token before publication.
pub const SCHEDULING_CONTEXT_POLICY_ABI_UNSET: u16 = 0;
pub const SCHEDULING_CONTEXT_CRITICALITY_USER: u8 = 0;
pub const SCHEDULING_CONTEXT_CRITICALITY_SYSTEM: u8 = 1;
pub const SCHEDULING_CONTEXT_CRITICALITY_DEADLINE: u8 = 2;
pub const SCHEDULING_CONTEXT_MAX_REFILLS: u8 = 8;
pub const SCHEDULING_CONTEXT_SNAPSHOT_ABI_VERSION: u16 = 2;
pub const SCHEDULING_CONTEXT_TIMEOUT_ACTION_NONE: u64 = 0;
pub const SCHEDULING_CONTEXT_TIMEOUT_ACTION_MISSING_HANDLER_THROTTLE: u64 = 1;
pub const SCHEDULING_CONTEXT_TIMEOUT_ACTION_STALE_HANDLER_THROTTLE: u64 = 2;
/// Copies the caller's effective scheduling-context and shared-domain ledgers.
/// This is read-only observability for acceptance probes and grants no
/// scheduling or IPC authority.
pub const SYS_RUSTOS_SCHEDULING_CONTEXT_SNAPSHOT: u64 = 0x5255_004e;

/// Rootd-authored temporal authority carried unchanged through loaderd/procd.
///
/// The kernel validates this closed wire shape again at commit; neither a
/// launch weight nor a userspace priority request can be converted into CPU
/// budget by an intermediate component.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosSchedulingContextPolicy {
    pub abi_version: u16,
    pub refill_capacity: u8,
    pub criticality: u8,
    pub flags: u32,
    pub cpu_mask: u64,
    pub budget_ns: u64,
    pub period_ns: u64,
    pub domain: u64,
    pub policy_epoch: u64,
    pub timeout_endpoint_cap: u64,
    pub reserved0: u64,
    pub reserved1: u64,
}

impl RustosSchedulingContextPolicy {
    pub const fn new(
        cpu_mask: u64,
        budget_ns: u64,
        period_ns: u64,
        refill_capacity: u8,
        criticality: u8,
        domain: u64,
        policy_epoch: u64,
    ) -> Self {
        Self {
            abi_version: SCHEDULING_CONTEXT_POLICY_ABI_VERSION,
            refill_capacity,
            criticality,
            flags: 0,
            cpu_mask,
            budget_ns,
            period_ns,
            domain,
            policy_epoch,
            timeout_endpoint_cap: 0,
            reserved0: 0,
            reserved1: 0,
        }
    }

    pub const fn is_canonical(self) -> bool {
        self.abi_version == SCHEDULING_CONTEXT_POLICY_ABI_VERSION
            && self.refill_capacity != 0
            && self.refill_capacity <= SCHEDULING_CONTEXT_MAX_REFILLS
            && self.criticality <= SCHEDULING_CONTEXT_CRITICALITY_DEADLINE
            && self.flags == 0
            && self.cpu_mask != 0
            && self.budget_ns != 0
            && self.period_ns != 0
            && self.budget_ns <= self.period_ns
            && self.domain != 0
            && self.policy_epoch != 0
            && self.reserved0 == 0
            && self.reserved1 == 0
    }
}

/// Rootd-authored policy plus the kernel-sealed one-shot authority that makes
/// it admissible. Intermediate services may inspect but cannot mint it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosSchedulingContextAuthority {
    pub token: u64,
    pub policy: RustosSchedulingContextPolicy,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosSchedulingContextGrantBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub requester_pid: u64,
    pub exec_path_ptr: u64,
    pub exec_path_len: u64,
    pub policy: RustosSchedulingContextPolicy,
}

/// Kernel-stamped accounting state for the scheduling context effective at
/// the instant of this syscall. A passive server therefore observes its
/// caller-owned context without gaining authority over that context.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosSchedulingContextSnapshot {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub executing_task_id: u64,
    pub context_owner_task_id: u64,
    pub context_identity_slot: u64,
    pub context_identity_generation: u64,
    pub domain: u64,
    pub policy_epoch: u64,
    pub budget_ns: u64,
    pub period_ns: u64,
    pub context_available_ns: u64,
    pub context_pending_refill_ns: u64,
    pub context_next_eligible_ns: u64,
    pub context_consumed_ns: u64,
    pub context_exhaustion_count: u64,
    pub context_refill_count: u64,
    pub context_overflow_merge_count: u64,
    pub timeout_fault_count: u64,
    pub timeout_fault_consumed_ns: u64,
    pub timeout_fault_budget_ns: u64,
    pub timeout_fault_period_ns: u64,
    pub timeout_fault_reply: u64,
    pub timeout_endpoint_cap: u64,
    pub timeout_fault_action: u64,
    pub domain_available_ns: u64,
    pub domain_pending_refill_ns: u64,
    pub domain_next_eligible_ns: u64,
    pub domain_consumed_ns: u64,
    pub domain_exhaustion_count: u64,
    pub domain_refill_count: u64,
    pub domain_overflow_merge_count: u64,
}

impl RustosSchedulingContextSnapshot {
    pub const fn is_canonical(self) -> bool {
        self.abi_version == SCHEDULING_CONTEXT_SNAPSHOT_ABI_VERSION
            && self.reserved0 == 0
            && self.flags == 0
            && self.executing_task_id != 0
            && self.context_owner_task_id != 0
            && self.context_identity_slot != 0
            && self.context_identity_generation != 0
            && self.domain != 0
            && self.policy_epoch != 0
            && self.budget_ns != 0
            && self.period_ns >= self.budget_ns
            && self.budget_ns >= self.context_available_ns
            && self.budget_ns >= self.domain_available_ns
            && self.timeout_fault_action <= SCHEDULING_CONTEXT_TIMEOUT_ACTION_STALE_HANDLER_THROTTLE
            && (self.timeout_fault_count != 0
                || (self.timeout_fault_consumed_ns == 0
                    && self.timeout_fault_reply == 0
                    && self.timeout_fault_action == SCHEDULING_CONTEXT_TIMEOUT_ACTION_NONE))
    }
}
