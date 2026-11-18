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
//!
//! - **Owner:** Each service process owns one allocator instance and its mapped
//!   regions; no allocator state is shared across protection domains.
//! - **Boundary:** Bootstrap spans, mmap results, layouts, headers, and free
//!   spans are checked for overflow, alignment, containment, and provenance.
//! - **Lifecycle:** Admit bootstrap memory, allocate/split, validate on free,
//!   coalesce, retain mapped regions until process exit, and let process teardown
//!   reclaim the mappings.
//! - **Concurrency:** A futex-backed allocator lock (short spin, then sleep so a
//!   descheduled holder is never starved by its waiters) protects metadata;
//!   growth occurs only after releasing the metadata lock and is re-admitted
//!   before publish.
//! - **Failure:** Invalid layout, exhausted address space, corrupt metadata, and
//!   mmap failure return allocation failure rather than fabricating memory.
//! - **Forbidden:** No kernel-policy re-entry while locked, foreign-pointer free,
//!   silent integer wrap, or unmapping a region containing live allocations.
//! - **Evidence:** `service-heap-lifecycle`, `allocator-reentrancy`, and
//!   `kernel-resource-lifecycle`.

#[cfg(feature = "global-allocator")]
use core::alloc::{GlobalAlloc, Layout};
#[cfg(feature = "global-allocator")]
use core::cell::UnsafeCell;
#[cfg(feature = "global-allocator")]
use core::mem::{align_of, size_of};
#[cfg(feature = "global-allocator")]
use core::ptr;
#[cfg(feature = "global-allocator")]
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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
/// Smallest tail worth splitting off when an allocation shrinks in place.
#[cfg(feature = "global-allocator")]
const SHRINK_SPLIT_BYTES: usize = 256;

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

    /// Resize a live allocation without moving it. Returns false when the
    /// allocation is invalid or cannot reach `new_size` without a move, in
    /// which case nothing was changed and the caller falls back to
    /// allocate-copy-free. Never fires for alignments above what the original
    /// allocation guaranteed because `user` itself does not move.
    unsafe fn resize_in_place(&mut self, allocation: *mut u8, new_size: usize) -> bool {
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
        let Some(span_end) = span_start.checked_add(span_size) else {
            return false;
        };
        if span_start > header_start || span_end < user {
            return false;
        }
        let Some(wanted_end) = user
            .checked_add(new_size.max(1))
            .and_then(|end| align_up(end, FREE_ALIGN))
        else {
            return false;
        };
        if wanted_end <= span_end {
            // Shrink (or growth inside existing slack). Give back a tail only
            // when it is large enough to be worth a free-list node.
            let tail = span_end - wanted_end;
            if tail >= SHRINK_SPLIT_BYTES {
                (*header).span_size = wanted_end - span_start;
                self.insert_free_span(wanted_end, tail);
            }
            return true;
        }
        // Growth: absorb the free block that starts exactly at the span end.
        let mut link: *mut *mut FreeBlock = &mut self.free_head;
        while !(*link).is_null() && (*link as usize) < span_end {
            link = &mut (**link).next;
        }
        let block = *link;
        if block.is_null() || block as usize != span_end {
            return false;
        }
        let block_size = (*block).size;
        let Some(block_end) = span_end.checked_add(block_size) else {
            return false;
        };
        if wanted_end > block_end {
            return false;
        }
        let next = (*block).next;
        *link = next;
        let mut new_end = wanted_end;
        if block_end - new_end < MIN_FREE_BYTES {
            new_end = block_end;
        }
        (*header).span_size = new_end - span_start;
        if block_end > new_end {
            self.insert_free_span(new_end, block_end - new_end);
        }
        true
    }

    unsafe fn insert_free_span(&mut self, start: usize, size: usize) {
        debug_assert_eq!(start & (FREE_ALIGN - 1), 0);
        debug_assert!(size >= MIN_FREE_BYTES);

        let block = start as *mut FreeBlock;
        (*block).size = size;
        (*block).next = ptr::null_mut();

        let mut previous: *mut FreeBlock = ptr::null_mut();
        let mut link: *mut *mut FreeBlock = &mut self.free_head;
        while !(*link).is_null() && (*link as usize) < start {
            previous = *link;
            link = &mut (**link).next;
        }
        (*block).next = *link;
        *link = block;

        // Only the new span's neighbours can have become adjacent, so merge
        // locally instead of rescanning the whole free list on every free.
        let next = (*block).next;
        if !next.is_null() && start.checked_add((*block).size) == Some(next as usize) {
            (*block).size += (*next).size;
            (*block).next = (*next).next;
        }
        if !previous.is_null()
            && (previous as usize).checked_add((*previous).size) == Some(start)
        {
            (*previous).size += (*block).size;
            (*previous).next = (*block).next;
        }
    }
}

#[cfg(feature = "global-allocator")]
struct ReclaimingAllocator {
    state: UnsafeCell<AllocatorState>,
    /// `UNLOCKED`, `LOCKED`, or `CONTENDED` (locked with at least one sleeper).
    locked: AtomicU32,
    initialized: AtomicBool,
}

#[cfg(feature = "global-allocator")]
const UNLOCKED: u32 = 0;
#[cfg(feature = "global-allocator")]
const LOCKED: u32 = 1;
#[cfg(feature = "global-allocator")]
const CONTENDED: u32 = 2;
/// Spins before sleeping. Only useful when the holder runs on another CPU; on
/// one CPU the holder cannot make progress while its waiter spins, so the
/// bound is small and the wait then blocks instead of burning the budget.
#[cfg(feature = "global-allocator")]
const LOCK_SPINS: u32 = 64;

#[cfg(feature = "global-allocator")]
unsafe impl Sync for ReclaimingAllocator {}

#[cfg(feature = "global-allocator")]
impl ReclaimingAllocator {
    const fn new() -> Self {
        Self {
            state: UnsafeCell::new(AllocatorState::EMPTY),
            locked: AtomicU32::new(UNLOCKED),
            initialized: AtomicBool::new(false),
        }
    }

    /// # Safety
    /// The caller must pair this with exactly one `unlock` and must not touch
    /// the returned state after it.
    #[inline]
    unsafe fn lock(&self) -> *mut AllocatorState {
        // ORDERING: Acquire on success makes the previous holder's writes to
        // the free list visible; the failure path publishes nothing.
        if self
            .locked
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.lock_contended();
        }
        self.state.get()
    }

    /// A pure spin lock is unsound for a service heap: the holder can be
    /// descheduled (or wait on a page fault) inside its critical section, and
    /// every waiter then burns the process's whole scheduling budget while the
    /// holder is starved. On one CPU that livelocked `uiserver` for good (both
    /// the UI thread and a worker spinning in `alloc`/`dealloc`, the holder
    /// never dispatched). Waiters therefore sleep on a futex after a short
    /// spin and the unlock wakes one of them.
    ///
    /// # Safety
    /// Same contract as `lock`.
    #[cold]
    unsafe fn lock_contended(&self) {
        for _ in 0..LOCK_SPINS {
            // ORDERING: the Relaxed load is only a hint; the acquiring
            // compare-exchange below is what takes the lock.
            if self.locked.load(Ordering::Relaxed) == UNLOCKED
                && self
                    .locked
                    // ORDERING: Acquire on success, as in `lock`: the previous holder's free-list
                    // writes become visible; the Relaxed failure path publishes and reads nothing.
                    .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                return;
            }
            core::hint::spin_loop();
        }
        // Mark the lock contended before sleeping so the holder's unlock wakes
        // us; the swap also takes the lock if it was released meanwhile.
        // ORDERING: Acquire when the swap observes UNLOCKED, for the same
        // reason as the fast path.
        while self.locked.swap(CONTENDED, Ordering::Acquire) != UNLOCKED {
            // SAFETY: `locked` is a live process-private `AtomicU32`; the
            // kernel only reads it, and spurious wakeups re-run the swap.
            unsafe { syscall::futex_wait_private(self.locked.as_ptr(), CONTENDED) };
        }
    }

    /// # Safety
    /// The caller must hold the lock taken by `lock`.
    #[inline]
    unsafe fn unlock(&self) {
        // ORDERING: Release publishes the critical section's free-list writes
        // to the next acquirer; CONTENDED means someone may be asleep.
        if self.locked.swap(UNLOCKED, Ordering::Release) == CONTENDED {
            // SAFETY: as in `lock_contended`; a wake with no waiter is a no-op.
            unsafe { syscall::futex_wake_private(self.locked.as_ptr(), 1) };
        }
    }

    unsafe fn initialize(&self, base: usize, len: usize) -> bool {
        if base == 0
            || len == 0
            || self
                .initialized
                // ORDERING: one-shot admission latch. It guards no data itself (the free list is
                // protected by `locked`); AcqRel on success and Acquire on failure also order
                // it against the Release rollback store below from a failed first caller.
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }

        let state = &mut *self.lock();
        let installed = state.add_region(base, len);
        self.unlock();
        if !installed {
            // ORDERING: Release rollback of the latch after `add_region` rejected the
            // region, so a later `initialize` that wins the CAS observes the reset; no
            // region was installed, so no other data is published by this store.
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

    unsafe fn realloc(&self, allocation: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY (caller contract): `new_size` is non-zero and, rounded up to
        // `layout.align()`, does not overflow `isize`.
        let state = &mut *self.lock();
        let resized = state.resize_in_place(allocation, new_size);
        self.unlock();
        if resized {
            return allocation;
        }
        let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
        let moved = self.alloc(new_layout);
        if !moved.is_null() {
            ptr::copy_nonoverlapping(allocation, moved, layout.size().min(new_size));
            self.dealloc(allocation, layout);
        }
        moved
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
        // SAFETY: the test region `heap` is a live, 4096-aligned array that outlives
        // `allocator`; the allocator is unshared (single thread), so the state is
        // exclusively accessed, and every allocation is freed with its own layout.
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
        // SAFETY: as in the previous test: private live `heap`, single thread, each
        // allocation freed with the layout it was made with.
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
        // SAFETY: private live `heap`, single thread; both allocations are freed with
        // their own layouts and the coalesced block is released before `heap` drops.
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
    fn realloc_grows_and_shrinks_in_place_and_preserves_contents() {
        let mut heap = TestHeap([0; 32 * 1024]);
        let allocator = ReclaimingAllocator::new();
        // SAFETY: private live `heap`, single thread; each allocation is freed with
        // the layout it currently has.
        unsafe {
            assert!((&mut *allocator.state.get())
                .add_region(heap.0.as_mut_ptr() as usize, heap.0.len()));
            let small = Layout::from_size_align(64, 16).unwrap();
            let block = allocator.alloc(small);
            assert!(!block.is_null());
            core::ptr::write_bytes(block, 0xab, 64);
            let grown = allocator.realloc(block, small, 8 * 1024);
            assert_eq!(grown, block, "free tail follows, so growth stays in place");
            assert!((0..64).all(|i| *grown.add(i) == 0xab));
            core::ptr::write_bytes(grown, 0xcd, 8 * 1024);
            let big = Layout::from_size_align(8 * 1024, 16).unwrap();
            let shrunk = allocator.realloc(grown, big, 128);
            assert_eq!(shrunk, grown);
            assert!((0..128).all(|i| *shrunk.add(i) == 0xcd));
            // A neighbour blocks in-place growth: the block must move intact.
            let neighbour = allocator.alloc(small);
            assert!(!neighbour.is_null());
            let small128 = Layout::from_size_align(128, 16).unwrap();
            let moved = allocator.realloc(shrunk, small128, 4 * 1024);
            assert!(!moved.is_null());
            assert!((0..128).all(|i| *moved.add(i) == 0xcd));
            let moved_layout = Layout::from_size_align(4 * 1024, 16).unwrap();
            allocator.dealloc(moved, moved_layout);
            allocator.dealloc(neighbour, small);
            // Everything coalesced back: one allocation spanning most of the heap fits.
            let whole = Layout::from_size_align(30 * 1024, 16).unwrap();
            let all = allocator.alloc(whole);
            assert!(!all.is_null());
            allocator.dealloc(all, whole);
        }
    }

    /// Walk the free list and the live headers and assert the heap invariants:
    /// free blocks are address-ordered, aligned, large enough, non-adjacent
    /// (fully coalesced) and disjoint from every live span; live spans are
    /// disjoint, contain their header and user bytes, and the free plus live
    /// bytes account for the whole region.
    unsafe fn check_heap(
        allocator: &ReclaimingAllocator,
        base: usize,
        len: usize,
        live: &[(*mut u8, Layout, u8)],
    ) {
        let state = &*allocator.state.get();
        let mut spans: std::vec::Vec<(usize, usize, bool)> = std::vec::Vec::new();
        let mut block = state.free_head;
        let mut previous_end = 0_usize;
        while !block.is_null() {
            let start = block as usize;
            let size = (*block).size;
            assert_eq!(start & (FREE_ALIGN - 1), 0);
            assert!(size >= MIN_FREE_BYTES && size & (FREE_ALIGN - 1) == 0);
            assert!(start >= previous_end, "free list unordered or overlapping");
            assert!(start != previous_end || previous_end == 0, "uncoalesced");
            previous_end = start + size;
            spans.push((start, size, true));
            block = (*block).next;
        }
        for &(allocation, layout, _) in live {
            let user = allocation as usize;
            assert_eq!(user % layout.align(), 0);
            let header = (user - HEADER_BYTES) as *const AllocationHeader;
            assert_eq!((*header).cookie, ALLOCATION_COOKIE ^ user);
            let (start, size) = ((*header).span_start, (*header).span_size);
            assert!(start <= user - HEADER_BYTES);
            assert!(start + size >= user + layout.size());
            assert_eq!(size & (FREE_ALIGN - 1), 0);
            spans.push((start, size, false));
        }
        spans.sort();
        let mut cursor = base;
        let mut accounted = 0;
        for &(start, size, _) in &spans {
            assert!(start >= cursor, "overlapping spans");
            assert!(start + size <= base + len);
            cursor = start + size;
            accounted += size;
        }
        // Only sub-FREE_ALIGN head/tail slack of the region may be unaccounted.
        assert!(len - accounted < 2 * FREE_ALIGN);
    }

    #[test]
    fn randomized_realloc_alloc_free_keeps_heap_invariants_and_contents() {
        let mut heap = std::vec![0_u8; 8 * 1024 * 1024 + 64];
        let allocator = ReclaimingAllocator::new();
        let base = heap.as_mut_ptr() as usize;
        let len = heap.len();
        // SAFETY: private live `heap`, single thread; every pointer is released with
        // the layout it currently has and tracked in `live`.
        unsafe {
            assert!((&mut *allocator.state.get()).add_region(base, len));
            let (region_base, region_len) = {
                let start = align_up(base, FREE_ALIGN).unwrap();
                (start, (base + len) / FREE_ALIGN * FREE_ALIGN - start)
            };
            let mut live: std::vec::Vec<(*mut u8, Layout, u8)> = std::vec::Vec::new();
            let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
            let mut next = move || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            };
            let mut in_place = 0_u32;
            for step in 0..20_000_u32 {
                let roll = next() % 10;
                let aligns = [8_usize, 16, 32, 64, 256];
                let pick = (next() % 997) as usize;
                if (roll < 4 || live.is_empty()) && live.len() < 200 {
                    let align = aligns[(next() % 5) as usize];
                    let size = 1 + (pick * 7) % 3000;
                    let layout = Layout::from_size_align(size, align).unwrap();
                    let allocation = allocator.alloc(layout);
                    if !allocation.is_null() {
                        let tag = (step & 0xff) as u8;
                        ptr::write_bytes(allocation, tag, size);
                        live.push((allocation, layout, tag));
                    }
                } else if roll < 7 {
                    let index = pick % live.len();
                    let (allocation, layout, tag) = live[index];
                    let new_size = match next() % 4 {
                        0 => 1,
                        1 => layout.size() + 1 + pick % 16,
                        2 => layout.size() / 2 + 1,
                        _ => 1 + (pick * 13) % 6000,
                    };
                    let moved = allocator.realloc(allocation, layout, new_size);
                    if moved.is_null() {
                        continue;
                    }
                    if moved == allocation {
                        in_place += 1;
                    }
                    let keep = layout.size().min(new_size);
                    assert!((0..keep).all(|i| *moved.add(i) == tag), "contents lost");
                    ptr::write_bytes(moved, tag, new_size);
                    live[index] = (moved, Layout::from_size_align(new_size, layout.align()).unwrap(), tag);
                } else {
                    let index = pick % live.len();
                    let (allocation, layout, tag) = live.swap_remove(index);
                    assert!((0..layout.size()).all(|i| *allocation.add(i) == tag));
                    allocator.dealloc(allocation, layout);
                }
                if step % 50 == 0 {
                    check_heap(&allocator, region_base, region_len, &live);
                }
            }
            check_heap(&allocator, region_base, region_len, &live);
            assert!(in_place > 100, "in-place path barely exercised: {in_place}");
            for (allocation, layout, tag) in live.drain(..) {
                assert!((0..layout.size()).all(|i| *allocation.add(i) == tag));
                allocator.dealloc(allocation, layout);
            }
            let state = &*allocator.state.get();
            assert!(!state.free_head.is_null());
            assert!((*state.free_head).next.is_null(), "fully coalesced");
            assert_eq!((*state.free_head).size, region_len);
        }
    }

    #[test]
    fn realloc_handles_split_threshold_slack_and_overaligned_move() {
        let mut heap = TestHeap([0; 32 * 1024]);
        let allocator = ReclaimingAllocator::new();
        // SAFETY: private live `heap`, single thread; layouts match each live allocation.
        unsafe {
            assert!((&mut *allocator.state.get())
                .add_region(heap.0.as_mut_ptr() as usize, heap.0.len()));
            // Over-aligned allocation keeps its address and alignment across resizes.
            let aligned = Layout::from_size_align(512, 4096).unwrap();
            let block = allocator.alloc(aligned);
            assert_eq!(block as usize & 4095, 0);
            ptr::write_bytes(block, 0x5a, 512);
            let grown = allocator.realloc(block, aligned, 2048);
            assert_eq!(grown, block);
            let aligned_big = Layout::from_size_align(2048, 4096).unwrap();
            // Shrink by less than the split threshold: nothing is returned, still valid.
            let shrunk = allocator.realloc(grown, aligned_big, 2048 - 64);
            assert_eq!(shrunk, block);
            assert!((0..512).all(|i| *shrunk.add(i) == 0x5a));
            // The pin cannot fit in the prefix left by the over-aligned
            // allocation. It occupies the trailing span and forces a move.
            let pin_layout = Layout::from_size_align(4096, FREE_ALIGN).unwrap();
            let pin = allocator.alloc(pin_layout);
            assert!(!pin.is_null() && pin.addr() > shrunk.addr());
            let cur = Layout::from_size_align(2048 - 64, 4096).unwrap();
            let moved = allocator.realloc(shrunk, cur, 8192);
            assert!(!moved.is_null() && moved as usize & 4095 == 0);
            assert_ne!(moved, shrunk);
            assert!((0..512).all(|i| *moved.add(i) == 0x5a));
            let moved_layout = Layout::from_size_align(8192, 4096).unwrap();
            allocator.dealloc(moved, moved_layout);
            allocator.dealloc(pin, pin_layout);
            let state = &*allocator.state.get();
            assert!((*state.free_head).next.is_null(), "fully coalesced");
        }
    }

    #[test]
    fn allocator_honors_large_alignment() {
        let mut heap = TestHeap([0; 32 * 1024]);
        let allocator = ReclaimingAllocator::new();
        // SAFETY: private live `heap`, single thread; the allocation is freed with the
        // layout it was made with, and `heap` outlives the allocator's use of it.
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
        // SAFETY: `backing_layout` has non-zero size and valid alignment; the null result
        // is asserted right below and the block is freed with this same layout.
        let backing = unsafe { System.alloc(backing_layout) };
        assert!(!backing.is_null());
        // SAFETY: single-threaded test; the grow closure returns `backing`, a live
        // GROW_CHUNK_BYTES block, so `add_region` only touches memory this test owns.
        let allocation = unsafe {
            allocator.allocate_with_growth(layout, |chunk| {
                // ORDERING: test-only check that the growth callback runs outside the allocator
                // lock; Acquire pairs with the Release in `unlock`.
                assert_eq!(allocator.locked.load(Ordering::Acquire), UNLOCKED);
                assert!(chunk >= layout.size() + HEADER_BYTES);
                Some(backing as usize)
            })
        };
        assert!(!allocation.is_null());
        // SAFETY: the allocation came from `allocator` with this `layout` and is freed
        // once; `backing` is freed once with the layout it was allocated with, after
        // the allocator stops using it.
        unsafe {
            allocator.dealloc(allocation, layout);
            System.dealloc(backing, backing_layout);
        }
    }

    #[test]
    fn contended_allocations_sleep_instead_of_spinning_and_stay_disjoint() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        let mut heap = std::vec![0_u8; 1024 * 1024];
        let allocator = ReclaimingAllocator::new();
        // SAFETY: no other thread exists yet, so the state is unshared, and
        // `heap` outlives the allocator's use of the region.
        unsafe {
            assert!(
                (&mut *allocator.state.get()).add_region(heap.as_mut_ptr() as usize, heap.len())
            );
        }
        let failures = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let layout = Layout::from_size_align(256, 16).unwrap();
                    for round in 0..4000_usize {
                        // SAFETY: `layout` has non-zero size; the allocator
                        // serialises its own state.
                        let block = unsafe { allocator.alloc(layout) };
                        if block.is_null() {
                            failures.fetch_add(1, AtomicOrdering::Relaxed);
                            continue;
                        }
                        let stamp = (round & 0xff) as u8;
                        // SAFETY: `block` is a live 256-byte allocation owned
                        // by this thread until `dealloc` with the same layout.
                        unsafe {
                            core::ptr::write_bytes(block, stamp, layout.size());
                            if (0..layout.size()).any(|i| *block.add(i) != stamp) {
                                failures.fetch_add(1, AtomicOrdering::Relaxed);
                            }
                            allocator.dealloc(block, layout);
                        }
                    }
                });
            }
        });
        assert_eq!(failures.load(AtomicOrdering::Relaxed), 0);
        // ORDERING: test-only check after all threads joined; Acquire pairs with the
        // Release in `unlock`, so the lock is observed released.
        assert_eq!(allocator.locked.load(Ordering::Acquire), UNLOCKED);
    }

    #[test]
    fn duplicate_release_is_rejected_without_free_list_overlap() {
        let mut heap = TestHeap([0; 32 * 1024]);
        let allocator = ReclaimingAllocator::new();
        // SAFETY: private live `heap`; `lock`/`unlock` are paired on this thread, the
        // double `deallocate` is the validated rejection under test, and the later
        // allocations are freed with their own layouts.
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
        // SAFETY: private live `heap`; the second `initialize` is rejected by the latch
        // and installs nothing, and both allocations are freed with their own layouts.
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
