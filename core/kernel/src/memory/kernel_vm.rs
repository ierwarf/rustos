use core::ptr::{addr_of, addr_of_mut};

use spin::Mutex;
use x86_64::instructions::{interrupts, tlb};
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::registers::model_specific::Msr;
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::structures::paging::{PageTable, PageTableFlags, PhysFrame};
use x86_64::PhysAddr;
use x86_64::VirtAddr;

const ENTRIES_PER_TABLE: usize = 512;
const HUGE_2MIB: u64 = 2 * 1024 * 1024;
const KERNEL_PML4_SIZE_GB: usize = 512;
pub(crate) const DIRECT_MAP_PHYS_LIMIT: u64 = 512 * 1024 * 1024 * 1024;
const MAX_PAGE_BLOCK: u64 = DIRECT_MAP_PHYS_LIMIT / HUGE_2MIB;
const KERNEL_HIGHER_HALF_PML4_INDEX: usize = 256;
const MMIO_WINDOW_PML4_INDEX: usize = KERNEL_HIGHER_HALF_PML4_INDEX + 1;
const MMIO_WINDOW_SLOTS: usize = 16;
pub const KERNEL_VIRT_OFFSET: u64 = 0xffff_8000_0000_0000;
const MMIO_WINDOW_BASE: u64 = KERNEL_VIRT_OFFSET + (1_u64 << 39);
const MMIO_UNMAPPED_BLOCK: u64 = u64::MAX;

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
});

#[repr(C)]
pub struct PML4<const SIZE_GB: usize> {
    pub(crate) pml4: PageTable,
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

// Kept as explicit arch hooks even when current callers go through higher-level paging wrappers.
#[allow(dead_code)]
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
        pml4.map_mmio_blocks(phys_block, block_count, flags)
            .map(|virt_base| virt_base + offset)
    })
}
