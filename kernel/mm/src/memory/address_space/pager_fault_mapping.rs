//! Pager-fault leaf preparation and bounded PTE ownership transfer.
//!
//! This module deliberately contains both sides of the boundary: normal-time
//! VMA preparation can allocate intermediate tables, while the reply-time
//! install only changes an already-present leaf and a fixed array ledger.
use core::sync::atomic::{AtomicU64, Ordering};

use super::*;
use x86_64::structures::paging::page_table::PageTableEntry;

/// Software-owned leaf bit for frames installed by the IRQ-off anonymous
/// fault path.  It is an x86 available-to-software PTE bit, never consumed by
/// the hardware translation or protection decision.
const IRQ_OFF_PAGER_FAULT_LEAF: PageTableFlags = PageTableFlags::BIT_9;

/// Publishes a pre-zeroed frame into the active address space's already
/// prepared leaf without taking an address-space lock, allocating, or issuing
/// a TLB shootdown.
///
/// Callers must hold the exact [`PagerFaultInstallPermit`](kernel_ps::api::PagerFaultInstallPermit)
/// obtained from the VMA publication.  That permit prevents a withdrawing
/// writer from changing this leaf's topology until this function returns.
/// A non-present-to-present transition cannot leave a stale translation: no
/// prior translation exists for the non-present PTE.  `Ok(false)` means a
/// racing fault installed the same prepared leaf first, so its frame must be
/// returned to the reserve.
pub fn map_current_prepared_pager_fault_frame_at(
    start: VirtAddr,
    frame_phys: u64,
    flags: PageTableFlags,
) -> Result<bool, AddressSpaceError> {
    validate_user_page_range(start, 1)?;
    if frame_phys == 0 || !frame_phys.is_multiple_of(PAGE_4KIB_U64) {
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }
    let page_flags = normalize_user_page_flags(flags)? | IRQ_OFF_PAGER_FAULT_LEAF;
    let desired = frame_phys | page_flags.bits();
    with_prepared_user_leaf_atomic(kernel_vm::current_root_phys(), start, |leaf| {
        match leaf.compare_exchange(0, desired, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Ok(true),
            Err(observed) if observed & PageTableFlags::PRESENT.bits() != 0 => Ok(false),
            Err(_) => Err(AddressSpaceError::AlreadyMapped),
        }
    })
}

impl ProcessAddressSpace {
    /// Prepares a user range for pager replies while normal mapping policy is
    /// still executing.
    ///
    /// Pager-leaf metadata is reserved here, before the VMA becomes visible.
    /// The exception path can therefore publish a resident leaf without
    /// allocating, while the resident working set remains independent of the
    /// bounded fault-frame pool.
    pub fn prepare_pager_fault_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<(), AddressSpaceError> {
        if page_count == 0 {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }
        validate_user_page_range(start, page_count)?;

        // One table walk per *page table*, not per page.
        //
        // Every page inside one 2 MiB block resolves the same PDPT, PD and PT,
        // so resolving them per page repeated the identical three lookups 512
        // times over. That is `mmap` latency paid on every anonymous mapping,
        // and it grows linearly with mapping size: a 256 MiB range did 196 608
        // table resolutions where 512 are needed.
        //
        // The reservation was sized the same way. `page_count * 3` slots for a
        // 256 MiB range is 196 608 entries - 1.5 MiB of ledger capacity - to
        // hold at most a few hundred real table frames. The tight bound is one
        // table per block at each level, plus one per level for the partial
        // blocks at each end.
        const ENTRIES_PER_TABLE_SHIFT: u32 = 9;
        let pt_span = 1_usize << ENTRIES_PER_TABLE_SHIFT;
        let pd_span = pt_span << ENTRIES_PER_TABLE_SHIFT;
        let pdpt_span = pd_span << ENTRIES_PER_TABLE_SHIFT;
        let table_capacity = page_count
            .div_ceil(pt_span)
            .checked_add(page_count.div_ceil(pd_span))
            .and_then(|sum| sum.checked_add(page_count.div_ceil(pdpt_span)))
            .and_then(|sum| sum.checked_add(3))
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let mut published_tables = Vec::new();
        published_tables
            .try_reserve_exact(table_capacity)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        self.owned_frames
            .try_reserve_exact(table_capacity)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let mutation = begin_address_space_mutation(self.root_phys());

        let admission = (|| {
            let mut page_index = 0;
            while page_index < page_count {
                let virt = page_addr(start, page_index)?;
                let pml4_phys = self.root_phys();
                let pdpt_phys = ensure_next_table(
                    &mut self.owned_frames,
                    &mut published_tables,
                    pml4_phys,
                    p4_index(virt),
                )?;
                let pd_phys = ensure_next_table(
                    &mut self.owned_frames,
                    &mut published_tables,
                    pdpt_phys,
                    p3_index(virt),
                )?;
                let pt_phys = ensure_next_table(
                    &mut self.owned_frames,
                    &mut published_tables,
                    pd_phys,
                    p2_index(virt),
                )?;
                // SAFETY: `ensure_next_table` either retained this exact
                // table in the address-space ownership ledger or rejected the
                // admission. The mutation guard serializes topology changes.
                let pt = unsafe { kernel_vm::phys_to_table_mut(pt_phys) };
                // Every remaining page that lands in this same table is checked
                // here, against the table this iteration already resolved.
                let first_entry = p1_index(virt);
                let entries_here = (pt_span - first_entry).min(page_count - page_index);
                for entry in first_entry..first_entry + entries_here {
                    if !pt[entry].is_unused() {
                        return Err(AddressSpaceError::AlreadyMapped);
                    }
                }
                page_index += entries_here;
            }
            Ok(())
        })();
        if let Err(error) = admission {
            rollback_external_user_pages(self, &[], &published_tables, mutation);
            return Err(error);
        }

        drop(mutation);
        Ok(())
    }

    /// Installs one pager-granted frame into a normal-time prepared user leaf.
    ///
    /// # Exception-path contract
    ///
    /// `frame_phys` must come from an exact, unconsumed frame grant. This path
    /// performs no allocation or blocking lookup: normal-time VMA preparation
    /// has already installed every intermediate table it may traverse.
    pub fn map_prepared_pager_fault_frame_at(
        &mut self,
        start: VirtAddr,
        frame_phys: u64,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, 1)?;
        if frame_phys == 0 || !frame_phys.is_multiple_of(PAGE_4KIB_U64) {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        let page_flags = normalize_user_page_flags(flags)? | IRQ_OFF_PAGER_FAULT_LEAF;
        let mutation = begin_address_space_mutation(self.root_phys());
        with_prepared_user_leaf_mut(self.root_phys(), start, |entry| {
            if !entry.is_unused() {
                return Err(AddressSpaceError::AlreadyMapped);
            }
            entry.set_addr(PhysAddr::new(frame_phys), page_flags);
            Ok(())
        })?;

        drop(mutation);
        Ok(())
    }

    /// Revokes and frees pager-owned leaves while retaining their prepared
    /// intermediate tables for a later mapping of the same VMA.
    pub fn unmap_prepared_pager_fault_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<usize, AddressSpaceError> {
        if page_count == 0 {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }
        validate_user_page_range(start, page_count)?;
        let range_end = pager_fault_range_end(start, page_count)?;
        let leaves = self.pager_fault_leaves_in_range(start.as_u64(), range_end, page_count)?;
        if leaves.len() != page_count {
            return Err(AddressSpaceError::NotMapped);
        }

        let mut frames = Vec::new();
        frames
            .try_reserve_exact(page_count)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        for (page_index, leaf) in leaves.iter().copied().enumerate() {
            let virt = page_addr(start, page_index)?;
            if leaf.virtual_address != virt.as_u64() {
                return Err(AddressSpaceError::NotMapped);
            }
            if self.translate_user(virt).map(|phys| phys.as_u64()) != Some(leaf.physical_address) {
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
            frames.push(leaf.physical_address);
        }
        frames.sort_unstable();
        if frames.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }

        let mutation = begin_address_space_mutation(self.root_phys());
        for leaf in leaves.iter().copied() {
            let virt = VirtAddr::new(leaf.virtual_address);
            let unmapped = self
                .unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            debug_assert_eq!(unmapped.as_u64(), leaf.physical_address);
        }
        let _flushed_mutation = mutation.flush_for_reclaim();
        free_owned_frames_exact(&frames);
        Ok(page_count)
    }

    /// Updates protection on the present subset of a prepared pager range.
    ///
    /// Missing leaves are the normal demand state and retain no PTE rights;
    /// every present leaf must still have exact pager-ledger ownership before
    /// any mutation is published.
    pub fn protect_present_prepared_pager_fault_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<usize, AddressSpaceError> {
        if page_count == 0 {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }
        validate_user_page_range(start, page_count)?;
        let page_flags = normalize_user_page_flags(flags)?;
        let mut present = 0;
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            match self.pager_fault_leaf_at(virt.as_u64()) {
                Some(leaf) => {
                    if self.translate_user(virt).map(|phys| phys.as_u64())
                        != Some(leaf.physical_address)
                    {
                        return Err(AddressSpaceError::InvalidFrameOwnership);
                    }
                    present += 1;
                }
                None if matches!(self.lookup_user_page_state(virt), UserPageLookup::MissingPt) => {}
                None => return Err(AddressSpaceError::NotMapped),
            }
        }

        let _mutation = begin_address_space_mutation(self.root_phys());
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            if self.pager_fault_leaf_at(virt.as_u64()).is_some() {
                let tagged = self
                    .translate_user_with_flags(virt)
                    .is_some_and(|(_, flags)| flags.contains(IRQ_OFF_PAGER_FAULT_LEAF));
                self.protect_user_page(
                    virt,
                    page_flags
                        | if tagged {
                            IRQ_OFF_PAGER_FAULT_LEAF
                        } else {
                            PageTableFlags::empty()
                        },
                )?;
            }
        }
        Ok(present)
    }

    /// Revokes the present subset of a prepared pager range and frees only
    /// frames owned by the exact, normal-time-reserved pager-leaf ledger.
    pub fn unmap_present_prepared_pager_fault_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<usize, AddressSpaceError> {
        if page_count == 0 {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }
        validate_user_page_range(start, page_count)?;
        let range_end = pager_fault_range_end(start, page_count)?;
        let leaves = self.pager_fault_leaves_in_range(start.as_u64(), range_end, page_count)?;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(leaves.len())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let mut leaf_index = 0;
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            match leaves.get(leaf_index).copied() {
                Some(leaf) if leaf.virtual_address == virt.as_u64() => {
                    if self.translate_user(virt).map(|phys| phys.as_u64())
                        != Some(leaf.physical_address)
                    {
                        return Err(AddressSpaceError::InvalidFrameOwnership);
                    }
                    frames.push(leaf.physical_address);
                    leaf_index += 1;
                }
                _ if matches!(self.lookup_user_page_state(virt), UserPageLookup::MissingPt) => {}
                _ => return Err(AddressSpaceError::NotMapped),
            }
        }
        if leaf_index != leaves.len() {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        frames.sort_unstable();
        if frames.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }

        let mutation = begin_address_space_mutation(self.root_phys());
        for leaf in leaves.iter().copied() {
            let virt = VirtAddr::new(leaf.virtual_address);
            let unmapped = self
                .unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            if unmapped.as_u64() != leaf.physical_address {
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
        }
        let _flushed_mutation = mutation.flush_for_reclaim();
        free_owned_frames_exact(&frames);
        Ok(frames.len())
    }

    fn pager_fault_leaf_at(&self, virtual_address: u64) -> Option<PagerFaultLeaf> {
        self.irq_off_pager_fault_leaf_at(VirtAddr::new(virtual_address))
    }

    fn pager_fault_leaves_in_range(
        &self,
        start: u64,
        end: u64,
        maximum_count: usize,
    ) -> Result<Vec<PagerFaultLeaf>, AddressSpaceError> {
        let mut leaves = Vec::new();
        leaves
            .try_reserve_exact(maximum_count)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let mut virtual_address = start;
        while virtual_address < end {
            if let Some(leaf) = self.pager_fault_leaf_at(virtual_address) {
                if leaves.len() == maximum_count {
                    return Err(AddressSpaceError::InvalidFrameOwnership);
                }
                leaves.push(leaf);
            }
            virtual_address = virtual_address
                .checked_add(PAGE_4KIB_U64)
                .ok_or(AddressSpaceError::AddressOverflow)?;
        }
        Ok(leaves)
    }

    fn irq_off_pager_fault_leaf_at(&self, virt: VirtAddr) -> Option<PagerFaultLeaf> {
        let (frame, flags) = self.translate_user_with_flags(virt)?;
        flags
            .contains(IRQ_OFF_PAGER_FAULT_LEAF)
            .then_some(PagerFaultLeaf {
                virtual_address: virt.as_u64(),
                physical_address: frame.as_u64(),
            })
    }

    /// Collects only leaves whose physical-frame ownership came from the
    /// IRQ-off CAS path.  This normal-context page-table walk replaces a fault
    /// path Vec mutation, so the PTE tag is the ownership ledger for those
    /// frames during clone and address-space retirement.
    pub(crate) fn irq_off_pager_fault_leaves(&self) -> Result<Vec<PagerFaultLeaf>, AddressSpaceError> {
        let mut leaves = Vec::new();
        if self.pml4_frame_phys == 0 {
            return Ok(leaves);
        }
        let root = self.root_table_ref();
        let pml4_entry = &root[USER_PML4_INDEX];
        if pml4_entry.is_unused() {
            return Ok(leaves);
        }
        if pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::HugePageConflict);
        }
        // SAFETY: this normal-context owner walks its immutable page-table
        // hierarchy.  VMA writers serialize destructive changes externally.
        let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4_entry.addr()) };
        for p3 in 0..ENTRIES_PER_TABLE {
            let pdpt_entry = &pdpt[p3];
            if pdpt_entry.is_unused() {
                continue;
            }
            if pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                return Err(AddressSpaceError::HugePageConflict);
            }
            let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
            for p2 in 0..ENTRIES_PER_TABLE {
                let pd_entry = &pd[p2];
                if pd_entry.is_unused() {
                    continue;
                }
                if pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                    return Err(AddressSpaceError::HugePageConflict);
                }
                let pt = unsafe { kernel_vm::phys_to_table_ref(pd_entry.addr()) };
                for p1 in 0..ENTRIES_PER_TABLE {
                    let entry = &pt[p1];
                    if entry.is_unused() || !entry.flags().contains(IRQ_OFF_PAGER_FAULT_LEAF) {
                        continue;
                    }
                    if !entry.flags().contains(PageTableFlags::PRESENT) {
                        return Err(AddressSpaceError::InvalidFrameOwnership);
                    }
                    let virtual_address = ((USER_PML4_INDEX as u64) << 39)
                        | ((p3 as u64) << 30)
                        | ((p2 as u64) << 21)
                        | ((p1 as u64) << 12);
                    leaves.push(PagerFaultLeaf {
                        virtual_address,
                        physical_address: entry.addr().as_u64(),
                    });
                }
            }
        }
        Ok(leaves)
    }

}

fn pager_fault_range_end(start: VirtAddr, page_count: usize) -> Result<u64, AddressSpaceError> {
    let span = (page_count as u64)
        .checked_mul(PAGE_4KIB_U64)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    start
        .as_u64()
        .checked_add(span)
        .ok_or(AddressSpaceError::AddressOverflow)
}

/// Calls `f` with the atomic representation of an existing 4 KiB user leaf.
///
/// The VMA publication permit serializes topology changes with this helper.
/// Its only mutation is a zero-to-present CAS; normal VMA writers continue to
/// use `with_prepared_user_leaf_mut` under the address-space mutation guard.
fn with_prepared_user_leaf_atomic<R>(
    pml4_phys: PhysAddr,
    virt: VirtAddr,
    f: impl FnOnce(&AtomicU64) -> Result<R, AddressSpaceError>,
) -> Result<R, AddressSpaceError> {
    // SAFETY: the current root is active on this CPU and the publication permit
    // excludes concurrent topology mutation for the target prepared leaf.
    let root = unsafe { kernel_vm::phys_to_table_ref(pml4_phys) };
    let pml4_entry = &root[p4_index(virt)];
    if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: the permit keeps each prepared parent stable until the CAS ends.
    let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4_entry.addr()) };
    let pdpt_entry = &pdpt[p3_index(virt)];
    if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: as above for the next prepared non-huge parent.
    let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
    let pd_entry = &pd[p2_index(virt)];
    if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: the prepared PTE is a repr(transparent) `u64` and naturally
    // aligned.  Only this CAS path accesses it atomically while the permit is
    // held; normal writers first withdraw and drain that permit.
    let pt = unsafe { kernel_vm::phys_to_table_ref(pd_entry.addr()) };
    let raw = &pt[p1_index(virt)] as *const PageTableEntry as *mut u64;
    let leaf = unsafe { AtomicU64::from_ptr(raw) };
    f(leaf)
}

/// Calls `f` with an existing 4 KiB user leaf entry.
fn with_prepared_user_leaf_mut<R>(
    pml4_phys: PhysAddr,
    virt: VirtAddr,
    f: impl FnOnce(&mut PageTableEntry) -> Result<R, AddressSpaceError>,
) -> Result<R, AddressSpaceError> {
    // SAFETY: the caller serializes this address space and supplied its exact
    // owned root. The hierarchy below is validated before each direct-map use.
    let root = unsafe { kernel_vm::phys_to_table_mut(pml4_phys) };
    let pml4_entry = &mut root[p4_index(virt)];
    if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: the present non-huge parent entry was installed by the same
    // address space and remains stable for this serialized PTE update.
    let pdpt = unsafe { kernel_vm::phys_to_table_mut(pml4_entry.addr()) };
    let pdpt_entry = &mut pdpt[p3_index(virt)];
    if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: as above, this present non-huge entry names an owned user table.
    let pd = unsafe { kernel_vm::phys_to_table_mut(pdpt_entry.addr()) };
    let pd_entry = &mut pd[p2_index(virt)];
    if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::NotMapped);
    }
    // SAFETY: a prepared leaf requires this final present non-huge page table.
    let pt = unsafe { kernel_vm::phys_to_table_mut(pd_entry.addr()) };
    f(&mut pt[p1_index(virt)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pager_fault_range_end_preserves_page_exactness() {
        let start = VirtAddr::new(USER_SPACE_BASE + PAGE_4KIB_U64);
        assert_eq!(
            pager_fault_range_end(start, 3),
            Ok(USER_SPACE_BASE + 4 * PAGE_4KIB_U64)
        );
    }
}
