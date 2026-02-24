use buddy_system_allocator::LockedHeap;
use core::alloc::Layout;
use core::cell::UnsafeCell;
use spin::Once;

const HEAP_ORDER: usize = 32;
const HEAP_SIZE: usize = 1024 * 1024; // 1 MiB

#[repr(align(4096))]
struct HeapBytes([u8; HEAP_SIZE]);

struct KernelHeapMemory(UnsafeCell<HeapBytes>);

unsafe impl Sync for KernelHeapMemory {}

static HEAP_MEMORY: KernelHeapMemory = KernelHeapMemory(UnsafeCell::new(HeapBytes([0; HEAP_SIZE])));
static HEAP_INIT: Once<()> = Once::new();

#[global_allocator]
static HEAP: LockedHeap<HEAP_ORDER> = LockedHeap::<HEAP_ORDER>::new();

#[inline(always)]
fn heap_start() -> usize {
    unsafe { core::ptr::addr_of_mut!((*HEAP_MEMORY.0.get()).0) as *mut u8 as usize }
}

pub fn init_heap() {
    HEAP_INIT.call_once(|| {
        unsafe {
            HEAP.lock().init(heap_start(), HEAP_SIZE);
        }
    });
}

#[alloc_error_handler]
fn alloc_error_handler(layout: Layout) -> ! {
    panic!(
        "kernel heap allocation failed: size={}, align={}",
        layout.size(),
        layout.align()
    );
}
