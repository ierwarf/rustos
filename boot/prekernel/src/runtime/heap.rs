#[cfg(all(not(test), rustos_boot_image))]
use buddy_system_allocator::LockedHeap;
#[cfg(all(not(test), rustos_boot_image))]
use core::alloc::Layout;
#[cfg(all(not(test), rustos_boot_image))]
use core::cell::UnsafeCell;
#[cfg(all(not(test), rustos_boot_image))]
use spin::Once;

#[cfg(all(not(test), rustos_boot_image))]
const HEAP_ORDER: usize = 32;
#[cfg(all(not(test), rustos_boot_image))]
const HEAP_SIZE: usize = 256 * 1024;

#[cfg(all(not(test), rustos_boot_image))]
#[repr(align(4096))]
struct HeapBytes([u8; HEAP_SIZE]);

#[cfg(all(not(test), rustos_boot_image))]
struct HeapMemory(UnsafeCell<HeapBytes>);

#[cfg(all(not(test), rustos_boot_image))]
unsafe impl Sync for HeapMemory {}

#[cfg(all(not(test), rustos_boot_image))]
static HEAP_MEMORY: HeapMemory = HeapMemory(UnsafeCell::new(HeapBytes([0; HEAP_SIZE])));
#[cfg(all(not(test), rustos_boot_image))]
static HEAP_INIT: Once<()> = Once::new();

#[cfg(all(not(test), rustos_boot_image))]
#[global_allocator]
static HEAP: LockedHeap<HEAP_ORDER> = LockedHeap::<HEAP_ORDER>::new();

#[cfg(all(not(test), rustos_boot_image))]
#[inline(always)]
fn heap_start() -> usize {
    unsafe { core::ptr::addr_of_mut!((*HEAP_MEMORY.0.get()).0) as *mut u8 as usize }
}

#[cfg(all(not(test), rustos_boot_image))]
pub fn init_heap() {
    HEAP_INIT.call_once(|| unsafe {
        HEAP.lock().init(heap_start(), HEAP_SIZE);
    });
}

#[cfg(any(test, not(rustos_boot_image)))]
pub fn init_heap() {}

#[cfg(all(not(test), rustos_boot_image))]
#[alloc_error_handler]
fn alloc_error_handler(layout: Layout) -> ! {
    panic!(
        "prekernel heap allocation failed: size={}, align={}",
        layout.size(),
        layout.align()
    );
}
