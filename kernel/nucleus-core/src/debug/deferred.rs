//! Deferred records for diagnostics that lost the output sink.
//!
//! - **Owner:** `nucleus-core::debug` owns per-CPU deferred line/milestone slots
//!   and the sink handoff sequence counter.
//! - **Boundary:** Accepts bounded unsequenced diagnostic lines and milestones
//!   when the console sink cannot be acquired without spinning.
//! - **Lifecycle:** Claims an empty slot (`SLOT_WRITING`), writes record bytes
//!   and sequence, publishes `SLOT_READY` + per-CPU ready bit, then drains FIFO.
//! - **Concurrency:** Per-CPU slot claims prevent cross-CPU CAS contention;
//!   atomic state/ready-bit Release/Acquire pairs order payload publication.
//! - **Failure:** Full per-CPU rings fail closed and increment overflow
//!   accounting rather than overwriting parked custody.
//! - **Forbidden:** Spinning on another CPU while holding/seeking the sink,
//!   silently overwriting un-emitted records, or cross-CPU burst starvation.
//! - **Evidence:** `kernel/nucleus-core` deferred ring unit tests.
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::util::{cache_padded::CachePadded, lockdep::MAX_TRACKED_CPUS};

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
/// Ordinary diagnostics cannot consume these final slots. Required milestone
/// evidence is produced from scheduler/IRQ contexts where waiting for the
/// sink is forbidden, so it needs a bounded private reserve during the dense
/// multi-vCPU startup burst.
const DEFERRED_REQUIRED_RESERVE_PER_CPU: usize = 16;
const DEFERRED_GENERAL_SLOTS_PER_CPU: usize =
    DEFERRED_SLOTS_PER_CPU - DEFERRED_REQUIRED_RESERVE_PER_CPU;
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
// `SLOT_EMPTY` to `SLOT_WRITING`. The sole sink drainer reads only records
// acquired through their ready bitmap bit, clears that bit after emission,
// then releases `SLOT_EMPTY`. These transitions exclude payload reuse.
unsafe impl Sync for Slot {}

struct DeferredRing {
    /// One private slot array per CPU. `next_sequence` stays global: it is what
    /// lets the drainer merge the rings back into the order the records were
    /// parked in, across CPUs.
    cpu_slots: [[Slot; DEFERRED_SLOTS_PER_CPU]; MAX_TRACKED_CPUS],
    // A bit is the final publication, not just a hint. The sole sink drainer
    // clears it BEFORE returning the slot to Empty, so a new producer's ready
    // bit cannot be erased by the preceding record's retirement.
    ready: [CachePadded<AtomicU64>; MAX_TRACKED_CPUS],
    next_sequence: AtomicU64,
}

impl DeferredRing {
    const fn new() -> Self {
        Self {
            cpu_slots: [const { [const { Slot::new() }; DEFERRED_SLOTS_PER_CPU] };
                MAX_TRACKED_CPUS],
            ready: [const { CachePadded::new(AtomicU64::new(0)) }; MAX_TRACKED_CPUS],
            next_sequence: AtomicU64::new(0),
        }
    }

    /// A migrated/unknown producer is still safe: slot CAS owns its record.
    fn local_cpu(&self) -> usize {
        let cpu = crate::util::lockdep::current_cpu_index();
        if cpu < MAX_TRACKED_CPUS { cpu } else { 0 }
    }

}

static DEFERRED: DeferredRing = DeferredRing::new();

/// Park one rendered line. Returns false when the ring is full or the line is
/// longer than a parked record may be; the caller accounts the loss.
pub(super) fn park(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > DEFERRED_LINE_BYTES {
        return false;
    }
    let cpu = DEFERRED.local_cpu();
    for (index, slot) in DEFERRED.cpu_slots[cpu][..DEFERRED_GENERAL_SLOTS_PER_CPU].iter().enumerate() {
        if slot
            .state
            .compare_exchange(
                SLOT_EMPTY,
                SLOT_WRITING,
                // ORDERING: AcqRel claims the slot before any byte is written to it.
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            continue;
        }
        // Sequence is taken after the claim and orders the ready records.
        // A paused publisher is invisible until its final ready-bit release;
        // it cannot hold up other CPUs' already-published diagnostics.
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
        // ORDERING: Release makes payload fields visible before Ready state and bitmap bit.
        slot.state.store(SLOT_READY, Ordering::Release);
        // ORDERING: Release publishes this complete record to bitmap readers.
        // No drainer may inspect a Ready slot before this final publication.
        DEFERRED.ready[cpu].fetch_or(1_u64 << index, Ordering::Release);
        return true;
    }
    false
}

/// Park one milestone that could not reach the sink.
pub(super) fn park_milestone(parked: ParkedMilestone) -> bool {
    let cpu = DEFERRED.local_cpu();
    let capacity = if parked.output_class.must_reach_sink() {
        DEFERRED_SLOTS_PER_CPU
    } else {
        DEFERRED_GENERAL_SLOTS_PER_CPU
    };
    for (index, slot) in DEFERRED.cpu_slots[cpu][..capacity].iter().enumerate() {
        if slot
            .state
            .compare_exchange(
                SLOT_EMPTY,
                SLOT_WRITING,
                // ORDERING: AcqRel claims the slot before it is written.
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
        // ORDERING: Release publishes milestone payload before Ready state and bitmap bit.
        slot.state.store(SLOT_READY, Ordering::Release);
        // ORDERING: Release publishes this complete record to bitmap readers.
        // No drainer may inspect a Ready slot before this final publication.
        DEFERRED.ready[cpu].fetch_or(1_u64 << index, Ordering::Release);
        return true;
    }
    false
}

/// Emit at most `limit` ready records, oldest first. The caller holds the
/// output sink and is the sole drainer. Producers may continue publishing.
pub(super) fn drain(
    limit: usize,
    mut emit_line: impl FnMut(&[u8]),
    mut emit_milestone: impl FnMut(ParkedMilestone),
) -> usize {
    let mut emitted = 0;
    while emitted < limit {
        let mut oldest: Option<(u64, usize, usize)> = None;
        for (cpu, ready) in DEFERRED.ready.iter().enumerate() {
            // ORDERING: Acquire observes complete records published by park.
            let mut pending = ready.load(Ordering::Acquire);
            while pending != 0 {
                let index = pending.trailing_zeros() as usize;
                pending &= pending - 1;
                let slot = &DEFERRED.cpu_slots[cpu][index];
                let sequence = slot.sequence.load(Ordering::Relaxed);
                if oldest.is_none_or(|(oldest_sequence, _, _)| sequence < oldest_sequence) {
                    oldest = Some((sequence, cpu, index));
                }
            }
        }
        let Some((_, cpu, index)) = oldest else { break; };
        let slot = &DEFERRED.cpu_slots[cpu][index];
        // SAFETY: the sink is held, so this is the only drainer. Publication
        // was acquired above and this slot stays Ready until emission ends.
        if slot.kind.load(Ordering::Relaxed) == KIND_MILESTONE {
            if let Some(parked) = unsafe { *slot.milestone.get() } {
                emit_milestone(parked);
            }
        } else {
            let len = (slot.len.load(Ordering::Relaxed) as usize).min(DEFERRED_LINE_BYTES);
            // SAFETY: as above; len is clamped to the buffer size.
            let bytes = unsafe { core::slice::from_raw_parts(slot.bytes.get().cast::<u8>(), len) };
            emit_line(bytes);
        }
        // ORDERING: clear publication before releasing the slot. A parker
        // cannot reuse it until Empty, so its next ready bit cannot be lost.
        DEFERRED.ready[cpu].fetch_and(!(1_u64 << index), Ordering::Relaxed);
        slot.state.store(SLOT_EMPTY, Ordering::Release);
        emitted += 1;
    }
    emitted
}

#[cfg(test)]
mod tests {
    use super::{
        DEFERRED, DEFERRED_GENERAL_SLOTS_PER_CPU, DEFERRED_LINE_BYTES,
        DEFERRED_REQUIRED_RESERVE_PER_CPU, ParkedMilestone, drain, park, park_milestone,
    };
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
        drain(usize::MAX, |bytes| seen.push(bytes.to_vec()), |_| {});
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
    fn drain_budget_retains_records_and_cannot_be_extended_by_new_producers() {
        let _serial = serialized();
        assert!(park(b"first"));
        assert!(park(b"second"));
        assert!(park(b"third"));
        let mut seen = Vec::new();
        let emitted = drain(2, |bytes| {
            seen.push(bytes.to_vec());
            assert!(park(b"concurrent"));
        }, |_| {});
        assert_eq!(emitted, 2);
        assert_eq!(seen, vec![b"first".to_vec(), b"second".to_vec()]);
        assert_eq!(drain_all(), vec![b"third".to_vec(), b"concurrent".to_vec(), b"concurrent".to_vec()]);
        // ORDERING: test assertion under serialized mutex; Acquire observes cleared bits.
        assert!(DEFERRED.ready.iter().all(|bits| bits.load(core::sync::atomic::Ordering::Acquire) == 0));
        assert!(park(b"reused"));
        assert_eq!(drain_all(), vec![b"reused".to_vec()]);
    }

    #[test]
    fn zero_budget_does_not_consume_published_custody() {
        let _serial = serialized();
        assert!(park(b"retained"));
        assert_eq!(drain(0, |_| panic!("zero budget emitted a line"), |_| panic!("zero budget emitted a milestone")), 0);
        assert_eq!(drain_all(), vec![b"retained".to_vec()]);
    }

    #[test]
    fn a_full_ring_refuses_rather_than_overwriting_a_parked_line() {
        let _serial = serialized();
        for _ in 0..DEFERRED_GENERAL_SLOTS_PER_CPU {
            assert!(park(b"x"));
        }
        // The caller must account this loss; silently dropping the oldest
        // record would make the gap invisible.
        assert!(!park(b"x"));
        assert_eq!(drain_all().len(), DEFERRED_GENERAL_SLOTS_PER_CPU);
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
        for _ in 0..DEFERRED_GENERAL_SLOTS_PER_CPU {
            assert!(park(b"x"));
        }
        assert!(!park(b"x"));

        let remaining = DEFERRED
            .cpu_slots
            .iter()
            .skip(1)
            .flat_map(|slots| slots.iter())
            .filter(|slot| {
                // ORDERING: test assertion under serialized mutex; Acquire reads published slot state.
                slot.state.load(core::sync::atomic::Ordering::Acquire) != super::SLOT_EMPTY
            })
            .count();
        assert_eq!(remaining, 0);
        assert_eq!(drain_all().len(), DEFERRED_GENERAL_SLOTS_PER_CPU);
    }

    #[test]
    fn best_effort_milestones_cannot_consume_required_reserve() {
        let _serial = serialized();
        let ordinary = ParkedMilestone {
            record: super::super::MilestoneRecord::EMPTY,
            user_context: None,
            output_class: super::MilestoneOutputClass::BestEffort,
        };
        for _ in 0..DEFERRED_GENERAL_SLOTS_PER_CPU {
            assert!(park_milestone(ordinary));
        }
        assert!(!park_milestone(ordinary));
        let required = ParkedMilestone {
            output_class: super::MilestoneOutputClass::Required,
            ..ordinary
        };
        for _ in 0..DEFERRED_REQUIRED_RESERVE_PER_CPU {
            assert!(park_milestone(required));
        }
        assert!(!park_milestone(required));
        let mut ordinary_seen = 0;
        let mut required_seen = 0;
        drain(usize::MAX, |_| {}, |record| {
            if record.output_class.must_reach_sink() { required_seen += 1; }
            else { ordinary_seen += 1; }
        });
        assert_eq!(ordinary_seen, DEFERRED_GENERAL_SLOTS_PER_CPU);
        assert_eq!(required_seen, DEFERRED_REQUIRED_RESERVE_PER_CPU);
    }

    #[test]
    fn ordinary_burst_cannot_consume_required_milestone_reserve() {
        let _serial = serialized();
        for _ in 0..DEFERRED_GENERAL_SLOTS_PER_CPU {
            assert!(park(b"ordinary"));
        }
        assert!(!park(b"ordinary"));
        for _ in 0..DEFERRED_REQUIRED_RESERVE_PER_CPU {
            assert!(park_milestone(ParkedMilestone {
                record: super::super::MilestoneRecord::EMPTY,
                user_context: None,
                output_class: super::MilestoneOutputClass::Required,
            }));
        }
        assert!(!park_milestone(ParkedMilestone {
            record: super::super::MilestoneRecord::EMPTY,
            user_context: None,
            output_class: super::MilestoneOutputClass::Required,
        }));
        let mut milestones = 0;
        super::drain(usize::MAX, |_| {}, |_| milestones += 1);
        assert_eq!(milestones, DEFERRED_REQUIRED_RESERVE_PER_CPU);
    }
}
