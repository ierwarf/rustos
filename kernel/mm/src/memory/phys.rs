//! Physical-frame admission, reservation, allocation, and one-time release.
//!
//! - **Owner:** `kernel-mm` owns the global physical-frame lifecycle.
//! - **Boundary:** Boot memory-map ranges are untrusted until trimmed,
//!   canonicalized, nonoverlapping, and reserved around kernel assets.
//! - **Lifecycle:** Reserved or free frames become allocated, then return once
//!   to the free set; reserved frames never enter ordinary allocation.
//! - **Concurrency:** The central bitmap mutation boundary is bounded and must
//!   not invoke callbacks or services. Single-frame traffic is absorbed by
//!   per-CPU frame caches (magazines): a cache is a bounded stack of frames that
//!   stay marked used in the central bitmap and are additionally marked in a
//!   lock-free `cached` bitmap. Lock order is cache slot -> central allocator,
//!   one slot at a time. A cached frame is *free*: it counts toward free
//!   memory, the user watermark and the OOM watchdog (`free = central +
//!   sum(cache counts)`), is returned to the central set before any contiguous
//!   search, fixed claim or reported exhaustion, and is rejected as
//!   `AlreadyFree` if released again.
//! - **Failure:** Exhaustion is explicit; invalid, double, partial, and
//!   reserved-range frees are rejected before mutation.
//! - **Forbidden:** No fixed 4 KiB policy outside architecture page size, silent
//!   wrap, or best-effort double free.
//! - **Evidence:** `physical-frame-lifecycle`.
use boot_protocol::{BootInfo, BootMemoryKind, BootMemoryRegion};
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::PhysAddr;
#[cfg(all(rustos_boot_image, not(test)))]
use x86_64::instructions::interrupts;

use crate::memory::kernel_vm::{DIRECT_MAP_PHYS_LIMIT, higher_half_addr};

const PAGE_SIZE: u64 = 4096;
const BITS_PER_WORD: usize = 64;
const MAX_USABLE_RANGES: usize = 128;
const PHYS_ALLOC_SCAN_MILESTONE_FRAMES: usize = 64 * 1024;
/// Floor and ceiling of the kernel-only reserve: 1 MiB to 64 MiB.
const MIN_FREE_FLOOR_FRAMES: usize = 256;
const MIN_FREE_CEILING_FRAMES: usize = 16 * 1024;
/// The reserve is one 128th of free memory at boot (about 0.8%).
const MIN_FREE_DIVISOR: usize = 128;

/// Times a user-class allocation was refused, or cut short, by the watermark.
static USER_WATERMARK_DENIALS: AtomicU64 = AtomicU64::new(0);

/// Size of the kernel-only reserve for a machine with `free_frames` free at
/// boot.
///
/// This is the `min_free_kbytes` role: below it, user allocation fails while
/// the kernel keeps enough frames to run an OOM policy, tear a victim down, and
/// report. On a machine too small for the floor the reserve is an eighth of
/// memory so user allocation is never refused outright.
pub(crate) const fn min_free_for(free_frames: usize) -> usize {
    let scaled = free_frames / MIN_FREE_DIVISOR;
    let clamped = if scaled < MIN_FREE_FLOOR_FRAMES {
        MIN_FREE_FLOOR_FRAMES
    } else if scaled > MIN_FREE_CEILING_FRAMES {
        MIN_FREE_CEILING_FRAMES
    } else {
        scaled
    };
    let small_machine_cap = free_frames / 8;
    if clamped > small_machine_cap {
        small_machine_cap
    } else {
        clamped
    }
}


mod frame_descriptor_ledger;
pub use frame_descriptor_ledger::{
    CowFrameKind, CowMappingClaim, DataLeafRelease, SharedAliasTicket, bind_root_charge,
    root_admits, cancel_cow_mapping,
    cancel_data_leaf, cancel_lazy_table_record, cancel_private_file_data_leaf, cancel_shared_alias,
    TableClaim, claim_data_leaf, claim_lazy_table_record, claim_private_file_data_leaf, commit_cow_mapping,
    data_leaf_count, data_leaf_cow_identity, data_leaf_is_owned, lazy_table_record_count,
    lazy_table_record_is_owned, pop_lazy_table_record, prepare_shared_alias,
    promote_cow_mapping_in_place, publish_data_leaf, publish_lazy_table_record,
    publish_private_file_data_leaf, publish_shared_alias, register_data_leaf_root,
    register_lazy_table_root, release_data_leaf, rollback_shared_alias, try_claim_cow_mapping,
    unregister_data_leaf_root,
};
use frame_descriptor_ledger::{CowMappingRecord, FRAME_DESCRIPTORS, FrameDescriptorRecord};

mod frame_batch_profile;
mod frame_cache;
pub use frame_cache::FrameCacheStats;
use frame_cache::{FRAME_CACHE_SLOTS, PHYS_POOL, frame_cache_params};

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
    geo: Geometry,
    usable_frames: usize,
    /// Frames free in the central bitmap. Frames parked in a per-CPU cache are
    /// *not* counted here; see [`PhysPool::free_total`].
    free_frames: usize,
    /// Frames only kernel-class allocation may consume. See [`min_free_for`].
    min_free_frames: usize,
    next_hint: usize,
}

// SAFETY: the state is reachable only through the pool's central lock, whose
// tracked raw-spin guard serializes every mutation and pins the holder to one
// CPU. `Geometry` holds bitmap views whose words are atomics.
unsafe impl Send for PhysAllocatorState {}

/// Atomic word view over a frame bitmap that lives in direct-mapped memory.
///
/// The central `used` bitmap is mutated only under the central lock but is read
/// lock-free by the cache release path, so every word is an `AtomicU64`.
/// The `cached` bitmap is mutated lock-free with real atomic read-modify-writes.
#[derive(Clone, Copy)]
struct BitmapView {
    ptr: *const AtomicU64,
    words: usize,
}

// SAFETY: the view only hands out `&AtomicU64`; concurrent access is atomic.
unsafe impl Send for BitmapView {}
// SAFETY: as above.
unsafe impl Sync for BitmapView {}

impl BitmapView {
    /// A view of an allocator that has no cache bitmap: every bit reads clear.
    const EMPTY: Self = Self {
        ptr: ptr::null(),
        words: 0,
    };

    fn from_raw(ptr: *mut u64, words: usize) -> Self {
        Self {
            ptr: ptr.cast_const().cast::<AtomicU64>(),
            words,
        }
    }

    fn is_empty(&self) -> bool {
        self.words == 0 || self.ptr.is_null()
    }

    fn word(&self, index: usize) -> &AtomicU64 {
        assert!(index < self.words, "frame bitmap word out of range");
        // SAFETY: `index < words`, and `from_raw` was given `words` live,
        // 8-byte-aligned words of reserved direct-mapped memory that is never
        // freed; `AtomicU64` has the layout of `u64`.
        unsafe { &*self.ptr.add(index) }
    }

    /// Plain word load for a bitmap whose writers hold the central lock.
    fn load_word(&self, index: usize) -> u64 {
        // ORDERING: Relaxed; the central lock orders every writer, and a
        // lock-free reader only inspects bits of frames it owns.
        self.word(index).load(Ordering::Relaxed)
    }

    fn store_word(&self, index: usize, value: u64) {
        // ORDERING: Relaxed; only the central-lock holder stores, so the lock
        // release publishes the word to the next holder.
        self.word(index).store(value, Ordering::Relaxed);
    }

    fn get(&self, frame: usize) -> bool {
        if self.is_empty() {
            return false;
        }
        self.load_word(frame / BITS_PER_WORD) & (1_u64 << (frame % BITS_PER_WORD)) != 0
    }

    /// Sets the bit and returns whether it was already set.
    fn test_and_set(&self, frame: usize) -> bool {
        if self.is_empty() {
            return false;
        }
        let bit = 1_u64 << (frame % BITS_PER_WORD);
        // ORDERING: AcqRel makes this the single arbiter between concurrent
        // releases of one frame and publishes the claim before the frame is
        // pushed to a cache.
        self.word(frame / BITS_PER_WORD)
            .fetch_or(bit, Ordering::AcqRel)
            & bit
            != 0
    }

    fn clear(&self, frame: usize) {
        if self.is_empty() {
            return;
        }
        // ORDERING: Release orders the preceding central-bitmap update before
        // the frame stops being visible as cached.
        self.word(frame / BITS_PER_WORD)
            .fetch_and(!(1_u64 << (frame % BITS_PER_WORD)), Ordering::Release);
    }

    fn get_acquire(&self, frame: usize) -> bool {
        if self.is_empty() {
            return false;
        }
        // ORDERING: Acquire pairs with the Release in `clear`.
        self.word(frame / BITS_PER_WORD).load(Ordering::Acquire)
            & (1_u64 << (frame % BITS_PER_WORD))
            != 0
    }
}

/// Immutable-after-init description of the frame space, copied out of the
/// state so the cache release path can validate a frame without any lock.
#[derive(Clone, Copy)]
struct Geometry {
    frame_count: usize,
    usable_ranges: [UsableFrameRange; MAX_USABLE_RANGES],
    usable_range_count: usize,
    bitmap_phys_start: u64,
    /// Pages of both bitmaps (`used` then `cached`).
    bitmap_page_count: usize,
    used: BitmapView,
    cached: BitmapView,
    cache_batch: usize,
    cache_high: usize,
}

impl Geometry {
    const fn new() -> Self {
        let (cache_batch, cache_high) = frame_cache_params(0);
        Self {
            frame_count: 0,
            usable_ranges: [UsableFrameRange::EMPTY; MAX_USABLE_RANGES],
            usable_range_count: 0,
            bitmap_phys_start: 0,
            bitmap_page_count: 0,
            used: BitmapView::EMPTY,
            cached: BitmapView::EMPTY,
            cache_batch,
            cache_high,
        }
    }

    fn is_boot_usable(&self, frame_index: usize) -> bool {
        self.usable_ranges[..self.usable_range_count]
            .iter()
            .any(|range| range.contains(frame_index))
    }

    /// Every release check that does not depend on allocation state.
    fn validate_release(&self, phys: PhysAddr) -> Result<usize, FreeFrameError> {
        let phys_addr = phys.as_u64();
        if !phys_addr.is_multiple_of(PAGE_SIZE) {
            return Err(FreeFrameError::NonPageAligned);
        }
        let frame_index = (phys_addr / PAGE_SIZE) as usize;
        if frame_index >= self.frame_count {
            return Err(FreeFrameError::OutsideAllocatorRange);
        }
        if !self.is_boot_usable(frame_index) {
            return Err(FreeFrameError::Reserved);
        }
        if self.bitmap_phys_start != 0 {
            let bitmap_start = self.bitmap_phys_start;
            let bitmap_end = bitmap_start + self.bitmap_page_count as u64 * PAGE_SIZE;
            if (bitmap_start..bitmap_end).contains(&phys_addr) {
                return Err(FreeFrameError::BitmapBacking);
            }
        }
        Ok(frame_index)
    }
}

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
            geo: Geometry::new(),
            usable_frames: 0,
            free_frames: 0,
            min_free_frames: 0,
            next_hint: 0,
        }
    }

    fn set_used(&mut self, frame_index: usize) {
        let word_index = frame_index / BITS_PER_WORD;
        let word = self.geo.used.load_word(word_index);
        self.geo
            .used
            .store_word(word_index, word | (1_u64 << (frame_index % BITS_PER_WORD)));
    }

    fn set_free(&mut self, frame_index: usize) {
        let word_index = frame_index / BITS_PER_WORD;
        let word = self.geo.used.load_word(word_index);
        self.geo
            .used
            .store_word(word_index, word & !(1_u64 << (frame_index % BITS_PER_WORD)));
    }

    fn is_used(&self, frame_index: usize) -> bool {
        self.geo.used.get(frame_index)
    }

    /// Applies `set` to every bit of `[frame_start, frame_start + page_count)`
    /// one word at a time; a multi-megabyte run is a handful of word writes
    /// under the allocator lock instead of one read-modify-write per frame.
    fn update_range_words(&mut self, frame_start: usize, page_count: usize, set: bool) {
        let end = frame_start.saturating_add(page_count);
        let bitmap = self.geo.used;
        let mut frame = frame_start;
        while frame < end {
            let word_index = frame / BITS_PER_WORD;
            let bit_index = frame % BITS_PER_WORD;
            let bits = (BITS_PER_WORD - bit_index).min(end - frame);
            let mask = if bits == BITS_PER_WORD {
                u64::MAX
            } else {
                ((1_u64 << bits) - 1) << bit_index
            };
            let word = bitmap.load_word(word_index);
            bitmap.store_word(word_index, if set { word | mask } else { word & !mask });
            frame += bits;
        }
    }

    fn mark_range_used(&mut self, frame_start: usize, page_count: usize) {
        self.update_range_words(frame_start, page_count, true);
    }

    fn mark_range_free(&mut self, frame_start: usize, page_count: usize) {
        self.update_range_words(frame_start, page_count, false);
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
        let end_frame = ((end / PAGE_SIZE) as usize).min(self.geo.frame_count);
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
        let allocator_end = (self.geo.frame_count as u64)
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

    #[cfg(test)]
    fn alloc_contiguous_locked(&mut self, page_count: usize) -> Option<PhysAddr> {
        self.alloc_contiguous_bounded_locked(page_count, self.geo.frame_count)
    }

    /// Fills `out` with up to `out.len()` single frames under one lock hold,
    /// stopping at the first allocation failure. Unlike
    /// `alloc_contiguous_locked`, frames need not be adjacent — callers that
    /// map each page independently (a process's private address space) should
    /// use this instead of a contiguous run, which is a strictly harder
    /// search this caller does not need and can fail under fragmentation
    /// where single-frame allocation would not.
    #[cfg(test)]
    fn alloc_frames_locked(&mut self, out: &mut [PhysAddr]) -> usize {
        self.alloc_frames_locked_with_fault_gate(out, || {
            nucleus_core::util::fault_injection::should_fail("alloc.frame")
        })
    }

    /// Like [`Self::alloc_frames_locked`], but never dips into the kernel
    /// reserve: it fills at most `free_frames - min_free_frames` slots.
    ///
    /// A short fill is the user-visible memory-pressure signal. The reserve
    /// stays available to the callers that must make progress while the system
    /// is short - heap growth, address-space retirement, IPC, the fault-frame
    /// reserve's own producer excepted - so a refusal here can still be
    /// reported and resolved instead of wedging the kernel.
    ///
    /// `cached_frames` are free frames parked in per-CPU caches. They are part
    /// of the memory the reserve is measured against, but this call fills only
    /// from the central bitmap, so a short fill with `cached_frames != 0` tells
    /// the caller to drain the caches and retry rather than report exhaustion.
    fn alloc_user_frames_locked(
        &mut self,
        out: &mut [PhysAddr],
        cached_frames: usize,
        should_fail: impl FnMut() -> bool,
    ) -> usize {
        let admissible = (self.free_frames + cached_frames).saturating_sub(self.min_free_frames);
        let want = out.len().min(admissible);
        let filled = self.alloc_frames_locked_with_fault_gate(&mut out[..want], should_fail);
        if filled < out.len() && admissible < out.len() {
            USER_WATERMARK_DENIALS.fetch_add(1, Ordering::Relaxed);
        }
        filled
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
            match self.alloc_contiguous_bounded_locked(1, self.geo.frame_count) {
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
        let start_frame = self.find_alloc_start_locked(page_count, max_frame_exclusive)?;
        self.commit_alloc_locked(start_frame, page_count);
        Some(PhysAddr::new(start_frame as u64 * PAGE_SIZE))
    }

    fn commit_alloc_locked(&mut self, start_frame: usize, page_count: usize) {
        self.mark_range_used(start_frame, page_count);
        self.free_frames = self.free_frames.saturating_sub(page_count);
        self.next_hint = start_frame.saturating_add(page_count);
    }

    fn find_alloc_start_locked(
        &self,
        page_count: usize,
        max_frame_exclusive: usize,
    ) -> Option<usize> {
        if !self.initialized || page_count == 0 || page_count > self.free_frames {
            return None;
        }
        let bounded_frame_count = self.geo.frame_count.min(max_frame_exclusive);
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
        for range in &self.geo.usable_ranges[..self.geo.usable_range_count] {
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
            let word = self.geo.used.load_word(word_index) | before_frame_mask;
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
            let shifted = self.geo.used.load_word(word_index) >> bit_index;
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
        let frame_index = self.geo.validate_release(phys)?;
        // The `cached` bit arbitrates against a concurrent release of the same
        // frame through a CPU cache: whoever sets it first owns the release.
        if self.geo.cached.test_and_set(frame_index) {
            return Err(FreeFrameError::AlreadyFree);
        }
        if !self.is_used(frame_index) {
            self.geo.cached.clear(frame_index);
            return Err(FreeFrameError::AlreadyFree);
        }
        self.return_to_central_locked(frame_index);
        self.geo.cached.clear(frame_index);
        Ok(())
    }

    /// Marks a used (allocated or cached) frame free in the central bitmap.
    /// Clearing `used` before the caller clears `cached` keeps a racing
    /// duplicate release rejected throughout the transition.
    fn return_to_central_locked(&mut self, frame_index: usize) {
        self.set_free(frame_index);
        self.free_frames = self.free_frames.saturating_add(1);
        self.next_hint = self.next_hint.min(frame_index);
    }

    /// Picks a free frame for a CPU cache and publishes it as `cached` before
    /// it is marked used, so no window shows a used, uncached frame that a
    /// duplicate release could mistake for an allocation.
    fn take_frame_for_cache_locked(&mut self) -> Option<usize> {
        let start = self.find_alloc_start_locked(1, self.geo.frame_count)?;
        self.geo.cached.test_and_set(start);
        self.commit_alloc_locked(start, 1);
        Some(start)
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
        let mut state = PHYS_POOL.state.lock();
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
        let used_bitmap_bytes = bitmap_words
            .checked_mul(core::mem::size_of::<u64>())
            .expect("physical allocator bitmap size overflow");
        // The `used` bitmap is followed by the same-sized `cached` bitmap.
        let bitmap_bytes = used_bitmap_bytes
            .checked_mul(2)
            .expect("physical allocator bitmap size overflow");
        let bitmap_pages = bitmap_bytes.div_ceil(PAGE_SIZE as usize);
        let frame_descriptor_bytes = frame_count
            .checked_mul(core::mem::size_of::<FrameDescriptorRecord>())
            .expect("frame descriptor size overflow");
        let frame_descriptor_pages = frame_descriptor_bytes.div_ceil(PAGE_SIZE as usize);
        // One additional mapping record per frame admits a complete two-way
        // fork because each shared descriptor keeps its first mapping inline.
        let cow_mapping_count = frame_count;
        let cow_mapping_bytes = cow_mapping_count
            .checked_mul(core::mem::size_of::<CowMappingRecord>())
            .expect("COW mapping record size overflow");
        let cow_mapping_pages = cow_mapping_bytes.div_ceil(PAGE_SIZE as usize);
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
        // SAFETY: `bitmap_phys` was just reserved from usable memory outside
        // the kernel image, early-system image, and AP trampoline, spans
        // `bitmap_bytes`, and is reached through the direct map. `init` runs
        // once before any allocation, so nothing else references the region.
        unsafe {
            ptr::write_bytes(bitmap_ptr.cast::<u8>(), 0, bitmap_bytes);
            ptr::write_bytes(bitmap_ptr.cast::<u8>(), 0xff, used_bitmap_bytes);
        }

        let Some(frame_descriptor_phys) = find_usable_span_excluding_ranges(
            memory_map,
            frame_descriptor_pages,
            &[
                (image_start, image_end),
                (early_system_start, early_system_end),
                (
                    nucleus_core::ap_trampoline::TRAMPOLINE_PHYS,
                    nucleus_core::ap_trampoline::TRAMPOLINE_PHYS
                        + nucleus_core::ap_trampoline::RESERVED_BYTES,
                ),
                (bitmap_phys, bitmap_phys + bitmap_pages as u64 * PAGE_SIZE),
            ],
        ) else {
            panic!("failed to reserve physical frame descriptors");
        };
        let Some(cow_mapping_phys) = find_usable_span_excluding_ranges(
            memory_map,
            cow_mapping_pages,
            &[
                (image_start, image_end),
                (early_system_start, early_system_end),
                (
                    nucleus_core::ap_trampoline::TRAMPOLINE_PHYS,
                    nucleus_core::ap_trampoline::TRAMPOLINE_PHYS
                        + nucleus_core::ap_trampoline::RESERVED_BYTES,
                ),
                (bitmap_phys, bitmap_phys + bitmap_pages as u64 * PAGE_SIZE),
                (
                    frame_descriptor_phys,
                    frame_descriptor_phys + frame_descriptor_pages as u64 * PAGE_SIZE,
                ),
            ],
        ) else {
            panic!("failed to reserve COW mapping records");
        };
        let frame_descriptor_ptr =
            higher_half_addr(frame_descriptor_phys) as *mut FrameDescriptorRecord;
        let cow_mapping_ptr = higher_half_addr(cow_mapping_phys) as *mut CowMappingRecord;
        // SAFETY: `find_usable_span_excluding_ranges` selected this complete,
        // direct-mapped boot span; it is reserved before normal allocation and
        // covers exactly `frame_descriptor_bytes` zero-initialized descriptors.
        unsafe {
            ptr::write_bytes(frame_descriptor_ptr.cast::<u8>(), 0, frame_descriptor_bytes);
            ptr::write_bytes(cow_mapping_ptr.cast::<u8>(), 0, cow_mapping_bytes);
        }
        FRAME_DESCRIPTORS.install(
            frame_descriptor_ptr,
            frame_count,
            cow_mapping_ptr,
            cow_mapping_count,
        );

        let mut new_state = PhysAllocatorState::new();
        new_state.geo.frame_count = frame_count;
        new_state.geo.used = BitmapView::from_raw(bitmap_ptr, bitmap_words);
        // SAFETY: the allocation spans `2 * bitmap_words` words, so the second
        // half starts `bitmap_words` words past the base, inside the region.
        new_state.geo.cached =
            BitmapView::from_raw(unsafe { bitmap_ptr.add(bitmap_words) }, bitmap_words);
        new_state.geo.bitmap_phys_start = bitmap_phys;
        new_state.geo.bitmap_page_count = bitmap_pages;

        for (start_phys, page_count) in usable_region_spans(memory_map) {
            let start_frame = (start_phys / PAGE_SIZE) as usize;
            if new_state.geo.usable_range_count >= MAX_USABLE_RANGES {
                panic!("boot memory map has too many usable ranges");
            }
            new_state.geo.usable_ranges[new_state.geo.usable_range_count] = UsableFrameRange {
                start_frame,
                page_count,
            };
            new_state.geo.usable_range_count += 1;
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
        new_state.reserve_phys_range(
            frame_descriptor_phys,
            frame_descriptor_pages as u64 * PAGE_SIZE,
        );
        new_state.reserve_phys_range(cow_mapping_phys, cow_mapping_pages as u64 * PAGE_SIZE);
        new_state.min_free_frames = min_free_for(new_state.free_frames);
        (new_state.geo.cache_batch, new_state.geo.cache_high) =
            frame_cache_params(new_state.usable_frames);
        new_state.initialized = true;

        PHYS_POOL.install_locked(&mut state, new_state);
    });
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
///
/// A frame parked in a CPU cache reads as used in the central bitmap but is
/// allocatable, so it is not withheld.
pub fn range_is_withheld_from_allocation(phys_start: u64, byte_len: u64) -> bool {
    if byte_len == 0 || !phys_start.is_multiple_of(PAGE_SIZE) || !byte_len.is_multiple_of(PAGE_SIZE)
    {
        return false;
    }
    irq_safe(|| {
        let state = PHYS_POOL.state.lock();
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
            frame >= state.geo.frame_count
                || !frame_is_boot_usable(&state, frame)
                || (state.is_used(frame) && !state.geo.cached.get_acquire(frame))
        })
    })
}

/// Atomically remove one exact page-aligned boot-usable range from the general
/// allocator. Architecture bootstrap code uses this only after resolving the
/// complete fixed range and before writing any byte into it.
pub fn claim_fixed_range(phys_start: u64, byte_len: u64) -> Result<(), FixedRangeClaimError> {
    irq_safe(|| PHYS_POOL.claim_fixed_range(phys_start, byte_len))
}

/// The cache slot of the calling CPU. Any slot is correct - slots are locked -
/// so a stale or shared index costs only contention.
#[inline]
fn current_cache_slot() -> usize {
    nucleus_core::util::lockdep::current_cpu_index() % FRAME_CACHE_SLOTS
}

pub fn alloc_frame() -> Option<PhysAddr> {
    alloc_frame_with_fault_gate(
        nucleus_core::util::fault_injection::should_fail("alloc.frame"),
        || crate::debug::warn!(memory, "fault injection: alloc.frame failed"),
        || alloc_contiguous(1),
    )
}

/// Allocates one frame for a user-driven mapping, refusing to dip into the
/// kernel reserve.
///
/// Anonymous first touch, copy-on-write, page-table growth for a user range,
/// and process creation are user-class. Kernel structures that must make
/// progress under pressure keep using [`alloc_frame`].
pub fn alloc_frame_user() -> Option<PhysAddr> {
    let mut out = [PhysAddr::new(0)];
    (alloc_frames_batch_user(&mut out) == 1).then_some(out[0])
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
    irq_safe(|| PHYS_POOL.alloc_contiguous(current_cache_slot(), page_count, usize::MAX))
}

pub fn alloc_contiguous_below(page_count: usize, max_phys_addr_inclusive: u64) -> Option<PhysAddr> {
    let max_frame_exclusive = max_phys_addr_inclusive
        .checked_add(1)
        .map_or(usize::MAX, |end| {
            usize::try_from(end / PAGE_SIZE).unwrap_or(usize::MAX)
        });
    irq_safe(|| PHYS_POOL.alloc_contiguous(current_cache_slot(), page_count, max_frame_exclusive))
}

pub fn try_free_frame(phys: PhysAddr) -> Result<(), FreeFrameError> {
    let mut failures = [(PhysAddr::new(0), FreeFrameError::AlreadyFree)];
    let frames = [phys];
    match irq_safe(|| PHYS_POOL.release_frames(current_cache_slot(), &frames, &mut failures)) {
        0 => Ok(()),
        _ => Err(failures[0].1),
    }
}

/// Fills `out` with independent (not necessarily adjacent) frames, for callers
/// that would otherwise call `alloc_frame` once per page in a loop - a
/// process's private address-space population is the motivating case. Returns
/// the number of frames actually filled; a short fill (fewer than `out.len()`)
/// means the allocator ran out, mirroring what a per-page `alloc_frame()` loop
/// would have found on its next iteration.
pub fn alloc_frames_batch(out: &mut [PhysAddr]) -> usize {
    let filled = irq_safe(|| {
        PHYS_POOL.alloc_frames(current_cache_slot(), out, false, || {
            nucleus_core::util::fault_injection::should_fail("alloc.frame")
        })
    });
    frame_batch_profile::record_allocation(out.len(), filled);
    filled
}

/// User-class [`alloc_frames_batch`]: fills no more than the frames above the
/// kernel reserve. A short fill means pressure, not only exhaustion.
pub fn alloc_frames_batch_user(out: &mut [PhysAddr]) -> usize {
    let filled = irq_safe(|| {
        PHYS_POOL.alloc_frames(current_cache_slot(), out, true, || {
            nucleus_core::util::fault_injection::should_fail("alloc.frame")
        })
    });
    frame_batch_profile::record_allocation(out.len(), filled);
    filled
}

/// Frames the kernel withholds from user-class allocation.
pub fn min_free_bytes() -> u64 {
    // ORDERING: Relaxed; immutable after allocator initialization.
    PHYS_POOL.min_free.load(Ordering::Relaxed) as u64 * PAGE_SIZE
}

/// Whether free memory is at or below the user-class watermark. Frames parked
/// in CPU caches count as free.
pub fn below_user_watermark() -> bool {
    // ORDERING: Relaxed; immutable after allocator initialization.
    PHYS_POOL.free_total() <= PHYS_POOL.min_free.load(Ordering::Relaxed)
}

/// Count of user-class allocations refused or truncated by the watermark.
pub fn user_watermark_denials() -> u64 {
    USER_WATERMARK_DENIALS.load(Ordering::Relaxed)
}

/// Frames parked in per-CPU caches; they are included in [`free_bytes`].
pub fn cached_frame_count() -> usize {
    PHYS_POOL.cached_total()
}

/// Cache slow-path counters, for diagnostics.
pub fn frame_cache_stats() -> FrameCacheStats {
    PHYS_POOL.stats()
}

/// Returns every cached frame to the central set. Safe from any CPU at any time
/// (slots are locked); a CPU leaving service calls this for its own slot so
/// its frames rejoin contiguous allocation.
pub fn drain_cpu_frame_cache(cpu_index: usize) {
    irq_safe(|| PHYS_POOL.drain_slot(cpu_index));
}

/// Returns every CPU's cached frames to the central set.
pub fn drain_all_frame_caches() {
    irq_safe(|| PHYS_POOL.drain_all());
}

/// Frees every frame in `frames`. Failures are written into `failures`
/// (per-slot, extras beyond its length are counted but not recorded) so the
/// caller can log them *after* the lock and any IRQ-off region has already
/// ended - logging must never happen while an allocator lock is held. Returns
/// the number of failures.
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
    let failed = irq_safe(|| PHYS_POOL.release_frames(current_cache_slot(), frames, failures));
    frame_batch_profile::record_free(frames.len(), failed, rollback);
    failed
}

/// Emits and clears one bounded count window. `window_ticks == 0` forces a
/// drain for an isolated benchmark boundary.
pub fn drain_frame_batch_profile(now_tick: u64, window_ticks: u64) -> usize {
    frame_batch_profile::drain(now_tick, window_ticks)
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
    irq_safe(|| PHYS_POOL.state.lock().usable_frames as u64 * PAGE_SIZE)
}

// DIAGNOSTIC: Release kernels omit the physical-memory status printer.
#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
pub fn free_bytes() -> u64 {
    PHYS_POOL.free_total() as u64 * PAGE_SIZE
}

fn boot_info_from_ptr(boot_info_ptr: *const BootInfo) -> &'static BootInfo {
    // SAFETY: the pointer is the one the boot handoff gave the kernel entry.
    // `from_ptr` rejects a null, misaligned, or invalidly encoded block; that
    // the block is mapped and not concurrently written is the handoff's
    // contract, stated in `BootInfo::from_ptr`'s safety section.
    match unsafe { BootInfo::from_ptr(boot_info_ptr) } {
        Ok(boot_info) => boot_info,
        Err(error) => panic!("{}", error.as_str()),
    }
}

fn boot_memory_map(boot_info: &BootInfo) -> &'static [BootMemoryRegion] {
    // SAFETY: `boot_info` passed `BootInfo::validate`, which requires a
    // non-null, aligned `entries_ptr` and a non-zero `entry_count`, and the
    // handoff keeps the memory-map array resident and unmodified for the life
    // of the kernel.
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
    state.geo.is_boot_usable(frame_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn test_state(bitmap: &mut [u64], page_count: usize, next_hint: usize) -> PhysAllocatorState {
        let mut state = PhysAllocatorState::new();
        state.initialized = true;
        state.geo.frame_count = page_count;
        state.geo.used = BitmapView::from_raw(bitmap.as_mut_ptr(), bitmap.len());
        state.usable_frames = page_count;
        state.free_frames = page_count;
        state.next_hint = next_hint;
        state.geo.usable_range_count = 1;
        state.geo.usable_ranges[0] = UsableFrameRange {
            start_frame: 0,
            page_count,
        };
        state.mark_range_free(0, page_count);
        state
    }

    #[test]
    fn word_granular_range_marking_matches_per_bit_marking() {
        let mut words = [0_u64; 4];
        let mut state = test_state(&mut words, 256, 0);
        for (start, count) in [(0, 1), (3, 61), (60, 8), (64, 64), (100, 100), (255, 1), (10, 0)] {
            state.mark_range_free(0, 256);
            state.mark_range_used(start, count);
            for frame in 0..256 {
                assert_eq!(state.is_used(frame), (start..start + count).contains(&frame));
            }
            state.mark_range_free(start, count);
            assert!((0..256).all(|frame| !state.is_used(frame)));
        }
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
    fn reserve_scales_with_memory_and_is_clamped() {
        // 8 GiB of frames: one 128th exceeds 64 MiB, so the ceiling holds.
        assert_eq!(min_free_for(2 * 1024 * 1024), MIN_FREE_CEILING_FRAMES);
        // 256 MiB: one 128th is 2 MiB, between the floor and the ceiling.
        assert_eq!(min_free_for(64 * 1024), 512);
        // 16 MiB: one 128th is below the floor, and an eighth is above it.
        assert_eq!(min_free_for(4096), MIN_FREE_FLOOR_FRAMES);
        // A tiny machine reserves an eighth, never the whole floor.
        assert_eq!(min_free_for(512), 64);
        assert_eq!(min_free_for(0), 0);
    }

    #[test]
    fn user_allocation_stops_at_the_reserve_and_kernel_allocation_does_not() {
        let mut bitmap = [0_u64; 1];
        let mut state = test_state(&mut bitmap, 16, 0);
        state.min_free_frames = 4;
        let denials = USER_WATERMARK_DENIALS.load(Ordering::Relaxed);

        let mut out = [PhysAddr::new(0); 16];
        // Twelve frames sit above the reserve; the request is cut short there.
        assert_eq!(state.alloc_user_frames_locked(&mut out, 0, || false), 12);
        assert_eq!(state.free_frames, 4);
        assert!(USER_WATERMARK_DENIALS.load(Ordering::Relaxed) > denials);

        // At the watermark user allocation yields nothing...
        assert_eq!(state.alloc_user_frames_locked(&mut out[..1], 0, || false), 0);
        assert_eq!(state.free_frames, 4);
        // ...while the kernel class still reaches the reserve.
        assert_eq!(state.alloc_frames_locked(&mut out[..4]), 4);
        assert_eq!(state.free_frames, 0);
    }

    #[test]
    fn user_allocation_below_the_reserve_recovers_after_frames_are_freed() {
        let mut bitmap = [0_u64; 1];
        let mut state = test_state(&mut bitmap, 8, 0);
        state.min_free_frames = 2;
        let mut out = [PhysAddr::new(0); 8];
        assert_eq!(state.alloc_user_frames_locked(&mut out, 0, || false), 6);
        assert_eq!(state.alloc_user_frames_locked(&mut out[..1], 0, || false), 0);
        state.free_frame_locked(out[0]).unwrap();
        state.free_frame_locked(out[1]).unwrap();
        assert_eq!(state.alloc_user_frames_locked(&mut out[..2], 0, || false), 2);
        assert_eq!(state.free_frames, 2);
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
