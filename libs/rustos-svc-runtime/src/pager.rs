//! Pagerd's fixed fault-rendezvous syscall wrappers.

use crate::syscall::syscall1;
use rustos_user_abi::pager::{PagerFaultDispatchWire, PagerFaultReplyWire};
use rustos_user_abi::syscall::{
    RustosPagerFaultReplyArgs, RustosPagerFaultWaitArgs, PAGER_FAULT_RENDEZVOUS_ABI_VERSION,
    SYS_RUSTOS_PAGER_FAULT_REPLY, SYS_RUSTOS_PAGER_FAULT_WAIT,
};

/// Wait for one kernel-stamped, worker-bound anonymous page-fault dispatch.
#[inline]
pub unsafe fn fault_wait(dispatch_out: &mut PagerFaultDispatchWire) -> i64 {
    let args = RustosPagerFaultWaitArgs {
        abi_version: PAGER_FAULT_RENDEZVOUS_ABI_VERSION,
        flags: 0,
        reserved0: 0,
        dispatch_out: (dispatch_out as *mut PagerFaultDispatchWire) as u64,
    };
    syscall1(
        SYS_RUSTOS_PAGER_FAULT_WAIT,
        (&args as *const RustosPagerFaultWaitArgs) as u64,
    )
}

/// Return pagerd's exact response for a dispatch previously received by this
/// worker. The kernel rejects stale or cross-worker tokens.
#[inline]
pub unsafe fn fault_reply(reply: PagerFaultReplyWire) -> i64 {
    let args = RustosPagerFaultReplyArgs {
        abi_version: PAGER_FAULT_RENDEZVOUS_ABI_VERSION,
        flags: 0,
        reserved0: 0,
        reply,
    };
    syscall1(
        SYS_RUSTOS_PAGER_FAULT_REPLY,
        (&args as *const RustosPagerFaultReplyArgs) as u64,
    )
}
