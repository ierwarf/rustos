//! Per-CPU physical-frame caches (magazines) in front of the central bitmap.
//!
//! - **Owner:** `kernel-mm` physical allocator; `phys.rs` owns the bitmap and
//!   the frame lifecycle, this module owns the cache tier and its accounting.
//! - **Boundary:** Frame indexes enter a cache only after the same release
//!   validation the central path applies; a cache never mints or hides a frame.
//! - **Lifecycle:** A cached frame is used in the central bitmap, set in the
//!   lock-free `cached` bitmap, and still free memory. It leaves the cache by
//!   allocation, spill at the high mark, or a whole-cache drain.
//! - **Concurrency:** Lock order is cache slot -> central allocator, one slot
//!   at a time. A frame moves central <-> cache only with both locks held.
//!   `free = central + sum(slot counts)` is what the watermark and the memory
//!   pressure reads use.
//! - **Failure:** Exhaustion is reported only after every cache was drained;
//!   a duplicate release is `AlreadyFree` whether the frame is cached, free,
//!   or racing through another CPU's slot.
//! - **Forbidden:** No nested slot locks, no callback or logging beyond the
//!   fault-injection warning under the locks, no frame outside the bitmaps.
//! - **Evidence:** `physical-frame-lifecycle`.
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS, TrackedSpinLock};
use spin::Once;
use x86_64::PhysAddr;

use super::{FixedRangeClaimError, FreeFrameError, Geometry, PAGE_SIZE, PhysAllocatorState};

/// Per-CPU frame cache slots; one per tracked CPU.
pub(super) const FRAME_CACHE_SLOTS: usize = MAX_TRACKED_CPUS;
/// Hard capacity of one magazine; `cache_high` never exceeds it.
const FRAME_CACHE_CAPACITY: usize = 128;
/// Refill/spill batch bounds. Linux sizes `pcp->batch` at roughly one 1024th
/// of the zone, capped near 256 KiB (64 pages) and then quartered, and keeps
/// `high` a small multiple of it; this is the same shape scaled to this
/// allocator: a batch amortizes one central-lock hold over `batch` frames,
/// while `high` bounds how much memory idles in caches.
const FRAME_CACHE_MIN_BATCH: usize = 4;
const FRAME_CACHE_MAX_BATCH: usize = 32;
const FRAME_CACHE_BATCH_DIVISOR: usize = 4096;
const FRAME_CACHE_HIGH_FACTOR: usize = 4;
/// Whole-cache drains an allocation attempts before it reports exhaustion.
const FRAME_CACHE_DRAIN_ROUNDS: usize = 2;
const _: () = assert!(FRAME_CACHE_MAX_BATCH * FRAME_CACHE_HIGH_FACTOR <= FRAME_CACHE_CAPACITY);

/// Refill batch and spill-at-high mark for a machine with `usable_frames`.
pub(super) const fn frame_cache_params(usable_frames: usize) -> (usize, usize) {
    let scaled = usable_frames / FRAME_CACHE_BATCH_DIVISOR;
    let batch = if scaled < FRAME_CACHE_MIN_BATCH {
        FRAME_CACHE_MIN_BATCH
    } else if scaled > FRAME_CACHE_MAX_BATCH {
        FRAME_CACHE_MAX_BATCH
    } else {
        scaled
    };
    (batch, batch * FRAME_CACHE_HIGH_FACTOR)
}

type CentralLock = TrackedSpinLock<PhysAllocatorState, { LockClass::PhysicalAllocator as u8 }>;
type CacheLock = TrackedSpinLock<FrameCache, { LockClass::PhysicalFrameCache as u8 }>;

pub(super) static PHYS_POOL: PhysPool = PhysPool::new();

/// One CPU's magazine: a bounded LIFO of frame indexes.
///
/// Every frame here is marked used in the central bitmap and set in the
/// `cached` bitmap. It is free memory that merely sits closer to its CPU.
struct FrameCache {
    len: usize,
    frames: [usize; FRAME_CACHE_CAPACITY],
}

impl FrameCache {
    const fn new() -> Self {
        Self {
            len: 0,
            frames: [0; FRAME_CACHE_CAPACITY],
        }
    }

    fn push(&mut self, frame: usize) {
        assert!(self.len < FRAME_CACHE_CAPACITY, "frame cache overflow");
        self.frames[self.len] = frame;
        self.len += 1;
    }

    fn pop(&mut self) -> Option<usize> {
        self.len = self.len.checked_sub(1)?;
        Some(self.frames[self.len])
    }
}

/// A cache plus a lock-free copy of its length, padded to its own line so one
/// CPU's traffic never bounces another CPU's slot.
#[repr(align(64))]
struct CacheSlot {
    /// Mirror of `cache.len`, written only under the slot lock. Readers (the
    /// memory-pressure reads and the watermark) tolerate a stale value.
    count: AtomicUsize,
    cache: CacheLock,
}

impl CacheSlot {
    const fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            cache: TrackedSpinLock::new(FrameCache::new()),
        }
    }

    fn publish_count(&self, cache: &FrameCache) {
        // ORDERING: Relaxed; the value is advisory for lock-free readers, and
        // every decision that must be exact re-reads under the locks.
        self.count.store(cache.len, Ordering::Relaxed);
    }
}

/// Counters for the slow paths; they never sit on the cache-hit path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameCacheStats {
    pub refills: u64,
    pub spills: u64,
    pub drains: u64,
}

enum AllocPass {
    Done,
    /// The request is admissible but other CPUs' caches hold the frames.
    NeedDrain,
}

/// The physical allocator: a central bitmap behind one lock, fronted by one
/// bounded frame cache per CPU.
///
/// Lock order is `cache slot -> central`, and a thread holds at most one slot,
/// so no path nests two slots. `free_total` = `central_free` + the slot
/// counts, and that sum is what the watermark, the OOM watchdog and the
/// pressure reads see. A frame moves central <-> cache only with both locks
/// held, so the sum changes only by real allocation and release.
pub(super) struct PhysPool {
    pub(super) state: CentralLock,
    /// Published once, after the state is installed.
    geo: Once<Geometry>,
    /// Mirror of `state.free_frames`, stored under the central lock.
    central_free: AtomicUsize,
    /// Mirror of `state.min_free_frames`, stored when the state is installed.
    pub(super) min_free: AtomicUsize,
    slots: [CacheSlot; FRAME_CACHE_SLOTS],
    refills: AtomicU64,
    spills: AtomicU64,
    drains: AtomicU64,
}

impl PhysPool {
    pub(super) const fn new() -> Self {
        Self {
            state: TrackedSpinLock::new(PhysAllocatorState::new()),
            geo: Once::new(),
            central_free: AtomicUsize::new(0),
            min_free: AtomicUsize::new(0),
            slots: [const { CacheSlot::new() }; FRAME_CACHE_SLOTS],
            refills: AtomicU64::new(0),
            spills: AtomicU64::new(0),
            drains: AtomicU64::new(0),
        }
    }

    fn geometry(&self) -> Option<&Geometry> {
        self.geo.get()
    }

    fn slot(&self, index: usize) -> &CacheSlot {
        &self.slots[index % FRAME_CACHE_SLOTS]
    }

    /// Publishes a fully built state. Callers hold the central lock.
    pub(super) fn install_locked(
        &self,
        guard: &mut PhysAllocatorState,
        new_state: PhysAllocatorState,
    ) {
        *guard = new_state;
        let geo = guard.geo;
        // ORDERING: Relaxed; `Once::call_once` below releases both mirrors to
        // every reader that can observe an initialized geometry.
        self.central_free
            .store(guard.free_frames, Ordering::Relaxed);
        self.min_free
            .store(guard.min_free_frames, Ordering::Relaxed);
        self.geo.call_once(|| geo);
    }

    /// Runs `f` under the central lock and republishes the free-count mirror.
    fn with_central<R>(&self, f: impl FnOnce(&mut PhysAllocatorState) -> R) -> R {
        let mut state = self.state.lock();
        let result = f(&mut state);
        // ORDERING: Relaxed; advisory mirror, exact value re-read under lock.
        self.central_free
            .store(state.free_frames, Ordering::Relaxed);
        result
    }

    fn cached_elsewhere(&self, own_slot: usize) -> usize {
        let own = own_slot % FRAME_CACHE_SLOTS;
        self.slots
            .iter()
            .enumerate()
            // ORDERING: Relaxed; slot mirrors are advisory counts.
            .map(|(index, slot)| {
                if index == own {
                    0
                } else {
                    slot.count.load(Ordering::Relaxed)
                }
            })
            .sum()
    }

    pub(super) fn cached_total(&self) -> usize {
        // ORDERING: Relaxed; advisory slot mirrors.
        self.slots
            .iter()
            .map(|slot| slot.count.load(Ordering::Relaxed))
            .sum()
    }

    /// Free frames: central plus cached. Exact when the allocator is quiescent;
    /// a concurrent central<->cache move can make it read high by at most one
    /// batch because the two mirrors are stored one after the other.
    pub(super) fn free_total(&self) -> usize {
        // ORDERING: Relaxed; advisory mirror.
        self.central_free.load(Ordering::Relaxed) + self.cached_total()
    }

    pub(super) fn stats(&self) -> FrameCacheStats {
        // ORDERING: Relaxed; monotonic diagnostics.
        FrameCacheStats {
            refills: self.refills.load(Ordering::Relaxed),
            spills: self.spills.load(Ordering::Relaxed),
            drains: self.drains.load(Ordering::Relaxed),
        }
    }

    /// Fills `out` with single frames. `user` applies the kernel-reserve
    /// watermark. `should_fail` is consulted before every frame and a `true`
    /// stops the fill (fault injection).
    pub(super) fn alloc_frames(
        &self,
        slot: usize,
        out: &mut [PhysAddr],
        user: bool,
        mut should_fail: impl FnMut() -> bool,
    ) -> usize {
        let Some(geo) = self.geometry() else {
            return 0;
        };
        let mut filled = 0;
        let mut drains = 0;
        loop {
            match self.alloc_pass(slot, geo, out, &mut filled, user, &mut should_fail) {
                AllocPass::Done => return filled,
                AllocPass::NeedDrain => {
                    if drains == FRAME_CACHE_DRAIN_ROUNDS {
                        return filled;
                    }
                    drains += 1;
                    self.drain_all();
                }
            }
        }
    }

    fn alloc_pass(
        &self,
        slot_index: usize,
        geo: &Geometry,
        out: &mut [PhysAddr],
        filled: &mut usize,
        user: bool,
        should_fail: &mut impl FnMut() -> bool,
    ) -> AllocPass {
        let slot = self.slot(slot_index);
        let mut cache = slot.cache.lock();

        // Stage 1: serve from this CPU's cache without touching the central
        // lock. A user request may use a cached frame only while the free
        // memory it can see leaves the reserve intact even if every other CPU
        // makes the same decision concurrently (`FRAME_CACHE_SLOTS * high`
        // frames of slack); closer to the reserve the exact locked path below
        // decides.
        if cache.len > 0 {
            let reserve = if user {
                // ORDERING: Relaxed; immutable after install.
                self.min_free.load(Ordering::Relaxed) + FRAME_CACHE_SLOTS * geo.cache_high
            } else {
                0
            };
            // ORDERING: Relaxed; a stale mirror can only be conservative or
            // off by a batch, which the slack above covers.
            let central = self.central_free.load(Ordering::Relaxed);
            while *filled < out.len() && cache.len > 0 && central + cache.len > reserve {
                let injected = should_fail();
                if injected {
                    crate::debug::warn!(memory, "fault injection: alloc.frame failed");
                    slot.publish_count(&cache);
                    return AllocPass::Done;
                }
                let Some(frame) = cache.pop() else { break };
                geo.cached.clear(frame);
                out[*filled] = PhysAddr::new(frame as u64 * PAGE_SIZE);
                *filled += 1;
            }
            slot.publish_count(&cache);
            if *filled == out.len() {
                return AllocPass::Done;
            }
        }

        // Stage 2: the central allocator, under slot -> central order.
        let mut state = self.state.lock();
        let need = out.len() - *filled;
        let cached = cache.len + self.cached_elsewhere(slot_index);
        let faulted = core::cell::Cell::new(false);
        let mut gate = || {
            let hit = should_fail();
            if hit {
                faulted.set(true);
            }
            hit
        };
        let range = *filled..out.len();
        let admissible = if user {
            (state.free_frames + cached).saturating_sub(state.min_free_frames)
        } else {
            usize::MAX
        };
        let serve = need.min(admissible);
        let from_central = if user {
            // Counts the watermark denial itself.
            state.alloc_user_frames_locked(&mut out[range.clone()], cached, &mut gate)
        } else {
            state.alloc_frames_locked_with_fault_gate(&mut out[range.clone()], &mut gate)
        };
        *filled += from_central;
        let mut served = from_central;
        // The central set ran dry below the admissible count: this CPU's own
        // cache still holds frames the watermark already admitted.
        while !faulted.get() && served < serve && cache.len > 0 {
            if gate() {
                break;
            }
            let Some(frame) = cache.pop() else { break };
            geo.cached.clear(frame);
            out[*filled] = PhysAddr::new(frame as u64 * PAGE_SIZE);
            *filled += 1;
            served += 1;
        }
        let satisfied = *filled == out.len();
        let short_on_cached_elsewhere = !faulted.get() && served < serve;

        // Refill, only when memory is comfortably above the reserve so the
        // caches never hoard the last frames.
        if satisfied && !faulted.get() {
            let room = geo.cache_high.saturating_sub(cache.len);
            let want = geo.cache_batch.min(room);
            if want != 0 && state.free_frames > state.min_free_frames + 2 * geo.cache_batch {
                let mut taken = 0;
                while taken < want {
                    let Some(frame) = state.take_frame_for_cache_locked() else {
                        break;
                    };
                    cache.push(frame);
                    taken += 1;
                }
                if taken != 0 {
                    // ORDERING: Relaxed; diagnostics.
                    self.refills.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // central -> cache: raise the cache count first, then lower the
        // central mirror, so the free total never reads low.
        slot.publish_count(&cache);
        // ORDERING: Relaxed; advisory mirror.
        self.central_free
            .store(state.free_frames, Ordering::Relaxed);
        if short_on_cached_elsewhere {
            AllocPass::NeedDrain
        } else {
            AllocPass::Done
        }
    }

    /// Moves a whole slot's cache back to the central bitmap.
    pub(super) fn drain_slot(&self, slot_index: usize) {
        let Some(geo) = self.geometry() else { return };
        let slot = self.slot(slot_index);
        // ORDERING: Relaxed; a miss only skips frames pushed concurrently.
        if slot.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut cache = slot.cache.lock();
        if cache.len == 0 {
            return;
        }
        let mut state = self.state.lock();
        while let Some(frame) = cache.pop() {
            state.return_to_central_locked(frame);
            geo.cached.clear(frame);
        }
        // cache -> central: raise the central mirror first.
        // ORDERING: Relaxed; advisory mirror.
        self.central_free
            .store(state.free_frames, Ordering::Relaxed);
        slot.publish_count(&cache);
        // ORDERING: Relaxed; diagnostics.
        self.drains.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns every cached frame to the central bitmap, one slot at a time.
    pub(super) fn drain_all(&self) {
        for index in 0..FRAME_CACHE_SLOTS {
            self.drain_slot(index);
        }
    }

    /// Releases `frames`, recording rejections into `failures`.
    pub(super) fn release_frames(
        &self,
        slot_index: usize,
        frames: &[PhysAddr],
        failures: &mut [(PhysAddr, FreeFrameError)],
    ) -> usize {
        let Some(geo) = self.geometry() else {
            for (slot, &phys) in failures.iter_mut().zip(frames) {
                *slot = (phys, FreeFrameError::OutsideAllocatorRange);
            }
            return frames.len();
        };
        // A bulk release (address-space teardown) is cheaper as one central
        // hold than as a stream of cache spills.
        if frames.len() >= geo.cache_high {
            return self.with_central(|state| state.free_frames_locked(frames, failures));
        }
        let slot = self.slot(slot_index);
        let mut cache = slot.cache.lock();
        let mut failed = 0;
        for &phys in frames {
            if let Err(err) = self.release_to_cache(geo, slot, &mut cache, phys) {
                if let Some(entry) = failures.get_mut(failed) {
                    *entry = (phys, err);
                }
                failed += 1;
            }
        }
        slot.publish_count(&cache);
        failed
    }

    fn release_to_cache(
        &self,
        geo: &Geometry,
        slot: &CacheSlot,
        cache: &mut FrameCache,
        phys: PhysAddr,
    ) -> Result<(), FreeFrameError> {
        let frame = geo.validate_release(phys)?;
        // The `cached` bit is the single arbiter between concurrent releases
        // of one frame: only the CPU that sets it proceeds.
        if geo.cached.test_and_set(frame) {
            return Err(FreeFrameError::AlreadyFree);
        }
        if !geo.used.get(frame) {
            geo.cached.clear(frame);
            return Err(FreeFrameError::AlreadyFree);
        }
        if cache.len >= geo.cache_high {
            self.spill(geo, slot, cache);
        }
        cache.push(frame);
        Ok(())
    }

    /// Returns the newest `batch` frames of a full cache to the central set.
    fn spill(&self, geo: &Geometry, slot: &CacheSlot, cache: &mut FrameCache) {
        let mut state = self.state.lock();
        for _ in 0..geo.cache_batch {
            let Some(frame) = cache.pop() else { break };
            state.return_to_central_locked(frame);
            geo.cached.clear(frame);
        }
        // ORDERING: Relaxed; advisory mirror (cache -> central: central first).
        self.central_free
            .store(state.free_frames, Ordering::Relaxed);
        slot.publish_count(cache);
        // ORDERING: Relaxed; diagnostics.
        self.spills.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn alloc_contiguous(
        &self,
        slot: usize,
        page_count: usize,
        below: usize,
    ) -> Option<PhysAddr> {
        if page_count == 1 && below == usize::MAX {
            let mut out = [PhysAddr::new(0)];
            return (self.alloc_frames(slot, &mut out, false, || false) == 1).then_some(out[0]);
        }
        for round in 0..=FRAME_CACHE_DRAIN_ROUNDS {
            if let Some(found) =
                self.with_central(|state| state.alloc_contiguous_bounded_locked(page_count, below))
            {
                return Some(found);
            }
            // Cached frames fragment the central set and are free memory a
            // contiguous run may need.
            if round == FRAME_CACHE_DRAIN_ROUNDS || self.cached_total() == 0 {
                return None;
            }
            self.drain_all();
        }
        None
    }

    pub(super) fn claim_fixed_range(
        &self,
        phys_start: u64,
        byte_len: u64,
    ) -> Result<(), FixedRangeClaimError> {
        for round in 0..=FRAME_CACHE_DRAIN_ROUNDS {
            let claimed =
                self.with_central(|state| state.claim_fixed_range_locked(phys_start, byte_len));
            if claimed != Err(FixedRangeClaimError::AlreadyOwned)
                || round == FRAME_CACHE_DRAIN_ROUNDS
                || self.cached_total() == 0
            {
                return claimed;
            }
            // A cached frame reads as used; it is free, so release it and look
            // again before declaring the range owned.
            self.drain_all();
        }
        Err(FixedRangeClaimError::AlreadyOwned)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::memory::phys::tests::test_state;
    use crate::memory::phys::{BITS_PER_WORD, BitmapView};

    /// A leaked pool over `frames` frames. Leaking keeps the bitmap alive for
    /// the `'static` views and lets scoped threads share it.
    fn test_pool(frames: usize, batch: usize, high: usize, min_free: usize) -> &'static PhysPool {
        let words = frames.div_ceil(BITS_PER_WORD);
        let storage: &'static mut [u64] =
            alloc::boxed::Box::leak(alloc::vec![0_u64; words * 2].into_boxed_slice());
        let (used, cached) = storage.split_at_mut(words);
        let cached_view = BitmapView::from_raw(cached.as_mut_ptr(), words);
        let mut state = test_state(used, frames, 0);
        state.geo.cached = cached_view;
        state.geo.cache_batch = batch;
        state.geo.cache_high = high;
        state.min_free_frames = min_free;
        let pool: &'static PhysPool =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(PhysPool::new()));
        let mut guard = pool.state.lock();
        pool.install_locked(&mut guard, state);
        drop(guard);
        pool
    }

    fn alloc_one(pool: &PhysPool, slot: usize, user: bool) -> Option<PhysAddr> {
        let mut out = [PhysAddr::new(0)];
        (pool.alloc_frames(slot, &mut out, user, || false) == 1).then_some(out[0])
    }

    fn free_one(pool: &PhysPool, slot: usize, frame: PhysAddr) -> Result<(), FreeFrameError> {
        let mut failures = [(PhysAddr::new(0), FreeFrameError::AlreadyFree)];
        match pool.release_frames(slot, &[frame], &mut failures) {
            0 => Ok(()),
            _ => Err(failures[0].1),
        }
    }

    /// Every bit in both bitmaps is clear and the counts agree: the allocator
    /// is back to its freshly initialized state.
    fn assert_fully_free(pool: &PhysPool, frames: usize) {
        pool.drain_all();
        assert_eq!(pool.cached_total(), 0);
        assert_eq!(pool.free_total(), frames);
        let state = pool.state.lock();
        assert_eq!(state.free_frames, frames);
        for frame in 0..frames {
            assert!(!state.is_used(frame), "frame {frame} leaked in used bitmap");
            assert!(!state.geo.cached.get(frame), "frame {frame} stuck cached");
        }
    }

    #[test]
    fn cache_params_scale_with_memory_and_stay_bounded() {
        assert_eq!(
            frame_cache_params(0),
            (FRAME_CACHE_MIN_BATCH, FRAME_CACHE_MIN_BATCH * 4)
        );
        assert_eq!(frame_cache_params(64 * 1024).0, 16);
        let (batch, high) = frame_cache_params(usize::MAX / 2);
        assert_eq!(batch, FRAME_CACHE_MAX_BATCH);
        assert!(high <= FRAME_CACHE_CAPACITY);
    }

    #[test]
    fn single_frame_traffic_hits_the_cache_after_one_refill() {
        let pool = test_pool(512, 8, 32, 0);
        let mut held = alloc::vec::Vec::new();
        for _ in 0..8 {
            held.push(alloc_one(pool, 0, false).unwrap());
        }
        // One refill served all eight; the central lock saw one acquisition's
        // worth of work and the rest stayed in the magazine.
        assert_eq!(pool.stats().refills, 1);
        assert_eq!(pool.free_total(), 512 - 8);
        assert!(pool.cached_total() > 0);
        for frame in held {
            free_one(pool, 0, frame).unwrap();
        }
        assert_eq!(pool.free_total(), 512);
        assert_eq!(pool.stats().refills, 1, "frees must not force a refill");
        assert_fully_free(pool, 512);
    }

    #[test]
    fn free_total_is_exact_through_refill_spill_and_drain() {
        let pool = test_pool(256, 8, 32, 0);
        let mut held = alloc::vec::Vec::new();
        for n in 1..=100 {
            held.push(alloc_one(pool, 1, false).unwrap());
            assert_eq!(pool.free_total(), 256 - n);
        }
        for (n, frame) in held.drain(..).enumerate() {
            free_one(pool, 1, frame).unwrap();
            assert_eq!(pool.free_total(), 256 - 100 + n + 1);
        }
        assert!(
            pool.stats().spills > 0,
            "100 frees overflow a 32-frame magazine"
        );
        assert!(pool.cached_total() <= 32);
        assert_fully_free(pool, 256);
    }

    #[test]
    fn double_free_is_rejected_in_cache_and_after_drain() {
        let pool = test_pool(128, 8, 32, 0);
        let frame = alloc_one(pool, 0, false).unwrap();
        free_one(pool, 0, frame).unwrap();
        // Parked in slot 0's magazine; a second release - even through
        // another CPU's slot - must still be rejected.
        assert_eq!(free_one(pool, 0, frame), Err(FreeFrameError::AlreadyFree));
        assert_eq!(free_one(pool, 3, frame), Err(FreeFrameError::AlreadyFree));
        pool.drain_all();
        assert_eq!(free_one(pool, 0, frame), Err(FreeFrameError::AlreadyFree));
        // A frame that was never allocated, an unaligned and an out-of-range one.
        assert_eq!(
            free_one(pool, 0, PhysAddr::new(100 * PAGE_SIZE)),
            Err(FreeFrameError::AlreadyFree)
        );
        assert_eq!(
            free_one(pool, 0, PhysAddr::new(PAGE_SIZE + 8)),
            Err(FreeFrameError::NonPageAligned)
        );
        assert_eq!(
            free_one(pool, 0, PhysAddr::new(4096 * PAGE_SIZE)),
            Err(FreeFrameError::OutsideAllocatorRange)
        );
        assert_fully_free(pool, 128);
    }

    #[test]
    fn user_watermark_counts_cached_frames_and_is_exact_at_exhaustion() {
        let pool = test_pool(256, 8, 32, 40);
        let mut held = alloc::vec::Vec::new();
        // The reserve is measured against central + cached, so user allocation
        // yields exactly `frames - reserve` even though most of the free
        // frames sit in magazines and the fast path backs off near the reserve.
        while let Some(frame) = alloc_one(pool, 0, true) {
            held.push(frame);
        }
        assert_eq!(held.len(), 256 - 40);
        assert_eq!(pool.free_total(), 40);
        // Kernel class reaches the reserve, wherever its frames are parked.
        let mut kernel = alloc::vec::Vec::new();
        while let Some(frame) = alloc_one(pool, 5, false) {
            kernel.push(frame);
        }
        assert_eq!(kernel.len(), 40);
        assert_eq!(pool.free_total(), 0);
        for frame in held.into_iter().chain(kernel) {
            free_one(pool, 2, frame).unwrap();
        }
        assert_fully_free(pool, 256);
    }

    #[test]
    fn exhaustion_is_reported_only_after_other_cpus_caches_are_drained() {
        let pool = test_pool(128, 8, 32, 0);
        // Slot 0 parks frames in its magazine.
        let parked: alloc::vec::Vec<_> = (0..10)
            .map(|_| alloc_one(pool, 0, false).unwrap())
            .collect();
        for frame in parked {
            free_one(pool, 0, frame).unwrap();
        }
        assert!(pool.slot(0).count.load(Ordering::Relaxed) > 0);
        // Slot 1 can still take every frame in the system.
        let mut all = alloc::vec::Vec::new();
        while let Some(frame) = alloc_one(pool, 1, false) {
            all.push(frame);
        }
        assert_eq!(all.len(), 128);
        assert!(pool.stats().drains > 0);
        let mut unique: alloc::vec::Vec<u64> = all.iter().map(|f| f.as_u64()).collect();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 128);
        for frame in all {
            free_one(pool, 1, frame).unwrap();
        }
        assert_fully_free(pool, 128);
    }

    #[test]
    fn contiguous_allocation_and_fixed_claims_see_cached_frames_as_free() {
        let pool = test_pool(64, 8, 32, 0);
        let held = alloc_one(pool, 0, false).unwrap();
        assert!(pool.cached_total() > 0);
        // 63 contiguous frames exist only if the magazine's frames count.
        let run = pool
            .alloc_contiguous(0, 63, usize::MAX)
            .expect("cached frames must be reclaimed");
        assert_ne!(run.as_u64(), held.as_u64());
        assert_eq!(pool.free_total(), 0);
        for offset in 0..63 {
            free_one(pool, 0, PhysAddr::new(run.as_u64() + offset * PAGE_SIZE)).unwrap();
        }
        free_one(pool, 0, held).unwrap();
        assert_fully_free(pool, 64);

        // A fixed claim over frames a magazine holds must succeed, and the
        // claimed frames must never come back out of a cache.
        let first = alloc_one(pool, 0, false).unwrap();
        assert!(pool.cached_total() > 0);
        assert_eq!(
            pool.claim_fixed_range(PAGE_SIZE * 40, PAGE_SIZE * 2),
            Ok(())
        );
        assert_eq!(
            pool.claim_fixed_range(PAGE_SIZE * 40, PAGE_SIZE),
            Err(FixedRangeClaimError::AlreadyOwned)
        );
        let mut rest = alloc::vec::Vec::new();
        while let Some(frame) = alloc_one(pool, 1, false) {
            rest.push(frame.as_u64() / PAGE_SIZE);
        }
        assert!(!rest.contains(&40) && !rest.contains(&41));
        assert_eq!(rest.len(), 64 - 2 - 1);
        drop(first);
    }

    #[test]
    fn bulk_release_bypasses_the_cache_and_still_detects_duplicates() {
        let pool = test_pool(256, 8, 32, 0);
        let mut frames = alloc::vec::Vec::new();
        let mut out = [PhysAddr::new(0); 64];
        assert_eq!(pool.alloc_frames(0, &mut out, false, || false), 64);
        frames.extend_from_slice(&out);
        let cached_before = pool.cached_total();
        let mut failures = [(PhysAddr::new(0), FreeFrameError::AlreadyFree); 4];
        // 64 >= high: one central hold, no spill traffic.
        assert_eq!(pool.release_frames(0, &frames, &mut failures), 0);
        assert_eq!(pool.cached_total(), cached_before);
        assert_eq!(pool.stats().spills, 0);
        assert_eq!(pool.release_frames(0, &frames, &mut failures), 64);
        assert_eq!(failures[0].1, FreeFrameError::AlreadyFree);
        assert_fully_free(pool, 256);
    }

    #[test]
    fn fault_gate_stops_a_cached_fill_without_losing_frames() {
        let pool = test_pool(128, 8, 32, 0);
        // Warm the magazine so the fault lands on the cache-hit path.
        let warm = alloc_one(pool, 0, false).unwrap();
        free_one(pool, 0, warm).unwrap();
        let mut out = [PhysAddr::new(0); 6];
        let mut calls = 0;
        let filled = pool.alloc_frames(0, &mut out, false, || {
            calls += 1;
            calls == 3
        });
        assert_eq!(filled, 2);
        assert_eq!(pool.free_total(), 128 - 2);
        for frame in &out[..filled] {
            free_one(pool, 0, *frame).unwrap();
        }
        assert_fully_free(pool, 128);
    }

    #[test]
    fn uninitialized_pool_refuses_without_panicking() {
        let pool: &'static PhysPool =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(PhysPool::new()));
        let mut out = [PhysAddr::new(0); 2];
        assert_eq!(pool.alloc_frames(0, &mut out, true, || false), 0);
        assert_eq!(
            free_one(pool, 0, PhysAddr::new(0)),
            Err(FreeFrameError::OutsideAllocatorRange)
        );
        assert_eq!(pool.alloc_contiguous(0, 1, usize::MAX), None);
        assert_eq!(pool.free_total(), 0);
    }

    /// Threads hammer a small pool across shared and private slots, with user
    /// and kernel classes, batches and singles, bulk and cached releases. An
    /// ownership table proves no frame is ever handed to two holders, and the
    /// closing audit proves nothing leaked.
    #[test]
    fn threaded_stress_never_duplicates_or_leaks_a_frame() {
        use std::sync::atomic::AtomicBool;
        const FRAMES: usize = 1024;
        const THREADS: usize = 8;
        const ROUNDS: usize = 4000;
        let pool = test_pool(FRAMES, 8, 32, 48);
        let owners: alloc::vec::Vec<AtomicBool> =
            (0..FRAMES).map(|_| AtomicBool::new(false)).collect();
        let owners = &owners;
        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                scope.spawn(move || {
                    // Threads 0..4 own slots 0..4; threads 4..8 share slot 0..4
                    // too, so slot locks are contended as well as CPU-private.
                    let slot = thread % 4;
                    let mut seed = 0x9E37_79B9_7F4A_7C15_u64 ^ (thread as u64 + 1);
                    let mut next = move || {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed
                    };
                    let mut held: alloc::vec::Vec<PhysAddr> = alloc::vec::Vec::new();
                    for _ in 0..ROUNDS {
                        let r = next();
                        if r % 5 < 3 && held.len() < 96 {
                            let want = 1 + (r >> 8) as usize % if r % 7 == 0 { 70 } else { 3 };
                            let mut out = [PhysAddr::new(0); 70];
                            let user = (r >> 20) % 2 == 0;
                            let got = pool.alloc_frames(slot, &mut out[..want], user, || false);
                            for frame in &out[..got] {
                                let index = (frame.as_u64() / PAGE_SIZE) as usize;
                                // ORDERING: AcqRel pairs with the Release hand-back.
                                assert!(
                                    !owners[index].swap(true, Ordering::AcqRel),
                                    "frame {index} handed out twice"
                                );
                                held.push(*frame);
                            }
                        } else if !held.is_empty() {
                            let count = (1 + (r >> 8) as usize % if r % 11 == 0 { 70 } else { 4 })
                                .min(held.len());
                            let batch: alloc::vec::Vec<PhysAddr> =
                                held.drain(held.len() - count..).collect();
                            for frame in &batch {
                                // ORDERING: Release publishes the hand-back.
                                owners[(frame.as_u64() / PAGE_SIZE) as usize]
                                    .store(false, Ordering::Release);
                            }
                            let mut failures = [(PhysAddr::new(0), FreeFrameError::AlreadyFree); 4];
                            assert_eq!(pool.release_frames(slot, &batch, &mut failures), 0);
                        }
                        if r % 997 == 0 {
                            pool.drain_slot((r >> 4) as usize % 4);
                        }
                    }
                    let mut failures = [(PhysAddr::new(0), FreeFrameError::AlreadyFree); 4];
                    for frame in &held {
                        // ORDERING: Release publishes the hand-back.
                        owners[(frame.as_u64() / PAGE_SIZE) as usize]
                            .store(false, Ordering::Release);
                    }
                    assert_eq!(pool.release_frames(slot, &held, &mut failures), 0);
                });
            }
        });
        assert_fully_free(pool, FRAMES);
        assert!(pool.stats().refills > 0 && pool.stats().spills > 0);
    }
}
