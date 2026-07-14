use core::ffi::c_void;
use core::ptr;

use x86_64::PhysAddr;

const DMA_PAGE_SIZE: usize = 4096;

use crate::driver::iommu::{self, DmaMappingKind};
use crate::memory::{paging, phys};

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
    cpu_ptr.cast()
}
