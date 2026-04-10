use boot_protocol::BootInfo;
use core::alloc::Layout;
use x86_64::PhysAddr;
pub use x86_64::structures::paging::PageTableFlags;

pub use crate::memory::heap::KernelAllocator;
pub use crate::memory::paging::ProcessAddressSpace;

pub fn init_paging(boot_info_ptr: *const BootInfo) {
    crate::memory::paging::init(boot_info_ptr);
}

pub fn init_phys(boot_info_ptr: *const BootInfo) {
    crate::memory::phys::init(boot_info_ptr);
}

pub fn init_heap() {
    crate::memory::heap::init_heap();
}

pub fn handle_alloc_error(layout: Layout) -> ! {
    crate::memory::heap::handle_alloc_error(layout)
}

pub fn higher_half_addr(addr: u64) -> u64 {
    crate::lowlevel::address::higher_half_addr(addr)
}

pub const fn kernel_virt_offset() -> u64 {
    crate::lowlevel::address::KERNEL_VIRT_OFFSET
}

pub fn paging_smoke_test() {
    crate::memory::paging::smoke_test();
}

pub fn usable_bytes() -> u64 {
    crate::memory::phys::usable_bytes()
}

pub fn free_bytes() -> u64 {
    crate::memory::phys::free_bytes()
}

pub fn alloc_frame() -> Option<PhysAddr> {
    crate::memory::phys::alloc_frame()
}

pub fn free_frame(phys: PhysAddr) {
    crate::memory::phys::free_frame(phys);
}

pub fn debug_direct_map_flags_for_addr(addr: u64) -> Option<PageTableFlags> {
    crate::memory::kernel_vm::debug_direct_map_flags_for_addr(addr)
}
