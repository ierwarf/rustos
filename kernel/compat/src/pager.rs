//! Normal-time pager IPC dispatch and exact reply adoption.
//!
//! Exception entry publishes only fixed fault/grant authority. This worker runs
//! from the bounded nucleus housekeeping task, where allocation and endpoint
//! IPC are permitted.

use core::{
    mem::size_of,
    ptr,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use crate::ipc::{EndpointResponseTake, KernelEndpointHandle, KernelReplyHandle, endpoint};
use crate::memory as mm_api;
use crate::multitask as ps_api;
use rustos_user_abi::pager::{PagerFaultDispatchWire, PagerFaultReplyWire};
use rustos_user_abi::syscall::{
    COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_PAGERD, CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
    IPC_SERVICE_PAGERD,
};
use x86_64::{VirtAddr, structures::paging::PageTableFlags};

// Faults are pipelined per housekeeping turn, not drained one at a time. At a
// budget of one, every fault cost a full turn, so early user work serialised
// behind housekeeping: the turn always found more work, the task never idled,
// and load-based wake placement stopped putting user tasks on that CPU at all.
// These stay small and fixed so the turn remains bounded; each dispatch binds
// its own fault slot and reply handle, so several may be in flight at once.
const DISPATCH_BUDGET: usize = 8;
const RESPONSE_BUDGET: usize = 8;
static FIRST_ANONYMOUS_FAULT_COMPLETED: AtomicBool = AtomicBool::new(false);
static COMPLETED_ANONYMOUS_FAULTS: AtomicU64 = AtomicU64::new(0);
static REJECTED_ANONYMOUS_FAULT_GRANTS: AtomicU64 = AtomicU64::new(0);
static REJECTED_ANONYMOUS_FAULT_MAPS: AtomicU64 = AtomicU64::new(0);

fn bytes_of<T>(value: &T) -> &[u8] {
    // SAFETY: protocol structures are repr(C), fully initialized, and copied
    // synchronously before this borrow ends.
    unsafe { core::slice::from_raw_parts(ptr::from_ref(value).cast(), size_of::<T>()) }
}

fn read_unaligned<T: Copy>(bytes: &[u8]) -> Option<T> {
    (bytes.len() == size_of::<T>()).then(|| {
        // SAFETY: length is exact and read_unaligned imposes no alignment
        // requirement. All protocol fields are validated after the copy.
        unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
    })
}

fn dispatch_wire(reservation: ps_api::PagerFaultReservation) -> PagerFaultDispatchWire {
    PagerFaultDispatchWire {
        request: reservation.request,
        zeroed_frame_capability: reservation.zeroed_frame_capability,
        granted_frame_rights: reservation.granted_frame_rights,
        reserved0: 0,
        reserved1: [0; 2],
    }
}

fn request_for(
    reservation: ps_api::PagerFaultReservation,
    process_id: u64,
) -> Option<CommercialMaxProtocolRequest> {
    let dispatch = dispatch_wire(reservation);
    if process_id == 0 || !dispatch.is_canonical() {
        return None;
    }
    let payload = bytes_of(&dispatch);
    let payload_len = u32::try_from(payload.len()).ok()?;
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_PAGERD;
    request.header.op = COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE;
    request.header.service_id = IPC_SERVICE_PAGERD;
    request.header.subject_pid = process_id;
    request.header.subject_tid = reservation.request.task_id;
    request.header.ticket = reservation.token;
    request.payload_len = payload_len;
    request.payload[..payload.len()].copy_from_slice(payload);
    Some(request)
}

fn frame_binding(
    reservation: ps_api::PagerFaultReservation,
) -> mm_api::frame_capability::FrameGrantBinding {
    mm_api::frame_capability::FrameGrantBinding {
        fault_token: reservation.token,
        process_generation: reservation.request.process_generation,
        mm_generation: reservation.request.mm_generation,
        vma_generation: reservation.request.vma_generation,
        pager_epoch: reservation.request.object.pager_epoch,
    }
}

fn cancel_grant(reservation: ps_api::PagerFaultReservation) {
    let _ = mm_api::frame_capability::cancel_frame_grant(
        reservation.zeroed_frame_capability,
        frame_binding(reservation),
    );
}

/// Wakes the exact blocked fault owner and queues it as the next bounded
/// synchronous handoff candidate. A pager completion can release a user lock
/// holder, so treating it as an ordinary sleeper wake can deadlock a same-class
/// spinner until a later fairness turn. The hint carries no authority: the
/// scheduler revalidates the task's runnable owner and saved continuation.
fn wake_fault_owner(task_id: u64) -> bool {
    let woke = ps_api::wake_task(task_id);
    if woke {
        let _ = ps_api::set_next_synchronous_pick_hint(task_id);
    }
    woke
}

fn finish_without_mapping(reservation: ps_api::PagerFaultReservation) {
    cancel_grant(reservation);
    let _ = ps_api::consume_pager_fault_reply(reservation.token);
    let _ = wake_fault_owner(reservation.request.task_id);
}

fn decode_reply(
    reservation: ps_api::PagerFaultReservation,
    bytes: &[u8],
) -> Option<PagerFaultReplyWire> {
    let response = read_unaligned::<CommercialMaxProtocolResponse>(bytes)?;
    if response.status != 0
        || response.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || response.header.protocol != COMMERCIAL_MAX_PROTOCOL_PAGERD
        || response.header.op != COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE
        || response.header.service_id != IPC_SERVICE_PAGERD
        || response.header.ticket != reservation.token
        || response.descriptor_count != 0
        || response.reserved0 != 0
        || response.reserved1 != 0
        || usize::try_from(response.payload_len).ok()? != size_of::<PagerFaultReplyWire>()
    {
        return None;
    }
    read_unaligned::<PagerFaultReplyWire>(&response.payload[..size_of::<PagerFaultReplyWire>()])
}

fn page_flags(rights: u32) -> PageTableFlags {
    use rustos_user_abi::pager::{VM_PROT_EXECUTE, VM_PROT_WRITE};

    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if rights & VM_PROT_WRITE != 0 {
        flags |= PageTableFlags::WRITABLE;
    }
    if rights & VM_PROT_EXECUTE == 0 {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    flags
}

fn adopt_response(reservation: ps_api::PagerFaultReservation, response: &[u8]) {
    let dispatch = dispatch_wire(reservation);
    let Some(reply) = decode_reply(reservation, response) else {
        if let Ok(claimed) = ps_api::claim_pager_fault_reply(reservation.token) {
            finish_without_mapping(claimed);
        }
        return;
    };
    if !reply.is_canonical_zeroed_for(dispatch) {
        if let Ok(claimed) = ps_api::claim_pager_fault_reply(reservation.token) {
            finish_without_mapping(claimed);
        }
        return;
    }
    let Ok(claimed) = ps_api::claim_pager_fault_reply(reservation.token) else {
        return;
    };
    let Ok(frame) = mm_api::frame_capability::take_frame_grant(
        reply.frame_capability,
        frame_binding(claimed),
        u64::from(reply.frame_rights),
    ) else {
        let rejected = REJECTED_ANONYMOUS_FAULT_GRANTS.fetch_add(1, Ordering::Relaxed) + 1;
        if rejected.is_multiple_of(16) {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-anon-grant-rejected",
                rejected,
                claimed.request.task_id,
            );
        }
        let _ = ps_api::consume_pager_fault_reply(claimed.token);
        let _ = wake_fault_owner(claimed.request.task_id);
        return;
    };
    let mapped = ps_api::with_validated_fault_address_space(claimed.request, |_, address_space| {
        address_space.map_prepared_pager_fault_frame_at(
            VirtAddr::new(claimed.request.virtual_address),
            frame.as_u64(),
            page_flags(reply.frame_rights),
        )
    })
    .is_ok_and(|result| result.is_ok());
    if !mapped {
        let rejected = REJECTED_ANONYMOUS_FAULT_MAPS.fetch_add(1, Ordering::Relaxed) + 1;
        if rejected.is_multiple_of(16) {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-anon-map-rejected",
                rejected,
                claimed.request.task_id,
            );
        }
        mm_api::phys::free_frame(frame);
    }
    let _ = ps_api::consume_pager_fault_reply(claimed.token);
    let woke = wake_fault_owner(claimed.request.task_id);
    if mapped && woke {
        let completed = COMPLETED_ANONYMOUS_FAULTS.fetch_add(1, Ordering::Relaxed) + 1;
        if completed.is_multiple_of(16) {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-anon-fault-progress",
                claimed.request.process_handle,
                claimed.request.virtual_address,
            );
        }
    }
    if mapped
        && woke
        && FIRST_ANONYMOUS_FAULT_COMPLETED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "pager-anon-first-touch-complete",
            claimed.token,
            claimed.request.task_id,
        );
    }
}

fn poll_one_response() -> usize {
    let Some(reservation) = ps_api::next_dispatched_pager_fault_response() else {
        return 0;
    };
    let reply = KernelReplyHandle::from_raw(reservation.dispatch_reply_handle);
    match endpoint::take_response_detailed(reply, 0) {
        Ok(EndpointResponseTake::Pending) => 0,
        Ok(EndpointResponseTake::Response((bytes, handles))) => {
            if handles.is_empty() {
                adopt_response(reservation, &bytes);
            } else if let Ok(claimed) = ps_api::claim_pager_fault_reply(reservation.token) {
                finish_without_mapping(claimed);
            }
            1
        }
        Ok(EndpointResponseTake::Error { .. }) | Err(_) => {
            if let Ok(claimed) = ps_api::claim_pager_fault_reply(reservation.token) {
                finish_without_mapping(claimed);
            }
            1
        }
    }
}

fn dispatch_one() -> usize {
    let Some(reservation) = ps_api::take_next_pager_fault_for_dispatch() else {
        return 0;
    };
    let Ok(vma) = ps_api::validate_fault_request(reservation.request) else {
        if let Ok(claimed) = ps_api::claim_pager_fault_reply(reservation.token) {
            finish_without_mapping(claimed);
        }
        return 1;
    };
    let Some(request) = request_for(reservation, vma.process_id) else {
        if let Ok(claimed) = ps_api::claim_pager_fault_reply(reservation.token) {
            finish_without_mapping(claimed);
        }
        return 1;
    };
    let Some(endpoint_handle) = KernelEndpointHandle::from_identity(
        reservation.endpoint.slot,
        reservation.endpoint.generation,
    ) else {
        if let Ok(claimed) = ps_api::claim_pager_fault_reply(reservation.token) {
            finish_without_mapping(claimed);
        }
        return 1;
    };
    // The dispatcher runs on the housekeeping task, so this call carries that
    // task's scheduling-context custody and wakes the parked receiver. A bare
    // `enqueue_call` publishes neither: the receiver never observes the request,
    // and its reply panics the completion path for completing "without
    // scheduling-context custody". Billing the faulting task instead was tried
    // and double-faults on kernel stack exhaustion, because that task is
    // already BlockedOnPager.
    match crate::user::syscall::linux::ipc_ops::enqueue_call_and_wake(
        endpoint_handle,
        bytes_of(&request),
    ) {
        Ok(reply) => {
            if ps_api::bind_pager_fault_dispatch_reply(reservation.token, reply.raw()).is_err() {
                let _ = ps_api::current_task_id()
                    .map(|dispatcher| endpoint::cancel_call(reply, dispatcher));
                if let Ok(claimed) = ps_api::claim_pager_fault_reply(reservation.token) {
                    finish_without_mapping(claimed);
                }
            }
        }
        Err(_) => {
            if let Ok(claimed) = ps_api::claim_pager_fault_reply(reservation.token) {
                finish_without_mapping(claimed);
            }
        }
    }
    1
}

/// Runs a fixed amount of normal-time pager work from nucleus housekeeping.
pub fn service_deferred_work() -> usize {
    let mut work = 0;
    for _ in 0..RESPONSE_BUDGET {
        work += poll_one_response();
    }
    for _ in 0..DISPATCH_BUDGET {
        work += dispatch_one();
    }
    work
}

#[cfg(test)]
mod tests {
    use super::request_for;
    use crate::multitask::{PagerFaultReservation, PagerFaultState};
    use rustos_user_abi::pager::{
        PAGER_FAULT_ABI_VERSION, PagerEndpointCapabilityWire, PagerFaultRequestWire,
        PagerObjectIdentityWire, VM_ACCESS_READ, VM_OBJECT_ANONYMOUS, VM_PROT_READ,
    };
    use rustos_user_abi::syscall::{
        COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE, COMMERCIAL_MAX_PROTOCOL_PAGERD, IPC_SERVICE_PAGERD,
    };

    fn reservation() -> PagerFaultReservation {
        PagerFaultReservation {
            token: 19,
            state: PagerFaultState::DispatchedToPager,
            request: PagerFaultRequestWire {
                version: PAGER_FAULT_ABI_VERSION,
                access: VM_ACCESS_READ,
                fault_token: 19,
                process_handle: 3,
                process_generation: 5,
                task_id: 7,
                task_generation: 8,
                mm_generation: 11,
                vma_generation: 13,
                virtual_address: 0x4000,
                deadline_ns: 17,
                scheduling_domain: 23,
                charge_token: 29,
                object: PagerObjectIdentityWire {
                    object_type: VM_OBJECT_ANONYMOUS,
                    rights: VM_PROT_READ,
                    slot: 31,
                    generation: 37,
                    pager_epoch: 41,
                    backing_generation: 43,
                    ..PagerObjectIdentityWire::default()
                },
                ..PagerFaultRequestWire::default()
            },
            endpoint: PagerEndpointCapabilityWire {
                slot: 43,
                generation: 47,
                rights: 1,
            },
            zeroed_frame_capability: 53,
            granted_frame_rights: VM_PROT_READ,
            dispatch_reply_handle: 0,
        }
    }

    #[test]
    fn pager_dispatch_envelope_binds_exact_fault_subject_and_ticket() {
        let reservation = reservation();
        let request = request_for(reservation, 59).expect("dispatch request");
        assert_eq!(request.header.protocol, COMMERCIAL_MAX_PROTOCOL_PAGERD);
        assert_eq!(request.header.op, COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE);
        assert_eq!(request.header.service_id, IPC_SERVICE_PAGERD);
        assert_eq!(request.header.subject_pid, 59);
        assert_eq!(request.header.subject_tid, reservation.request.task_id);
        assert_eq!(request.header.ticket, reservation.token);
    }
}
