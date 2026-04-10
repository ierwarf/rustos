#[cfg(not(test))]
use buddy_system_allocator::LockedHeap;
#[cfg(not(test))]
use core::alloc::Layout;
#[cfg(not(test))]
use core::cell::UnsafeCell;
#[cfg(not(test))]
use spin::Once;

#[cfg(not(test))]
const HEAP_ORDER: usize = 32;
#[cfg(not(test))]
const HEAP_SIZE: usize = 256 * 1024;

#[cfg(not(test))]
#[repr(align(4096))]
struct HeapBytes([u8; HEAP_SIZE]);

#[cfg(not(test))]
struct HeapMemory(UnsafeCell<HeapBytes>);

#[cfg(not(test))]
unsafe impl Sync for HeapMemory {}

#[cfg(not(test))]
static HEAP_MEMORY: HeapMemory = HeapMemory(UnsafeCell::new(HeapBytes([0; HEAP_SIZE])));
#[cfg(not(test))]
static HEAP_INIT: Once<()> = Once::new();

#[cfg(not(test))]
#[global_allocator]
static HEAP: LockedHeap<HEAP_ORDER> = LockedHeap::<HEAP_ORDER>::new();

#[cfg(not(test))]
#[inline(always)]
fn heap_start() -> usize {
    unsafe { core::ptr::addr_of_mut!((*HEAP_MEMORY.0.get()).0) as *mut u8 as usize }
}

#[cfg(not(test))]
pub fn init_heap() {
    HEAP_INIT.call_once(|| unsafe {
        HEAP.lock().init(heap_start(), HEAP_SIZE);
    });
}

#[cfg(test)]
pub fn init_heap() {}

#[cfg(not(test))]
#[alloc_error_handler]
fn alloc_error_handler(layout: Layout) -> ! {
    panic!(
        "prekernel heap allocation failed: size={}, align={}",
        layout.size(),
        layout.align()
    );
}
