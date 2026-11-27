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
//!   sum(cache counts)`), is returned to the central set before a required
//!   contiguous/fixed claim reports exhaustion, and is rejected as
//!   `AlreadyFree` if released again.
//!   Speculative anonymous folio/huge-page probes never drain caches; they
//!   bound bitmap work and fall back to the ordinary allocator on failure.
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
const PHYS_ALLOC_SCAN_MILESTONE_WORDS: usize = PHYS_ALLOC_SCAN_MILESTONE_FRAMES / BITS_PER_WORD;

#[derive(Clone, Copy)]
#[repr(u8)]
enum FrameScanKind {
    Independent = 1,
    CacheRefill = 2,
    Contiguous = 3,
}

#[derive(Default)]
struct FrameScanWork {
    bitmap_words: usize,
    index_words: usize,
}

#[derive(Clone, Copy)]
struct FrameScanProfile {
    scans: u64,
    distance_sum: u64,
    bitmap_words: u64,
    index_words: u64,
    max_distance: usize,
    sample_pages: usize,
    sample_free: usize,
    sample_bitmap_words: usize,
    sample_index_words: usize,
}

impl FrameScanProfile {
    const EMPTY: Self = Self {
        scans: 0,
        distance_sum: 0,
        bitmap_words: 0,
        index_words: 0,
        max_distance: 0,
        sample_pages: 0,
        sample_free: 0,
        sample_bitmap_words: 0,
        sample_index_words: 0,
    };
}

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
    CowFrameKind, CowMappingClaim, DataClaim, DataLeafCensus, DataLeafRelease, SHARED_ALIAS_RUN_MAX,
    SharedAliasMode, SharedAliasRequest, SharedAliasRunError, SharedAliasTicket, bind_root_charge,
    claim_cow_replacement, cancel_cow_replacement,
    claim_private_file_cache_frame, release_private_file_cache_frame,
    root_admits, cancel_cow_mapping,
    cancel_data_leaf, cancel_lazy_table_record, cancel_shared_alias,
    cancel_shared_alias_run,
    TableClaim, claim_data_leaf, claim_lazy_table_record, commit_cow_mapping,
    data_leaf_census, data_leaf_count, data_leaf_cow_identity, data_leaf_is_owned, lazy_table_record_count,
    lazy_table_record_is_owned, pop_lazy_table_record, prepare_shared_alias,
    prepare_shared_alias_run,
    promote_cow_mapping_in_place, publish_data_leaf, publish_lazy_table_record,
    publish_shared_alias, publish_shared_alias_run,
    register_data_leaf_root,
    register_lazy_table_root, rekey_data_leaf, release_data_leaf, rollback_shared_alias, try_claim_cow_mapping,
    unregister_data_leaf_root,
    anon_huge_table, cancel_data_leaf_run, claim_data_leaf_run, deposit_anon_huge_table,
    publish_data_leaf_run, take_anon_huge_table,
};
use frame_descriptor_ledger::{CowMappingRecord, FRAME_DESCRIPTORS, FrameDescriptorRecord};

mod frame_batch_profile;
mod frame_cache;
mod free_word_index;
use frame_cache::{FRAME_CACHE_SLOTS, PHYS_POOL, frame_cache_params};
use free_word_index::FreeRunIndex;

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
    /// Independent scan cursor so a failed huge-page probe does not repeatedly
    /// rescan the same fragmented prefix or move the ordinary-frame cursor.
    huge_next_block: usize,
    /// Exact multi-order free-run hierarchy, mutated only with the central bitmap.
    free_words: FreeRunIndex,
    /// Diagnostic-only windows, copied by housekeeping without rendering under
    /// the allocator lock. One row per FrameScanKind; no hot-path sink calls.
    scan_profiles: [FrameScanProfile; 3],
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
    /// Pages of both bitmaps (`used` then `cached`) and the free-word index.
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

    /// Sorts `usable_ranges[..usable_range_count]` by `start_frame` and merges
    /// overlapping or adjacent intervals so lookup and overlap checks are exact
    /// `O(log R)` binary searches over strictly disjoint ascending ranges.
    fn sort_and_merge_usable_ranges(&mut self) {
        let count = self.usable_range_count;
        if count == 0 {
            return;
        }
        self.usable_ranges[..count].sort_unstable_by_key(|range| range.start_frame);
        let mut write = 0_usize;
        for read in 0..count {
            let next = self.usable_ranges[read];
            if next.page_count == 0 {
                continue;
            }
            if write > 0 {
                let prev = &mut self.usable_ranges[write - 1];
                let prev_end = prev.end_frame();
                if next.start_frame <= prev_end {
                    let merged_end = prev_end.max(next.end_frame());
                    prev.page_count = merged_end - prev.start_frame;
                    continue;
                }
            }
            self.usable_ranges[write] = next;
            write += 1;
        }
        for slot in &mut self.usable_ranges[write..count] {
            *slot = UsableFrameRange::EMPTY;
        }
        self.usable_range_count = write;
    }

    #[inline]
    fn is_boot_usable(&self, frame_index: usize) -> bool {
        let ranges = &self.usable_ranges[..self.usable_range_count];
        let idx = ranges.partition_point(|range| range.end_frame() <= frame_index);
        idx < ranges.len() && ranges[idx].start_frame <= frame_index
    }

    /// True iff `[start_frame, end_frame)` overlaps any canonical boot-usable range.
    #[inline]
    fn overlaps_boot_usable(&self, start_frame: usize, end_frame: usize) -> bool {
        if start_frame >= end_frame {
            return false;
        }
        let ranges = &self.usable_ranges[..self.usable_range_count];
        let idx = ranges.partition_point(|range| range.end_frame() <= start_frame);
        idx < ranges.len() && ranges[idx].start_frame < end_frame
    }

    /// True iff every frame in `[start_frame, end_frame)` is boot-usable.
    /// Relies on `usable_ranges` being sorted and coalesced.
    #[inline]
    fn contains_boot_usable_range(&self, start_frame: usize, end_frame: usize) -> bool {
        if start_frame >= end_frame {
            return true;
        }
        let ranges = &self.usable_ranges[..self.usable_range_count];
        let idx = ranges.partition_point(|range| range.end_frame() <= start_frame);
        idx < ranges.len()
            && ranges[idx].start_frame <= start_frame
            && ranges[idx].end_frame() >= end_frame
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

    #[inline]
    fn end_frame(self) -> usize {
        self.start_frame.saturating_add(self.page_count)
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
            huge_next_block: 1,
            free_words: FreeRunIndex::EMPTY,
            scan_profiles: [FrameScanProfile::EMPTY; 3],
        }
    }

    fn set_used(&mut self, frame_index: usize) {
        let word_index = frame_index / BITS_PER_WORD;
        let word = self.geo.used.load_word(word_index);
        self.store_used_word(word_index, word, word | (1_u64 << (frame_index % BITS_PER_WORD)));
    }

    fn set_free(&mut self, frame_index: usize) {
        let word_index = frame_index / BITS_PER_WORD;
        let word = self.geo.used.load_word(word_index);
        self.store_used_word(word_index, word, word & !(1_u64 << (frame_index % BITS_PER_WORD)));
    }

    /// All central bitmap writes pass here, including cache refill/drain and
    /// fixed/huge claims. Higher levels change only on order-predicate edges,
    /// so consuming another free bit without changing max run order adds no
    /// index traffic.
    #[inline]
    fn store_used_word(&mut self, index: usize, before: u64, after: u64) {
        self.geo.used.store_word(index, after);
        if before != after {
            self.free_words
                .on_used_word_changed(self.geo.used, index, before, after);
        }
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
            self.store_used_word(word_index, word, if set { word | mask } else { word & !mask });
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
        if !self.geo.contains_boot_usable_range(start_frame, end_frame) {
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
        let start_frame = self.find_alloc_start_locked(page_count, FrameScanKind::Contiguous)?;
        self.commit_alloc_locked(start_frame, page_count);
        Some(PhysAddr::new(start_frame as u64 * PAGE_SIZE))
    }

    /// Speculative 64 KiB allocation: inspect a bounded number of bitmap words
    /// near the normal cursor, with no cache drain or fallback search.
    fn try_alloc_anon_folio_locked(&mut self) -> Option<PhysAddr> {
        const SEARCH_WORDS: usize = 8;
        let pages = ANON_FOLIO_PAGES;
        // Do not count cached frames: this path cannot acquire or drain them.
        if !self.initialized || self.free_frames.saturating_sub(self.min_free_frames) < pages {
            return None;
        }
        let words = self.geo.frame_count.div_ceil(BITS_PER_WORD);
        let first = (self.next_hint / BITS_PER_WORD).min(words.checked_sub(1)?);
        for offset in 0..words.min(SEARCH_WORDS) {
            let word_index = (first + offset) % words;
            let used = self.geo.used.load_word(word_index);
            for bit in (0..BITS_PER_WORD).step_by(pages) {
                let frame = word_index * BITS_PER_WORD + bit;
                if frame == 0 || frame + pages > self.geo.frame_count {
                    continue;
                }
                let mask = ((1_u64 << pages) - 1) << bit;
                if used & mask == 0 {
                    self.commit_alloc_locked(frame, pages);
                    return Some(PhysAddr::new(frame as u64 * PAGE_SIZE));
                }
            }
        }
        None
    }

    /// Bounded opportunistic 2 MiB allocation. Each candidate is eight bitmap
    /// words; failure never drains CPU caches or performs compaction. The
    /// ordinary 4 KiB allocator retains ownership of every constituent frame.
    fn try_alloc_anon_huge_frame_locked(&mut self) -> Option<PhysAddr> {
        const SEARCH_BLOCKS: usize = 32;
        const WORDS_PER_BLOCK: usize = ANON_HUGE_PAGE_COUNT / BITS_PER_WORD;
        if !self.initialized
            || self.free_frames.saturating_sub(self.min_free_frames) < ANON_HUGE_PAGE_COUNT
        {
            return None;
        }
        // Block zero contains physical page zero, which is never a data frame.
        // Round down so a partial final block is never inspected or returned.
        let blocks = self.geo.frame_count / ANON_HUGE_PAGE_COUNT;
        if blocks <= 1 {
            return None;
        }
        let first = self.huge_next_block.clamp(1, blocks - 1);
        let mut block = first;
        for _ in 0..(blocks - 1).min(SEARCH_BLOCKS) {
            self.huge_next_block = if block + 1 == blocks { 1 } else { block + 1 };
            let first_word = block * WORDS_PER_BLOCK;
            // Non-usable, reserved and cached frames all remain set in `used`.
            // The central lock excludes allocation while these words are read.
            if (first_word..first_word + WORDS_PER_BLOCK)
                .all(|word| self.geo.used.load_word(word) == 0)
            {
                let frame = block * ANON_HUGE_PAGE_COUNT;
                self.commit_alloc_locked(frame, ANON_HUGE_PAGE_COUNT);
                return Some(PhysAddr::new(frame as u64 * PAGE_SIZE));
            }
            block = self.huge_next_block;
        }
        None
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
        let mut available = 0_u64;
        let mut word_index = 0;
        let mut used_word = 0_u64;
        while filled < out.len() {
            if should_fail() {
                crate::debug::warn!(memory, "fault injection: alloc.frame failed");
                break;
            }
            if available == 0 {
                let Some(word) = self.free_word_at_hint_locked(FrameScanKind::Independent) else {
                    break;
                };
                (word_index, used_word, available) = word;
            }
            // Reuse the loaded word for every free bit in it. The old path
            // restarted the usable-range walk and loaded this same word for
            // every frame in a batch. `used` admits only boot-usable frames.
            let bit = available.trailing_zeros() as usize;
            available &= available - 1;
            let before = used_word;
            used_word |= 1_u64 << bit;
            let frame = word_index * BITS_PER_WORD + bit;
            self.store_used_word(word_index, before, used_word);
            self.free_frames -= 1;
            self.next_hint = frame + 1;
            out[filled] = PhysAddr::new(frame as u64 * PAGE_SIZE);
            filled += 1;
        }
        filled
    }

    /// Returns the next allocation word and every usable free bit at or after
    /// its first candidate. Non-usable bits are permanently set in `used`;
    /// the explicit end mask also excludes padding in the last bitmap word.
    fn free_word_at_hint_locked(&mut self, kind: FrameScanKind) -> Option<(usize, u64, u64)> {
        let first = self.find_alloc_start_locked(1, kind)?;
        let word_index = first / BITS_PER_WORD;
        let used = self.geo.used.load_word(word_index);
        let word_end = ((word_index + 1) * BITS_PER_WORD).min(self.geo.frame_count);
        let valid_bits = word_end - word_index * BITS_PER_WORD;
        let valid = u64::MAX >> (BITS_PER_WORD - valid_bits);
        let available = !used & valid & (u64::MAX << (first % BITS_PER_WORD));
        Some((word_index, used, available))
    }

    fn commit_alloc_locked(&mut self, start_frame: usize, page_count: usize) {
        self.mark_range_used(start_frame, page_count);
        self.free_frames = self.free_frames.saturating_sub(page_count);
        self.next_hint = start_frame.saturating_add(page_count);
    }

    fn find_alloc_start_locked(&mut self, page_count: usize, kind: FrameScanKind) -> Option<usize> {
        if !self.initialized || page_count == 0 || page_count > self.free_frames {
            return None;
        }
        let frame_count = self.geo.frame_count;
        if page_count > frame_count {
            return None;
        }

        let last_start_exclusive = frame_count
            .checked_sub(page_count)?
            .saturating_add(1);
        let start_hint = self.next_hint.min(last_start_exclusive.saturating_sub(1));

        self.find_contiguous_free_range(start_hint, last_start_exclusive, page_count, kind)
            .or_else(|| self.find_contiguous_free_range(0, start_hint, page_count, kind))
    }

    fn find_contiguous_free_range(
        &mut self,
        start: usize,
        end: usize,
        page_count: usize,
        kind: FrameScanKind,
    ) -> Option<usize> {
        let mut work = FrameScanWork::default();
        let mut scanned = 0usize;
        let found = self.find_contiguous_free_range_with_work(
            start,
            end,
            page_count,
            &mut scanned,
            &mut work,
        );
        self.record_scan_milestone(scanned, page_count, kind, &work);
        found
    }

    fn find_contiguous_free_range_with_work(
        &self,
        start: usize,
        end: usize,
        page_count: usize,
        scanned: &mut usize,
        work: &mut FrameScanWork,
    ) -> Option<usize> {
        if start >= end || page_count == 0 {
            return None;
        }

        let ranges = &self.geo.usable_ranges[..self.geo.usable_range_count];
        let first_range = ranges.partition_point(|range| range.end_frame() <= start);
        for &range in &ranges[first_range..] {
            if range.start_frame >= end {
                break;
            }
            let range_start = range.start_frame.max(start);
            let range_end = range.end_frame();
            let start_limit_end = end.min(range_end);
            if range_start >= start_limit_end {
                continue;
            }

            let found = if page_count == 1 {
                self.next_free_frame_in_range(range_start, start_limit_end, work)
                    .inspect(|&candidate| {
                        *scanned = scanned.saturating_add(candidate.saturating_sub(range_start) + 1);
                    })
            } else if page_count <= 2 * BITS_PER_WORD - 2 {
                self.find_contiguous_medium_in_range(
                    range_start,
                    start_limit_end,
                    range_end,
                    page_count,
                    scanned,
                    work,
                )
            } else {
                self.find_contiguous_large_in_range(
                    range_start,
                    start_limit_end,
                    range_end,
                    page_count,
                    scanned,
                    work,
                )
            };
            if found.is_some() {
                return found;
            }
        }

        None
    }

    /// Finds the earliest contiguous free run of length `2 <= page_count <= 126`
    /// starting in `[frame, start_limit_end)` and contained in `range_end`.
    /// Uses the order-`floor(log2(page_count))` summary tree so fragmented words
    /// without a qualifying run start are skipped in `O(log N)`.
    fn find_contiguous_medium_in_range(
        &self,
        mut frame: usize,
        start_limit_end: usize,
        range_end: usize,
        page_count: usize,
        scanned: &mut usize,
        work: &mut FrameScanWork,
    ) -> Option<usize> {
        let order = (page_count.ilog2() as usize).min(free_word_index::MAX_RUN_ORDER);
        let min_len = 1_usize << order;
        let words = self.geo.used.words;
        while frame < start_limit_end {
            let first_word = frame / BITS_PER_WORD;
            let Some(word_index) =
                self.free_words
                    .next_word_with_run_order(order, first_word, &mut work.index_words)
            else {
                return None;
            };
            if word_index * BITS_PER_WORD >= start_limit_end {
                return None;
            }
            let search_frame = frame.max(word_index * BITS_PER_WORD);
            *scanned = scanned.saturating_add(search_frame.saturating_sub(frame) + 1);
            work.bitmap_words += 1;
            let free_w = !self.geo.used.load_word(word_index);
            let free_next = if word_index + 1 < words {
                work.bitmap_words += 1;
                !self.geo.used.load_word(word_index + 1)
            } else {
                0
            };
            let start_bit = search_frame % BITS_PER_WORD;
            if page_count <= BITS_PER_WORD {
                let starts = free_word_index::exact_run_starts_in_word(free_w, free_next, page_count)
                    & (u64::MAX << start_bit);
                if starts == 0 {
                    frame = (word_index + 1) * BITS_PER_WORD;
                    continue;
                }
                let candidate = word_index * BITS_PER_WORD + starts.trailing_zeros() as usize;
                if candidate >= start_limit_end || candidate.saturating_add(page_count) > range_end {
                    return None;
                }
                return Some(candidate);
            }
            // `65 <= page_count <= 126`: `min_len == 64`; locate the earliest
            // 64-frame start in `word_index` and verify the remaining tail.
            let starts = free_word_index::exact_run_starts_in_word(free_w, free_next, min_len)
                & (u64::MAX << start_bit);
            if starts == 0 {
                frame = (word_index + 1) * BITS_PER_WORD;
                continue;
            }
            let candidate = word_index * BITS_PER_WORD + starts.trailing_zeros() as usize;
            if candidate >= start_limit_end {
                return None;
            }
            let extra = self.free_run_len_from(
                candidate + min_len,
                range_end,
                page_count - min_len,
                work,
            );
            if min_len + extra == page_count {
                return Some(candidate);
            }
            frame = candidate.saturating_add(min_len + extra).saturating_add(1);
        }
        None
    }

    /// Finds the earliest contiguous free run of length `page_count >= 127`.
    /// Any such run must contain at least `(page_count - 126).div_ceil(64)`
    /// consecutive all-free central words, so non-qualifying islands are skipped
    /// directly via the all-free-run summary index.
    fn find_contiguous_large_in_range(
        &self,
        mut frame: usize,
        start_limit_end: usize,
        range_end: usize,
        page_count: usize,
        scanned: &mut usize,
        work: &mut FrameScanWork,
    ) -> Option<usize> {
        let min_full_words = (page_count - (2 * BITS_PER_WORD - 2)).div_ceil(BITS_PER_WORD);
        let (af_order, threshold_words) = free_word_index::all_free_order_for(min_full_words);
        while frame < start_limit_end {
            let min_w_all = frame.div_ceil(BITS_PER_WORD);
            let Some(w_all) =
                self.free_words
                    .next_all_free_run(af_order, min_w_all, &mut work.index_words)
            else {
                return None;
            };
            let w_all_frame = w_all * BITS_PER_WORD;
            if w_all_frame >= range_end {
                return None;
            }
            let candidate = if w_all > 0 && w_all_frame > frame {
                work.bitmap_words += 1;
                let prev_used = self.geo.used.load_word(w_all - 1);
                let leading_free = prev_used.leading_zeros() as usize;
                (w_all_frame - leading_free).max(frame)
            } else {
                frame
            };
            if candidate >= start_limit_end {
                return None;
            }
            *scanned = scanned.saturating_add(candidate.saturating_sub(frame) + 1);
            let prefix = w_all_frame.saturating_sub(candidate);
            let known_full = (threshold_words * BITS_PER_WORD)
                .min(range_end.saturating_sub(w_all_frame));
            let verified = prefix + known_full;
            if verified >= page_count {
                return Some(candidate);
            }
            let extra = self.free_run_len_from(
                w_all_frame + known_full,
                range_end,
                page_count - verified,
                work,
            );
            if verified + extra == page_count {
                return Some(candidate);
            }
            frame = candidate.saturating_add(verified + extra).saturating_add(1);
        }
        None
    }

    fn next_free_frame_in_range(&self, frame: usize, end: usize, work: &mut FrameScanWork) -> Option<usize> {
        if frame >= end {
            return None;
        }
        let first_word = frame / BITS_PER_WORD;
        work.bitmap_words += 1;
        let available = !self.geo.used.load_word(first_word)
            & (u64::MAX << (frame % BITS_PER_WORD));
        let (word_index, available) = if available != 0 {
            (first_word, available)
        } else {
            let next = self.free_words.next_available(first_word + 1, &mut work.index_words)?;
            if next * BITS_PER_WORD >= end {
                return None;
            }
            work.bitmap_words += 1;
            let available = !self.geo.used.load_word(next);
            assert_ne!(available, 0, "free-word index names a full central word");
            (next, available)
        };
        let candidate = word_index * BITS_PER_WORD + available.trailing_zeros() as usize;
        (candidate < end).then_some(candidate)
    }

    fn free_run_len_from(&self, mut frame: usize, end: usize, limit: usize, work: &mut FrameScanWork) -> usize {
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
            work.bitmap_words += 1;
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

    fn record_scan_milestone(&mut self, scanned_frames: usize, page_count: usize, kind: FrameScanKind, work: &FrameScanWork) {
        let long_distance = scanned_frames >= PHYS_ALLOC_SCAN_MILESTONE_FRAMES;
        if !long_distance && work.bitmap_words + work.index_words < PHYS_ALLOC_SCAN_MILESTONE_WORDS {
            return;
        }
        // Never enter a diagnostic sink while holding the IRQ-off allocator
        // lock. Preserve all qualifying work in plain, centrally owned counts;
        // the normal-context profile drain copies these before rendering.
        let profile = &mut self.scan_profiles[kind as usize - 1];
        profile.scans = profile.scans.saturating_add(1);
        profile.distance_sum = profile.distance_sum.saturating_add(scanned_frames as u64);
        profile.bitmap_words = profile.bitmap_words.saturating_add(work.bitmap_words as u64);
        profile.index_words = profile.index_words.saturating_add(work.index_words as u64);
        if scanned_frames >= profile.max_distance {
            profile.max_distance = scanned_frames;
            profile.sample_pages = page_count;
            profile.sample_free = self.free_frames;
            profile.sample_bitmap_words = work.bitmap_words;
            profile.sample_index_words = work.index_words;
        }
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

    /// Refills a CPU cache one bitmap word at a time. Publish each cached bit
    /// before its used bit, so no window shows an uncached allocated frame.
    fn take_frames_for_cache_locked(&mut self, out: &mut [usize]) -> usize {
        let mut filled = 0;
        while filled < out.len() {
            let Some((word_index, mut used, mut available)) = self.free_word_at_hint_locked(FrameScanKind::CacheRefill) else {
                break;
            };
            while filled < out.len() && available != 0 {
                let bit = available.trailing_zeros() as usize;
                available &= available - 1;
                let frame = word_index * BITS_PER_WORD + bit;
                self.geo.cached.test_and_set(frame);
                let before = used;
                used |= 1_u64 << bit;
                self.store_used_word(word_index, before, used);
                self.free_frames -= 1;
                self.next_hint = frame + 1;
                out[filled] = frame;
                filled += 1;
            }
        }
        filled
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
        // `used`, `cached`, then the multi-order free-run hierarchy.
        let free_index_words = FreeRunIndex::backing_words(bitmap_words)
            .expect("physical free-word index exceeds direct-map geometry");
        let free_index_bytes = free_index_words
            .checked_mul(core::mem::size_of::<u64>())
            .expect("physical free-word index size overflow");
        let bitmap_bytes = used_bitmap_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(free_index_bytes))
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
        // SAFETY: the allocation spans both bitmaps and the index, all zeroed
        // above except `used`, whose initially-full words describe no free frames.
        new_state.geo.cached =
            BitmapView::from_raw(unsafe { bitmap_ptr.add(bitmap_words) }, bitmap_words);
        new_state.free_words = unsafe {
            FreeRunIndex::from_raw(bitmap_ptr.add(bitmap_words * 2), bitmap_words)
        };
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
        }
        new_state.geo.sort_and_merge_usable_ranges();
        for range_index in 0..new_state.geo.usable_range_count {
            let range = new_state.geo.usable_ranges[range_index];
            new_state.mark_range_free(range.start_frame, range.page_count);
            new_state.usable_frames = new_state
                .usable_frames
                .checked_add(range.page_count)
                .expect("usable frame count overflow");
            new_state.free_frames = new_state
                .free_frames
                .checked_add(range.page_count)
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

/// True when any frame of the page-aligned range is boot-usable RAM that the
/// allocator may hand out. Device apertures mapped to user space must answer
/// false, otherwise a user mapping could alias allocator-owned frames.
pub fn range_overlaps_boot_usable(phys_start: u64, byte_len: u64) -> bool {
    irq_safe(|| {
        let state = PHYS_POOL.state.lock();
        range_overlaps_boot_usable_locked(&state, phys_start, byte_len)
    })
}

/// End of the highest boot-usable RAM frame, `None` before the allocator is
/// initialised. Device apertures that a hypervisor places after guest RAM derive
/// their base from this.
pub fn boot_ram_top() -> Option<u64> {
    irq_safe(|| {
        let state = PHYS_POOL.state.lock();
        state
            .initialized
            .then(|| state.geo.frame_count as u64 * PAGE_SIZE)
    })
}

fn range_overlaps_boot_usable_locked(state: &PhysAllocatorState, phys_start: u64, byte_len: u64) -> bool {
    // Fail closed: an uninitialised allocator or malformed range claims overlap.
    if !state.initialized
        || byte_len == 0
        || !phys_start.is_multiple_of(PAGE_SIZE)
        || !byte_len.is_multiple_of(PAGE_SIZE)
    {
        return true;
    }
    let Some(end) = phys_start.checked_add(byte_len) else {
        return true;
    };
    // The allocator deliberately excludes physical page zero from geometry;
    // that does not make it an admissible device aperture.
    if phys_start == 0 {
        return true;
    }
    let first = phys_start / PAGE_SIZE;
    let last = end / PAGE_SIZE;
    let limit = (state.geo.frame_count as u64).min(last);
    if first >= limit {
        return false;
    }
    state.geo.overlaps_boot_usable(first as usize, limit as usize)
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
    irq_safe(|| PHYS_POOL.alloc_contiguous(current_cache_slot(), page_count))
}

pub fn try_free_frame(phys: PhysAddr) -> Result<(), FreeFrameError> {
    let mut failures = [(PhysAddr::new(0), FreeFrameError::AlreadyFree)];
    let frames = [phys];
    match irq_safe(|| PHYS_POOL.release_frames(current_cache_slot(), &frames, &mut failures)) {
        0 => Ok(()),
        _ => Err(failures[0].1),
    }
}

/// Fixed, small anonymous folio order. The PTEs remain 4 KiB; the physical
/// allocation is naturally aligned and the frames can retire independently.
pub(crate) const ANON_FOLIO_PAGES: usize = 16;

/// Number of ordinary physical frames backing one hardware 2 MiB user leaf.
pub(crate) const ANON_HUGE_PAGE_COUNT: usize = 512;

/// A nonblocking user-class allocation for the IRQ-off first-touch path.
/// Fragmentation or contention simply leaves the ordinary fault path in charge.
pub(crate) fn try_alloc_anon_folio() -> Option<PhysAddr> {
    if nucleus_core::util::fault_injection::should_fail("alloc.frame") {
        return None;
    }
    irq_safe(|| PHYS_POOL.try_alloc_anon_folio())
}

/// Tries one naturally aligned, user-class 2 MiB span without waiting on the
/// central lock. Contention, fragmentation and reserve pressure are ordinary
/// fallback outcomes; all returned frames remain individually releasable.
pub(crate) fn try_alloc_anon_huge_frame() -> Option<PhysAddr> {
    if nucleus_core::util::fault_injection::should_fail("alloc.frame") {
        return None;
    }
    irq_safe(|| PHYS_POOL.try_alloc_anon_huge_frame())
}

/// Fills `out` with independent (not necessarily adjacent) user-class frames,
/// no more than the frames above the kernel reserve. A short fill means pressure, not only exhaustion.
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

/// Count of user-class allocations refused or truncated by the watermark.
pub fn user_watermark_denials() -> u64 {
    USER_WATERMARK_DENIALS.load(Ordering::Relaxed)
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

/// Normal-context companion to the batch profile. A contended try-lock leaves
/// every count pending; only a complete copied window is removed from state.
fn drain_frame_scan_profile() -> usize {
    let Some(profiles) = irq_safe(|| {
        let mut state = PHYS_POOL.state.try_lock()?;
        Some(core::mem::replace(&mut state.scan_profiles, [FrameScanProfile::EMPTY; 3]))
    }) else {
        return 0;
    };
    // The central lock is gone and the caller's IRQ state has been restored.
    let classes = [
        (FrameScanKind::Independent, "phys-frame-scan-total", "phys-frame-scan-work"),
        (FrameScanKind::CacheRefill, "phys-cache-scan-total", "phys-cache-scan-work"),
        (FrameScanKind::Contiguous, "phys-run-scan-total", "phys-run-scan-work"),
    ];
    let mut emitted = 0;
    for (profile, (kind, total_name, work_name)) in profiles.into_iter().zip(classes) {
        if profile.scans == 0 {
            continue;
        }
        // Keep the old fields for the maximum-distance sample, but no longer
        // emit a line per scan. The per-class total carries that exact count.
        if profile.max_distance >= PHYS_ALLOC_SCAN_MILESTONE_FRAMES {
            crate::debug::record_milestone(
                crate::debug::LogCategory::Memory, "phys-contig-scan",
                profile.max_distance as u64, profile.sample_free as u64,
            );
            emitted += 1;
        }
        // arg0: kind (1 independent, 2 cache refill, 3 required contiguous) in
        // bits 56..63 and sample request pages in bits 0..55. arg1: this
        // sample's bitmap/index search reads in the high/low 32 bits.
        crate::debug::record_milestone(
            crate::debug::LogCategory::Memory, "phys-scan-word-work",
            ((kind as u64) << 56) | profile.sample_pages as u64,
            ((profile.sample_bitmap_words.min(u32::MAX as usize) as u64) << 32)
                | profile.sample_index_words.min(u32::MAX as usize) as u64,
        );
        crate::debug::record_milestone(
            crate::debug::LogCategory::Memory, total_name, profile.scans, profile.distance_sum,
        );
        crate::debug::record_milestone(
            crate::debug::LogCategory::Memory, work_name, profile.bitmap_words, profile.index_words,
        );
        emitted += 3;
    }
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

        // Physical zero is the absent-root / absent-frame sentinel used by
        // page tables and the pager. Never place metadata or lend a frame there.
        let start = align_up(region.phys_start, PAGE_SIZE)?
            .clamp(PAGE_SIZE, DIRECT_MAP_PHYS_LIMIT);
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
        bitmap.fill(u64::MAX);
        let mut state = PhysAllocatorState::new();
        state.initialized = true;
        state.geo.frame_count = page_count;
        state.geo.used = BitmapView::from_raw(bitmap.as_mut_ptr(), bitmap.len());
        let summary = alloc::boxed::Box::leak(
            alloc::vec![0_u64; FreeRunIndex::backing_words(bitmap.len()).unwrap()].into_boxed_slice(),
        );
        // SAFETY: leaked zeroed test backing is exclusively owned by this state.
        state.free_words = unsafe { FreeRunIndex::from_raw(summary.as_mut_ptr(), bitmap.len()) };
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

    pub(super) fn assert_index_matches_bitmap(state: &PhysAllocatorState) {
        state.free_words.assert_matches_bitmap(state.geo.used);
    }

    #[test]
    fn freed_island_does_not_rescan_the_intervening_full_bitmap_words() {
        const FRAMES: usize = 400_129;
        const LIVE_PREFIX: usize = 380_000;
        let mut bitmap = alloc::vec![u64::MAX; FRAMES.div_ceil(BITS_PER_WORD)];
        let mut state = test_state(&mut bitmap, FRAMES, 0);
        state.claim_fixed_range_locked(0, LIVE_PREFIX as u64 * PAGE_SIZE).unwrap();
        state.free_frame_locked(PhysAddr::new(7 * PAGE_SIZE)).unwrap();
        let mut one = [PhysAddr::new(0)];
        assert_eq!(state.alloc_frames_locked(&mut one), 1);
        assert_eq!(one[0].as_u64(), 7 * PAGE_SIZE);

        let mut work = FrameScanWork::default();
        assert_eq!(state.next_free_frame_in_range(8, FRAMES, &mut work), Some(LIVE_PREFIX));
        assert_eq!(work.bitmap_words, 2);
        assert!(work.index_words <= 7);
        assert_eq!(state.alloc_frames_locked(&mut one), 1);
        assert_eq!(one[0].as_u64(), LIVE_PREFIX as u64 * PAGE_SIZE);
    }

    #[test]
    fn indexed_contiguous_search_matches_required_runs_with_fragmentation_and_wrap() {
        const FRAMES: usize = 137;
        for pattern in 0..32 {
            let mut bitmap = [u64::MAX; FRAMES.div_ceil(BITS_PER_WORD)];
            let mut state = test_state(&mut bitmap, FRAMES, (pattern * 41) % FRAMES);
            for frame in 0..FRAMES {
                if (frame * 17 + pattern * 13) % 23 < 7 {
                    state.reserve_phys_range(frame as u64 * PAGE_SIZE, PAGE_SIZE);
                }
            }
            assert_index_matches_bitmap(&state);
            for pages in 1..=70 {
                let last = FRAMES - pages + 1;
                let hint = state.next_hint.min(last - 1);
                let expected = (hint..last).chain(0..hint)
                    .find(|start| (*start..*start + pages).all(|frame| !state.is_used(frame)));
                assert_eq!(state.find_alloc_start_locked(pages, FrameScanKind::Contiguous), expected);
            }
        }
    }

    #[test]
    fn scan_profile_keeps_work_totals_and_exact_max_distance_sample_without_rendering() {
        let mut bitmap = [u64::MAX; 4];
        let mut state = test_state(&mut bitmap, 256, 0);
        state.record_scan_milestone(380_000, 1, FrameScanKind::Independent,
            &FrameScanWork { bitmap_words: 2, index_words: 7 });
        state.record_scan_milestone(70_000, 1, FrameScanKind::Independent,
            &FrameScanWork { bitmap_words: 2, index_words: 3 });
        state.record_scan_milestone(8, 64, FrameScanKind::Contiguous,
            &FrameScanWork { bitmap_words: 1100, index_words: 2 });
        let ordinary = state.scan_profiles[FrameScanKind::Independent as usize - 1];
        assert_eq!((ordinary.scans, ordinary.distance_sum), (2, 450_000));
        assert_eq!((ordinary.bitmap_words, ordinary.index_words), (4, 10));
        assert_eq!((ordinary.max_distance, ordinary.sample_free), (380_000, 256));
        assert_eq!((ordinary.sample_bitmap_words, ordinary.sample_index_words), (2, 7));
        let contiguous = state.scan_profiles[FrameScanKind::Contiguous as usize - 1];
        assert_eq!((contiguous.scans, contiguous.sample_pages), (1, 64));
        assert_eq!((contiguous.bitmap_words, contiguous.index_words), (1100, 2));
        assert_eq!(state.scan_profiles[FrameScanKind::CacheRefill as usize - 1].scans, 0);
    }

    #[test]
    fn anonymous_folio_is_aligned_owned_and_individually_releasable() {
        let mut words = [0_u64; 4];
        let mut state = test_state(&mut words, 256, 17);
        assert_eq!(state.reserve_phys_range(ANON_FOLIO_PAGES as u64 * PAGE_SIZE, PAGE_SIZE), 1);
        let head = state.try_alloc_anon_folio_locked().unwrap();
        let start = (head.as_u64() / PAGE_SIZE) as usize;
        assert_eq!(start, 32);
        assert!((start..start + ANON_FOLIO_PAGES).all(|p| state.is_used(p)));
        state.free_frame_locked(PhysAddr::new(head.as_u64() + PAGE_SIZE)).unwrap();
        assert!(!state.is_used(start + 1));
        assert!(state.is_used(start) && state.is_used(start + 2));
    }

    #[test]
    fn anonymous_folio_bounds_the_search_and_preserves_the_watermark() {
        let mut words = [0_u64; 16];
        let mut state = test_state(&mut words, 1024, 0);
        for frame in (0..512).step_by(ANON_FOLIO_PAGES) {
            assert_eq!(state.reserve_phys_range(frame as u64 * PAGE_SIZE, PAGE_SIZE), 1);
        }
        let before = state.free_frames;
        assert!(state.try_alloc_anon_folio_locked().is_none());
        assert_eq!(state.free_frames, before);
        state.next_hint = 512;
        state.min_free_frames = before - ANON_FOLIO_PAGES + 1;
        assert!(state.try_alloc_anon_folio_locked().is_none());
        assert_eq!(state.free_frames, before);
        state.min_free_frames -= 1;
        assert_eq!(state.try_alloc_anon_folio_locked(), Some(PhysAddr::new(512 * PAGE_SIZE)));
        assert_eq!(state.free_frames, state.min_free_frames);
    }

    #[test]
    fn anonymous_huge_frame_is_aligned_and_keeps_independent_frame_ownership() {
        let mut words = [0_u64; 32];
        let mut state = test_state(&mut words, 2048, 17);
        state.reserve_phys_range((ANON_HUGE_PAGE_COUNT as u64 + 7) * PAGE_SIZE, PAGE_SIZE);
        let before = state.free_frames;
        let head = state.try_alloc_anon_huge_frame_locked().unwrap();
        assert_eq!(head.as_u64(), 2 * ANON_HUGE_PAGE_COUNT as u64 * PAGE_SIZE);
        assert_eq!(state.free_frames, before - ANON_HUGE_PAGE_COUNT);
        let start = (head.as_u64() / PAGE_SIZE) as usize;
        assert!((start..start + ANON_HUGE_PAGE_COUNT).all(|frame| state.is_used(frame)));
        state.free_frame_locked(PhysAddr::new(head.as_u64() + 137 * PAGE_SIZE)).unwrap();
        assert!(!state.is_used(start + 137));
        assert!(state.is_used(start + 136) && state.is_used(start + 138));
        assert_eq!(state.free_frames, before - ANON_HUGE_PAGE_COUNT + 1);
    }

    #[test]
    fn anonymous_huge_scan_is_bounded_and_advances_past_fragmented_blocks() {
        let mut words = [0_u64; 35 * 8];
        let mut state = test_state(&mut words, 35 * ANON_HUGE_PAGE_COUNT, 73);
        for block in 1..=32 {
            state.reserve_phys_range((block * ANON_HUGE_PAGE_COUNT) as u64 * PAGE_SIZE, PAGE_SIZE);
        }
        let before = state.free_frames;
        assert_eq!(state.try_alloc_anon_huge_frame_locked(), None);
        assert_eq!(state.free_frames, before);
        assert_eq!(state.next_hint, 73, "a failed probe must not disturb small allocations");
        assert_eq!(state.huge_next_block, 33);
        assert_eq!(state.try_alloc_anon_huge_frame_locked(),
            Some(PhysAddr::new(33 * ANON_HUGE_PAGE_COUNT as u64 * PAGE_SIZE)));
    }

    #[test]
    fn anonymous_huge_probe_honors_watermark_and_rejects_incomplete_tail() {
        let mut words = [0_u64; 24];
        let mut state = test_state(&mut words, 1535, 0);
        let before = state.free_frames;
        state.min_free_frames = before - ANON_HUGE_PAGE_COUNT + 1;
        assert_eq!(state.try_alloc_anon_huge_frame_locked(), None);
        assert_eq!(state.free_frames, before);
        state.min_free_frames -= 1;
        assert_eq!(state.try_alloc_anon_huge_frame_locked(), Some(PhysAddr::new(512 * PAGE_SIZE)));
        assert_eq!(state.free_frames, state.min_free_frames);
        state.min_free_frames = 0;
        assert_eq!(state.try_alloc_anon_huge_frame_locked(), None,
            "block zero and an incomplete final block are never huge candidates");
    }

    #[test]
    fn device_aperture_overlap_check_is_fail_closed() {
        let mut words = [0_u64; 4];
        let state = test_state(&mut words, 256, 0);
        let page = PAGE_SIZE;
        assert!(range_overlaps_boot_usable_locked(&state, 0, page));
        assert!(range_overlaps_boot_usable_locked(&state, 255 * page, 4 * page));
        assert!(!range_overlaps_boot_usable_locked(&state, 256 * page, 4 * page));
        assert!(range_overlaps_boot_usable_locked(&state, 3, page));
        assert!(range_overlaps_boot_usable_locked(&state, 0, 0));
        assert!(range_overlaps_boot_usable_locked(&state, u64::MAX - page + 1, 2 * page));
        let mut uninit = test_state(&mut words, 256, 0);
        uninit.initialized = false;
        assert!(range_overlaps_boot_usable_locked(&uninit, 512 * page, page));
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
    fn firmware_usable_page_zero_is_never_allocatable() {
        let regions = [BootMemoryRegion {
            phys_start: 0,
            page_count: 3,
            kind: BootMemoryKind::Usable,
            _reserved0: 0,
        }];
        let spans: alloc::vec::Vec<_> = usable_region_spans(&regions).collect();
        assert_eq!(spans, [(PAGE_SIZE, 2)]);
        assert_eq!(find_usable_span_excluding_ranges(&regions, 1, &[]), Some(PAGE_SIZE));
        assert_eq!(find_usable_span_excluding_ranges(&regions, 3, &[]), None);
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

        // `last_start_exclusive = 5` clamps `start_hint = 6` to `4`, taking `[4..8)`.
        let first = state
            .alloc_contiguous_locked(4)
            .expect("contiguous allocation within pool limit should succeed");
        assert_eq!(first.as_u64(), PAGE_SIZE * 4);
        assert_eq!(state.free_frames, 4);

        // Split the remaining [0..4) range into [0] and [2..4) so no 3-frame run fits.
        assert_eq!(state.claim_fixed_range_locked(PAGE_SIZE, PAGE_SIZE), Ok(()));
        assert_eq!(state.free_frames, 3);
        assert_eq!(state.alloc_contiguous_locked(3), None);
        assert_eq!(state.free_frames, 3);

        let second = state
            .alloc_contiguous_locked(2)
            .expect("remaining 2-frame run within limit should succeed");
        assert_eq!(second.as_u64(), PAGE_SIZE * 2);
        assert_eq!(state.free_frames, 1);
        assert_eq!(state.alloc_contiguous_locked(2), None);

        let last = state
            .alloc_contiguous_locked(1)
            .expect("final free frame within limit should succeed");
        assert_eq!(last.as_u64(), 0);
        assert_eq!(state.free_frames, 0);
        assert_eq!(state.alloc_contiguous_locked(1), None);
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
    fn batch_word_walk_handles_fragmentation_wrap_and_partial_final_word() {
        let mut bitmap = [0_u64; 3];
        let mut state = test_state(&mut bitmap, 149, 130);
        for frame in (0..149).step_by(7) {
            state.reserve_phys_range(frame as u64 * PAGE_SIZE, PAGE_SIZE);
        }
        let expected = state.free_frames;
        let mut out = [PhysAddr::new(0); 154];
        assert_eq!(state.alloc_frames_locked_with_fault_gate(&mut out, || false), expected);
        assert_eq!(state.free_frames, 0);
        let mut seen = [false; 149];
        for frame in &out[..expected] {
            let index = (frame.as_u64() / PAGE_SIZE) as usize;
            assert!(index < 149 && !index.is_multiple_of(7));
            assert!(!seen[index], "batch returned one physical frame twice");
            seen[index] = true;
        }
        for (frame, seen) in seen.into_iter().enumerate() {
            assert_eq!(seen, !frame.is_multiple_of(7));
        }
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

    #[test]
    fn usable_ranges_sort_merge_and_binary_search_match_linear_oracle() {
        let mut geo = Geometry::new();
        // Insert unsorted, overlapping, adjacent, and gapped ranges (up to MAX_USABLE_RANGES).
        let raw = [
            (500, 40),
            (10, 20),
            (30, 15),   // adjacent to [10, 30) -> merges to [10, 45)
            (25, 30),   // overlaps [10, 45) -> merges to [10, 55)
            (1000, 1),
            (100, 10),
            (510, 50),  // overlaps [500, 540) -> merges to [500, 560)
            (500, 0),   // empty ignored
        ];
        for (idx, &(start_frame, page_count)) in raw.iter().enumerate() {
            geo.usable_ranges[idx] = UsableFrameRange {
                start_frame,
                page_count,
            };
        }
        geo.usable_range_count = raw.len();
        geo.sort_and_merge_usable_ranges();

        assert_eq!(geo.usable_range_count, 4);
        let merged = &geo.usable_ranges[..geo.usable_range_count];
        assert_eq!(
            merged.iter().map(|r| (r.start_frame, r.page_count)).collect::<alloc::vec::Vec<_>>(),
            alloc::vec![(10, 45), (100, 10), (500, 60), (1000, 1)]
        );

        let oracle_usable = |f: usize| {
            raw.iter()
                .any(|&(s, n)| n > 0 && (s..s + n).contains(&f))
        };
        for f in 0..1024 {
            assert_eq!(geo.is_boot_usable(f), oracle_usable(f), "frame {f}");
        }
        for start in [0, 9, 10, 54, 55, 99, 100, 109, 110, 499, 500, 559, 560, 999, 1000, 1001] {
            for len in [0, 1, 2, 10, 50, 200] {
                let end = start + len;
                let expected_overlap = (start..end).any(oracle_usable);
                let expected_contains = (start..end).all(oracle_usable);
                assert_eq!(
                    geo.overlaps_boot_usable(start, end),
                    expected_overlap,
                    "overlap [{start}, {end})"
                );
                assert_eq!(
                    geo.contains_boot_usable_range(start, end),
                    expected_contains,
                    "contains [{start}, {end})"
                );
            }
        }

        // Stress MAX_USABLE_RANGES disjoint ranges inserted in reverse order.
        let mut many = Geometry::new();
        for i in 0..MAX_USABLE_RANGES {
            let rev = MAX_USABLE_RANGES - 1 - i;
            many.usable_ranges[i] = UsableFrameRange {
                start_frame: rev * 16 + 4,
                page_count: 8,
            };
        }
        many.usable_range_count = MAX_USABLE_RANGES;
        many.sort_and_merge_usable_ranges();
        assert_eq!(many.usable_range_count, MAX_USABLE_RANGES);
        for f in 0..(MAX_USABLE_RANGES * 16 + 16) {
            let expected = (f % 16) >= 4 && (f % 16) < 12 && f < MAX_USABLE_RANGES * 16;
            assert_eq!(many.is_boot_usable(f), expected, "128-range frame {f}");
        }
    }

    #[test]
    fn fragmented_multi_page_contiguous_search_skips_islands_with_bounded_reads() {
        const FRAMES: usize = 400_128;
        let words = FRAMES.div_ceil(BITS_PER_WORD);
        let mut bitmap = alloc::vec![u64::MAX; words];
        let mut state = test_state(&mut bitmap, FRAMES, 0);

        // Fill the first 380,000 frames with alternating used/free single frames
        // (190,000 isolated 1-frame free islands — the worst case for linear hopping).
        for w in 0..(380_000 / BITS_PER_WORD) {
            let before = state.geo.used.load_word(w);
            state.store_used_word(w, before, 0x5555_5555_5555_5555);
        }
        // Leave one qualifying 512-page run starting at an unaligned boundary (380,003).
        let target_start = 380_003;
        let target_len = 512;
        state.mark_range_used(380_000, FRAMES - 380_000);
        state.mark_range_free(target_start, target_len);
        // Also place sub-threshold 256-page and 63-page islands before target_start
        // (starting at odd frames so the preceding even frame in 0x5555.. is used,
        // and sealing the trailing frame so island lengths are exact).
        state.mark_range_free(64_001, 63);
        state.set_used(64_001 + 63);
        state.mark_range_free(128_001, 256);
        state.set_used(128_001 + 256);

        for (pages, expected, max_bitmap_reads, max_index_reads) in [
            (2, 64_001, 4, 16),
            (3, 64_001, 4, 16),
            (4, 64_001, 4, 16),
            (16, 64_001, 4, 16),
            (32, 64_001, 4, 16),
            (63, 64_001, 4, 16),
            (64, 128_001, 4, 16),
            (127, 128_001, 4, 16),
            (256, 128_001, 8, 16),
            (257, target_start, 16, 32),
            (512, target_start, 16, 32),
        ] {
            let mut work = FrameScanWork::default();
            let mut scanned = 0;
            let found = state.find_contiguous_free_range_with_work(
                0,
                FRAMES - pages + 1,
                pages,
                &mut scanned,
                &mut work,
            );
            assert_eq!(found, Some(expected), "pages={pages}");
            assert!(
                work.bitmap_words <= max_bitmap_reads,
                "pages={pages} read too many bitmap words: {}",
                work.bitmap_words
            );
            assert!(
                work.index_words <= max_index_reads,
                "pages={pages} read too many index words: {}",
                work.index_words
            );
        }
    }

    #[test]
    fn multi_page_contiguous_search_matches_oracle_across_large_and_unaligned_runs() {
        const FRAMES: usize = 1600;
        let words = FRAMES.div_ceil(BITS_PER_WORD);
        let mut bitmap = alloc::vec![u64::MAX; words];
        let mut state = test_state(&mut bitmap, FRAMES, 0);
        // Carve non-trivial runs spanning word boundaries, including unaligned
        // 63, 64, 65, 126, 127, 128, 255, 256, 512-page runs.
        state.mark_range_used(0, FRAMES);
        for (start, len) in [
            (3, 2),
            (62, 5),      // crosses word 0 -> 1
            (129, 64),    // crosses word 2 -> 3
            (255, 126),   // crosses word 3 -> 5 (max without guaranteed full word)
            (449, 127),   // crosses word 7 -> 9 (1 full word)
            (703, 256),   // unaligned 256-page run
            (1027, 512),  // unaligned 512-page run (exactly 7 full words + edges)
        ] {
            state.mark_range_free(start, len);
        }
        state.free_frames = (0..FRAMES).filter(|&f| !state.is_used(f)).count();
        assert_index_matches_bitmap(&state);

        for hint in [0, 1, 63, 64, 256, 450, 704, 1028, 1500] {
            state.next_hint = hint;
            for pages in [1, 2, 3, 4, 5, 7, 16, 32, 63, 64, 65, 126, 127, 128, 190, 191, 255, 256, 319, 511, 512, 513] {
                let last = FRAMES - pages + 1;
                let clamped = hint.min(last - 1);
                let expected = (clamped..last)
                    .chain(0..clamped)
                    .find(|start| (*start..*start + pages).all(|frame| !state.is_used(frame)));
                assert_eq!(
                    state.find_alloc_start_locked(pages, FrameScanKind::Contiguous),
                    expected,
                    "hint={hint} pages={pages}"
                );
            }
        }
    }
}
