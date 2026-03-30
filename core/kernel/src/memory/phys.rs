use boot_protocol::{
    BootInfo, BootMemoryKind, BootMemoryRegion, BOOT_INFO_MAGIC, BOOT_INFO_VERSION,
};
use core::ptr;

use spin::Mutex;
use x86_64::instructions::interrupts;
use x86_64::PhysAddr;

use crate::memory::kernel_vm::{higher_half_addr, DIRECT_MAP_PHYS_LIMIT};

const PAGE_SIZE: u64 = 4096;
const BITS_PER_WORD: usize = 64;

static PHYS_ALLOCATOR: Mutex<PhysAllocatorState> = Mutex::new(PhysAllocatorState::new());

struct PhysAllocatorState {
    initialized: bool,
    bitmap_ptr: *mut u64,
    bitmap_words: usize,
    frame_count: usize,
    usable_frames: usize,
    free_frames: usize,
    next_hint: usize,
    memory_map_ptr: *const BootMemoryRegion,
    memory_map_count: usize,
    bitmap_phys_start: u64,
    bitmap_page_count: usize,
}

unsafe impl Send for PhysAllocatorState {}

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
            memory_map_ptr: ptr::null(),
            memory_map_count: 0,
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

    fn alloc_contiguous_locked(&mut self, page_count: usize) -> Option<PhysAddr> {
        if !self.initialized || page_count == 0 || page_count > self.free_frames {
            return None;
        }
        if page_count > self.frame_count {
            return None;
        }

        let limit = self.frame_count.checked_sub(page_count)?.saturating_add(1);
        let start_hint = self.next_hint.min(limit.saturating_sub(1));

        self.find_contiguous_free_range(start_hint, limit, page_count)
            .or_else(|| self.find_contiguous_free_range(0, start_hint, page_count))
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
    ) -> Option<usize> {
        if start >= end || page_count == 0 {
            return None;
        }

        let mut frame = start;
        while frame < end {
            if self.is_used(frame) {
                frame += 1;
                continue;
            }

            let mut run_len = 1usize;
            while run_len < page_count && frame + run_len < end && !self.is_used(frame + run_len) {
                run_len += 1;
            }

            if run_len == page_count {
                return Some(frame);
            }

            frame += run_len.saturating_add(1);
        }

        None
    }

    fn free_frame_locked(&mut self, phys: PhysAddr) {
        let phys_addr = phys.as_u64();
        if phys_addr % PAGE_SIZE != 0 {
            panic!("attempted to free non-page-aligned frame: {:#x}", phys_addr);
        }

        let frame_index = (phys_addr / PAGE_SIZE) as usize;
        if frame_index >= self.frame_count {
            panic!(
                "attempted to free frame outside allocator range: {:#x}",
                phys_addr
            );
        }
        if !frame_is_boot_usable(self, frame_index) {
            panic!("attempted to free reserved frame: {:#x}", phys_addr);
        }
        if self.bitmap_phys_start != 0 {
            let bitmap_start = self.bitmap_phys_start;
            let bitmap_end = bitmap_start + self.bitmap_page_count as u64 * PAGE_SIZE;
            if (bitmap_start..bitmap_end).contains(&phys_addr) {
                panic!("attempted to free physical allocator bitmap backing");
            }
        }
        if !self.is_used(frame_index) {
            panic!("attempted to free already-free frame: {:#x}", phys_addr);
        }

        self.set_free(frame_index);
        self.free_frames = self.free_frames.saturating_add(1);
        self.next_hint = self.next_hint.min(frame_index);
    }
}

pub fn init(boot_info_ptr: *const BootInfo) {
    interrupts::without_interrupts(|| {
        let mut state = PHYS_ALLOCATOR.lock();
        if state.initialized {
            return;
        }

        let boot_info = boot_info_from_ptr(boot_info_ptr);
        let memory_map = boot_memory_map(boot_info);

        let max_phys_end = usable_region_spans(memory_map)
            .map(|(start_phys, page_count)| start_phys + page_count as u64 * PAGE_SIZE)
            .max()
            .unwrap_or(0);
        if max_phys_end == 0 {
            panic!("boot memory map did not expose usable physical memory");
        }

        let frame_count = (max_phys_end / PAGE_SIZE) as usize;
        let bitmap_words = frame_count.div_ceil(BITS_PER_WORD);
        let bitmap_bytes = bitmap_words * core::mem::size_of::<u64>();
        let bitmap_pages = bitmap_bytes.div_ceil(PAGE_SIZE as usize);
        let Some(bitmap_phys) = usable_region_spans(memory_map)
            .find(|(_, page_count)| *page_count >= bitmap_pages)
            .map(|(start_phys, _)| start_phys)
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
            memory_map_ptr: boot_info.memory_map.entries_ptr as *const BootMemoryRegion,
            memory_map_count: boot_info.memory_map.entry_count as usize,
            bitmap_phys_start: bitmap_phys,
            bitmap_page_count: bitmap_pages,
        };

        for (start_phys, page_count) in usable_region_spans(memory_map) {
            let start_frame = (start_phys / PAGE_SIZE) as usize;
            new_state.mark_range_free(start_frame, page_count);
            new_state.usable_frames = new_state.usable_frames.saturating_add(page_count);
            new_state.free_frames = new_state.free_frames.saturating_add(page_count);
        }

        new_state.mark_range_used((bitmap_phys / PAGE_SIZE) as usize, bitmap_pages);
        new_state.free_frames = new_state.free_frames.saturating_sub(bitmap_pages);
        new_state.initialized = true;

        *state = new_state;
    });
}

pub fn alloc_frame() -> Option<PhysAddr> {
    alloc_contiguous(1)
}

pub fn alloc_contiguous(page_count: usize) -> Option<PhysAddr> {
    interrupts::without_interrupts(|| PHYS_ALLOCATOR.lock().alloc_contiguous_locked(page_count))
}

pub fn free_frame(phys: PhysAddr) {
    interrupts::without_interrupts(|| PHYS_ALLOCATOR.lock().free_frame_locked(phys));
}

pub fn usable_bytes() -> u64 {
    interrupts::without_interrupts(|| PHYS_ALLOCATOR.lock().usable_frames as u64 * PAGE_SIZE)
}

pub fn free_bytes() -> u64 {
    interrupts::without_interrupts(|| PHYS_ALLOCATOR.lock().free_frames as u64 * PAGE_SIZE)
}

fn boot_info_from_ptr(boot_info_ptr: *const BootInfo) -> &'static BootInfo {
    if boot_info_ptr.is_null() {
        panic!("boot info pointer is null");
    }

    let boot_info = unsafe { &*boot_info_ptr };
    if boot_info.magic != BOOT_INFO_MAGIC {
        panic!("boot info magic mismatch");
    }
    if boot_info.version != BOOT_INFO_VERSION {
        panic!("boot info version mismatch");
    }
    if boot_info.memory_map.entry_count == 0 || boot_info.memory_map.entries_ptr == 0 {
        panic!("boot info memory map is empty");
    }

    boot_info
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

        let start = region.phys_start.min(DIRECT_MAP_PHYS_LIMIT);
        let end = region
            .phys_start
            .saturating_add(region.page_count.saturating_mul(PAGE_SIZE))
            .min(DIRECT_MAP_PHYS_LIMIT);
        if end <= start {
            return None;
        }

        let page_count = ((end - start) / PAGE_SIZE) as usize;
        (page_count != 0).then_some((start, page_count))
    })
}

fn frame_is_boot_usable(state: &PhysAllocatorState, frame_index: usize) -> bool {
    if state.memory_map_ptr.is_null() || state.memory_map_count == 0 {
        return false;
    }

    let phys = frame_index as u64 * PAGE_SIZE;
    let regions =
        unsafe { core::slice::from_raw_parts(state.memory_map_ptr, state.memory_map_count) };
    usable_region_spans(regions).any(|(start_phys, page_count)| {
        let end_phys = start_phys + page_count as u64 * PAGE_SIZE;
        (start_phys..end_phys).contains(&phys)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let regions = [BootMemoryRegion {
            phys_start: 0,
            page_count: 8,
            kind: BootMemoryKind::Usable,
            _reserved0: 0,
        }];

        let mut state = PhysAllocatorState {
            initialized: true,
            bitmap_ptr: bitmap.as_mut_ptr(),
            bitmap_words: 1,
            frame_count: 8,
            usable_frames: 8,
            free_frames: 8,
            next_hint: 0,
            memory_map_ptr: regions.as_ptr(),
            memory_map_count: regions.len(),
            bitmap_phys_start: 0,
            bitmap_page_count: 0,
        };
        state.mark_range_free(0, 8);

        let first = state.alloc_contiguous_locked(2).unwrap();
        assert_eq!(first.as_u64(), 0);
        let second = state.alloc_contiguous_locked(1).unwrap();
        assert_eq!(second.as_u64(), PAGE_SIZE * 2);

        state.free_frame_locked(first);
        let reused = state.alloc_contiguous_locked(1).unwrap();
        assert_eq!(reused.as_u64(), 0);
    }
}
