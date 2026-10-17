//! Reply-owned IPC priority donation.
//!
//! - **Owner:** one live reply capability owns at most one donation record;
//!   the independent donation ledger owns bounded metadata, while publication
//!   retains original donor/root/receiver loans through each effect.
//! - **Boundary:** the compat IPC boundary supplies exact reply, donor, and
//!   receiver identities. Only the exact IPC_RECV worker becomes the target;
//!   no process-wide worker selection supplies authority.
//! - **Lifecycle:** reserve on call admission, bind to the exact receiving
//!   worker at receive commit, then revoke on the first terminal event
//!   (reply, cancel, donor exit, worker exit, or process teardown).
//! - **Concurrency:** `SchedulerDonation` serializes reply-edge mutation;
//!   dispatch observes only its receiver-slot inheritance counter. The
//!   selected worker's direct handoff is enqueued on that worker's own CPU
//!   dispatch policy, never on a system-wide hint.
//! - **Failure:** exhausted capacity fails admission closed instead of
//!   silently dropping the inheritance edge, and an unknown or retired
//!   identity is rejected rather than repaired.
//! - **Forbidden:** no process-wide or permanent boost, no donation without a
//!   live reply capability, and no target rebinding outside the bind
//!   transaction.
//! - **Evidence:** `ipc-priority-inheritance`.

/// A bounded priority-inheritance edge for one synchronous IPC reply
/// capability.  The edge lasts only while that reply capability is live: the
/// caller donates its effective scheduling class to the receiver, and the
/// class therefore propagates through a nested synchronous call chain.
///
/// Keeping this in the scheduler rather than in the IPC object store avoids a
/// dependency from `kernel-ipc-runtime` back into `kernel-ps`.  Its lifetime
/// is still tied to the reply capability by the compat IPC boundary.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum IpcDonationTarget {
    AwaitingReceiver,
    BoundWorker(u64),
}

/// Which key space a donation ledger entry belongs to.
///
/// The ledger is keyed by a bare `u64`, and two subsystems mint keys into it:
/// generic IPC reply handles, `(generation << 16) | (index + 1)`, and pager
/// fault tokens, `(generation << 8) | slot`. Both are generational packings
/// over disjoint object tables, but their *numbers* overlap - the smallest
/// reply handle `0x1_0001` is also the fault token for slot 1 at generation
/// 256, and slot 1 is reused constantly. Without a namespace an aliased
/// lookup settles another subsystem's donation, which surfaces as
/// "cancelled reply returned stale scheduling-context custody". The key is
/// therefore the pair, never the number alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask) enum DonationNamespace {
    IpcReply,
    PagerFault,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct IpcPriorityDonation {
    pub(super) reply: u64,
    pub(super) namespace: DonationNamespace,
    pub(super) donor_task_id: u64,
    /// Root scheduling-context owner propagated across nested passive-server
    /// calls. The immediate donor remains the reply wake owner.
    pub(super) context_owner_task_id: u64,
    pub(super) context_owner_slot: usize,
    pub(super) priority_donated: bool,
    pub(super) custody_active: bool,
    /// Only an exact receive binds a worker; an attached reply can wait without
    /// promoting or selecting any runnable process peer.
    pub(super) target: IpcDonationTarget,
}
