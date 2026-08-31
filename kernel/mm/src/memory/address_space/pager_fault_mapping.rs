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

        let mut frames = Vec::new();
        frames
            .try_reserve_exact(page_count)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            let leaf = self
                .pager_fault_leaf_at(virt.as_u64())
                .ok_or(AddressSpaceError::NotMapped)?;
            if self.translate_user(virt).map(|phys| phys.as_u64()) != Some(leaf.physical_address)
                || frames.contains(&leaf.physical_address)
            {
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
            frames.push(leaf.physical_address);
        }

        let mutation = begin_address_space_mutation(self.root_phys());
        for (page_index, frame_phys) in frames.iter().copied().enumerate() {
            let virt = page_addr(start, page_index)?;
            let unmapped = self
                .unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            debug_assert_eq!(unmapped.as_u64(), frame_phys);
            self.release_pager_fault_leaf(virt.as_u64(), frame_phys)
                .expect("pager-fault leaf preflight lost exact ownership");
        }
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

        let present_capacity = page_count.min(self.pager_fault_leaves.len());
        let mut virtual_addresses = Vec::new();
        let mut frames = Vec::new();
        virtual_addresses
            .try_reserve_exact(present_capacity)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        frames
            .try_reserve_exact(present_capacity)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            match self.pager_fault_leaf_at(virt.as_u64()) {
                Some(leaf) => {
                    if self.translate_user(virt).map(|phys| phys.as_u64())
                        != Some(leaf.physical_address)
                        || frames.contains(&leaf.physical_address)
                    {
                        return Err(AddressSpaceError::InvalidFrameOwnership);
                    }
                    virtual_addresses.push(virt.as_u64());
                    frames.push(leaf.physical_address);
                }
                None if matches!(self.lookup_user_page_state(virt), UserPageLookup::MissingPt) => {}
                None => return Err(AddressSpaceError::NotMapped),
            }
        }

        let mutation = begin_address_space_mutation(self.root_phys());
        for (virtual_address, frame_phys) in virtual_addresses
            .iter()
            .copied()
            .zip(frames.iter().copied())
        {
            let virt = VirtAddr::new(virtual_address);
            let unmapped = self
                .unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            if unmapped.as_u64() != frame_phys
                || self
                    .release_pager_fault_leaf(virtual_address, frame_phys)
                    .is_err()
            {
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
        }
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
}
