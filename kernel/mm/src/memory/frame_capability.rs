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
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use x86_64::PhysAddr;

use super::{kernel_vm, phys};

pub const MAX_PAGER_FRAME_GRANTS: usize = 128;
pub const MAX_PREALLOCATED_PAGER_FAULT_FRAMES: usize = 64;
const FRAME_BYTES: usize = 4096;
const HANDLE_INDEX_BITS: u32 = 16;
const HANDLE_INDEX_MASK: u64 = (1_u64 << HANDLE_INDEX_BITS) - 1;
const HANDLE_MAX_GENERATION: u64 = u64::MAX >> HANDLE_INDEX_BITS;
const FRAME_RIGHT_READ: u64 = 1 << 0;
const FRAME_RIGHT_WRITE: u64 = 1 << 1;
const FRAME_RIGHT_EXECUTE: u64 = 1 << 2;
const FRAME_RIGHT_KNOWN: u64 = FRAME_RIGHT_READ | FRAME_RIGHT_WRITE | FRAME_RIGHT_EXECUTE;
static FIRST_FAULT_FRAME_REPLENISHMENT: AtomicBool = AtomicBool::new(false);

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
}

impl FaultFramePool {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(FAULT_FRAME_POOL_COLD),
            frames: [const { AtomicU64::new(0) }; MAX_PREALLOCATED_PAGER_FAULT_FRAMES],
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
                return Ok(());
            }
        }
        Err(FrameGrantError::Pressure)
    }

    fn reserve(&self) -> Result<u64, FrameGrantError> {
        // ORDERING: Ready acquires the completed boot fill before any fault
        // can observe a slot; a cold or filling pool is not usable authority.
        if self.state.load(Ordering::Acquire) != FAULT_FRAME_POOL_READY {
            return Err(FrameGrantError::Pressure);
        }
        for slot in &self.frames {
            // ORDERING: AcqRel claims one exact wired frame and prevents a
            // second fault from receiving the same opaque grant backing.
            let frame_phys = slot.swap(0, Ordering::AcqRel);
            if frame_phys != 0 {
                return Ok(frame_phys);
            }
        }
        Err(FrameGrantError::OutOfFrames)
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
}
