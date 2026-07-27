//! Reclaiming allocator for long-lived RustOS policy services.
//!
//! The kernel supplies a bootstrap heap so service startup never depends on a
//! userspace pager.  Freed allocations are returned to an address-ordered free
//! list and adjacent spans are coalesced, which bounds resident memory by peak
//! live demand instead of cumulative allocation traffic.  If the bootstrap
//! region cannot satisfy a request, a raw anonymous `mmap` adds another region.
//!
//! The allocator deliberately keeps mapped regions for the lifetime of the
//! service.  This avoids pager re-entry and `munmap` lifetime races while still
//! reclaiming every allocation inside those regions.

#[cfg(feature = "global-allocator")]
use core::alloc::{GlobalAlloc, Layout};
#[cfg(feature = "global-allocator")]
use core::cell::UnsafeCell;
#[cfg(feature = "global-allocator")]
use core::mem::{align_of, size_of};
#[cfg(feature = "global-allocator")]
use core::ptr;
#[cfg(feature = "global-allocator")]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "global-allocator")]
use crate::syscall;
use crate::BootstrapHeap;

const GROW_CHUNK_BYTES: usize = 1024 * 1024;

#[cfg(feature = "global-allocator")]
const ALLOCATION_COOKIE: usize = 0x5255_5354_4f53_414c;

#[cfg(feature = "global-allocator")]
#[repr(C)]
struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

#[cfg(feature = "global-allocator")]
#[repr(C)]
struct AllocationHeader {
    span_start: usize,
    span_size: usize,
    cookie: usize,
}

#[cfg(feature = "global-allocator")]
const FREE_ALIGN: usize = align_of::<FreeBlock>();
#[cfg(feature = "global-allocator")]
const MIN_FREE_BYTES: usize = size_of::<FreeBlock>();
#[cfg(feature = "global-allocator")]
const HEADER_BYTES: usize = size_of::<AllocationHeader>();

#[cfg(feature = "global-allocator")]
struct AllocatorState {
    free_head: *mut FreeBlock,
}

#[cfg(feature = "global-allocator")]
impl AllocatorState {
    const EMPTY: Self = Self {
        free_head: ptr::null_mut(),
    };

    unsafe fn add_region(&mut self, base: usize, len: usize) -> bool {
        let Some(start) = align_up(base, FREE_ALIGN) else {
            return false;
        };
        let Some(raw_end) = base.checked_add(len) else {
            return false;
        };
        let end = raw_end & !(FREE_ALIGN - 1);
        if end <= start || end - start < MIN_FREE_BYTES {
            return false;
        }
        self.insert_free_span(start, end - start);
        true
    }

    unsafe fn allocate(&mut self, layout: Layout) -> *mut u8 {
        let alignment = layout.align().max(FREE_ALIGN);
        let requested = layout.size().max(1);
        let mut link: *mut *mut FreeBlock = &mut self.free_head;

        while !(*link).is_null() {
            let block = *link;
            let block_start = block as usize;
            let block_size = (*block).size;
            let Some(block_end) = block_start.checked_add(block_size) else {
                return ptr::null_mut();
            };
            let Some(user) = align_up(
                match block_start.checked_add(HEADER_BYTES) {
                    Some(value) => value,
                    None => return ptr::null_mut(),
                },
                alignment,
            ) else {
                return ptr::null_mut();
            };
            let header_start = user - HEADER_BYTES;
            let allocation_start = if header_start - block_start >= MIN_FREE_BYTES {
                header_start
            } else {
                block_start
            };
            let Some(requested_end) = user.checked_add(requested) else {
                return ptr::null_mut();
            };
            let Some(mut allocation_end) = align_up(requested_end, FREE_ALIGN) else {
                return ptr::null_mut();
            };

            if allocation_end <= block_end {
                if block_end - allocation_end < MIN_FREE_BYTES {
                    allocation_end = block_end;
                }

                let next = (*block).next;
                *link = next;
                if allocation_start - block_start >= MIN_FREE_BYTES {
                    self.insert_free_span(block_start, allocation_start - block_start);
                }
                if block_end - allocation_end >= MIN_FREE_BYTES {
                    self.insert_free_span(allocation_end, block_end - allocation_end);
                }

                let header = (user - HEADER_BYTES) as *mut AllocationHeader;
                ptr::write(
                    header,
                    AllocationHeader {
                        span_start: allocation_start,
                        span_size: allocation_end - allocation_start,
                        cookie: ALLOCATION_COOKIE ^ user,
                    },
                );
                return user as *mut u8;
            }
            link = &mut (*block).next;
        }
        ptr::null_mut()
    }

    unsafe fn deallocate(&mut self, allocation: *mut u8) -> bool {
        let user = allocation as usize;
        let Some(header_start) = user.checked_sub(HEADER_BYTES) else {
            return false;
        };
        let header = header_start as *mut AllocationHeader;
        if (*header).cookie != ALLOCATION_COOKIE ^ user {
            return false;
        }
        let span_start = (*header).span_start;
        let span_size = (*header).span_size;
        if span_size < HEADER_BYTES
            || span_start > header_start
            || span_start
                .checked_add(span_size)
                .is_none_or(|end| end < user)
        {
            return false;
        }
        (*header).cookie = 0;
        self.insert_free_span(span_start, span_size);
        true
    }

    unsafe fn insert_free_span(&mut self, start: usize, size: usize) {
        debug_assert_eq!(start & (FREE_ALIGN - 1), 0);
        debug_assert!(size >= MIN_FREE_BYTES);

        let block = start as *mut FreeBlock;
        (*block).size = size;
        (*block).next = ptr::null_mut();

        let mut link: *mut *mut FreeBlock = &mut self.free_head;
        while !(*link).is_null() && (*link as usize) < start {
            link = &mut (**link).next;
        }
        (*block).next = *link;
        *link = block;
        self.coalesce();
    }

    unsafe fn coalesce(&mut self) {
        let mut current = self.free_head;
        while !current.is_null() {
            let next = (*current).next;
            if next.is_null() {
                break;
            }
            let Some(current_end) = (current as usize).checked_add((*current).size) else {
                break;
            };
            if current_end == next as usize {
                (*current).size += (*next).size;
                (*current).next = (*next).next;
            } else {
                current = next;
            }
        }
    }
}

#[cfg(feature = "global-allocator")]
struct ReclaimingAllocator {
    state: UnsafeCell<AllocatorState>,
    locked: AtomicBool,
    initialized: AtomicBool,
}

#[cfg(feature = "global-allocator")]
unsafe impl Sync for ReclaimingAllocator {}

#[cfg(feature = "global-allocator")]
impl ReclaimingAllocator {
    const fn new() -> Self {
        Self {
            state: UnsafeCell::new(AllocatorState::EMPTY),
            locked: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
        }
    }

    unsafe fn lock(&self) -> *mut AllocatorState {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        self.state.get()
    }

    unsafe fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }

    unsafe fn initialize(&self, base: usize, len: usize) -> bool {
        if base == 0
            || len == 0
            || self
                .initialized
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }

        let state = &mut *self.lock();
        let installed = state.add_region(base, len);
        self.unlock();
        if !installed {
            self.initialized.store(false, Ordering::Release);
        }
        installed
    }

    unsafe fn try_allocate(&self, layout: Layout) -> *mut u8 {
        let state = &mut *self.lock();
        let allocation = state.allocate(layout);
        self.unlock();
        allocation
    }

    unsafe fn allocate_with_growth<F>(&self, layout: Layout, grow: F) -> *mut u8
    where
        F: FnOnce(usize) -> Option<usize>,
    {
        let allocation = self.try_allocate(layout);
        if !allocation.is_null() {
            return allocation;
        }

        // A grow request can synchronously cross the syscall/service boundary.
        // Never hold the allocator spin lock across that wait: another service
        // thread may need the allocator to make the very progress that wakes us.
        let chunk = grow_chunk_bytes(layout);
        let Some(base) = grow(chunk) else {
            return ptr::null_mut();
        };

        let state = &mut *self.lock();
        let allocation = if state.add_region(base, chunk) {
            state.allocate(layout)
        } else {
            ptr::null_mut()
        };
        self.unlock();
        allocation
    }
}

#[cfg(feature = "global-allocator")]
unsafe impl GlobalAlloc for ReclaimingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.allocate_with_growth(layout, |chunk| {
            let mapped = syscall::mmap_anonymous(chunk);
            if mapped > 0 {
                Some(mapped as usize)
            } else {
                None
            }
        })
    }

    unsafe fn dealloc(&self, allocation: *mut u8, _layout: Layout) {
        if allocation.is_null() {
            return;
        }
        let state = &mut *self.lock();
        let _ = state.deallocate(allocation);
        self.unlock();
    }
}

#[cfg(feature = "global-allocator")]
fn grow_chunk_bytes(layout: Layout) -> usize {
    let overhead = HEADER_BYTES
        .saturating_add(layout.align())
        .saturating_add(MIN_FREE_BYTES);
    align_up(layout.size().saturating_add(overhead), 4096)
        .unwrap_or(usize::MAX & !4095)
        .max(GROW_CHUNK_BYTES)
}

#[cfg(feature = "global-allocator")]
fn align_up(value: usize, alignment: usize) -> Option<usize> {
    debug_assert!(alignment.is_power_of_two());
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

#[cfg(feature = "global-allocator")]
#[global_allocator]
static ALLOC: ReclaimingAllocator = ReclaimingAllocator::new();

/// Install the kernel-supplied bootstrap heap before service code allocates.
///
/// This is called exactly once by the static-PIE entry trampoline.
#[cfg(feature = "global-allocator")]
pub unsafe fn init(heap: BootstrapHeap) {
    let _ = ALLOC.initialize(heap.base, heap.len);
}

#[cfg(not(feature = "global-allocator"))]
pub unsafe fn init(_heap: BootstrapHeap) {}

#[cfg(all(test, feature = "global-allocator"))]
mod tests {
    extern crate std;

    use core::alloc::{GlobalAlloc, Layout};
    use std::alloc::System;

    use super::*;

    #[repr(align(4096))]
    struct TestHeap([u8; 32 * 1024]);

    #[test]
    fn freed_large_allocation_is_reused_without_growth() {
        let mut heap = TestHeap([0; 32 * 1024]);
        let allocator = ReclaimingAllocator::new();
        unsafe {
            assert!((&mut *allocator.state.get())
                .add_region(heap.0.as_mut_ptr() as usize, heap.0.len()));
            let layout = Layout::from_size_align(12 * 1024, 64).unwrap();
            let first = allocator.alloc(layout);
            assert!(!first.is_null());
            allocator.dealloc(first, layout);
            let second = allocator.alloc(layout);
            assert_eq!(second, first);
            allocator.dealloc(second, layout);
        }
    }

    #[test]
    fn cumulative_transient_traffic_is_bounded_by_peak_live_memory() {
        let mut heap = TestHeap([0; 32 * 1024]);
        let allocator = ReclaimingAllocator::new();
        unsafe {
            assert!((&mut *allocator.state.get())
                .add_region(heap.0.as_mut_ptr() as usize, heap.0.len()));
            let layout = Layout::from_size_align(12 * 1024, 64).unwrap();
            let mut first = ptr::null_mut();
            for iteration in 0..4096 {
                let allocation = allocator.alloc(layout);
                assert!(!allocation.is_null());
                if iteration == 0 {
                    first = allocation;
                } else {
                    assert_eq!(allocation, first);
                }
                allocator.dealloc(allocation, layout);
            }
        }
    }

    #[test]
    fn adjacent_frees_coalesce_for_a_larger_request() {
        let mut heap = TestHeap([0; 32 * 1024]);
        let allocator = ReclaimingAllocator::new();
        unsafe {
            assert!((&mut *allocator.state.get())
                .add_region(heap.0.as_mut_ptr() as usize, heap.0.len()));
            let small = Layout::from_size_align(6 * 1024, 16).unwrap();
            let first = allocator.alloc(small);
            let second = allocator.alloc(small);
            assert!(!first.is_null() && !second.is_null());
            allocator.dealloc(first, small);
            allocator.dealloc(second, small);

            let large = Layout::from_size_align(16 * 1024, 16).unwrap();
            let combined = allocator.alloc(large);
            assert!(!combined.is_null());
            allocator.dealloc(combined, large);
        }
    }

    #[test]
    fn allocator_honors_large_alignment() {
        let mut heap = TestHeap([0; 32 * 1024]);
        let allocator = ReclaimingAllocator::new();
        unsafe {
            assert!((&mut *allocator.state.get())
                .add_region(heap.0.as_mut_ptr() as usize, heap.0.len()));
            let layout = Layout::from_size_align(1024, 4096).unwrap();
            let allocation = allocator.alloc(layout);
            assert!(!allocation.is_null());
            assert_eq!(allocation as usize & 4095, 0);
            allocator.dealloc(allocation, layout);
        }
    }

    #[test]
    fn growth_is_page_aligned_and_bounded_by_request() {
        let small = Layout::from_size_align(16, 8).unwrap();
        assert_eq!(grow_chunk_bytes(small), GROW_CHUNK_BYTES);

        let large = Layout::from_size_align(2 * GROW_CHUNK_BYTES, 4096).unwrap();
        let chunk = grow_chunk_bytes(large);
        assert!(chunk >= large.size() + HEADER_BYTES);
        assert_eq!(chunk & 4095, 0);
    }

    #[test]
    fn growth_callback_runs_without_allocator_lock() {
        let allocator = ReclaimingAllocator::new();
        let layout = Layout::from_size_align(48 * 1024, 64).unwrap();
        let backing_layout = Layout::from_size_align(GROW_CHUNK_BYTES, 4096).unwrap();
        let backing = unsafe { System.alloc(backing_layout) };
        assert!(!backing.is_null());
        let allocation = unsafe {
            allocator.allocate_with_growth(layout, |chunk| {
                assert!(!allocator.locked.load(Ordering::Acquire));
                assert!(chunk >= layout.size() + HEADER_BYTES);
                Some(backing as usize)
            })
        };
        assert!(!allocation.is_null());
        unsafe {
            allocator.dealloc(allocation, layout);
            System.dealloc(backing, backing_layout);
        }
    }

    #[test]
    fn duplicate_release_is_rejected_without_free_list_overlap() {
        let mut heap = TestHeap([0; 32 * 1024]);
        let allocator = ReclaimingAllocator::new();
        unsafe {
            assert!((&mut *allocator.state.get())
                .add_region(heap.0.as_mut_ptr() as usize, heap.0.len()));
            let layout = Layout::from_size_align(4096, 64).unwrap();
            let allocation = allocator.alloc(layout);
            assert!(!allocation.is_null());

            let state = &mut *allocator.lock();
            assert!(state.deallocate(allocation));
            assert!(!state.deallocate(allocation));
            allocator.unlock();

            let first = allocator.alloc(layout);
            let second = allocator.alloc(layout);
            assert!(!first.is_null() && !second.is_null());
            assert_ne!(first, second);
            allocator.dealloc(first, layout);
            allocator.dealloc(second, layout);
        }
    }

    #[test]
    fn bootstrap_region_is_installed_once() {
        let mut heap = TestHeap([0; 32 * 1024]);
        let allocator = ReclaimingAllocator::new();
        unsafe {
            assert!(allocator.initialize(heap.0.as_mut_ptr() as usize, heap.0.len()));
            assert!(!allocator.initialize(heap.0.as_mut_ptr() as usize, heap.0.len()));

            let layout = Layout::from_size_align(12 * 1024, 64).unwrap();
            let first = allocator.alloc(layout);
            let second = allocator.alloc(layout);
            assert!(!first.is_null() && !second.is_null());
            assert_ne!(first, second);
            allocator.dealloc(first, layout);
            allocator.dealloc(second, layout);
        }
    }
}
