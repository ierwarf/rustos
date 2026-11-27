//! Lock-free per-CPU kernel tail-event ring for IPC outlier attribution (T1).
//!
//! - **Owner:** `nucleus_core::debug` owns storage and milestone draining; kernel
//!   callers in compat, ipc-runtime, and scheduler record bounded events only when
//!   `rustos_ipc_phase_profile` is enabled.
//! - **Boundary:** Pure diagnostic ring. No scheduling, admission, wake, or
//!   timeout path reads this ring. Compiled down to zero-cost no-ops on shipping
//!   builds (`not(rustos_ipc_phase_profile)`).
//! - **Concurrency:** Task-context events and IRQ/IPI events write disjoint
//!   per-CPU rings (`TASK_RINGS[cpu]` vs `IRQ_RINGS[cpu]`) so an interrupt
//!   preempting a task write cannot overwrite or tear the interrupted write.
//!   Overwrite wraps count deterministically on drain without atomic contention on
//!   the record path.

#[cfg(rustos_ipc_phase_profile)]
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(rustos_ipc_phase_profile)]
use super::{LogCategory, record_milestone};
#[cfg(rustos_ipc_phase_profile)]
use crate::util::lockdep::{MAX_TRACKED_CPUS, current_cpu_index};

/// Magic `SYS_RUSTOS_PHASE_PROFILE_DRAIN` `arg0` selecting full tail-ring drain.
pub const TAIL_DRAIN_MAGIC: u64 = 0x5441_494c;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpcTailEventKind {
    EnqueueReq = 1,
    WakeTarget = 2,
    SwitchOut = 3,
    SwitchIn = 4,
    DirectHandoff = 5,
    ReplyPublish = 6,
    CallerWakeRetry = 7,
    IrqEntry = 8,
    RescheduleIpi = 9,
    QueueFullRetry = 10,
    WaitArm = 11,
    WaitResume = 12,
    SchedulerAck = 13,
    UserCopyOrValidate = 14,
}

impl IpcTailEventKind {
    #[cfg(any(test, rustos_ipc_phase_profile))]
    const COUNT: usize = 15;
}

#[cfg(rustos_ipc_phase_profile)]
const RING_SLOTS: usize = 1024;

#[cfg(rustos_ipc_phase_profile)]
const MAX_DRAINED_EVENTS: usize = 32;

#[cfg(rustos_ipc_phase_profile)]
struct TailSlot {
    seq_tag: AtomicU64,
    tsc: AtomicU64,
    meta: AtomicU64,
    arg0: AtomicU64,
    arg1: AtomicU64,
}

#[cfg(rustos_ipc_phase_profile)]
impl TailSlot {
    const fn new() -> Self {
        Self {
            seq_tag: AtomicU64::new(0),
            tsc: AtomicU64::new(0),
            meta: AtomicU64::new(0),
            arg0: AtomicU64::new(0),
            arg1: AtomicU64::new(0),
        }
    }
}

#[cfg(rustos_ipc_phase_profile)]
struct PerCpuTailRing {
    head: AtomicU64,
    drained_head: AtomicU64,
    slots: [TailSlot; RING_SLOTS],
}

#[cfg(rustos_ipc_phase_profile)]
impl PerCpuTailRing {
    const fn new() -> Self {
        Self {
            head: AtomicU64::new(0),
            drained_head: AtomicU64::new(0),
            slots: [const { TailSlot::new() }; RING_SLOTS],
        }
    }

    #[inline]
    fn record(
        &self,
        is_irq: bool,
        cpu: usize,
        kind: IpcTailEventKind,
        flags: u8,
        task_slot: u32,
        arg0: u64,
        arg1: u64,
    ) {
        let tsc = super::phase_profile::phase_now();
        let seq = self.head.fetch_add(1, Ordering::Relaxed);
        let slot = &self.slots[(seq as usize) & (RING_SLOTS - 1)];
        let meta = ((is_irq as u64) << 56)
            | (((cpu as u64) & 0xff) << 48)
            | (((kind as u64) & 0xff) << 40)
            | (((flags as u64) & 0xff) << 32)
            | (task_slot as u64);
        slot.tsc.store(tsc, Ordering::Relaxed);
        slot.meta.store(meta, Ordering::Relaxed);
        slot.arg0.store(arg0, Ordering::Relaxed);
        slot.arg1.store(arg1, Ordering::Relaxed);
        slot.seq_tag.store(seq.wrapping_add(1), Ordering::Release);
    }

    fn reset(&self) {
        let head = self.head.load(Ordering::Acquire);
        self.drained_head.store(head, Ordering::Release);
    }

    fn drain_into(
        &self,
        kind_counts: &mut [u64; IpcTailEventKind::COUNT],
        emit_budget: &mut usize,
        min_gap_tsc: u64,
    ) -> (u64, u64, usize) {
        let head = self.head.load(Ordering::Acquire);
        let prev = self.drained_head.swap(head, Ordering::AcqRel);
        let total = head.saturating_sub(prev);
        if total == 0 {
            return (0, 0, 0);
        }
        let overwritten = total.saturating_sub(RING_SLOTS as u64);
        let start = head.saturating_sub(total.min(RING_SLOTS as u64));
        let mut emitted = 0usize;
        let mut last_tsc = 0u64;
        // Pass 1: accumulate exact counts and find top-gap / anomaly events in the retained tail.
        let mut best_seqs = [(0u64, 0u64); 8];
        for seq in start..head {
            let slot = &self.slots[(seq as usize) & (RING_SLOTS - 1)];
            let tag = slot.seq_tag.load(Ordering::Acquire);
            if tag != seq.wrapping_add(1) {
                continue;
            }
            let tsc = slot.tsc.load(Ordering::Relaxed);
            let meta = slot.meta.load(Ordering::Relaxed);
            let kind_idx = ((meta >> 40) & 0xff) as usize;
            if kind_idx < kind_counts.len() {
                kind_counts[kind_idx] += 1;
            }
            let gap = if last_tsc != 0 {
                tsc.saturating_sub(last_tsc)
            } else {
                0
            };
            last_tsc = tsc;
            // Prioritize retries/queue-full/waits alongside large timestamp gaps.
            let priority_bonus = match kind_idx {
                7 | 9 | 10 => 1_000_000_000u64,
                _ => 0,
            };
            let score = gap.saturating_add(priority_bonus);
            if min_gap_tsc == 0 || gap >= min_gap_tsc || priority_bonus > 0 {
                if let Some(min_pos) = best_seqs
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, entry)| entry.0)
                    .map(|(idx, _)| idx)
                {
                    if score > best_seqs[min_pos].0 {
                        best_seqs[min_pos] = (score, seq);
                    }
                }
            }
        }
        // Pass 2: emit highest-score events within budget (or fall back to tail end).
        for &(score, seq) in &best_seqs {
            if score == 0 || *emit_budget == 0 {
                continue;
            }
            let slot = &self.slots[(seq as usize) & (RING_SLOTS - 1)];
            if slot.seq_tag.load(Ordering::Acquire) != seq.wrapping_add(1) {
                continue;
            }
            let tsc = slot.tsc.load(Ordering::Relaxed);
            let meta = slot.meta.load(Ordering::Relaxed);
            let arg0 = slot.arg0.load(Ordering::Relaxed);
            let arg1 = slot.arg1.load(Ordering::Relaxed);
            record_milestone(LogCategory::Compat, "ipc-tail-ev-a", meta, tsc);
            record_milestone(LogCategory::Compat, "ipc-tail-ev-b", arg0, arg1);
            *emit_budget -= 1;
            emitted += 2;
        }
        (total, overwritten, emitted)
    }
}

#[cfg(rustos_ipc_phase_profile)]
static TASK_RINGS: [PerCpuTailRing; MAX_TRACKED_CPUS] =
    [const { PerCpuTailRing::new() }; MAX_TRACKED_CPUS];

#[cfg(rustos_ipc_phase_profile)]
static IRQ_RINGS: [PerCpuTailRing; MAX_TRACKED_CPUS] =
    [const { PerCpuTailRing::new() }; MAX_TRACKED_CPUS];

/// Records one task-context diagnostic event in the current CPU's tail ring.
#[inline]
pub fn record_task_tail_event(
    kind: IpcTailEventKind,
    flags: u8,
    task_slot: u32,
    arg0: u64,
    arg1: u64,
) {
    #[cfg(rustos_ipc_phase_profile)]
    {
        let cpu = current_cpu_index() % MAX_TRACKED_CPUS;
        TASK_RINGS[cpu].record(false, cpu, kind, flags, task_slot, arg0, arg1);
    }
    #[cfg(not(rustos_ipc_phase_profile))]
    {
        let _ = (kind, flags, task_slot, arg0, arg1);
    }
}

/// Records one interrupt/IPI diagnostic event in the current CPU's disjoint IRQ tail ring.
#[inline]
pub fn record_irq_tail_event(
    kind: IpcTailEventKind,
    flags: u8,
    task_slot: u32,
    arg0: u64,
    arg1: u64,
) {
    #[cfg(rustos_ipc_phase_profile)]
    {
        let cpu = current_cpu_index() % MAX_TRACKED_CPUS;
        IRQ_RINGS[cpu].record(true, cpu, kind, flags, task_slot, arg0, arg1);
    }
    #[cfg(not(rustos_ipc_phase_profile))]
    {
        let _ = (kind, flags, task_slot, arg0, arg1);
    }
}

/// Resets per-CPU tail rings without emitting milestones (used at probe entry).
pub fn reset_ipc_tail_rings() {
    #[cfg(rustos_ipc_phase_profile)]
    {
        for ring in TASK_RINGS.iter().chain(IRQ_RINGS.iter()) {
            ring.reset();
        }
    }
}

/// Drains per-CPU tail rings outside measurement windows.
///
/// When `emit_events` is false, this behaves like `reset_ipc_tail_rings`.
/// When `emit_events` is true, emits `ipc-tail-summary`, nonzero `ipc-tail-kind`
/// totals, and up to 32 filtered `ipc-tail-ev-a`/`ipc-tail-ev-b` pairs.
pub fn drain_ipc_tail_rings(emit_events: bool, min_gap_tsc: u64) -> usize {
    #[cfg(not(rustos_ipc_phase_profile))]
    {
        let _ = (emit_events, min_gap_tsc);
        0
    }
    #[cfg(rustos_ipc_phase_profile)]
    {
        if !emit_events {
            reset_ipc_tail_rings();
            return 0;
        }
        let mut kind_counts = [0u64; IpcTailEventKind::COUNT];
        let mut total_events = 0u64;
        let mut overwritten_events = 0u64;
        let mut emit_budget = MAX_DRAINED_EVENTS;
        let mut emitted = 0usize;
        for ring in TASK_RINGS.iter().chain(IRQ_RINGS.iter()) {
            let (total, over, ev_emitted) =
                ring.drain_into(&mut kind_counts, &mut emit_budget, min_gap_tsc);
            total_events = total_events.saturating_add(total);
            overwritten_events = overwritten_events.saturating_add(over);
            emitted += ev_emitted;
        }
        record_milestone(
            LogCategory::Compat,
            "ipc-tail-summary",
            total_events,
            overwritten_events,
        );
        emitted += 1;
        for (kind, count) in kind_counts.iter().enumerate() {
            if *count != 0 {
                record_milestone(LogCategory::Compat, "ipc-tail-kind", kind as u64, *count);
                emitted += 1;
            }
        }
        emitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_event_kind_discriminants_stay_within_bounds() {
        let all = [
            IpcTailEventKind::EnqueueReq,
            IpcTailEventKind::WakeTarget,
            IpcTailEventKind::SwitchOut,
            IpcTailEventKind::SwitchIn,
            IpcTailEventKind::DirectHandoff,
            IpcTailEventKind::ReplyPublish,
            IpcTailEventKind::CallerWakeRetry,
            IpcTailEventKind::IrqEntry,
            IpcTailEventKind::RescheduleIpi,
            IpcTailEventKind::QueueFullRetry,
            IpcTailEventKind::WaitArm,
            IpcTailEventKind::WaitResume,
            IpcTailEventKind::SchedulerAck,
            IpcTailEventKind::UserCopyOrValidate,
        ];
        for kind in all {
            assert!((kind as usize) < IpcTailEventKind::COUNT);
        }
    }
}
