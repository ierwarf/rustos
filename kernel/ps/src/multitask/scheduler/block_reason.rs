//! Exact scheduler wait identities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask) enum BlockReason {
    None,
    Generic,
    EndpointReceive(u64),
    EndpointReply(u64),
    /// A fixed pager-fault slot token. Exception ingress arms this before any
    /// normal-time IPC dispatcher touches endpoint or reply registries.
    PagerFault(u64),
    /// A pagerd worker is parked on the fixed fault-rendezvous mailbox. This
    /// intentionally carries no endpoint identity: the mailbox is a single,
    /// capability-gated kernel object rather than generic IPC state.
    PagerService,
}
