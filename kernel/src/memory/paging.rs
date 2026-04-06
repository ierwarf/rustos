pub use crate::memory::address_space::{
    AddressSpaceError, ProcessAddressSpace, USER_SPACE_BASE, USER_SPACE_END_EXCLUSIVE, UserRegion,
};
pub use crate::memory::kernel_vm::{KERNEL_PML4, KERNEL_VIRT_OFFSET, WRITE_COMBINE_BIT};

use x86_64::PhysAddr;

pub fn init(boot_info_ptr: *const boot_protocol::BootInfo) {
    crate::memory::kernel_vm::init(boot_info_ptr);
}

pub fn kernel_root_phys() -> PhysAddr {
    crate::memory::kernel_vm::kernel_root_phys()
}

pub fn current_root_phys() -> PhysAddr {
    crate::memory::kernel_vm::current_root_phys()
}

pub fn load_address_space_phys(root_phys: PhysAddr) {
    crate::memory::kernel_vm::load_address_space_phys(root_phys);
}

#[allow(dead_code)]
pub fn load_kernel_address_space() {
    crate::memory::kernel_vm::load_kernel_address_space();
}

pub fn with_kernel_address_space<R>(f: impl FnOnce() -> R) -> R {
    crate::memory::kernel_vm::with_kernel_address_space(f)
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

pub fn mark_direct_map_range_executable(phys_addr: u64, size: usize) -> bool {
    crate::memory::kernel_vm::mark_direct_map_range_executable(phys_addr, size)
}

pub fn direct_map_phys_is_executable(phys_addr: u64) -> bool {
    crate::memory::kernel_vm::direct_map_phys_is_executable(phys_addr)
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

#[allow(dead_code)]
pub fn mmio_addr(phys_addr: u64) -> Option<u64> {
    crate::memory::kernel_vm::mmio_addr(phys_addr)
}

#[allow(dead_code)]
pub fn mmio_addr_wc(phys_addr: u64) -> Option<u64> {
    crate::memory::kernel_vm::mmio_addr_wc(phys_addr)
}

pub fn smoke_test() {
    crate::memory::address_space::smoke_test();
}
