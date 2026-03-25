#[cfg(not(test))]
use buddy_system_allocator::LockedHeap;
#[cfg(not(test))]
use core::alloc::Layout;
use spin::Once;
#[cfg(not(test))]
use x86_64::PhysAddr;

#[cfg(not(test))]
use crate::memory::{kernel_vm, phys};

const HEAP_ORDER: usize = 32;
const PAGE_SIZE: usize = 4096;
const MIN_HEAP_SIZE: usize = 64 * 1024 * 1024;
const MAX_HEAP_SIZE: usize = 512 * 1024 * 1024;

static HEAP_INIT: Once<()> = Once::new();

#[cfg(not(test))]
#[global_allocator]
static HEAP: LockedHeap<HEAP_ORDER> = LockedHeap::<HEAP_ORDER>::new();

#[cfg(not(test))]
pub fn init_heap() {
    HEAP_INIT.call_once(|| unsafe {
        let usable_bytes = phys::usable_bytes() as usize;
        let target_bytes = usable_bytes
            .saturating_div(8)
            .clamp(MIN_HEAP_SIZE, MAX_HEAP_SIZE);
        let mut remaining_pages = target_bytes.div_ceil(PAGE_SIZE);
        let mut heap = HEAP.lock();
        let mut initialized = false;
        let mut added_bytes = 0usize;

        while remaining_pages != 0 {
            let Some((chunk_phys, chunk_pages)) = allocate_heap_chunk(remaining_pages) else {
                break;
            };

            let chunk_bytes = chunk_pages * PAGE_SIZE;
            let chunk_start = kernel_vm::higher_half_addr(chunk_phys.as_u64()) as usize;
            let chunk_end = chunk_start + chunk_bytes;

            if !initialized {
                heap.init(chunk_start, chunk_bytes);
                initialized = true;
            } else {
                heap.add_to_heap(chunk_start, chunk_end);
            }

            added_bytes = added_bytes.saturating_add(chunk_bytes);
            remaining_pages = remaining_pages.saturating_sub(chunk_pages);
        }

        if !initialized || added_bytes < target_bytes {
            panic!(
                "failed to initialize kernel heap: requested={} bytes, provisioned={} bytes",
                target_bytes, added_bytes
            );
        }
    });
}

#[cfg(test)]
pub fn init_heap() {}

#[cfg(not(test))]
pub fn is_initialized() -> bool {
    HEAP_INIT.is_completed()
}

#[cfg(test)]
pub fn is_initialized() -> bool {
    true
}

#[cfg(not(test))]
#[alloc_error_handler]
fn alloc_error_handler(layout: Layout) -> ! {
    if !is_initialized() {
        panic!(
            "kernel heap allocation before heap initialization: size={}, align={}",
            layout.size(),
            layout.align()
        );
    }

    panic!(
        "kernel heap allocation failed: size={}, align={}",
        layout.size(),
        layout.align()
    );
}

#[cfg(not(test))]
fn allocate_heap_chunk(max_pages: usize) -> Option<(PhysAddr, usize)> {
    let mut chunk_pages = max_pages;
    while chunk_pages != 0 {
        if let Some(phys) = phys::alloc_contiguous(chunk_pages) {
            return Some((phys, chunk_pages));
        }
        chunk_pages /= 2;
    }
    None
}
