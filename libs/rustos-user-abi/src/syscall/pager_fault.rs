//! Dedicated pager-fault rendezvous syscall ABI.

/// Pagerd-only receive on the fixed fault rendezvous. The kernel returns one
/// exact pre-zeroed-frame dispatch through the supplied output pointer after
/// pagerd has registered its receive wait.
pub const SYS_RUSTOS_PAGER_FAULT_WAIT: u64 = 0x5255_0050;
/// Pagerd-only completion of one fixed fault rendezvous dispatch. This is not
/// a generic IPC reply capability and may only complete the worker-bound token
/// returned by `SYS_RUSTOS_PAGER_FAULT_WAIT`.
pub const SYS_RUSTOS_PAGER_FAULT_REPLY: u64 = 0x5255_0051;
pub const PAGER_FAULT_RENDEZVOUS_ABI_VERSION: u16 = 1;

/// Output placement for a pagerd fault-rendezvous wait. The output storage is
/// validated before the worker parks, then written only after it is woken by a
/// fixed dispatch slot.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosPagerFaultWaitArgs {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub dispatch_out: u64,
}

/// Pagerd's exact reply payload for one worker-bound rendezvous dispatch.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosPagerFaultReplyArgs {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub reply: crate::pager::PagerFaultReplyWire,
}
