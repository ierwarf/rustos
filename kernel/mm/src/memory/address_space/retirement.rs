//! Address-space retirement and ownership reconciliation.
//!
//! - **Owner:** `kernel-mm` owns the final reclamation of every frame an
//!   address space allocated.
//! - **Boundary:** retirement runs only once the TLB protocol has proved the
//!   root inactive on every CPU and no task can still reference it.
//! - **Lifecycle:** retire the root → walk tag-recorded ownership → reconcile
//!   the two ledgers → free.
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
            // Unit tests may use synthetic address spaces that do not have privileged CR3
            // visibility or canonical direct-map page-table backing.
            let mut recorded_frames = self.owned_frames.clone();
            recorded_frames.sort_unstable();
            recorded_frames.dedup();
            free_owned_frames_silently(recorded_frames.into_iter());
            return;
        }

        // Cross-CPU lifetime invariant: the scheduler/process retirement
        // barrier must remove every remote owner before any page-table frame is
        // reclaimed. The generation-bound targeted shootdown also removes translations
        // cached before those CPUs switched roots; measured range/root-specific
        // retirement can replace it later without weakening this ordering.
        let reclaim_barrier =
            kernel_hal::api::arch::tlb::begin_address_space_retirement(self.root_phys());
        drop(reclaim_barrier);
        let (current_frame, _) = Cr3::read();
        if current_frame.start_address() == self.root_phys() {
            panic!("cannot drop the active process address space");
        }

        // Capture tag-recorded ownership before freeing the tables that hold
        // the tags.
        //
        // Reclaim failures and walk failures are different facts. A frame the
        // allocator refuses is a leak with a diagnostic; a page table that
        // cannot be walked means the structure itself disagrees with what
        // published it, and continuing would free frames chosen by corrupted
        // topology.
        let mut tagged = match self.pager_fault_ownership() {
            Ok(tagged) => tagged,
            Err(error) => panic!(
                "process address space: page-table ownership walk found corruption root={:#x} error={:?}",
                self.pml4_frame_phys, error
            ),
        };

        let mut recorded_frames = self.owned_frames.clone();
        recorded_frames.sort_unstable();
        recorded_frames.dedup();
        if recorded_frames.len() != self.owned_frames.len() {
            crate::debug::println!(
                "process address space: duplicate owned frame entries detected root={:#x} owned={} unique={}",
                self.pml4_frame_phys,
                self.owned_frames.len(),
                recorded_frames.len(),
            );
        }
        if recorded_frames.is_empty() {
            crate::debug::println!(
                "process address space: missing ownership ledger root={:#x}",
                self.pml4_frame_phys,
            );
            recorded_frames.push(self.pml4_frame_phys);
        }
        assert_ledgers_are_disjoint(self.pml4_frame_phys, &recorded_frames, &mut tagged);

        // owned_frames is the allocation ledger. A page-table walk is not an
        // ownership oracle because shared memfd and device mappings install
        // borrowed leaf frames which their backing objects must release.
        free_owned_frames_logged(self.pml4_frame_phys, recorded_frames.into_iter());
        free_owned_frames_logged(
            self.pml4_frame_phys,
            tagged.leaves.iter().map(|leaf| leaf.physical_address),
        );
        free_owned_frames_logged(self.pml4_frame_phys, tagged.tables.iter().copied());
    }
}

/// Fails stop when one frame is claimed by more than one ownership ledger.
///
/// The two ledgers are written by different paths - the normal-time mapping
/// transaction under its lock, and the exception-time installer through a
/// page-table tag - and neither can observe the other while it writes. Their
/// disagreement is therefore not recoverable bookkeeping: it means one frame is
/// about to be returned to the allocator twice.
///
/// `recorded_frames` must be sorted; `tagged.tables` is sorted here.
fn assert_ledgers_are_disjoint(
    root_phys: u64,
    recorded_frames: &[u64],
    tagged: &mut PagerFaultOwnership,
) {
    tagged.tables.sort_unstable();
    for &table in &tagged.tables {
        assert!(
            recorded_frames.binary_search(&table).is_err(),
            "process address space: table frame claimed by both ownership ledgers root={root_phys:#x} frame={table:#x}"
        );
    }
    for leaf in &tagged.leaves {
        let frame = leaf.physical_address;
        assert!(
            recorded_frames.binary_search(&frame).is_err(),
            "process address space: leaf frame claimed by both ownership ledgers root={root_phys:#x} frame={frame:#x}"
        );
        assert!(
            tagged.tables.binary_search(&frame).is_err(),
            "process address space: frame claimed as both leaf and table root={root_phys:#x} frame={frame:#x}"
        );
    }
}
