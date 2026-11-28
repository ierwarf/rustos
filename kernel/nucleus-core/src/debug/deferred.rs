//! Bounded records for the background debugcon consumer and early sink contention.
//!
//! - **Owner:** `nucleus-core::debug` owns per-CPU deferred line/milestone slots
//!   and the sink handoff sequence counter.
//! - **Boundary:** Accepts owned diagnostic lines and milestones without
//!   touching the output sink or waiting for a consumer.
//! - **Lifecycle:** Claims an occupied bit, writes the bounded slot payload,
//!   publishes its ready bit, and returns the slot only after consumption.
//! - **Concurrency:** Per-CPU occupied/ready bitmaps serialize slot reuse and
//!   Release/Acquire publication; producers never contend for a global sink.
//! - **Failure:** Full per-CPU rings fail closed and increment overflow
//!   accounting rather than overwriting parked custody.
//! - **Forbidden:** Spinning on another CPU while holding/seeking the sink,
//!   silently overwriting un-emitted records, or cross-CPU burst starvation.
//! - **Evidence:** `kernel/nucleus-core` deferred ring unit tests.
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
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
// A single-vCPU boot can produce over a hundred records before the fair
// consumer runs again. Do not put peak storage in the file-backed boot ELF.
const BITS_PER_WORD: usize = u64::BITS as usize;
const DEFERRED_SLOTS_PER_CPU: usize = 4 * BITS_PER_WORD;
// CPU0 absorbs the entire burst when there is no spare vCPU for the
// consumer. Provision extra storage on that CPU only.
const BSP_SLOTS: usize = 8 * BITS_PER_WORD;
const BITMAP_WORDS: usize = BSP_SLOTS / BITS_PER_WORD;
/// Ordinary diagnostics cannot consume these final slots. Required milestone
/// evidence is produced from scheduler/IRQ contexts where waiting for the
/// sink is forbidden, so it needs a bounded private reserve during the dense
/// multi-vCPU startup burst.
const DEFERRED_REQUIRED_RESERVE_PER_CPU: usize = 32;
#[cfg(test)]
const BSP_GENERAL_SLOTS: usize = BSP_SLOTS - DEFERRED_REQUIRED_RESERVE_PER_CPU;
/// Longest line the ring accepts, matching the serialized diagnostic bound.
pub(super) const DEFERRED_LINE_BYTES: usize = 512;

/// An enqueued milestone keeps its record, not its rendered bytes.
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
    kind: AtomicU8,
    sequence: AtomicU64,
    len: AtomicU32,
    bytes: UnsafeCell<[u8; DEFERRED_LINE_BYTES]>,
    milestone: UnsafeCell<MaybeUninit<ParkedMilestone>>,
}

impl Slot {
    const fn new() -> Self {
        Self {
            kind: AtomicU8::new(KIND_LINE),
            sequence: AtomicU64::new(0),
            len: AtomicU32::new(0),
            bytes: UnsafeCell::new([0; DEFERRED_LINE_BYTES]),
            milestone: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

// SAFETY: only the producer that claims an occupied bit writes that slot.
// The sole drainer acquires the ready bit, then clears it before releasing
// the occupied bit. This excludes reuse for the duration of a payload read.
unsafe impl Sync for Slot {}

struct DeferredRing {
    /// One private slot array per CPU. `next_sequence` stays global: it is what
    /// lets the drainer merge the rings back into the order the records were
    /// parked in, across CPUs.
    cpu_slots: [[Slot; DEFERRED_SLOTS_PER_CPU]; MAX_TRACKED_CPUS],
    bsp_extra: [Slot; BSP_SLOTS - DEFERRED_SLOTS_PER_CPU],
    occupied: [CachePadded<[AtomicU64; BITMAP_WORDS]>; MAX_TRACKED_CPUS],
    // Ready is final publication; it must clear before occupied clears, so
    // the next producer cannot have its publication erased by retirement.
    ready: [CachePadded<[AtomicU64; BITMAP_WORDS]>; MAX_TRACKED_CPUS],
    next_sequence: AtomicU64,
}

impl DeferredRing {
    const fn new() -> Self {
        Self {
            cpu_slots: [const { [const { Slot::new() }; DEFERRED_SLOTS_PER_CPU] };
                MAX_TRACKED_CPUS],
            bsp_extra: [const { Slot::new() }; BSP_SLOTS - DEFERRED_SLOTS_PER_CPU],
            occupied: [const { CachePadded::new([const { AtomicU64::new(0) }; BITMAP_WORDS]) };
                MAX_TRACKED_CPUS],
            ready: [const { CachePadded::new([const { AtomicU64::new(0) }; BITMAP_WORDS]) };
                MAX_TRACKED_CPUS],
            next_sequence: AtomicU64::new(0),
        }
    }

    /// A migrated/unknown producer is still safe: slot CAS owns its record.
    fn local_cpu(&self) -> usize {
        let cpu = crate::util::lockdep::current_cpu_index();
        if cpu < MAX_TRACKED_CPUS { cpu } else { 0 }
    }

    const fn slot_count(cpu: usize) -> usize {
        if cpu == 0 { BSP_SLOTS } else { DEFERRED_SLOTS_PER_CPU }
    }

    fn slot(&self, cpu: usize, index: usize) -> &Slot {
        if cpu == 0 && index >= DEFERRED_SLOTS_PER_CPU {
            &self.bsp_extra[index - DEFERRED_SLOTS_PER_CPU]
        } else {
            &self.cpu_slots[cpu][index]
        }
    }

    fn enqueue(&self, required: bool, fill: impl FnOnce(&Slot)) -> bool {
        let cpu = self.local_cpu();
        let reserved = if required { 0 } else { DEFERRED_REQUIRED_RESERVE_PER_CPU };
        let capacity = Self::slot_count(cpu) - reserved;
        for (word, occupied) in self.occupied[cpu].iter().enumerate() {
            let base = word * BITS_PER_WORD;
            let allowed = match capacity.saturating_sub(base) {
                0 => break,
                count if count >= BITS_PER_WORD => u64::MAX,
                count => (1_u64 << count) - 1,
            };
            let mut available = !occupied.load(Ordering::Relaxed) & allowed;
            while available != 0 {
                let offset = available.trailing_zeros() as usize;
                let bit = 1_u64 << offset;
                available &= !bit;
                // ORDERING: Acquire pairs with release at reuse; only the
                // zero-to-one winner can touch payload. Try each slot once.
                if occupied.fetch_or(bit, Ordering::AcqRel) & bit != 0 {
                    continue;
                }
                let slot = self.slot(cpu, base + offset);
                // ORDERING: AcqRel allocation and an Acquire batch cutoff
                // order already-published cross-CPU predecessors.
                slot.sequence.store(
                    self.next_sequence.fetch_add(1, Ordering::AcqRel),
                    Ordering::Relaxed,
                );
                fill(slot);
                // ORDERING: Release publishes the complete initialized slot.
                self.ready[cpu][word].fetch_or(bit, Ordering::Release);
                return true;
            }
        }
        false
    }
}

static DEFERRED: DeferredRing = DeferredRing::new();

/// Park one rendered line. Returns false when the ring is full or the line is
/// longer than a parked record may be; the caller accounts the loss.
pub(super) fn park(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > DEFERRED_LINE_BYTES {
        return false;
    }
    DEFERRED.enqueue(false, |slot| {
        // SAFETY: enqueue owns the occupied bit and has not published Ready.
        unsafe { (&mut *slot.bytes.get())[..bytes.len()].copy_from_slice(bytes) };
        slot.len.store(bytes.len() as u32, Ordering::Relaxed);
        slot.kind.store(KIND_LINE, Ordering::Relaxed);
    })
}

/// Queue one milestone without formatting or touching the sink.
pub(super) fn park_milestone(parked: ParkedMilestone) -> bool {
    DEFERRED.enqueue(parked.output_class.must_reach_sink(), |slot| {
        // SAFETY: enqueue owns the occupied bit and will publish only after
        // both value and kind are initialized.
        unsafe { (*slot.milestone.get()).write(parked) };
        slot.kind.store(KIND_MILESTONE, Ordering::Relaxed);
    })
}

pub(super) enum ReadyRecord<'a> {
    Line(&'a [u8]),
    Milestone(ParkedMilestone),
}

/// Consume at most `limit` ready records, oldest currently published first.
/// A single cutoff excludes records allocated while scanning, so a freshly
/// published cross-CPU successor cannot jump over an earlier scanned CPU.
/// The caller owns the sink. Producers may continue publishing.
pub(super) fn drain(limit: usize, mut emit: impl FnMut(ReadyRecord<'_>)) -> usize {
    let cutoff = DEFERRED.next_sequence.load(Ordering::Acquire);
    let mut emitted = 0;
    while emitted < limit {
        let mut oldest: Option<(u64, usize, usize)> = None;
        for (cpu, ready) in DEFERRED.ready.iter().enumerate() {
            let words = DeferredRing::slot_count(cpu) / BITS_PER_WORD;
            for (word, ready) in ready[..words].iter().enumerate() {
                // ORDERING: Acquire observes complete records published by enqueue.
                let mut pending = ready.load(Ordering::Acquire);
                while pending != 0 {
                    let index = word * BITS_PER_WORD + pending.trailing_zeros() as usize;
                    pending &= pending - 1;
                    let slot = DEFERRED.slot(cpu, index);
                    let sequence = slot.sequence.load(Ordering::Relaxed);
                    if sequence < cutoff
                        && oldest.is_none_or(|(oldest_sequence, _, _)| sequence < oldest_sequence)
                    {
                        oldest = Some((sequence, cpu, index));
                    }
                }
            }
        }
        let Some((_, cpu, index)) = oldest else { break; };
        let slot = DEFERRED.slot(cpu, index);
        // SAFETY: the sink is held, so this is the only drainer. Publication
        // was acquired above and this slot stays Ready until emission ends.
        if slot.kind.load(Ordering::Relaxed) == KIND_MILESTONE {
            // SAFETY: this kind was published together with a fully filled
            // milestone, and the occupied bit excludes reuse.
            let parked = unsafe { (*slot.milestone.get()).assume_init_read() };
            emit(ReadyRecord::Milestone(parked));
        } else {
            let len = (slot.len.load(Ordering::Relaxed) as usize).min(DEFERRED_LINE_BYTES);
            // SAFETY: as above; len is clamped to the buffer size.
            let bytes = unsafe { core::slice::from_raw_parts(slot.bytes.get().cast::<u8>(), len) };
            emit(ReadyRecord::Line(bytes));
        }
        let word = index / BITS_PER_WORD;
        let bit = 1_u64 << (index % BITS_PER_WORD);
        // ORDERING: clear publication before returning ownership for reuse.
        DEFERRED.ready[cpu][word].fetch_and(!bit, Ordering::Relaxed);
        DEFERRED.occupied[cpu][word].fetch_and(!bit, Ordering::Release);
        emitted += 1;
    }
    emitted
}

#[cfg(test)]
mod tests {
    use super::{
        BSP_GENERAL_SLOTS, DEFERRED, DEFERRED_LINE_BYTES,
        DEFERRED_REQUIRED_RESERVE_PER_CPU, ParkedMilestone, ReadyRecord, drain, park, park_milestone,
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
        drain(usize::MAX, |record| {
            if let ReadyRecord::Line(bytes) = record {
                seen.push(bytes.to_vec());
            }
        });
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
        let emitted = drain(2, |record| {
            if let ReadyRecord::Line(bytes) = record {
                seen.push(bytes.to_vec());
                assert!(park(b"concurrent"));
            }
        });
        assert_eq!(emitted, 2);
        assert_eq!(seen, vec![b"first".to_vec(), b"second".to_vec()]);
        assert_eq!(drain_all(), vec![b"third".to_vec(), b"concurrent".to_vec(), b"concurrent".to_vec()]);
        // ORDERING: test assertion under serialized mutex; Acquire observes cleared bits.
        assert!(DEFERRED.ready.iter().all(|words| words.iter().all(|bits| bits.load(core::sync::atomic::Ordering::Acquire) == 0)));
        assert!(park(b"reused"));
        assert_eq!(drain_all(), vec![b"reused".to_vec()]);
    }

    #[test]
    fn records_admitted_inside_a_turn_wait_for_the_next_cutoff() {
        let _serial = serialized();
        assert!(park(b"first"));
        let emitted = drain(8, |record| {
            assert!(matches!(record, ReadyRecord::Line(b"first")));
            assert!(park(b"later"));
        });
        assert_eq!(emitted, 1);
        assert_eq!(drain_all(), vec![b"later".to_vec()]);
    }

    #[test]
    fn zero_budget_does_not_consume_published_custody() {
        let _serial = serialized();
        assert!(park(b"retained"));
        assert_eq!(drain(0, |_| panic!("zero budget emitted a record")), 0);
        assert_eq!(drain_all(), vec![b"retained".to_vec()]);
    }

    #[test]
    fn a_full_ring_refuses_rather_than_overwriting_a_parked_line() {
        let _serial = serialized();
        for _ in 0..BSP_GENERAL_SLOTS {
            assert!(park(b"x"));
        }
        // The caller must account this loss; silently dropping the oldest
        // record would make the gap invisible.
        assert!(!park(b"x"));
        assert_eq!(drain_all().len(), BSP_GENERAL_SLOTS);
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
        for _ in 0..BSP_GENERAL_SLOTS {
            assert!(park(b"x"));
        }
        assert!(!park(b"x"));

        // ORDERING: serialized test; Acquire snapshots slot custody.
        assert!(DEFERRED.occupied.iter().skip(1).all(|words| words.iter().all(
            |bits| bits.load(core::sync::atomic::Ordering::Acquire) == 0
        )));
        assert_eq!(drain_all().len(), BSP_GENERAL_SLOTS);
    }

    #[test]
    fn concurrent_claims_publish_each_owned_line_once() {
        let _serial = serialized();
        std::thread::scope(|scope| {
            for producer in 0..4_u8 {
                scope.spawn(move || {
                    for record in 0..32_u8 {
                        assert!(park(&[producer, record, producer ^ record]));
                    }
                });
            }
        });
        let mut seen = drain_all();
        seen.sort_unstable();
        let expected: Vec<_> = (0..4_u8)
            .flat_map(|producer| (0..32_u8).map(move |record| vec![producer, record, producer ^ record]))
            .collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn best_effort_milestones_cannot_consume_required_reserve() {
        let _serial = serialized();
        let ordinary = ParkedMilestone {
            record: super::super::MilestoneRecord::EMPTY,
            user_context: None,
            output_class: super::MilestoneOutputClass::BestEffort,
        };
        for _ in 0..BSP_GENERAL_SLOTS {
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
        drain(usize::MAX, |record| {
            if let ReadyRecord::Milestone(record) = record {
                if record.output_class.must_reach_sink() { required_seen += 1; }
                else { ordinary_seen += 1; }
            }
        });
        assert_eq!(ordinary_seen, BSP_GENERAL_SLOTS);
        assert_eq!(required_seen, DEFERRED_REQUIRED_RESERVE_PER_CPU);
    }

    #[test]
    fn ordinary_burst_cannot_consume_required_milestone_reserve() {
        let _serial = serialized();
        for _ in 0..BSP_GENERAL_SLOTS {
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
        drain(usize::MAX, |record| {
            if matches!(record, ReadyRecord::Milestone(_)) {
                milestones += 1;
            }
        });
        assert_eq!(milestones, DEFERRED_REQUIRED_RESERVE_PER_CPU);
    }
}
