//! Owner: compat adopts worker-bound private file/section fault replies.
//! Boundary: a typed source pin binds provider epoch, object, content and page.
//! Lifecycle: pin, validate current MM/VMA, publish independent leaf custody,
//! then consume the fault and wake only its exact waiter.
//! Failure: a stale source or target publishes nothing; cache withdrawal never
//! frees a surviving leaf. No file I/O, path selection or cache policy lives here.

use super::*;
use mm_api::frame_source::{FrameSourceKey, FrameSourceOwner};
use rustos_user_abi::pager::{VM_PROT_EXECUTE, VM_PROT_READ};

/// Cache owner and backing namespace are deliberately different identities:
/// pagerd supplies the frame; the request names its opaque backing provider.
fn source_authority(
    request: PagerFaultRequestWire,
    rights: u32,
) -> (FrameSourceOwner, FrameSourceKey, u64) {
    (
        FrameSourceOwner {
            service_slot: rustos_user_abi::syscall::IPC_SERVICE_PAGERD,
            service_generation: request.object.pager_epoch,
        },
        FrameSourceKey {
            namespace: request.object.backing_service,
            object: request.object.slot,
            object_generation: request.object.generation,
            content_generation: request.object.backing_generation,
            page_offset: request.object_offset,
        },
        // A private writable view never gains write access to cached bytes.
        // Its first write obtains an independently owned frame through COW.
        u64::from(VM_PROT_READ | (rights & VM_PROT_EXECUTE)),
    )
}

pub(super) fn complete_cached_file_reply(
    claimed: ps_api::PagerFaultReservation,
    reply: PagerFaultReplyWire,
) -> Result<(), ps_api::PagerFaultSlotError> {
    let (owner, key, rights) = source_authority(claimed.request, reply.frame_rights);
    let result = mm_api::frame_source::pin_frame_source(reply.frame_capability, owner, key, rights)
        .map_err(|error| match error {
            mm_api::frame_source::FrameSourceError::Authority
            | mm_api::frame_source::FrameSourceError::Mismatch
            | mm_api::frame_source::FrameSourceError::Invalid => {
                ps_api::PagerFaultSlotError::Authority
            }
            mm_api::frame_source::FrameSourceError::RefOverflow => {
                ps_api::PagerFaultSlotError::Pressure
            }
            _ => ps_api::PagerFaultSlotError::Stale,
        })
        .and_then(|source| {
            // Normal syscall context: process/MM/VMA and access are checked
            // under the exact retained process state, not a PID-only lookup.
            // Keep `source` borrowed until the leaf has independent custody.
            ps_api::with_validated_fault_address_space(claimed.request, |_, _, address_space| {
                address_space.map_cached_private_file_cow_at(
                    VirtAddr::new(claimed.request.virtual_address),
                    page_flags(reply.frame_rights),
                    &source,
                )
            })
            .map_err(|_| ps_api::PagerFaultSlotError::Stale)?
            .map_err(|error| match error {
                mm_api::paging::AddressSpaceError::OutOfFrames => {
                    ps_api::PagerFaultSlotError::Pressure
                }
                mm_api::paging::AddressSpaceError::ProtectionViolation => {
                    ps_api::PagerFaultSlotError::Authority
                }
                _ => ps_api::PagerFaultSlotError::Stale,
            })
        });
    cancel_grant(claimed);
    let _ = ps_api::consume_pager_fault_reply(claimed.token);
    let _ = wake_fault_owner(claimed.token, claimed.request.task_id);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::pager::*;

    #[test]
    fn source_authority_binds_backing_and_epoch_not_target_pid_or_va() {
        let request = PagerFaultRequestWire {
            object: PagerObjectIdentityWire {
                backing_service: 2,
                slot: 7,
                generation: 11,
                pager_epoch: 13,
                backing_generation: 17,
                ..PagerObjectIdentityWire::default()
            },
            object_offset: 0x9000,
            ..PagerFaultRequestWire::default()
        };
        let (owner, key, rights) = source_authority(request, VM_PROT_READ | VM_PROT_WRITE);
        assert_eq!(
            owner.service_slot,
            rustos_user_abi::syscall::IPC_SERVICE_PAGERD
        );
        assert_eq!(owner.service_generation, 13);
        assert_eq!(
            key,
            FrameSourceKey {
                namespace: 2,
                object: 7,
                object_generation: 11,
                content_generation: 17,
                page_offset: 0x9000,
            }
        );
        assert_eq!(rights, u64::from(VM_PROT_READ));
        let (_, _, code_rights) = source_authority(request, VM_PROT_READ | VM_PROT_EXECUTE);
        assert_eq!(code_rights, u64::from(VM_PROT_READ | VM_PROT_EXECUTE));
        let alias = PagerFaultRequestWire {
            virtual_address: 0x1234000,
            process_handle: 999,
            ..request
        };
        assert_eq!(
            source_authority(alias, VM_PROT_READ),
            source_authority(request, VM_PROT_READ)
        );
    }
}
