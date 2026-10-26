//! Owner: bounded CPU diagnostic cells and per-slot exact-UID history.
//! Boundary: scalar Class56 leaves; no resource observations or rendering.
//! Lifecycle: consume-once windows retain monotonic open-turn phase baselines.
//! Concurrency: IRQ exclusion precedes CPU sampling; leaves never nest.
//! Failure: busy reporter refuses without consuming; counters saturate.
//! Forbidden: no dispatch/billing/lifetime grants, allocation or lower owners.
//! Evidence: independent cell/epoch/tuple/UID witnesses, not hardware proof.
use super::{
    SCHEDULER_ENTRY_CAUSE_COUNT, SCHEDULER_PHASE_COUNT, SchedulerEntryCause, SchedulerPhase,
};
use crate::multitask::scheduler::{MAX_TASK, NO_IDLE_CPU};
use core::{
    panic::Location,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use nucleus_core::util::lockdep::{self, LockClass};
use crate::multitask::irq_leaf::IrqLeaf;

pub(in crate::multitask) struct Counters {
    pub runtime_profile_started_ticks: u64,
    pub runtime_profile_ns: [u64; MAX_TASK],
    pub runtime_profile_dispatches: [u64; MAX_TASK],
    pub runtime_profile_entry_counts: [u64; SCHEDULER_ENTRY_CAUSE_COUNT],
    pub runtime_profile_same_task_dispatches: u64,
    pub runtime_profile_task_switches: u64,
    pub runtime_profile_address_space_switches: u64,
    pub runtime_profile_cross_cpu_migrations: u64,
    #[cfg(test)]
    pub runtime_profile_last_cpu: [u8; MAX_TASK],
    #[cfg(test)]
    pub runtime_profile_last_task_id: [u64; MAX_TASK],
    pub runtime_profile_lock_acquisitions: u64,
    pub runtime_profile_lock_wait_ns: u64,
    pub runtime_profile_lock_hold_ns: u64,
    pub runtime_profile_lock_wait_max_ns: u64,
    pub runtime_profile_lock_hold_max_ns: u64,
    pub runtime_profile_lock_hold_max_caller: Option<&'static Location<'static>>,
    pub runtime_profile_lock_hold_max_attributed_ns: u64,
    pub runtime_profile_deferred_wakes: u64,
    pub runtime_profile_phase_ns: [u64; SCHEDULER_PHASE_COUNT],
    pub runtime_profile_runnable_samples: u64,
    // Never reset by a report: a remote drain must not spend an open turn's
    // baseline. Window segment attribution and lifetime clocks are distinct.
    pub phase_lifetime_ns: u64,
}
impl Counters {
    pub const fn new() -> Self {
        Self {
            runtime_profile_started_ticks: 0,
            runtime_profile_ns: [0; MAX_TASK],
            runtime_profile_dispatches: [0; MAX_TASK],
            runtime_profile_entry_counts: [0; SCHEDULER_ENTRY_CAUSE_COUNT],
            runtime_profile_same_task_dispatches: 0,
            runtime_profile_task_switches: 0,
            runtime_profile_address_space_switches: 0,
            runtime_profile_cross_cpu_migrations: 0,
            #[cfg(test)]
            runtime_profile_last_cpu: [NO_IDLE_CPU; MAX_TASK],
            #[cfg(test)]
            runtime_profile_last_task_id: [0; MAX_TASK],
            runtime_profile_lock_acquisitions: 0,
            runtime_profile_lock_wait_ns: 0,
            runtime_profile_lock_hold_ns: 0,
            runtime_profile_lock_wait_max_ns: 0,
            runtime_profile_lock_hold_max_ns: 0,
            runtime_profile_lock_hold_max_caller: None,
            runtime_profile_lock_hold_max_attributed_ns: 0,
            runtime_profile_deferred_wakes: 0,
            runtime_profile_phase_ns: [0; SCHEDULER_PHASE_COUNT],
            runtime_profile_runnable_samples: 0,
            phase_lifetime_ns: 0,
        }
    }
    pub fn account(&mut self, slot: usize, ns: u64) {
        self.runtime_profile_ns[slot] = self.runtime_profile_ns[slot].saturating_add(ns);
    }
    pub fn dispatch(&mut self, slot: usize) {
        self.runtime_profile_dispatches[slot] =
            self.runtime_profile_dispatches[slot].saturating_add(1);
    }
    pub fn entry(&mut self, cause: SchedulerEntryCause) {
        let n = &mut self.runtime_profile_entry_counts[cause as usize];
        *n = n.saturating_add(1);
    }
    pub fn phase(&mut self, phase: SchedulerPhase, ns: u64) {
        let n = &mut self.runtime_profile_phase_ns[phase as usize];
        *n = n.saturating_add(ns);
        self.phase_lifetime_ns = self.phase_lifetime_ns.saturating_add(ns);
    }
    #[cfg(test)]
    pub fn wait(&mut self, ns: u64) {
        self.runtime_profile_lock_acquisitions =
            self.runtime_profile_lock_acquisitions.saturating_add(1);
        self.runtime_profile_lock_wait_ns = self.runtime_profile_lock_wait_ns.saturating_add(ns);
        self.runtime_profile_lock_wait_max_ns = self.runtime_profile_lock_wait_max_ns.max(ns);
    }
    #[cfg(test)]
    pub fn hold(&mut self, ns: u64, attributed: u64, caller: &'static Location<'static>) {
        self.runtime_profile_lock_hold_ns = self.runtime_profile_lock_hold_ns.saturating_add(ns);
        if ns > self.runtime_profile_lock_hold_max_ns {
            self.runtime_profile_lock_hold_max_ns = ns;
            self.runtime_profile_lock_hold_max_caller = Some(caller);
            self.runtime_profile_lock_hold_max_attributed_ns = attributed;
        }
    }
    pub(super) fn take(&mut self) -> Self {
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut window = core::mem::replace(self, Self::new());
        self.phase_lifetime_ns = window.phase_lifetime_ns;
        #[cfg(test)]
        {
            core::mem::swap(
                &mut self.runtime_profile_last_cpu,
                &mut window.runtime_profile_last_cpu,
            );
            core::mem::swap(
                &mut self.runtime_profile_last_task_id,
                &mut window.runtime_profile_last_task_id,
            );
        }
        window
    }
    fn merge(&mut self, other: Self) {
        for slot in 0..MAX_TASK {
            self.account(slot, other.runtime_profile_ns[slot]);
            self.runtime_profile_dispatches[slot] = self.runtime_profile_dispatches[slot]
                .saturating_add(other.runtime_profile_dispatches[slot]);
        }
        for (n, v) in self
            .runtime_profile_entry_counts
            .iter_mut()
            .zip(other.runtime_profile_entry_counts)
        {
            *n = n.saturating_add(v);
        }
        for (n, v) in self
            .runtime_profile_phase_ns
            .iter_mut()
            .zip(other.runtime_profile_phase_ns)
        {
            *n = n.saturating_add(v);
        }
        macro_rules! sum {($($field:ident),*) => {$(self.$field=self.$field.saturating_add(other.$field);)*};}
        sum!(
            runtime_profile_same_task_dispatches,
            runtime_profile_task_switches,
            runtime_profile_address_space_switches,
            runtime_profile_cross_cpu_migrations,
            runtime_profile_lock_acquisitions,
            runtime_profile_lock_wait_ns,
            runtime_profile_lock_hold_ns,
            runtime_profile_deferred_wakes,
            runtime_profile_runnable_samples
        );
        self.runtime_profile_lock_wait_max_ns = self
            .runtime_profile_lock_wait_max_ns
            .max(other.runtime_profile_lock_wait_max_ns);
        if other.runtime_profile_lock_hold_max_ns > self.runtime_profile_lock_hold_max_ns {
            self.runtime_profile_lock_hold_max_ns = other.runtime_profile_lock_hold_max_ns;
            self.runtime_profile_lock_hold_max_caller = other.runtime_profile_lock_hold_max_caller;
            self.runtime_profile_lock_hold_max_attributed_ns =
                other.runtime_profile_lock_hold_max_attributed_ns;
        }
    }
}
type Leaf<T> = IrqLeaf<T, { LockClass::SchedulerDiagnostics as u8 }>;
struct Cell(Leaf<Counters>);
impl Cell {
    const fn new() -> Self {
        Self(Leaf::new(Counters::new()))
    }
    fn with<R>(&self, f: impl FnOnce(&mut Counters) -> R) -> R {
        #[cfg(not(test))]
        {
            x86_64::instructions::interrupts::without_interrupts(|| f(&mut self.0.lock()))
        }
        #[cfg(test)]
        {
            f(&mut self.0.lock())
        }
    }
}
const CPUS: usize = lockdep::MAX_TRACKED_CPUS;
static CELLS: [Cell; CPUS] = [const { Cell::new() }; CPUS];
static STARTED: AtomicU64 = AtomicU64::new(0);
static DRAINING: AtomicBool = AtomicBool::new(false);
struct Drain;
impl Drop for Drain {
    fn drop(&mut self) {
        // ORDERING: finish every cell drain before another reporter may begin.
        DRAINING.store(false, Ordering::Release);
    }
}
/// [`local`] for a caller that already excludes interrupts on `cpu` and holds
/// its index (the IPC lane): no second mask and no index derivation.
pub(in crate::multitask::scheduler) fn local_masked<R>(
    cpu: usize,
    f: impl FnOnce(&mut Counters) -> R,
) -> R {
    CELLS[cpu].with(f)
}

pub(in crate::multitask::scheduler) fn local<R>(f: impl FnOnce(&mut Counters) -> R) -> R {
    #[cfg(not(test))]
    {
        x86_64::instructions::interrupts::without_interrupts(|| {
            CELLS[lockdep::current_cpu_index()].with(f)
        })
    }
    #[cfg(test)]
    {
        CELLS[lockdep::current_cpu_index()].with(f)
    }
}
struct History {
    id: Option<u64>,
    cpu: u8,
}
impl History {
    /// `(id + 1) << 8 | cpu` in one word; zero is the empty row. One word
    /// swap replaces the per-slot leaf every dispatch used to take: the row is
    /// a diagnostic tuple, and the swap keeps its id and CPU together.
    fn encode(&self) -> u64 {
        self.id.map_or(0, |id| (id.wrapping_add(1) << 8) | u64::from(self.cpu))
    }
    fn decode(word: u64) -> Self {
        match word {
            0 => Self { id: None, cpu: NO_IDLE_CPU },
            word => Self { id: Some((word >> 8).wrapping_sub(1)), cpu: word as u8 },
        }
    }
}
static HISTORY: [AtomicU64; MAX_TASK] = [const { AtomicU64::new(0) }; MAX_TASK];
pub(in crate::multitask::scheduler) fn migration(slot: usize, id: u64, cpu: u8) -> bool {
    let next = History { id: Some(id), cpu }.encode();
    // ORDERING: Relaxed; a diagnostic tuple published and consumed as one word.
    let row = History::decode(HISTORY[slot].swap(next, Ordering::Relaxed));
    row.id == Some(id) && row.cpu != NO_IDLE_CPU && row.cpu != cpu
}
pub(in crate::multitask::scheduler) fn clear_history(slot: usize) {
    // ORDERING: Relaxed; see `migration`.
    HISTORY[slot].store(0, Ordering::Relaxed);
}
fn due(now: u64, started: u64, period: u64) -> bool {
    now.saturating_sub(started) >= period
}

pub(super) fn take(now: u64, period: u64) -> Option<Counters> {
    // Ordinary IRQ turns only read the epoch: no global hot CAS/guard.
    // ORDERING: read the latest report epoch without writing a hot global line.
    let observed = STARTED.load(Ordering::Acquire);
    if observed != 0 && !due(now, observed, period) {
        return None;
    }
    // ORDERING: exclusive collector custody joins the prior drain's release.
    if DRAINING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    let _drain = Drain;
    // ORDERING: recheck epoch after collector admission, not a copied precheck.
    let old = STARTED.load(Ordering::Acquire);
    if old == 0 {
        // ORDERING: publish the first complete timestamp without consuming counters.
        let _ = STARTED.compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire);
        return None;
    }
    if !due(now, old, period)
        // ORDERING: spend the exact due epoch once before destructive collection.
        || STARTED
            .compare_exchange(old, now, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return None;
    }
    let mut window = Counters::new();
    window.runtime_profile_started_ticks = old;
    for cell in &CELLS {
        window.merge(cell.with(Counters::take));
    }
    Some(window)
}
pub(in crate::multitask::scheduler) fn reset_before_boot() {
    // ORDERING: boot reset precedes any CPU diagnostic publication.
    STARTED.store(0, Ordering::Release);
    for cell in &CELLS {
        cell.with(|row| *row = Counters::new());
    }
    for slot in 0..MAX_TASK {
        clear_history(slot);
    }
}
#[cfg(test)]
mod tests;
