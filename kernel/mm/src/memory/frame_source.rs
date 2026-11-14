//! Generic immutable frame-source capabilities, used by external pagers.
//!
//! No file names, image formats, cache replacement, or I/O policy live here.
//! A source owns one descriptor/cache reference and one resident charge. A
//! short-lived pin prevents withdrawal from freeing that reference while a
//! normal-context installer publishes its own exact leaf descriptor. Withdrawal
//! rejects new pins immediately; the last pin only enqueues cold reclamation.
//! Existing mapped leaves independently retain their physical frame until TLB
//! reclamation completes. Source residency stays charged until cold cleanup.
//!
//! Supply and reclamation require normal syscall/cold context. Withdrawal and
//! pin Drop are allocation-free and perform no physical/descriptor reclamation,
//! including provider-exit cleanup from exception context. Keys are immutable between the
//! release publication and final pin release. Per-field relaxed accesses below
//! are protected by that publication and the packed generation/reference word.
//!
//! - **Owner:** kernel-mm owns source slots, pins and the resident charge; an
//!   external pager owns what a source contains and when it is withdrawn.
//! - **Boundary:** callers present a key and owner; neither is trusted beyond an
//!   exact match, and a pin is the only way to read a source's frame.
//! - **Lifecycle:** supply, pin, publish a leaf, unpin, withdraw, then cold
//!   reclamation once the last pin drops; mapped leaves outlive withdrawal.
//! - **Concurrency:** publication and revoke serialize on the packed
//!   generation/reference word; withdrawal and pin Drop never allocate.
//! - **Failure:** a stale key, owner or generation yields no pin; exhaustion
//!   refuses supply and never evicts a pinned source.
//! - **Forbidden:** file names, cache policy, I/O, and reclamation from
//!   exception context.
//! - **Evidence:** `formal/frame-source-lifecycle` and the native table and
//!   descriptor tests below.
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::{charge, frame_capability, phys};
use x86_64::PhysAddr;

pub const MAX_FRAME_SOURCES: usize = 16_384;
const FRAME_BYTES: u64 = 4096;
const INDEX_BITS: u32 = 16;
const INDEX_MASK: u64 = (1 << INDEX_BITS) - 1;
const STATE_MASK: u64 = 3;
const FREE: u64 = 0;
const PUBLISHING: u64 = 1;
const LIVE: u64 = 2;
const RETIRING: u64 = 3;
const REF_SHIFT: u32 = 2;
const REF_ONE: u64 = 1 << REF_SHIFT;
const REF_MAX: u64 = (1 << 16) - 1;
const GEN_SHIFT: u32 = REF_SHIFT + 16;
const GEN_MAX: u64 = u64::MAX >> GEN_SHIFT;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameSourceOwner {
    pub service_slot: u64,
    pub service_generation: u64,
}
impl FrameSourceOwner {
    pub const fn is_valid(self) -> bool {
        self.service_slot != 0 && self.service_generation != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameSourceKey {
    pub namespace: u64,
    pub object: u64,
    pub object_generation: u64,
    pub content_generation: u64,
    pub page_offset: u64,
}
impl FrameSourceKey {
    pub const fn is_valid(self) -> bool {
        self.namespace != 0
            && self.object != 0
            && self.object_generation != 0
            && self.content_generation != 0
            && self.page_offset.is_multiple_of(FRAME_BYTES)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameSourceError {
    Invalid,
    NoSlots,
    NoMemory,
    Quota,
    Stale,
    Authority,
    Mismatch,
    RefOverflow,
}

#[derive(Clone, Copy)]
struct Source {
    owner: FrameSourceOwner,
    key: FrameSourceKey,
    frame: u64,
    rights: u64,
    group: usize,
}

struct Slot {
    word: AtomicU64,
    owner_slot: AtomicU64,
    owner_generation: AtomicU64,
    namespace: AtomicU64,
    object: AtomicU64,
    object_generation: AtomicU64,
    content_generation: AtomicU64,
    page_offset: AtomicU64,
    frame: AtomicU64,
    rights: AtomicU64,
    group: AtomicUsize,
    next_ready: AtomicUsize,
}
impl Slot {
    const fn new() -> Self {
        Self {
            word: AtomicU64::new(0),
            owner_slot: AtomicU64::new(0),
            owner_generation: AtomicU64::new(0),
            namespace: AtomicU64::new(0),
            object: AtomicU64::new(0),
            object_generation: AtomicU64::new(0),
            content_generation: AtomicU64::new(0),
            page_offset: AtomicU64::new(0),
            frame: AtomicU64::new(0),
            rights: AtomicU64::new(0),
            group: AtomicUsize::new(0),
            next_ready: AtomicUsize::new(0),
        }
    }
    fn write(&self, source: Source) {
        self.owner_slot
            .store(source.owner.service_slot, Ordering::Relaxed);
        self.owner_generation
            .store(source.owner.service_generation, Ordering::Relaxed);
        self.namespace
            .store(source.key.namespace, Ordering::Relaxed);
        self.object.store(source.key.object, Ordering::Relaxed);
        self.object_generation
            .store(source.key.object_generation, Ordering::Relaxed);
        self.content_generation
            .store(source.key.content_generation, Ordering::Relaxed);
        self.page_offset
            .store(source.key.page_offset, Ordering::Relaxed);
        self.frame.store(source.frame, Ordering::Relaxed);
        self.rights.store(source.rights, Ordering::Relaxed);
        self.group.store(source.group, Ordering::Relaxed);
    }
    fn read(&self) -> Source {
        Source {
            owner: FrameSourceOwner {
                service_slot: self.owner_slot.load(Ordering::Relaxed),
                service_generation: self.owner_generation.load(Ordering::Relaxed),
            },
            key: FrameSourceKey {
                namespace: self.namespace.load(Ordering::Relaxed),
                object: self.object.load(Ordering::Relaxed),
                object_generation: self.object_generation.load(Ordering::Relaxed),
                content_generation: self.content_generation.load(Ordering::Relaxed),
                page_offset: self.page_offset.load(Ordering::Relaxed),
            },
            frame: self.frame.load(Ordering::Relaxed),
            rights: self.rights.load(Ordering::Relaxed),
            group: self.group.load(Ordering::Relaxed),
        }
    }
}

const fn word(generation: u64, refs: u64, state: u64) -> u64 {
    (generation << GEN_SHIFT) | (refs << REF_SHIFT) | state
}
const fn generation(value: u64) -> u64 {
    value >> GEN_SHIFT
}
const fn references(value: u64) -> u64 {
    (value >> REF_SHIFT) & REF_MAX
}
const fn handle(index: usize, generation: u64) -> u64 {
    (generation << INDEX_BITS) | (index as u64 + 1)
}

struct Table<const N: usize> {
    slots: [Slot; N],
    cursor: AtomicUsize,
    ready: AtomicUsize,
    backlog: AtomicUsize,
    reclaiming: AtomicBool,
    release: fn(u64, u64, usize),
}
impl<const N: usize> Table<N> {
    const fn new(release: fn(u64, u64, usize)) -> Self {
        assert!(N > 0 && N < INDEX_MASK as usize);
        Self {
            slots: [const { Slot::new() }; N],
            cursor: AtomicUsize::new(0),
            ready: AtomicUsize::new(0),
            backlog: AtomicUsize::new(0),
            reclaiming: AtomicBool::new(false),
            release,
        }
    }
    fn decode(token: u64) -> Result<(usize, u64), FrameSourceError> {
        let index = token & INDEX_MASK;
        let generation = token >> INDEX_BITS;
        if index == 0 || index > N as u64 || generation == 0 || generation > GEN_MAX {
            return Err(FrameSourceError::Stale);
        }
        Ok((index as usize - 1, generation))
    }
    fn publish(
        &self,
        owner: FrameSourceOwner,
        key: FrameSourceKey,
        rights: u64,
        group: usize,
        allocate: impl FnOnce(u64) -> Result<u64, FrameSourceError>,
    ) -> Result<u64, FrameSourceError> {
        if !owner.is_valid()
            || !key.is_valid()
            || rights & frame_capability::FRAME_RIGHT_READ == 0
            || rights & !frame_capability::FRAME_RIGHT_KNOWN != 0
            || rights & frame_capability::FRAME_RIGHT_WRITE != 0
        {
            return Err(FrameSourceError::Invalid);
        }
        // ORDERING: Relaxed; cursor affects fairness only, never authority.
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % N;
        for offset in 0..N {
            let index = (start + offset) % N;
            let slot = &self.slots[index];
            // ORDERING: Acquire pairs with final release before slot reuse.
            let old = slot.word.load(Ordering::Acquire);
            if old & STATE_MASK != FREE || generation(old) == GEN_MAX {
                continue;
            }
            let next = generation(old) + 1;
            // ORDERING: AcqRel claims the unique publishing owner.
            if slot
                .word
                .compare_exchange(
                    old,
                    word(next, 0, PUBLISHING),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }
            let token = handle(index, next);
            let frame = match allocate(token) {
                Ok(frame) => frame,
                Err(error) => {
                    // ORDERING: Release permits reuse, but never reuse of token.
                    slot.word.store(word(next, 0, FREE), Ordering::Release);
                    return Err(error);
                }
            };
            assert!(
                frame != 0 && frame.is_multiple_of(FRAME_BYTES),
                "invalid allocated source frame"
            );
            slot.write(Source {
                owner,
                key,
                frame,
                rights,
                group,
            });
            // ORDERING: Release publishes immutable fields and source custody.
            slot.word.store(word(next, 1, LIVE), Ordering::Release);
            return Ok(token);
        }
        Err(FrameSourceError::NoSlots)
    }
    fn pin_raw(&self, token: u64) -> Result<Pin<'_, N>, FrameSourceError> {
        let (index, expected) = Self::decode(token)?;
        let slot = &self.slots[index];
        loop {
            // ORDERING: Acquire observes source publication or withdrawal.
            let value = slot.word.load(Ordering::Acquire);
            if generation(value) != expected || value & STATE_MASK != LIVE {
                return Err(FrameSourceError::Stale);
            }
            if references(value) == REF_MAX {
                return Err(FrameSourceError::RefOverflow);
            }
            // ORDERING: AcqRel pins immutable fields before a withdraw can free.
            if slot
                .word
                .compare_exchange_weak(value, value + REF_ONE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(Pin {
                    table: self,
                    index,
                    generation: expected,
                    source: slot.read(),
                });
            }
        }
    }
    fn pin(
        &self,
        token: u64,
        owner: FrameSourceOwner,
        key: FrameSourceKey,
        rights: u64,
    ) -> Result<Pin<'_, N>, FrameSourceError> {
        let pin = self.pin_raw(token)?;
        if pin.source.owner != owner {
            return Err(FrameSourceError::Authority);
        }
        if pin.source.key != key || rights == 0 || rights & !pin.source.rights != 0 {
            return Err(FrameSourceError::Mismatch);
        }
        Ok(pin)
    }
    fn retire(&self, token: u64, owner: FrameSourceOwner) -> Result<(), FrameSourceError> {
        let pin = self.pin_raw(token)?;
        if pin.source.owner != owner {
            return Err(FrameSourceError::Authority);
        }
        let slot = &self.slots[pin.index];
        loop {
            // ORDERING: Acquire sees competing pins and withdrawals.
            let value = slot.word.load(Ordering::Acquire);
            if generation(value) != pin.generation || value & STATE_MASK != LIVE {
                return Err(FrameSourceError::Stale);
            }
            // ORDERING: AcqRel closes admission before the owner reference drops.
            if slot
                .word
                .compare_exchange_weak(
                    value,
                    (value & !STATE_MASK) | RETIRING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.release_pin(pin.index, pin.generation); // source-owner reference
                return Ok(()); // local pin Drop releases the final control borrow
            }
        }
    }
    fn release_pin(&self, index: usize, expected: u64) {
        let slot = &self.slots[index];
        // ORDERING: AcqRel makes the last pin observe all earlier installers.
        let old = slot.word.fetch_sub(REF_ONE, Ordering::AcqRel);
        assert_eq!(generation(old), expected, "source pin crossed generation");
        assert!(references(old) > 0, "source reference underflow");
        if references(old) != 1 {
            return;
        }
        assert_eq!(old & STATE_MASK, RETIRING, "live source lost owner custody");
        // One producer per node: only the last reference can enqueue it, and
        // that slot cannot be reused before the cold consumer finishes.
        // ORDERING: Acquire/Release publish the intrusive link before its head.
        let mut head = self.ready.load(Ordering::Acquire);
        loop {
            slot.next_ready.store(head, Ordering::Relaxed);
            match self.ready.compare_exchange_weak(
                head,
                index + 1,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => head = current,
            }
        }
    }
    fn reclaim(&self, budget: usize) -> usize {
        if budget == 0 {
            return 0;
        }
        // ORDERING: Acquire/Release serialize consumers; contention never waits.
        if self
            .reclaiming
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return 0;
        }
        // Only this consumer touches backlog. Producers publish to ready.
        let mut next = self.backlog.swap(0, Ordering::Relaxed);
        let mut reclaimed = 0;
        while reclaimed < budget {
            if next == 0 {
                // ORDERING: Acquire takes the complete producer-published list.
                next = self.ready.swap(0, Ordering::AcqRel);
                if next == 0 {
                    break;
                }
            }
            let index = next - 1;
            let slot = &self.slots[index];
            next = slot.next_ready.load(Ordering::Relaxed);
            // ORDERING: Acquire pairs with final pin release before reclamation.
            let value = slot.word.load(Ordering::Acquire);
            assert_eq!(
                value & STATE_MASK,
                RETIRING,
                "source queue contains live slot"
            );
            assert_eq!(references(value), 0, "source queue contains borrowed slot");
            let source = slot.read();
            (self.release)(source.frame, handle(index, generation(value)), source.group);
            // ORDERING: Release permits reuse after descriptor/charge cleanup.
            slot.word
                .store(word(generation(value), 0, FREE), Ordering::Release);
            reclaimed += 1;
        }
        self.backlog.store(next, Ordering::Relaxed);
        self.reclaiming.store(false, Ordering::Release);
        reclaimed
    }
    fn retire_owner(&self, owner: FrameSourceOwner) -> usize {
        let mut retired = 0;
        for (index, slot) in self.slots.iter().enumerate() {
            // ORDERING: Acquire samples only; retire revalidates exact authority.
            let value = slot.word.load(Ordering::Acquire);
            if value & STATE_MASK == LIVE
                && self.retire(handle(index, generation(value)), owner).is_ok()
            {
                retired += 1;
            }
        }
        retired
    }
}

struct Pin<'a, const N: usize> {
    table: &'a Table<N>,
    index: usize,
    generation: u64,
    source: Source,
}
impl<const N: usize> Drop for Pin<'_, N> {
    fn drop(&mut self) {
        self.table.release_pin(self.index, self.generation);
    }
}

static SOURCES: Table<MAX_FRAME_SOURCES> = Table::new(release_source);

fn release_source(frame: u64, token: u64, group: usize) {
    match phys::release_private_file_cache_frame(frame, token)
        .expect("source descriptor custody lost")
    {
        phys::DataLeafRelease::FrameReusable => phys::free_frame(PhysAddr::new(frame)),
        phys::DataLeafRelease::FrameRetained => {}
    }
    charge::uncharge(group, 1);
}

/// Copies a bounded payload once into an immutable source frame; the rest of
/// its page is zero. Source rights are READ and optional EXECUTE, never WRITE;
/// a writable private target is a different, COW-owned frame, not a source edit.
/// Caller must authenticate owner/key and derive `group`
/// from the supplying service, not from an untrusted syscall field.
pub fn supply_frame_source(
    owner: FrameSourceOwner,
    key: FrameSourceKey,
    rights: u64,
    group: usize,
    bytes: &[u8],
) -> Result<u64, FrameSourceError> {
    if bytes.len() > FRAME_BYTES as usize {
        return Err(FrameSourceError::Invalid);
    }
    SOURCES.publish(owner, key, rights, group, |token| {
        if !charge::try_charge(group, 1) {
            return Err(FrameSourceError::Quota);
        }
        let Some(frame) = frame_capability::allocate_filled_frame(bytes) else {
            charge::uncharge(group, 1);
            return Err(FrameSourceError::NoMemory);
        };
        assert!(
            phys::claim_private_file_cache_frame(frame.as_u64(), token),
            "allocated source frame has a conflicting descriptor"
        );
        Ok(frame.as_u64())
    })
}

/// A normal-context source borrow. Keep it until leaf publication has either
/// acquired independent descriptor custody or completely failed.
#[must_use]
pub struct FrameSourcePin {
    pin: Pin<'static, MAX_FRAME_SOURCES>,
}
impl FrameSourcePin {
    pub(crate) fn frame_phys(&self) -> u64 {
        self.pin.source.frame
    }
    pub(crate) fn permits(&self, rights: u64) -> bool {
        rights != 0 && rights & !self.pin.source.rights == 0
    }
    pub(crate) fn backing_identity(&self) -> u64 {
        handle(self.pin.index, self.pin.generation)
    }
}
pub fn pin_frame_source(
    token: u64,
    owner: FrameSourceOwner,
    key: FrameSourceKey,
    rights: u64,
) -> Result<FrameSourcePin, FrameSourceError> {
    SOURCES
        .pin(token, owner, key, rights)
        .map(|pin| FrameSourcePin { pin })
}
pub fn retire_frame_source(token: u64, owner: FrameSourceOwner) -> Result<(), FrameSourceError> {
    SOURCES.retire(token, owner)
}
/// Close an exact provider-service lease. This is allocation-free, does not
/// wait for installers or TLBs, and is safe on the exit/exception cleanup path.
/// Already admitted installers and mapped leaves retain their own custody.
pub fn retire_frame_sources_for_owner(owner: FrameSourceOwner) -> usize {
    SOURCES.retire_owner(owner)
}

/// Ordinary-context, bounded cold cleanup. Never call while holding a service
/// registry, process-state, or scheduler lock: descriptor retirement can wait
/// for a concurrent MM mutation to finish its TLB reclamation.
pub fn reclaim_frame_sources(budget: usize) -> usize {
    SOURCES.reclaim(budget)
}

#[cfg(test)]
#[path = "frame_source/tests.rs"]
mod tests;
