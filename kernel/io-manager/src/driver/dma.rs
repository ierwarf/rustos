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

struct StreamingDmaMapping {
    device_ptr: usize,
    _cpu_ptr: usize,
    mapping: DmaMapping,
}

static DMA_ALLOCATIONS: Mutex<Vec<DmaAllocation>> = Mutex::new(Vec::new());
static DMA_STREAMING_MAPPINGS: Mutex<Vec<StreamingDmaMapping>> = Mutex::new(Vec::new());

pub(crate) const DMA_MAPPING_ERROR: u64 = u64::MAX;

pub(crate) fn set_mask(device: *mut c_void, mask: u64) -> i32 {
    iommu::set_mask(device, mask)
}

pub(crate) fn set_coherent_mask(device: *mut c_void, mask: u64) -> i32 {
    iommu::set_coherent_mask(device, mask)
}

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

pub(crate) fn map_single(device: *mut c_void, cpu_addr: *mut c_void, size: usize) -> u64 {
    if cpu_addr.is_null() || size == 0 {
        return DMA_MAPPING_ERROR;
    }

    let phys_addr = crate::memory::paging::kernel_virtual_to_physical_addr(cpu_addr as u64);
    let Some(mapping) =
        iommu::map_physical_range(device, phys_addr, size, DmaMappingKind::Streaming)
    else {
        return DMA_MAPPING_ERROR;
    };

    irq_safe(|| {
        DMA_STREAMING_MAPPINGS.lock().push(StreamingDmaMapping {
            device_ptr: device as usize,
            _cpu_ptr: cpu_addr as usize,
            mapping,
        });
    });
    mapping.dma_addr.raw()
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

pub(crate) fn unmap_single(device: *mut c_void, dma_addr: u64, size: usize) {
    if dma_addr == DMA_MAPPING_ERROR {
        return;
    }

    irq_safe(|| {
        let mut mappings = DMA_STREAMING_MAPPINGS.lock();
        if let Some(index) = mappings.iter().position(|mapping| {
            mapping.device_ptr == device as usize
                && mapping.mapping.dma_addr.raw() == dma_addr
                && (size == 0 || mapping.mapping.size == size)
        }) {
            let mapping = mappings.remove(index);
            iommu::unmap_range(device, mapping.mapping);
        }
    });
}

pub(crate) fn mapping_error(dma_addr: u64) -> i32 {
    if dma_addr == DMA_MAPPING_ERROR { 1 } else { 0 }
}

pub(crate) fn sync_single_for_cpu(_device: *mut c_void, _dma_addr: u64, _size: usize) {}

pub(crate) fn sync_single_for_device(_device: *mut c_void, _dma_addr: u64, _size: usize) {}

fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    interrupts::without_interrupts(f)
}
