use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::cmp::min;
use core::ops::Range;
use core::ptr::{self, addr_of, addr_of_mut};

use spin::Mutex;
use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::instructions::{interrupts, tlb};
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::registers::model_specific::Msr;
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::structures::paging::{PageTable, PageTableFlags, PhysFrame};

const ENTRIES_PER_TABLE: usize = 512;
const PAGE_4KIB: usize = 4096;
const PAGE_4KIB_U64: u64 = PAGE_4KIB as u64;
const HUGE_2MIB: u64 = 2 * 1024 * 1024;
const KERNEL_PML4_SIZE_GB: usize = 512;
const ADDRESS_SPACE_LIMIT: u64 = 512 * 1024 * 1024 * 1024;
const MAX_PAGE_BLOCK: u64 = ADDRESS_SPACE_LIMIT / HUGE_2MIB;
const KERNEL_HIGHER_HALF_PML4_INDEX: usize = 256;
const MMIO_WINDOW_PML4_INDEX: usize = KERNEL_HIGHER_HALF_PML4_INDEX + 1;
const MMIO_WINDOW_SLOTS: usize = 16;
const USER_PML4_INDEX: usize = 1;
pub const KERNEL_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
const MMIO_WINDOW_BASE: u64 = KERNEL_VIRT_OFFSET + (1_u64 << 39);
pub const USER_SPACE_BASE: u64 = (USER_PML4_INDEX as u64) << 39;
pub const USER_SPACE_END_EXCLUSIVE: u64 = ((USER_PML4_INDEX + 1) as u64) << 39;
// User-space graphics surfaces, glibc, and predecoded media assets now live in
// normal process address spaces, so the fixed process frame pool must hold more
// than a single full-screen surface. 65_536 pages = 256 MiB, which leaves room
// for a predecoded 800x600x48-frame GIF cache plus loader/runtime overhead.
const PROCESS_FRAME_POOL_PAGES: usize = 65_536;
const PROCESS_FRAME_BITMAP_WORDS: usize = (PROCESS_FRAME_POOL_PAGES + 63) / 64;
const MMIO_UNMAPPED_BLOCK: u64 = u64::MAX;

// 2 MiB huge-page PDE uses bit 12 as the PAT selector bit.
pub const WRITE_COMBINE_BIT: PageTableFlags = PageTableFlags::from_bits_retain(1 << 12);
const MMIO_UNCACHED_FLAGS: PageTableFlags = PageTableFlags::NO_CACHE;
const MMIO_WRITE_COMBINE_FLAGS: PageTableFlags = WRITE_COMBINE_BIT;

#[derive(Clone, Copy)]
struct MmioWindowSlot {
    phys_block: u64,
    flags_bits: u64,
}

impl MmioWindowSlot {
    const fn unmapped() -> Self {
        Self {
            phys_block: MMIO_UNMAPPED_BLOCK,
            flags_bits: 0,
        }
    }

    fn matches(self, phys_block: u64, flags: PageTableFlags) -> bool {
        self.phys_block == phys_block && self.flags_bits == flags.bits()
    }

    fn is_unmapped(self) -> bool {
        self.phys_block == MMIO_UNMAPPED_BLOCK
    }
}

pub static KERNEL_PML4: Mutex<PML4<KERNEL_PML4_SIZE_GB>> = Mutex::new(PML4 {
    pml4: PageTable::new(),
    pdp: PageTable::new(),
    pd: [const { PageTable::new() }; KERNEL_PML4_SIZE_GB],
    mmio_pdp: PageTable::new(),
    mmio_pd: PageTable::new(),
    mmio_blocks: [MmioWindowSlot::unmapped(); MMIO_WINDOW_SLOTS],
});

#[repr(align(4096))]
struct ProcessFrame {
    _bytes: [u8; PAGE_4KIB],
}

struct ProcessFrameMemory(UnsafeCell<[ProcessFrame; PROCESS_FRAME_POOL_PAGES]>);

unsafe impl Sync for ProcessFrameMemory {}

static PROCESS_FRAME_MEMORY: ProcessFrameMemory = ProcessFrameMemory(UnsafeCell::new(
    [const {
        ProcessFrame {
            _bytes: [0; PAGE_4KIB],
        }
    }; PROCESS_FRAME_POOL_PAGES],
));

static PROCESS_FRAME_POOL: Mutex<ProcessFramePool> = Mutex::new(ProcessFramePool::new());

struct ProcessFramePool {
    used: [u64; PROCESS_FRAME_BITMAP_WORDS],
}

impl ProcessFramePool {
    const fn new() -> Self {
        Self {
            used: [0; PROCESS_FRAME_BITMAP_WORDS],
        }
    }

    fn alloc(&mut self) -> Option<usize> {
        for (word_index, word) in self.used.iter_mut().enumerate() {
            if *word == u64::MAX {
                continue;
            }

            let bit_index = (!*word).trailing_zeros() as usize;
            let frame_index = word_index * 64 + bit_index;
            if frame_index >= PROCESS_FRAME_POOL_PAGES {
                return None;
            }

            *word |= 1_u64 << bit_index;
            unsafe {
                ptr::write_bytes(process_frame_ptr(frame_index), 0, PAGE_4KIB);
            }
            return Some(frame_index);
        }

        None
    }

    fn free(&mut self, frame_index: usize) {
        if frame_index >= PROCESS_FRAME_POOL_PAGES {
            panic!("process frame index out of range");
        }

        let word_index = frame_index / 64;
        let bit_index = frame_index % 64;
        let mask = 1_u64 << bit_index;

        if self.used[word_index] & mask == 0 {
            panic!("process frame was already free");
        }

        self.used[word_index] &= !mask;
    }
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
    pml4_frame: usize,
    next_user_addr: u64,
    owned_frames: Vec<usize>,
    regions: Vec<UserRegion>,
}

unsafe fn set_pat_wc_slot4() {
    const IA32_PAT: u32 = 0x277;
    const PAT_WC: u64 = 0x01;

    let mut msr = Msr::new(IA32_PAT);
    let mut pat = unsafe { msr.read() };
    pat &= !(0xff_u64 << 32); // slot4 clear
    pat |= PAT_WC << 32; // slot4 = WC
    unsafe { msr.write(pat) };
}

#[repr(C)]
pub struct PML4<const SIZE_GB: usize> {
    pml4: PageTable,
    pdp: PageTable,
    pd: [PageTable; SIZE_GB],
    mmio_pdp: PageTable,
    mmio_pd: PageTable,
    mmio_blocks: [MmioWindowSlot; MMIO_WINDOW_SLOTS],
}

impl<const SIZE_GB: usize> PML4<SIZE_GB> {
    pub fn init(&mut self) {
        self.pml4 = PageTable::new();
        self.pdp = PageTable::new();
        self.pd = [const { PageTable::new() }; SIZE_GB];
        self.mmio_pdp = PageTable::new();
        self.mmio_pd = PageTable::new();
        self.mmio_blocks = [MmioWindowSlot::unmapped(); MMIO_WINDOW_SLOTS];

        self.pml4.zero();
        self.pdp.zero();
        self.mmio_pdp.zero();
        self.mmio_pd.zero();

        let table_flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        let huge_flags = table_flags | PageTableFlags::HUGE_PAGE;

        let pdp_phys = PhysAddr::new(addr_of_mut!(self.pdp) as u64);
        self.pml4[0].set_addr(pdp_phys, table_flags);
        self.pml4[KERNEL_HIGHER_HALF_PML4_INDEX].set_addr(pdp_phys, table_flags);
        let mmio_pdp_phys = PhysAddr::new(addr_of_mut!(self.mmio_pdp) as u64);
        let mmio_pd_phys = PhysAddr::new(addr_of_mut!(self.mmio_pd) as u64);
        self.pml4[MMIO_WINDOW_PML4_INDEX].set_addr(mmio_pdp_phys, table_flags);
        self.mmio_pdp[0].set_addr(mmio_pd_phys, table_flags);

        for pdp_index in 0..SIZE_GB {
            self.pd[pdp_index].zero();

            let pd_phys = PhysAddr::new(addr_of_mut!(self.pd[pdp_index]) as u64);
            self.pdp[pdp_index].set_addr(pd_phys, table_flags);

            let gib_base = (pdp_index as u64) << 30;
            for pd_index in 0..ENTRIES_PER_TABLE {
                let phys = PhysAddr::new(gib_base + (pd_index as u64) * HUGE_2MIB);
                self.pd[pdp_index][pd_index].set_addr(phys, huge_flags);
            }
        }
    }

    fn find_free_mmio_slots(&self, block_count: usize) -> Option<usize> {
        if block_count == 0 || block_count > MMIO_WINDOW_SLOTS {
            return None;
        }

        let mut run_start = 0usize;
        let mut run_len = 0usize;
        for (slot, mapping) in self.mmio_blocks.iter().enumerate() {
            if mapping.is_unmapped() {
                if run_len == 0 {
                    run_start = slot;
                }
                run_len += 1;
                if run_len == block_count {
                    return Some(run_start);
                }
            } else {
                run_len = 0;
            }
        }
        None
    }

    fn map_mmio_blocks(
        &mut self,
        phys_block_start: u64,
        block_count: usize,
        mmio_flags: PageTableFlags,
    ) -> Option<u64> {
        if block_count == 0 {
            return None;
        }

        let table_flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        let huge_flags = table_flags | PageTableFlags::HUGE_PAGE | mmio_flags;

        let slot_start = self.find_free_mmio_slots(block_count)?;
        for block_offset in 0..block_count {
            let slot = slot_start + block_offset;
            let phys_block = phys_block_start + block_offset as u64;
            self.mmio_blocks[slot] = MmioWindowSlot {
                phys_block,
                flags_bits: mmio_flags.bits(),
            };
            self.mmio_pd[slot].set_addr(PhysAddr::new(phys_block * HUGE_2MIB), huge_flags);
            tlb::flush(VirtAddr::new(mmio_slot_base(slot)));
        }
        Some(mmio_slot_base(slot_start))
    }

    fn unmap_mmio_blocks(&mut self, virt_base: u64, block_count: usize) -> bool {
        if block_count == 0 || virt_base < MMIO_WINDOW_BASE || virt_base % HUGE_2MIB != 0 {
            return false;
        }

        let slot_start = ((virt_base - MMIO_WINDOW_BASE) / HUGE_2MIB) as usize;
        if slot_start.checked_add(block_count).is_none_or(|end| end > MMIO_WINDOW_SLOTS) {
            return false;
        }

        for slot in slot_start..slot_start + block_count {
            if self.mmio_blocks[slot].is_unmapped() {
                return false;
            }
        }

        for slot in slot_start..slot_start + block_count {
            self.mmio_blocks[slot] = MmioWindowSlot::unmapped();
            self.mmio_pd[slot].set_unused();
            tlb::flush(VirtAddr::new(mmio_slot_base(slot)));
        }
        true
    }

    fn block_check(&self, page_block: u64) {
        if page_block >= (ENTRIES_PER_TABLE * SIZE_GB) as u64 {
            panic!("Paging map error : block index should be less than block count.");
        }
    }

    fn pd_indices(&self, page_block: u64) -> (usize, usize) {
        self.block_check(page_block);
        (
            page_block as usize / ENTRIES_PER_TABLE,
            page_block as usize % ENTRIES_PER_TABLE,
        )
    }

    fn pd_entry_mut(&mut self, page_block: u64) -> &mut PageTableEntry {
        let (pdp_idx, pd_idx) = self.pd_indices(page_block);
        &mut self.pd[pdp_idx][pd_idx]
    }

    fn flush_block(virt_block: u64) {
        tlb::flush(VirtAddr::new(virt_block * HUGE_2MIB));
    }

    pub fn map(&mut self, virt_block: u64, phys_block: u64, flags: PageTableFlags) {
        self.block_check(virt_block);
        self.block_check(phys_block);

        let flags = flags | PageTableFlags::HUGE_PAGE;

        self.pd_entry_mut(virt_block)
            .set_addr(PhysAddr::new(phys_block * HUGE_2MIB), flags);
        Self::flush_block(virt_block);
    }

    pub fn add_flags(&mut self, virt_block: u64, flags: PageTableFlags) {
        let entry = self.pd_entry_mut(virt_block);
        let phys_block = entry.addr().as_u64() / HUGE_2MIB;
        let merged_flags = entry.flags() | flags;
        self.map(virt_block, phys_block, merged_flags);
    }

    pub unsafe fn load(&self) {
        let pml4_phys = PhysAddr::new(addr_of!(self.pml4) as u64);
        let pml4_frame = PhysFrame::containing_address(pml4_phys);

        unsafe {
            Cr3::write(pml4_frame, Cr3Flags::empty());
        }
    }
}

impl ProcessAddressSpace {
    pub fn new() -> Result<Self, AddressSpaceError> {
        let pml4_frame = alloc_process_frame()?;
        let root = unsafe { process_frame_table_mut(pml4_frame) };
        root.zero();

        interrupts::without_interrupts(|| {
            let kernel_pml4 = KERNEL_PML4.lock();
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
            pml4_frame,
            next_user_addr: USER_SPACE_BASE,
            owned_frames: vec![pml4_frame],
            regions: Vec::new(),
        })
    }

    pub fn root_phys(&self) -> PhysAddr {
        process_frame_phys(self.pml4_frame)
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

            let frame_index = match alloc_process_frame() {
                Ok(frame_index) => frame_index,
                Err(err) => {
                    rollback_user_pages(self, &mapped_pages);
                    return Err(err);
                }
            };

            if let Err(err) = self.map_user_page(virt, process_frame_phys(frame_index), page_flags)
            {
                free_process_frame(frame_index);
                rollback_user_pages(self, &mapped_pages);
                return Err(err);
            }

            mapped_pages.push((virt, frame_index));
        }

        self.owned_frames
            .extend(mapped_pages.iter().map(|(_, frame_index)| *frame_index));

        let region = UserRegion { start, page_count };
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
            let frame_index =
                process_frame_index_from_phys(phys).ok_or(AddressSpaceError::NotMapped)?;
            frames.push(frame_index);
        }

        for page_index in 0..page_count {
            let virt = page_addr(start, page_index)?;
            let unmapped = self
                .unmap_user_page(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            debug_assert_eq!(Some(unmapped), frames.get(page_index).copied());
        }

        for frame_index in frames {
            remove_owned_frame(&mut self.owned_frames, frame_index)?;
            free_process_frame(frame_index);
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

        let pdpt = unsafe { phys_to_table_ref(pml4_entry.addr()) };
        let pdpt_entry = &pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pd = unsafe { phys_to_table_ref(pdpt_entry.addr()) };
        let pd_entry = &pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pt = unsafe { phys_to_table_ref(pd_entry.addr()) };
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
                    phys.as_u64() as *mut u8,
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
            visit(phys.as_u64() as *const u8, chunk)?;

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
                    phys.as_u64() as *const u8,
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

    pub fn zero_user_bytes(
        &self,
        start: VirtAddr,
        byte_len: usize,
    ) -> Result<(), AddressSpaceError> {
        self.validate_user_buffer_access(start, byte_len, UserBufferAccess::Write)?;
        self.zero_user_bytes_unchecked(start, byte_len)
    }

    pub fn zero_user_bytes_unchecked(
        &self,
        start: VirtAddr,
        byte_len: usize,
    ) -> Result<(), AddressSpaceError> {
        if byte_len == 0 {
            return Ok(());
        }

        let mut cursor = start.as_u64();
        let mut zeroed = 0usize;

        while zeroed < byte_len {
            let virt = VirtAddr::new(cursor);
            let phys = self
                .translate_user(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            let page_offset = (cursor as usize) & (PAGE_4KIB - 1);
            let chunk = min(PAGE_4KIB - page_offset, byte_len - zeroed);

            unsafe {
                ptr::write_bytes(phys.as_u64() as *mut u8, 0, chunk);
            }

            cursor = cursor
                .checked_add(chunk as u64)
                .ok_or(AddressSpaceError::AddressOverflow)?;
            zeroed += chunk;
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

    pub unsafe fn load(&self) {
        let frame = PhysFrame::containing_address(self.root_phys());
        unsafe {
            Cr3::write(frame, Cr3Flags::empty());
        }
    }

    fn root_table_ref(&self) -> &'static PageTable {
        unsafe { process_frame_table_ref(self.pml4_frame) }
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

        let pml4_frame = self.pml4_frame;
        let owned_frames = &mut self.owned_frames;
        let root = unsafe { process_frame_table_mut(pml4_frame) };
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

        let root = unsafe { process_frame_table_mut(self.pml4_frame) };
        let pml4_entry = &mut root[p4_index(virt)];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }

        let pdpt = unsafe { phys_to_table_mut(pml4_entry.addr()) };
        let pdpt_entry = &mut pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }

        let pd = unsafe { phys_to_table_mut(pdpt_entry.addr()) };
        let pd_entry = &mut pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }

        let pt = unsafe { phys_to_table_mut(pd_entry.addr()) };
        let pt_entry = &mut pt[p1_index(virt)];
        if pt_entry.is_unused() || pt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::NotMapped);
        }

        pt_entry.set_addr(pt_entry.addr(), flags);
        self.flush_if_active(virt);
        Ok(())
    }

    fn unmap_user_page(&mut self, virt: VirtAddr) -> Option<usize> {
        if !is_user_addr(virt.as_u64()) {
            return None;
        }

        let root = unsafe { process_frame_table_mut(self.pml4_frame) };
        let pml4_entry = &mut root[p4_index(virt)];
        if pml4_entry.is_unused() || pml4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pdpt = unsafe { phys_to_table_mut(pml4_entry.addr()) };
        let pdpt_entry = &mut pdpt[p3_index(virt)];
        if pdpt_entry.is_unused() || pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pd = unsafe { phys_to_table_mut(pdpt_entry.addr()) };
        let pd_entry = &mut pd[p2_index(virt)];
        if pd_entry.is_unused() || pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let pt = unsafe { phys_to_table_mut(pd_entry.addr()) };
        let pt_entry = &mut pt[p1_index(virt)];
        if pt_entry.is_unused() || pt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return None;
        }

        let frame_index = process_frame_index_from_phys(pt_entry.addr());
        pt_entry.set_unused();
        self.flush_if_active(virt);
        frame_index
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
    fn drop(&mut self) {
        let (current_frame, _) = Cr3::read();
        if current_frame.start_address() == self.root_phys() {
            panic!("cannot drop the active process address space");
        }

        for &frame_index in &self.owned_frames {
            free_process_frame(frame_index);
        }
    }
}

fn rollback_user_pages(space: &mut ProcessAddressSpace, pages: &[(VirtAddr, usize)]) {
    for &(virt, frame_index) in pages.iter().rev() {
        let unmapped = space.unmap_user_page(virt);
        if unmapped != Some(frame_index) {
            panic!("user page rollback mismatch");
        }
        free_process_frame(frame_index);
    }
}

fn remove_owned_frame(
    owned_frames: &mut Vec<usize>,
    frame_index: usize,
) -> Result<(), AddressSpaceError> {
    let Some(position) = owned_frames.iter().position(|owned| *owned == frame_index) else {
        return Err(AddressSpaceError::NotMapped);
    };
    owned_frames.swap_remove(position);
    Ok(())
}

fn alloc_process_frame() -> Result<usize, AddressSpaceError> {
    interrupts::without_interrupts(|| {
        PROCESS_FRAME_POOL
            .lock()
            .alloc()
            .ok_or(AddressSpaceError::OutOfFrames)
    })
}

fn free_process_frame(frame_index: usize) {
    interrupts::without_interrupts(|| {
        PROCESS_FRAME_POOL.lock().free(frame_index);
    });
}

fn ensure_next_table<'a>(
    owned_frames: &mut Vec<usize>,
    parent: &'a mut PageTable,
    index: usize,
) -> Result<&'a mut PageTable, AddressSpaceError> {
    let entry = &mut parent[index];

    if entry.is_unused() {
        let table_frame = alloc_process_frame()?;
        unsafe {
            process_frame_table_mut(table_frame).zero();
        }
        entry.set_addr(process_frame_phys(table_frame), user_table_flags());
        owned_frames.push(table_frame);
    } else {
        if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(AddressSpaceError::HugePageConflict);
        }

        let merged_flags = entry.flags() | user_table_flags();
        entry.set_addr(entry.addr(), merged_flags);
    }

    Ok(unsafe { phys_to_table_mut(entry.addr()) })
}

fn process_frame_ptr(frame_index: usize) -> *mut u8 {
    if frame_index >= PROCESS_FRAME_POOL_PAGES {
        panic!("process frame index out of range");
    }

    let base = PROCESS_FRAME_MEMORY.0.get() as *mut ProcessFrame;
    unsafe { base.add(frame_index) as *mut u8 }
}

fn process_frame_range() -> Range<usize> {
    let start = process_frame_phys(0).as_u64() as usize;
    let end = start + PROCESS_FRAME_POOL_PAGES * PAGE_4KIB;
    start..end
}

fn process_frame_phys(frame_index: usize) -> PhysAddr {
    PhysAddr::new(kernel_virtual_to_physical(
        process_frame_ptr(frame_index) as u64
    ))
}

fn process_frame_index_from_phys(phys: PhysAddr) -> Option<usize> {
    let addr = phys.as_u64() as usize;
    let range = process_frame_range();
    if !range.contains(&addr) {
        return None;
    }

    let offset = addr - range.start;
    if offset % PAGE_4KIB != 0 {
        return None;
    }

    Some(offset / PAGE_4KIB)
}

unsafe fn process_frame_table_ref(frame_index: usize) -> &'static PageTable {
    unsafe { &*(process_frame_ptr(frame_index) as *const PageTable) }
}

unsafe fn process_frame_table_mut(frame_index: usize) -> &'static mut PageTable {
    unsafe { &mut *(process_frame_ptr(frame_index) as *mut PageTable) }
}

unsafe fn phys_to_table_ref(phys: PhysAddr) -> &'static PageTable {
    unsafe { &*(phys.as_u64() as *const PageTable) }
}

unsafe fn phys_to_table_mut(phys: PhysAddr) -> &'static mut PageTable {
    unsafe { &mut *(phys.as_u64() as *mut PageTable) }
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

fn kernel_virtual_to_physical(addr: u64) -> u64 {
    if addr >= KERNEL_VIRT_OFFSET {
        addr - KERNEL_VIRT_OFFSET
    } else {
        addr
    }
}

fn mmio_slot_base(slot: usize) -> u64 {
    MMIO_WINDOW_BASE + slot as u64 * HUGE_2MIB
}

pub fn init() {
    unsafe {
        set_pat_wc_slot4();

        interrupts::without_interrupts(|| {
            let mut pml4 = KERNEL_PML4.lock();
            pml4.init();
            pml4.load();
        });
    }
}

pub fn kernel_root_phys() -> PhysAddr {
    interrupts::without_interrupts(|| {
        let pml4 = KERNEL_PML4.lock();
        PhysAddr::new(kernel_virtual_to_physical(addr_of!(pml4.pml4) as u64))
    })
}

pub fn load_address_space_phys(root_phys: PhysAddr) {
    interrupts::without_interrupts(|| unsafe {
        let frame = PhysFrame::containing_address(root_phys);
        Cr3::write(frame, Cr3Flags::empty());
    });
}

pub fn load_kernel_address_space() {
    load_address_space_phys(kernel_root_phys());
}

pub fn higher_half_addr(addr: u64) -> u64 {
    if addr >= KERNEL_VIRT_OFFSET {
        addr
    } else {
        addr + KERNEL_VIRT_OFFSET
    }
}

pub fn lower_half_addr(addr: u64) -> u64 {
    if addr >= KERNEL_VIRT_OFFSET {
        addr - KERNEL_VIRT_OFFSET
    } else {
        addr
    }
}

pub fn kernel_virtual_to_physical_addr(addr: u64) -> u64 {
    kernel_virtual_to_physical(addr)
}

pub fn map_mmio_range(phys_addr: u64, size: usize) -> Option<u64> {
    map_mmio_range_internal(phys_addr, size, false)
}

pub fn map_mmio_range_wc(phys_addr: u64, size: usize) -> Option<u64> {
    map_mmio_range_internal(phys_addr, size, true)
}

pub fn unmap_mmio_range(virt_addr: u64, size: usize) -> bool {
    if size == 0 {
        return false;
    }

    let virt_base = virt_addr & !(HUGE_2MIB - 1);
    let offset = virt_addr - virt_base;
    let span = match offset.checked_add(size.saturating_sub(1) as u64) {
        Some(span) => span,
        None => return false,
    };
    let block_count = (span / HUGE_2MIB + 1) as usize;

    interrupts::without_interrupts(|| {
        let mut pml4 = KERNEL_PML4.lock();
        pml4.unmap_mmio_blocks(virt_base, block_count)
    })
}

pub fn mmio_addr(phys_addr: u64) -> Option<u64> {
    map_mmio_range(phys_addr, 1)
}

pub fn mmio_addr_wc(phys_addr: u64) -> Option<u64> {
    map_mmio_range_wc(phys_addr, 1)
}

fn map_mmio_range_internal(phys_addr: u64, size: usize, write_combine: bool) -> Option<u64> {
    if size == 0 {
        return None;
    }

    let phys_block = phys_addr / HUGE_2MIB;
    let offset = phys_addr % HUGE_2MIB;
    let last = phys_addr.checked_add(size.saturating_sub(1) as u64)?;
    let last_block = last / HUGE_2MIB;
    let block_count = (last_block - phys_block + 1) as usize;
    let flags = if write_combine {
        MMIO_WRITE_COMBINE_FLAGS
    } else {
        MMIO_UNCACHED_FLAGS
    };
    interrupts::without_interrupts(|| {
        let mut pml4 = KERNEL_PML4.lock();
        pml4.map_mmio_blocks(phys_block, block_count, flags)
            .map(|virt_base| virt_base + offset)
    })
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
        ptr::copy_nonoverlapping(phys.as_u64() as *const u8, probe.as_mut_ptr(), probe.len());
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
