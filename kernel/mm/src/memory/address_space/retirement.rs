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
        if self.pml4_frame_phys == 0 {
            return;
        }

        let reclaim_barrier =
            kernel_hal::api::arch::tlb::begin_address_space_retirement(self.root_phys());
        drop(reclaim_barrier);
        let (current_frame, _) = Cr3::read();
        if current_frame.start_address() == self.root_phys() {
            panic!("cannot drop the active process address space");
        }

        let mut tagged = match self.pager_fault_ownership() {
            Ok(tagged) => tagged,
            Err(error) => panic!(
                "process address space: page-table ownership walk found corruption root={:#x} error={:?}",
                self.pml4_frame_phys, error
            ),
        };
        let mut explicit_tables =
            crate::memory::phys::drain_lazy_table_records(self.pml4_frame_phys);
        explicit_tables.sort_unstable();
        tagged.tables.sort_unstable();
        assert_eq!(
            explicit_tables, tagged.tables,
            "process address space: explicit lazy-table ledger disagrees with PTE tags root={:#x}",
            self.pml4_frame_phys,
        );

        let descriptor_count = phys::data_leaf_count(self.pml4_frame_phys)
            .expect("retired root lost its data-leaf descriptor");
        assert_eq!(
            descriptor_count as usize,
            tagged.leaves.len(),
            "process address space: data-leaf descriptor count disagrees with PTE tags root={:#x}",
            self.pml4_frame_phys,
        );
        assert_tagged_ownership(self.pml4_frame_phys, &tagged);

        let mut reusable_leaves = Vec::new();
        reusable_leaves
            .try_reserve_exact(tagged.leaves.len())
            .expect("retirement could not reserve reusable-leaf batch");
        for leaf in tagged.leaves.iter().copied() {
            match phys::release_data_leaf(
                self.pml4_frame_phys,
                leaf.virtual_address,
                leaf.physical_address,
            ) {
                Some(phys::DataLeafRelease::FrameReusable) => {
                    reusable_leaves.push(leaf.physical_address);
                }
                Some(phys::DataLeafRelease::FrameRetained) => {}
                None => panic!("retired PTE tag disagrees with its data-frame descriptor"),
            }
        }
        phys::unregister_data_leaf_root(self.pml4_frame_phys);

        free_retired_frames_logged(self.pml4_frame_phys, reusable_leaves.into_iter());
        free_retired_frames_logged(self.pml4_frame_phys, explicit_tables.into_iter());
        free_retired_frames_logged(self.pml4_frame_phys, core::iter::once(self.pml4_frame_phys));
    }
}

fn assert_tagged_ownership(root_phys: u64, tagged: &PagerFaultOwnership) {
    for leaf in &tagged.leaves {
        let frame = leaf.physical_address;
        assert!(
            tagged.tables.binary_search(&frame).is_err(),
            "process address space: frame claimed as both leaf and table root={root_phys:#x} frame={frame:#x}"
        );
        assert!(
            phys::data_leaf_is_owned(root_phys, leaf.virtual_address, leaf.physical_address,),
            "process address space: leaf tag disagrees with data descriptor root={root_phys:#x} va={:#x} frame={frame:#x}",
            leaf.virtual_address,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagged(tables: &[u64], leaves: &[(u64, u64)]) -> PagerFaultOwnership {
        PagerFaultOwnership {
            tables: tables.to_vec(),
            leaves: leaves
                .iter()
                .map(|&(virtual_address, physical_address)| PagerFaultLeaf {
                    virtual_address,
                    physical_address,
                })
                .collect(),
        }
    }

    #[test]
    #[should_panic(expected = "frame claimed as both leaf and table")]
    fn one_frame_cannot_be_owned_as_both_a_leaf_and_a_table() {
        let ownership = tagged(&[0x4000], &[(0x1_0000_0000, 0x4000)]);
        assert_tagged_ownership(0x1000, &ownership);
    }
}
