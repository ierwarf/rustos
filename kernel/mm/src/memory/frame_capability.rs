//! One-shot, generation-bound frame grants for user-pager replies.
//!
//! - **Owner:** `kernel-mm` owns physical frames and grant custody; pagerd may
//!   return only the opaque handle created for the exact kernel fault.
//! - **Boundary:** A grant binds fault, process/MM/VMA, pager epoch, and rights.
//! - **Lifecycle:** Allocate and zero, publish one grant, then either consume
//!   exactly once into a PTE transaction or cancel and return the frame.
//! - **Concurrency:** The bounded registry uses all-atomic slot publication;
//!   normal allocation is outside that registry and fault-time claim never
//!   waits on an allocator or a raw lock.
//! - **Failure:** Malformed, stale, exhausted, or mismatched grants fail closed
//!   without exposing a physical address or aliasing a reused slot.
//! - **Forbidden:** No physical-frame-number wire authority, generation wrap,
//!   W+X grant, duplicate consume, or frame leak on publication failure.
//! - **Evidence:** Exact unit tests and `pager-frame-grant-*` mutations.

use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use x86_64::PhysAddr;

use super::{kernel_vm, phys};

/// Opaque grant slots. One per ring0 fault slot.
///
/// `kernel-compat` static-asserts this against `PAGER_MAX_FRAME_GRANTS` in the
/// shared ABI; `kernel-mm` stays free of the user ABI on purpose.
pub const MAX_PAGER_FRAME_GRANTS: usize = 128;
/// Wired, pre-zeroed frames the fault path may consume without allocating.
///
/// This equals the ring0 fault-slot count, not a smaller independent number,
/// and that relation is the progress condition for demand paging. A fault slot
/// holds one reserve frame from reservation until its reply consumes or
/// cancels it, so a reserve smaller than the slot table can run dry *while
/// slots are still free* - an exhaustion with no admission point to refuse at.
/// Ring0 then returns `UserFaultDisposition::Unhandled` for a perfectly valid
/// non-present fault and the process dies with a SIGSEGV that names nothing.
///
/// That is not hypothetical. With 64 frames behind 128 slots, making the fault
/// path a direct rendezvous cut the housekeeping turns that had been quietly
/// doing the replenishing; the reserve drained, and the first visible symptom
/// was a dead user thread, an absent devmgrd endpoint and a failed boot - not
/// "pager reserve exhausted".
///
/// With this relation the reserve can only be empty after fault-slot admission
/// has already refused, and that refusal is counted and named.
pub const MAX_PREALLOCATED_PAGER_FAULT_FRAMES: usize = 2048;

const _: () = assert!(
    MAX_PREALLOCATED_PAGER_FAULT_FRAMES >= MAX_PAGER_FRAME_GRANTS,
    "a reserved frame always occupies a grant slot, so the reserve may not be larger than it can publish"
);
const FRAME_BYTES: usize = 4096;
const HANDLE_INDEX_BITS: u32 = 16;
const HANDLE_INDEX_MASK: u64 = (1_u64 << HANDLE_INDEX_BITS) - 1;
const HANDLE_MAX_GENERATION: u64 = u64::MAX >> HANDLE_INDEX_BITS;
const FRAME_RIGHT_READ: u64 = 1 << 0;
const FRAME_RIGHT_WRITE: u64 = 1 << 1;
const FRAME_RIGHT_EXECUTE: u64 = 1 << 2;
const FRAME_RIGHT_KNOWN: u64 = FRAME_RIGHT_READ | FRAME_RIGHT_WRITE | FRAME_RIGHT_EXECUTE;
static FIRST_FAULT_FRAME_REPLENISHMENT: AtomicBool = AtomicBool::new(false);
/// Set by the IRQ-off consumer once the wired reserve crosses its low-water
/// mark.  The producer consumes this at an ordinary scheduled kernel-task
/// boundary; the bit is only a wake reason, never fault authority.
static PAGER_FAULT_REFILL_REQUESTED: AtomicBool = AtomicBool::new(false);
const PAGER_FAULT_REFILL_LOW_WATERMARK: usize = MAX_PREALLOCATED_PAGER_FAULT_FRAMES * 3 / 4;
/// Frames one producer pass may publish before it yields.
///
/// Refilling the whole pool in a single turn is what a producer *wants*, and it
/// is exactly wrong: every frame costs an allocator acquisition and a 4 KiB
/// zeroing, so a full 2048-frame pass is 8 MiB of memset holding its CPU for
/// about a millisecond. Measured, that showed up as a 1.2 ms p99 on a probe
/// that does one `mmap` and one `munmap` and takes no fault at all - the
/// producer was simply in front of it. The request bit survives a partial
/// pass, so a bounded budget costs more turns and no throughput, while keeping
/// the producer off the tail of unrelated work.
const PAGER_FAULT_REFILL_PASS_BUDGET: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameGrantBinding {
    pub fault_token: u64,
    pub process_generation: u64,
    pub mm_generation: u64,
    pub vma_generation: u64,
    pub pager_epoch: u64,
}

impl FrameGrantBinding {
    const fn is_canonical(self) -> bool {
        self.fault_token != 0
            && self.process_generation != 0
            && self.mm_generation != 0
            && self.vma_generation != 0
            && self.pager_epoch != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameGrantError {
    Malformed,
    Pressure,
    Stale,
    OutOfFrames,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameGrant {
    frame_phys: u64,
    binding: FrameGrantBinding,
    rights: u64,
    from_fault_pool: bool,
}

struct FrameGrantSlot {
    state: AtomicU8,
    generation: AtomicU64,
    frame_phys: AtomicU64,
    fault_token: AtomicU64,
    process_generation: AtomicU64,
    mm_generation: AtomicU64,
    vma_generation: AtomicU64,
    pager_epoch: AtomicU64,
    rights: AtomicU64,
    from_fault_pool: AtomicU8,
}

impl FrameGrantSlot {
    const fn empty() -> Self {
        Self {
            state: AtomicU8::new(FRAME_GRANT_FREE),
            generation: AtomicU64::new(0),
            frame_phys: AtomicU64::new(0),
            fault_token: AtomicU64::new(0),
            process_generation: AtomicU64::new(0),
            mm_generation: AtomicU64::new(0),
            vma_generation: AtomicU64::new(0),
            pager_epoch: AtomicU64::new(0),
            rights: AtomicU64::new(0),
            from_fault_pool: AtomicU8::new(0),
        }
    }
}

const FRAME_GRANT_FREE: u8 = 0;
const FRAME_GRANT_PUBLISHING: u8 = 1;
const FRAME_GRANT_LIVE: u8 = 2;
const FRAME_GRANT_CLAIMED: u8 = 3;

const FAULT_FRAME_POOL_COLD: u8 = 0;
const FAULT_FRAME_POOL_FILLING: u8 = 1;
const FAULT_FRAME_POOL_READY: u8 = 2;

struct FaultFramePool {
    state: AtomicU8,
    frames: [AtomicU64; MAX_PREALLOCATED_PAGER_FAULT_FRAMES],
    /// Frames currently published in `frames`, maintained incrementally.
    ///
    /// This is authority, not a census: a claimer decrements it before it may
    /// take a slot, so it is what lets an empty pool be rejected in one atomic
    /// operation instead of a full sweep of the array.
    available: AtomicUsize,
    /// Rotating scan origin, so concurrent claimers start on different slots.
    cursor: AtomicUsize,
    /// Frames left at the most recent reservation, the lowest depth any
    /// reservation has observed, and reservations refused for want of a frame.
    ///
    /// The low-water mark is the point: a reserve that reaches zero once has
    /// already failed a fault, and an average hides that. `usize::MAX` means
    /// no reservation has happened yet. These live on the pool rather than in
    /// statics so a test owns its own census.
    depth: AtomicUsize,
    low_watermark: AtomicUsize,
    exhaustions: AtomicU64,
}

impl FaultFramePool {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(FAULT_FRAME_POOL_COLD),
            frames: [const { AtomicU64::new(0) }; MAX_PREALLOCATED_PAGER_FAULT_FRAMES],
            available: AtomicUsize::new(0),
            cursor: AtomicUsize::new(0),
            depth: AtomicUsize::new(usize::MAX),
            low_watermark: AtomicUsize::new(usize::MAX),
            exhaustions: AtomicU64::new(0),
        }
    }

    fn publish(&self, frame_phys: u64) -> Result<(), FrameGrantError> {
        if frame_phys == 0 || !frame_phys.is_multiple_of(FRAME_BYTES as u64) {
            return Err(FrameGrantError::Malformed);
        }
        for slot in &self.frames {
            // ORDERING: a Release publication makes a fully zeroed physical
            // page visible before an exception-time AcqRel reservation reads it.
            if slot
                .compare_exchange(0, frame_phys, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                // ORDERING: Release makes the frame visible in its slot before
                // the count that authorizes a claimer to go looking for it.
                self.available.fetch_add(1, Ordering::Release);
                return Ok(());
            }
        }
        Err(FrameGrantError::Pressure)
    }

    /// Claims one wired frame, or reports the pool empty.
    ///
    /// This runs on every anonymous fault with interrupts disabled, so its cost
    /// is paid by every page the system demand-pages. It used to sweep the
    /// whole slot array on *every* call - once to find a frame and then again
    /// to recount the survivors for the depth census - which made the fault
    /// path O(pool size) and put a hard ceiling on how large the pool could
    /// usefully be. The available count is now maintained incrementally, so a
    /// claim reserves its slot before touching the array and an empty pool is
    /// rejected without touching it at all.
    fn reserve(&self) -> Result<u64, FrameGrantError> {
        // ORDERING: Ready acquires the completed boot fill before any fault
        // can observe a slot; a cold or filling pool is not usable authority.
        if self.state.load(Ordering::Acquire) != FAULT_FRAME_POOL_READY {
            return Err(FrameGrantError::Pressure);
        }
        // Reserve the right to one frame first. Winning this decrement is what
        // guarantees the scan below finds a slot, so the scan never has to run
        // to completion to prove the pool is empty.
        // ORDERING: AcqRel serializes concurrent claims against each other and
        // against a producer's Release increment.
        let remaining = match self.available.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |available| available.checked_sub(1),
        ) {
            Ok(previous) => previous - 1,
            Err(_) => {
                // The exhaustion this counter exists to make legible. Its
                // previous first symptom was a dead user thread several layers
                // away.
                let empty = self.exhaustions.fetch_add(1, Ordering::Relaxed) + 1;
                if empty.is_multiple_of(1024) || empty == 1 {
                    crate::debug::record_milestone(
                        crate::debug::LogCategory::Memory,
                        "pager-pressure-fault-frame-reserve-empty",
                        empty,
                        MAX_PREALLOCATED_PAGER_FAULT_FRAMES as u64,
                    );
                }
                // ORDERING: Release publishes the demand signal for the
                // producer task; an empty pool is exactly when it must run.
                PAGER_FAULT_REFILL_REQUESTED.store(true, Ordering::Release);
                return Err(FrameGrantError::OutOfFrames);
            }
        };
        // A rotating start keeps concurrent claimers off each other's slots
        // instead of having every CPU contend on index 0.
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        let mut reserved = 0;
        for offset in 0..MAX_PREALLOCATED_PAGER_FAULT_FRAMES {
            let index = start.wrapping_add(offset) % MAX_PREALLOCATED_PAGER_FAULT_FRAMES;
            // ORDERING: AcqRel claims one exact wired frame and prevents a
            // second fault from receiving the same opaque grant backing.
            let frame_phys = self.frames[index].swap(0, Ordering::AcqRel);
            if frame_phys != 0 {
                reserved = frame_phys;
                break;
            }
        }
        if reserved == 0 {
            // The count promised a frame the array did not hold. Give the
            // reservation back rather than leaving the pool permanently short.
            // ORDERING: Release restores the count for the next claimer.
            self.available.fetch_add(1, Ordering::Release);
            return Err(FrameGrantError::OutOfFrames);
        }
        self.record_depth(remaining);
        if remaining <= PAGER_FAULT_REFILL_LOW_WATERMARK {
            // ORDERING: Release publishes the low-water observation after the
            // exact frame reservation.  The timer only uses this as a reason
            // to enter a safe scheduler boundary; it never performs refill
            // allocation in IRQ context.
            PAGER_FAULT_REFILL_REQUESTED.store(true, Ordering::Release);
        }
        Ok(reserved)
    }

    fn has_empty_slot(&self) -> bool {
        // ORDERING: Acquire observes either a producer's completed Release
        // increment or a consumer's AcqRel claim. Retry state only, never
        // frame authority.
        self.available.load(Ordering::Acquire) < MAX_PREALLOCATED_PAGER_FAULT_FRAMES
    }

    fn record_depth(&self, remaining: usize) {
        // ORDERING: Relaxed throughout. These are diagnostics; no authority
        // reads them, and a lost update costs one sample of a monotone floor.
        self.depth.store(remaining, Ordering::Relaxed);
        let mut low = self.low_watermark.load(Ordering::Relaxed);
        while remaining < low {
            match self.low_watermark.compare_exchange_weak(
                low,
                remaining,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => low = observed,
            }
        }
    }

    fn observed_depth(&self) -> Option<usize> {
        match self.depth.load(Ordering::Relaxed) {
            usize::MAX => None,
            depth => Some(depth),
        }
    }

    fn observed_low_watermark(&self) -> Option<usize> {
        match self.low_watermark.load(Ordering::Relaxed) {
            usize::MAX => None,
            depth => Some(depth),
        }
    }

    fn return_frame(&self, frame_phys: u64) -> bool {
        // ORDERING: Acquire observes Ready only after its boot-time frame
        // publications, preventing a cancellation from racing a partial fill.
        if self.state.load(Ordering::Acquire) != FAULT_FRAME_POOL_READY {
            return false;
        }
        self.publish(frame_phys).is_ok()
    }
}

struct FrameGrantTable {
    slots: [FrameGrantSlot; MAX_PAGER_FRAME_GRANTS],
}

impl FrameGrantTable {
    const fn new() -> Self {
        Self {
            slots: [const { FrameGrantSlot::empty() }; MAX_PAGER_FRAME_GRANTS],
        }
    }

    fn publish(
        &self,
        frame_phys: u64,
        binding: FrameGrantBinding,
        rights: u64,
        from_fault_pool: bool,
    ) -> Result<u64, FrameGrantError> {
        if frame_phys == 0
            || !frame_phys.is_multiple_of(FRAME_BYTES as u64)
            || !binding.is_canonical()
            || rights == 0
            || rights & !FRAME_RIGHT_KNOWN != 0
            || (rights & FRAME_RIGHT_WRITE != 0 && rights & FRAME_RIGHT_EXECUTE != 0)
        {
            return Err(FrameGrantError::Malformed);
        }
        for (index, slot) in self.slots.iter().enumerate() {
            // ORDERING: claiming Free with Acquire excludes a competing
            // publisher before this writer computes or replaces the slot's
            // non-reusable generation and payload.
            if slot
                .state
                .compare_exchange(
                    FRAME_GRANT_FREE,
                    FRAME_GRANT_PUBLISHING,
                    // ORDERING: this acquire owns the free slot before a
                    // publisher may derive its next generation.
                    Ordering::Acquire,
                    // ORDERING: a losing publisher observes no payload and
                    // retries another bounded slot without synchronization.
                    Ordering::Relaxed,
                )
                .is_err()
            {
                continue;
            }
            // ORDERING: only the publisher that owns Publishing may read the
            // prior generation; Live readers cannot observe a partial update.
            let Some(generation) = slot.generation.load(Ordering::Relaxed).checked_add(1) else {
                // ORDERING: Release ends the private Publishing interval; no
                // reader may have observed payload from this rejected slot.
                slot.state.store(FRAME_GRANT_FREE, Ordering::Release);
                continue;
            };
            if generation > HANDLE_MAX_GENERATION {
                // ORDERING: Release makes this exhausted attempt visible as
                // Free while its non-wrapping generation remains retained.
                slot.state.store(FRAME_GRANT_FREE, Ordering::Release);
                continue;
            }
            let Some(handle) = encode_handle(index, generation) else {
                // ORDERING: Release abandons only this unpublished slot;
                // callers cannot observe an invalid handle or payload.
                slot.state.store(FRAME_GRANT_FREE, Ordering::Release);
                continue;
            };
            slot.frame_phys.store(frame_phys, Ordering::Relaxed);
            slot.fault_token
                .store(binding.fault_token, Ordering::Relaxed);
            slot.process_generation
                .store(binding.process_generation, Ordering::Relaxed);
            slot.mm_generation
                .store(binding.mm_generation, Ordering::Relaxed);
            slot.vma_generation
                .store(binding.vma_generation, Ordering::Relaxed);
            slot.pager_epoch
                .store(binding.pager_epoch, Ordering::Relaxed);
            slot.rights.store(rights, Ordering::Relaxed);
            slot.from_fault_pool
                .store(u8::from(from_fault_pool), Ordering::Relaxed);
            slot.generation.store(generation, Ordering::Relaxed);
            // ORDERING: the live Release publishes every grant field; a claim
            // acquires this state before it compares any authority field.
            slot.state.store(FRAME_GRANT_LIVE, Ordering::Release);
            return Ok(handle);
        }
        Err(FrameGrantError::Pressure)
    }

    fn take(
        &self,
        handle: u64,
        binding: FrameGrantBinding,
        required_rights: u64,
    ) -> Result<FrameGrant, FrameGrantError> {
        let (index, generation) = decode_handle(handle).ok_or(FrameGrantError::Stale)?;
        let slot = self.slots.get(index).ok_or(FrameGrantError::Stale)?;
        // ORDERING: acquiring Live pairs with publication's final Release so
        // all following relaxed payload observations describe one grant.
        if slot.state.load(Ordering::Acquire) != FRAME_GRANT_LIVE
            || slot.generation.load(Ordering::Relaxed) != generation
            || slot.fault_token.load(Ordering::Relaxed) != binding.fault_token
            || slot.process_generation.load(Ordering::Relaxed) != binding.process_generation
            || slot.mm_generation.load(Ordering::Relaxed) != binding.mm_generation
            || slot.vma_generation.load(Ordering::Relaxed) != binding.vma_generation
            || slot.pager_epoch.load(Ordering::Relaxed) != binding.pager_epoch
            || required_rights & !FRAME_RIGHT_KNOWN != 0
            || required_rights & !slot.rights.load(Ordering::Relaxed) != 0
        {
            return Err(FrameGrantError::Stale);
        }
        // ORDERING: the AcqRel claim is the single consume linearization
        // point; a concurrent cancel or reply observes Claimed and fails.
        if slot
            .state
            .compare_exchange(
                FRAME_GRANT_LIVE,
                FRAME_GRANT_CLAIMED,
                // ORDERING: AcqRel both sees the published payload and makes
                // this one-shot claimant visible before it consumes the page.
                Ordering::AcqRel,
                // ORDERING: acquiring the losing state distinguishes an
                // already-consumed/cancelled grant from a valid live one.
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(FrameGrantError::Stale);
        }
        let grant = FrameGrant {
            frame_phys: slot.frame_phys.load(Ordering::Relaxed),
            binding,
            rights: slot.rights.load(Ordering::Relaxed),
            from_fault_pool: slot.from_fault_pool.load(Ordering::Relaxed) != 0,
        };
        // ORDERING: consuming a grant is terminal for this generation; only
        // after its exact frame copy is local may a new publisher reuse slot.
        slot.state.store(FRAME_GRANT_FREE, Ordering::Release);
        Ok(grant)
    }
}

/// Wired fault frames left after the most recent reservation.
pub fn pager_fault_reserve_depth() -> Option<usize> {
    FAULT_FRAME_POOL.observed_depth()
}

/// The lowest reserve depth any fault has observed this boot.
///
/// The progress condition for demand paging is that this stays above zero. A
/// zero means a valid non-present fault was refused for want of a wired frame,
/// which reaches the surface as an unexplained SIGSEGV several layers away
/// rather than as pager pressure.
pub fn pager_fault_reserve_low_watermark() -> Option<usize> {
    FAULT_FRAME_POOL.observed_low_watermark()
}

/// Reservations refused because the wired reserve was empty.
pub fn pager_fault_reserve_exhaustions() -> u64 {
    FAULT_FRAME_POOL.exhaustions.load(Ordering::Relaxed)
}

static FRAME_GRANTS: FrameGrantTable = FrameGrantTable::new();
static FAULT_FRAME_POOL: FaultFramePool = FaultFramePool::new();

/// Wires the bounded fault-frame reserve after physical memory initialization.
///
/// This runs once before user tasks exist. A failure is terminal at boot: a
/// pageable product topology must not advertise a fault path that later falls
/// back to allocator work from exception context.
pub fn preallocate_pager_fault_frames() -> Result<(), FrameGrantError> {
    if FAULT_FRAME_POOL
        .state
        .compare_exchange(
            FAULT_FRAME_POOL_COLD,
            FAULT_FRAME_POOL_FILLING,
            // ORDERING: AcqRel makes this initializer the only physical-frame
            // owner while acquiring any prior failed-fill reset publication.
            Ordering::AcqRel,
            // ORDERING: failure needs only the published pool state below;
            // payload fields are inaccessible until Ready.
            Ordering::Acquire,
        )
        .is_err()
    {
        // ORDERING: Acquire observes whether a prior boot initializer sealed
        // Ready, versus a still-private Filling attempt that must fail closed.
        return (FAULT_FRAME_POOL.state.load(Ordering::Acquire) == FAULT_FRAME_POOL_READY)
            .then_some(())
            .ok_or(FrameGrantError::Pressure);
    }
    for slot in &FAULT_FRAME_POOL.frames {
        let Some(frame) = phys::alloc_frame() else {
            for allocated in &FAULT_FRAME_POOL.frames {
                // ORDERING: AcqRel withdraws only frames published by this
                // failed Filling owner before they are returned to phys.
                let frame_phys = allocated.swap(0, Ordering::AcqRel);
                if frame_phys != 0 {
                    phys::free_frame(PhysAddr::new(frame_phys));
                }
            }
            // ORDERING: Release reopens a fully drained reserve; no fault can
            // reserve it until a later initializer publishes Ready.
            FAULT_FRAME_POOL
                .state
                .store(FAULT_FRAME_POOL_COLD, Ordering::Release);
            return Err(FrameGrantError::OutOfFrames);
        };
        // SAFETY: boot owns the newly allocated frame exclusively until the
        // slot's Release publication makes its all-zero contents observable.
        unsafe {
            ptr::write_bytes(
                kernel_vm::higher_half_addr(frame.as_u64()) as *mut u8,
                0,
                FRAME_BYTES,
            );
        }
        // ORDERING: during Filling no reservation is permitted, so this
        // direct Release store cannot race a consumer or double-publish.
        slot.store(frame.as_u64(), Ordering::Release);
    }
    // ORDERING: Ready is the final boot publication; every frame-store before
    // it is visible to a reserve operation that acquires this state.
    FAULT_FRAME_POOL
        .state
        .store(FAULT_FRAME_POOL_READY, Ordering::Release);
    Ok(())
}

/// Replenishes a bounded number of consumed wired fault frames from normal
/// housekeeping context. Fault entry remains allocation-free: only this path
/// allocates and zeroes replacements, then release-publishes them to empty
/// reserve slots.
pub fn replenish_pager_fault_frames(budget: usize) -> usize {
    // ORDERING: Acquire pairs with the boot initializer's Release seal, so a
    // replenisher never observes a pool that is still filling.
    if budget == 0 || FAULT_FRAME_POOL.state.load(Ordering::Acquire) != FAULT_FRAME_POOL_READY {
        return 0;
    }
    let mut replenished = 0;
    for _ in 0..budget.min(MAX_PREALLOCATED_PAGER_FAULT_FRAMES) {
        if !FAULT_FRAME_POOL
            .frames
            .iter()
            // ORDERING: Acquire pairs with the Release that published a frame
            // into this slot, so an empty slot is only observed after the
            // consumer's take has become visible.
            .any(|slot| slot.load(Ordering::Acquire) == 0)
        {
            break;
        }
        let Some(frame) = phys::alloc_frame() else {
            break;
        };
        // SAFETY: this normal-time owner holds the fresh physical frame until
        // `publish` release-publishes its fully zeroed contents.
        unsafe {
            ptr::write_bytes(
                kernel_vm::higher_half_addr(frame.as_u64()) as *mut u8,
                0,
                FRAME_BYTES,
            );
        }
        if FAULT_FRAME_POOL.publish(frame.as_u64()).is_err() {
            phys::free_frame(frame);
            break;
        }
        replenished += 1;
    }
    if replenished != 0
        && FIRST_FAULT_FRAME_REPLENISHMENT
            // ORDERING: AcqRel makes exactly one caller the first replenisher;
            // the Acquire failure edge lets every loser observe that the
            // milestone is already published instead of emitting a duplicate.
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Memory,
            "pager-fault-frame-replenishment-started",
            replenished as u64,
            budget as u64,
        );
    }
    replenished
}

/// Reports whether an IRQ-off consumer crossed the reserve low-water mark.
///
/// IRQ code may read this bit to force its next *user-frame* timer boundary
/// through the scheduler.  It must not call the producer itself.
pub fn pager_fault_frame_refill_requested() -> bool {
    // ORDERING: Acquire pairs with the IRQ consumer's Release low-water
    // publication before a timer decides whether it may retain user execution.
    PAGER_FAULT_REFILL_REQUESTED.load(Ordering::Acquire)
}

/// Refill the wired anonymous-fault reserve from its dedicated normal-context
/// producer task.
///
/// This is intentionally separate from generic housekeeping: a burst of
/// first-touch faults requests the producer at 75% reserve, and that producer
/// restores the complete bounded pool before unrelated reaping or profiling
/// work can delay it.  The fault that set the bit was already resolved by its
/// leaf CAS; no page fault waits for this function.
pub fn service_pager_fault_frame_refill() -> usize {
    // ORDERING: AcqRel consumes one producer request while preserving a new
    // IRQ-side Release request that races with this producer pass.
    if !PAGER_FAULT_REFILL_REQUESTED.swap(false, Ordering::AcqRel) {
        return 0;
    }
    let replenished = replenish_pager_fault_frames(PAGER_FAULT_REFILL_PASS_BUDGET);
    if FAULT_FRAME_POOL.has_empty_slot() {
        // Allocation pressure is observable and retryable.  Retain the wake
        // reason rather than silently treating a partial producer pass as a
        // full reserve.
        // ORDERING: Release retains the producer wake reason until a later
        // pass sees a fully published pool.
        PAGER_FAULT_REFILL_REQUESTED.store(true, Ordering::Release);
    }
    replenished
}

/// Takes one pre-zeroed wired frame for a fault ring0 answers by itself.
///
/// This is the exception-entry source for anonymous first touch. Ring0 maps
/// the frame into the faulting task's own address space and never hands it to
/// a pager, so there is no grant to mint, publish, and claim back: the grant
/// registry exists to carry a frame *across* a round trip to another process,
/// and this path has no round trip. The reserve's guarantee is unchanged - one
/// wired, already-zeroed frame is available without calling the physical
/// allocator at exception entry.
///
/// The caller owns the frame outright. It must either map it or hand it back
/// through [`return_pager_fault_frame`].
pub fn take_pager_fault_frame() -> Option<PhysAddr> {
    FAULT_FRAME_POOL.reserve().ok().map(PhysAddr::new)
}

/// Returns a frame from [`take_pager_fault_frame`] that was never mapped.
///
/// False means the reserve would not accept it; the caller must then free the
/// frame to the ordinary allocator rather than leak it.
pub fn return_pager_fault_frame(frame: PhysAddr) -> bool {
    FAULT_FRAME_POOL.return_frame(frame.as_u64())
}

/// Allocates and zeroes one ordinary frame, with no grant and no reserve draw.
///
/// This is for pages ring0 populates around a fault *after* the fault has been
/// answered, in ordinary syscall context. It deliberately does not touch the
/// wired reserve: that reserve exists so exception entry never allocates, and
/// spending it on best-effort surplus pages would trade a guarantee for a
/// throughput gain. Returning `None` here simply means fewer surplus pages.
pub fn allocate_zeroed_frame() -> Option<PhysAddr> {
    let frame = phys::alloc_frame()?;
    // SAFETY: `alloc_frame` returns one live page below the direct-map ceiling
    // and this caller owns it exclusively until it is mapped or freed.
    unsafe {
        ptr::write_bytes(
            kernel_vm::higher_half_addr(frame.as_u64()) as *mut u8,
            0,
            FRAME_BYTES,
        );
    }
    Some(frame)
}

pub fn allocate_zeroed_frame_grant(
    binding: FrameGrantBinding,
    rights: u64,
) -> Result<u64, FrameGrantError> {
    let frame = phys::alloc_frame().ok_or(FrameGrantError::OutOfFrames)?;
    // SAFETY: `alloc_frame` returns one live page below the direct-map ceiling;
    // no handle is published until this exact page has been fully initialized.
    unsafe {
        ptr::write_bytes(
            kernel_vm::higher_half_addr(frame.as_u64()) as *mut u8,
            0,
            FRAME_BYTES,
        );
    }
    match FRAME_GRANTS.publish(frame.as_u64(), binding, rights, false) {
        Ok(handle) => Ok(handle),
        Err(error) => {
            phys::free_frame(frame);
            Err(error)
        }
    }
}

/// Mints one opaque grant from a pre-zeroed wired fault-frame reserve.
///
/// This is the exception-safe counterpart of
/// `allocate_zeroed_frame_grant`: it neither invokes the physical allocator
/// nor takes a raw registry lock. Cancellation returns the frame to the same
/// bounded reserve; successful PTE adoption consumes it as ordinary process
/// frame ownership.
pub fn reserve_preallocated_zeroed_frame_grant(
    binding: FrameGrantBinding,
    rights: u64,
) -> Result<u64, FrameGrantError> {
    let frame_phys = FAULT_FRAME_POOL.reserve()?;
    match FRAME_GRANTS.publish(frame_phys, binding, rights, true) {
        Ok(handle) => Ok(handle),
        Err(error) => {
            if !FAULT_FRAME_POOL.return_frame(frame_phys) {
                phys::free_frame(PhysAddr::new(frame_phys));
            }
            Err(error)
        }
    }
}

pub fn take_frame_grant(
    handle: u64,
    binding: FrameGrantBinding,
    required_rights: u64,
) -> Result<PhysAddr, FrameGrantError> {
    if required_rights == 0 {
        return Err(FrameGrantError::Malformed);
    }
    FRAME_GRANTS
        .take(handle, binding, required_rights)
        .map(|grant| PhysAddr::new(grant.frame_phys))
}

pub fn cancel_frame_grant(handle: u64, binding: FrameGrantBinding) -> Result<(), FrameGrantError> {
    let grant = FRAME_GRANTS.take(handle, binding, 0)?;
    if !grant.from_fault_pool || !FAULT_FRAME_POOL.return_frame(grant.frame_phys) {
        phys::free_frame(PhysAddr::new(grant.frame_phys));
    }
    Ok(())
}

fn encode_handle(index: usize, generation: u64) -> Option<u64> {
    let encoded_index = u64::try_from(index).ok()?.checked_add(1)?;
    if encoded_index > HANDLE_INDEX_MASK || generation == 0 || generation > HANDLE_MAX_GENERATION {
        return None;
    }
    generation
        .checked_shl(HANDLE_INDEX_BITS)
        .map(|value| value | encoded_index)
}

fn decode_handle(handle: u64) -> Option<(usize, u64)> {
    let encoded_index = handle & HANDLE_INDEX_MASK;
    let generation = handle >> HANDLE_INDEX_BITS;
    if encoded_index == 0 || generation == 0 {
        return None;
    }
    let index = usize::try_from(encoded_index - 1).ok()?;
    (index < MAX_PAGER_FRAME_GRANTS).then_some((index, generation))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(fault_token: u64) -> FrameGrantBinding {
        FrameGrantBinding {
            fault_token,
            process_generation: 3,
            mm_generation: 5,
            vma_generation: 7,
            pager_epoch: 11,
        }
    }

    #[test]
    fn exact_binding_consumes_frame_grant_once() {
        let table = FrameGrantTable::new();
        let exact = binding(13);
        let handle = table
            .publish(0x2000, exact, FRAME_RIGHT_READ, false)
            .unwrap();
        assert_eq!(
            table.take(handle, binding(14), FRAME_RIGHT_READ),
            Err(FrameGrantError::Stale)
        );
        assert_eq!(
            table
                .take(handle, exact, FRAME_RIGHT_READ)
                .map(|grant| grant.frame_phys),
            Ok(0x2000)
        );
        assert_eq!(
            table.take(handle, exact, FRAME_RIGHT_READ),
            Err(FrameGrantError::Stale)
        );
    }

    #[test]
    fn reused_slot_never_accepts_stale_frame_handle() {
        let table = FrameGrantTable::new();
        let binding = binding(17);
        let stale = table
            .publish(0x3000, binding, FRAME_RIGHT_READ, false)
            .unwrap();
        table.take(stale, binding, FRAME_RIGHT_READ).unwrap();
        let current = table
            .publish(0x4000, binding, FRAME_RIGHT_READ, false)
            .unwrap();
        assert_ne!(stale, current);
        assert_eq!(
            table.take(stale, binding, FRAME_RIGHT_READ),
            Err(FrameGrantError::Stale)
        );
    }

    #[test]
    fn rights_are_subset_checked_and_wx_is_never_published() {
        let table = FrameGrantTable::new();
        let binding = binding(19);
        assert_eq!(
            table.publish(
                0x5000,
                binding,
                FRAME_RIGHT_READ | FRAME_RIGHT_WRITE | FRAME_RIGHT_EXECUTE,
                false,
            ),
            Err(FrameGrantError::Malformed)
        );
        let handle = table
            .publish(0x6000, binding, FRAME_RIGHT_READ | FRAME_RIGHT_WRITE, false)
            .unwrap();
        assert_eq!(
            table.take(handle, binding, FRAME_RIGHT_EXECUTE),
            Err(FrameGrantError::Stale)
        );
    }

    #[test]
    fn wired_fault_pool_is_bounded_and_reserves_each_frame_once() {
        let pool = FaultFramePool::new();
        assert_eq!(pool.reserve(), Err(FrameGrantError::Pressure));
        pool.publish(0x8000).unwrap();
        pool.publish(0x9000).unwrap();
        // ORDERING: this test models the final boot Ready publication after
        // both frame slots have received their zeroed physical identities.
        pool.state.store(FAULT_FRAME_POOL_READY, Ordering::Release);
        let first = pool.reserve().unwrap();
        let second = pool.reserve().unwrap();
        assert_ne!(first, second);
        assert!(matches!(first, 0x8000 | 0x9000));
        assert!(matches!(second, 0x8000 | 0x9000));
        assert_eq!(pool.reserve(), Err(FrameGrantError::OutOfFrames));
        assert!(pool.return_frame(first));
        assert_eq!(pool.reserve(), Ok(first));
    }

    /// Fills a pool the way boot does, without touching the physical allocator.
    fn ready_pool(frames: usize) -> FaultFramePool {
        let pool = FaultFramePool::new();
        for index in 0..frames {
            pool.publish(0x10_000 + index as u64 * FRAME_BYTES as u64)
                .unwrap();
        }
        // ORDERING: models the final boot Ready publication, after every slot
        // has received its zeroed physical identity.
        pool.state.store(FAULT_FRAME_POOL_READY, Ordering::Release);
        pool
    }

    /// The progress condition for demand paging, stated as a test.
    ///
    /// A fault consumes one wired frame permanently - the frame becomes the
    /// target address space's page - so the completion path must replace it
    /// *before* it hands execution back to the fault owner. When replacement
    /// was left to a housekeeping task instead, a sustained fault-owner to
    /// pagerd handoff chain never yielded to housekeeping, drained the reserve,
    /// and turned a valid non-present fault into a dead thread several layers
    /// away from the actual cause.
    #[test]
    fn a_reserve_replenished_at_each_completion_never_runs_dry() {
        const FRAMES: usize = 8;
        let pool = ready_pool(FRAMES);
        for round in 0..FRAMES * 16 {
            let frame = pool
                .reserve()
                .unwrap_or_else(|error| panic!("round {round} must reserve: {error:?}"));
            // The fault maps the frame into the process, so it never returns.
            // Replenishment is what closes the lifecycle.
            assert!(pool.publish(frame).is_ok(), "round {round} must replenish");
        }
        assert_eq!(pool.exhaustions.load(Ordering::Relaxed), 0);
        assert_eq!(pool.observed_low_watermark(), Some(FRAMES - 1));
        assert!(
            pool.observed_low_watermark().is_some_and(|low| low > 0),
            "the reserve must never reach zero while faults are completing"
        );
    }

    /// The control, and the diagnostic this class of failure was missing. With
    /// no replenishment the reserve drains after exactly its own size, and the
    /// refusal is *counted* under its own cause rather than surfacing as an
    /// unexplained fault several layers away.
    #[test]
    fn an_unreplenished_reserve_drains_after_exactly_its_size_and_says_so() {
        const FRAMES: usize = 8;
        let pool = ready_pool(FRAMES);
        for round in 0..FRAMES {
            assert!(
                pool.reserve().is_ok(),
                "round {round} is within the reserve"
            );
        }
        assert_eq!(pool.observed_low_watermark(), Some(0));
        assert_eq!(pool.reserve(), Err(FrameGrantError::OutOfFrames));
        assert_eq!(pool.exhaustions.load(Ordering::Relaxed), 1);
        assert_eq!(pool.reserve(), Err(FrameGrantError::OutOfFrames));
        assert_eq!(pool.exhaustions.load(Ordering::Relaxed), 2);
    }

    /// The ring0-served anonymous path takes a reserve frame outright instead
    /// of minting a grant it would immediately claim back. A fault it cannot
    /// map must therefore return that exact frame to the same pool, or every
    /// rejected mapping would permanently shrink the reserve - the failure
    /// mode a wired reserve exists to make impossible.
    #[test]
    fn a_frame_taken_for_a_ring0_fault_and_not_mapped_returns_to_the_reserve() {
        const FRAMES: usize = 4;
        let pool = ready_pool(FRAMES);
        for round in 0..FRAMES * 8 {
            let frame = pool
                .reserve()
                .unwrap_or_else(|error| panic!("round {round} must reserve: {error:?}"));
            assert!(
                pool.return_frame(frame),
                "round {round} must return the exact unmapped frame"
            );
        }
        assert_eq!(pool.exhaustions.load(Ordering::Relaxed), 0);
        assert_eq!(
            pool.observed_low_watermark(),
            Some(FRAMES - 1),
            "a returned frame must leave the reserve exactly as deep as it was"
        );
        // And the reserve is still whole: every frame is reservable again.
        for round in 0..FRAMES {
            assert!(pool.reserve().is_ok(), "round {round} is within the reserve");
        }
        assert_eq!(pool.reserve(), Err(FrameGrantError::OutOfFrames));
    }

    /// The static relation that makes exhaustion unreachable before fault-slot
    /// admission has already refused. A reserve smaller than the slot table
    /// can run dry while slots are still free, which is an exhaustion with no
    /// admission point to refuse at.
    #[test]
    fn the_reserve_is_never_smaller_than_the_fault_slot_table() {
        assert!(MAX_PREALLOCATED_PAGER_FAULT_FRAMES >= MAX_PAGER_FRAME_GRANTS);
        assert_eq!(MAX_PREALLOCATED_PAGER_FAULT_FRAMES, 2048);
    }
}
