use core::convert::TryFrom;
use core::ptr::{addr_of, addr_of_mut};
use core::slice;

use boot_protocol::BootInfo;
use object::LittleEndian;
use object::elf::{
    self as objelf, FileHeader64 as RawElfHeader, ProgramHeader64 as RawProgramHeader,
};
use spin::Mutex;
use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::instructions::{interrupts, tlb};
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::registers::model_specific::Msr;
use x86_64::registers::model_specific::{Efer, EferFlags};
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::structures::paging::{PageTable, PageTableFlags, PhysFrame};

const ENTRIES_PER_TABLE: usize = 512;
const HUGE_2MIB: u64 = 2 * 1024 * 1024;
const PAGE_4KIB: u64 = 4096;
const KERNEL_PML4_SIZE_GB: usize = 512;
pub const DIRECT_MAP_PHYS_LIMIT: u64 = 512 * 1024 * 1024 * 1024;
const MAX_PAGE_BLOCK: u64 = DIRECT_MAP_PHYS_LIMIT / HUGE_2MIB;
const KERNEL_HIGHER_HALF_PML4_INDEX: usize = 256;
const MMIO_WINDOW_PML4_INDEX: usize = KERNEL_HIGHER_HALF_PML4_INDEX + 1;
const MMIO_WINDOW_SLOTS: usize = 16;
const DIRECT_MAP_SPLIT_TABLES: usize = 128;
pub use crate::lowlevel::address::KERNEL_VIRT_OFFSET;
const MMIO_WINDOW_BASE: u64 = KERNEL_VIRT_OFFSET + (1_u64 << 39);
const MMIO_UNMAPPED_BLOCK: u64 = u64::MAX;
const SPLIT_BLOCK_UNMAPPED: u64 = u64::MAX;
const KERNEL_LOAD_BIAS_MIN: u64 = 0x0020_0000;
const MAX_KERNEL_PHYSICAL_KASLR_SLIDE: u64 = 0x0020_0000;
const ELF64_HEADER_SIZE: usize = 64;
const ELF64_PROGRAM_HEADER_SIZE: usize = 56;
const ELF64_DYNAMIC_ENTRY_SIZE: usize = 16;
const ELF64_RELA_ENTRY_SIZE: usize = 24;
const ELF_ENDIAN: LittleEndian = LittleEndian;
const DT_NULL: i64 = 0;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const R_X86_64_RELATIVE: u32 = 8;

// 2 MiB huge-page PDE uses bit 12 as the PAT selector bit.
pub const WRITE_COMBINE_BIT: PageTableFlags = PageTableFlags::from_bits_retain(1 << 12);
const MMIO_UNCACHED_FLAGS: PageTableFlags = PageTableFlags::NO_CACHE;
const MMIO_WRITE_COMBINE_FLAGS: PageTableFlags = WRITE_COMBINE_BIT;

#[derive(Clone, Copy)]
struct MmioWindowSlot {
    phys_block: u64,
    #[allow(dead_code)]
    flags_bits: u64,
}

#[derive(Clone, Copy)]
struct DynamicSegmentInfo {
    addr: u64,
    size: usize,
}

#[derive(Clone, Copy)]
struct DynamicRelocationTable {
    addr: u64,
    size: usize,
}

impl MmioWindowSlot {
    const fn unmapped() -> Self {
        Self {
            phys_block: MMIO_UNMAPPED_BLOCK,
            flags_bits: 0,
        }
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
    split_pt: [const { PageTable::new() }; DIRECT_MAP_SPLIT_TABLES],
    split_blocks: [SPLIT_BLOCK_UNMAPPED; DIRECT_MAP_SPLIT_TABLES],
});

#[repr(C)]
pub struct PML4<const SIZE_GB: usize> {
    pub(crate) pml4: PageTable,
    pdp: PageTable,
    pd: [PageTable; SIZE_GB],
    mmio_pdp: PageTable,
    mmio_pd: PageTable,
    mmio_blocks: [MmioWindowSlot; MMIO_WINDOW_SLOTS],
    split_pt: [PageTable; DIRECT_MAP_SPLIT_TABLES],
    split_blocks: [u64; DIRECT_MAP_SPLIT_TABLES],
}

impl<const SIZE_GB: usize> PML4<SIZE_GB> {
    pub fn init(&mut self, boot_info_ptr: *const BootInfo) {
        self.pml4 = PageTable::new();
        self.pdp = PageTable::new();
        self.pd = [const { PageTable::new() }; SIZE_GB];
        self.mmio_pdp = PageTable::new();
        self.mmio_pd = PageTable::new();
        self.mmio_blocks = [MmioWindowSlot::unmapped(); MMIO_WINDOW_SLOTS];
        self.split_pt = [const { PageTable::new() }; DIRECT_MAP_SPLIT_TABLES];
        self.split_blocks = [SPLIT_BLOCK_UNMAPPED; DIRECT_MAP_SPLIT_TABLES];

        self.pml4.zero();
        self.pdp.zero();
        self.mmio_pdp.zero();
        self.mmio_pd.zero();
        for table in &mut self.split_pt {
            table.zero();
        }

        let table_flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        let huge_flags = table_flags | PageTableFlags::HUGE_PAGE | PageTableFlags::NO_EXECUTE;

        let pdp_phys = PhysAddr::new(kernel_virtual_to_physical(addr_of_mut!(self.pdp) as u64));
        self.pml4[0].set_addr(pdp_phys, table_flags);
        self.pml4[KERNEL_HIGHER_HALF_PML4_INDEX].set_addr(pdp_phys, table_flags);
        let mmio_pdp_phys = PhysAddr::new(kernel_virtual_to_physical(
            addr_of_mut!(self.mmio_pdp) as u64
        ));
        let mmio_pd_phys =
            PhysAddr::new(kernel_virtual_to_physical(addr_of_mut!(self.mmio_pd) as u64));
        self.pml4[MMIO_WINDOW_PML4_INDEX].set_addr(mmio_pdp_phys, table_flags);
        self.mmio_pdp[0].set_addr(mmio_pd_phys, table_flags);

        for pdp_index in 0..SIZE_GB {
            self.pd[pdp_index].zero();

            let pd_phys = PhysAddr::new(kernel_virtual_to_physical(
                addr_of_mut!(self.pd[pdp_index]) as u64,
            ));
            self.pdp[pdp_index].set_addr(pd_phys, table_flags);

            let gib_base = (pdp_index as u64) << 30;
            for pd_index in 0..ENTRIES_PER_TABLE {
                let phys = PhysAddr::new(gib_base + (pd_index as u64) * HUGE_2MIB);
                self.pd[pdp_index][pd_index].set_addr(phys, huge_flags);
            }
        }
        self.protect_loaded_kernel_image(boot_info_ptr)
            .expect("kernel image executable protections must be derivable");
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
        let huge_flags =
            table_flags | PageTableFlags::HUGE_PAGE | PageTableFlags::NO_EXECUTE | mmio_flags;

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

    fn ensure_current_root_mmio_window_entry(&self) {
        let kernel_root_phys =
            PhysAddr::new(kernel_virtual_to_physical(addr_of!(self.pml4) as u64));
        let (current_root, _) = Cr3::read();
        if current_root.start_address() == kernel_root_phys {
            return;
        }

        let kernel_entry = &self.pml4[MMIO_WINDOW_PML4_INDEX];
        if kernel_entry.is_unused() {
            return;
        }

        let current_root = unsafe { phys_to_table_mut(current_root.start_address()) };
        let current_entry = &mut current_root[MMIO_WINDOW_PML4_INDEX];
        if current_entry.addr() == kernel_entry.addr()
            && current_entry.flags() == kernel_entry.flags()
        {
            return;
        }

        current_entry.set_addr(kernel_entry.addr(), kernel_entry.flags());
    }

    fn unmap_mmio_blocks(&mut self, virt_base: u64, block_count: usize) -> bool {
        if block_count == 0 || virt_base < MMIO_WINDOW_BASE || virt_base % HUGE_2MIB != 0 {
            return false;
        }

        let slot_start = ((virt_base - MMIO_WINDOW_BASE) / HUGE_2MIB) as usize;
        if slot_start
            .checked_add(block_count)
            .is_none_or(|end| end > MMIO_WINDOW_SLOTS)
        {
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
        if page_block >= MAX_PAGE_BLOCK {
            panic!("paging map error: block index out of range");
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

    fn pd_entry_ref(&self, page_block: u64) -> &PageTableEntry {
        let (pdp_idx, pd_idx) = self.pd_indices(page_block);
        &self.pd[pdp_idx][pd_idx]
    }

    fn flush_block(virt_block: u64) {
        tlb::flush(VirtAddr::new(virt_block * HUGE_2MIB));
    }

    fn flush_direct_map_page(phys_addr: u64) {
        let page_base = align_down(phys_addr, PAGE_4KIB);
        tlb::flush(VirtAddr::new(page_base));
        tlb::flush(VirtAddr::new(higher_half_addr(page_base)));
    }

    fn flush_split_block(block_index: u64) {
        let phys_base = block_index * HUGE_2MIB;
        let mut page = phys_base;
        let end = phys_base + HUGE_2MIB;
        while page < end {
            Self::flush_direct_map_page(page);
            page += PAGE_4KIB;
        }
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

    fn find_split_slot(&self, page_block: u64) -> Option<usize> {
        self.split_blocks
            .iter()
            .position(|block| *block == page_block)
    }

    fn allocate_split_slot(&mut self, page_block: u64) -> Option<usize> {
        let slot = self
            .split_blocks
            .iter()
            .position(|block| *block == SPLIT_BLOCK_UNMAPPED)?;
        self.split_blocks[slot] = page_block;
        self.split_pt[slot].zero();
        Some(slot)
    }

    fn ensure_split_block_table(&mut self, page_block: u64) -> Option<&mut PageTable> {
        if let Some(slot) = self.find_split_slot(page_block) {
            return Some(&mut self.split_pt[slot]);
        }

        let (phys_base, leaf_flags) = {
            let entry = self.pd_entry_ref(page_block);
            if entry.is_unused() || !entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                return None;
            }
            (
                entry.addr().as_u64(),
                entry.flags() & !PageTableFlags::HUGE_PAGE,
            )
        };

        let slot = self.allocate_split_slot(page_block)?;
        let table = &mut self.split_pt[slot];
        for page_index in 0..ENTRIES_PER_TABLE {
            let page_phys = phys_base + page_index as u64 * PAGE_4KIB;
            table[page_index].set_addr(PhysAddr::new(page_phys), leaf_flags);
        }

        let table_flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        let pt_phys = PhysAddr::new(kernel_virtual_to_physical(
            addr_of_mut!(self.split_pt[slot]) as u64,
        ));
        self.pd_entry_mut(page_block).set_addr(pt_phys, table_flags);
        Self::flush_split_block(page_block);
        Some(&mut self.split_pt[slot])
    }

    fn update_page_range_flags(
        &mut self,
        phys_start: u64,
        size: u64,
        add_flags: PageTableFlags,
        remove_flags: PageTableFlags,
    ) -> Result<(), &'static str> {
        if size == 0 {
            return Ok(());
        }

        let start = align_down(phys_start, PAGE_4KIB);
        let end = align_up(
            phys_start
                .checked_add(size)
                .ok_or("direct-map protection range overflow")?,
            PAGE_4KIB,
        )
        .ok_or("direct-map protection range overflow")?;
        if end > DIRECT_MAP_PHYS_LIMIT {
            return Err("direct-map protection range exceeds mapped physical limit");
        }

        let mut page_phys = start;
        while page_phys < end {
            let block_index = page_phys / HUGE_2MIB;
            let block_end = (block_index + 1) * HUGE_2MIB;
            if page_phys % HUGE_2MIB == 0
                && block_end <= end
                && self.find_split_slot(block_index).is_none()
            {
                let entry = self.pd_entry_mut(block_index);
                if !entry.is_unused() && entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                    let mut flags = entry.flags();
                    flags.remove(remove_flags);
                    flags |= add_flags | PageTableFlags::HUGE_PAGE;
                    entry.set_addr(entry.addr(), flags);
                    Self::flush_block(block_index);
                    page_phys = block_end;
                    continue;
                }
            }

            let page_index = ((page_phys % HUGE_2MIB) / PAGE_4KIB) as usize;
            let table = self
                .ensure_split_block_table(block_index)
                .ok_or("direct-map split table budget exhausted")?;
            let entry = &mut table[page_index];
            let mut flags = entry.flags();
            flags.remove(remove_flags | PageTableFlags::HUGE_PAGE);
            flags |= add_flags;
            entry.set_addr(entry.addr(), flags);
            Self::flush_direct_map_page(page_phys);
            page_phys += PAGE_4KIB;
        }

        Ok(())
    }

    fn direct_map_phys_flags(&self, phys_addr: u64) -> Option<PageTableFlags> {
        if phys_addr >= DIRECT_MAP_PHYS_LIMIT {
            return None;
        }

        let block_index = phys_addr / HUGE_2MIB;
        let entry = self.pd_entry_ref(block_index);
        if entry.is_unused() {
            return None;
        }
        if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Some(entry.flags());
        }

        let slot = self.find_split_slot(block_index)?;
        let page_index = ((phys_addr % HUGE_2MIB) / PAGE_4KIB) as usize;
        let page = &self.split_pt[slot][page_index];
        if page.is_unused() {
            return None;
        }
        Some(page.flags())
    }

    fn protect_loaded_kernel_image(
        &mut self,
        boot_info_ptr: *const BootInfo,
    ) -> Result<(), &'static str> {
        let (load_bias, header) = find_loaded_kernel_image(boot_info_ptr)?;
        let elf_type = header.e_type.get(ELF_ENDIAN);
        let phoff = usize::try_from(header.e_phoff.get(ELF_ENDIAN))
            .map_err(|_| "kernel ELF program-header offset overflow")?;
        let phentsize = usize::from(header.e_phentsize.get(ELF_ENDIAN));
        let phnum = usize::from(header.e_phnum.get(ELF_ENDIAN));
        if phentsize != ELF64_PROGRAM_HEADER_SIZE || phnum == 0 {
            return Err("kernel ELF program headers are invalid");
        }

        for index in 0..phnum {
            let ph_phys: u64 = load_bias
                .checked_add(phoff as u64)
                .and_then(|value: u64| value.checked_add((index * phentsize) as u64))
                .ok_or("kernel ELF program-header address overflow")?;
            let ph = read_raw_program_header_at(ph_phys)?;
            if ph.p_type.get(ELF_ENDIAN) != objelf::PT_LOAD {
                continue;
            }

            let mem_size = ph.p_memsz.get(ELF_ENDIAN);
            if mem_size == 0 {
                continue;
            }

            let segment_start = segment_phys_addr(load_bias, elf_type, &ph)?;
            let segment_end = segment_start
                .checked_add(mem_size)
                .ok_or("kernel PT_LOAD range overflow")?;
            let (add_flags, remove_flags) = kernel_segment_flag_delta(ph.p_flags.get(ELF_ENDIAN))?;
            self.update_page_range_flags(
                segment_start,
                segment_end - segment_start,
                add_flags,
                remove_flags,
            )?;
            self.audit_kernel_segment_protection(
                index,
                segment_start,
                segment_end,
                ph.p_flags.get(ELF_ENDIAN),
            )?;
        }

        Ok(())
    }

    fn audit_kernel_segment_protection(
        &self,
        _index: usize,
        segment_start: u64,
        segment_end: u64,
        program_flags: u32,
    ) -> Result<(), &'static str> {
        let executable = (program_flags & objelf::PF_X) != 0;
        let writable = (program_flags & objelf::PF_W) != 0;
        let first_flags = self
            .direct_map_phys_flags(segment_start)
            .ok_or("kernel PT_LOAD first page flags missing after protection")?;
        let last_flags = self
            .direct_map_phys_flags(segment_end.saturating_sub(1))
            .ok_or("kernel PT_LOAD last page flags missing after protection")?;

        for flags in [first_flags, last_flags] {
            if executable {
                if flags.contains(PageTableFlags::NO_EXECUTE) {
                    return Err("kernel executable PT_LOAD page remained NX");
                }
                if flags.contains(PageTableFlags::WRITABLE) {
                    return Err("kernel executable PT_LOAD page remained writable");
                }
            } else {
                if !flags.contains(PageTableFlags::NO_EXECUTE) {
                    return Err("kernel non-executable PT_LOAD page is executable");
                }
                if writable && !flags.contains(PageTableFlags::WRITABLE) {
                    return Err("kernel writable PT_LOAD page is read-only");
                }
                if !writable && flags.contains(PageTableFlags::WRITABLE) {
                    return Err("kernel read-only PT_LOAD page remained writable");
                }
            }
        }

        Ok(())
    }

    pub unsafe fn load(&self) {
        let pml4_phys = PhysAddr::new(kernel_virtual_to_physical(addr_of!(self.pml4) as u64));
        let pml4_frame = PhysFrame::containing_address(pml4_phys);
        unsafe {
            Cr3::write(pml4_frame, Cr3Flags::empty());
        }
    }
}

pub fn init(boot_info_ptr: *const BootInfo) {
    unsafe {
        set_pat_wc_slot4();
        interrupts::without_interrupts(|| {
            Efer::write(Efer::read() | EferFlags::NO_EXECUTE_ENABLE);
            crate::debug::boot_trace::println_fmt(format_args!("kernel: paging nxe enabled"));
            let mut pml4 = KERNEL_PML4.lock();
            crate::debug::boot_trace::println_fmt(format_args!("kernel: paging tables init begin"));
            pml4.init(boot_info_ptr);
            crate::debug::boot_trace::println_fmt(format_args!("kernel: paging tables init done"));
            rebase_loaded_kernel_image_to_higher_half(boot_info_ptr)
                .expect("kernel higher-half rebase must succeed");
            crate::debug::boot_trace::println_fmt(format_args!(
                "kernel: paging higher-half rebase done"
            ));
            pml4.load();
            crate::debug::boot_trace::println_fmt(format_args!("kernel: paging root loaded"));
        });
    }
}

pub fn kernel_root_phys() -> PhysAddr {
    interrupts::without_interrupts(|| {
        let pml4 = KERNEL_PML4.lock();
        PhysAddr::new(kernel_virtual_to_physical(addr_of!(pml4.pml4) as u64))
    })
}

pub fn current_root_phys() -> PhysAddr {
    interrupts::without_interrupts(|| {
        let (frame, _) = Cr3::read();
        frame.start_address()
    })
}

pub fn load_address_space_phys(root_phys: PhysAddr) {
    interrupts::without_interrupts(|| unsafe {
        let frame = PhysFrame::containing_address(root_phys);
        Cr3::write(frame, Cr3Flags::empty());
    });
}

// Kept as explicit arch hooks even when current callers go through higher-level paging wrappers.
#[allow(dead_code)]
pub fn load_kernel_address_space() {
    load_address_space_phys(kernel_root_phys());
}

pub fn with_kernel_address_space<R>(f: impl FnOnce() -> R) -> R {
    interrupts::without_interrupts(|| {
        let kernel_root = kernel_root_phys();
        let current_root = current_root_phys();
        if current_root == kernel_root {
            return f();
        }

        load_address_space_phys(kernel_root);
        let result = f();
        load_address_space_phys(current_root);
        result
    })
}

pub fn higher_half_addr(addr: u64) -> u64 {
    crate::lowlevel::address::higher_half_addr(addr)
}

pub fn lower_half_addr(addr: u64) -> u64 {
    crate::lowlevel::address::lower_half_addr(addr)
}

pub fn kernel_virtual_to_physical_addr(addr: u64) -> u64 {
    kernel_virtual_to_physical(addr)
}

pub fn direct_map_flags_for_phys(phys_addr: u64) -> Option<PageTableFlags> {
    interrupts::without_interrupts(|| {
        let pml4 = KERNEL_PML4.lock();
        pml4.direct_map_phys_flags(phys_addr)
    })
}

pub fn update_direct_map_range_flags(
    phys_addr: u64,
    size: usize,
    add_flags: PageTableFlags,
    remove_flags: PageTableFlags,
) -> bool {
    if size == 0 {
        return false;
    }

    interrupts::without_interrupts(|| {
        let mut pml4 = KERNEL_PML4.lock();
        pml4.update_page_range_flags(phys_addr, size as u64, add_flags, remove_flags)
            .is_ok()
    })
}

pub fn mark_direct_map_range_executable(phys_addr: u64, size: usize) -> bool {
    if size == 0 {
        return false;
    }

    interrupts::without_interrupts(|| {
        let mut pml4 = KERNEL_PML4.lock();
        pml4.update_page_range_flags(
            phys_addr,
            size as u64,
            PageTableFlags::empty(),
            PageTableFlags::NO_EXECUTE | PageTableFlags::WRITABLE,
        )
        .is_ok()
    })
}

pub fn mark_direct_map_range_writable_noexec(phys_addr: u64, size: usize) -> bool {
    if size == 0 {
        return false;
    }

    interrupts::without_interrupts(|| {
        let mut pml4 = KERNEL_PML4.lock();
        pml4.update_page_range_flags(
            phys_addr,
            size as u64,
            PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
            PageTableFlags::empty(),
        )
        .is_ok()
    })
}

pub fn mark_direct_map_range_readonly_noexec(phys_addr: u64, size: usize) -> bool {
    if size == 0 {
        return false;
    }

    interrupts::without_interrupts(|| {
        let mut pml4 = KERNEL_PML4.lock();
        pml4.update_page_range_flags(
            phys_addr,
            size as u64,
            PageTableFlags::NO_EXECUTE,
            PageTableFlags::WRITABLE,
        )
        .is_ok()
    })
}

pub fn direct_map_phys_is_executable(phys_addr: u64) -> bool {
    interrupts::without_interrupts(|| {
        let pml4 = KERNEL_PML4.lock();
        pml4.direct_map_phys_flags(phys_addr)
            .is_some_and(|flags: PageTableFlags| !flags.contains(PageTableFlags::NO_EXECUTE))
    })
}

pub fn debug_direct_map_flags_for_addr(addr: u64) -> Option<PageTableFlags> {
    let phys_addr = lower_half_addr(addr);
    interrupts::without_interrupts(|| {
        let pml4 = KERNEL_PML4.lock();
        pml4.direct_map_phys_flags(phys_addr)
    })
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

#[allow(dead_code)]
pub fn mmio_addr(phys_addr: u64) -> Option<u64> {
    map_mmio_range(phys_addr, 1)
}

#[allow(dead_code)]
pub fn mmio_addr_wc(phys_addr: u64) -> Option<u64> {
    map_mmio_range_wc(phys_addr, 1)
}

pub(crate) unsafe fn phys_to_table_ref(phys: PhysAddr) -> &'static PageTable {
    unsafe { &*(higher_half_addr(phys.as_u64()) as *const PageTable) }
}

pub(crate) unsafe fn phys_to_table_mut(phys: PhysAddr) -> &'static mut PageTable {
    unsafe { &mut *(higher_half_addr(phys.as_u64()) as *mut PageTable) }
}

fn set_pat_wc_slot4() {
    const IA32_PAT: u32 = 0x277;
    const PAT_WC: u64 = 0x01;

    let mut msr = Msr::new(IA32_PAT);
    let mut pat = unsafe { msr.read() };
    pat &= !(0xff_u64 << 32);
    pat |= PAT_WC << 32;
    unsafe {
        msr.write(pat);
    }
}

fn kernel_virtual_to_physical(addr: u64) -> u64 {
    if addr >= KERNEL_VIRT_OFFSET {
        addr - KERNEL_VIRT_OFFSET
    } else {
        addr
    }
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

fn find_loaded_kernel_image(
    boot_info_ptr: *const BootInfo,
) -> Result<(u64, RawElfHeader<LittleEndian>), &'static str> {
    if let Some(load_bias) = loaded_kernel_load_bias_from_boot_info(boot_info_ptr) {
        let header = read_raw_elf_header_at(load_bias)?;
        if header.e_ident.magic == objelf::ELFMAG
            && header.e_ident.class == objelf::ELFCLASS64
            && header.e_ident.data == objelf::ELFDATA2LSB
            && header.e_version.get(ELF_ENDIAN) == objelf::EV_CURRENT as u32
            && header.e_machine.get(ELF_ENDIAN) == objelf::EM_X86_64
        {
            return Ok((load_bias, header));
        }
        return Err("boot info kernel image load bias does not point at a valid ELF header");
    }

    let scan_end = KERNEL_LOAD_BIAS_MIN
        .checked_add(MAX_KERNEL_PHYSICAL_KASLR_SLIDE)
        .ok_or("kernel ELF scan window overflow")?;

    let mut candidate = KERNEL_LOAD_BIAS_MIN;
    while candidate <= scan_end {
        let header = read_raw_elf_header_at(candidate)?;
        if header.e_ident.magic == objelf::ELFMAG
            && header.e_ident.class == objelf::ELFCLASS64
            && header.e_ident.data == objelf::ELFDATA2LSB
            && header.e_version.get(ELF_ENDIAN) == objelf::EV_CURRENT as u32
            && header.e_machine.get(ELF_ENDIAN) == objelf::EM_X86_64
        {
            return Ok((candidate, header));
        }
        candidate = candidate.saturating_add(PAGE_4KIB);
    }

    Err("loaded kernel ELF header not found in expected scan window")
}

fn loaded_kernel_load_bias_from_boot_info(boot_info_ptr: *const BootInfo) -> Option<u64> {
    let boot_info = unsafe { BootInfo::from_ptr(boot_info_ptr) }.ok()?;
    let image = boot_info.nucleus_image;
    image.is_present().then_some(image.load_bias)
}

fn read_raw_elf_header_at(phys_addr: u64) -> Result<RawElfHeader<LittleEndian>, &'static str> {
    let bytes = unsafe { slice::from_raw_parts(phys_addr as *const u8, ELF64_HEADER_SIZE) };
    if bytes.len() < ELF64_HEADER_SIZE {
        return Err("kernel ELF header is truncated");
    }
    Ok(unsafe { (bytes.as_ptr() as *const RawElfHeader<LittleEndian>).read_unaligned() })
}

fn read_raw_program_header_at(
    phys_addr: u64,
) -> Result<RawProgramHeader<LittleEndian>, &'static str> {
    let bytes = unsafe { slice::from_raw_parts(phys_addr as *const u8, ELF64_PROGRAM_HEADER_SIZE) };
    if bytes.len() < ELF64_PROGRAM_HEADER_SIZE {
        return Err("kernel ELF program header is truncated");
    }
    Ok(unsafe { (bytes.as_ptr() as *const RawProgramHeader<LittleEndian>).read_unaligned() })
}

fn segment_phys_addr(
    load_bias: u64,
    elf_type: u16,
    program_header: &RawProgramHeader<LittleEndian>,
) -> Result<u64, &'static str> {
    if elf_type == objelf::ET_DYN {
        return load_bias
            .checked_add(program_header.p_vaddr.get(ELF_ENDIAN))
            .ok_or("kernel PT_LOAD relocated address overflow");
    }

    let physical = program_header.p_paddr.get(ELF_ENDIAN);
    if physical != 0 {
        Ok(physical)
    } else {
        Ok(program_header.p_vaddr.get(ELF_ENDIAN))
    }
}

fn find_dynamic_segment(
    load_bias: u64,
    header: &RawElfHeader<LittleEndian>,
) -> Result<Option<DynamicSegmentInfo>, &'static str> {
    let elf_type = header.e_type.get(ELF_ENDIAN);
    let phoff = usize::try_from(header.e_phoff.get(ELF_ENDIAN))
        .map_err(|_| "kernel ELF dynamic phoff overflow")?;
    let phentsize = usize::from(header.e_phentsize.get(ELF_ENDIAN));
    let phnum = usize::from(header.e_phnum.get(ELF_ENDIAN));
    if phentsize != ELF64_PROGRAM_HEADER_SIZE {
        return Err("kernel ELF dynamic phentsize is invalid");
    }

    for index in 0..phnum {
        let ph_phys = load_bias
            .checked_add(phoff as u64)
            .and_then(|value| value.checked_add((index * phentsize) as u64))
            .ok_or("kernel ELF dynamic phdr address overflow")?;
        let ph = read_raw_program_header_at(ph_phys)?;
        if ph.p_type.get(ELF_ENDIAN) != objelf::PT_DYNAMIC {
            continue;
        }

        let addr = segment_phys_addr(load_bias, elf_type, &ph)?;
        let size = usize::try_from(ph.p_memsz.get(ELF_ENDIAN))
            .map_err(|_| "kernel ELF dynamic segment size overflow")?;
        if size == 0 {
            return Err("kernel ELF dynamic segment is empty");
        }
        return Ok(Some(DynamicSegmentInfo { addr, size }));
    }

    Ok(None)
}

fn parse_dynamic_relocation_table(
    header: &RawElfHeader<LittleEndian>,
    load_bias: u64,
    dynamic: DynamicSegmentInfo,
) -> Result<Option<DynamicRelocationTable>, &'static str> {
    let elf_type = header.e_type.get(ELF_ENDIAN);
    let entry_count = dynamic.size / ELF64_DYNAMIC_ENTRY_SIZE;
    let mut rela_addr = None;
    let mut rela_size = None;
    let mut rela_ent = None;

    for index in 0..entry_count {
        let entry_addr = dynamic
            .addr
            .checked_add((index * ELF64_DYNAMIC_ENTRY_SIZE) as u64)
            .ok_or("kernel ELF dynamic entry address overflow")?;
        let tag = read_i64_from_memory(entry_addr);
        let value = read_u64_from_memory(entry_addr + 8);
        match tag {
            DT_NULL => break,
            DT_RELA => {
                rela_addr = Some(if elf_type == objelf::ET_DYN {
                    load_bias
                        .checked_add(value)
                        .ok_or("kernel ELF rela address overflow")?
                } else {
                    value
                });
            }
            DT_RELASZ => {
                rela_size =
                    Some(usize::try_from(value).map_err(|_| "kernel ELF rela size overflow")?);
            }
            DT_RELAENT => {
                rela_ent =
                    Some(usize::try_from(value).map_err(|_| "kernel ELF rela ent size overflow")?);
            }
            _ => {}
        }
    }

    let Some(addr) = rela_addr else {
        return Ok(None);
    };
    let Some(size) = rela_size else {
        return Ok(None);
    };
    if size == 0 {
        return Ok(None);
    }
    if rela_ent.unwrap_or(ELF64_RELA_ENTRY_SIZE) != ELF64_RELA_ENTRY_SIZE {
        return Err("kernel ELF rela entry size is unsupported");
    }
    if size % ELF64_RELA_ENTRY_SIZE != 0 {
        return Err("kernel ELF rela size is not aligned");
    }
    Ok(Some(DynamicRelocationTable { addr, size }))
}

fn rebase_loaded_kernel_image_to_higher_half(
    boot_info_ptr: *const BootInfo,
) -> Result<(), &'static str> {
    let (load_bias, header) = find_loaded_kernel_image(boot_info_ptr)?;
    if header.e_type.get(ELF_ENDIAN) != objelf::ET_DYN {
        return Ok(());
    }

    let Some(dynamic) = find_dynamic_segment(load_bias, &header)? else {
        return Err("kernel ELF PT_DYNAMIC segment not found");
    };
    let Some(relocations) = parse_dynamic_relocation_table(&header, load_bias, dynamic)? else {
        return Ok(());
    };

    let high_bias = higher_half_addr(load_bias);
    let rela_count = relocations.size / ELF64_RELA_ENTRY_SIZE;
    let mut applied = 0usize;
    for index in 0..rela_count {
        let entry_addr = relocations
            .addr
            .checked_add((index * ELF64_RELA_ENTRY_SIZE) as u64)
            .ok_or("kernel ELF rela entry overflow")?;
        let offset = read_u64_from_memory(entry_addr);
        let info = read_u64_from_memory(entry_addr + 8);
        let addend = read_i64_from_memory(entry_addr + 16);

        if (info as u32) != R_X86_64_RELATIVE {
            return Err("kernel ELF contains unsupported relocation type");
        }
        if (info >> 32) != 0 {
            return Err("kernel ELF relative relocation references a symbol");
        }

        let target = load_bias
            .checked_add(offset)
            .ok_or("kernel ELF relocation target overflow")?;
        let rebased = add_signed_u64(high_bias, addend)
            .ok_or("kernel ELF relocation rebased value overflow")?;
        write_u64_to_memory(target, rebased);
        applied += 1;
    }

    crate::debug::boot_trace::println_fmt(format_args!(
        "kernel: rebased {} kernel relative relocations to higher half",
        applied
    ));
    Ok(())
}

fn kernel_segment_flag_delta(
    program_flags: u32,
) -> Result<(PageTableFlags, PageTableFlags), &'static str> {
    let writable = (program_flags & objelf::PF_W) != 0;
    let executable = (program_flags & objelf::PF_X) != 0;
    if writable && executable {
        return Err("kernel PT_LOAD segment must not be writable and executable");
    }

    if executable {
        Ok((
            PageTableFlags::empty(),
            PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
        ))
    } else if writable {
        Ok((
            PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
            PageTableFlags::empty(),
        ))
    } else {
        Ok((PageTableFlags::NO_EXECUTE, PageTableFlags::WRITABLE))
    }
}

fn add_signed_u64(base: u64, offset: i64) -> Option<u64> {
    if offset >= 0 {
        base.checked_add(offset as u64)
    } else {
        base.checked_sub(offset.unsigned_abs())
    }
}

fn read_i64_from_memory(addr: u64) -> i64 {
    unsafe { (addr as *const i64).read_unaligned() }
}

fn read_u64_from_memory(addr: u64) -> u64 {
    unsafe { (addr as *const u64).read_unaligned() }
}

fn write_u64_to_memory(addr: u64, value: u64) {
    unsafe { (addr as *mut u64).write_unaligned(value) }
}

fn mmio_slot_base(slot: usize) -> u64 {
    MMIO_WINDOW_BASE + slot as u64 * HUGE_2MIB
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
        pml4.ensure_current_root_mmio_window_entry();
        pml4.map_mmio_blocks(phys_block, block_count, flags)
            .map(|virt_base| virt_base + offset)
    })
}
