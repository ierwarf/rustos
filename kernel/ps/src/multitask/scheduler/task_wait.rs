//! Scheduler adapters for owner-generation-bound wait arms and identities.

use super::runqueue;
use super::BlockReason;

/// Encodes a block reason for the per-slot wait payload.
#[inline]
pub(super) fn encoded_reason(reason: BlockReason) -> (u8, u64) {
    match reason {
        BlockReason::None => (runqueue::wait::REASON_NONE, 0),
        BlockReason::Generic => (runqueue::wait::REASON_GENERIC, 0),
        BlockReason::EndpointReceive(endpoint) => {
            (runqueue::wait::REASON_ENDPOINT_RECEIVE, endpoint)
        }
        BlockReason::EndpointReply(reply) => (runqueue::wait::REASON_ENDPOINT_REPLY, reply),
        BlockReason::PagerFault(token) => (runqueue::wait::REASON_PAGER_FAULT, token),
        BlockReason::PagerService => (runqueue::wait::REASON_PAGER_SERVICE, 0),
    }
}
