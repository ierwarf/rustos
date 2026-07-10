use boot_protocol::{BootInfo, BootMemoryKind, BootMemoryRegion};
use core::ptr;

use spin::Mutex;
use x86_64::PhysAddr;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

use crate::memory::kernel_vm::{DIRECT_MAP_PHYS_LIMIT, higher_half_addr};

const PAGE_SIZE: u64 = 4096;
const BITS_PER_WORD: usize = 64;
const MAX_USABLE_RANGES: usize = 128;
const PHYS_ALLOC_SCAN_MILESTONE_FRAMES: usize = 64 * 1024;

static PHYS_ALLOCATOR: Mutex<PhysAllocatorState> = Mutex::new(PhysAllocatorState::new());

#[inline]
fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(not(test))]
    {
        interrupts::without_interrupts(f)
    }

    #[cfg(test)]
    {
        f()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeFrameError {
    NonPageAligned,
    OutsideAllocatorRange,
    Reserved,
    BitmapBacking,
    AlreadyFree,
}

struct PhysAllocatorState {
    initialized: bool,
    bitmap_ptr: *mut u64,
    bitmap_words: usize,
    frame_count: usize,
    usable_frames: usize,
    free_frames: usize,
    next_hint: usize,
    usable_ranges: [UsableFrameRange; MAX_USABLE_RANGES],
    usable_range_count: usize,
    bitmap_phys_start: u64,
    bitmap_page_count: usize,
}

unsafe impl Send for PhysAllocatorState {}

#[derive(Clone, Copy)]
struct UsableFrameRange {
    start_frame: usize,
    page_count: usize,
}

impl UsableFrameRange {
    const EMPTY: Self = Self {
        start_frame: 0,
        page_count: 0,
    };

    fn contains(self, frame_index: usize) -> bool {
        let end = self.start_frame.saturating_add(self.page_count);
        (self.start_frame..end).contains(&frame_index)
    }
}

impl PhysAllocatorState {
    const fn new() -> Self {
        Self {
            initialized: false,
            bitmap_ptr: ptr::null_mut(),
            bitmap_words: 0,
            frame_count: 0,
            usable_frames: 0,
            free_frames: 0,
            next_hint: 0,
            usable_ranges: [UsableFrameRange::EMPTY; MAX_USABLE_RANGES],
            usable_range_count: 0,
            bitmap_phys_start: 0,
            bitmap_page_count: 0,
        }
    }

    fn bitmap(&self) -> &[u64] {
        if self.bitmap_words == 0 || self.bitmap_ptr.is_null() {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.bitmap_ptr.cast_const(), self.bitmap_words) }
        }
    }

    fn bitmap_mut(&mut self) -> &mut [u64] {
        if self.bitmap_words == 0 || self.bitmap_ptr.is_null() {
            &mut []
        } else {
            unsafe { core::slice::from_raw_parts_mut(self.bitmap_ptr, self.bitmap_words) }
        }
    }

    fn set_used(&mut self, frame_index: usize) {
        let word_index = frame_index / BITS_PER_WORD;
        let bit_index = frame_index % BITS_PER_WORD;
        self.bitmap_mut()[word_index] |= 1_u64 << bit_index;
    }

    fn set_free(&mut self, frame_index: usize) {
        let word_index = frame_index / BITS_PER_WORD;
        let bit_index = frame_index % BITS_PER_WORD;
        self.bitmap_mut()[word_index] &= !(1_u64 << bit_index);
    }

    fn is_used(&self, frame_index: usize) -> bool {
        let word_index = frame_index / BITS_PER_WORD;
        let bit_index = frame_index % BITS_PER_WORD;
        (self.bitmap()[word_index] & (1_u64 << bit_index)) != 0
    }

    fn mark_range_used(&mut self, frame_start: usize, page_count: usize) {
        for frame_index in frame_start..frame_start.saturating_add(page_count) {
            self.set_used(frame_index);
        }
    }

    fn mark_range_free(&mut self, frame_start: usize, page_count: usize) {
        for frame_index in frame_start..frame_start.saturating_add(page_count) {
            self.set_free(frame_index);
        }
    }

    fn reserve_phys_range(&mut self, phys_start: u64, byte_len: u64) -> usize {
        if byte_len == 0 {
            return 0;
        }

        let start = align_down(phys_start, PAGE_SIZE);
        let Some(end) = phys_start
            .checked_add(byte_len)
            .and_then(|end| align_up(end, PAGE_SIZE))
        else {
            return 0;
        };
        if end <= start {
            return 0;
        }

        let start_frame = (start / PAGE_SIZE) as usize;
        let end_frame = ((end / PAGE_SIZE) as usize).min(self.frame_count);
        let mut reserved = 0usize;
        for frame_index in start_frame..end_frame {
            if frame_is_boot_usable(self, frame_index) && !self.is_used(frame_index) {
                self.set_used(frame_index);
                reserved += 1;
            }
        }
        self.free_frames = self.free_frames.saturating_sub(reserved);
        reserved
    }

    fn alloc_contiguous_locked(&mut self, page_count: usize) -> Option<PhysAddr> {
        self.alloc_contiguous_bounded_locked(page_count, self.frame_count)
    }

    fn alloc_contiguous_bounded_locked(
        &mut self,
        page_count: usize,
        max_frame_exclusive: usize,
    ) -> Option<PhysAddr> {
        if !self.initialized || page_count == 0 || page_count > self.free_frames {
            return None;
        }
        let bounded_frame_count = self.frame_count.min(max_frame_exclusive);
        if page_count > bounded_frame_count {
            return None;
        }

        let last_start_exclusive = bounded_frame_count
            .checked_sub(page_count)?
            .saturating_add(1);
        let start_hint = self.next_hint.min(last_start_exclusive.saturating_sub(1));

        self.find_contiguous_free_range(
            start_hint,
            last_start_exclusive,
            page_count,
            bounded_frame_count,
        )
        .or_else(|| self.find_contiguous_free_range(0, start_hint, page_count, bounded_frame_count))
        .map(|start_frame| {
            self.mark_range_used(start_frame, page_count);
            self.free_frames = self.free_frames.saturating_sub(page_count);
            self.next_hint = start_frame.saturating_add(page_count);
            PhysAddr::new(start_frame as u64 * PAGE_SIZE)
        })
    }

    fn find_contiguous_free_range(
        &self,
        start: usize,
        end: usize,
        page_count: usize,
        max_frame_exclusive: usize,
    ) -> Option<usize> {
        if start >= end || page_count == 0 {
            return None;
        }

        let mut scanned = 0usize;
        for range in &self.usable_ranges[..self.usable_range_count] {
            let range_start = range.start_frame.max(start);
            let range_end = range
                .start_frame
                .saturating_add(range.page_count)
                .min(max_frame_exclusive);
            let start_limit_end = end.min(range_end);
            if range_start >= start_limit_end {
                continue;
            }

            let mut frame = range_start;
            while let Some(candidate) = self.next_free_frame_in_range(frame, start_limit_end) {
                scanned = scanned.saturating_add(candidate.saturating_sub(frame) + 1);
                let run_len = self.free_run_len_from(candidate, range_end, page_count);

                if run_len == page_count {
                    self.record_scan_milestone(scanned);
                    return Some(candidate);
                }

                frame = candidate.saturating_add(run_len).saturating_add(1);
            }
        }

        self.record_scan_milestone(scanned);
        None
    }

    fn next_free_frame_in_range(&self, mut frame: usize, end: usize) -> Option<usize> {
        while frame < end {
            let word_index = frame / BITS_PER_WORD;
            let bit_index = frame % BITS_PER_WORD;
            let before_frame_mask = if bit_index == 0 {
                0
            } else {
                (1_u64 << bit_index) - 1
            };
            let word = self.bitmap()[word_index] | before_frame_mask;
            if word != u64::MAX {
                let candidate = word_index * BITS_PER_WORD + (!word).trailing_zeros() as usize;
                if candidate < end {
                    return Some(candidate);
                }
            }
            frame = (word_index + 1) * BITS_PER_WORD;
        }
        None
    }

    fn free_run_len_from(&self, mut frame: usize, end: usize, limit: usize) -> usize {
        let mut run_len = 0usize;
        while frame < end && run_len < limit {
            let word_index = frame / BITS_PER_WORD;
            let bit_index = frame % BITS_PER_WORD;
            let remaining_word_bits = BITS_PER_WORD - bit_index;
            let remaining_range_bits = end - frame;
            let remaining_limit_bits = limit - run_len;
            let max_bits = remaining_word_bits
                .min(remaining_range_bits)
                .min(remaining_limit_bits);
            let shifted = self.bitmap()[word_index] >> bit_index;
            let free_bits = (!shifted).trailing_ones() as usize;
            let advance = free_bits.min(max_bits);
            run_len += advance;
            frame += advance;
            if advance < max_bits {
                break;
            }
        }
        run_len
    }

    fn record_scan_milestone(&self, scanned_frames: usize) {
        if scanned_frames < PHYS_ALLOC_SCAN_MILESTONE_FRAMES {
            return;
        }
        crate::debug::record_milestone(
            crate::debug::LogCategory::Memory,
            "phys-contig-scan",
            scanned_frames as u64,
            self.free_frames as u64,
        );
    }

    fn free_frame_locked(&mut self, phys: PhysAddr) -> Result<(), FreeFrameError> {
        let phys_addr = phys.as_u64();
        if !phys_addr.is_multiple_of(PAGE_SIZE) {
            return Err(FreeFrameError::NonPageAligned);
        }

        let frame_index = (phys_addr / PAGE_SIZE) as usize;
        if frame_index >= self.frame_count {
            return Err(FreeFrameError::OutsideAllocatorRange);
        }
        if !frame_is_boot_usable(self, frame_index) {
            return Err(FreeFrameError::Reserved);
        }
        if self.bitmap_phys_start != 0 {
            let bitmap_start = self.bitmap_phys_start;
            let bitmap_end = bitmap_start + self.bitmap_page_count as u64 * PAGE_SIZE;
            if (bitmap_start..bitmap_end).contains(&phys_addr) {
                return Err(FreeFrameError::BitmapBacking);
            }
        }
        if !self.is_used(frame_index) {
            return Err(FreeFrameError::AlreadyFree);
        }

        self.set_free(frame_index);
        self.free_frames = self.free_frames.saturating_add(1);
        self.next_hint = self.next_hint.min(frame_index);
        Ok(())
    }
}

pub fn init(boot_info_ptr: *const BootInfo) {
    irq_safe(|| {
        let mut state = PHYS_ALLOCATOR.lock();
        if state.initialized {
            return;
        }

        let boot_info = boot_info_from_ptr(boot_info_ptr);
        let memory_map = boot_memory_map(boot_info);

        let max_phys_end = usable_region_spans(memory_map)
            .filter_map(|(start_phys, page_count)| {
                (page_count as u64)
                    .checked_mul(PAGE_SIZE)
                    .and_then(|bytes| start_phys.checked_add(bytes))
            })
            .max()
            .unwrap_or(0);
        if max_phys_end == 0 {
            panic!("boot memory map did not expose usable physical memory");
        }

        let frame_count = (max_phys_end / PAGE_SIZE) as usize;
        let bitmap_words = frame_count.div_ceil(BITS_PER_WORD);
        let bitmap_bytes = bitmap_words
            .checked_mul(core::mem::size_of::<u64>())
            .expect("physical allocator bitmap size overflow");
        let bitmap_pages = bitmap_bytes.div_ceil(PAGE_SIZE as usize);
        let image_start = align_down(boot_info.nucleus_image.phys_start, PAGE_SIZE);
        let image_end = boot_info
            .nucleus_image
            .phys_start
            .checked_add(boot_info.nucleus_image.size)
            .and_then(|end| align_up(end, PAGE_SIZE))
            .unwrap_or(DIRECT_MAP_PHYS_LIMIT)
            .min(DIRECT_MAP_PHYS_LIMIT);

        let Some(bitmap_phys) =
            find_usable_span_excluding_range(memory_map, bitmap_pages, image_start, image_end)
        else {
            panic!("failed to reserve physical allocator bitmap");
        };

        let bitmap_ptr = higher_half_addr(bitmap_phys) as *mut u64;
        unsafe {
            ptr::write_bytes(bitmap_ptr.cast::<u8>(), 0xff, bitmap_bytes);
        }

        let mut new_state = PhysAllocatorState {
            initialized: false,
            bitmap_ptr,
            bitmap_words,
            frame_count,
            usable_frames: 0,
            free_frames: 0,
            next_hint: 0,
            usable_ranges: [UsableFrameRange::EMPTY; MAX_USABLE_RANGES],
            usable_range_count: 0,
            bitmap_phys_start: bitmap_phys,
            bitmap_page_count: bitmap_pages,
        };

        for (start_phys, page_count) in usable_region_spans(memory_map) {
            let start_frame = (start_phys / PAGE_SIZE) as usize;
            if new_state.usable_range_count >= MAX_USABLE_RANGES {
                panic!("boot memory map has too many usable ranges");
            }
            new_state.usable_ranges[new_state.usable_range_count] = UsableFrameRange {
                start_frame,
                page_count,
            };
            new_state.usable_range_count += 1;
            new_state.mark_range_free(start_frame, page_count);
            new_state.usable_frames = new_state
                .usable_frames
                .checked_add(page_count)
                .expect("usable frame count overflow");
            new_state.free_frames = new_state
                .free_frames
                .checked_add(page_count)
                .expect("free frame count overflow");
        }

        new_state.reserve_phys_range(
            boot_info.nucleus_image.phys_start,
            boot_info.nucleus_image.size,
        );
        new_state.reserve_phys_range(bitmap_phys, bitmap_pages as u64 * PAGE_SIZE);
        new_state.initialized = true;

        *state = new_state;
    });
}

pub fn alloc_frame() -> Option<PhysAddr> {
    if nucleus_core::util::fault_injection::should_fail("alloc.frame") {
        crate::debug::warn!(memory, "fault injection: alloc.frame failed");
        return None;
    }
    alloc_contiguous(1)
}

pub fn alloc_contiguous(page_count: usize) -> Option<PhysAddr> {
    irq_safe(|| PHYS_ALLOCATOR.lock().alloc_contiguous_locked(page_count))
}

pub fn alloc_contiguous_below(page_count: usize, max_phys_addr_inclusive: u64) -> Option<PhysAddr> {
    irq_safe(|| {
        let mut state = PHYS_ALLOCATOR.lock();
        let Some(max_end_exclusive) = max_phys_addr_inclusive.checked_add(1) else {
            return state.alloc_contiguous_locked(page_count);
        };
        let max_frame_exclusive =
            usize::try_from(max_end_exclusive / PAGE_SIZE).unwrap_or(usize::MAX);
        state.alloc_contiguous_bounded_locked(page_count, max_frame_exclusive)
    })
}

pub fn try_free_frame(phys: PhysAddr) -> Result<(), FreeFrameError> {
    irq_safe(|| PHYS_ALLOCATOR.lock().free_frame_locked(phys))
}

pub fn free_frame(phys: PhysAddr) {
    if let Err(err) = try_free_frame(phys) {
        let phys_addr = phys.as_u64();
        match err {
            FreeFrameError::NonPageAligned => {
                panic!("attempted to free non-page-aligned frame: {:#x}", phys_addr)
            }
            FreeFrameError::OutsideAllocatorRange => {
                panic!(
                    "attempted to free frame outside allocator range: {:#x}",
                    phys_addr
                )
            }
            FreeFrameError::Reserved => {
                panic!("attempted to free reserved frame: {:#x}", phys_addr)
            }
            FreeFrameError::BitmapBacking => {
                panic!("attempted to free physical allocator bitmap backing")
            }
            FreeFrameError::AlreadyFree => {
                panic!("attempted to free already-free frame: {:#x}", phys_addr)
            }
        }
    }
}

pub fn usable_bytes() -> u64 {
    irq_safe(|| PHYS_ALLOCATOR.lock().usable_frames as u64 * PAGE_SIZE)
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
pub fn free_bytes() -> u64 {
    irq_safe(|| PHYS_ALLOCATOR.lock().free_frames as u64 * PAGE_SIZE)
}

fn boot_info_from_ptr(boot_info_ptr: *const BootInfo) -> &'static BootInfo {
    match unsafe { BootInfo::from_ptr(boot_info_ptr) } {
        Ok(boot_info) => boot_info,
        Err(error) => panic!("{}", error.as_str()),
    }
}

fn boot_memory_map(boot_info: &BootInfo) -> &'static [BootMemoryRegion] {
    unsafe {
        core::slice::from_raw_parts(
            boot_info.memory_map.entries_ptr as *const BootMemoryRegion,
            boot_info.memory_map.entry_count as usize,
        )
    }
}

fn usable_region_spans<'a>(
    regions: &'a [BootMemoryRegion],
) -> impl Iterator<Item = (u64, usize)> + 'a {
    regions.iter().filter_map(|region| {
        if region.kind != BootMemoryKind::Usable || region.page_count == 0 {
            return None;
        }

        let start = align_up(region.phys_start, PAGE_SIZE)?.min(DIRECT_MAP_PHYS_LIMIT);
        let end = region
            .page_count
            .checked_mul(PAGE_SIZE)
            .and_then(|bytes| region.phys_start.checked_add(bytes))
            .map(|end| align_down(end, PAGE_SIZE))
            .unwrap_or(0)
            .min(DIRECT_MAP_PHYS_LIMIT);
        if end <= start {
            return None;
        }

        let page_count = ((end - start) / PAGE_SIZE) as usize;
        (page_count != 0).then_some((start, page_count))
    })
}

fn find_usable_span_excluding_range(
    regions: &[BootMemoryRegion],
    required_pages: usize,
    reserved_start: u64,
    reserved_end: u64,
) -> Option<u64> {
    if required_pages == 0 {
        return None;
    }

    let required_bytes = required_pages as u64 * PAGE_SIZE;
    for (span_start, page_count) in usable_region_spans(regions) {
        let span_end = span_start + page_count as u64 * PAGE_SIZE;
        if reserved_start >= reserved_end
            || reserved_end <= span_start
            || reserved_start >= span_end
        {
            if span_has_pages(span_start, span_end, required_bytes) {
                return Some(span_start);
            }
            continue;
        }

        let before_end = reserved_start.min(span_end);
        if span_has_pages(span_start, before_end, required_bytes) {
            return Some(span_start);
        }

        let after_start = reserved_end.max(span_start);
        if span_has_pages(after_start, span_end, required_bytes) {
            return Some(after_start);
        }
    }

    None
}

fn span_has_pages(start: u64, end: u64, required_bytes: u64) -> bool {
    end.checked_sub(start)
        .map(|bytes| bytes >= required_bytes)
        .unwrap_or(false)
}

fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    value
        .checked_add(align.checked_sub(1)?)
        .map(|value| align_down(value, align))
}

fn frame_is_boot_usable(state: &PhysAllocatorState, frame_index: usize) -> bool {
    state.usable_ranges[..state.usable_range_count]
        .iter()
        .any(|range| range.contains(frame_index))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(bitmap: &mut [u64], page_count: usize, next_hint: usize) -> PhysAllocatorState {
        let mut state = PhysAllocatorState {
            initialized: true,
            bitmap_ptr: bitmap.as_mut_ptr(),
            bitmap_words: bitmap.len(),
            frame_count: page_count,
            usable_frames: page_count,
            free_frames: page_count,
            next_hint,
            usable_ranges: [UsableFrameRange::EMPTY; MAX_USABLE_RANGES],
            usable_range_count: 1,
            bitmap_phys_start: 0,
            bitmap_page_count: 0,
        };
        state.usable_ranges[0] = UsableFrameRange {
            start_frame: 0,
            page_count,
        };
        state.mark_range_free(0, page_count);
        state
    }

    #[test]
    fn usable_region_spans_filter_and_trim_to_direct_map() {
        let regions = [
            BootMemoryRegion {
                phys_start: 0,
                page_count: 4,
                kind: BootMemoryKind::Reserved,
                _reserved0: 0,
            },
            BootMemoryRegion {
                phys_start: 0x2000,
                page_count: 3,
                kind: BootMemoryKind::Usable,
                _reserved0: 0,
            },
            BootMemoryRegion {
                phys_start: DIRECT_MAP_PHYS_LIMIT - PAGE_SIZE,
                page_count: 4,
                kind: BootMemoryKind::Usable,
                _reserved0: 0,
            },
        ];

        let spans: alloc::vec::Vec<_> = usable_region_spans(&regions).collect();
        assert_eq!(
            spans,
            alloc::vec![(0x2000, 3), (DIRECT_MAP_PHYS_LIMIT - PAGE_SIZE, 1)]
        );
    }

    #[test]
    fn bitmap_allocator_reuses_freed_frames() {
        let mut bitmap = [u64::MAX; 1];
        let mut state = test_state(&mut bitmap, 8, 0);

        let first = state.alloc_contiguous_locked(2).unwrap();
        assert_eq!(first.as_u64(), 0);
        let second = state.alloc_contiguous_locked(1).unwrap();
        assert_eq!(second.as_u64(), PAGE_SIZE * 2);

        let _ = state.free_frame_locked(first);
        let reused = state.alloc_contiguous_locked(1).unwrap();
        assert_eq!(reused.as_u64(), 0);
    }

    #[test]
    fn bounded_allocator_stays_under_limit() {
        let mut bitmap = [u64::MAX; 1];
        let mut state = test_state(&mut bitmap, 8, 6);

        let allocated = state
            .alloc_contiguous_bounded_locked(2, 4)
            .expect("bounded allocation should succeed");
        assert_eq!(allocated.as_u64(), PAGE_SIZE * 2);
    }

    #[test]
    fn allocator_can_use_last_possible_contiguous_block() {
        let mut bitmap = [u64::MAX; 1];
        let mut state = test_state(&mut bitmap, 8, 6);

        let allocated = state
            .alloc_contiguous_locked(2)
            .expect("allocator should use the last valid contiguous range");
        assert_eq!(allocated.as_u64(), PAGE_SIZE * 6);
    }

    #[test]
    fn bitmap_backing_skips_reserved_kernel_image() {
        let regions = [BootMemoryRegion {
            phys_start: 0x900000,
            page_count: 0x6000,
            kind: BootMemoryKind::Usable,
            _reserved0: 0,
        }];

        let bitmap = find_usable_span_excluding_range(&regions, 1, 0x200000, 0x5e6f000)
            .expect("bitmap backing should fit after kernel image");
        assert_eq!(bitmap, 0x5e6f000);
    }

    #[test]
    fn reserve_phys_range_removes_kernel_image_from_free_set() {
        let mut bitmap = [u64::MAX; 2];
        let mut state = test_state(&mut bitmap, 128, 0);

        let reserved = state.reserve_phys_range(PAGE_SIZE * 4 + 1, PAGE_SIZE * 2);
        assert_eq!(reserved, 3);
        assert_eq!(state.free_frames, 125);
        assert!(state.is_used(4));
        assert!(state.is_used(5));
        assert!(state.is_used(6));
    }
}
