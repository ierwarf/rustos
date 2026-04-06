#[cfg(not(test))]
use buddy_system_allocator::LockedHeap;
#[cfg(not(test))]
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
#[cfg(not(test))]
use core::ptr;
use spin::{Mutex, Once};
#[cfg(not(test))]
use x86_64::PhysAddr;
use x86_64::instructions::interrupts;
#[cfg(all(not(test), rustos_debug_print_enabled))]
use x86_64::instructions::port::Port;

#[cfg(not(test))]
use crate::memory::{kernel_vm, phys};

const HEAP_ORDER: usize = 32;
const PAGE_SIZE: usize = 4096;
const MIN_HEAP_SIZE: usize = 64 * 1024 * 1024;
const MAX_HEAP_SIZE: usize = 512 * 1024 * 1024;
#[cfg(test)]
const ACTIVE_ALLOCATION_SLOTS: usize = 128;
#[cfg(not(test))]
const ACTIVE_ALLOCATION_SLOTS: usize = 65_536;
#[cfg(test)]
const FREED_QUARANTINE_SLOTS: usize = 64;
#[cfg(not(test))]
const FREED_QUARANTINE_SLOTS: usize = 8_192;
#[cfg(all(not(test), rustos_debug_print_enabled))]
const DEBUGCON_PORT: u16 = 0x00e9;

static HEAP_INIT: Once<()> = Once::new();

#[cfg(not(test))]
static HEAP: LockedHeap<HEAP_ORDER> = LockedHeap::<HEAP_ORDER>::new();
static ALLOCATION_TRACKER: Mutex<AllocationTracker> = Mutex::new(AllocationTracker::new());

#[cfg(not(test))]
#[global_allocator]
static KERNEL_ALLOCATOR: KernelAllocator = KernelAllocator;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapViolationKind {
    DoubleFree,
    InvalidFree,
    LayoutMismatch,
    TrackerOverflow,
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
    Tombstone = 2,
}

struct AllocationTracker {
    active: [ActiveAllocEntry; ACTIVE_ALLOCATION_SLOTS],
    freed: [FreedAllocEntry; FREED_QUARANTINE_SLOTS],
    freed_cursor: usize,
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
            double_free_count: 0,
            invalid_free_count: 0,
            layout_mismatch_count: 0,
            tracker_overflow_count: 0,
        }
    }

    fn record_alloc(&mut self, ptr: usize, layout: Layout) -> bool {
        self.clear_freed(ptr);
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

        self.active[slot] = ActiveAllocEntry {
            state: ActiveAllocState::Tombstone,
            ..ActiveAllocEntry::EMPTY
        };
        self.record_freed(ptr, entry.size, entry.align);
        Ok(())
    }

    fn insert_active(&mut self, ptr: usize, size: usize, align: usize) -> bool {
        let mut slot = hash_ptr(ptr) & (ACTIVE_ALLOCATION_SLOTS - 1);
        let mut first_tombstone = None;
        for _ in 0..ACTIVE_ALLOCATION_SLOTS {
            match self.active[slot].state {
                ActiveAllocState::Empty => {
                    let slot = first_tombstone.unwrap_or(slot);
                    self.active[slot] = ActiveAllocEntry {
                        ptr,
                        size,
                        align,
                        state: ActiveAllocState::Occupied,
                    };
                    return true;
                }
                ActiveAllocState::Tombstone => {
                    if first_tombstone.is_none() {
                        first_tombstone = Some(slot);
                    }
                }
                ActiveAllocState::Occupied => {
                    if self.active[slot].ptr == ptr {
                        self.active[slot].size = size;
                        self.active[slot].align = align;
                        return true;
                    }
                }
            }
            slot = (slot + 1) & (ACTIVE_ALLOCATION_SLOTS - 1);
        }

        self.tracker_overflow_count = self.tracker_overflow_count.saturating_add(1);
        false
    }

    fn find_active_slot(&self, ptr: usize) -> Option<usize> {
        let mut slot = hash_ptr(ptr) & (ACTIVE_ALLOCATION_SLOTS - 1);
        for _ in 0..ACTIVE_ALLOCATION_SLOTS {
            match self.active[slot].state {
                ActiveAllocState::Empty => return None,
                ActiveAllocState::Occupied if self.active[slot].ptr == ptr => return Some(slot),
                ActiveAllocState::Occupied | ActiveAllocState::Tombstone => {}
            }
            slot = (slot + 1) & (ACTIVE_ALLOCATION_SLOTS - 1);
        }
        None
    }

    fn clear_freed(&mut self, ptr: usize) {
        for entry in &mut self.freed {
            if entry.ptr == ptr {
                *entry = FreedAllocEntry::EMPTY;
            }
        }
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
}

#[inline]
const fn hash_ptr(ptr: usize) -> usize {
    ptr.rotate_right(17)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15usize)
}

#[cfg(not(test))]
struct KernelAllocator;

#[cfg(not(test))]
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        interrupts::without_interrupts(|| {
            let mut tracker = ALLOCATION_TRACKER.lock();
            let ptr = unsafe { HEAP.alloc(layout) };
            if ptr.is_null() {
                return ptr;
            }
            if tracker.record_alloc(ptr as usize, layout) {
                return ptr;
            }
            heap_violation_log(
                HeapViolationKind::TrackerOverflow,
                ptr as usize,
                layout,
                None,
            );
            unsafe { HEAP.dealloc(ptr, layout) };
            ptr::null_mut()
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _ = unsafe { try_dealloc(ptr, layout) };
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.alloc(layout) };
        if !ptr.is_null() {
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
        let mut tracker = ALLOCATION_TRACKER.lock();
        match tracker.begin_dealloc(ptr as usize, layout) {
            Ok(()) => {
                drop(tracker);
                unsafe { HEAP.dealloc(ptr, layout) };
                Ok(())
            }
            Err(kind) => {
                let expected = tracker.expected_layout(ptr as usize);
                drop(tracker);
                heap_violation_log(kind, ptr as usize, layout, expected);
                Err(kind)
            }
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
            HeapViolationKind::TrackerOverflow => write_literal(&mut port, b"tracker-overflow"),
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
fn write_literal(port: &mut Port<u8>, bytes: &[u8]) {
    for &byte in bytes {
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
            unsafe { port.write(HEX[nibble]) };
        }
    }
}

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
    fn tracker_clears_recent_free_when_pointer_is_reused() {
        let mut tracker = AllocationTracker::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        assert!(tracker.record_alloc(0x3000, layout));
        assert_eq!(tracker.begin_dealloc(0x3000, layout), Ok(()));
        assert!(tracker.is_recently_freed(0x3000));
        assert!(tracker.record_alloc(0x3000, layout));
        assert!(!tracker.is_recently_freed(0x3000));
        assert_eq!(tracker.begin_dealloc(0x3000, layout), Ok(()));
    }
}
