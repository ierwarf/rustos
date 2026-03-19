use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr;

use spin::Mutex;
use x86_64::instructions::interrupts;

const DEFAULT_DMA_MASK: u64 = 0xffff_ffff;
const DMA_ALIGNMENT: usize = 4096;

struct DeviceDmaState {
    device_ptr: usize,
    dma_mask: u64,
    coherent_dma_mask: u64,
}

struct DmaAllocation {
    device_ptr: usize,
    cpu_ptr: usize,
    size: usize,
    align: usize,
    dma_handle: u64,
}

struct StreamingDmaMapping {
    device_ptr: usize,
    cpu_ptr: usize,
    size: usize,
    dma_addr: u64,
}

static DEVICE_DMA_STATES: Mutex<Vec<DeviceDmaState>> = Mutex::new(Vec::new());
static DMA_ALLOCATIONS: Mutex<Vec<DmaAllocation>> = Mutex::new(Vec::new());
static DMA_STREAMING_MAPPINGS: Mutex<Vec<StreamingDmaMapping>> = Mutex::new(Vec::new());

pub(crate) const DMA_MAPPING_ERROR: u64 = u64::MAX;

pub(crate) fn set_mask(device: *mut c_void, mask: u64) -> i32 {
    irq_safe(|| {
        let mut states = ensure_device_state(device as usize);
        if let Some(state) = states
            .iter_mut()
            .find(|state| state.device_ptr == device as usize)
        {
            state.dma_mask = mask;
        }
    });
    0
}

pub(crate) fn set_coherent_mask(device: *mut c_void, mask: u64) -> i32 {
    irq_safe(|| {
        let mut states = ensure_device_state(device as usize);
        if let Some(state) = states
            .iter_mut()
            .find(|state| state.device_ptr == device as usize)
        {
            state.coherent_dma_mask = mask;
        }
    });
    0
}

pub(crate) fn set_mask_and_coherent(device: *mut c_void, mask: u64) -> i32 {
    let status = set_mask(device, mask);
    if status != 0 {
        return status;
    }
    set_coherent_mask(device, mask)
}

pub(crate) fn alloc_coherent(
    device: *mut c_void,
    size: usize,
    dma_handle: *mut u64,
) -> *mut c_void {
    if dma_handle.is_null() || size == 0 {
        return ptr::null_mut();
    }

    let Ok(layout) = Layout::from_size_align(size, DMA_ALIGNMENT) else {
        return ptr::null_mut();
    };
    let cpu_ptr = unsafe { alloc_zeroed(layout) };
    if cpu_ptr.is_null() {
        return ptr::null_mut();
    }

    let phys = crate::paging::kernel_virtual_to_physical_addr(cpu_ptr as u64);
    let coherent_mask = irq_safe(|| coherent_mask_for(device as usize));
    let end = phys.saturating_add(size.saturating_sub(1) as u64);
    if end > coherent_mask {
        unsafe {
            dealloc(cpu_ptr, layout);
        }
        return ptr::null_mut();
    }

    unsafe {
        *dma_handle = phys;
    }
    irq_safe(|| {
        DMA_ALLOCATIONS.lock().push(DmaAllocation {
            device_ptr: device as usize,
            cpu_ptr: cpu_ptr as usize,
            size,
            align: DMA_ALIGNMENT,
            dma_handle: phys,
        });
    });
    cpu_ptr.cast()
}

pub(crate) fn map_single(device: *mut c_void, cpu_addr: *mut c_void, size: usize) -> u64 {
    if cpu_addr.is_null() || size == 0 {
        return DMA_MAPPING_ERROR;
    }

    let dma_addr = crate::paging::kernel_virtual_to_physical_addr(cpu_addr as u64);
    if exceeds_mask(dma_addr, size, irq_safe(|| dma_mask_for(device as usize))) {
        return DMA_MAPPING_ERROR;
    }

    irq_safe(|| {
        DMA_STREAMING_MAPPINGS.lock().push(StreamingDmaMapping {
            device_ptr: device as usize,
            cpu_ptr: cpu_addr as usize,
            size,
            dma_addr,
        });
    });
    dma_addr
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
                && allocation.dma_handle == dma_handle
        });
        index.map(|index| allocations.remove(index))
    });

    let Some(allocation) = allocation else {
        return;
    };
    let Ok(layout) = Layout::from_size_align(allocation.size, allocation.align) else {
        return;
    };
    unsafe {
        dealloc(allocation.cpu_ptr as *mut u8, layout);
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
                && mapping.dma_addr == dma_addr
                && (size == 0 || mapping.size == size)
        }) {
            mappings.remove(index);
        }
    });
}

pub(crate) fn mapping_error(dma_addr: u64) -> i32 {
    if dma_addr == DMA_MAPPING_ERROR { 1 } else { 0 }
}

pub(crate) fn sync_single_for_cpu(_device: *mut c_void, _dma_addr: u64, _size: usize) {}

pub(crate) fn sync_single_for_device(_device: *mut c_void, _dma_addr: u64, _size: usize) {}

fn coherent_mask_for(device_ptr: usize) -> u64 {
    DEVICE_DMA_STATES
        .lock()
        .iter()
        .find(|state| state.device_ptr == device_ptr)
        .map(|state| {
            if state.coherent_dma_mask != 0 {
                state.coherent_dma_mask
            } else if state.dma_mask != 0 {
                state.dma_mask
            } else {
                DEFAULT_DMA_MASK
            }
        })
        .unwrap_or(DEFAULT_DMA_MASK)
}

fn dma_mask_for(device_ptr: usize) -> u64 {
    DEVICE_DMA_STATES
        .lock()
        .iter()
        .find(|state| state.device_ptr == device_ptr)
        .map(|state| {
            if state.dma_mask != 0 {
                state.dma_mask
            } else {
                DEFAULT_DMA_MASK
            }
        })
        .unwrap_or(DEFAULT_DMA_MASK)
}

fn ensure_device_state(device_ptr: usize) -> spin::mutex::MutexGuard<'static, Vec<DeviceDmaState>> {
    let mut states = DEVICE_DMA_STATES.lock();
    if states.iter().all(|state| state.device_ptr != device_ptr) {
        states.push(DeviceDmaState {
            device_ptr,
            dma_mask: DEFAULT_DMA_MASK,
            coherent_dma_mask: DEFAULT_DMA_MASK,
        });
    }
    states
}

fn exceeds_mask(dma_addr: u64, size: usize, mask: u64) -> bool {
    let end = match dma_addr.checked_add(size.saturating_sub(1) as u64) {
        Some(end) => end,
        None => return true,
    };
    end > mask
}

fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    interrupts::without_interrupts(f)
}
