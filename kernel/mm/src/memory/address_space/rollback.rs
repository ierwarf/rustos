//! Page-map rollback with shootdown-before-reclaim ordering.
//!
//! - **Owner:** `kernel-mm` owns unpublished mapping rollback and frame return.
//! - **Boundary:** the caller supplies the live address-space mutation guard.
//! - **Lifecycle:** remove leaf entries → complete cross-CPU shootdown → remove
//!   owned-frame metadata and free only the exact private frames.
//! - **Concurrency:** the mutation guard excludes address-space activation and
//!   other page-table mutation throughout rollback.
//! - **Failure:** an unexpected frame or leaf identity is a fatal invariant.
//! - **Forbidden:** no frame free or rejected external-map return before the
//!   corresponding stale translations are acknowledged.
//! - **Evidence:** `tlb-shootdown-lifecycle` and
//!   `process-address-space-lifecycle`.

use super::*;

pub(super) fn rollback_user_pages(
    space: &mut ProcessAddressSpace,
    pages: &[(VirtAddr, u64)],
    mutation: &mut AddressSpaceMutationGuard,
) {
    for &(virt, frame_phys) in pages.iter().rev() {
        let unmapped = space.unmap_user_page(virt);
        if unmapped.map(|phys| phys.as_u64()) != Some(frame_phys) {
            panic!("user page rollback mismatch");
        }
        if space.owned_frames.contains(&frame_phys) {
            let removed = remove_owned_frame(&mut space.owned_frames, frame_phys);
            debug_assert!(removed.is_ok());
        }
    }
    mutation.flush_before_reclaim();
    for &(_, frame_phys) in pages.iter().rev() {
        phys::free_frame(PhysAddr::new(frame_phys));
    }
}

pub(super) fn rollback_external_user_pages(
    space: &mut ProcessAddressSpace,
    pages: &[VirtAddr],
    mutation: &mut AddressSpaceMutationGuard,
) {
    for &virt in pages.iter().rev() {
        let _ = space.unmap_user_page(virt);
    }
    mutation.flush_before_reclaim();
}
