//! Transactional user address-space and page-table ownership.
//!
//! - **Owner:** `kernel-mm` owns PTE mutation; `syscalld` owns mapping policy
//!   and `kernel-ps` owns process-generation lifetime.
//! - **Boundary:** User ranges, protections, backing frames, and replacement
//!   plans are untrusted until complete-span admission.
//! - **Lifecycle:** Reserve/validate the full plan, install atomically, retain
//!   backing, then unmap/protect/reclaim exactly once.
//! - **Concurrency:** Callers serialize process address-space mutation and hold
//!   an exact process generation; no service call occurs under page-table
//!   mutation state.
//! - **Failure:** Overflow, alias, W+X, partial span, and allocation failure
//!   leave the prior mapping unchanged.
//! - **Forbidden:** No destructive `MAP_FIXED` pre-cleanup, partial protection,
//!   guest pointer as frame authority, or hidden identity mapping.
//! - **Evidence:** `memory-map` and `user-memory-access`.
use alloc::vec::Vec;
use core::ptr;

use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::instructions::{interrupts, tlb};
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{PageTable, PageTableFlags};

use kernel_hal::api::arch::tlb::{AddressSpaceMutationGuard, begin_address_space_mutation};

mod atomic_user;
mod owned_frames;
mod pager_fault_mapping;
mod rollback;
mod user_copy;
pub use pager_fault_mapping::map_current_prepared_pager_fault_frame_at;
use owned_frames::{
    FRAME_BATCH_CHUNK, free_frame_buffer_tail, free_owned_frames_exact, free_owned_frames_logged,
    free_owned_frames_silently, free_rollback_frames_exact, remove_owned_frame, track_owned_frame,
};
use rollback::{rollback_external_user_pages, rollback_user_pages};
pub use user_copy::{ValidatedUserRead, ValidatedUserWrite};

use crate::memory::{kernel_vm, phys};

const ENTRIES_PER_TABLE: usize = 512;
const PAGE_4KIB: usize = 4096;
const PAGE_4KIB_U64: u64 = PAGE_4KIB as u64;
const USER_PML4_INDEX: usize = 1;
pub const USER_SPACE_BASE: u64 = (USER_PML4_INDEX as u64) << 39;
pub const USER_SPACE_END_EXCLUSIVE: u64 = ((USER_PML4_INDEX + 1) as u64) << 39;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PagerFaultLeaf {
    virtual_address: u64,
    physical_address: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressSpaceError {
    ZeroSizedAllocation,
    AddressOverflow,
    AddressOutOfRange,
    AddressNotPageAligned,
    AlreadyMapped,
    NotMapped,
    ProtectionViolation,
    HugePageConflict,
    OutOfFrames,
    InvalidFrameOwnership,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserRegion {
    pub start: VirtAddr,
    pub page_count: usize,
}

impl UserRegion {
    pub fn len_bytes(&self) -> usize {
        self.page_count.saturating_mul(PAGE_4KIB)
    }

    pub fn end(&self) -> VirtAddr {
        VirtAddr::new(self.start.as_u64() + self.len_bytes() as u64)
    }
}

#[derive(Debug)]
pub struct ProcessAddressSpace {
    pml4_frame_phys: u64,
    next_user_addr: u64,
    owned_frames: Vec<u64>,
    regions: Vec<UserRegion>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PublishedUserTable {
    parent_phys: u64,
    parent_index: usize,
    table_phys: u64,
}

impl ProcessAddressSpace {
    #[inline(never)]
    pub fn new() -> Result<Self, AddressSpaceError> {
        let pml4_phys = phys::alloc_frame().ok_or(AddressSpaceError::OutOfFrames)?;
        let root = unsafe { kernel_vm::phys_to_table_mut(pml4_phys) };
        root.zero();

        interrupts::without_interrupts(|| {
            let kernel_pml4 = kernel_vm::KERNEL_PML4.lock();
            for index in 0..ENTRIES_PER_TABLE {
                let src = &kernel_pml4.pml4[index];
                let dst = &mut root[index];
                if src.is_unused() {
                    dst.set_unused();
                } else {
                    dst.set_addr(src.addr(), src.flags());
                }
            }
        });

        Ok(Self {
            pml4_frame_phys: pml4_phys.as_u64(),
            next_user_addr: USER_SPACE_BASE,
            owned_frames: {
                let mut frames = Vec::new();
                track_owned_frame(&mut frames, pml4_phys.as_u64())?;
                frames
            },
            regions: Vec::new(),
        })
    }

    pub fn root_phys(&self) -> PhysAddr {
        PhysAddr::new(self.pml4_frame_phys)
    }

    #[cfg(any(test, debug_assertions))]
    pub fn empty_for_tests() -> Self {
        Self {
            pml4_frame_phys: 0,
            next_user_addr: USER_SPACE_BASE,
            owned_frames: Vec::new(),
            regions: Vec::new(),
        }
    }

    pub fn clone_user_space(&self) -> Result<Self, AddressSpaceError> {
        let mut cloned = Self::new()?;
        cloned.next_user_addr = self.next_user_addr;

        for region in &self.regions {
            for page_index in 0..region.page_count {
                let virt = page_addr(region.start, page_index)?;
                let (src_phys, flags) = self
                    .translate_user_with_flags(virt)
                    .ok_or(AddressSpaceError::NotMapped)?;
                cloned.map_zeroed_user_pages_at(virt, 1, flags)?;
                let dst_phys = cloned
                    .translate_user(virt)
                    .ok_or(AddressSpaceError::NotMapped)?;
                unsafe {
                    ptr::copy_nonoverlapping(
                        higher_half_ptr(src_phys),
                        higher_half_ptr(dst_phys),
                        PAGE_4KIB,
                    );
                }
            }
        }
        // IRQ-off anonymous leaves are owned by their tagged PTEs rather than
        // the legacy Vec ledger: collect them only in this normal clone path.
        for leaf in self.irq_off_pager_fault_leaves()? {
            let virt = VirtAddr::new(leaf.virtual_address);
            let (src_phys, flags) = self
                .translate_user_with_flags(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            if src_phys.as_u64() != leaf.physical_address {
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
            cloned.map_zeroed_user_pages_at(virt, 1, flags)?;
            let dst_phys = cloned
                .translate_user(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            unsafe {
                ptr::copy_nonoverlapping(
                    higher_half_ptr(src_phys),
                    higher_half_ptr(dst_phys),
                    PAGE_4KIB,
                );
            }
        }

        Ok(cloned)
    }

    pub fn regions(&self) -> &[UserRegion] {
        &self.regions
    }

    pub fn alloc_user_bytes(
        &mut self,
        byte_len: usize,
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        let page_count = byte_len_to_page_count(byte_len)?;
        self.alloc_user_pages(page_count, flags)
    }

    pub fn alloc_user_pages(
        &mut self,
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        if page_count == 0 {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }

        let start = align_up(self.next_user_addr, PAGE_4KIB_U64)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let region = self.map_zeroed_user_pages_at(VirtAddr::new(start), page_count, flags)?;
        self.next_user_addr = region.end().as_u64();
        Ok(region)
    }

    pub fn map_zeroed_user_bytes_at(
        &mut self,
        start: VirtAddr,
        byte_len: usize,
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        let page_count = byte_len_to_page_count(byte_len)?;
        self.map_zeroed_user_pages_at(start, page_count, flags)
    }

    pub fn map_zeroed_user_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        validate_user_page_range(start, page_count)?;

        let page_flags = normalize_user_page_flags(flags)?;
        let table_capacity = page_count
            .checked_mul(3)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let mut mapped_pages = Vec::new();
        let mut published_tables = Vec::new();
        mapped_pages
            .try_reserve_exact(page_count)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        published_tables
            .try_reserve_exact(table_capacity)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        self.owned_frames
            .try_reserve_exact(
                page_count
                    .checked_add(table_capacity)
                    .ok_or(AddressSpaceError::AddressOverflow)?,
            )
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        self.regions
            .try_reserve(1)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let mutation = begin_address_space_mutation(self.root_phys());

        // Frames are allocated in chunks under one `PHYS_ALLOCATOR` acquisition
        // instead of one per page: this loop maps each page independently, so
        // nothing downstream needs the frames to be physically adjacent, and a
        // large mapping otherwise pays one tracked-lock acquire/release pair
        // per 4 KiB page.
        let mut frame_buffer = [PhysAddr::new(0); FRAME_BATCH_CHUNK];
        let mut frame_buffer_len = 0usize;
        let mut frame_buffer_pos = 0usize;

        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            if self.translate_user(virt).is_some() {
                free_frame_buffer_tail(&frame_buffer[frame_buffer_pos..frame_buffer_len]);
                rollback_user_pages(self, &mapped_pages, &published_tables, mutation);
                return Err(AddressSpaceError::AlreadyMapped);
            }

            if frame_buffer_pos == frame_buffer_len {
                let want = (page_count - page_index).min(FRAME_BATCH_CHUNK);
                frame_buffer_len = phys::alloc_frames_batch(&mut frame_buffer[..want]);
                frame_buffer_pos = 0;
                if frame_buffer_len == 0 {
                    rollback_user_pages(self, &mapped_pages, &published_tables, mutation);
                    return Err(AddressSpaceError::OutOfFrames);
                }
            }
            let frame_phys = frame_buffer[frame_buffer_pos];
            frame_buffer_pos += 1;

            unsafe {
                ptr::write_bytes(higher_half_ptr(frame_phys), 0, PAGE_4KIB);
            }

            if let Err(err) =
                self.map_user_page(virt, frame_phys, page_flags, &mut published_tables)
            {
                phys::free_frame(frame_phys);
                free_frame_buffer_tail(&frame_buffer[frame_buffer_pos..frame_buffer_len]);
                rollback_user_pages(self, &mapped_pages, &published_tables, mutation);
                return Err(err);
            }

            mapped_pages.push((virt, frame_phys.as_u64()));
        }

        for &(_, frame_phys) in &mapped_pages {
            if let Err(err) = track_owned_frame(&mut self.owned_frames, frame_phys) {
                rollback_user_pages(self, &mapped_pages, &published_tables, mutation);
                return Err(err);
            }
        }

        let region = UserRegion { start, page_count };
        self.regions.push(region);
        Ok(region)
    }

    pub fn map_existing_user_pages_at(
        &mut self,
        start: VirtAddr,
        frames: &[u64],
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        self.map_existing_user_pages_at_with_leaf_flags(
            start,
            frames,
            flags,
            PageTableFlags::empty(),
        )
    }

    /// Map page-aligned pre-owned frames with the x86 4-KiB PAT selector.
    /// The caller must ensure every alias of these external frames uses the
    /// same write-combine memory type.
    pub fn map_existing_user_pages_at_write_combine(
        &mut self,
        start: VirtAddr,
        frames: &[u64],
        flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        self.map_existing_user_pages_at_with_leaf_flags(
            start,
            frames,
            flags,
            PageTableFlags::HUGE_PAGE,
        )
    }

    fn map_existing_user_pages_at_with_leaf_flags(
        &mut self,
        start: VirtAddr,
        frames: &[u64],
        flags: PageTableFlags,
        leaf_flags: PageTableFlags,
    ) -> Result<UserRegion, AddressSpaceError> {
        if frames.is_empty() {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }

        validate_user_page_range(start, frames.len())?;
        let page_flags = normalize_user_page_flags(flags)? | leaf_flags;
        let table_capacity = frames
            .len()
            .checked_mul(3)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let mut mapped_pages = Vec::new();
        let mut published_tables = Vec::new();
        mapped_pages
            .try_reserve_exact(frames.len())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        published_tables
            .try_reserve_exact(table_capacity)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        self.owned_frames
            .try_reserve_exact(table_capacity)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        self.regions
            .try_reserve(1)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let mutation = begin_address_space_mutation(self.root_phys());

        for (page_index, frame_phys) in frames.iter().copied().enumerate() {
            let virt = page_addr(start, page_index)?;
            if self.translate_user(virt).is_some() {
                rollback_external_user_pages(self, &mapped_pages, &published_tables, mutation);
                return Err(AddressSpaceError::AlreadyMapped);
            }
            if let Err(err) = self.map_user_page(
                virt,
                PhysAddr::new(frame_phys),
                page_flags,
                &mut published_tables,
            ) {
                rollback_external_user_pages(self, &mapped_pages, &published_tables, mutation);
                return Err(err);
            }
            mapped_pages.push(virt);
        }

        let region = UserRegion {
            start,
            page_count: frames.len(),
        };
        self.regions.push(region);
        Ok(region)
    }

    pub fn unmap_user_bytes(
        &mut self,
        start: VirtAddr,
        byte_len: usize,
    ) -> Result<usize, AddressSpaceError> {
        let page_count = byte_len_to_page_count(byte_len)?;
        self.unmap_user_pages_at(start, page_count)
    }

    pub fn unmap_user_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<usize, AddressSpaceError> {
        validate_user_page_range(start, page_count)?;

        let updated_regions = self.plan_region_subtraction(start, page_count)?;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(page_count)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            let phys = self
                .translate_user(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            if phys.as_u64() % PAGE_4KIB_U64 != 0 {
                return Err(AddressSpaceError::NotMapped);
            }
            if !self.owned_frames.contains(&phys.as_u64()) || frames.contains(&phys.as_u64()) {
                return Err(AddressSpaceError::InvalidFrameOwnership);
            }
            frames.push(phys.as_u64());
        }

        let mutation = begin_address_space_mutation(self.root_phys());
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            let unmapped = self
                .unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            debug_assert_eq!(Some(unmapped.as_u64()), frames.get(page_index).copied());
        }
        let _flushed_mutation = mutation.flush_for_reclaim();

        for &frame_phys in &frames {
            remove_owned_frame(&mut self.owned_frames, frame_phys)?;
        }
        free_owned_frames_exact(&frames);

        self.regions = updated_regions;
        Ok(page_count)
    }

    pub fn unmap_user_pages_without_free_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<usize, AddressSpaceError> {
        validate_user_page_range(start, page_count)?;

        let updated_regions = self.plan_region_subtraction(start, page_count)?;
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            let phys = self
                .translate_user(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            if !phys.as_u64().is_multiple_of(PAGE_4KIB_U64) {
                return Err(AddressSpaceError::NotMapped);
            }
        }

        let _mutation = begin_address_space_mutation(self.root_phys());
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            self.unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
        }

        self.regions = updated_regions;
        Ok(page_count)
    }

    pub fn protect_user_bytes(
        &mut self,
        start: VirtAddr,
        byte_len: usize,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        let page_count = byte_len_to_page_count(byte_len)?;
        self.protect_user_pages_at(start, page_count, flags)
    }

    pub fn protect_user_pages_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, page_count)?;
        let page_flags = normalize_user_page_flags(flags)?;

        if !validate_protection_span(page_count, |page_index| {
            page_addr(start, page_index).ok().is_some_and(|virt| {
                matches!(
                    self.lookup_user_page_state(virt),
                    UserPageLookup::Present { .. }
                )
            })
        }) {
            return Err(AddressSpaceError::NotMapped);
        }

        let _mutation = begin_address_space_mutation(self.root_phys());
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            self.protect_user_page(virt, page_flags)?;
        }

        Ok(())
    }

    pub fn ensure_user_region_mapped(
        &self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(start, page_count)?;
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            if self.translate_user(virt).is_none() {
                return Err(AddressSpaceError::NotMapped);
            }
        }
        Ok(())
    }

    pub fn debug_dump_user_range_state(&self, start: VirtAddr, page_count: usize, reason: &str) {
        crate::debug::println!(
            "address space range dump: reason={} start={:#x} pages={}",
            reason,
            start.as_u64(),
            page_count,
        );
        let capped = page_count.min(8);
        for page_index in 0..capped {
            let Ok(virt) = page_addr(start, page_index) else {
                break;
            };
            self.debug_dump_user_page_state(virt, reason);
        }
        if page_count > capped {
            crate::debug::println!(
                "address space range dump: truncated remaining_pages={}",
                page_count - capped
            );
        }
    }

    #[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
    pub fn debug_dump_user_page_state(&self, virt: VirtAddr, reason: &str) {
        match self.lookup_user_page_state(virt) {
            UserPageLookup::NotUser => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=not-user",
                reason,
                virt.as_u64(),
            ),
            UserPageLookup::MissingPml4 => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=missing-pml4",
                reason,
                virt.as_u64(),
            ),
            UserPageLookup::MissingPdpt => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=missing-pdpt",
                reason,
                virt.as_u64(),
            ),
            UserPageLookup::MissingPd => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=missing-pd",
                reason,
                virt.as_u64(),
            ),
            UserPageLookup::MissingPt => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=missing-pt",
                reason,
                virt.as_u64(),
            ),
            UserPageLookup::Present { phys, flags } => crate::debug::println!(
                "address space page dump: reason={} addr={:#x} state=present phys={:#x} flags={:?}",
                reason,
                virt.as_u64(),
                phys.as_u64(),
                flags,
            ),
        }
    }

    pub fn translate_user(&self, virt: VirtAddr) -> Option<PhysAddr> {
        if !is_user_addr(virt.as_u64()) {
            return None;
        }

        let (phys, _) = self.translate_user_with_flags(virt)?;
        Some(phys)
    }

    pub fn translate_user_with_flags(&self, virt: VirtAddr) -> Option<(PhysAddr, PageTableFlags)> {
        if !is_user_addr(virt.as_u64()) {
            return None;
        }

        let p4 = p4_index(virt);
        let root = self.root_table_ref();
        let pml4_entry = &root[p4];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4_entry.addr()) };
        let pdpt_entry = &pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
        let pd_entry = &pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pt = unsafe { kernel_vm::phys_to_table_ref(pd_entry.addr()) };
        let pt_entry = &pt[p1_index(virt)];
        if pt_entry.is_unused() {
            return None;
        }

        let offset = virt.as_u64() & (PAGE_4KIB_U64 - 1);
        Some((
            PhysAddr::new(pt_entry.addr().as_u64() + offset),
            pt_entry.flags(),
        ))
    }

    fn root_table_ref(&self) -> &'static PageTable {
        unsafe { kernel_vm::phys_to_table_ref(self.root_phys()) }
    }

    fn flush_if_active(&self, virt: VirtAddr) {
        let (current_frame, _) = Cr3::read();
        if current_frame.start_address() == self.root_phys() {
            tlb::flush(virt);
        }
    }

    fn map_user_page(
        &mut self,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageTableFlags,
        published_tables: &mut Vec<PublishedUserTable>,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(virt, 1)?;

        let pml4_phys = self.root_phys();
        let pdpt_phys = ensure_next_table(
            &mut self.owned_frames,
            published_tables,
            pml4_phys,
            p4_index(virt),
        )?;
        let pd_phys = ensure_next_table(
            &mut self.owned_frames,
            published_tables,
            pdpt_phys,
            p3_index(virt),
        )?;
        let pt_phys = ensure_next_table(
            &mut self.owned_frames,
            published_tables,
            pd_phys,
            p2_index(virt),
        )?;
        let pt = unsafe { kernel_vm::phys_to_table_mut(pt_phys) };

        let entry = &mut pt[p1_index(virt)];
        if !entry.is_unused() {
            return Err(AddressSpaceError::AlreadyMapped);
        }

        entry.set_addr(phys, flags);
        self.flush_if_active(virt);
        Ok(())
    }

    fn protect_user_page(
        &mut self,
        virt: VirtAddr,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(virt, 1)?;

        let root = unsafe { kernel_vm::phys_to_table_mut(self.root_phys()) };
        let pml4_entry = &mut root[p4_index(virt)];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }

        let pdpt = unsafe { kernel_vm::phys_to_table_mut(pml4_entry.addr()) };
        let pdpt_entry = &mut pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }

        let pd = unsafe { kernel_vm::phys_to_table_mut(pdpt_entry.addr()) };
        let pd_entry = &mut pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }

        let pt = unsafe { kernel_vm::phys_to_table_mut(pd_entry.addr()) };
        let pt_entry = &mut pt[p1_index(virt)];
        if pt_entry.is_unused() {
            return Err(AddressSpaceError::NotMapped);
        }

        // Bit 7 is PS in a directory entry, but PAT in a 4-KiB leaf PTE.
        // Preserve it across mprotect so a write-combine external mapping
        // cannot silently become write-back and create conflicting aliases.
        pt_entry.set_addr(
            pt_entry.addr(),
            preserve_4k_leaf_pat(pt_entry.flags(), flags),
        );
        self.flush_if_active(virt);
        Ok(())
    }

    fn unmap_user_page(&mut self, virt: VirtAddr) -> Option<PhysAddr> {
        if !is_user_addr(virt.as_u64()) {
            return None;
        }

        let root = unsafe { kernel_vm::phys_to_table_mut(self.root_phys()) };
        let pml4_entry = &mut root[p4_index(virt)];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pdpt = unsafe { kernel_vm::phys_to_table_mut(pml4_entry.addr()) };
        let pdpt_entry = &mut pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pd = unsafe { kernel_vm::phys_to_table_mut(pdpt_entry.addr()) };
        let pd_entry = &mut pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pt = unsafe { kernel_vm::phys_to_table_mut(pd_entry.addr()) };
        let pt_entry = &mut pt[p1_index(virt)];
        if pt_entry.is_unused() {
            return None;
        }

        let frame_phys = pt_entry.addr();
        pt_entry.set_unused();
        self.flush_if_active(virt);
        Some(frame_phys)
    }

    fn plan_region_subtraction(
        &self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<Vec<UserRegion>, AddressSpaceError> {
        let end = page_addr(start, page_count)?;
        let start_u64 = start.as_u64();
        let end_u64 = end.as_u64();
        let capacity = self
            .regions
            .len()
            .checked_add(1)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let mut updated = Vec::new();
        updated
            .try_reserve_exact(capacity)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let mut touched = false;

        for region in self.regions.iter().copied() {
            let region_start = region.start.as_u64();
            let region_end = region.end().as_u64();
            if end_u64 <= region_start || start_u64 >= region_end {
                updated.push(region);
                continue;
            }

            touched = true;

            if start_u64 > region_start {
                let left_pages = ((start_u64 - region_start) / PAGE_4KIB_U64) as usize;
                if left_pages != 0 {
                    updated.push(UserRegion {
                        start: region.start,
                        page_count: left_pages,
                    });
                }
            }

            if end_u64 < region_end {
                let right_pages = ((region_end - end_u64) / PAGE_4KIB_U64) as usize;
                if right_pages != 0 {
                    updated.push(UserRegion {
                        start: VirtAddr::new(end_u64),
                        page_count: right_pages,
                    });
                }
            }
        }

        if !touched {
            return Err(AddressSpaceError::NotMapped);
        }

        Ok(updated)
    }
}

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

        // Capture CAS-installed frame ownership before freeing the table
        // frames that contain their tagged PTEs.  A failure is an ownership
        // contract violation, not a reason to leak a user frame silently.
        let irq_off_pager_fault_frames = self
            .irq_off_pager_fault_leaves()
            .expect("IRQ-off pager-fault PTE ownership walk failed during address-space drop");

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

        // owned_frames is the allocation ledger. A page-table walk is not an
        // ownership oracle because shared memfd and device mappings install
        // borrowed leaf frames which their backing objects must release.
        free_owned_frames_logged(self.pml4_frame_phys, recorded_frames.into_iter());
        free_owned_frames_logged(
            self.pml4_frame_phys,
            irq_off_pager_fault_frames
                .iter()
                .map(|leaf| leaf.physical_address),
        );
    }
}

fn ensure_next_table(
    owned_frames: &mut Vec<u64>,
    published_tables: &mut Vec<PublishedUserTable>,
    parent_phys: PhysAddr,
    index: usize,
) -> Result<PhysAddr, AddressSpaceError> {
    let parent = unsafe { kernel_vm::phys_to_table_mut(parent_phys) };
    let entry = &mut parent[index];

    if entry.is_unused() {
        let Some(table_phys) = phys::alloc_frame() else {
            return Err(AddressSpaceError::OutOfFrames);
        };
        unsafe {
            kernel_vm::phys_to_table_mut(table_phys).zero();
        }
        if let Err(err) = track_owned_frame(owned_frames, table_phys.as_u64()) {
            let _ = phys::try_free_frame(table_phys);
            return Err(err);
        }
        entry.set_addr(table_phys, user_table_flags());
        published_tables.push(PublishedUserTable {
            parent_phys: parent_phys.as_u64(),
            parent_index: index,
            table_phys: table_phys.as_u64(),
        });
    } else {
        if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::HugePageConflict);
        }
        if !entry.flags().contains(user_table_flags()) {
            return Err(AddressSpaceError::ProtectionViolation);
        }
    }

    Ok(entry.addr())
}

fn validate_user_page_range(start: VirtAddr, page_count: usize) -> Result<(), AddressSpaceError> {
    if page_count == 0 {
        return Err(AddressSpaceError::ZeroSizedAllocation);
    }
    if !is_page_aligned(start.as_u64()) {
        return Err(AddressSpaceError::AddressNotPageAligned);
    }
    if !is_user_addr(start.as_u64()) {
        return Err(AddressSpaceError::AddressOutOfRange);
    }

    let span = (page_count as u64)
        .checked_mul(PAGE_4KIB_U64)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    let end = start
        .as_u64()
        .checked_add(span)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    if end > USER_SPACE_END_EXCLUSIVE {
        return Err(AddressSpaceError::AddressOutOfRange);
    }

    Ok(())
}

fn validate_protection_span<F>(page_count: usize, mut page_is_present: F) -> bool
where
    F: FnMut(usize) -> bool,
{
    (0..page_count).all(&mut page_is_present)
}

fn byte_len_to_page_count(byte_len: usize) -> Result<usize, AddressSpaceError> {
    if byte_len == 0 {
        return Err(AddressSpaceError::ZeroSizedAllocation);
    }

    let rounded = byte_len
        .checked_add(PAGE_4KIB - 1)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    Ok(rounded / PAGE_4KIB)
}

fn page_addr(start: VirtAddr, page_index: usize) -> Result<VirtAddr, AddressSpaceError> {
    let delta = (page_index as u64)
        .checked_mul(PAGE_4KIB_U64)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    let addr = start
        .as_u64()
        .checked_add(delta)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    Ok(VirtAddr::new(addr))
}

#[derive(Clone, Copy)]
enum UserBufferAccess {
    Read,
    Write,
}

fn validate_user_page_access(
    flags: PageTableFlags,
    access: UserBufferAccess,
) -> Result<(), AddressSpaceError> {
    let required = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if !flags.contains(required) {
        return Err(AddressSpaceError::ProtectionViolation);
    }

    if matches!(access, UserBufferAccess::Write) && !flags.contains(PageTableFlags::WRITABLE) {
        return Err(AddressSpaceError::ProtectionViolation);
    }

    Ok(())
}

fn align_down(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value & !(align - 1)
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|aligned| aligned & !(align - 1))
}

fn is_page_aligned(addr: u64) -> bool {
    addr & (PAGE_4KIB_U64 - 1) == 0
}

fn is_user_addr(addr: u64) -> bool {
    (USER_SPACE_BASE..USER_SPACE_END_EXCLUSIVE).contains(&addr)
}

fn p4_index(virt: VirtAddr) -> usize {
    ((virt.as_u64() >> 39) & 0x1ff) as usize
}

fn p3_index(virt: VirtAddr) -> usize {
    ((virt.as_u64() >> 30) & 0x1ff) as usize
}

fn p2_index(virt: VirtAddr) -> usize {
    ((virt.as_u64() >> 21) & 0x1ff) as usize
}

fn p1_index(virt: VirtAddr) -> usize {
    ((virt.as_u64() >> 12) & 0x1ff) as usize
}

fn user_table_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE
}

fn normalize_user_page_flags(flags: PageTableFlags) -> Result<PageTableFlags, AddressSpaceError> {
    if flags.contains(PageTableFlags::HUGE_PAGE) {
        return Err(AddressSpaceError::HugePageConflict);
    }
    if flags.contains(PageTableFlags::WRITABLE) && !flags.contains(PageTableFlags::NO_EXECUTE) {
        return Err(AddressSpaceError::ProtectionViolation);
    }

    Ok(flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE)
}

fn preserve_4k_leaf_pat(existing: PageTableFlags, requested: PageTableFlags) -> PageTableFlags {
    requested | (existing & PageTableFlags::HUGE_PAGE)
}

enum UserPageLookup {
    NotUser,
    MissingPml4,
    MissingPdpt,
    MissingPd,
    MissingPt,
    Present {
        phys: PhysAddr,
        flags: PageTableFlags,
    },
}

impl ProcessAddressSpace {
    fn lookup_user_page_state(&self, virt: VirtAddr) -> UserPageLookup {
        if !is_user_addr(virt.as_u64()) {
            return UserPageLookup::NotUser;
        }

        let p4 = p4_index(virt);
        let root = self.root_table_ref();
        let pml4_entry = &root[p4];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return UserPageLookup::MissingPml4;
        }

        let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4_entry.addr()) };
        let pdpt_entry = &pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return UserPageLookup::MissingPdpt;
        }

        let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
        let pd_entry = &pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return UserPageLookup::MissingPd;
        }

        let pt = unsafe { kernel_vm::phys_to_table_ref(pd_entry.addr()) };
        let pt_entry = &pt[p1_index(virt)];
        if pt_entry.is_unused() {
            return UserPageLookup::MissingPt;
        }

        let offset = virt.as_u64() & (PAGE_4KIB_U64 - 1);
        UserPageLookup::Present {
            phys: PhysAddr::new(pt_entry.addr().as_u64() + offset),
            flags: pt_entry.flags(),
        }
    }
}

fn higher_half_ptr(phys: PhysAddr) -> *mut u8 {
    kernel_vm::higher_half_addr(phys.as_u64()) as *mut u8
}

pub fn smoke_test() {
    use crate::debug;

    let mut space =
        ProcessAddressSpace::new().expect("process address space allocation must succeed");
    let region = space
        .alloc_user_bytes(8192, PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE)
        .expect("process user region allocation must succeed");
    let sample = b"proc-paging-ok";
    space
        .copy_into_user(region.start, sample)
        .expect("process user copy must succeed");

    let phys = space
        .translate_user(region.start)
        .expect("process user translation must succeed");
    let mut probe = [0_u8; 14];
    unsafe {
        ptr::copy_nonoverlapping(
            higher_half_ptr(phys) as *const u8,
            probe.as_mut_ptr(),
            probe.len(),
        );
    }

    if probe != *sample {
        panic!("process paging smoke test data mismatch");
    }

    debug::println!(
        "Process paging smoke test passed: user_va={:#x}, phys={:#x}",
        region.start.as_u64(),
        phys.as_u64()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_len_to_page_count_rounds_up() {
        assert_eq!(byte_len_to_page_count(1).unwrap(), 1);
        assert_eq!(byte_len_to_page_count(PAGE_4KIB).unwrap(), 1);
        assert_eq!(byte_len_to_page_count(PAGE_4KIB + 1).unwrap(), 2);
    }

    #[test]
    fn validate_user_page_range_rejects_unaligned_or_oob() {
        assert_eq!(
            validate_user_page_range(VirtAddr::new(USER_SPACE_BASE + 1), 1),
            Err(AddressSpaceError::AddressNotPageAligned)
        );
        assert_eq!(
            validate_user_page_range(VirtAddr::new(USER_SPACE_END_EXCLUSIVE), 1),
            Err(AddressSpaceError::AddressOutOfRange)
        );
        assert!(validate_user_page_range(VirtAddr::new(USER_SPACE_BASE), 1).is_ok());
    }

    #[test]
    fn user_page_flags_enforce_wx_and_reject_huge_pages() {
        assert_eq!(
            normalize_user_page_flags(PageTableFlags::WRITABLE),
            Err(AddressSpaceError::ProtectionViolation)
        );
        assert_eq!(
            normalize_user_page_flags(PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE),
            Ok(PageTableFlags::PRESENT
                | PageTableFlags::USER_ACCESSIBLE
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_EXECUTE)
        );
        assert_eq!(
            normalize_user_page_flags(PageTableFlags::HUGE_PAGE | PageTableFlags::NO_EXECUTE),
            Err(AddressSpaceError::HugePageConflict)
        );
    }

    #[test]
    fn mprotect_preserves_write_combine_pat_on_4k_leaf() {
        let existing = PageTableFlags::PRESENT
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::WRITABLE
            | PageTableFlags::NO_EXECUTE
            | PageTableFlags::HUGE_PAGE;
        let requested =
            PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE | PageTableFlags::NO_EXECUTE;
        let preserved = preserve_4k_leaf_pat(existing, requested);
        assert!(preserved.contains(PageTableFlags::HUGE_PAGE));
        assert!(!preserved.contains(PageTableFlags::WRITABLE));
    }

    #[test]
    fn protection_span_preflight_rejects_a_hole_before_commit() {
        let mut visited = 0;
        let accepted = validate_protection_span(4, |page_index| {
            assert_eq!(page_index, visited);
            visited += 1;
            page_index != 2
        });
        assert!(!accepted);
        assert_eq!(visited, 3);

        assert!(validate_protection_span(4, |_| true));
    }

    #[test]
    fn unmap_region_plan_is_complete_before_metadata_commit() {
        let mut space = ProcessAddressSpace::empty_for_tests();
        let start = VirtAddr::new(USER_SPACE_BASE);
        space.regions.push(UserRegion {
            start,
            page_count: 4,
        });
        let original = space.regions.clone();

        let plan = space
            .plan_region_subtraction(VirtAddr::new(USER_SPACE_BASE + PAGE_4KIB_U64), 2)
            .unwrap();
        assert_eq!(space.regions, original);
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0],
            UserRegion {
                start,
                page_count: 1,
            }
        );
        assert_eq!(
            plan[1],
            UserRegion {
                start: VirtAddr::new(USER_SPACE_BASE + 3 * PAGE_4KIB_U64),
                page_count: 1,
            }
        );
    }
}
