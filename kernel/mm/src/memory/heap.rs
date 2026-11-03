//! Bounded kernel heap and allocation-lifecycle diagnostics.
//!
//! - **Owner:** `kernel-mm` owns backing frames, buddy state, and allocation
//!   provenance; callers own only returned live spans.
//! - **Boundary:** Layout, pointer, capacity, and physical-frame ranges are
//!   checked before tracker or allocator mutation.
//! - **Lifecycle:** Allocate backing, publish one tracked live span, validate
//!   exact layout on free, quarantine its address, then return it to the buddy.
//! - **Concurrency:** Allocation runs with interrupts disabled. Small blocks
//!   (at most [`MAX_CLASS_BYTES`]) are served from per-CPU size-class
//!   magazines; the buddy lock is taken once per refill or drain *batch*, never
//!   per allocation. The lock order is CPU cache slot -> buddy, one slot at a
//!   time, and a busy slot is bypassed rather than waited on. Provenance lives
//!   in [`TRACKER_SHARDS`] independent tracker shards chosen by pointer hash;
//!   a tracker shard is a leaf: it is never held across the buddy, a cache
//!   slot, or another shard, and the normal path performs no quarantine scan.
//! - **Failure:** Exhaustion, tracker saturation, invalid free, double free, and
//!   layout mismatch fail without freeing an unowned span.
//! - **Forbidden:** No unbounded scan on successful allocation, allocator
//!   recursion, free-before-validation, fabricated span, or policy work. A
//!   cached block is never tracked live and never handed out twice; caches
//!   are drained before the buddy reports exhaustion.
//! - **Evidence:** `kernel-resource-lifecycle` and `physical-frame-lifecycle`.

#[cfg(not(test))]
use buddy_system_allocator::LockedHeap;
#[cfg(not(test))]
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::ptr;
use nucleus_core::util::lockdep::MAX_TRACKED_CPUS;
use spin::Mutex;
#[cfg(not(test))]
use spin::Once;
#[cfg(not(test))]
use x86_64::PhysAddr;
#[cfg(not(test))]
use x86_64::instructions::interrupts;
#[cfg(all(not(test), rustos_debug_print_enabled))]
use x86_64::instructions::port::Port;

#[cfg(not(test))]
use crate::memory::{kernel_vm, phys};

#[cfg(not(test))]
const HEAP_ORDER: usize = 32;
#[cfg(not(test))]
const PAGE_SIZE: usize = 4096;
#[cfg(not(test))]
const MIN_HEAP_SIZE: usize = 64 * 1024 * 1024;
#[cfg(not(test))]
const MAX_HEAP_SIZE: usize = 512 * 1024 * 1024;
/// Independent tracker shards; a pointer always hashes to the same shard.
const TRACKER_SHARD_BITS: u32 = 4;
const TRACKER_SHARDS: usize = 1 << TRACKER_SHARD_BITS;
/// Live-allocation slots per shard (65,536 across the heap in production).
#[cfg(test)]
const ACTIVE_ALLOCATION_SLOTS: usize = 128;
#[cfg(not(test))]
const ACTIVE_ALLOCATION_SLOTS: usize = 65_536 / TRACKER_SHARDS;
/// Recently-freed quarantine entries per shard (8,192 across the heap).
#[cfg(test)]
const FREED_QUARANTINE_SLOTS: usize = 64;
#[cfg(not(test))]
const FREED_QUARANTINE_SLOTS: usize = 8_192 / TRACKER_SHARDS;

/// Smallest and largest block served from a CPU cache. Larger requests, and
/// any request whose alignment exceeds its class, go straight to the buddy.
const MIN_CLASS_SHIFT: u32 = 4;
const MAX_CLASS_SHIFT: u32 = 11;
const MIN_CLASS_BYTES: usize = 1 << MIN_CLASS_SHIFT;
const MAX_CLASS_BYTES: usize = 1 << MAX_CLASS_SHIFT;
const SIZE_CLASSES: usize = (MAX_CLASS_SHIFT - MIN_CLASS_SHIFT + 1) as usize;
/// Bytes one CPU may park per size class, and the hard magazine capacity.
/// Linux slab per-CPU partial lists and tcmalloc/jemalloc thread caches bound
/// idle memory the same way: by bytes, so large classes hold few blocks.
const CLASS_CACHE_BYTES: usize = 16 * 1024;
const MAGAZINE_CAPACITY: usize = 64;
const MIN_MAGAZINE_CAPACITY: usize = 8;
const HEAP_CACHE_SLOTS: usize = MAX_TRACKED_CPUS;
#[cfg(all(not(test), rustos_debug_print_enabled))]
const DEBUGCON_PORT: u16 = 0x00e9;

#[cfg(not(test))]
static HEAP_INIT: Once<()> = Once::new();

#[cfg(not(test))]
static HEAP: LockedHeap<HEAP_ORDER> = LockedHeap::<HEAP_ORDER>::new();
#[cfg(not(test))]
static HEAP_CORE: HeapCore<BuddyBackend> = HeapCore::new(BuddyBackend);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapViolationKind {
    DoubleFree,
    InvalidFree,
    LayoutMismatch,
}

#[derive(Clone, Copy)]
struct ActiveAllocEntry {
    ptr: usize,
    size: usize,
    align: usize,
    state: ActiveAllocState,
}

impl ActiveAllocEntry {
    const EMPTY: Self = Self {
        ptr: 0,
        size: 0,
        align: 0,
        state: ActiveAllocState::Empty,
    };
}

#[derive(Clone, Copy)]
struct FreedAllocEntry {
    ptr: usize,
    size: usize,
    align: usize,
}

impl FreedAllocEntry {
    const EMPTY: Self = Self {
        ptr: 0,
        size: 0,
        align: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum ActiveAllocState {
    Empty = 0,
    Occupied = 1,
}

/// `log2(ACTIVE_ALLOCATION_SLOTS)`; each shard's table size is a power of two.
const ACTIVE_ALLOCATION_INDEX_BITS: u32 = ACTIVE_ALLOCATION_SLOTS.trailing_zeros();
const _: () = assert!(ACTIVE_ALLOCATION_SLOTS.is_power_of_two());

struct AllocationTracker {
    active: [ActiveAllocEntry; ACTIVE_ALLOCATION_SLOTS],
    freed: [FreedAllocEntry; FREED_QUARANTINE_SLOTS],
    freed_cursor: usize,
    active_count: usize,
    peak_active_count: usize,
    double_free_count: usize,
    invalid_free_count: usize,
    layout_mismatch_count: usize,
    tracker_overflow_count: usize,
}

impl AllocationTracker {
    const fn new() -> Self {
        Self {
            active: [ActiveAllocEntry::EMPTY; ACTIVE_ALLOCATION_SLOTS],
            freed: [FreedAllocEntry::EMPTY; FREED_QUARANTINE_SLOTS],
            freed_cursor: 0,
            active_count: 0,
            peak_active_count: 0,
            double_free_count: 0,
            invalid_free_count: 0,
            layout_mismatch_count: 0,
            tracker_overflow_count: 0,
        }
    }

    fn record_alloc(&mut self, ptr: usize, layout: Layout) -> bool {
        self.insert_active(ptr, layout.size(), layout.align())
    }

    fn begin_dealloc(&mut self, ptr: usize, layout: Layout) -> Result<(), HeapViolationKind> {
        let Some(slot) = self.find_active_slot(ptr) else {
            if self.is_recently_freed(ptr) {
                self.double_free_count = self.double_free_count.saturating_add(1);
                return Err(HeapViolationKind::DoubleFree);
            }

            self.invalid_free_count = self.invalid_free_count.saturating_add(1);
            return Err(HeapViolationKind::InvalidFree);
        };

        let entry = self.active[slot];
        if entry.size != layout.size() || entry.align != layout.align() {
            self.layout_mismatch_count = self.layout_mismatch_count.saturating_add(1);
            return Err(HeapViolationKind::LayoutMismatch);
        }

        self.active_count = self.active_count.saturating_sub(1);
        self.close_deletion_hole(slot);
        self.record_freed(ptr, entry.size, entry.align);
        Ok(())
    }

    fn insert_active(&mut self, ptr: usize, size: usize, align: usize) -> bool {
        let mut slot = home_slot(ptr);
        for _ in 0..ACTIVE_ALLOCATION_SLOTS {
            match self.active[slot].state {
                ActiveAllocState::Empty => {
                    self.insert_new_active(slot, ptr, size, align);
                    return true;
                }
                ActiveAllocState::Occupied if self.active[slot].ptr == ptr => {
                    self.active[slot].size = size;
                    self.active[slot].align = align;
                    return true;
                }
                ActiveAllocState::Occupied => {}
            }
            slot = (slot + 1) & (ACTIVE_ALLOCATION_SLOTS - 1);
        }
        self.tracker_overflow_count = self.tracker_overflow_count.saturating_add(1);
        false
    }

    /// Backward-shift deletion for linear probing. Removal leaves no
    /// tombstone, so probe chains stay as short as the live load allows.
    /// Tombstones were never reclaimed: over a long run they consumed every
    /// empty slot and turned each allocation into a full-table scan with
    /// interrupts disabled under the global tracker lock.
    fn close_deletion_hole(&mut self, mut hole: usize) {
        self.active[hole] = ActiveAllocEntry::EMPTY;
        let mut scan = (hole + 1) & (ACTIVE_ALLOCATION_SLOTS - 1);
        while self.active[scan].state == ActiveAllocState::Occupied {
            let home = home_slot(self.active[scan].ptr);
            let scan_distance = scan.wrapping_sub(home) & (ACTIVE_ALLOCATION_SLOTS - 1);
            let hole_distance = hole.wrapping_sub(home) & (ACTIVE_ALLOCATION_SLOTS - 1);
            if hole_distance < scan_distance {
                self.active[hole] = self.active[scan];
                self.active[scan] = ActiveAllocEntry::EMPTY;
                hole = scan;
            }
            scan = (scan + 1) & (ACTIVE_ALLOCATION_SLOTS - 1);
        }
    }

    fn insert_new_active(&mut self, slot: usize, ptr: usize, size: usize, align: usize) {
        self.active[slot] = ActiveAllocEntry {
            ptr,
            size,
            align,
            state: ActiveAllocState::Occupied,
        };
        self.active_count = self.active_count.saturating_add(1);
        self.peak_active_count = self.peak_active_count.max(self.active_count);
    }

    fn find_active_slot(&self, ptr: usize) -> Option<usize> {
        let mut slot = home_slot(ptr);
        for _ in 0..ACTIVE_ALLOCATION_SLOTS {
            match self.active[slot].state {
                ActiveAllocState::Empty => return None,
                ActiveAllocState::Occupied if self.active[slot].ptr == ptr => return Some(slot),
                ActiveAllocState::Occupied => {}
            }
            slot = (slot + 1) & (ACTIVE_ALLOCATION_SLOTS - 1);
        }
        None
    }

    fn is_recently_freed(&self, ptr: usize) -> bool {
        self.freed.iter().any(|entry| entry.ptr == ptr)
    }

    fn recent_freed_layout(&self, ptr: usize) -> Option<(usize, usize)> {
        self.freed
            .iter()
            .find(|entry| entry.ptr == ptr)
            .map(|entry| (entry.size, entry.align))
    }

    fn record_freed(&mut self, ptr: usize, size: usize, align: usize) {
        let slot = self.freed_cursor % FREED_QUARANTINE_SLOTS;
        self.freed[slot] = FreedAllocEntry { ptr, size, align };
        self.freed_cursor = self.freed_cursor.wrapping_add(1);
    }

    fn expected_layout(&self, ptr: usize) -> Option<(usize, usize)> {
        self.find_active_slot(ptr)
            .map(|slot| {
                let entry = self.active[slot];
                (entry.size, entry.align)
            })
            .or_else(|| self.recent_freed_layout(ptr))
    }

    fn occupancy(&self) -> TrackerOccupancy {
        let mut occupied = 0usize;
        let mut empty = 0usize;
        for entry in &self.active {
            match entry.state {
                ActiveAllocState::Occupied => occupied += 1,
                ActiveAllocState::Empty => empty += 1,
            }
        }
        TrackerOccupancy {
            active_count: self.active_count,
            peak_active_count: self.peak_active_count,
            occupied_slots: occupied,
            empty_slots: empty,
            overflow_count: self.tracker_overflow_count,
        }
    }
}

#[derive(Clone, Copy)]
// DIAGNOSTIC: Occupancy fields feed debug-only allocator telemetry.
#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
struct TrackerOccupancy {
    active_count: usize,
    peak_active_count: usize,
    occupied_slots: usize,
    empty_slots: usize,
    overflow_count: usize,
}

/// Fibonacci hashing of the 16-byte allocation granule. The product's top
/// [`TRACKER_SHARD_BITS`] bits pick the shard and the next
/// `ACTIVE_ALLOCATION_INDEX_BITS` bits are the home slot inside it, so shard
/// and slot are independent. The previous form masked the LOW bits of a product
/// taken over `ptr.rotate_right(17)`; those depend only on pointer bits
/// 17..33, so every allocation inside one 128 KiB window shared a single home
/// slot.
#[inline]
const fn pointer_hash(ptr: usize) -> u64 {
    ((ptr >> 4) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[inline]
const fn home_slot(ptr: usize) -> usize {
    ((pointer_hash(ptr) << TRACKER_SHARD_BITS) >> (64 - ACTIVE_ALLOCATION_INDEX_BITS)) as usize
}

#[inline]
const fn tracker_shard(ptr: usize) -> usize {
    (pointer_hash(ptr) >> (64 - TRACKER_SHARD_BITS)) as usize
}

pub struct KernelAllocator;

/// Where blocks ultimately come from. Production is the buddy allocator; the
/// host tests substitute a counting system-allocator backend so the cache tier
/// can be stress-tested across threads.
trait BlockBackend {
    /// One block, or null.
    fn alloc(&self, layout: Layout) -> *mut u8;
    /// Up to `out.len()` blocks under one backend critical section; returns
    /// the number written.
    fn alloc_batch(&self, layout: Layout, out: &mut [usize]) -> usize;
    /// # Safety
    /// `ptr` came from this backend with exactly `layout` and is not in use.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout);
    /// # Safety
    /// Every pointer came from this backend with exactly `layout` and is not
    /// in use.
    unsafe fn dealloc_batch(&self, layout: Layout, ptrs: &[usize]);
}

#[cfg(not(test))]
struct BuddyBackend;

#[cfg(not(test))]
impl BlockBackend for BuddyBackend {
    fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `LockedHeap::alloc` takes the buddy lock itself and accepts
        // any non-zero-size layout; callers never pass a zero-size layout.
        unsafe { HEAP.alloc(layout) }
    }

    fn alloc_batch(&self, layout: Layout, out: &mut [usize]) -> usize {
        let mut heap = HEAP.lock();
        let mut filled = 0;
        while filled < out.len() {
            match heap.alloc(layout) {
                Ok(block) => {
                    out[filled] = block.as_ptr() as usize;
                    filled += 1;
                }
                Err(()) => break,
            }
        }
        filled
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the trait contract: `ptr` is an unused block of `layout`
        // that this buddy handed out.
        unsafe { HEAP.dealloc(ptr, layout) };
    }

    unsafe fn dealloc_batch(&self, layout: Layout, ptrs: &[usize]) {
        let mut heap = HEAP.lock();
        for &block in ptrs {
            if let Some(block) = core::ptr::NonNull::new(block as *mut u8) {
                // SAFETY: the trait contract: each block is an unused
                // allocation of `layout` from this buddy.
                unsafe { heap.dealloc(block, layout) };
            }
        }
    }
}

/// A bounded LIFO of free blocks of one size class.
struct Magazine {
    len: usize,
    blocks: [usize; MAGAZINE_CAPACITY],
}

impl Magazine {
    const fn new() -> Self {
        Self {
            len: 0,
            blocks: [0; MAGAZINE_CAPACITY],
        }
    }

    fn push(&mut self, block: usize) {
        self.blocks[self.len] = block;
        self.len += 1;
    }

    fn pop(&mut self) -> Option<usize> {
        self.len = self.len.checked_sub(1)?;
        Some(self.blocks[self.len])
    }
}

struct CpuCache {
    magazines: [Magazine; SIZE_CLASSES],
}

impl CpuCache {
    const fn new() -> Self {
        Self {
            magazines: [const { Magazine::new() }; SIZE_CLASSES],
        }
    }
}

/// Size class (index and block size) a layout is served from, or `None` for
/// the buddy. A block of class size `c` is `c`-aligned, so any layout whose
/// size and alignment are at most `c` fits it.
#[inline]
fn size_class(layout: Layout) -> Option<(usize, usize)> {
    let need = layout.size().max(layout.align()).max(MIN_CLASS_BYTES);
    if need > MAX_CLASS_BYTES {
        return None;
    }
    let bytes = need.next_power_of_two();
    Some(((bytes.trailing_zeros() - MIN_CLASS_SHIFT) as usize, bytes))
}

/// Magazine capacity of a class: a byte budget, clamped.
#[inline]
const fn magazine_capacity(class_bytes: usize) -> usize {
    let by_bytes = CLASS_CACHE_BYTES / class_bytes;
    if by_bytes < MIN_MAGAZINE_CAPACITY {
        MIN_MAGAZINE_CAPACITY
    } else if by_bytes > MAGAZINE_CAPACITY {
        MAGAZINE_CAPACITY
    } else {
        by_bytes
    }
}

/// The heap proper: sharded provenance tracker plus per-CPU size-class caches
/// in front of a [`BlockBackend`].
struct HeapCore<B: BlockBackend> {
    backend: B,
    shards: [Mutex<AllocationTracker>; TRACKER_SHARDS],
    caches: [Mutex<CpuCache>; HEAP_CACHE_SLOTS],
}

impl<B: BlockBackend> HeapCore<B> {
    const fn new(backend: B) -> Self {
        Self {
            backend,
            shards: [const { Mutex::new(AllocationTracker::new()) }; TRACKER_SHARDS],
            caches: [const { Mutex::new(CpuCache::new()) }; HEAP_CACHE_SLOTS],
        }
    }

    fn block_layout(class_bytes: usize) -> Option<Layout> {
        Layout::from_size_align(class_bytes, class_bytes).ok()
    }

    /// Allocates and records one span; null on exhaustion or tracker
    /// saturation. `cpu` only chooses a cache slot, so any value is correct.
    fn alloc(&self, cpu: usize, layout: Layout) -> *mut u8 {
        let ptr = match size_class(layout) {
            Some((class, bytes)) => self.alloc_small(cpu, class, bytes),
            None => self.alloc_large(layout),
        };
        if ptr.is_null() {
            return ptr;
        }
        // The block is unreachable by any other CPU until it is returned, so
        // it can be recorded after every allocator lock is gone.
        let mut tracker = self.shards[tracker_shard(ptr as usize)].lock();
        if tracker.record_alloc(ptr as usize, layout) {
            return ptr;
        }
        let occupancy = tracker.occupancy();
        drop(tracker);
        heap_tracker_overflow_log(ptr as usize, layout, occupancy);
        self.release_block(cpu, ptr, layout);
        ptr::null_mut()
    }

    fn alloc_large(&self, layout: Layout) -> *mut u8 {
        let ptr = self.backend.alloc(layout);
        if !ptr.is_null() {
            return ptr;
        }
        // Idle cached blocks keep the buddy from coalescing; reclaim them
        // before reporting exhaustion.
        self.drain_all();
        self.backend.alloc(layout)
    }

    fn alloc_small(&self, cpu: usize, class: usize, bytes: usize) -> *mut u8 {
        let Some(block_layout) = Self::block_layout(bytes) else {
            return ptr::null_mut();
        };
        if let Some(mut cache) = self.caches[cpu % HEAP_CACHE_SLOTS].try_lock() {
            let magazine = &mut cache.magazines[class];
            if let Some(block) = magazine.pop() {
                return block as *mut u8;
            }
            // Refill: one buddy hold for a half magazine.
            let batch = magazine_capacity(bytes) / 2;
            let mut fresh = [0usize; MAGAZINE_CAPACITY];
            let got = self.backend.alloc_batch(block_layout, &mut fresh[..batch]);
            if got != 0 {
                for &block in &fresh[1..got] {
                    magazine.push(block);
                }
                return fresh[0] as *mut u8;
            }
        } else {
            // Slot busy (re-entry or a CPU index shared during early boot):
            // bypass it instead of waiting.
            let ptr = self.backend.alloc(block_layout);
            if !ptr.is_null() {
                return ptr;
            }
        }
        self.drain_all();
        self.backend.alloc(block_layout)
    }

    /// Validates `layout` against provenance, then returns the span.
    ///
    /// # Safety
    /// `ptr` must be a pointer this heap returned for `layout` or an invalid
    /// one; validation rejects the latter before any block moves.
    unsafe fn dealloc(
        &self,
        cpu: usize,
        ptr: *mut u8,
        layout: Layout,
    ) -> Result<(), HeapViolationKind> {
        let mut tracker = self.shards[tracker_shard(ptr as usize)].lock();
        match tracker.begin_dealloc(ptr as usize, layout) {
            Ok(()) => {
                drop(tracker);
                self.release_block(cpu, ptr, layout);
                Ok(())
            }
            Err(kind) => {
                let expected = tracker.expected_layout(ptr as usize);
                drop(tracker);
                heap_violation_log(kind, ptr as usize, layout, expected);
                Err(kind)
            }
        }
    }

    /// Returns an already-validated, untracked block to a cache or the
    /// backend.
    fn release_block(&self, cpu: usize, ptr: *mut u8, layout: Layout) {
        let Some((class, bytes)) = size_class(layout) else {
            // SAFETY: the span was validated (or never recorded) and is no
            // longer reachable; `layout` is the one it was allocated with.
            unsafe { self.backend.dealloc(ptr, layout) };
            return;
        };
        let Some(block_layout) = Self::block_layout(bytes) else {
            return;
        };
        let Some(mut cache) = self.caches[cpu % HEAP_CACHE_SLOTS].try_lock() else {
            // SAFETY: as above; the block was allocated with `block_layout`.
            unsafe { self.backend.dealloc(ptr, block_layout) };
            return;
        };
        let magazine = &mut cache.magazines[class];
        let capacity = magazine_capacity(bytes);
        if magazine.len == capacity {
            // Spill a half magazine in one buddy hold.
            let keep = capacity / 2;
            // SAFETY: the spilled blocks are free, were allocated with
            // `block_layout`, and leave the magazine before the call returns.
            unsafe {
                self.backend
                    .dealloc_batch(block_layout, &magazine.blocks[keep..capacity]);
            }
            magazine.len = keep;
        }
        magazine.push(ptr as usize);
    }

    /// Returns every cached block to the backend, one slot at a time.
    fn drain_all(&self) {
        for slot in &self.caches {
            let mut cache = slot.lock();
            for (class, magazine) in cache.magazines.iter_mut().enumerate() {
                if magazine.len == 0 {
                    continue;
                }
                let Some(block_layout) = Self::block_layout(MIN_CLASS_BYTES << class) else {
                    continue;
                };
                // SAFETY: cached blocks are free and were allocated with this
                // class layout; the magazine is emptied right after.
                unsafe {
                    self.backend
                        .dealloc_batch(block_layout, &magazine.blocks[..magazine.len]);
                }
                magazine.len = 0;
            }
        }
    }
}

#[cfg(not(test))]
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        interrupts::without_interrupts(|| {
            HEAP_CORE.alloc(nucleus_core::util::lockdep::current_cpu_index(), layout)
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged under `GlobalAlloc::dealloc`'s contract.
        let _ = unsafe { try_dealloc(ptr, layout) };
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged under `GlobalAlloc::alloc_zeroed`'s contract.
        let ptr = unsafe { self.alloc(layout) };
        if !ptr.is_null() {
            // SAFETY: `ptr` is a fresh allocation of `layout.size()` bytes.
            unsafe { ptr.write_bytes(0, layout.size()) };
        }
        ptr
    }
}

#[cfg(not(test))]
pub(crate) unsafe fn try_dealloc(ptr: *mut u8, layout: Layout) -> Result<(), HeapViolationKind> {
    if ptr.is_null() {
        return Ok(());
    }

    interrupts::without_interrupts(|| {
        // SAFETY: the caller's contract; validation precedes any block move.
        unsafe {
            HEAP_CORE.dealloc(
                nucleus_core::util::lockdep::current_cpu_index(),
                ptr,
                layout,
            )
        }
    })
}

#[cfg(all(not(test), rustos_debug_print_enabled))]
fn heap_violation_log(
    kind: HeapViolationKind,
    ptr: usize,
    layout: Layout,
    expected: Option<(usize, usize)>,
) {
    interrupts::without_interrupts(|| {
        let mut port = Port::new(DEBUGCON_PORT);
        write_literal(&mut port, b"heap violation: ");
        match kind {
            HeapViolationKind::DoubleFree => write_literal(&mut port, b"double-free"),
            HeapViolationKind::InvalidFree => write_literal(&mut port, b"invalid-free"),
            HeapViolationKind::LayoutMismatch => write_literal(&mut port, b"layout-mismatch"),
        }
        write_literal(&mut port, b" ptr=0x");
        write_hex(&mut port, ptr as u64);
        write_literal(&mut port, b" size=0x");
        write_hex(&mut port, layout.size() as u64);
        write_literal(&mut port, b" align=0x");
        write_hex(&mut port, layout.align() as u64);
        if let Some((expected_size, expected_align)) = expected {
            write_literal(&mut port, b" expected_size=0x");
            write_hex(&mut port, expected_size as u64);
            write_literal(&mut port, b" expected_align=0x");
            write_hex(&mut port, expected_align as u64);
        }
        write_literal(&mut port, b"\r\n");
    });
}

#[cfg(any(test, not(rustos_debug_print_enabled)))]
fn heap_violation_log(
    _kind: HeapViolationKind,
    _ptr: usize,
    _layout: Layout,
    _expected: Option<(usize, usize)>,
) {
}

#[cfg(all(not(test), rustos_debug_print_enabled))]
fn heap_tracker_overflow_log(ptr: usize, layout: Layout, occupancy: TrackerOccupancy) {
    let overflow_count = occupancy.overflow_count as u64;
    if overflow_count > 8 && !overflow_count.is_power_of_two() {
        return;
    }
    interrupts::without_interrupts(|| {
        let mut port = Port::new(DEBUGCON_PORT);
        write_literal(&mut port, b"heap violation: tracker-overflow");
        write_literal(&mut port, b" ptr=0x");
        write_hex(&mut port, ptr as u64);
        write_literal(&mut port, b" size=0x");
        write_hex(&mut port, layout.size() as u64);
        write_literal(&mut port, b" align=0x");
        write_hex(&mut port, layout.align() as u64);
        write_literal(&mut port, b" overflow_count=");
        write_hex(&mut port, overflow_count);
        write_literal(&mut port, b" active=");
        write_hex(&mut port, occupancy.active_count as u64);
        write_literal(&mut port, b" peak=");
        write_hex(&mut port, occupancy.peak_active_count as u64);
        write_literal(&mut port, b" occupied=");
        write_hex(&mut port, occupancy.occupied_slots as u64);
        write_literal(&mut port, b" empty=");
        write_hex(&mut port, occupancy.empty_slots as u64);
        write_literal(&mut port, b"\r\n");
    });
}

#[cfg(any(test, not(rustos_debug_print_enabled)))]
fn heap_tracker_overflow_log(_ptr: usize, _layout: Layout, _occupancy: TrackerOccupancy) {}

#[cfg(all(not(test), rustos_debug_print_enabled))]
fn write_literal(port: &mut Port<u8>, bytes: &[u8]) {
    for &byte in bytes {
        // SAFETY: port 0xe9 is the debug console; a byte write has no memory effect.
        unsafe { port.write(byte) };
    }
}

#[cfg(all(not(test), rustos_debug_print_enabled))]
fn write_hex(port: &mut Port<u8>, value: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut started = false;
    for shift in (0..16).rev() {
        let nibble = ((value >> (shift * 4)) & 0xf) as usize;
        if nibble != 0 || started || shift == 0 {
            started = true;
            // SAFETY: port 0xe9 is the debug console; a byte write has no memory effect.
            unsafe { port.write(HEX[nibble]) };
        }
    }
}

#[cfg(not(test))]
pub fn init_heap() {
    // SAFETY: `call_once` runs this once; the chunks come from the physical
    // allocator, are direct-mapped, and are donated to the buddy exactly once.
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

pub fn handle_alloc_error(layout: Layout) -> ! {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_rejects_double_free_without_touching_allocator() {
        let mut tracker = AllocationTracker::new();
        let layout = Layout::from_size_align(128, 16).unwrap();
        assert!(tracker.record_alloc(0x1000, layout));
        assert_eq!(tracker.begin_dealloc(0x1000, layout), Ok(()));
        assert_eq!(
            tracker.begin_dealloc(0x1000, layout),
            Err(HeapViolationKind::DoubleFree)
        );
    }

    #[test]
    fn tracker_rejects_layout_mismatch() {
        let mut tracker = AllocationTracker::new();
        let layout = Layout::from_size_align(256, 32).unwrap();
        let wrong = Layout::from_size_align(128, 32).unwrap();
        assert!(tracker.record_alloc(0x2000, layout));
        assert_eq!(
            tracker.begin_dealloc(0x2000, wrong),
            Err(HeapViolationKind::LayoutMismatch)
        );
        assert!(tracker.find_active_slot(0x2000).is_some());
    }

    #[test]
    fn tracker_reuse_keeps_quarantine_cold_and_live_state_authoritative() {
        let mut tracker = AllocationTracker::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        assert!(tracker.record_alloc(0x3000, layout));
        let occupancy = tracker.occupancy();
        assert_eq!(occupancy.active_count, 1);
        assert_eq!(occupancy.peak_active_count, 1);
        assert_eq!(occupancy.occupied_slots, 1);
        assert_eq!(occupancy.empty_slots, ACTIVE_ALLOCATION_SLOTS - 1);
        assert_eq!(occupancy.overflow_count, 0);
        assert_eq!(tracker.begin_dealloc(0x3000, layout), Ok(()));
        assert!(tracker.is_recently_freed(0x3000));
        assert_eq!(tracker.recent_freed_layout(0x3000), Some((64, 8)));
        assert_eq!(tracker.expected_layout(0x3000), Some((64, 8)));
        assert!(tracker.record_alloc(0x3000, layout));
        // The quarantine ring is checked only after active lookup fails. It
        // need not be scanned and cleared on the successful allocation path.
        assert!(tracker.is_recently_freed(0x3000));
        assert!(tracker.find_active_slot(0x3000).is_some());
        assert_eq!(tracker.begin_dealloc(0x3000, layout), Ok(()));
    }

    #[test]
    fn tracker_churn_returns_every_slot_to_empty() {
        let mut tracker = AllocationTracker::new();
        let layout = Layout::from_size_align(32, 8).unwrap();
        for round in 0..4 {
            for index in 0..ACTIVE_ALLOCATION_SLOTS {
                let ptr = 0x10_0000 + round * 0x10_0000 + index * 0x40;
                assert!(tracker.record_alloc(ptr, layout));
            }
            for index in 0..ACTIVE_ALLOCATION_SLOTS {
                let ptr = 0x10_0000 + round * 0x10_0000 + index * 0x40;
                assert_eq!(tracker.begin_dealloc(ptr, layout), Ok(()));
            }
            // Deletion leaves no residue: a later lookup or insert terminates
            // at its first empty slot instead of scanning the whole table.
            assert_eq!(tracker.occupancy().empty_slots, ACTIVE_ALLOCATION_SLOTS);
        }
        assert!(tracker.record_alloc(0x20_0000, layout));
    }

    #[test]
    fn tracker_spreads_pointers_from_one_small_window() {
        // Half the table filled from one 8 KiB window. The previous hash gave
        // every pointer below 128 KiB the same home slot, so the last insert
        // probed past every earlier one.
        let mut tracker = AllocationTracker::new();
        let layout = Layout::from_size_align(16, 16).unwrap();
        let count = ACTIVE_ALLOCATION_SLOTS / 2;
        let mut max_displacement = 0;
        for index in 0..count {
            let ptr = 0x40_0000 + index * 0x40;
            assert!(tracker.record_alloc(ptr, layout));
            let slot = tracker.find_active_slot(ptr).unwrap();
            let displacement = slot.wrapping_sub(home_slot(ptr)) & (ACTIVE_ALLOCATION_SLOTS - 1);
            max_displacement = max_displacement.max(displacement);
        }
        assert!(
            max_displacement < count / 4,
            "max displacement {max_displacement}"
        );
        // Removing from the middle of a cluster keeps every survivor reachable.
        for index in (0..count).step_by(3) {
            assert_eq!(
                tracker.begin_dealloc(0x40_0000 + index * 0x40, layout),
                Ok(())
            );
        }
        for index in 0..count {
            let live = tracker.find_active_slot(0x40_0000 + index * 0x40).is_some();
            assert_eq!(live, index % 3 != 0);
        }
    }

    extern crate std;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// System-allocator backend that counts critical sections (one per
    /// `alloc`, `alloc_batch`, `dealloc`, `dealloc_batch`) and enforces a byte
    /// limit, standing in for the buddy lock.
    struct TestBackend {
        sections: AtomicUsize,
        outstanding: AtomicUsize,
        limit: usize,
    }

    impl TestBackend {
        fn new(limit: usize) -> Self {
            Self {
                sections: AtomicUsize::new(0),
                outstanding: AtomicUsize::new(0),
                limit,
            }
        }

        fn take(&self, layout: Layout) -> *mut u8 {
            let mut current = self.outstanding.load(Ordering::Relaxed);
            loop {
                if current + layout.size() > self.limit {
                    return ptr::null_mut();
                }
                match self.outstanding.compare_exchange(
                    current,
                    current + layout.size(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(seen) => current = seen,
                }
            }
            // SAFETY: the layout has non-zero size.
            unsafe { std::alloc::alloc(layout) }
        }
    }

    impl BlockBackend for TestBackend {
        fn alloc(&self, layout: Layout) -> *mut u8 {
            self.sections.fetch_add(1, Ordering::Relaxed);
            self.take(layout)
        }

        fn alloc_batch(&self, layout: Layout, out: &mut [usize]) -> usize {
            self.sections.fetch_add(1, Ordering::Relaxed);
            let mut filled = 0;
            while filled < out.len() {
                let block = self.take(layout);
                if block.is_null() {
                    break;
                }
                out[filled] = block as usize;
                filled += 1;
            }
            filled
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            self.sections.fetch_add(1, Ordering::Relaxed);
            self.outstanding.fetch_sub(layout.size(), Ordering::Relaxed);
            // SAFETY: trait contract.
            unsafe { std::alloc::dealloc(ptr, layout) };
        }

        unsafe fn dealloc_batch(&self, layout: Layout, ptrs: &[usize]) {
            self.sections.fetch_add(1, Ordering::Relaxed);
            for &block in ptrs {
                self.outstanding.fetch_sub(layout.size(), Ordering::Relaxed);
                // SAFETY: trait contract.
                unsafe { std::alloc::dealloc(block as *mut u8, layout) };
            }
        }
    }

    fn core_with_limit(limit: usize) -> &'static HeapCore<TestBackend> {
        std::boxed::Box::leak(std::boxed::Box::new(HeapCore::new(TestBackend::new(limit))))
    }

    fn sections(core: &HeapCore<TestBackend>) -> usize {
        core.backend.sections.load(Ordering::Relaxed)
    }

    fn live_total(core: &HeapCore<TestBackend>) -> usize {
        core.shards
            .iter()
            .map(|shard| shard.lock().active_count)
            .sum()
    }

    fn free(core: &HeapCore<TestBackend>, cpu: usize, ptr: *mut u8, layout: Layout) {
        // SAFETY: tests pass pointers this core returned, or exercise rejection.
        assert_eq!(unsafe { core.dealloc(cpu, ptr, layout) }, Ok(()));
    }

    #[test]
    fn size_classes_cover_size_and_alignment() {
        let class = |size, align| size_class(Layout::from_size_align(size, align).unwrap());
        assert_eq!(class(1, 1), Some((0, 16)));
        assert_eq!(class(16, 16), Some((0, 16)));
        assert_eq!(class(17, 1), Some((1, 32)));
        assert_eq!(class(24, 64), Some((2, 64)));
        assert_eq!(class(2048, 8), Some((SIZE_CLASSES - 1, 2048)));
        assert_eq!(class(2049, 8), None);
        assert_eq!(class(8, 4096), None);
        assert_eq!(magazine_capacity(16), MAGAZINE_CAPACITY);
        assert_eq!(magazine_capacity(2048), MIN_MAGAZINE_CAPACITY);
    }

    #[test]
    fn tracker_shards_are_independent_of_home_slots_and_spread() {
        let mut per_shard = [0usize; TRACKER_SHARDS];
        for index in 0..8192usize {
            per_shard[tracker_shard(0x1000_0000 + index * 0x40)] += 1;
        }
        let expected = 8192 / TRACKER_SHARDS;
        assert!(
            per_shard
                .iter()
                .all(|&n| n > expected / 2 && n < expected * 3 / 2),
            "{per_shard:?}"
        );
    }

    #[test]
    fn small_traffic_takes_the_backend_once_per_batch_not_per_call() {
        let core = core_with_limit(usize::MAX);
        let layout = Layout::from_size_align(48, 8).unwrap();
        let mut held = std::vec::Vec::new();
        for _ in 0..200 {
            let ptr = core.alloc(0, layout);
            assert!(!ptr.is_null());
            held.push(ptr);
        }
        for ptr in held.drain(..) {
            free(core, 0, ptr, layout);
        }
        for _ in 0..200 {
            let ptr = core.alloc(0, layout);
            assert!(!ptr.is_null());
            held.push(ptr);
        }
        // 400 operations; a cache-less heap needs 400 backend critical
        // sections, the magazine needs refills plus a few spills.
        assert!(sections(core) < 40, "sections={}", sections(core));
        for ptr in held.drain(..) {
            free(core, 0, ptr, layout);
        }
        assert_eq!(live_total(core), 0);
        core.drain_all();
        assert_eq!(core.backend.outstanding.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn violations_are_rejected_before_any_block_moves() {
        let core = core_with_limit(usize::MAX);
        let layout = Layout::from_size_align(100, 8).unwrap();
        let wrong = Layout::from_size_align(60, 8).unwrap();
        let ptr = core.alloc(2, layout);
        assert!(!ptr.is_null());
        // SAFETY: deliberately invalid frees; validation must reject them.
        unsafe {
            assert_eq!(
                core.dealloc(2, ptr, wrong),
                Err(HeapViolationKind::LayoutMismatch)
            );
            assert_eq!(
                core.dealloc(2, 0x1230 as *mut u8, layout),
                Err(HeapViolationKind::InvalidFree)
            );
        }
        free(core, 2, ptr, layout);
        // SAFETY: as above; the span is quarantined, not reusable twice.
        assert_eq!(
            unsafe { core.dealloc(2, ptr, layout) },
            Err(HeapViolationKind::DoubleFree)
        );
        // The rejected frees never put a block in a magazine twice.
        let first = core.alloc(2, layout);
        let second = core.alloc(2, layout);
        assert_ne!(first, second);
        free(core, 2, first, layout);
        free(core, 2, second, layout);
    }

    #[test]
    fn exhaustion_drains_idle_caches_before_failing() {
        let core = core_with_limit(20 * 1024);
        let small = Layout::from_size_align(500, 8).unwrap();
        let mut held = std::vec::Vec::new();
        for _ in 0..30 {
            held.push(core.alloc(0, small));
        }
        for ptr in held.drain(..) {
            free(core, 0, ptr, small);
        }
        // About 16 KiB of 512-byte blocks idle in magazines; the 8 KiB request
        // fits only once they are returned to the backend.
        let big = Layout::from_size_align(8 * 1024, 8).unwrap();
        let ptr = core.alloc(1, big);
        assert!(!ptr.is_null(), "idle cached blocks must be reclaimed");
        free(core, 1, ptr, big);
        // True exhaustion still fails.
        let huge = Layout::from_size_align(64 * 1024, 8).unwrap();
        assert!(core.alloc(1, huge).is_null());
    }

    #[test]
    fn tracker_saturation_fails_the_allocation_without_leaking_the_block() {
        let core = core_with_limit(usize::MAX);
        let layout = Layout::from_size_align(16, 16).unwrap();
        let mut held = std::vec::Vec::new();
        let mut failed = 0;
        // 16 shards x 128 slots: far more than 2048 live spans must overflow
        // some shard.
        for _ in 0..6000 {
            let ptr = core.alloc(0, layout);
            if ptr.is_null() {
                failed += 1;
            } else {
                held.push(ptr);
            }
        }
        assert!(failed > 0);
        assert!(held.len() <= TRACKER_SHARDS * ACTIVE_ALLOCATION_SLOTS);
        for ptr in held.drain(..) {
            free(core, 0, ptr, layout);
        }
        core.drain_all();
        assert_eq!(live_total(core), 0);
        assert_eq!(
            core.backend.outstanding.load(Ordering::Relaxed),
            0,
            "an allocation refused by the tracker must return its block"
        );
    }

    /// Threads allocate mixed sizes and alignments, stamp each span with a
    /// pattern, and free spans other threads allocated, across shared and
    /// private cache slots. Overlap would corrupt a stamp; a lost or doubled
    /// block would break the closing accounting.
    #[test]
    fn threaded_stress_keeps_spans_disjoint_and_accounting_exact() {
        use std::sync::Mutex as StdMutex;
        const THREADS: usize = 8;
        const ROUNDS: usize = 3000;
        let core = core_with_limit(usize::MAX);
        let exchange: StdMutex<std::vec::Vec<(usize, Layout)>> =
            StdMutex::new(std::vec::Vec::new());
        let exchange = &exchange;
        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                scope.spawn(move || {
                    let cpu = thread % 5;
                    let mut seed = 0x2545_F491_4F6C_DD1D_u64 ^ ((thread as u64 + 1) << 17);
                    let mut next = move || {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed
                    };
                    let mut own: std::vec::Vec<(usize, Layout)> = std::vec::Vec::new();
                    for _ in 0..ROUNDS {
                        let r = next();
                        if r % 8 < 5 {
                            let size = match r % 13 {
                                0 => 2049 + (r >> 8) as usize % 6000,
                                1..=3 => 200 + (r >> 8) as usize % 1800,
                                _ => 1 + (r >> 8) as usize % 200,
                            };
                            let align = 1usize << ((r >> 32) % 7);
                            let layout = Layout::from_size_align(size, align).unwrap();
                            let ptr = core.alloc(cpu, layout);
                            if ptr.is_null() {
                                // Test-sized tracker shards saturate; the
                                // refused block must not leak (checked below).
                                continue;
                            }
                            assert_eq!(ptr as usize % align, 0);
                            let stamp = (ptr as usize >> 4) as u8;
                            // SAFETY: a fresh span of `size` bytes owned by this thread.
                            unsafe { ptr.write_bytes(stamp, size) };
                            own.push((ptr as usize, layout));
                        } else if r % 8 < 7 && !own.is_empty() {
                            let index = (r >> 8) as usize % own.len();
                            let (ptr, layout) = own.swap_remove(index);
                            check_and_free(core, cpu, ptr, layout);
                        } else if r % 2 == 0 {
                            exchange
                                .lock()
                                .unwrap()
                                .extend(own.drain(..own.len().min(4)));
                        } else {
                            let taken = exchange.lock().unwrap().pop();
                            if let Some((ptr, layout)) = taken {
                                check_and_free(core, cpu, ptr, layout);
                            }
                        }
                    }
                    for (ptr, layout) in own {
                        check_and_free(core, cpu, ptr, layout);
                    }
                });
            }
        });
        for (ptr, layout) in exchange.lock().unwrap().drain(..) {
            check_and_free(core, 0, ptr, layout);
        }
        assert_eq!(live_total(core), 0);
        core.drain_all();
        assert_eq!(core.backend.outstanding.load(Ordering::Relaxed), 0);

        fn check_and_free(core: &HeapCore<TestBackend>, cpu: usize, ptr: usize, layout: Layout) {
            let stamp = (ptr >> 4) as u8;
            // SAFETY: the span is live and `layout.size()` bytes long.
            let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, layout.size()) };
            assert!(
                bytes.iter().all(|&b| b == stamp),
                "span at {ptr:#x} was overwritten"
            );
            free(core, cpu, ptr as *mut u8, layout);
        }
    }
}
