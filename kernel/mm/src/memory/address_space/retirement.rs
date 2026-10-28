//! Address-space retirement and ownership reconciliation.
//!
//! - **Owner:** `kernel-mm` owns the final reclamation of every frame an
//!   address space allocated.
//! - **Boundary:** retirement runs only once the TLB protocol has proved the
//!   root inactive on every CPU and no task can still reference it.
//! - **Lifecycle:** retire the root → walk tag-recorded ownership → drain and
//!   reconcile the three ledgers → free.
//! - **Concurrency:** no installer can run here; the address space is inactive
//!   and unreachable, which is what makes the walk a stable reading.
//! - **Failure:** a reclaim refusal is a leak with a diagnostic; a structural
//!   disagreement between the ledgers or inside the page tables is corruption
//!   and stops the kernel.
//! - **Forbidden:** freeing a frame chosen by a topology that failed to
//!   validate.
//! - **Evidence:** `process-address-space-lifecycle`, `tlb-shootdown-lifecycle`.

use super::*;

impl Drop for ProcessAddressSpace {
    #[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
    fn drop(&mut self) {
        // Last release enqueues only; no allocation, lock, CR3 or shootdown here.
        drop(self.execution.take());
    }
}

/// Stops retirement when a PTE ownership tag has no matching descriptor.
///
/// A frame has exactly one descriptor role, so a frame tagged as both a leaf and
/// a table fails one of the two lookups that feed this check.
fn assert_tagged_frame_owned(root_phys: u64, frame: u64, kind: &str, owned: bool) {
    assert!(
        owned,
        "process address space: {kind} tag disagrees with its descriptor root={root_phys:#x} frame={frame:#x}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tagged_frame_with_a_descriptor_passes() {
        assert_tagged_frame_owned(0x1000, 0x4000, "leaf", true);
    }

    #[test]
    #[should_panic(expected = "tag disagrees with its descriptor")]
    fn one_frame_cannot_be_owned_as_both_a_leaf_and_a_table() {
        // The table lookup reports a frame the ledger holds only as data.
        assert_tagged_frame_owned(0x1000, 0x4000, "table", false);
    }
}

/// Only the closed, zero-reference deferred owner calls this cold finalizer.
pub(super) fn reclaim_root(root: u64) {
    let address_space = core::mem::ManuallyDrop::new(ProcessAddressSpace {
        pml4_frame_phys: root,
        next_user_addr: super::USER_SPACE_BASE,
        execution: None,
    });
    if address_space.pml4_frame_phys == 0 {
        return;
    }

    let reclaim_barrier =
        kernel_hal::api::arch::tlb::begin_address_space_retirement(address_space.root_phys());
    drop(reclaim_barrier);
    let (current_frame, _) = Cr3::read();
    if current_frame.start_address() == address_space.root_phys() {
        panic!("cannot drop the active process address space");
    }

    let root_phys = address_space.pml4_frame_phys;

    // Pass 1 validates the whole topology against the descriptor ledger before
    // any frame is released, and allocates nothing: the walk is streamed and
    // every comparison is a count or a per-frame descriptor lookup. A table or
    // data frame has exactly one descriptor role, so a frame claimed as both a
    // leaf and a table fails one of the two ownership checks below.
    let mut tagged_tables = 0_usize;
    let mut tagged_leaves = 0_u64;
    if let Err(error) = address_space.walk_pager_fault_ownership(
        |table| {
            assert_tagged_frame_owned(
                root_phys,
                table,
                "table",
                phys::lazy_table_record_is_owned(root_phys, table),
            );
            tagged_tables += 1;
        },
        |leaf| {
            assert_tagged_frame_owned(
                root_phys,
                leaf.physical_address,
                "leaf",
                phys::data_leaf_is_owned(root_phys, leaf.virtual_address, leaf.physical_address),
            );
            tagged_leaves += 1;
        },
    ) {
        panic!(
            "process address space: page-table ownership walk found corruption root={root_phys:#x} error={error:?}"
        );
    }
    assert_eq!(
        phys::lazy_table_record_count(root_phys),
        tagged_tables,
        "process address space: explicit lazy-table ledger disagrees with PTE tags root={root_phys:#x}",
    );
    let descriptor_count =
        phys::data_leaf_count(root_phys).expect("retired root lost its data-leaf descriptor");
    assert_eq!(
        descriptor_count, tagged_leaves,
        "process address space: data-leaf descriptor count disagrees with PTE tags root={root_phys:#x}",
    );

    // Pass 2 settles each frame as it is found. Leaves go first, then tables,
    // then the root, in bounded allocator batches held on the stack.
    let mut frames = RetiredFrameBatcher::new(root_phys);
    if let Err(error) = address_space.walk_pager_fault_ownership(
        |_table| {},
        |leaf| match phys::release_data_leaf(root_phys, leaf.virtual_address, leaf.physical_address)
        {
            Some(phys::DataLeafRelease::FrameReusable) => frames.push(leaf.physical_address),
            Some(phys::DataLeafRelease::FrameRetained) => {}
            None => panic!("retired PTE tag disagrees with its data-frame descriptor"),
        },
    ) {
        panic!(
            "process address space: page-table ownership changed during retirement root={root_phys:#x} error={error:?}"
        );
    }
    while let Some(table) = phys::pop_lazy_table_record(root_phys) {
        frames.push(table);
    }
    phys::unregister_data_leaf_root(root_phys);
    frames.push(root_phys);
    frames.finish();
}
