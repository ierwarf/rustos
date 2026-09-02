//! Owner: compat owns normal-time adoption; pagerd owns mapping policy.
//! Boundary: one worker-bound reply consumes one fixed fault and frame grant.
//! Lifecycle: claim, validate, map or reject, release donation, then wake.
//! Concurrency: token generation, worker identity, and charge token are exact.
//! Failure: stale grants or failed maps revoke authority before owner wakeup.
//! Forbidden: housekeeping dispatch, generic reply caps, and retained grants.
//! Evidence: PagerFaultSlotLifecycle, PagerFrameGrantLifecycle, focused tests.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::memory as mm_api;
use crate::multitask as ps_api;
use rustos_user_abi::pager::{PagerFaultDispatchWire, PagerFaultReplyWire};
use x86_64::{VirtAddr, structures::paging::PageTableFlags};

static FIRST_ANONYMOUS_FAULT_COMPLETED: AtomicBool = AtomicBool::new(false);
static COMPLETED_ANONYMOUS_FAULTS: AtomicU64 = AtomicU64::new(0);
static REJECTED_ANONYMOUS_FAULT_GRANTS: AtomicU64 = AtomicU64::new(0);
static REJECTED_ANONYMOUS_FAULT_MAPS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn dispatch_wire(reservation: ps_api::PagerFaultReservation) -> PagerFaultDispatchWire {
    PagerFaultDispatchWire {
        request: reservation.request,
        zeroed_frame_capability: reservation.zeroed_frame_capability,
        granted_frame_rights: reservation.granted_frame_rights,
        reserved0: 0,
        reserved1: [0; 2],
    }
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

/// Wakes only the task still blocked on this exact fault token and publishes a
/// real reply-handoff token. A delayed pager reply cannot wake a later wait.
fn wake_fault_owner(fault_token: u64, task_id: u64) -> bool {
    ps_api::complete_pager_fault_wake_handoff(fault_token, task_id)
}

fn finish_without_mapping(reservation: ps_api::PagerFaultReservation) {
    cancel_grant(reservation);
    let _ = ps_api::consume_pager_fault_reply(reservation.token);
    let _ = wake_fault_owner(reservation.token, reservation.request.task_id);
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

/// Completes a reply received through the fixed pagerd rendezvous. The worker
/// identity check happens before this function claims any mapping authority;
/// generic IPC reply handles are intentionally not accepted here.
pub(crate) fn adopt_rendezvous_reply(
    reservation: ps_api::PagerFaultReservation,
    reply: PagerFaultReplyWire,
    pager_task_id: u64,
) -> Result<(), ps_api::PagerFaultSlotError> {
    let dispatch = dispatch_wire(reservation);
    if !reply.is_canonical_zeroed_for(dispatch) {
        let claimed = ps_api::claim_pager_fault_reply_from_pager(reservation.token, pager_task_id)?;
        finish_without_mapping(claimed);
        return Err(ps_api::PagerFaultSlotError::Malformed);
    }
    let claimed = ps_api::claim_pager_fault_reply_from_pager(reservation.token, pager_task_id)?;
    complete_claimed_reply(claimed, reply);
    Ok(())
}

/// Rejects a dispatched rendezvous request when pagerd's prevalidated output
/// buffer became unusable. Releasing the exact owner-bound slot wakes the
/// faulting task without mapping a frame, rather than leaving it stranded.
pub(crate) fn reject_rendezvous_dispatch(
    reservation: ps_api::PagerFaultReservation,
    pager_task_id: u64,
) -> Result<(), ps_api::PagerFaultSlotError> {
    let claimed = ps_api::claim_pager_fault_reply_from_pager(reservation.token, pager_task_id)?;
    finish_without_mapping(claimed);
    Ok(())
}

fn complete_claimed_reply(claimed: ps_api::PagerFaultReservation, reply: PagerFaultReplyWire) {
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
        // The reservation still owns its preallocated grant when pagerd's
        // reply cannot claim it. Return that exact authority to the reserve;
        // otherwise a malformed reply would leak both a grant slot and one of
        // the bounded exception-time frames.
        cancel_grant(claimed);
        let _ = ps_api::consume_pager_fault_reply(claimed.token);
        let _ = wake_fault_owner(claimed.token, claimed.request.task_id);
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
    // A successful grant claim permanently transfers one wired reserve frame
    // into the target address space (or frees it after a rejected mapping).
    // Replace exactly that consumed frame before handing execution back to the
    // fault owner. This is ordinary pager syscall context, not exception
    // entry, and it closes the reserve lifecycle without running unrelated
    // housekeeping on either handoff path. Deferring this solely to the
    // housekeeping task lets a sustained fault-owner <-> pagerd handoff chain
    // consume all 64 frames and turn a valid non-present fault into SIGSEGV.
    let _ = mm_api::frame_capability::replenish_pager_fault_frames(1);
    let woke = wake_fault_owner(claimed.token, claimed.request.task_id);
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
            // ORDERING: AcqRel publishes exactly one first-touch milestone;
            // failed Acquire observes the worker that already published it.
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

/// Anonymous fault delivery no longer uses nucleus housekeeping. Pagerd drains
/// its fixed rendezvous directly; this compatibility hook remains only for the
/// boot loop's stable call surface and intentionally has no pager work.
pub fn service_deferred_work() -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::dispatch_wire;
    use crate::multitask::{PagerFaultReservation, PagerFaultState};
    use rustos_user_abi::pager::{
        PAGER_FAULT_ABI_VERSION, PagerEndpointCapabilityWire, PagerFaultRequestWire,
        PagerObjectIdentityWire, VM_ACCESS_READ, VM_OBJECT_ANONYMOUS, VM_PROT_READ,
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
            dispatch_owner_task_id: 71,
        }
    }

    #[test]
    fn pager_rendezvous_wire_binds_exact_fault_subject_and_ticket() {
        let reservation = reservation();
        let wire = dispatch_wire(reservation);
        assert!(wire.is_canonical());
        assert_eq!(wire.request.fault_token, reservation.token);
        assert_eq!(wire.request.task_id, reservation.request.task_id);
        assert_eq!(
            wire.zeroed_frame_capability,
            reservation.zeroed_frame_capability
        );
        assert_eq!(wire.granted_frame_rights, reservation.granted_frame_rights);
    }
}
