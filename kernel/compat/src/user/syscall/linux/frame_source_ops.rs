//! Privileged immutable frame-source admission; no filesystem or cache policy.
//! Source publication is serialized against service-lease withdrawal. Payload
//! copies and cold reclamation occur outside the registry lock; the publishing
//! closure only allocates a fresh page and publishes bounded source metadata.
//!
//! - **Owner:** the pager service supplies sources; kernel-mm owns the slots.
//! - **Boundary:** the caller must hold PagerPolicy and the live pager lease;
//!   key, length and payload are untrusted until validated.
//! - **Lifecycle:** supply publishes a bounded source, withdrawal rejects new
//!   pins, and cold reclamation runs after the last pin is released.
//! - **Concurrency:** publication is serialized against lease withdrawal; the
//!   registry lock is never held across a payload copy or reclamation.
//! - **Failure:** every refusal publishes nothing and leaves no charged slot.
//! - **Forbidden:** paths, cache replacement, and guest-facing authority.
//! - **Evidence:** `formal/frame-source-lifecycle` and `kvm-smoke
//!   --library-map-probe`.
use super::*;
use kernel_mm::api::frame_source::{self, FrameSourceError, FrameSourceKey, FrameSourceOwner};
use rustos_user_abi::syscall::{
    FRAME_SOURCE_OP_OWNER_EPOCH, FRAME_SOURCE_OP_RETIRE, FRAME_SOURCE_OP_SUPPLY,
    IPC_SERVICE_PAGERD, RustosFrameSourceArgs, ServiceCapability,
};

const _: () =
    assert!(frame_source::MAX_FRAME_SOURCES == rustos_user_abi::syscall::FRAME_SOURCE_TABLE_SLOTS);
const _: () = assert!(rustos_user_abi::syscall::FRAME_SOURCE_PAGE_BYTES == 4096);

pub(super) fn source_errno(error: FrameSourceError) -> i64 {
    match error {
        FrameSourceError::Invalid | FrameSourceError::Mismatch => LINUX_EINVAL,
        FrameSourceError::NoSlots | FrameSourceError::Stale => LINUX_EAGAIN,
        FrameSourceError::NoMemory | FrameSourceError::Quota => LINUX_ENOMEM,
        FrameSourceError::Authority => LINUX_EPERM,
        FrameSourceError::RefOverflow => LINUX_EOVERFLOW,
    }
}

pub(super) fn control(args_ptr: u64) -> u64 {
    linux_result((|| {
        require_service_capability(ServiceCapability::PagerPolicy)?;
        let pid = current_user_pid()?;
        let args = read_user_struct::<RustosFrameSourceArgs>(args_ptr)?;
        if !args.is_canonical() {
            return Err(LINUX_EINVAL);
        }
        if args.op == FRAME_SOURCE_OP_SUPPLY {
            let group = multitask::current_user_job_charge_group().ok_or(LINUX_ESRCH)?;
            let mut payload = [0_u8; 4096];
            let len = args.payload_len as usize; // canonicality bounds it by one page
            if len != 0 {
                read_user_bytes(args.payload_ptr, &mut payload[..len])?;
            }
            // Never reclaim under the registry lock: a concurrent PTE/COW change
            // may retain a descriptor role until its TLB acknowledgements arrive.
            frame_source::reclaim_frame_sources(16);
            return ipc_ops::with_owned_service_lease(pid, IPC_SERVICE_PAGERD, |slot, generation| {
                frame_source::supply_frame_source(
                    FrameSourceOwner {
                        service_slot: slot,
                        service_generation: generation,
                    },
                    FrameSourceKey {
                        namespace: args.key.namespace,
                        object: args.key.object,
                        object_generation: args.key.object_generation,
                        content_generation: args.key.content_generation,
                        page_offset: args.key.page_offset,
                    },
                    args.rights,
                    group,
                    &payload[..len],
                )
            })
            .ok_or(LINUX_EPERM)?
            .map_err(source_errno);
        }
        let owner =
            ipc_ops::with_owned_service_lease(pid, IPC_SERVICE_PAGERD, |slot, generation| {
                FrameSourceOwner {
                    service_slot: slot,
                    service_generation: generation,
                }
            })
            .ok_or(LINUX_EPERM)?;
        match args.op {
            FRAME_SOURCE_OP_RETIRE => {
                frame_source::retire_frame_source(args.token, owner).map_err(source_errno)?;
                frame_source::reclaim_frame_sources(16);
                Ok(0)
            }
            FRAME_SOURCE_OP_OWNER_EPOCH => {
                if owner.service_generation > i64::MAX as u64 {
                    Err(LINUX_EOVERFLOW)
                } else {
                    Ok(owner.service_generation)
                }
            }
            _ => Err(LINUX_EINVAL),
        }
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_refusals_distinguish_memory_capacity_and_authority() {
        assert_eq!(source_errno(FrameSourceError::Quota), LINUX_ENOMEM);
        assert_eq!(source_errno(FrameSourceError::NoMemory), LINUX_ENOMEM);
        assert_eq!(source_errno(FrameSourceError::NoSlots), LINUX_EAGAIN);
        assert_eq!(source_errno(FrameSourceError::Stale), LINUX_EAGAIN);
        assert_eq!(source_errno(FrameSourceError::Authority), LINUX_EPERM);
        assert_eq!(source_errno(FrameSourceError::Mismatch), LINUX_EINVAL);
        assert_eq!(source_errno(FrameSourceError::RefOverflow), LINUX_EOVERFLOW);
    }
}
