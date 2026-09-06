//! Page-map rollback with shootdown-before-reclaim ordering.
//!
//! - **Owner:** `kernel-mm` owns unpublished mapping rollback and frame return.
//! - **Boundary:** the caller supplies the live address-space mutation guard.
//! - **Lifecycle:** remove leaf entries → complete cross-CPU shootdown → remove
//!   owned-frame metadata and free only the exact private frames.
//! - **Concurrency:** the mutation guard excludes address-space activation and
//!   other normal-time page-table mutation throughout rollback.
//! - **Failure:** an unexpected frame or leaf identity is a fatal invariant.
//! - **Forbidden:** no frame free or rejected external-map return before the
//!   corresponding stale translations are acknowledged.
//! - **Evidence:** `tlb-shootdown-lifecycle` and
//!   `process-address-space-lifecycle`.
//!
//! # Rollback is leaf-only
//!
//! A failed transaction keeps any intermediate table it published. Withdrawing
//! one is no longer provable: the exception-time installer can put a leaf into
//! a table this transaction created, in the same 2 MiB block, while the
//! mutation guard is held - that guard excludes normal-time writers, not the
//! fault path, and the fault path takes no lock at all. Retaining the table
//! costs one empty 4 KiB frame until retirement, which is exactly what `munmap`
//! already does. Data leaves still release their exact physical descriptors.

use super::*;

/// The order a rollback must withdraw in: reverse publication order.
///
/// A later leaf exists only because the mapping loop got past the earlier ones,
/// so undoing in reverse is what keeps every step's precondition true and what
/// stops a partial rollback from leaving a hole behind a live entry.
fn rollback_order<T: Copy>(pages: &[T]) -> impl Iterator<Item = T> {
    pages.iter().rev().copied()
}

pub(super) fn rollback_user_pages(
    space: &mut ProcessAddressSpace,
    pages: &[(VirtAddr, u64)],
    mutation: AddressSpaceMutationGuard,
) {
    for (virt, frame_phys) in rollback_order(pages) {
        let unmapped = space.unmap_user_page(virt);
        if unmapped.map(|phys| phys.as_u64()) != Some(frame_phys) {
            panic!("user page rollback mismatch");
        }
    }
    let _flushed_mutation = mutation.flush_for_reclaim();
    for (virt, frame_phys) in rollback_order(pages) {
        assert_eq!(
            phys::release_data_leaf(space.pml4_frame_phys, virt.as_u64(), frame_phys,),
            Some(phys::DataLeafRelease::FrameReusable),
            "rolled-back exclusive leaf lost exact data descriptor"
        );
    }
    free_rollback_frames_exact(rollback_order(pages).map(|(_, frame_phys)| frame_phys));
}

pub(super) fn rollback_external_user_pages(
    space: &mut ProcessAddressSpace,
    pages: &[VirtAddr],
    mutation: AddressSpaceMutationGuard,
) {
    for virt in rollback_order(pages) {
        let _ = space.unmap_user_page(virt);
    }
    // The borrowed frames return to their backing object, so the shootdown is
    // still owed before that object may hand them out again.
    let _flushed_mutation = mutation.flush_for_reclaim();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_rollback_runs_in_reverse_publication_order() {
        let pages = [
            (VirtAddr::new(USER_SPACE_BASE), 0x1000_u64),
            (VirtAddr::new(USER_SPACE_BASE + PAGE_4KIB_U64), 0x2000),
            (VirtAddr::new(USER_SPACE_BASE + 2 * PAGE_4KIB_U64), 0x3000),
        ];
        let order = rollback_order(&pages)
            .map(|(_, frame_phys)| frame_phys)
            .collect::<alloc::vec::Vec<_>>();
        assert_eq!(order, alloc::vec![0x3000, 0x2000, 0x1000]);
    }
}
