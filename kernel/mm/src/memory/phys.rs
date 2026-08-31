//! Physical-frame admission, reservation, allocation, and one-time release.
//!
//! - **Owner:** `kernel-mm` owns the global physical-frame lifecycle.
//! - **Boundary:** Boot memory-map ranges are untrusted until trimmed,
//!   canonicalized, nonoverlapping, and reserved around kernel assets.
//! - **Lifecycle:** Reserved or free frames become allocated, then return once
//!   to the free set; reserved frames never enter ordinary allocation.
//! - **Concurrency:** The allocator mutation boundary is bounded and must not
//!   invoke callbacks or services.
//! - **Failure:** Exhaustion is explicit; invalid, double, partial, and
//!   reserved-range frees are rejected before mutation.
//! - **Forbidden:** No fixed 4 KiB policy outside architecture page size, silent
//!   wrap, or best-effort double free.
//! - **Evidence:** `physical-frame-lifecycle`.
use boot_protocol::{BootInfo, BootMemoryKind, BootMemoryRegion};
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use x86_64::PhysAddr;
#[cfg(all(rustos_boot_image, not(test)))]
use x86_64::instructions::interrupts;

use crate::memory::kernel_vm::{DIRECT_MAP_PHYS_LIMIT, higher_half_addr};

const PAGE_SIZE: u64 = 4096;
const BITS_PER_WORD: usize = 64;
const MAX_USABLE_RANGES: usize = 128;
const PHYS_ALLOC_SCAN_MILESTONE_FRAMES: usize = 64 * 1024;

static PHYS_ALLOCATOR: TrackedSpinLock<PhysAllocatorState, { LockClass::PhysicalAllocator as u8 }> =
    TrackedSpinLock::new(PhysAllocatorState::new());

/// Count-only workload evidence for the batched allocator. These atomics never
/// participate in admission and add no TSC read to the allocator critical
/// section. A drain publishes `(frames, lock acquisitions)` for each class.
// ORDERING: Profile counts are diagnostic-only and intentionally approximate;
// the AcqRel drain claim below only elects one emitter for each time window.
struct FrameBatchProfile {
    alloc_frames: AtomicU64,
    alloc_batches: AtomicU64,
    alloc_short: AtomicU64,
    free_frames: AtomicU64,
    free_batches: AtomicU64,
    free_failures: AtomicU64,
    rollback_frames: AtomicU64,
    rollback_batches: AtomicU64,
    rollback_failures: AtomicU64,
    last_drain_tick: AtomicU64,
}

impl FrameBatchProfile {
    const fn new() -> Self {
        Self {
            alloc_frames: AtomicU64::new(0),
            alloc_batches: AtomicU64::new(0),
            alloc_short: AtomicU64::new(0),
            free_frames: AtomicU64::new(0),
            free_batches: AtomicU64::new(0),
            free_failures: AtomicU64::new(0),
            rollback_frames: AtomicU64::new(0),
            rollback_batches: AtomicU64::new(0),
            rollback_failures: AtomicU64::new(0),
            last_drain_tick: AtomicU64::new(0),
        }
    }

    fn record_allocation(&self, requested: usize, filled: usize) {
        self.alloc_frames
            .fetch_add(filled as u64, Ordering::Relaxed);
        self.alloc_batches.fetch_add(1, Ordering::Relaxed);
        if filled < requested {
            self.alloc_short.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_free(&self, frames: usize, failed: usize, rollback: bool) {
        let freed = frames.saturating_sub(failed) as u64;
        if rollback {
            self.rollback_frames.fetch_add(freed, Ordering::Relaxed);
            self.rollback_batches.fetch_add(1, Ordering::Relaxed);
            self.rollback_failures
                .fetch_add(failed as u64, Ordering::Relaxed);
        } else {
            self.free_frames.fetch_add(freed, Ordering::Relaxed);
            self.free_batches.fetch_add(1, Ordering::Relaxed);
            self.free_failures
                .fetch_add(failed as u64, Ordering::Relaxed);
        }
    }
}

static FRAME_BATCH_PROFILE: FrameBatchProfile = FrameBatchProfile::new();

#[inline]
fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    // A dependent crate's test binary links this with `cfg(test)` false, which
    // would put `cli`/`sti` in a host process. `rustos_boot_image` is the fact
    // that decides whether we own the CPU.
    #[cfg(all(rustos_boot_image, not(test)))]
    {
        interrupts::without_interrupts(f)
    }

    #[cfg(any(not(rustos_boot_image), test))]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedRangeClaimError {
    NotInitialized,
    InvalidRange,
    OutsideUsableMemory,
    AlreadyOwned,
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

// SAFETY: the state is reachable only through PHYS_ALLOCATOR, whose tracked
// raw-spin guard serializes every mutation and pins the holder to one CPU.
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

    fn claim_fixed_range_locked(
        &mut self,
        phys_start: u64,
        byte_len: u64,
    ) -> Result<(), FixedRangeClaimError> {
        if !self.initialized {
            return Err(FixedRangeClaimError::NotInitialized);
        }
        if byte_len == 0
            || !phys_start.is_multiple_of(PAGE_SIZE)
            || !byte_len.is_multiple_of(PAGE_SIZE)
        {
            return Err(FixedRangeClaimError::InvalidRange);
        }
        let end = phys_start
            .checked_add(byte_len)
            .ok_or(FixedRangeClaimError::InvalidRange)?;
        let allocator_end = (self.frame_count as u64)
            .checked_mul(PAGE_SIZE)
            .ok_or(FixedRangeClaimError::InvalidRange)?;
        if end > allocator_end {
            return Err(FixedRangeClaimError::OutsideUsableMemory);
        }

        let start_frame = usize::try_from(phys_start / PAGE_SIZE)
            .map_err(|_| FixedRangeClaimError::InvalidRange)?;
        let page_count = usize::try_from(byte_len / PAGE_SIZE)
            .map_err(|_| FixedRangeClaimError::InvalidRange)?;
        let end_frame = start_frame
            .checked_add(page_count)
            .ok_or(FixedRangeClaimError::InvalidRange)?;
        if !(start_frame..end_frame).all(|frame| frame_is_boot_usable(self, frame)) {
            return Err(FixedRangeClaimError::OutsideUsableMemory);
        }
        if (start_frame..end_frame).any(|frame| self.is_used(frame)) {
            return Err(FixedRangeClaimError::AlreadyOwned);
        }

        self.mark_range_used(start_frame, page_count);
        self.free_frames = self
            .free_frames
            .checked_sub(page_count)
            .expect("fixed physical claim exceeded the free-frame count");
        Ok(())
    }

    fn alloc_contiguous_locked(&mut self, page_count: usize) -> Option<PhysAddr> {
        self.alloc_contiguous_bounded_locked(page_count, self.frame_count)
    }

    /// Fills `out` with up to `out.len()` single frames under one lock hold,
    /// stopping at the first allocation failure. Unlike
    /// `alloc_contiguous_locked`, frames need not be adjacent — callers that
    /// map each page independently (a process's private address space) should
    /// use this instead of a contiguous run, which is a strictly harder
    /// search this caller does not need and can fail under fragmentation
    /// where single-frame allocation would not.
    fn alloc_frames_locked(&mut self, out: &mut [PhysAddr]) -> usize {
        self.alloc_frames_locked_with_fault_gate(out, || {
            nucleus_core::util::fault_injection::should_fail("alloc.frame")
        })
    }

    fn alloc_frames_locked_with_fault_gate(
        &mut self,
        out: &mut [PhysAddr],
        mut should_fail: impl FnMut() -> bool,
    ) -> usize {
        let mut filled = 0;
        while filled < out.len() {
            if should_fail() {
                crate::debug::warn!(memory, "fault injection: alloc.frame failed");
                break;
            }
            match self.alloc_contiguous_bounded_locked(1, self.frame_count) {
                Some(addr) => {
                    out[filled] = addr;
                    filled += 1;
                }
                None => break,
            }
        }
        filled
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

    /// Frees every frame in `frames` under one lock hold. Every failure is
    /// recorded into `failures` (best-effort — extras past its capacity are
    /// dropped from the count only, not silently lost from the free itself)
    /// so the caller can log outside the lock; `free_frame_locked` continues
    /// past a rejected frame rather than aborting the batch.
    fn free_frames_locked(
        &mut self,
        frames: &[PhysAddr],
        failures: &mut [(PhysAddr, FreeFrameError)],
    ) -> usize {
        let mut failed = 0;
        for &phys in frames {
            if let Err(err) = self.free_frame_locked(phys) {
                if let Some(slot) = failures.get_mut(failed) {
                    *slot = (phys, err);
                }
                failed += 1;
            }
        }
        failed
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
        let early_system = boot_info.early_system_image;
        let early_system_start = if early_system.is_present() {
            align_down(early_system.ptr, PAGE_SIZE)
        } else {
            0
        };
        let early_system_end = if early_system.is_present() {
            early_system
                .ptr
                .checked_add(early_system.len)
                .and_then(|end| align_up(end, PAGE_SIZE))
                .unwrap_or(0)
                .min(DIRECT_MAP_PHYS_LIMIT)
        } else {
            0
        };

        let Some(bitmap_phys) = find_usable_span_excluding_ranges(
            memory_map,
            bitmap_pages,
            &[
                (image_start, image_end),
                (early_system_start, early_system_end),
                (
                    nucleus_core::ap_trampoline::TRAMPOLINE_PHYS,
                    nucleus_core::ap_trampoline::TRAMPOLINE_PHYS
                        + nucleus_core::ap_trampoline::RESERVED_BYTES,
                ),
            ],
        ) else {
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
        if early_system_start < early_system_end {
            new_state.reserve_phys_range(early_system.ptr, early_system.len);
        }
        new_state.reserve_phys_range(bitmap_phys, bitmap_pages as u64 * PAGE_SIZE);
        new_state.initialized = true;

        *state = new_state;
    });
}

pub fn alloc_frame() -> Option<PhysAddr> {
    alloc_frame_with_fault_gate(
        nucleus_core::util::fault_injection::should_fail("alloc.frame"),
        || crate::debug::warn!(memory, "fault injection: alloc.frame failed"),
        || alloc_contiguous(1),
    )
}

/// Reports whether every frame of an exact range is withheld from ordinary
/// allocation, either by an explicit claim or by never having been usable.
///
/// This exists so an owner that hardens a fixed physical range can prove it
/// actually owns it. A range that is merely *written* by bootstrap code is not
/// owned: if the allocator still lists it free, the next allocation collides
/// with the bootstrap asset, and when the hardening also removes write
/// permission the collision surfaces as a kernel write fault far from its
/// cause. Ownership and hardening must be checked together, at the point the
/// hardening happens.
pub fn range_is_withheld_from_allocation(phys_start: u64, byte_len: u64) -> bool {
    if byte_len == 0 || !phys_start.is_multiple_of(PAGE_SIZE) || !byte_len.is_multiple_of(PAGE_SIZE)
    {
        return false;
    }
    irq_safe(|| {
        let state = PHYS_ALLOCATOR.lock();
        if !state.initialized {
            return false;
        }
        let Ok(start_frame) = usize::try_from(phys_start / PAGE_SIZE) else {
            return false;
        };
        let Ok(pages) = usize::try_from(byte_len / PAGE_SIZE) else {
            return false;
        };
        (start_frame..start_frame.saturating_add(pages)).all(|frame| {
            frame >= state.frame_count
                || !frame_is_boot_usable(&state, frame)
                || state.is_used(frame)
        })
    })
}

/// Atomically remove one exact page-aligned boot-usable range from the general
/// allocator. Architecture bootstrap code uses this only after resolving the
/// complete fixed range and before writing any byte into it.
pub fn claim_fixed_range(phys_start: u64, byte_len: u64) -> Result<(), FixedRangeClaimError> {
    irq_safe(|| {
        PHYS_ALLOCATOR
            .lock()
            .claim_fixed_range_locked(phys_start, byte_len)
    })
}

fn alloc_frame_with_fault_gate(
    faulted: bool,
    on_fault: impl FnOnce(),
    allocate: impl FnOnce() -> Option<PhysAddr>,
) -> Option<PhysAddr> {
    if faulted {
        on_fault();
        None
    } else {
        allocate()
    }
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

/// Fills `out` with independent (not necessarily adjacent) frames under one
/// lock acquisition, for callers that would otherwise call `alloc_frame` once
/// per page in a loop — a process's private address-space population is the
/// motivating case. Returns the number of frames actually filled; a short
/// fill (fewer than `out.len()`) means the allocator ran out, mirroring what
/// a per-page `alloc_frame()` loop would have found on its next iteration.
pub fn alloc_frames_batch(out: &mut [PhysAddr]) -> usize {
    let filled = irq_safe(|| PHYS_ALLOCATOR.lock().alloc_frames_locked(out));
    FRAME_BATCH_PROFILE.record_allocation(out.len(), filled);
    filled
}

/// Frees every frame in `frames` under one lock acquisition. Failures are
/// written into `failures` (per-slot, extras beyond its length are counted
/// but not recorded) so the caller can log them *after* the lock and any
/// IRQ-off region has already ended — logging must never happen while
/// `PHYS_ALLOCATOR` is held. Returns the number of failures.
pub fn try_free_frames_batch(
    frames: &[PhysAddr],
    failures: &mut [(PhysAddr, FreeFrameError)],
) -> usize {
    try_free_frames_batch_profiled(frames, failures, false)
}

/// Rollback-only counterpart to [`try_free_frames_batch`]. Keeping the
/// operation identical while classifying its count separately lets a real
/// map-failure workload prove that every partially acquired frame returned
/// under bounded allocator lock acquisitions.
pub fn try_free_frames_batch_rollback(
    frames: &[PhysAddr],
    failures: &mut [(PhysAddr, FreeFrameError)],
) -> usize {
    try_free_frames_batch_profiled(frames, failures, true)
}

fn try_free_frames_batch_profiled(
    frames: &[PhysAddr],
    failures: &mut [(PhysAddr, FreeFrameError)],
    rollback: bool,
) -> usize {
    let failed = irq_safe(|| PHYS_ALLOCATOR.lock().free_frames_locked(frames, failures));
    FRAME_BATCH_PROFILE.record_free(frames.len(), failed, rollback);
    failed
}

fn emit_frame_batch_total(name: &'static str, frames: &AtomicU64, batches: &AtomicU64) -> usize {
    let frames = frames.swap(0, Ordering::Relaxed);
    let batches = batches.swap(0, Ordering::Relaxed);
    if frames == 0 && batches == 0 {
        return 0;
    }
    crate::debug::record_milestone(crate::debug::LogCategory::Memory, name, frames, batches);
    1
}

fn emit_frame_batch_scalar(name: &'static str, value: &AtomicU64) -> usize {
    let value = value.swap(0, Ordering::Relaxed);
    if value == 0 {
        return 0;
    }
    crate::debug::record_milestone(crate::debug::LogCategory::Memory, name, value, 0);
    1
}

/// Emits and clears one bounded count window. `window_ticks == 0` forces a
/// drain for an isolated benchmark boundary.
pub fn drain_frame_batch_profile(now_tick: u64, window_ticks: u64) -> usize {
    let last = FRAME_BATCH_PROFILE.last_drain_tick.load(Ordering::Relaxed);
    if window_ticks != 0 && now_tick.saturating_sub(last) < window_ticks {
        return 0;
    }
    // ORDERING: Winning this AcqRel claim elects one drain owner for the
    // window; the count swaps remain diagnostic-only relaxed operations.
    if FRAME_BATCH_PROFILE
        .last_drain_tick
        .compare_exchange(last, now_tick, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return 0;
    }

    let mut emitted = 0;
    emitted += emit_frame_batch_total(
        "frame-batch-alloc",
        &FRAME_BATCH_PROFILE.alloc_frames,
        &FRAME_BATCH_PROFILE.alloc_batches,
    );
    emitted += emit_frame_batch_total(
        "frame-batch-free",
        &FRAME_BATCH_PROFILE.free_frames,
        &FRAME_BATCH_PROFILE.free_batches,
    );
    emitted += emit_frame_batch_total(
        "frame-batch-rollback",
        &FRAME_BATCH_PROFILE.rollback_frames,
        &FRAME_BATCH_PROFILE.rollback_batches,
    );
    emitted += emit_frame_batch_scalar("frame-batch-short", &FRAME_BATCH_PROFILE.alloc_short);
    emitted += emit_frame_batch_scalar(
        "frame-batch-free-failure",
        &FRAME_BATCH_PROFILE.free_failures,
    );
    emitted += emit_frame_batch_scalar(
        "frame-batch-rollback-failure",
        &FRAME_BATCH_PROFILE.rollback_failures,
    );
    emitted
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

// DIAGNOSTIC: Release kernels omit the physical-memory status printer.
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

fn find_usable_span_excluding_ranges(
    regions: &[BootMemoryRegion],
    required_pages: usize,
    reserved_ranges: &[(u64, u64)],
) -> Option<u64> {
    if required_pages == 0 {
        return None;
    }

    let required_bytes = required_pages as u64 * PAGE_SIZE;
    for (span_start, page_count) in usable_region_spans(regions) {
        let span_end = span_start + page_count as u64 * PAGE_SIZE;
        let mut candidate = span_start;
        loop {
            let candidate_end = candidate.checked_add(required_bytes)?;
            if candidate_end > span_end {
                break;
            }
            let next_candidate = reserved_ranges
                .iter()
                .filter_map(|&(reserved_start, reserved_end)| {
                    (reserved_start < reserved_end
                        && candidate < reserved_end
                        && reserved_start < candidate_end)
                        .then_some(reserved_end)
                })
                .max();
            let Some(next_candidate) = next_candidate else {
                return Some(candidate);
            };
            candidate = align_up(next_candidate, PAGE_SIZE)?;
        }
    }

    None
}

#[cfg(test)]
fn find_usable_span_excluding_range(
    regions: &[BootMemoryRegion],
    required_pages: usize,
    reserved_start: u64,
    reserved_end: u64,
) -> Option<u64> {
    find_usable_span_excluding_ranges(regions, required_pages, &[(reserved_start, reserved_end)])
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
    fn fixed_range_claim_is_atomic_exact_and_not_reallocatable() {
        let mut bitmap = [u64::MAX; 1];
        let mut state = test_state(&mut bitmap, 16, 0);
        let free_before = state.free_frames;

        assert_eq!(
            state.claim_fixed_range_locked(PAGE_SIZE * 4, PAGE_SIZE * 2),
            Ok(())
        );
        assert_eq!(state.free_frames, free_before - 2);
        assert!(state.is_used(4));
        assert!(state.is_used(5));
        assert_eq!(
            state.claim_fixed_range_locked(PAGE_SIZE * 4, PAGE_SIZE),
            Err(FixedRangeClaimError::AlreadyOwned)
        );
        assert_eq!(
            state.claim_fixed_range_locked(PAGE_SIZE * 8 + 1, PAGE_SIZE),
            Err(FixedRangeClaimError::InvalidRange)
        );

        let allocated: alloc::vec::Vec<_> = (0..state.free_frames)
            .map(|_| {
                state
                    .alloc_contiguous_locked(1)
                    .expect("remaining frame must allocate")
                    .as_u64()
            })
            .collect();
        assert!(!allocated.contains(&(PAGE_SIZE * 4)));
        assert!(!allocated.contains(&(PAGE_SIZE * 5)));
    }

    #[test]
    fn allocation_fault_gate_prevents_allocator_mutation() {
        let allocation_called = core::cell::Cell::new(false);
        let fault_reported = core::cell::Cell::new(false);
        assert_eq!(
            alloc_frame_with_fault_gate(
                true,
                || fault_reported.set(true),
                || {
                    allocation_called.set(true);
                    Some(PhysAddr::new(PAGE_SIZE))
                },
            ),
            None
        );
        assert!(fault_reported.get());
        assert!(!allocation_called.get());
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
    fn batch_alloc_fills_distinct_frames_under_one_lock_hold() {
        let mut bitmap = [u64::MAX; 1];
        let mut state = test_state(&mut bitmap, 8, 0);

        let mut out = [PhysAddr::new(0); 5];
        let filled = state.alloc_frames_locked(&mut out);
        assert_eq!(filled, 5);

        let mut seen: alloc::vec::Vec<u64> = out.iter().map(|frame| frame.as_u64()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 5, "batch must not repeat a frame");
        assert_eq!(state.free_frames, 3);
    }

    #[test]
    fn batch_alloc_short_fills_when_the_allocator_runs_out() {
        let mut bitmap = [u64::MAX; 1];
        let mut state = test_state(&mut bitmap, 3, 0);

        let mut out = [PhysAddr::new(0); 5];
        let filled = state.alloc_frames_locked(&mut out);
        assert_eq!(filled, 3, "only 3 frames exist to give");
        assert_eq!(state.free_frames, 0);
    }

    #[test]
    fn partial_batch_fault_returns_every_acquired_frame_exactly_once() {
        let mut bitmap = [u64::MAX; 1];
        let mut state = test_state(&mut bitmap, 16, 0);
        let free_before = state.free_frames;
        let mut out = [PhysAddr::new(0); 8];
        let mut attempts = 0;
        let filled = state.alloc_frames_locked_with_fault_gate(&mut out, || {
            attempts += 1;
            attempts == 4
        });
        assert_eq!(filled, 3);
        assert_eq!(state.free_frames, free_before - filled);

        let mut failures = [(PhysAddr::new(0), FreeFrameError::AlreadyFree); 8];
        assert_eq!(state.free_frames_locked(&out[..filled], &mut failures), 0);
        assert_eq!(state.free_frames, free_before);
        assert_eq!(
            state.free_frames_locked(&out[..filled], &mut failures),
            filled,
            "a second rollback must reject every already-returned frame"
        );
    }

    #[test]
    fn rollback_workload_is_counted_separately_from_ordinary_free() {
        let profile = FrameBatchProfile::new();
        profile.record_allocation(8, 3);
        profile.record_free(3, 0, true);
        profile.record_free(4, 1, false);

        assert_eq!(profile.alloc_frames.load(Ordering::Relaxed), 3);
        assert_eq!(profile.alloc_short.load(Ordering::Relaxed), 1);
        assert_eq!(profile.rollback_frames.load(Ordering::Relaxed), 3);
        assert_eq!(profile.rollback_batches.load(Ordering::Relaxed), 1);
        assert_eq!(profile.rollback_failures.load(Ordering::Relaxed), 0);
        assert_eq!(profile.free_frames.load(Ordering::Relaxed), 3);
        assert_eq!(profile.free_batches.load(Ordering::Relaxed), 1);
        assert_eq!(profile.free_failures.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn batch_free_returns_every_frame_and_reports_every_failure() {
        let mut bitmap = [u64::MAX; 1];
        let mut state = test_state(&mut bitmap, 8, 0);

        let mut allocated = [PhysAddr::new(0); 4];
        assert_eq!(state.alloc_frames_locked(&mut allocated), 4);
        assert_eq!(state.free_frames, 4);

        // Free three real frames plus one already-free frame, which must be
        // reported as a failure without aborting the rest of the batch.
        let already_free = PhysAddr::new(PAGE_SIZE * 7);
        let to_free = [allocated[0], allocated[1], already_free, allocated[2]];
        let mut failures = [(PhysAddr::new(0), FreeFrameError::AlreadyFree); 4];
        let failed = state.free_frames_locked(&to_free, &mut failures);

        assert_eq!(failed, 1);
        assert_eq!(failures[0], (already_free, FreeFrameError::AlreadyFree));
        // The three real frees landed even though one entry in the batch failed.
        assert_eq!(state.free_frames, 7);
        assert!(!state.is_used((allocated[0].as_u64() / PAGE_SIZE) as usize));
        assert!(!state.is_used((allocated[1].as_u64() / PAGE_SIZE) as usize));
        assert!(!state.is_used((allocated[2].as_u64() / PAGE_SIZE) as usize));
        // The fourth allocated frame was never included in the free batch.
        assert!(state.is_used((allocated[3].as_u64() / PAGE_SIZE) as usize));
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
    fn bitmap_backing_skips_kernel_image_and_early_system() {
        let regions = [BootMemoryRegion {
            phys_start: 0x900000,
            page_count: 0x6000,
            kind: BootMemoryKind::Usable,
            _reserved0: 0,
        }];

        let bitmap = find_usable_span_excluding_ranges(
            &regions,
            2,
            &[(0x200000, 0x5e6f000), (0x5e6f000, 0x5e71000)],
        )
        .expect("bitmap backing should fit after reserved boot inputs");
        assert_eq!(bitmap, 0x5e71000);
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

    /// The AP startup pages must leave the free set, and stay out of it.
    ///
    /// `ap_trampoline::seal` makes this exact range read-only in the direct
    /// map once the APs are up, so a frame handed out of it panics the kernel
    /// on its first write. Boot claims the range immediately after
    /// `init_phys`; this pins the two properties that claim depends on - that
    /// the range is claimable from a fresh allocator, and that claiming it
    /// actually removes both frames.
    #[test]
    fn ap_trampoline_range_is_claimable_and_leaves_the_free_set() {
        let mut bitmap = [u64::MAX; 2];
        let mut state = test_state(&mut bitmap, 128, 0);

        let start = nucleus_core::ap_trampoline::TRAMPOLINE_PHYS;
        let bytes = nucleus_core::ap_trampoline::RESERVED_BYTES;
        let first = usize::try_from(start / PAGE_SIZE).expect("trampoline frame index");
        let pages = usize::try_from(bytes / PAGE_SIZE).expect("trampoline page count");
        assert_eq!(pages, 2, "the seal covers exactly the claimed range");

        state
            .claim_fixed_range_locked(start, bytes)
            .expect("fresh allocator must be able to claim the AP startup pages");
        for frame in first..first + pages {
            assert!(state.is_used(frame), "frame {frame} stayed allocatable");
        }
        assert_eq!(state.free_frames, 128 - pages);

        // A second claim must report the range as taken rather than silently
        // handing the trampoline to two owners.
        assert_eq!(
            state.claim_fixed_range_locked(start, bytes),
            Err(FixedRangeClaimError::AlreadyOwned)
        );
    }
}
