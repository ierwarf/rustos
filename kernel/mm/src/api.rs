//! Public ring0 memory-substrate API.
//!
//! - **Owner:** `kernel-mm`; other kernel crates use this module rather than
//!   private paging/frame implementations.
//! - **Boundary:** Every exported range, mapping, user-copy backing, and MMIO
//!   operation preserves the private implementation's complete admission.
//! - **Lifecycle:** APIs make reservation, publication, protection, unmap, and
//!   reclaim ownership explicit.
//! - **Concurrency:** Callers must satisfy the documented process/IRQ/lock
//!   context; this facade adds no hidden blocking.
//! - **Failure:** Fallible operations return without partial publication.
//! - **Forbidden:** No raw guest pointer, policy decision, or cross-crate
//!   private-module reach-through.
//! - **Evidence:** `memory-map`, `kernel-memory-protection`,
//!   `physical-frame-lifecycle`, and `user-memory-access`.
use boot_protocol::BootInfo;
use core::alloc::Layout;
use x86_64::PhysAddr;

pub mod alloc {
    use super::Layout;

    pub use crate::memory::heap::KernelAllocator;

    pub fn init_heap() {
        crate::memory::heap::init_heap();
    }

    pub fn handle_alloc_error(layout: Layout) -> ! {
        crate::memory::heap::handle_alloc_error(layout)
    }
}

pub mod heap {
    pub use crate::memory::heap::*;
}

pub mod address_space {
    pub use crate::memory::paging::ProcessAddressSpace;
    pub use x86_64::structures::paging::PageTableFlags;

    pub fn debug_direct_map_flags_for_addr(addr: u64) -> Option<PageTableFlags> {
        crate::memory::kernel_vm::debug_direct_map_flags_for_addr(addr)
    }
}

pub mod boot {
    use super::BootInfo;

    pub fn init_paging(boot_info_ptr: *const BootInfo) {
        crate::memory::paging::init(boot_info_ptr);
    }

    pub fn initialize_current_cpu_cache_attributes() -> bool {
        crate::memory::kernel_vm::initialize_current_cpu_cache_attributes()
    }

    pub fn init_phys(boot_info_ptr: *const BootInfo) {
        crate::memory::phys::init(boot_info_ptr);
    }

    pub fn paging_smoke_test() {
        crate::memory::paging::smoke_test();
    }
}

pub mod paging {
    pub use crate::memory::paging::*;
}

pub mod phys {
    pub use crate::memory::phys::*;

    use super::PhysAddr;

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
}

pub mod kernel_vm {
    pub use crate::memory::kernel_vm::*;
}

pub mod virt {
    pub fn higher_half_addr(addr: u64) -> u64 {
        crate::lowlevel::address::higher_half_addr(addr)
    }

    pub const fn kernel_virt_offset() -> u64 {
        crate::lowlevel::address::KERNEL_VIRT_OFFSET
    }
}

pub use address_space::{PageTableFlags, ProcessAddressSpace, debug_direct_map_flags_for_addr};
pub use alloc::{KernelAllocator, handle_alloc_error, init_heap};
pub use boot::{init_paging, init_phys, paging_smoke_test};
pub use phys::{alloc_frame, free_bytes, free_frame, usable_bytes};
pub use virt::{higher_half_addr, kernel_virt_offset};
