//! Deferred records for diagnostics that lost the output sink.
//!
//! Linux's printk never discards a record because a CPU lost the console: the
//! record is appended to a ringbuffer and whichever CPU next owns the console
//! emits it. The rule that makes this necessary is the same one that forbids
//! waiting - a context serializing the console must not spin for another CPU -
//! so the only remaining way to keep the record is to hand it on.
//!
//! This ring is that hand-off for the unsequenced diagnostics: a writer that
//! cannot take the sink parks its already-rendered line here, and the next
//! writer that does take the sink emits the parked lines, oldest first, before
//! its own. Milestone frames are deliberately not routed through it: their
//! output sequence is allocated while the sink is held, and that order is the
//! evidence the acceptance harness verifies.
//!
//! Overflow is still accounted rather than hidden. A full ring means the sink
//! has been unavailable for longer than the ring is deep, which is a real
//! transport failure and not something to paper over.
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::util::lockdep::MAX_TRACKED_CPUS;

use super::{CurrentUserLogContext, MilestoneOutputClass, MilestoneRecord};

/// Parked records per CPU.
///
/// The ring was one shared array, which made every parker CAS its way along
/// slots other CPUs were claiming at the same time, and made overflow global:
/// one noisy CPU could consume the depth the others needed, which is what
/// `dropped=164` was. Linux gives printk per-CPU buffers for the same reason.
/// A parker now only ever touches its own CPU's slots, so the claim is
/// uncontended and a burst is charged to the CPU that produced it. Total depth
/// also rises with the topology instead of being shared out.
const DEFERRED_SLOTS_PER_CPU: usize = 64;
/// Longest line the ring accepts, matching the serialized diagnostic bound.
pub(super) const DEFERRED_LINE_BYTES: usize = 512;

const SLOT_EMPTY: u8 = 0;
const SLOT_WRITING: u8 = 1;
const SLOT_READY: u8 = 2;

/// A parked milestone keeps its record, not its rendered bytes.
///
/// Rendering allocates the output sequence, and that sequence is the order the
/// acceptance harness verifies. Rendering a parked milestone at park time
/// would let a CPU that lost the sink stamp a lower sequence than one already
/// written, so the record is carried and rendered by the drainer, under the
/// sink, where every other milestone's sequence is also allocated.
#[derive(Clone, Copy)]
pub(super) struct ParkedMilestone {
    pub(super) record: MilestoneRecord,
    pub(super) user_context: Option<CurrentUserLogContext>,
    pub(super) output_class: MilestoneOutputClass,
}

const KIND_LINE: u8 = 0;
const KIND_MILESTONE: u8 = 1;

struct Slot {
    state: AtomicU8,
    kind: AtomicU8,
    sequence: AtomicU64,
    len: AtomicU32,
    bytes: UnsafeCell<[u8; DEFERRED_LINE_BYTES]>,
    milestone: UnsafeCell<Option<ParkedMilestone>>,
}

impl Slot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(SLOT_EMPTY),
            kind: AtomicU8::new(KIND_LINE),
            sequence: AtomicU64::new(0),
            len: AtomicU32::new(0),
            bytes: UnsafeCell::new([0; DEFERRED_LINE_BYTES]),
            milestone: UnsafeCell::new(None),
        }
    }
}

// SAFETY: a slot's buffer is written only by the CPU that moved it from
// `SLOT_EMPTY` to `SLOT_WRITING`, and read only by the CPU that observes
// `SLOT_READY` and returns it to `SLOT_EMPTY`. The state transitions are the
// exclusive-access protocol.
unsafe impl Sync for Slot {}

struct DeferredRing {
    /// One private slot array per CPU. `next_sequence` stays global: it is what
    /// lets the drainer merge the rings back into the order the records were
    /// parked in, across CPUs.
    cpu_slots: [[Slot; DEFERRED_SLOTS_PER_CPU]; MAX_TRACKED_CPUS],
    next_sequence: AtomicU64,
}

impl DeferredRing {
    const fn new() -> Self {
        Self {
            cpu_slots: [const { [const { Slot::new() }; DEFERRED_SLOTS_PER_CPU] };
                MAX_TRACKED_CPUS],
            next_sequence: AtomicU64::new(0),
        }
    }

    /// The parking CPU's own slots. An unknown index would alias another CPU's
    /// ring, so it is folded to zero rather than trusted.
    fn local_slots(&self) -> &[Slot; DEFERRED_SLOTS_PER_CPU] {
        let cpu = crate::util::lockdep::current_cpu_index();
        &self.cpu_slots[if cpu < MAX_TRACKED_CPUS { cpu } else { 0 }]
    }

    fn every_slot(&self) -> impl Iterator<Item = &Slot> {
        self.cpu_slots.iter().flat_map(|slots| slots.iter())
    }
}

static DEFERRED: DeferredRing = DeferredRing::new();

/// Park one rendered line. Returns false when the ring is full or the line is
/// longer than a parked record may be; the caller accounts the loss.
pub(super) fn park(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > DEFERRED_LINE_BYTES {
        return false;
    }
    for slot in DEFERRED.local_slots().iter() {
        // ORDERING: AcqRel claims the slot before any byte is written to it,
        // so no drainer can observe a partially filled record.
        if slot
            .state
            .compare_exchange(
                SLOT_EMPTY,
                SLOT_WRITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            continue;
        }
        // Sequence is taken after the claim so parked order matches emission
        // order even when several CPUs park concurrently.
        slot.sequence.store(
            DEFERRED.next_sequence.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );
        // SAFETY: this CPU owns the slot for the whole `SLOT_WRITING` window.
        unsafe {
            let buffer = &mut *slot.bytes.get();
            buffer[..bytes.len()].copy_from_slice(bytes);
        }
        slot.len.store(bytes.len() as u32, Ordering::Relaxed);
        slot.kind.store(KIND_LINE, Ordering::Relaxed);
        // ORDERING: Release publishes the complete record to the drainer.
        slot.state.store(SLOT_READY, Ordering::Release);
        return true;
    }
    false
}

/// Park one milestone that could not reach the sink.
pub(super) fn park_milestone(parked: ParkedMilestone) -> bool {
    for slot in DEFERRED.local_slots().iter() {
        // ORDERING: AcqRel claims the slot before it is written, so no drainer
        // can observe a partially published record.
        if slot
            .state
            .compare_exchange(
                SLOT_EMPTY,
                SLOT_WRITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            continue;
        }
        slot.sequence.store(
            DEFERRED.next_sequence.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );
        // SAFETY: this CPU owns the slot for the whole `SLOT_WRITING` window.
        unsafe {
            *slot.milestone.get() = Some(parked);
        }
        slot.kind.store(KIND_MILESTONE, Ordering::Relaxed);
        // ORDERING: Release publishes the complete record to the drainer.
        slot.state.store(SLOT_READY, Ordering::Release);
        return true;
    }
    false
}

/// Emit every parked line, oldest first. The caller must hold the output sink.
pub(super) fn drain(
    mut emit_line: impl FnMut(&[u8]),
    mut emit_milestone: impl FnMut(ParkedMilestone),
) {
    loop {
        let mut oldest: Option<(u64, &Slot)> = None;
        for slot in DEFERRED.every_slot() {
            // ORDERING: Acquire pairs with the parking CPU's Release, so the
            // bytes are visible before they are read.
            if slot.state.load(Ordering::Acquire) != SLOT_READY {
                continue;
            }
            let sequence = slot.sequence.load(Ordering::Relaxed);
            if oldest.is_none_or(|(oldest_sequence, _)| sequence < oldest_sequence) {
                oldest = Some((sequence, slot));
            }
        }
        let Some((_, slot)) = oldest else {
            return;
        };
        // SAFETY: the sink is held, so this is the only drainer, and the slot
        // stays `SLOT_READY` - and therefore untouched by any parker - until
        // this CPU releases it below.
        if slot.kind.load(Ordering::Relaxed) == KIND_MILESTONE {
            if let Some(parked) = unsafe { *slot.milestone.get() } {
                emit_milestone(parked);
            }
        } else {
            let len = (slot.len.load(Ordering::Relaxed) as usize).min(DEFERRED_LINE_BYTES);
            let bytes = unsafe { core::slice::from_raw_parts(slot.bytes.get().cast::<u8>(), len) };
            emit_line(bytes);
        }
        // ORDERING: Release makes the buffer reusable only after the emit has
        // consumed it.
        slot.state.store(SLOT_EMPTY, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFERRED, DEFERRED_LINE_BYTES, DEFERRED_SLOTS_PER_CPU, drain, park};
    use alloc::vec::Vec;

    /// The rings are process-wide statics and every test here fills and drains
    /// them, so they must not run concurrently or one test consumes another's
    /// records.
    static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = drain_all();
        guard
    }

    fn drain_all() -> Vec<Vec<u8>> {
        let mut seen = Vec::new();
        drain(|bytes| seen.push(bytes.to_vec()), |_| {});
        seen
    }

    #[test]
    fn parked_lines_emit_oldest_first_and_free_their_slots() {
        let _serial = serialized();
        assert!(park(b"first"));
        assert!(park(b"second"));
        assert!(park(b"third"));
        assert_eq!(
            drain_all(),
            vec![b"first".to_vec(), b"second".to_vec(), b"third".to_vec()]
        );
        // Draining returns the slots, so the ring accepts the same depth again.
        assert!(park(b"again"));
        assert_eq!(drain_all(), vec![b"again".to_vec()]);
    }

    #[test]
    fn a_full_ring_refuses_rather_than_overwriting_a_parked_line() {
        let _serial = serialized();
        for _ in 0..DEFERRED_SLOTS_PER_CPU {
            assert!(park(b"x"));
        }
        // The caller must account this loss; silently dropping the oldest
        // record would make the gap invisible.
        assert!(!park(b"x"));
        assert_eq!(drain_all().len(), DEFERRED_SLOTS_PER_CPU);
    }

    #[test]
    fn an_oversized_or_empty_line_is_refused_before_any_slot_is_claimed() {
        let _serial = serialized();
        assert!(!park(b""));
        assert!(!park(&[b'x'; DEFERRED_LINE_BYTES + 1]));
        assert!(drain_all().is_empty());
        assert!(park(&[b'x'; DEFERRED_LINE_BYTES]));
        assert_eq!(drain_all().len(), 1);
        let _ = &DEFERRED;
    }

    #[test]
    fn one_cpus_burst_cannot_consume_the_depth_another_cpu_needs() {
        let _serial = serialized();
        // Filling this CPU's ring must not reach any other CPU's slots; that
        // sharing is what let one noisy CPU drop everyone else's records.
        for _ in 0..DEFERRED_SLOTS_PER_CPU {
            assert!(park(b"x"));
        }
        assert!(!park(b"x"));

        let remaining = DEFERRED
            .cpu_slots
            .iter()
            .skip(1)
            .flat_map(|slots| slots.iter())
            .filter(|slot| {
                slot.state.load(core::sync::atomic::Ordering::Acquire) != super::SLOT_EMPTY
            })
            .count();
        assert_eq!(remaining, 0);
        assert_eq!(drain_all().len(), DEFERRED_SLOTS_PER_CPU);
    }
}
