//! Privileged immutable frame-source admission; no filesystem or cache policy.
//! Source publication is serialized against service-lease withdrawal. Payload
//! copies and cold reclamation occur outside the registry lock; the publishing
//! closure only allocates a fresh page and publishes bounded source metadata.
use super::*;
use kernel_mm::api::frame_source::{self, FrameSourceError, FrameSourceKey, FrameSourceOwner};
use rustos_user_abi::syscall::{
    FRAME_SOURCE_OP_OWNER_EPOCH, FRAME_SOURCE_OP_RETIRE, FRAME_SOURCE_OP_SUPPLY,
    IPC_SERVICE_PAGERD, RustosFrameSourceArgs, ServiceCapability,
};

const _: () =
    assert!(frame_source::MAX_FRAME_SOURCES == rustos_user_abi::syscall::FRAME_SOURCE_MAX_LIVE);
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
    if !ipc_ops::current_process_has_service_capability(ServiceCapability::PagerPolicy) {
        return linux_errno(LINUX_EPERM);
    }
    let Some(pid) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_ESRCH);
    };
    let args = match usermem::read_current_user_struct::<RustosFrameSourceArgs>(args_ptr) {
        Ok(args) if args.is_canonical() => args,
        Ok(_) => return linux_errno(LINUX_EINVAL),
        Err(error) => return linux_errno(address_space_error_to_linux_errno(error)),
    };
    if args.op == FRAME_SOURCE_OP_SUPPLY {
        let Some(group) = multitask::current_user_job_charge_group() else {
            return linux_errno(LINUX_ESRCH);
        };
        let mut payload = [0_u8; 4096];
        let len = args.payload_len as usize; // canonicality bounds it by one page
        if len != 0 {
            if let Err(error) =
                usermem::copy_from_current_user_exact(args.payload_ptr, &mut payload[..len])
            {
                return linux_errno(address_space_error_to_linux_errno(error));
            }
        }
        // Never reclaim under the registry lock: a concurrent PTE/COW change
        // may retain a descriptor role until its TLB acknowledgements arrive.
        frame_source::reclaim_frame_sources(16);
        let result =
            ipc_ops::with_owned_service_lease(pid, IPC_SERVICE_PAGERD, |slot, generation| {
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
            });
        return match result {
            Some(Ok(token)) => token,
            Some(Err(error)) => linux_errno(source_errno(error)),
            None => linux_errno(LINUX_EPERM),
        };
    }
    let Some(owner) =
        ipc_ops::with_owned_service_lease(pid, IPC_SERVICE_PAGERD, |slot, generation| {
            FrameSourceOwner {
                service_slot: slot,
                service_generation: generation,
            }
        })
    else {
        return linux_errno(LINUX_EPERM);
    };
    match args.op {
        FRAME_SOURCE_OP_RETIRE => match frame_source::retire_frame_source(args.token, owner) {
            Ok(()) => {
                frame_source::reclaim_frame_sources(16);
                0
            }
            Err(error) => linux_errno(source_errno(error)),
        },
        FRAME_SOURCE_OP_OWNER_EPOCH => {
            if owner.service_generation > i64::MAX as u64 {
                linux_errno(LINUX_EOVERFLOW)
            } else {
                owner.service_generation
            }
        }
        _ => linux_errno(LINUX_EINVAL),
    }
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
