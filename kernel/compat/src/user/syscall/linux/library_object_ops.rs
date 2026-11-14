//! Pager-only control of shared immutable-file cache objects.
//!
//! - **Owner:** pagerd owns what is cached and when it is evicted; this file
//!   only admits its create/supply/seal/retire requests into the registry.
//! - **Boundary:** the caller must hold PagerPolicy and the live pagerd lease;
//!   identity, page index and payload are untrusted until validated.
//! - **Lifecycle:** payloads are copied before any lock; registry mutation is
//!   bounded and performs no IPC; cold frame reclamation runs after the lock.
//! - **Failure:** every refusal publishes nothing; a partially built object is
//!   invisible to guests until sealed.
//! - **Concurrency:** one registry lock guards mutation and is never held across
//!   a payload copy, IPC or frame reclamation; the service lease is taken first.
//! - **Forbidden:** paths, policy, and guest-facing authority.
//! - **Evidence:** `kvm-smoke --library-map-probe` and the registry tests in
//!   `library_cache.rs`.
use super::*;
use crate::library_cache::{KernelSources, LibraryError, REGISTRY};
use kernel_mm::api::frame_source::{self, FrameSourceOwner};
use rustos_user_abi::syscall::{
    LibraryPressureWire, LIBRARY_OBJECT_OP_CREATE, LIBRARY_OBJECT_OP_PRESSURE,
    LIBRARY_OBJECT_OP_RETIRE, LIBRARY_OBJECT_OP_SEAL,
    LIBRARY_OBJECT_OP_SUPPLY_PAGE, IPC_SERVICE_PAGERD, RustosLibraryObjectArgs, ServiceCapability,
};

const PAGE_BYTES: u64 = 4096;

pub(super) fn library_errno(error: LibraryError) -> i64 {
    match error {
        LibraryError::Invalid => LINUX_EINVAL,
        LibraryError::Exists => LINUX_EEXIST,
        LibraryError::NoSlots | LibraryError::Incomplete | LibraryError::Sealed => LINUX_EBUSY,
        LibraryError::Budget => LINUX_ENOMEM,
        LibraryError::Stale => LINUX_ESTALE,
        LibraryError::Authority => LINUX_EPERM,
        LibraryError::Source(source) => frame_source_ops::source_errno(source),
    }
}

pub(super) fn control(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(ServiceCapability::PagerPolicy) {
        return linux_errno(LINUX_EPERM);
    }
    let Some(pid) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_ESRCH);
    };
    let args = match usermem::read_current_user_struct::<RustosLibraryObjectArgs>(args_ptr) {
        Ok(args) if args.is_canonical() => args,
        Ok(_) => return linux_errno(LINUX_EINVAL),
        Err(error) => return linux_errno(address_space_error_to_linux_errno(error)),
    };
    if args.op == LIBRARY_OBJECT_OP_PRESSURE {
        // Read-only counters for the pager's retention policy; no registry
        // mutation and no lease closure, so nothing to unwind.
        let wire = LibraryPressureWire {
            free_pages: crate::memory::phys::free_bytes() / PAGE_BYTES,
            min_free_pages: crate::memory::phys::min_free_bytes() / PAGE_BYTES,
            watermark_denials: crate::memory::phys::user_watermark_denials(),
            reserved_pages: REGISTRY.lock().census().1,
        };
        return match usermem::write_current_user_struct(args.payload_ptr, &wire) {
            Ok(()) => 0,
            Err(error) => linux_errno(address_space_error_to_linux_errno(error)),
        };
    }
    let mut payload = [0_u8; 4096];
    let len = if args.op == LIBRARY_OBJECT_OP_SUPPLY_PAGE {
        let len = args.payload_len as usize; // canonicality bounds it by one page
        if let Err(error) =
            usermem::copy_from_current_user_exact(args.payload_ptr, &mut payload[..len])
        {
            return linux_errno(address_space_error_to_linux_errno(error));
        }
        len
    } else {
        0
    };
    let group = match args.op {
        LIBRARY_OBJECT_OP_CREATE | LIBRARY_OBJECT_OP_SUPPLY_PAGE => match multitask::current_user_job_charge_group() {
            Some(group) => group,
            None => return linux_errno(LINUX_ESRCH),
        },
        _ => 0,
    };
    if args.op == LIBRARY_OBJECT_OP_SUPPLY_PAGE {
        // Never reclaim under the registry lock; see frame_source_ops.
        frame_source::reclaim_frame_sources(16);
    }
    // The lease closure is bounded: no IPC, user copy, or process-state lock.
    let outcome = ipc_ops::with_owned_service_lease(pid, IPC_SERVICE_PAGERD, |slot, generation| {
        let owner = FrameSourceOwner { service_slot: slot, service_generation: generation };
        let mut registry = REGISTRY.lock();
        match args.op {
            LIBRARY_OBJECT_OP_CREATE => registry.create(owner, group, args.identity),
            LIBRARY_OBJECT_OP_SUPPLY_PAGE => registry.supply_page(
                &KernelSources,
                args.object,
                owner,
                args.page_index,
                group,
                &payload[..len],
            ),
            LIBRARY_OBJECT_OP_SEAL => registry.seal(args.object, owner).map(|()| 0),
            LIBRARY_OBJECT_OP_RETIRE => {
                registry.retire(&KernelSources, args.object, owner).map(|()| 0)
            }
            _ => Err(LibraryError::Invalid),
        }
    });
    match outcome {
        Some(Ok(value)) => {
            if args.op == LIBRARY_OBJECT_OP_RETIRE {
                frame_source::reclaim_frame_sources(16);
            }
            value
        }
        Some(Err(error)) => linux_errno(library_errno(error)),
        None => linux_errno(LINUX_EPERM),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_refusal_has_a_distinct_actionable_errno() {
        assert_eq!(library_errno(LibraryError::Exists), LINUX_EEXIST);
        assert_eq!(library_errno(LibraryError::Authority), LINUX_EPERM);
        assert_eq!(library_errno(LibraryError::Stale), LINUX_ESTALE);
        assert_eq!(library_errno(LibraryError::Budget), LINUX_ENOMEM);
        assert_eq!(library_errno(LibraryError::Invalid), LINUX_EINVAL);
        assert_eq!(library_errno(LibraryError::Incomplete), LINUX_EBUSY);
    }
}
