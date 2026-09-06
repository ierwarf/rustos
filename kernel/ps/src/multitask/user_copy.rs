//! Generation-bound kernel copyout admission, including resident COW splits.
//!
//! - **Owner:** `kernel-ps` binds MM mechanics to the exact process VMA authority.
//! - **Boundary:** User pointers and COW tags alone never authorize a write.
//! - **Lifecycle:** Bind under ProcessStateLock, admit VMA, split, retranslate,
//!   construct the write proof, then copy before releasing the state lock.
//! - **Concurrency:** Fork/exec/unmap/protect share that lock. Exception COW
//!   replacement is serialized by the exact-root TLB and frame descriptor guards.
//! - **Failure:** Missing, reserved, shared, or read-only authority fails closed.
//! - **Forbidden:** No current-CR3 substitution for a retained foreign process;
//!   no direct-map write through the old readonly shared frame.
//! - **Evidence:** `fork-cow-contract.md`; copyout authority unit tests and KVM pread.

use core::ops::Deref;
use kernel_mm::api::{address_space::ValidatedUserWrite, paging, phys::CowFrameKind};
use rustos_user_abi::pager::{
    PagerVmRegionWire, VM_ACCESS_WRITE, VM_COMMIT_COMMITTED, VM_OBJECT_ANONYMOUS,
    VM_OBJECT_FILE_PRIVATE, VM_OBJECT_IMAGE_SECTION, VM_PROT_WRITE, VM_SHARING_PRIVATE,
};
use x86_64::VirtAddr;

use super::{ProcessAddressSpace, RetainedCurrentUserAddressSpace, pager_vma, process_table};

impl RetainedCurrentUserAddressSpace {
    pub(crate) fn try_with_user_copy_address_space<R>(
        &self,
        f: impl FnOnce(&UserCopyAddressSpace<'_>) -> R,
    ) -> Option<R> {
        self.process
            .with_exact_visible_state(self.identity, |_, state| {
                f(&UserCopyAddressSpace::new(
                    state.address_space(),
                    self.process.handle(),
                    self.identity,
                ))
            })
    }
}

/// Constructed only inside a current/retained process-state bind. The inner
/// reference and every returned proof remain within that lock's closure.
pub(crate) struct UserCopyAddressSpace<'a> {
    address_space: &'a ProcessAddressSpace,
    handle: process_table::ProcessHandle,
    identity: process_table::ProcessIdentity,
}

impl<'a> UserCopyAddressSpace<'a> {
    pub(super) fn new(
        address_space: &'a ProcessAddressSpace,
        handle: process_table::ProcessHandle,
        identity: process_table::ProcessIdentity,
    ) -> Self {
        Self {
            address_space,
            handle,
            identity,
        }
    }

    pub(crate) fn validate_user_write(
        &self,
        start: VirtAddr,
        len: usize,
    ) -> Result<ValidatedUserWrite<'_>, paging::AddressSpaceError> {
        self.address_space
            .validate_user_write_resolving_cow(start, len, |page| {
                // This is the slow path only. Ordinary writable copyout retains
                // the single-walk fast path and does not scan the VMA table.
                let (_, region) = pager_vma::lookup_slot(
                    self.handle,
                    self.identity,
                    page.as_u64(),
                    VM_ACCESS_WRITE,
                )
                .map_err(|_| paging::AddressSpaceError::ProtectionViolation)?;
                let kind = copyout_cow_kind(region)?;
                self.address_space.resolve_cow_write_for_copyout(page, kind)
            })
    }

    pub(crate) fn validate_user_write_buffer(
        &self,
        start: VirtAddr,
        len: usize,
    ) -> Result<(), paging::AddressSpaceError> {
        self.validate_user_write(start, len).map(drop)
    }
}

impl Deref for UserCopyAddressSpace<'_> {
    type Target = ProcessAddressSpace;
    fn deref(&self) -> &Self::Target {
        self.address_space
    }
}

/// COW describes backing ownership, not a protection override. Check logical
/// rights even when the PTE retains its COW bit after mprotect/decommit.
fn copyout_cow_kind(region: PagerVmRegionWire) -> Result<CowFrameKind, paging::AddressSpaceError> {
    if region.commit_state != VM_COMMIT_COMMITTED
        || region.prot & VM_PROT_WRITE == 0
        || region.sharing != VM_SHARING_PRIVATE
    {
        return Err(paging::AddressSpaceError::ProtectionViolation);
    }
    match region.object.object_type {
        VM_OBJECT_ANONYMOUS => Ok(CowFrameKind::AnonymousFork),
        VM_OBJECT_FILE_PRIVATE | VM_OBJECT_IMAGE_SECTION => Ok(CowFrameKind::PrivateFileSection),
        _ => Err(paging::AddressSpaceError::ProtectionViolation),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::pager::*;

    #[test]
    fn copyout_cow_requires_committed_private_write_authority_for_both_abis() {
        let mut region = PagerVmRegionWire {
            commit_state: VM_COMMIT_COMMITTED,
            prot: VM_PROT_WRITE,
            sharing: VM_SHARING_PRIVATE,
            ..Default::default()
        };
        for (object, kind) in [
            (VM_OBJECT_ANONYMOUS, CowFrameKind::AnonymousFork),
            (VM_OBJECT_FILE_PRIVATE, CowFrameKind::PrivateFileSection),
            (VM_OBJECT_IMAGE_SECTION, CowFrameKind::PrivateFileSection),
        ] {
            region.object.object_type = object;
            assert_eq!(copyout_cow_kind(region), Ok(kind));
            for invalid in [
                PagerVmRegionWire {
                    prot: VM_PROT_READ,
                    ..region
                },
                PagerVmRegionWire { prot: 0, ..region },
                PagerVmRegionWire {
                    commit_state: VM_COMMIT_RESERVED,
                    ..region
                },
                PagerVmRegionWire {
                    sharing: VM_SHARING_SHARED,
                    ..region
                },
            ] {
                assert_eq!(
                    copyout_cow_kind(invalid),
                    Err(paging::AddressSpaceError::ProtectionViolation)
                );
            }
        }
        for kind in [
            VM_OBJECT_FILE_SHARED,
            VM_OBJECT_MEMFD,
            VM_OBJECT_DEVICE_PINNED,
            0,
            u16::MAX,
        ] {
            region.object.object_type = kind;
            assert!(copyout_cow_kind(region).is_err());
        }
    }
}
