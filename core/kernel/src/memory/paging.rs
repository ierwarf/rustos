pub use crate::memory::address_space::{
    AddressSpaceError, ProcessAddressSpace, UserRegion, USER_SPACE_BASE, USER_SPACE_END_EXCLUSIVE,
};
pub use crate::memory::kernel_vm::{KERNEL_PML4, KERNEL_VIRT_OFFSET, WRITE_COMBINE_BIT};

use x86_64::PhysAddr;

pub fn init() {
    crate::memory::kernel_vm::init();
}

pub fn kernel_root_phys() -> PhysAddr {
    crate::memory::kernel_vm::kernel_root_phys()
}

pub fn load_address_space_phys(root_phys: PhysAddr) {
    crate::memory::kernel_vm::load_address_space_phys(root_phys);
}

pub fn load_kernel_address_space() {
    crate::memory::kernel_vm::load_kernel_address_space();
}

pub fn higher_half_addr(addr: u64) -> u64 {
    crate::memory::kernel_vm::higher_half_addr(addr)
}

pub fn lower_half_addr(addr: u64) -> u64 {
    crate::memory::kernel_vm::lower_half_addr(addr)
}

pub fn kernel_virtual_to_physical_addr(addr: u64) -> u64 {
    crate::memory::kernel_vm::kernel_virtual_to_physical_addr(addr)
}

pub fn map_mmio_range(phys_addr: u64, size: usize) -> Option<u64> {
    crate::memory::kernel_vm::map_mmio_range(phys_addr, size)
}

pub fn map_mmio_range_wc(phys_addr: u64, size: usize) -> Option<u64> {
    crate::memory::kernel_vm::map_mmio_range_wc(phys_addr, size)
}

pub fn unmap_mmio_range(virt_addr: u64, size: usize) -> bool {
    crate::memory::kernel_vm::unmap_mmio_range(virt_addr, size)
}

pub fn mmio_addr(phys_addr: u64) -> Option<u64> {
    crate::memory::kernel_vm::mmio_addr(phys_addr)
}

pub fn mmio_addr_wc(phys_addr: u64) -> Option<u64> {
    crate::memory::kernel_vm::mmio_addr_wc(phys_addr)
}

pub fn smoke_test() {
    crate::memory::address_space::smoke_test();
}
