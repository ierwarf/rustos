//! Lock-free reply metadata used only to choose the authoritative lock order.

use core::sync::atomic::{AtomicU64, Ordering};

use super::{MAX_REPLY_OBJECTS, REPLIES, ReplyObject, slab};

/// Advisory message-id mirror indexed by the reply slot. A stale reply handle
/// may observe a later generation's value, so consumers must still validate
/// the full handle and message id under `REPLIES`; this mirror only removes the
/// redundant first reply-slot acquisition needed to choose lock order.
static REPLY_MESSAGE_IDS: [AtomicU64; MAX_REPLY_OBJECTS] =
    [const { AtomicU64::new(0) }; MAX_REPLY_OBJECTS];

pub(super) fn insert_reply_object(reply_object: ReplyObject) -> Result<u64, ReplyObject> {
    let message_id = reply_object.message_id;
    let reply_id = REPLIES.insert(reply_object)?;
    if let Some(index) = slab::slot_index::<MAX_REPLY_OBJECTS>(reply_id) {
        REPLY_MESSAGE_IDS[index].store(message_id, Ordering::Release);
    }
    Ok(reply_id)
}

#[inline]
pub(super) fn published_reply_message_id(reply_id: u64) -> Option<u64> {
    let index = slab::slot_index::<MAX_REPLY_OBJECTS>(reply_id)?;
    let message_id = REPLY_MESSAGE_IDS[index].load(Ordering::Acquire);
    (message_id != 0).then_some(message_id)
}
