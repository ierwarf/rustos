//! Custody conservation check: a runnable task is somewhere a dispatcher finds.
//!
//! - **Owner:** observation only. [`scan`] reads owner words, the published rq
//!   mirrors, mailbox / FIFO pending edges and the CPU execution owners; it
//!   writes nothing but its own suspect table and diagnostics.
//! - **Rule:** a word that names run custody must have its record: `Local` a
//!   published rq membership, `RemoteQueued` a pending mailbox edge,
//!   `DirectHandoff` a FIFO record of its generation (checked under the FIFO
//!   leaf only for an aged suspect), `Running` a CPU executing it; `Migrating`
//!   is transient and `Blocked` never carries run intent. A gap whose exact word
//!   stays unchanged for [`THRESHOLD_TICKS`] is a lost enqueue (I1).
//! - **Concurrency:** lock-free; one scanner at a time by a try-flag, no wait,
//!   no scheduler lock, so it may run from the timer or idle path.
//! - **Failure:** reports `scheduler-custody-lost` + the slot's flight ring as
//!   bounded, rate-limited milestones. Never panics, never repairs.
//! - **Evidence:** `conservation::tests`; xtask classifies the milestone as its
//!   own failure kind (`tools/xtask/src/kvm/evidence.rs`).

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::ownership::{self, FLIGHT_DEPTH};
use super::{RunOwnerState, MAILBOX_PENDING, MAX_TASK};

/// 2 s at the 1024 Hz scheduler tick: three orders of magnitude above
/// scheduling latency and above the ~13 ms 8-vCPU stalls in the IPC notes.
pub(super) const THRESHOLD_TICKS: u64 = 2048;
/// One scan per quarter second at most, on whichever CPU gets there first.
pub(super) const PERIOD_TICKS: u64 = 256;
const MAX_REPORTS_PER_SCAN: usize = 4;
const MAX_REPORTS_PER_BOOT: usize = 32;

/// Why a word's custody record is missing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(in crate::multitask::scheduler) enum CustodyGap {
    LocalWithoutMembership = 1,
    RemoteWithoutMailbox = 2,
    DirectWithoutFifo = 3,
    MigratingStuck = 4,
    BlockedWithRunIntent = 5,
    RunningWithoutCpu = 6,
}

/// One reported lost-custody observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::multitask::scheduler) struct CustodyReport {
    pub(in crate::multitask::scheduler) slot: usize,
    pub(in crate::multitask::scheduler) gap: CustodyGap,
    pub(in crate::multitask::scheduler) word: u64,
    pub(in crate::multitask::scheduler) age_ticks: u64,
}

static NEXT_SCAN: AtomicU64 = AtomicU64::new(0);
static SCANNING: AtomicBool = AtomicBool::new(false);
static REPORTS: AtomicUsize = AtomicUsize::new(0);
/// Scanner-owned (under `SCANNING`): the exact suspect word, when it was first
/// seen, and the last word reported for the slot.
static SUSPECT_WORD: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static SUSPECT_SINCE: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
static REPORTED_WORD: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];

pub(super) fn reset_before_publication() {
    NEXT_SCAN.store(0, Ordering::Relaxed);
    REPORTS.store(0, Ordering::Relaxed);
    for slot in 0..MAX_TASK {
        SUSPECT_WORD[slot].store(0, Ordering::Relaxed);
        SUSPECT_SINCE[slot].store(0, Ordering::Relaxed);
        REPORTED_WORD[slot].store(0, Ordering::Relaxed);
    }
}

/// Periodic entry for an existing timer / idle path. Cheap when not due: one
/// relaxed load. Emits at most [`MAX_REPORTS_PER_SCAN`] reports per pass and
/// [`MAX_REPORTS_PER_BOOT`] per boot.
pub(in crate::multitask::scheduler) fn scan(now_ticks: u64) {
    // ORDERING: Relaxed; a stale value only delays or repeats a cheap pass.
    if now_ticks < NEXT_SCAN.load(Ordering::Relaxed) {
        return;
    }
    // ORDERING: Acquire/Release bracket the scanner-owned suspect table.
    if SCANNING.swap(true, Ordering::Acquire) {
        return;
    }
    NEXT_SCAN.store(now_ticks.saturating_add(PERIOD_TICKS), Ordering::Relaxed);
    let mut emitted = 0;
    scan_slots(now_ticks, |report| {
        if emitted >= MAX_REPORTS_PER_SCAN
            || REPORTS.fetch_add(1, Ordering::Relaxed) >= MAX_REPORTS_PER_BOOT
        {
            return;
        }
        emitted += 1;
        emit(report);
    });
    SCANNING.store(false, Ordering::Release);
}

/// The classification pass, separated from rate limiting for witnesses. The
/// caller holds `SCANNING` (or is a single-threaded witness).
pub(super) fn scan_slots(now_ticks: u64, mut report: impl FnMut(CustodyReport)) {
    for slot in 0..MAX_TASK {
        let word = ownership::owner_raw(slot);
        let Some(gap) = classify(slot, word) else {
            SUSPECT_WORD[slot].store(0, Ordering::Relaxed);
            continue;
        };
        if SUSPECT_WORD[slot].load(Ordering::Relaxed) != word {
            SUSPECT_WORD[slot].store(word, Ordering::Relaxed);
            SUSPECT_SINCE[slot].store(now_ticks, Ordering::Relaxed);
            continue;
        }
        let age_ticks = now_ticks.saturating_sub(SUSPECT_SINCE[slot].load(Ordering::Relaxed));
        if age_ticks < THRESHOLD_TICKS
            || REPORTED_WORD[slot].load(Ordering::Relaxed) == word
            || !confirm(slot, word, gap)
        {
            continue;
        }
        REPORTED_WORD[slot].store(word, Ordering::Relaxed);
        report(CustodyReport { slot, gap, word, age_ticks });
    }
}

fn classify(slot: usize, word: u64) -> Option<CustodyGap> {
    let owner = super::RunOwnerSnapshot::decode(word);
    match owner.state {
        RunOwnerState::Local => {
            let cpu = owner.cpu?;
            (!super::published_local_membership(slot, cpu)).then_some(CustodyGap::LocalWithoutMembership)
        }
        RunOwnerState::RemoteQueued => {
            let cpu = owner.cpu?;
            // ORDERING: Acquire, as the drainer's own pending load.
            (MAILBOX_PENDING.get(cpu)?.load(Ordering::Acquire) == 0)
                .then_some(CustodyGap::RemoteWithoutMailbox)
        }
        // Every unexecuted DirectHandoff word is tracked (an in-flight fused
        // `DirectSwitch` legitimately has no record within its dispatch turn);
        // `confirm` checks the FIFO only once the exact word has aged.
        RunOwnerState::DirectHandoff => crate::multitask::cpu_local::task_execution_owner(slot)
            .is_none()
            .then_some(CustodyGap::DirectWithoutFifo),
        RunOwnerState::Migrating => Some(CustodyGap::MigratingStuck),
        RunOwnerState::Blocked => owner.runnable.then_some(CustodyGap::BlockedWithRunIntent),
        RunOwnerState::Running => crate::multitask::cpu_local::task_execution_owner(slot)
            .is_none()
            .then_some(CustodyGap::RunningWithoutCpu),
        RunOwnerState::Dormant | RunOwnerState::Retiring | RunOwnerState::Retired => None,
    }
}

/// The exact (leaf-taking) check behind a lock-free suspicion, run only for an
/// aged suspect, never on a dispatch path.
fn confirm(slot: usize, word: u64, gap: CustodyGap) -> bool {
    let owner = super::RunOwnerSnapshot::decode(word);
    match (gap, owner.cpu) {
        (CustodyGap::DirectWithoutFifo, Some(cpu)) => {
            !super::super::sync_handoff::pending(cpu)
                || !super::super::sync_handoff::holds_direct_record(cpu, slot, owner.generation)
        }
        _ => true,
    }
}

fn emit(report: CustodyReport) {
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "scheduler-custody-lost",
        report.slot as u64 | ((report.gap as u64) << 8) | (report.age_ticks.min(u32::MAX.into()) << 16),
        report.word,
    );
    let (ring, len) = ownership::flight_ring(report.slot);
    for (index, entry) in ring[..len].iter().enumerate() {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "scheduler-custody-ring",
            report.slot as u64 | ((index as u64) << 8),
            entry.raw,
        );
    }
}

const _: () = assert!(FLIGHT_DEPTH + 1 <= 10, "ring report must fit one milestone drain");

#[cfg(test)]
mod tests;
