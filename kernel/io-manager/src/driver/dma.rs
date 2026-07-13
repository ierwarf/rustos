use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;

use crate::sync::KernelSpinLock as Mutex;
use x86_64::PhysAddr;
use x86_64::instructions::interrupts;

const DMA_PAGE_SIZE: usize = 4096;

use crate::driver::iommu::{self, DmaMapping, DmaMappingKind};
use crate::memory::{paging, phys};

struct DmaAllocation {
    device_ptr: usize,
    cpu_ptr: usize,
    page_count: usize,
    mapping: DmaMapping,
}

static DMA_ALLOCATIONS: Mutex<Vec<DmaAllocation>> = Mutex::new(Vec::new());

pub(crate) const DMA_MAPPING_ERROR: u64 = u64::MAX;

pub(crate) fn set_mask_and_coherent(device: *mut c_void, mask: u64) -> i32 {
    iommu::set_mask_and_coherent(device, mask)
}

pub(crate) fn alloc_coherent(
    device: *mut c_void,
    size: usize,
    dma_handle: *mut u64,
) -> *mut c_void {
    if dma_handle.is_null() || size == 0 {
        return ptr::null_mut();
    }

    if size > usize::MAX.saturating_sub(DMA_PAGE_SIZE - 1) {
        return ptr::null_mut();
    }
    let page_count = size.div_ceil(DMA_PAGE_SIZE);
    let coherent_mask = iommu::coherent_mask(device);
    let Some(phys_start) = phys::alloc_contiguous_below(page_count, coherent_mask) else {
        return ptr::null_mut();
    };

    let cpu_ptr = paging::higher_half_addr(phys_start.as_u64()) as *mut u8;
    let zero_len = page_count * DMA_PAGE_SIZE;
    unsafe {
        ptr::write_bytes(cpu_ptr, 0, zero_len);
    }

    let Some(mapping) =
        iommu::map_physical_range(device, phys_start.as_u64(), size, DmaMappingKind::Coherent)
    else {
        for page_index in 0..page_count {
            let page_phys = phys_start.as_u64() + (page_index * DMA_PAGE_SIZE) as u64;
            phys::free_frame(PhysAddr::new(page_phys));
        }
        return ptr::null_mut();
    };

    unsafe {
        *dma_handle = mapping.dma_addr.raw();
    }
    irq_safe(|| {
        DMA_ALLOCATIONS.lock().push(DmaAllocation {
            device_ptr: device as usize,
            cpu_ptr: cpu_ptr as usize,
            page_count,
            mapping,
        });
    });
    cpu_ptr.cast()
}

pub(crate) fn free_coherent(device: *mut c_void, cpu_addr: *mut c_void, dma_handle: u64) {
    if cpu_addr.is_null() {
        return;
    }

    let allocation = irq_safe(|| {
        let mut allocations = DMA_ALLOCATIONS.lock();
        let index = allocations.iter().position(|allocation| {
            allocation.cpu_ptr == cpu_addr as usize
                && allocation.device_ptr == device as usize
                && allocation.mapping.dma_addr.raw() == dma_handle
        });
        index.map(|index| allocations.remove(index))
    });

    let Some(allocation) = allocation else {
        return;
    };
    iommu::unmap_range(device, allocation.mapping);
    for page_index in 0..allocation.page_count {
        let page_phys = allocation.mapping.phys_addr + (page_index * DMA_PAGE_SIZE) as u64;
        phys::free_frame(PhysAddr::new(page_phys));
    }
}

fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    interrupts::without_interrupts(f)
}
