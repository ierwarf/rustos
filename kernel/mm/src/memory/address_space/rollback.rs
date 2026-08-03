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
    published_tables: &[PublishedUserTable],
    mutation: AddressSpaceMutationGuard,
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
    rollback_published_tables(space, published_tables);
    let _flushed_mutation = mutation.flush_for_reclaim();
    for &(_, frame_phys) in pages.iter().rev() {
        phys::free_frame(PhysAddr::new(frame_phys));
    }
    for table in published_tables_in_rollback_order(published_tables) {
        phys::free_frame(PhysAddr::new(table.table_phys));
    }
}

pub(super) fn rollback_external_user_pages(
    space: &mut ProcessAddressSpace,
    pages: &[VirtAddr],
    published_tables: &[PublishedUserTable],
    mutation: AddressSpaceMutationGuard,
) {
    for &virt in pages.iter().rev() {
        let _ = space.unmap_user_page(virt);
    }
    rollback_published_tables(space, published_tables);
    let _flushed_mutation = mutation.flush_for_reclaim();
    for table in published_tables_in_rollback_order(published_tables) {
        phys::free_frame(PhysAddr::new(table.table_phys));
    }
}

fn rollback_published_tables(
    space: &mut ProcessAddressSpace,
    published_tables: &[PublishedUserTable],
) {
    for table in published_tables_in_rollback_order(published_tables) {
        // SAFETY: the mutation guard excludes concurrent topology changes;
        // every logged child frame remains mapped until this rollback ends.
        let child = unsafe { kernel_vm::phys_to_table_ref(PhysAddr::new(table.table_phys)) };
        assert!(
            (0..ENTRIES_PER_TABLE).all(|index| child[index].is_unused()),
            "user page-table rollback found a live child entry"
        );
        // SAFETY: the commit log retains the exact live parent frame and index,
        // and reverse order proves no surviving child depends on this entry.
        let parent = unsafe { kernel_vm::phys_to_table_mut(PhysAddr::new(table.parent_phys)) };
        let entry = &mut parent[table.parent_index];
        assert_eq!(
            entry.addr().as_u64(),
            table.table_phys,
            "user page-table rollback parent identity changed"
        );
        entry.set_unused();
        remove_owned_frame(&mut space.owned_frames, table.table_phys)
            .expect("user page-table rollback lost owned-frame authority");
    }
}

fn published_tables_in_rollback_order(
    published_tables: &[PublishedUserTable],
) -> impl Iterator<Item = &PublishedUserTable> {
    published_tables.iter().rev()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intermediate_tables_rollback_in_reverse_publication_order() {
        let published = [
            PublishedUserTable {
                parent_phys: 1,
                parent_index: 2,
                table_phys: 3,
            },
            PublishedUserTable {
                parent_phys: 3,
                parent_index: 4,
                table_phys: 5,
            },
            PublishedUserTable {
                parent_phys: 5,
                parent_index: 6,
                table_phys: 7,
            },
        ];
        let order = published_tables_in_rollback_order(&published)
            .map(|table| table.table_phys)
            .collect::<alloc::vec::Vec<_>>();
        assert_eq!(order, alloc::vec![7, 5, 3]);
    }
}
