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
}
