use alloc::vec::Vec;
use core::cmp::min;
use core::ptr;

use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::instructions::{interrupts, tlb};
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{PageTable, PageTableFlags};

use crate::memory::{kernel_vm, phys};

const ENTRIES_PER_TABLE: usize = 512;
const PAGE_4KIB: usize = 4096;
const PAGE_4KIB_U64: u64 = PAGE_4KIB as u64;
const USER_PML4_INDEX: usize = 1;
pub const USER_SPACE_BASE: u64 = (USER_PML4_INDEX as u64) << 39;
pub const USER_SPACE_END_EXCLUSIVE: u64 = ((USER_PML4_INDEX + 1) as u64) << 39;

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
                        higher_half_ptr(src_phys) as *const u8,
                        higher_half_ptr(dst_phys) as *mut u8,
                        PAGE_4KIB,
                    );
                }
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
        let mut mapped_pages = Vec::with_capacity(page_count);

        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            if self.translate_user(virt).is_some() {
                rollback_user_pages(self, &mapped_pages);
                return Err(AddressSpaceError::AlreadyMapped);
            }

            let Some(frame_phys) = phys::alloc_frame() else {
                rollback_user_pages(self, &mapped_pages);
                return Err(AddressSpaceError::OutOfFrames);
            };

            unsafe {
                ptr::write_bytes(higher_half_ptr(frame_phys), 0, PAGE_4KIB);
            }

            if let Err(err) = self.map_user_page(virt, frame_phys, page_flags) {
                phys::free_frame(frame_phys);
                rollback_user_pages(self, &mapped_pages);
                return Err(err);
            }

            mapped_pages.push((virt, frame_phys.as_u64()));
        }

        for &(_, frame_phys) in &mapped_pages {
            if let Err(err) = track_owned_frame(&mut self.owned_frames, frame_phys) {
                rollback_user_pages(self, &mapped_pages);
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
        if frames.is_empty() {
            return Err(AddressSpaceError::ZeroSizedAllocation);
        }

        validate_user_page_range(start, frames.len())?;
        let page_flags = normalize_user_page_flags(flags)?;
        let mut mapped_pages = Vec::with_capacity(frames.len());

        for (page_index, frame_phys) in frames.iter().copied().enumerate() {
            let virt = page_addr(start, page_index)?;
            if self.translate_user(virt).is_some() {
                rollback_external_user_pages(self, &mapped_pages);
                return Err(AddressSpaceError::AlreadyMapped);
            }
            if let Err(err) = self.map_user_page(virt, PhysAddr::new(frame_phys), page_flags) {
                rollback_external_user_pages(self, &mapped_pages);
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

        let mut frames = Vec::with_capacity(page_count);
        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            let phys = self
                .translate_user(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            if phys.as_u64() % PAGE_4KIB_U64 != 0 {
                return Err(AddressSpaceError::NotMapped);
            }
            frames.push(phys.as_u64());
        }

        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            let unmapped = self
                .unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            debug_assert_eq!(Some(unmapped.as_u64()), frames.get(page_index).copied());
        }

        for frame_phys in frames {
            remove_owned_frame(&mut self.owned_frames, frame_phys)?;
            phys::free_frame(PhysAddr::new(frame_phys));
        }

        self.subtract_region_range(start, page_count)?;
        Ok(page_count)
    }

    pub fn unmap_user_pages_without_free_at(
        &mut self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<usize, AddressSpaceError> {
        validate_user_page_range(start, page_count)?;

        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            self.unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
        }

        self.subtract_region_range(start, page_count)?;
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
        if pt_entry.is_unused() || pt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let offset = virt.as_u64() & (PAGE_4KIB_U64 - 1);
        Some((
            PhysAddr::new(pt_entry.addr().as_u64() + offset),
            pt_entry.flags(),
        ))
    }

    pub fn copy_into_user(&self, start: VirtAddr, data: &[u8]) -> Result<(), AddressSpaceError> {
        self.validate_user_buffer_access(start, data.len(), UserBufferAccess::Write)?;
        self.write_user_bytes(start, data)
    }

    pub fn validate_user_write_buffer(
        &self,
        start: VirtAddr,
        byte_len: usize,
    ) -> Result<(), AddressSpaceError> {
        self.validate_user_buffer_access(start, byte_len, UserBufferAccess::Write)
    }

    pub fn validate_user_read_buffer(
        &self,
        start: VirtAddr,
        byte_len: usize,
    ) -> Result<(), AddressSpaceError> {
        self.validate_user_buffer_access(start, byte_len, UserBufferAccess::Read)
    }

    pub fn initialize_user_bytes(
        &self,
        start: VirtAddr,
        data: &[u8],
    ) -> Result<(), AddressSpaceError> {
        self.write_user_bytes(start, data)
    }

    fn write_user_bytes(&self, start: VirtAddr, data: &[u8]) -> Result<(), AddressSpaceError> {
        if data.is_empty() {
            return Ok(());
        }

        let mut cursor = start.as_u64();
        let mut written = 0usize;

        while written < data.len() {
            let virt = VirtAddr::new(cursor);
            let phys = self
                .translate_user(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            let page_offset = (cursor as usize) & (PAGE_4KIB - 1);
            let chunk = min(PAGE_4KIB - page_offset, data.len() - written);

            unsafe {
                ptr::copy_nonoverlapping(
                    data.as_ptr().add(written),
                    higher_half_ptr(phys) as *mut u8,
                    chunk,
                );
            }

            cursor = cursor
                .checked_add(chunk as u64)
                .ok_or(AddressSpaceError::AddressOverflow)?;
            written += chunk;
        }

        Ok(())
    }

    pub fn copy_from_user(
        &self,
        start: VirtAddr,
        dest: &mut [u8],
    ) -> Result<(), AddressSpaceError> {
        self.validate_user_buffer_access(start, dest.len(), UserBufferAccess::Read)?;
        self.read_user_bytes(start, dest)
    }

    pub fn visit_user_read_spans(
        &self,
        start: VirtAddr,
        byte_len: usize,
        mut visit: impl FnMut(*const u8, usize) -> Result<(), AddressSpaceError>,
    ) -> Result<(), AddressSpaceError> {
        if byte_len == 0 {
            return Ok(());
        }

        let start_addr = start.as_u64();
        if !is_user_addr(start_addr) {
            return Err(AddressSpaceError::AddressOutOfRange);
        }

        let last_addr = start_addr
            .checked_add(byte_len as u64 - 1)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        if !is_user_addr(last_addr) {
            return Err(AddressSpaceError::AddressOutOfRange);
        }

        let end_exclusive = last_addr
            .checked_add(1)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let mut cursor = start_addr;

        while cursor < end_exclusive {
            let virt = VirtAddr::new(cursor);
            let (phys, flags) = self
                .translate_user_with_flags(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            validate_user_page_access(flags, UserBufferAccess::Read)?;

            let page_offset = (cursor as usize) & (PAGE_4KIB - 1);
            let chunk = min(PAGE_4KIB - page_offset, (end_exclusive - cursor) as usize);
            visit(higher_half_ptr(phys) as *const u8, chunk)?;

            cursor = cursor
                .checked_add(chunk as u64)
                .ok_or(AddressSpaceError::AddressOverflow)?;
        }

        Ok(())
    }

    fn read_user_bytes(&self, start: VirtAddr, dest: &mut [u8]) -> Result<(), AddressSpaceError> {
        if dest.is_empty() {
            return Ok(());
        }

        let mut cursor = start.as_u64();
        let mut copied = 0usize;

        while copied < dest.len() {
            let virt = VirtAddr::new(cursor);
            let phys = self
                .translate_user(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            let page_offset = (cursor as usize) & (PAGE_4KIB - 1);
            let chunk = min(PAGE_4KIB - page_offset, dest.len() - copied);

            unsafe {
                ptr::copy_nonoverlapping(
                    higher_half_ptr(phys) as *const u8,
                    dest.as_mut_ptr().add(copied),
                    chunk,
                );
            }

            cursor = cursor
                .checked_add(chunk as u64)
                .ok_or(AddressSpaceError::AddressOverflow)?;
            copied += chunk;
        }

        Ok(())
    }

    fn validate_user_buffer_access(
        &self,
        start: VirtAddr,
        byte_len: usize,
        access: UserBufferAccess,
    ) -> Result<(), AddressSpaceError> {
        if byte_len == 0 {
            return Ok(());
        }

        let start_addr = start.as_u64();
        if !is_user_addr(start_addr) {
            return Err(AddressSpaceError::AddressOutOfRange);
        }

        let last_addr = start_addr
            .checked_add(byte_len as u64 - 1)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        if !is_user_addr(last_addr) {
            return Err(AddressSpaceError::AddressOutOfRange);
        }

        let mut cursor = align_down(start_addr, PAGE_4KIB_U64);
        let end_exclusive = align_up(
            last_addr
                .checked_add(1)
                .ok_or(AddressSpaceError::AddressOverflow)?,
            PAGE_4KIB_U64,
        )
        .ok_or(AddressSpaceError::AddressOverflow)?;

        while cursor < end_exclusive {
            let (_, flags) = self
                .translate_user_with_flags(VirtAddr::new(cursor))
                .ok_or(AddressSpaceError::NotMapped)?;
            validate_user_page_access(flags, access)?;
            cursor = cursor
                .checked_add(PAGE_4KIB_U64)
                .ok_or(AddressSpaceError::AddressOverflow)?;
        }

        Ok(())
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
    ) -> Result<(), AddressSpaceError> {
        validate_user_page_range(virt, 1)?;

        let pml4_phys = self.root_phys();
        let owned_frames = &mut self.owned_frames;
        let root = unsafe { kernel_vm::phys_to_table_mut(pml4_phys) };
        let pdpt = ensure_next_table(owned_frames, root, p4_index(virt))?;
        let pd = ensure_next_table(owned_frames, pdpt, p3_index(virt))?;
        let pt = ensure_next_table(owned_frames, pd, p2_index(virt))?;

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
        if pt_entry.is_unused() || pt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }

        pt_entry.set_addr(pt_entry.addr(), flags);
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
        if pt_entry.is_unused() || pt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let frame_phys = pt_entry.addr();
        pt_entry.set_unused();
        self.flush_if_active(virt);
        Some(frame_phys)
    }

    fn subtract_region_range(
        &mut self,
        start: VirtAddr,
        page_count: usize,
    ) -> Result<(), AddressSpaceError> {
        let end = page_addr(start, page_count)?;
        let start_u64 = start.as_u64();
        let end_u64 = end.as_u64();
        let mut updated = Vec::with_capacity(self.regions.len() + 1);
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

        self.regions = updated;
        Ok(())
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
            for frame_phys in recorded_frames {
                if frame_phys == 0 || frame_phys % PAGE_4KIB_U64 != 0 {
                    continue;
                }
                let _ = phys::try_free_frame(PhysAddr::new(frame_phys));
            }
            return;
        }

        let (current_frame, _) = Cr3::read();
        if current_frame.start_address() == self.root_phys() {
            panic!("cannot drop the active process address space");
        }

        let mut authoritative_frames = self.collect_authoritative_owned_frames();
        if authoritative_frames.is_empty() {
            authoritative_frames.push(self.pml4_frame_phys);
        }

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

        for frame_phys in authoritative_frames {
            if frame_phys == 0 || frame_phys % PAGE_4KIB_U64 != 0 {
                crate::debug::println!(
                    "process address space: skipping invalid owned frame root={:#x} frame={:#x}",
                    self.pml4_frame_phys,
                    frame_phys,
                );
                continue;
            }

            if let Err(err) = phys::try_free_frame(PhysAddr::new(frame_phys)) {
                crate::debug::println!(
                    "process address space: frame cleanup rejected root={:#x} frame={:#x} err={:?}",
                    self.pml4_frame_phys,
                    frame_phys,
                    err,
                );
            }
        }
    }
}

fn rollback_user_pages(space: &mut ProcessAddressSpace, pages: &[(VirtAddr, u64)]) {
    for &(virt, frame_phys) in pages.iter().rev() {
        let unmapped = space.unmap_user_page(virt);
        if unmapped.map(|phys| phys.as_u64()) != Some(frame_phys) {
            panic!("user page rollback mismatch");
        }
        phys::free_frame(PhysAddr::new(frame_phys));
    }
}

fn rollback_external_user_pages(space: &mut ProcessAddressSpace, pages: &[VirtAddr]) {
    for &virt in pages.iter().rev() {
        let _ = space.unmap_user_page(virt);
    }
}

fn remove_owned_frame(
    owned_frames: &mut Vec<u64>,
    frame_phys: u64,
) -> Result<(), AddressSpaceError> {
    let Some(position) = owned_frames.iter().position(|owned| *owned == frame_phys) else {
        return Err(AddressSpaceError::NotMapped);
    };
    owned_frames.swap_remove(position);
    Ok(())
}

fn track_owned_frame(
    owned_frames: &mut Vec<u64>,
    frame_phys: u64,
) -> Result<(), AddressSpaceError> {
    if frame_phys == 0 || frame_phys % PAGE_4KIB_U64 != 0 {
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }
    if owned_frames.iter().any(|owned| *owned == frame_phys) {
        return Err(AddressSpaceError::InvalidFrameOwnership);
    }
    owned_frames.push(frame_phys);
    Ok(())
}

fn ensure_next_table<'a>(
    owned_frames: &mut Vec<u64>,
    parent: &'a mut PageTable,
    index: usize,
) -> Result<&'a mut PageTable, AddressSpaceError> {
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
    } else {
        if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::HugePageConflict);
        }

        let merged_flags = entry.flags() | user_table_flags();
        entry.set_addr(entry.addr(), merged_flags);
    }

    Ok(unsafe { kernel_vm::phys_to_table_mut(entry.addr()) })
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

    Ok(flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE)
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
    fn collect_authoritative_owned_frames(&self) -> Vec<u64> {
        let mut frames = Vec::new();
        frames.push(self.pml4_frame_phys);

        let root = self.root_table_ref();
        let pml4_entry = &root[USER_PML4_INDEX];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return frames;
        }

        let pdpt_phys = pml4_entry.addr().as_u64();
        if is_page_aligned(pdpt_phys) && pdpt_phys != 0 {
            frames.push(pdpt_phys);
        }
        let pdpt = unsafe { kernel_vm::phys_to_table_ref(pml4_entry.addr()) };
        for pdpt_entry in pdpt.iter() {
            if pdpt_entry.is_unused() {
                continue;
            }
            if pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                let phys = pdpt_entry.addr().as_u64();
                if is_page_aligned(phys) && phys != 0 {
                    frames.push(phys);
                }
                continue;
            }

            let pd_phys = pdpt_entry.addr().as_u64();
            if is_page_aligned(pd_phys) && pd_phys != 0 {
                frames.push(pd_phys);
            }
            let pd = unsafe { kernel_vm::phys_to_table_ref(pdpt_entry.addr()) };
            for pd_entry in pd.iter() {
                if pd_entry.is_unused() {
                    continue;
                }
                if pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                    let phys = pd_entry.addr().as_u64();
                    if is_page_aligned(phys) && phys != 0 {
                        frames.push(phys);
                    }
                    continue;
                }

                let pt_phys = pd_entry.addr().as_u64();
                if is_page_aligned(pt_phys) && pt_phys != 0 {
                    frames.push(pt_phys);
                }
                let pt = unsafe { kernel_vm::phys_to_table_ref(pd_entry.addr()) };
                for pt_entry in pt.iter() {
                    if pt_entry.is_unused() {
                        continue;
                    }
                    let phys = pt_entry.addr().as_u64();
                    if is_page_aligned(phys) && phys != 0 {
                        frames.push(phys);
                    }
                }
            }
        }

        frames.sort_unstable();
        frames.dedup();
        frames
    }

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
        if pt_entry.is_unused() || pt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
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
        .alloc_user_bytes(8192, PageTableFlags::WRITABLE)
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
}
