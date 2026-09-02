//! Pager-fault leaf preparation and bounded PTE ownership transfer.
//!
//! This module deliberately contains both sides of the boundary: normal-time
//! VMA preparation can allocate intermediate tables, while the reply-time
//! install only changes an already-present leaf and a fixed array ledger.
use super::*;
use x86_64::structures::paging::page_table::PageTableEntry;

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

        let pager_leaf_limit = self
            .pager_fault_leaf_limit
            .checked_add(page_count)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        if self.pager_fault_leaves.capacity() < pager_leaf_limit {
            self.pager_fault_leaves
                .try_reserve_exact(pager_leaf_limit - self.pager_fault_leaves.len())
                .map_err(|_| AddressSpaceError::OutOfFrames)?;
        }

        let table_capacity = page_count
            .checked_mul(3)
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
            for page_index in 0..page_count {
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
                if !pt[p1_index(virt)].is_unused() {
                    return Err(AddressSpaceError::AlreadyMapped);
                }
            }
            Ok(())
        })();
        if let Err(error) = admission {
            rollback_external_user_pages(self, &[], &published_tables, mutation);
            return Err(error);
        }

        drop(mutation);
        self.pager_fault_leaf_limit = pager_leaf_limit;
        debug_assert!(self.pager_fault_leaves.capacity() >= self.pager_fault_leaf_limit);
        Ok(())
    }

    /// Installs one pager-granted frame into a normal-time prepared user leaf.
    ///
    /// # Exception-path contract
    ///
    /// `frame_phys` must come from an exact, unconsumed frame grant. This path
    /// performs no allocation or blocking lookup: normal-time VMA preparation
    /// has already reserved every ledger slot it may consume.
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
        if self.pager_fault_leaves.len() >= self.pager_fault_leaf_limit
            || self.pager_fault_leaves.len() == self.pager_fault_leaves.capacity()
        {
            return Err(AddressSpaceError::OutOfFrames);
        }
        if self.pager_fault_leaf_index(start.as_u64()).is_some() {
            return Err(AddressSpaceError::AlreadyMapped);
        }
        if self
            .pager_fault_leaves
            .iter()
            .any(|leaf| leaf.physical_address == frame_phys)
        {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }

        let page_flags = normalize_user_page_flags(flags)?;
        let mutation = begin_address_space_mutation(self.root_phys());
        with_prepared_user_leaf_mut(self.root_phys(), start, |entry| {
            if !entry.is_unused() {
                return Err(AddressSpaceError::AlreadyMapped);
            }
            entry.set_addr(PhysAddr::new(frame_phys), page_flags);
            Ok(())
        })?;

        self.record_pager_fault_leaf(start.as_u64(), frame_phys)
            .expect("fault-leaf admission was preflighted before PTE publication");
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
        let pager_leaf_limit = self
            .pager_fault_leaf_limit
            .checked_sub(page_count)
            .ok_or(AddressSpaceError::InvalidFrameOwnership)?;

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
        let removed = self.remove_pager_fault_leaves_in_range(start.as_u64(), range_end);
        assert_eq!(
            removed,
            leaves.len(),
            "pager-fault range preflight lost exact leaf ownership"
        );
        let _flushed_mutation = mutation.flush_for_reclaim();
        free_owned_frames_exact(&frames);
        self.pager_fault_leaf_limit = pager_leaf_limit;
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
                self.protect_user_page(virt, page_flags)?;
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
        let pager_leaf_limit = self
            .pager_fault_leaf_limit
            .checked_sub(page_count)
            .ok_or(AddressSpaceError::InvalidFrameOwnership)?;

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
        let removed = self.remove_pager_fault_leaves_in_range(start.as_u64(), range_end);
        assert_eq!(
            removed,
            leaves.len(),
            "pager-fault subset preflight lost exact leaf ownership"
        );
        let _flushed_mutation = mutation.flush_for_reclaim();
        free_owned_frames_exact(&frames);
        self.pager_fault_leaf_limit = pager_leaf_limit;
        Ok(frames.len())
    }

    fn pager_fault_leaf_index(&self, virtual_address: u64) -> Option<usize> {
        self.pager_fault_leaves
            .iter()
            .position(|leaf| leaf.virtual_address == virtual_address)
    }

    fn pager_fault_leaf_at(&self, virtual_address: u64) -> Option<PagerFaultLeaf> {
        self.pager_fault_leaf_index(virtual_address)
            .map(|index| self.pager_fault_leaves[index])
    }

    fn pager_fault_leaves_in_range(
        &self,
        start: u64,
        end: u64,
        maximum_count: usize,
    ) -> Result<Vec<PagerFaultLeaf>, AddressSpaceError> {
        let mut leaves = Vec::new();
        leaves
            .try_reserve_exact(maximum_count.min(self.pager_fault_leaves.len()))
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        for leaf in self
            .pager_fault_leaves
            .iter()
            .copied()
            .filter(|leaf| (start..end).contains(&leaf.virtual_address))
        {
            if leaves.len() == maximum_count {
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
            leaves.push(leaf);
        }
        leaves.sort_unstable_by_key(|leaf| leaf.virtual_address);
        Ok(leaves)
    }

    fn remove_pager_fault_leaves_in_range(&mut self, start: u64, end: u64) -> usize {
        let previous_len = self.pager_fault_leaves.len();
        self.pager_fault_leaves
            .retain(|leaf| !(start..end).contains(&leaf.virtual_address));
        previous_len - self.pager_fault_leaves.len()
    }

    fn record_pager_fault_leaf(
        &mut self,
        virtual_address: u64,
        physical_address: u64,
    ) -> Result<(), AddressSpaceError> {
        if physical_address == 0 || !physical_address.is_multiple_of(PAGE_4KIB_U64) {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }
        if self.pager_fault_leaves.len() >= self.pager_fault_leaf_limit
            || self.pager_fault_leaves.len() == self.pager_fault_leaves.capacity()
        {
            return Err(AddressSpaceError::OutOfFrames);
        }
        if self.pager_fault_leaf_index(virtual_address).is_some() {
            return Err(AddressSpaceError::AlreadyMapped);
        }
        if self
            .pager_fault_leaves
            .iter()
            .any(|leaf| leaf.physical_address == physical_address)
        {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }

        // The explicit capacity check above makes this push allocator-free.
        self.pager_fault_leaves.push(PagerFaultLeaf {
            virtual_address,
            physical_address,
        });
        Ok(())
    }

    fn release_pager_fault_leaf(
        &mut self,
        virtual_address: u64,
        physical_address: u64,
    ) -> Result<(), AddressSpaceError> {
        let Some(index) = self.pager_fault_leaf_index(virtual_address) else {
            return Err(AddressSpaceError::NotMapped);
        };
        if self.pager_fault_leaves[index].physical_address != physical_address {
            return Err(AddressSpaceError::InvalidFrameOwnership);
        }

        self.pager_fault_leaves.swap_remove(index);
        Ok(())
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
    fn pager_fault_leaf_ledger_uses_normal_time_reservation_without_fault_path_growth() {
        const TEST_LEAF_LIMIT: usize =
            crate::memory::frame_capability::MAX_PREALLOCATED_PAGER_FAULT_FRAMES + 7;

        let mut space = ProcessAddressSpace::empty_for_tests();
        space
            .pager_fault_leaves
            .try_reserve_exact(TEST_LEAF_LIMIT)
            .expect("test pager-leaf metadata reservation");
        space.pager_fault_leaf_limit = TEST_LEAF_LIMIT;
        let reserved_capacity = space.pager_fault_leaves.capacity();
        let start = USER_SPACE_BASE;

        assert_eq!(
            space.record_pager_fault_leaf(start, 0),
            Err(AddressSpaceError::InvalidFrameOwnership)
        );
        assert_eq!(space.record_pager_fault_leaf(start, PAGE_4KIB_U64), Ok(()));
        assert_eq!(
            space.record_pager_fault_leaf(start, 2 * PAGE_4KIB_U64),
            Err(AddressSpaceError::AlreadyMapped)
        );
        assert_eq!(
            space.record_pager_fault_leaf(start + PAGE_4KIB_U64, PAGE_4KIB_U64),
            Err(AddressSpaceError::InvalidFrameOwnership)
        );

        for index in 1..TEST_LEAF_LIMIT {
            assert_eq!(
                space.record_pager_fault_leaf(
                    start + (index as u64) * PAGE_4KIB_U64,
                    (index as u64 + 1) * PAGE_4KIB_U64,
                ),
                Ok(())
            );
            assert_eq!(space.pager_fault_leaves.capacity(), reserved_capacity);
        }
        assert_eq!(space.pager_fault_leaves.len(), TEST_LEAF_LIMIT);
        assert_eq!(
            space.record_pager_fault_leaf(
                start + (TEST_LEAF_LIMIT as u64) * PAGE_4KIB_U64,
                (TEST_LEAF_LIMIT as u64 + 1) * PAGE_4KIB_U64,
            ),
            Err(AddressSpaceError::OutOfFrames)
        );

        assert_eq!(space.release_pager_fault_leaf(start, PAGE_4KIB_U64), Ok(()));
        assert_eq!(space.pager_fault_leaves.len(), TEST_LEAF_LIMIT - 1);
        assert_eq!(
            space.record_pager_fault_leaf(
                start + (TEST_LEAF_LIMIT as u64) * PAGE_4KIB_U64,
                (TEST_LEAF_LIMIT as u64 + 1) * PAGE_4KIB_U64,
            ),
            Ok(())
        );
        assert_eq!(space.pager_fault_leaves.capacity(), reserved_capacity);
    }

    #[test]
    fn pager_fault_leaf_range_collection_and_removal_are_batched_and_exact() {
        let mut space = ProcessAddressSpace::empty_for_tests();
        space
            .pager_fault_leaves
            .try_reserve_exact(4)
            .expect("test pager-leaf metadata reservation");
        space.pager_fault_leaf_limit = 4;
        let start = USER_SPACE_BASE;

        assert_eq!(
            space.record_pager_fault_leaf(start + 2 * PAGE_4KIB_U64, 3 * PAGE_4KIB_U64),
            Ok(())
        );
        assert_eq!(
            space.record_pager_fault_leaf(start + 4 * PAGE_4KIB_U64, 5 * PAGE_4KIB_U64),
            Ok(())
        );
        assert_eq!(space.record_pager_fault_leaf(start, PAGE_4KIB_U64), Ok(()));

        let end = start + 3 * PAGE_4KIB_U64;
        let leaves = space
            .pager_fault_leaves_in_range(start, end, 3)
            .expect("collect pager leaves");
        let virtual_addresses = leaves
            .iter()
            .map(|leaf| leaf.virtual_address)
            .collect::<Vec<_>>();
        assert_eq!(
            virtual_addresses.as_slice(),
            &[start, start + 2 * PAGE_4KIB_U64]
        );
        assert_eq!(space.remove_pager_fault_leaves_in_range(start, end), 2);
        assert_eq!(space.pager_fault_leaves.len(), 1);
        assert_eq!(
            space.pager_fault_leaves[0].virtual_address,
            start + 4 * PAGE_4KIB_U64
        );
    }
}
