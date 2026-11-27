//! Objectless lock-free-of-slab rendezvous slots for register-sized RPCs (`<= 16` bytes).
//!
//! - **Owner:** `kernel-ipc-runtime` owns virtual reply tokens, state machine
//!   transitions, and teardown discovery; `kernel-ps` treats the token cleanly
//!   as an opaque `u64` handle.
//! - **Boundary:** tokens have nonzero 40-bit generation (`bits[24..64]`), an
//!   8-bit slot index (`bits[16..24]`), and zero low 16 bits (`(token & 0xffff) == 0`),
//!   making every valid virtual token strictly disjoint from `REPLIES` slab handles
//!   (where low 16 bits hold `slot_index + 1 in 1..=512`).
//! - **Lifecycle:** `Idle -> RequestReady -> RequestTaken -> ResponseReady | ErrorReady -> Idle`.
//!   Retirement advances the 40-bit generation and permanently retires the slot at
//!   generation wrap (`generation == 0`) so a virtual token value is never reissued.
//! - **Concurrency:** lock order is `IpcEndpoint -> IpcReply` (`LockClass::IpcReply`
//!   on each cache-padded slot). Cancellation drops the slot lock before acquiring
//!   `ENDPOINTS` to recheck `fast_reply` publication, then re-acquires and
//!   re-validates the exact token generation.
//! - **Failure:** saturation across all 256 virtual slots returns `None` from
//!   reservation so callers transparently fall back to the `REPLIES` slab.
//! - **Forbidden:** no slab handle aliasing (low 16 bits must remain zero),
//!   no token reuse after retirement or rollback, no `REPLIES` slab allocation
//!   on virtual reservation, and no holding a virtual slot lock across
//!   `ENDPOINTS` cleanup.
//! - **Evidence:** `ipc-register-rendezvous/IpcRegisterRendezvous`,
//!   `virtual_rendezvous_tests`, and the `register-ipc` KVM bench probes.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use nucleus_core::util::cache_padded::CachePadded;
use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS, TrackedSpinLock, current_cpu_index};

use super::{
    CancelledCall, CancelledCallDisposition, ENDPOINTS, EndpointOwner, EndpointWakeSet,
    FastEndpointReceived, FastEndpointResponseTake, FastEndpointRollback, FastReplyCompletion,
    IpcError, KernelReplyHandle, ReplyCompletion, ReplySchedulingContextCustody,
};

/// Maximum payload carried by an objectless register rendezvous slot.
pub const VIRTUAL_MAX_BYTES: usize = rustos_user_abi::syscall::IPC_REGISTER_BYTES;
/// Number of cache-padded virtual rendezvous slots. Must remain <= 256 so the
/// index fits inside bits `[16..24]`.
pub const VIRTUAL_SLOT_COUNT: usize = 256;

const SLOT_SHIFT: u32 = 16;
const GEN_SHIFT: u32 = 24;
const SLOT_INDEX_MASK: u64 = 0xff;
const MAX_VIRTUAL_GENERATION: u64 = u64::MAX >> GEN_SHIFT;
const BITMAP_WORDS: usize = VIRTUAL_SLOT_COUNT / 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VirtualCallState {
    RequestReady,
    RequestTaken,
    ResponseReady,
    ErrorReady,
}

struct VirtualSlotData {
    occupied: bool,
    used: bool,
    generation: u64,
    endpoint_id: u64,
    receiver_owner: Option<EndpointOwner>,
    caller_process_id: u64,
    caller_task_id: u64,
    receiver_task_id: u64,
    receiver_request_capacity: usize,
    caller_response_capacity: usize,
    request_len: u8,
    response_len: u8,
    state: VirtualCallState,
    terminal_error: Option<IpcError>,
    scheduling_context: Option<ReplySchedulingContextCustody>,
    request: [u8; VIRTUAL_MAX_BYTES],
    response: [u8; VIRTUAL_MAX_BYTES],
}

impl VirtualSlotData {
    const fn new() -> Self {
        Self {
            occupied: false,
            used: false,
            generation: 1,
            endpoint_id: 0,
            receiver_owner: None,
            caller_process_id: 0,
            caller_task_id: 0,
            receiver_task_id: 0,
            receiver_request_capacity: 0,
            caller_response_capacity: 0,
            request_len: 0,
            response_len: 0,
            state: VirtualCallState::RequestReady,
            terminal_error: None,
            scheduling_context: None,
            request: [0; VIRTUAL_MAX_BYTES],
            response: [0; VIRTUAL_MAX_BYTES],
        }
    }

    #[inline]
    const fn token(&self, index: usize) -> u64 {
        (self.generation << GEN_SHIFT) | ((index as u64) << SLOT_SHIFT)
    }

    #[inline]
    fn retire_generation(&mut self) {
        self.generation = if self.generation == MAX_VIRTUAL_GENERATION {
            0
        } else {
            self.generation + 1
        };
    }

    fn reset_retired(&mut self, index: usize) {
        self.occupied = false;
        self.used = false;
        self.scheduling_context = None;
        self.terminal_error = None;
        self.retire_generation();
        clear_occupied_bit(index);
    }
}

static VIRTUAL_SLOTS: [CachePadded<TrackedSpinLock<VirtualSlotData, { LockClass::IpcReply as u8 }>>;
    VIRTUAL_SLOT_COUNT] = [const {
    CachePadded::new(TrackedSpinLock::new(VirtualSlotData::new()))
}; VIRTUAL_SLOT_COUNT];

static VIRTUAL_OCCUPIED: [AtomicU64; BITMAP_WORDS] =
    [const { AtomicU64::new(0) }; BITMAP_WORDS];

static VIRTUAL_NEXT_HINTS: [CachePadded<AtomicUsize>; MAX_TRACKED_CPUS] =
    [const { CachePadded::new(AtomicUsize::new(0)) }; MAX_TRACKED_CPUS];

#[inline]
pub const fn is_virtual_reply_token(raw: u64) -> bool {
    raw != 0 && (raw & 0xffff) == 0 && (raw >> GEN_SHIFT) != 0
}

#[inline]
fn decode_token(raw: u64) -> Option<(usize, u64)> {
    if !is_virtual_reply_token(raw) {
        return None;
    }
    let index = ((raw >> SLOT_SHIFT) & SLOT_INDEX_MASK) as usize;
    let generation = raw >> GEN_SHIFT;
    Some((index, generation))
}

#[inline]
fn set_occupied_bit(index: usize) {
    let word = index / 64;
    let bit = index % 64;
    VIRTUAL_OCCUPIED[word].fetch_or(1_u64 << bit, Ordering::Release);
}

#[inline]
fn clear_occupied_bit(index: usize) {
    let word = index / 64;
    let bit = index % 64;
    VIRTUAL_OCCUPIED[word].fetch_and(!(1_u64 << bit), Ordering::Release);
}

#[inline]
pub(super) fn supports_virtual_fast_call(request_len: usize, response_capacity: usize) -> bool {
    request_len > 0 && request_len <= VIRTUAL_MAX_BYTES && response_capacity <= VIRTUAL_MAX_BYTES
}

/// Reserves an idle virtual rendezvous slot under `IpcEndpoint -> IpcReply`.
/// Returns `None` if every virtual slot is occupied or permanently retired,
/// letting the caller fall back seamlessly to `REPLIES`.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_reserve(
    endpoint_id: u64,
    receiver_owner: Option<EndpointOwner>,
    caller_process_id: u64,
    caller_task_id: u64,
    receiver_task_id: u64,
    receiver_request_capacity: usize,
    caller_response_capacity: usize,
    request: &[u8],
    scheduling_context: &mut Option<ReplySchedulingContextCustody>,
) -> Option<u64> {
    if !supports_virtual_fast_call(request.len(), caller_response_capacity) {
        return None;
    }
    let cpu = current_cpu_index() % MAX_TRACKED_CPUS;
    let hint = VIRTUAL_NEXT_HINTS[cpu].load(Ordering::Relaxed) & (VIRTUAL_SLOT_COUNT - 1);
    let start_word = hint / 64;
    let start_bit = hint % 64;

    for word_offset in 0..BITMAP_WORDS {
        let word_idx = (start_word + word_offset) % BITMAP_WORDS;
        let occupied_word = VIRTUAL_OCCUPIED[word_idx].load(Ordering::Relaxed);
        let mut free_mask = !occupied_word;
        if word_offset == 0 && start_bit != 0 {
            free_mask = free_mask.rotate_right(start_bit as u32);
        }
        while free_mask != 0 {
            let tz = free_mask.trailing_zeros() as usize;
            let bit = if word_offset == 0 && start_bit != 0 {
                (start_bit + tz) % 64
            } else {
                tz
            };
            let idx = word_idx * 64 + bit;
            {
                let mut slot = VIRTUAL_SLOTS[idx].lock();
                if !slot.occupied && slot.generation != 0 {
                    slot.occupied = true;
                    slot.used = false;
                    slot.endpoint_id = endpoint_id;
                    slot.receiver_owner = receiver_owner;
                    slot.caller_process_id = caller_process_id;
                    slot.caller_task_id = caller_task_id;
                    slot.receiver_task_id = receiver_task_id;
                    slot.receiver_request_capacity = receiver_request_capacity;
                    slot.caller_response_capacity = caller_response_capacity;
                    slot.request_len = request.len() as u8;
                    slot.response_len = 0;
                    slot.state = VirtualCallState::RequestReady;
                    slot.terminal_error = None;
                    slot.scheduling_context = scheduling_context.take();
                    slot.request[..request.len()].copy_from_slice(request);
                    set_occupied_bit(idx);
                    let token = slot.token(idx);
                    VIRTUAL_NEXT_HINTS[cpu].store((idx + 1) & (VIRTUAL_SLOT_COUNT - 1), Ordering::Relaxed);
                    return Some(token);
                }
            }
            free_mask &= free_mask - 1;
        }
    }
    None
}

pub(super) fn rollback(
    endpoint_id: u64,
    reply_id: u64,
    caller_task_id: u64,
    receiver_task_id: u64,
) -> Result<(FastEndpointRollback, usize), IpcError> {
    let (idx, expected_gen) = decode_token(reply_id).ok_or(IpcError::InvalidHandle)?;
    let mut slot = VIRTUAL_SLOTS[idx].lock();
    if !slot.occupied || slot.generation != expected_gen {
        return Err(IpcError::InvalidHandle);
    }
    if slot.used
        || slot.endpoint_id != endpoint_id
        || slot.caller_task_id != caller_task_id
        || slot.receiver_task_id != receiver_task_id
        || slot.state != VirtualCallState::RequestReady
    {
        return Err(IpcError::PermissionDenied);
    }
    let capacity = slot.receiver_request_capacity;
    let scheduling_context = slot.scheduling_context.take();
    slot.reset_retired(idx);
    Ok((
        FastEndpointRollback {
            receiver_task_id,
            scheduling_context,
        },
        capacity,
    ))
}

pub(super) fn take_request<const N: usize>(
    reply_id: u64,
    endpoint_id: u64,
    receiver_task_id: u64,
    request_capacity: usize,
) -> Result<Option<FastEndpointReceived<N>>, IpcError> {
    let (idx, expected_gen) = decode_token(reply_id).ok_or(IpcError::PermissionDenied)?;
    let mut slot = VIRTUAL_SLOTS[idx].lock();
    if !slot.occupied
        || slot.generation != expected_gen
        || slot.endpoint_id != endpoint_id
        || slot.state != VirtualCallState::RequestReady
    {
        return Err(IpcError::InvalidHandle);
    }
    if slot.receiver_task_id != receiver_task_id {
        return Ok(None);
    }
    let req_len = usize::from(slot.request_len);
    if req_len > request_capacity || req_len > N {
        return Err(IpcError::BufferTooSmall);
    }
    let mut request = [0; N];
    request[..req_len].copy_from_slice(&slot.request[..req_len]);
    slot.state = VirtualCallState::RequestTaken;
    Ok(Some(FastEndpointReceived {
        reply: KernelReplyHandle::from_raw(reply_id),
        caller_process_id: slot.caller_process_id,
        caller_task_id: slot.caller_task_id,
        request_len: req_len,
        request,
        spill: None,
    }))
}

pub(super) fn complete_reply(
    reply_id: u64,
    receiver_task_id: u64,
    response: &[u8],
) -> Result<FastReplyCompletion, IpcError> {
    let (idx, expected_gen) = decode_token(reply_id).ok_or(IpcError::InvalidHandle)?;
    let mut slot = VIRTUAL_SLOTS[idx].lock();
    if !slot.occupied || slot.generation != expected_gen {
        return Err(IpcError::InvalidHandle);
    }
    if slot.used {
        return Err(IpcError::InvalidArgument);
    }
    if slot.receiver_task_id != receiver_task_id || slot.state != VirtualCallState::RequestTaken {
        return Err(IpcError::PermissionDenied);
    }
    let terminal_error =
        (response.len() > slot.caller_response_capacity).then_some(IpcError::BufferTooSmall);
    if let Some(error) = terminal_error {
        slot.response_len = 0;
        slot.terminal_error = Some(error);
        slot.state = VirtualCallState::ErrorReady;
    } else {
        slot.response[..response.len()].copy_from_slice(response);
        slot.response_len = response.len() as u8;
        slot.state = VirtualCallState::ResponseReady;
    }
    slot.used = true;
    Ok(FastReplyCompletion {
        completion: ReplyCompletion {
            caller_task_id: slot.caller_task_id,
            scheduling_context: slot.scheduling_context.take(),
        },
        terminal_error,
    })
}

pub(super) fn take_response<const N: usize>(
    reply_id: u64,
    caller_task_id: u64,
) -> Result<FastEndpointResponseTake<N>, IpcError> {
    let (idx, expected_gen) = decode_token(reply_id).ok_or(IpcError::InvalidHandle)?;
    let mut slot = VIRTUAL_SLOTS[idx].lock();
    if !slot.occupied || slot.generation != expected_gen {
        return Err(IpcError::InvalidHandle);
    }
    if slot.caller_task_id != caller_task_id {
        return Err(IpcError::PermissionDenied);
    }
    if slot.state == VirtualCallState::ErrorReady {
        let error = slot.terminal_error;
        return match error {
            Some(err) => {
                slot.reset_retired(idx);
                Ok(FastEndpointResponseTake::Error(err))
            }
            None => Err(IpcError::InvalidHandle),
        };
    }
    if slot.state != VirtualCallState::ResponseReady {
        return Ok(FastEndpointResponseTake::Pending);
    }
    let len = usize::from(slot.response_len);
    if len > N {
        slot.reset_retired(idx);
        return Err(IpcError::BufferTooSmall);
    }
    let mut response = [0; N];
    response[..len].copy_from_slice(&slot.response[..len]);
    slot.reset_retired(idx);
    Ok(FastEndpointResponseTake::Response {
        response_len: len,
        response,
    })
}

pub(super) fn fast_request_owned_by_receiver(reply_id: u64, caller_task_id: u64) -> bool {
    let Some((idx, expected_gen)) = decode_token(reply_id) else {
        return false;
    };
    let slot = VIRTUAL_SLOTS[idx].lock();
    slot.occupied
        && slot.generation == expected_gen
        && slot.caller_task_id == caller_task_id
        && slot.state != VirtualCallState::RequestReady
}

pub(super) fn received_request_retains_custody(reply_id: u64, caller_task_id: u64) -> bool {
    let Some((idx, expected_gen)) = decode_token(reply_id) else {
        return false;
    };
    let slot = VIRTUAL_SLOTS[idx].lock();
    slot.occupied
        && slot.generation == expected_gen
        && slot.caller_task_id == caller_task_id
        && slot.state != VirtualCallState::RequestReady
        && !slot.used
        && slot
            .scheduling_context
            .is_some_and(|held| held.caller_task_id() == caller_task_id)
}

pub(super) fn endpoint_receiver_process_for_reply(reply_id: u64) -> Option<u64> {
    let (idx, expected_gen) = decode_token(reply_id)?;
    let slot = VIRTUAL_SLOTS[idx].lock();
    if !slot.occupied || slot.generation != expected_gen {
        return None;
    }
    match slot.receiver_owner {
        Some(EndpointOwner::Process(process_id)) => Some(process_id),
        Some(EndpointOwner::Task(_)) | None => None,
    }
}

pub(super) fn reply_diagnostic(reply_id: u64) -> Option<(u64, u64, u64)> {
    let (idx, expected_gen) = decode_token(reply_id)?;
    let slot = VIRTUAL_SLOTS[idx].lock();
    if !slot.occupied || slot.generation != expected_gen {
        return None;
    }
    let receiver = match slot.receiver_owner {
        Some(EndpointOwner::Process(id)) => id,
        Some(EndpointOwner::Task(id)) => id | (1 << 63),
        None => 0,
    };
    let response_present = matches!(
        slot.state,
        VirtualCallState::ResponseReady | VirtualCallState::ErrorReady
    );
    let bits = (u64::from(slot.used) << 1)
        | (u64::from(response_present) << 2)
        | (1 << 4);
    Some((slot.endpoint_id, receiver, bits))
}

#[cfg(test)]
pub(super) fn has_scheduling_context(reply_id: u64) -> Result<bool, IpcError> {
    let (idx, expected_gen) = decode_token(reply_id).ok_or(IpcError::InvalidHandle)?;
    let slot = VIRTUAL_SLOTS[idx].lock();
    if !slot.occupied || slot.generation != expected_gen {
        return Err(IpcError::InvalidHandle);
    }
    Ok(slot.scheduling_context.is_some())
}

pub(super) fn cancel_call(reply_id: u64, caller_task_id: u64) -> Result<CancelledCall, IpcError> {
    let (idx, expected_gen) = decode_token(reply_id).ok_or(IpcError::InvalidHandle)?;
    let (endpoint_id, request_was_published) = {
        let slot = VIRTUAL_SLOTS[idx].lock();
        if !slot.occupied || slot.generation != expected_gen {
            return Err(IpcError::InvalidHandle);
        }
        if slot.caller_task_id != caller_task_id {
            return Err(IpcError::InvalidArgument);
        }
        (
            slot.endpoint_id,
            slot.state == VirtualCallState::RequestReady,
        )
    };
    let mut was_queued = false;
    let _ = ENDPOINTS.with_mut(endpoint_id, |endpoint| {
        if endpoint.fast_reply == Some(reply_id) {
            endpoint.fast_reply.take();
            was_queued = request_was_published;
        }
    });
    let mut slot = VIRTUAL_SLOTS[idx].lock();
    if !slot.occupied || slot.generation != expected_gen {
        return Err(IpcError::InvalidHandle);
    }
    let scheduling_context = slot.scheduling_context.take();
    slot.reset_retired(idx);
    Ok(CancelledCall {
        transfers: Vec::new(),
        disposition: if was_queued {
            CancelledCallDisposition::Queued
        } else {
            CancelledCallDisposition::InFlight
        },
        scheduling_context,
    })
}

pub(super) fn cancel_calls_for_task(
    task_id: u64,
    release_scheduling_context: &mut impl FnMut(KernelReplyHandle, ReplySchedulingContextCustody),
) -> usize {
    let mut cancelled = 0usize;
    for word_idx in 0..BITMAP_WORDS {
        let mut word = VIRTUAL_OCCUPIED[word_idx].load(Ordering::Acquire);
        while word != 0 {
            let bit = word.trailing_zeros() as usize;
            let idx = word_idx * 64 + bit;
            word &= word - 1;
            let candidate_reply = {
                let slot = VIRTUAL_SLOTS[idx].lock();
                if slot.occupied && slot.caller_task_id == task_id {
                    Some(slot.token(idx))
                } else {
                    None
                }
            };
            let Some(reply_id) = candidate_reply else {
                continue;
            };
            match cancel_call(reply_id, task_id) {
                Ok(call) => {
                    if let Some(custody) = call.scheduling_context {
                        release_scheduling_context(KernelReplyHandle::from_raw(reply_id), custody);
                    }
                    cancelled += 1;
                }
                Err(IpcError::InvalidHandle) => continue,
                Err(error) => panic!(
                    "published task-owned virtual call cancellation failed task={task_id} reply={reply_id} error={error:?}"
                ),
            }
        }
    }
    cancelled
}

pub(super) fn occupied_count_estimate() -> usize {
    VIRTUAL_OCCUPIED
        .iter()
        .map(|w| w.load(Ordering::Relaxed).count_ones() as usize)
        .sum()
}

pub(super) fn fail_endpoints(
    owned_contains: &impl Fn(u64) -> bool,
    err: IpcError,
    wake_set: &mut EndpointWakeSet,
) {
    for word_idx in 0..BITMAP_WORDS {
        let mut word = VIRTUAL_OCCUPIED[word_idx].load(Ordering::Acquire);
        while word != 0 {
            let bit = word.trailing_zeros() as usize;
            let idx = word_idx * 64 + bit;
            word &= word - 1;
            let mut slot = VIRTUAL_SLOTS[idx].lock();
            if !slot.occupied || slot.used || !owned_contains(slot.endpoint_id) {
                continue;
            }
            slot.state = VirtualCallState::ErrorReady;
            slot.terminal_error = Some(err);
            slot.used = true;
            wake_set.push_caller(slot.caller_task_id);
            if let Some(custody) = slot.scheduling_context.take() {
                wake_set.push_scheduling_context(
                    KernelReplyHandle::from_raw(slot.token(idx)),
                    custody,
                );
            }
        }
    }
}

#[cfg(test)]
pub(super) fn clear_all() {
    for (idx, lock) in VIRTUAL_SLOTS.iter().enumerate() {
        let mut slot = lock.lock();
        slot.occupied = false;
        slot.used = false;
        slot.generation = 1;
        slot.endpoint_id = 0;
        slot.receiver_owner = None;
        slot.caller_process_id = 0;
        slot.caller_task_id = 0;
        slot.receiver_task_id = 0;
        slot.receiver_request_capacity = 0;
        slot.caller_response_capacity = 0;
        slot.request_len = 0;
        slot.response_len = 0;
        slot.state = VirtualCallState::RequestReady;
        slot.terminal_error = None;
        slot.scheduling_context = None;
        clear_occupied_bit(idx);
    }
}
