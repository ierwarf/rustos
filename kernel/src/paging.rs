use core::ptr::{addr_of, addr_of_mut};
use spin::Mutex;
use x86_64::PhysAddr;
use x86_64::instructions::interrupts;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::registers::model_specific::Msr;
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::structures::paging::{PageTable, PageTableFlags, PhysFrame};

const HUGE_2MIB: u64 = 2 * 1024 * 1024;
const ENTRIES_PER_TABLE: usize = 512;
const ADDRESS_SPACE_LIMIT: u64 = 512 * 1024 * 1024 * 1024;
const MAX_PAGE_BLOCK: u64 = ADDRESS_SPACE_LIMIT / HUGE_2MIB;
// 2 MiB huge-page PDE uses bit 12 as the PAT selector bit.
pub const WRITE_COMBINE_BIT: PageTableFlags = PageTableFlags::from_bits_retain(1 << 12);

pub static KERNEL_PML4: Mutex<PML4> = Mutex::new(PML4 {
    pml4: PageTable::new(),
    pdp: PageTable::new(),
    pd: [const { PageTable::new() }; ENTRIES_PER_TABLE],
});

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
pub struct PML4 {
    pml4: PageTable,
    pdp: PageTable,
    pd: [PageTable; ENTRIES_PER_TABLE],
}

impl PML4 {
    pub fn init(&mut self) {
        self.pml4 = PageTable::new();
        self.pdp = PageTable::new();
        self.pd = [const { PageTable::new() }; ENTRIES_PER_TABLE];

        self.pml4.zero();
        self.pdp.zero();

        let table_flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        let huge_flags = table_flags | PageTableFlags::HUGE_PAGE;

        let pdp_phys = PhysAddr::new(addr_of_mut!(self.pdp) as u64);
        self.pml4[0].set_addr(pdp_phys, table_flags);

        for pdp_index in 0..ENTRIES_PER_TABLE {
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

    fn block_check(&self, page_block: u64) {
        if page_block >= MAX_PAGE_BLOCK {
            panic!("Paging map error : address should be less than 512GB.");
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

    pub fn map(&mut self, virt_block: u64, phys_block: u64, flags: PageTableFlags) {
        self.block_check(virt_block);
        self.block_check(phys_block);

        let flags = flags | PageTableFlags::HUGE_PAGE;

        self.pd_entry_mut(virt_block)
            .set_addr(PhysAddr::new(phys_block * HUGE_2MIB), flags);
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
