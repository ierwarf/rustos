//! Minimal bump allocator backed by the kernel-supplied bootstrap heap.
//!
//! Memory is never freed back; `Box::drop` etc. simply leak. That is
//! acceptable because the policy services accumulate state monotonically and
//! the 16 MiB pre-mapped heap (plus optional `mmap` extensions) is plenty for
//! the workloads in [`services/syscalld`](../../../../../services/syscalld)
//! and [`services/vfsd`](../../../../../services/vfsd).
//!
//! When the bump cursor would overflow the current region, the allocator
//! issues a raw `mmap` syscall and switches to that new region. The kernel
//! bootstrap fallback at
//! [`kernel/compat/src/user/syscall/linux/memory_ops.rs`](../../../../../kernel/compat/src/user/syscall/linux/memory_ops.rs)
//! handles this even before syscalld is registered.

#[cfg(feature = "global-allocator")]
use core::alloc::{GlobalAlloc, Layout};
#[cfg(feature = "global-allocator")]
use core::cell::UnsafeCell;
#[cfg(feature = "global-allocator")]
use core::ptr;
#[cfg(feature = "global-allocator")]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "global-allocator")]
use crate::syscall;
use crate::BootstrapHeap;

const GROW_CHUNK_BYTES: usize = 1 * 1024 * 1024;

#[cfg(feature = "global-allocator")]
struct BumpRegion {
    base: usize,
    end: usize,
    cursor: usize,
}

#[cfg(feature = "global-allocator")]
impl BumpRegion {
    const EMPTY: Self = Self {
        base: 0,
        end: 0,
        cursor: 0,
    };
}

#[cfg(feature = "global-allocator")]
struct BumpAllocator {
    region: UnsafeCell<BumpRegion>,
    locked: AtomicBool,
}

#[cfg(feature = "global-allocator")]
unsafe impl Sync for BumpAllocator {}

#[cfg(feature = "global-allocator")]
impl BumpAllocator {
    const fn new() -> Self {
        Self {
            region: UnsafeCell::new(BumpRegion::EMPTY),
            locked: AtomicBool::new(false),
        }
    }

    unsafe fn lock(&self) -> *mut BumpRegion {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        self.region.get()
    }

    unsafe fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

#[cfg(feature = "global-allocator")]
unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let region_ptr = self.lock();
        let region = &mut *region_ptr;
        let align = layout.align().max(1);
        let size = layout.size();

        let aligned = (region.cursor + align - 1) & !(align - 1);
        let new_cursor = match aligned.checked_add(size) {
            Some(value) => value,
            None => {
                self.unlock();
                return ptr::null_mut();
            }
        };
        if new_cursor <= region.end {
            region.cursor = new_cursor;
            self.unlock();
            return aligned as *mut u8;
        }

        // Out of room — request a new chunk via raw mmap and bump there.
        let chunk = grow_chunk_bytes(size);
        let mapped = syscall::mmap_anonymous(chunk);
        if mapped < 0 || mapped == 0 {
            self.unlock();
            return ptr::null_mut();
        }
        let base = mapped as usize;
        let end = base + chunk;
        let aligned = (base + align - 1) & !(align - 1);
        let Some(cursor) = aligned.checked_add(size) else {
            self.unlock();
            return ptr::null_mut();
        };
        if cursor > end {
            self.unlock();
            return ptr::null_mut();
        }
        region.base = base;
        region.end = end;
        region.cursor = cursor;
        self.unlock();
        aligned as *mut u8
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump-only: drops are leaks. Acceptable for accumulating policy state.
    }
}

#[cfg(feature = "global-allocator")]
fn grow_chunk_bytes(min_required: usize) -> usize {
    let rounded = (min_required + 0xfff) & !0xfff;
    rounded.max(GROW_CHUNK_BYTES)
}

#[cfg(feature = "global-allocator")]
#[global_allocator]
static ALLOC: BumpAllocator = BumpAllocator::new();

/// Install the kernel-supplied bootstrap heap as the initial bump region.
///
/// Safe to call exactly once at process start before any allocation occurs.
#[cfg(feature = "global-allocator")]
pub(crate) unsafe fn init(heap: BootstrapHeap) {
    if heap.base == 0 || heap.len == 0 {
        return;
    }
    let region = &mut *ALLOC.region.get();
    region.base = heap.base;
    region.end = heap.base + heap.len;
    region.cursor = heap.base;
}

#[cfg(not(feature = "global-allocator"))]
pub(crate) unsafe fn init(_heap: BootstrapHeap) {}
