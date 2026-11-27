//! Lock-free reply metadata used only to choose the authoritative lock order.

use core::sync::atomic::{AtomicU64, Ordering};

use super::{MAX_REPLY_OBJECTS, REPLIES, ReplyObject, slab};

/// Advisory message-id mirror indexed by the reply slot. A stale reply handle
/// may observe a later generation's value, so consumers must still validate
/// the full handle and message id under `REPLIES`; this mirror only removes the
/// redundant first reply-slot acquisition needed to choose lock order.
static REPLY_MESSAGE_IDS: [AtomicU64; MAX_REPLY_OBJECTS] =
    [const { AtomicU64::new(0) }; MAX_REPLY_OBJECTS];
/// Exact reply-slot handle owner & publication state mirrors.
///
/// Clearing `REPLY_READY_HANDLES[slot]` before publishing `REPLY_OWNER_HANDLES[slot]`
/// (inside `insert_reply_object`, before the request enters the endpoint queue)
/// and setting `REPLY_READY_HANDLES[slot] = reply_id` (under `Release`) before
/// dropping locks in reply/error/cancel lets `take_endpoint_response_for_wait`
/// observe known-pending live replies in ~10 cycles without double-slab locks.
/// Any stale handle or published/terminal reply misses this fast check cleanly
/// and takes the authoritative `ENDPOINT_MESSAGES` -> `REPLIES` lock path.
static REPLY_OWNER_HANDLES: [AtomicU64; MAX_REPLY_OBJECTS] =
    [const { AtomicU64::new(0) }; MAX_REPLY_OBJECTS];
static REPLY_READY_HANDLES: [AtomicU64; MAX_REPLY_OBJECTS] =
    [const { AtomicU64::new(0) }; MAX_REPLY_OBJECTS];

pub(super) fn insert_reply_object(reply_object: ReplyObject) -> Result<u64, ReplyObject> {
    let message_id = reply_object.message_id;
    // A synchronous caller has at most one outstanding fast call. Prefer its
    // stable slot instead of rotating through the reply pool and writing the
    // shared allocation cursor on every round trip. The hint is not authority:
    // collisions and exhausted generations retain the bounded full-pool scan.
    let hint = reply_object
        .fast_frame
        .as_ref()
        .map(|frame| frame.caller_task_id);
    let reply_id = match hint {
        Some(hint) => REPLIES.insert_with_hint(reply_object, hint)?,
        None => REPLIES.insert(reply_object)?,
    };
    if let Some(index) = slab::slot_index::<MAX_REPLY_OBJECTS>(reply_id) {
        REPLY_READY_HANDLES[index].store(0, Ordering::Release);
        REPLY_OWNER_HANDLES[index].store(reply_id, Ordering::Release);
        REPLY_MESSAGE_IDS[index].store(message_id, Ordering::Release);
    }
    Ok(reply_id)
}

/// Publishes a fast-call reply object (message id 0) built inside its slot, so
/// the ~600-byte frame is written once. `None` means the pool is full.
pub(super) fn insert_fast_reply_object(
    caller_task_id: u64, build: impl FnOnce() -> ReplyObject,
) -> Option<u64> {
    let reply_id = REPLIES.insert_with_hint_built(caller_task_id, build)?;
    if let Some(index) = slab::slot_index::<MAX_REPLY_OBJECTS>(reply_id) {
        REPLY_READY_HANDLES[index].store(0, Ordering::Release);
        REPLY_OWNER_HANDLES[index].store(reply_id, Ordering::Release);
        REPLY_MESSAGE_IDS[index].store(0, Ordering::Release);
    }
    Some(reply_id)
}

#[inline]
pub(super) fn note_reply_published(reply_id: u64) {
    if let Some(index) = slab::slot_index::<MAX_REPLY_OBJECTS>(reply_id) {
        REPLY_READY_HANDLES[index].store(reply_id, Ordering::Release);
    }
}

#[inline]
pub(super) fn clear_reply_publication(reply_id: u64) {
    if let Some(index) = slab::slot_index::<MAX_REPLY_OBJECTS>(reply_id) {
        let _ = REPLY_OWNER_HANDLES[index].compare_exchange(
            reply_id,
            0,
            Ordering::Release,
            Ordering::Relaxed,
        );
        REPLY_READY_HANDLES[index].store(reply_id, Ordering::Release);
        let _ = REPLY_MESSAGE_IDS[index];
    }
}

#[inline]
pub(super) fn reply_is_known_pending(reply_id: u64) -> bool {
    let Some(index) = slab::slot_index::<MAX_REPLY_OBJECTS>(reply_id) else {
        return false;
    };
    REPLY_MESSAGE_IDS[index].load(Ordering::Acquire) != 0
        && REPLY_OWNER_HANDLES[index].load(Ordering::Acquire) == reply_id
        && REPLY_READY_HANDLES[index].load(Ordering::Acquire) != reply_id
}

#[inline]
pub(super) fn published_reply_message_id(reply_id: u64) -> Option<u64> {
    let index = slab::slot_index::<MAX_REPLY_OBJECTS>(reply_id)?;
    let message_id = REPLY_MESSAGE_IDS[index].load(Ordering::Acquire);
    (message_id != 0).then_some(message_id)
}
