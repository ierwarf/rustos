//! Original CPU-local return serialization, independent of a global directory.
//! The same non-Copy turn retains physical custody through the RSP ACK.
use super::*;
use core::marker::PhantomData;
const VACANT: u8 = 0;
const OPEN: u8 = 1;
const PUBLISHING: u8 = 2;
const AWAITING_RSP: u8 = 3;
const POISONED: u8 = 4;
const COMMITTING: u8 = 5;
use super::super::scheduler::PlacementGrant;
use super::super::scheduler::stack_resources::activation::ActivationCredential;
use core::cell::UnsafeCell;
use nucleus_core::util::lockdep::{DispatchPreemption, LockClass};

struct TurnCell {
    phase: core::sync::atomic::AtomicU8,
    parked: UnsafeCell<Option<DispatchPreemption<'static>>>,
    activation: UnsafeCell<Option<ActivationCredential>>,
    placement: UnsafeCell<Option<PlacementGrant>>,
    installed_placement: UnsafeCell<Option<PlacementGrant>>,
    // Only explicit scalar protocol fixtures may omit resource custody. Real
    // native IRQ turns use exactly the production bank/interval checks.
    #[cfg(test)]
    phase_model: core::sync::atomic::AtomicBool,
}
// SAFETY: unique IRQ-excluded own-CPU Open/Publishing producer writes grants
// and parked before Awaiting-RSP; same-CPU Committing consumer takes them before
// reopening. Installed Current survives ACK; only own-CPU IRQ scope in Vacant/Open
// copies exact Current bank/owner/UID permission. Poison permits no grant access;
// isolated host recovery is explicit.
unsafe impl Sync for TurnCell {}
impl TurnCell {
    const fn new() -> Self {
        Self {
            phase: core::sync::atomic::AtomicU8::new(VACANT),
            parked: UnsafeCell::new(None),
            activation: UnsafeCell::new(None),
            placement: UnsafeCell::new(None),
            installed_placement: UnsafeCell::new(None),
            #[cfg(test)]
            phase_model: core::sync::atomic::AtomicBool::new(false),
        }
    }

    fn complete_placement(&self, grant: PlacementGrant) {
        assert!(
            matches!(self.phase.load(Ordering::Acquire), PUBLISHING | COMMITTING),
            "placement completion lacks admitted CPU phase"
        );
        grant.acknowledge();
        // SAFETY: unique Publishing/Committing own-CPU actor. Keep installed
        // Current permission until the next hardware/RSP-acknowledged replacement.
        unsafe {
            *self.installed_placement.get() = Some(grant);
        }
    }
    fn begin(&self, cpu: usize, from: usize) -> Option<CpuDispatchTurn<'_>> {
        self.phase
            .compare_exchange(VACANT, OPEN, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        #[cfg(test)]
        self.phase_model.store(false, Ordering::Relaxed);
        Some(CpuDispatchTurn {
            cell: self,
            cpu,
            from,
            live: true,
            preemption: None,
            claimed_return: UnsafeCell::new(None),
            priority_choice: UnsafeCell::new(None),
            architecture_phase: core::cell::Cell::new(0),
            affinity: PhantomData,
        })
    }
    fn models_only_phases(&self) -> bool {
        #[cfg(test)]
        { self.phase_model.load(Ordering::Relaxed) }
        #[cfg(not(test))]
        { false }
    }
    #[cfg(test)]
    fn begin_phase_model(&self, cpu: usize, from: usize) -> Option<CpuDispatchTurn<'_>> {
        let turn = self.begin(cpu, from)?;
        self.phase_model.store(true, Ordering::Relaxed);
        Some(turn)
    }
    fn begin_commit(&self) -> Option<CpuRspCommit<'_>> {
        // Admit the unique post-RSP consumer BEFORE any bank or transition ACK.
        self.phase
            .compare_exchange(
                AWAITING_RSP,
                COMMITTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()?;
        Some(CpuRspCommit {
            cell: self,
            live: true,
            affinity: PhantomData,
        })
    }
    #[cfg(test)]
    fn commit(&self) {
        if let Some(commit) = self.begin_commit() {
            commit.finish();
        }
    }
}
static TURNS: [TurnCell; MAX_TRACKED_CPUS] = [const { TurnCell::new() }; MAX_TRACKED_CPUS];

/// Once-admitted assembly consumer. Outgoing banks must be drained before
/// finish reopens this CPU. Abandonment quarantines, never fabricates an ACK.
pub(super) struct CpuRspCommit<'a> {
    cell: &'a TurnCell,
    live: bool,
    affinity: PhantomData<*mut ()>,
}
impl CpuRspCommit<'_> {
    pub(super) fn physical_banks(&self) -> bool { !self.cell.models_only_phases() }
    pub(super) fn finish(mut self) {
        // SAFETY: Awaiting-RSP -> Committing CAS gave this guard sole access.
        let preemption = unsafe { (&mut *self.cell.parked.get()).take() };
        if self.physical_banks() {
            assert!(preemption.is_some(), "RSP commit lost CPU preemption custody");
        }
        // SAFETY: the same unique Committing consumer owns activation storage.
        let activation = unsafe { (&mut *self.cell.activation.get()).take() };
        drop(activation);
        // SAFETY: admitted same-CPU Committing consumer, after bank/RSP ACK.
        let placement = unsafe { (&mut *self.cell.placement.get()).take() };
        if let Some(grant) = placement {
            self.cell.complete_placement(grant);
        }
        drop(preemption);
        self.live = false;
        self.cell.phase.store(VACANT, Ordering::Release);
    }
}
impl Drop for CpuRspCommit<'_> {
    fn drop(&mut self) {
        if self.live {
            self.cell.phase.store(POISONED, Ordering::Release);
        }
    }
}

#[must_use]
pub(in crate::multitask) struct CpuDispatchTurn<'a> {
    cell: &'a TurnCell,
    cpu: usize,
    from: usize,
    live: bool,
    preemption: Option<DispatchPreemption<'static>>,
    claimed_return: UnsafeCell<Option<super::super::scheduler::return_architecture::ClaimedReturn>>,
    priority_choice: UnsafeCell<Option<super::super::scheduler::dispatch_priority::Choice>>,
    architecture_phase: core::cell::Cell<u8>,
    // A CPU lease cannot migrate through a safe Send/Sync path.
    affinity: PhantomData<*mut ()>,
}
impl<'a> CpuDispatchTurn<'a> {
    /// Lend this original masked turn, not a fresh grant from queue state.
    /// A returned loan borrows self, so Current publication cannot consume the
    /// turn or move its physical bank until the loan is dropped.
    pub(in crate::multitask) fn original_current(&self) -> Option<(usize, usize)> {
        if self.cell.phase.load(Ordering::Acquire) != OPEN {
            return None;
        }
        #[cfg(not(test))]
        if !self.preemption.as_ref()?.is_masked()
            || nucleus_core::util::lockdep::current_cpu_index() != self.cpu
        {
            return None;
        }
        if !CURRENT_TASK_ACTIVE[self.cpu].load(Ordering::Acquire)
            || CURRENT_TASK_SLOTS[self.cpu].load(Ordering::Acquire) != self.from
            || !super::super::scheduler::stack_resources::cpu_leases::owns_current(self.cpu, self.from)
        {
            return None;
        }
        Some((self.cpu, self.from))
    }

    /// Admit priority BEFORE observing any prefix or selecting a peer. The
    /// original turn retains the phase through its actual Current publication.
    pub(in crate::multitask) fn begin_priority_choice(&self) {
        assert!(self.original_current().is_some(), "choice lost original CPU turn");
        // SAFETY: unique non-Send/non-Sync Open turn; immutable after admission.
        let bank = unsafe { &mut *self.priority_choice.get() };
        assert!(bank.is_none(), "CPU turn admitted two priority choices");
        *bank = Some(super::super::scheduler::dispatch_priority::Choice::begin(self));
    }
    pub(in crate::multitask) fn owns_priority_choice(&self) -> bool {
        self.original_current().is_some()
            // SAFETY: borrowing this original turn excludes publication/drop.
            && unsafe { &*self.priority_choice.get() }.as_ref().is_some_and(|choice| choice.is_live())
    }

    pub(in crate::multitask) fn retain_claimed_return(
        &self, receipt: super::super::scheduler::return_architecture::ClaimedReturn,
    ) {
        assert!(self.original_current().is_some(), "claim lost original masked CPU turn");
        // SAFETY: this non-Send/non-Sync original Open turn owns the receipt.
        // Only the successful validated claim writes it; no borrower precedes it.
        let bank = unsafe { &mut *self.claimed_return.get() };
        assert!(bank.is_none(), "CPU turn claimed two incoming frames");
        *bank = Some(receipt);
    }
    pub(in crate::multitask) fn claimed_return(
        &self,
    ) -> Option<&super::super::scheduler::return_architecture::ClaimedReturn> {
        self.original_current()?;
        // SAFETY: immutable once published by the same original Open turn;
        // borrowing self prevents consuming/publishing this turn concurrently.
        unsafe { &*self.claimed_return.get() }.as_ref()
    }
    pub(in crate::multitask) fn has_claimed_placement(&self, slot: usize) -> bool {
        self.original_current().is_some() && has_placement(self.cpu, slot)
    }
    pub(in crate::multitask) fn claimed_mm_root(&self, slot: usize, id: u64) -> Option<u64> {
        self.original_current()?;
        // SAFETY: same original Open turn continuously owns activation storage.
        unsafe { &*self.cell.activation.get() }.as_ref()?.root_for(slot, id)
    }
    pub(in crate::multitask) fn architecture_pending(&self) -> bool {
        self.claimed_return().is_some() && self.architecture_phase.get() == 0
    }
    pub(in crate::multitask) fn begin_architecture(&self) {
        assert!(self.architecture_pending(), "architecture already admitted or missing claim");
        self.architecture_phase.set(1);
    }
    pub(in crate::multitask) fn architecture_ready(&self) -> bool {
        self.architecture_phase.get() == 2
    }
    pub(in crate::multitask) fn mark_architecture_ready(&self) {
        assert!(self.claimed_return().is_some(), "architecture lacks original claim receipt");
        assert_eq!(self.architecture_phase.get(), 1, "architecture lacks admitted register install");
        self.architecture_phase.set(2);
    }

    pub(super) fn mask_irqs(&mut self) {
        self.preemption
            .as_mut()
            .expect("CPU turn lost preemption custody")
            .mask_irqs();
    }

    /// Borrow the already owned exact preemption scope; never enter it twice.
    pub(in crate::multitask) fn preemption(&self) -> &DispatchPreemption<'static> {
        self.preemption
            .as_ref()
            .expect("CPU turn lost preemption owner")
    }
    fn seal(&self) {
        #[cfg(not(test))]
        assert!(
            self.preemption
                .as_ref()
                .is_some_and(|owner| owner.is_masked()),
            "CPU return sealed before protected IRQ admission"
        );
        assert!(
            self.cell
                .phase
                .compare_exchange(OPEN, PUBLISHING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "CPU return published without its unique open turn"
        );
    }
    fn finish(mut self, changed: bool) {
        assert_eq!(
            self.cell.phase.load(Ordering::Acquire),
            PUBLISHING,
            "CPU return completed outside publication custody"
        );
        #[cfg(rustos_boot_image)]
        assert_ne!(
            nucleus_core::util::lockdep::current_lock_class(),
            Some(LockClass::Scheduler as u8),
            "CPU completion preceded directory unlock"
        );
        if changed {
            if let Some(preemption) = self.preemption.as_mut() {
                preemption.retain_irq_mask();
            }
            // SAFETY: this is the unique Publishing producer; consumers cannot
            // touch parked until the following Awaiting-RSP release publication.
            unsafe {
                let parked = &mut *self.cell.parked.get();
                assert!(parked.is_none(), "CPU overwrote parked preemption custody");
                *parked = self.preemption.take();
            }
            self.cell.phase.store(AWAITING_RSP, Ordering::Release);
        } else {
            // SAFETY: the unique Publishing producer owns activation storage.
            let activation = unsafe { (&mut *self.cell.activation.get()).take() };
            drop(activation);
            // SAFETY: Publishing producer still owns placement storage.
            let placement = unsafe { (&mut *self.cell.placement.get()).take() };
            if let Some(grant) = placement {
                self.cell.complete_placement(grant);
            }
            self.cell.phase.store(VACANT, Ordering::Release);
            // Make destruction inert before restoration can admit an IRQ turn.
            self.live = false;
            // After directory unlock; unchanged/control return restores caller IF.
            drop(self.preemption.take());
        }
        self.live = false;
    }
    pub(super) fn publish_return(self, to: usize, to_idle: bool) -> CpuReturnCompletion<'a> {
        #[cfg(not(test))]
        assert_eq!(
            nucleus_core::util::lockdep::current_cpu_index(),
            self.cpu,
            "CPU return lease moved to a foreign CPU"
        );
        assert!(
            CURRENT_TASK_ACTIVE[self.cpu].load(Ordering::Acquire),
            "CPU returned before current-slot admission"
        );
        assert_eq!(
            CURRENT_TASK_SLOTS[self.cpu].load(Ordering::Acquire),
            self.from,
            "CPU return lease lost its original Current"
        );
        assert!(
            !slot_has_remote_owner(self.cpu, to),
            "CPU return selected a remotely owned task"
        );
        assert!(
            !TRANSITION_ACTIVE[self.cpu].load(Ordering::Acquire),
            "CPU return overlaps an uncommitted RSP handoff"
        );
        // SAFETY: only this own-CPU Open producer accesses its activation bank.
        if let Some(activation) = unsafe { &*self.cell.activation.get() }.as_ref() {
            assert!(
                activation.slot() == to && activation.is_current(),
                "CPU return lost its exact MM activation grant"
            );
        }
        // SAFETY: only this Open producer can access its captured placement.
        if let Some(grant) = unsafe { &*self.cell.placement.get() }.as_ref() {
            assert!(
                grant.slot() == to && grant.is_current(),
                "CPU return lost captured placement custody"
            );
        }
        if !self.cell.models_only_phases() {
            if self.from != to {
                assert!(
                    unsafe { &*self.cell.placement.get() }.is_some(),
                    "changed return lacks placement grant"
                );
            }
            if let Some(receipt) = self.claimed_return() {
                assert!(self.architecture_phase.get() == 2 && receipt.slot() == to,
                    "CPU publication preceded exact claimed architecture/SIMD preparation");
            }
            // Qualify the exact grant while THIS original Open turn still owns
            // its receipt/banks. Sealing first would force scalar reconstruction.
            super::super::scheduler::execution_interval::finish_return(&self, to);
        } else {
            assert!(self.claimed_return().is_none(), "phase model cannot publish a physical claim");
        }
        self.seal();
        let publication = CpuSwitchPublication::new(self.cpu, self.from, to, to_idle);
        #[cfg(test)]
        if self.cell.models_only_phases() {
            publication.publish_phase_model();
        } else {
            publication.publish_before_return();
        }
        #[cfg(not(test))]
        publication.publish_before_return();
        // SAFETY: this sealed original turn is the unique bank consumer. Release
        // the ordering grant only after both Current and Transition are visible.
        if let Some(choice) = unsafe { (&mut *self.priority_choice.get()).take() } {
            choice.published();
        }
        let changed = self.from != to;
        CpuReturnCompletion {
            turn: Some(self),
            changed,
        }
    }
}
#[must_use]
pub(super) struct CpuReturnCompletion<'a> {
    turn: Option<CpuDispatchTurn<'a>>,
    changed: bool,
}
impl CpuReturnCompletion<'_> {
    pub(super) fn after_directory_unlock(mut self) {
        self.turn
            .take()
            .expect("CPU completion consumed twice")
            .finish(self.changed);
    }
}
impl Drop for CpuDispatchTurn<'_> {
    fn drop(&mut self) {
        if self.live {
            if let Some(preemption) = self.preemption.as_mut() {
                preemption.retain_irq_mask();
            }
            // An abandoned/partially published architecture transaction is not
            // rollback-safe. Never reopen the CPU after a producer panic.
            self.cell.phase.store(POISONED, Ordering::Release);
            // Keep any successful claim/architecture MM grant quarantined.
            // No proven RSP completion exists; poison cannot authorize close.
        }
    }
}

pub(super) fn begin(
    preemption: DispatchPreemption<'static>,
    from: usize,
) -> CpuDispatchTurn<'static> {
    let cpu = preemption.cpu();
    assert!(cpu < TURNS.len() && from < MAX_SCHEDULER_TASKS);
    assert!(
        !TRANSITION_ACTIVE[cpu].load(Ordering::Acquire),
        "CPU turn opened before prior RSP handoff committed"
    );
    let mut turn = TURNS[cpu]
        .begin(cpu, from)
        .expect("overlapping or poisoned CPU dispatch turn");
    turn.preemption = Some(preemption);
    turn
}
/// Explicit integer-only protocol input; never used by born-resource IRQ or
/// architecture witnesses. The production entry always calls `begin` instead.
#[cfg(test)]
pub(super) fn begin_phase_model(preemption: DispatchPreemption<'static>, from: usize) -> CpuDispatchTurn<'static> {
    let turn = begin(preemption, from);
    turn.cell.phase_model.store(true, Ordering::Relaxed);
    turn
}
pub(super) fn can_retain_identity(
    cpu: usize,
    slot: usize,
    task_id: u64,
    mm: &super::super::process_table::MmExecutionPin,
) -> bool {
    let Some(cell) = TURNS.get(cpu) else {
        return false;
    };
    #[cfg(not(test))]
    if current_cpu() != cpu {
        return false;
    }
    if cell.phase.load(Ordering::Acquire) != OPEN {
        return false;
    }
    // SAFETY: only the own-CPU IRQ-excluded producer queries this bank.
    unsafe { &*cell.activation.get() }
        .as_ref()
        .is_none_or(|old| old.matches_identity(slot, task_id, mm))
}

pub(super) fn can_retain_activation(cpu: usize, incoming: &ActivationCredential) -> bool {
    let Some(cell) = TURNS.get(cpu) else {
        return false;
    };
    #[cfg(not(test))]
    if current_cpu() != cpu {
        return false;
    }
    if cell.phase.load(Ordering::Acquire) != OPEN {
        return false;
    }
    // SAFETY: own-CPU IRQ-excluded Open producer, never a remote observer.
    unsafe { &*cell.activation.get() }
        .as_ref()
        .is_none_or(|old| old.same(incoming))
}
pub(super) fn retain_activation(cpu: usize, incoming: ActivationCredential) {
    assert!(
        can_retain_activation(cpu, &incoming),
        "CPU activation replaced a foreign live grant"
    );
    // SAFETY: own-CPU Open producer has unique mutable bank custody.
    let bank = unsafe { &mut *TURNS[cpu].activation.get() };
    if bank.is_none() {
        *bank = Some(incoming);
    }
    // A duplicate same-MM credential drops outside resource/queue leaves. The
    // original bank grant remains live: close never sees a zero-unit gap.
}

pub(super) fn current_placement_scope(cpu: usize, slot: usize) -> Option<Option<PlacementGrant>> {
    let cell = TURNS.get(cpu)?;
    #[cfg(not(test))]
    assert_eq!(
        current_cpu(),
        cpu,
        "installed placement read by foreign CPU"
    );
    if !matches!(cell.phase.load(Ordering::Acquire), VACANT | OPEN) {
        return None;
    }
    // SAFETY: continuous own-CPU IRQ scope; no producer/consumer overlaps.
    // Missing bootstrap bank differs from stale/foreign installed authority.
    match unsafe { &*cell.installed_placement.get() }.as_ref() {
        None => Some(None),
        Some(grant) if grant.slot() == slot => Some(Some(grant.copy_current_scope()?)),
        Some(_) => None,
    }
}

pub(super) fn has_placement(cpu: usize, slot: usize) -> bool {
    let Some(cell) = TURNS.get(cpu) else {
        return false;
    };
    #[cfg(not(test))]
    if current_cpu() != cpu {
        return false;
    }
    if cell.phase.load(Ordering::Acquire) != OPEN {
        return false;
    }
    // SAFETY: unique own-CPU Open producer, never a remote observer.
    unsafe { &*cell.placement.get() }
        .as_ref()
        .is_some_and(|grant| grant.slot() == slot && grant.is_current())
}
pub(super) fn retain_placement(cpu: usize, grant: PlacementGrant) {
    #[cfg(test)]
    if TURNS[cpu].phase.load(Ordering::Acquire) != OPEN || grant.is_fixture() {
        return;
    }
    #[cfg(not(test))]
    assert_eq!(current_cpu(), cpu, "placement moved to foreign producer");
    assert_eq!(
        TURNS[cpu].phase.load(Ordering::Acquire),
        OPEN,
        "placement lacks open CPU turn"
    );
    // SAFETY: unique own-CPU Open producer owns mutable bank storage.
    let bank = unsafe { &mut *TURNS[cpu].placement.get() };
    if let Some(old) = bank {
        assert!(
            old.same(&grant),
            "CPU placement replaced foreign live grant"
        );
    } else {
        *bank = Some(grant);
    }
}

#[inline]
pub(super) fn current_cpu() -> usize {
    #[cfg(rustos_boot_image)]
    assert!(
        !x86_64::instructions::interrupts::are_enabled(),
        "scheduler scratch accessed before protected IRQ admission"
    );
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    assert!(
        TURNS
            .get(cpu)
            .is_some_and(|cell| cell.phase.load(Ordering::Acquire) == OPEN),
        "scheduler scratch accessed outside an exact CPU-local turn"
    );
    cpu
}
pub(super) fn begin_commit(cpu: usize) -> Option<CpuRspCommit<'static>> {
    TURNS[cpu].begin_commit()
}
#[cfg(test)]
pub(super) fn commit(cpu: usize) {
    TURNS[cpu].commit();
}
/// Modeled IRQ entry only: caller supplies a real isolated physical bank.
/// Restoration is native fixture cleanup, never production poison recovery.
#[cfg(test)]
pub(in crate::multitask) fn with_original_current_model(
    slot: usize, f: impl FnOnce(&CpuDispatchTurn<'static>),
) {
    let previous = test_phase(0);
    let mut turn = begin(DispatchPreemption::enter_model(0), slot);
    turn.mask_irqs();
    f(&turn);
    drop(turn);
    test_restore_phase(0, previous);
}

#[cfg(test)]
pub(in crate::multitask) fn test_phase(cpu: usize) -> u8 {
    TURNS[cpu].phase.load(Ordering::Acquire)
}
#[cfg(test)]
pub(super) fn test_restore_phase(cpu: usize, phase: u8) {
    // Synthetic fixture cleanup must ACK parked custody, not erase its owner.
    if TURNS[cpu].phase.load(Ordering::Acquire) == AWAITING_RSP {
        TURNS[cpu].commit();
    }
    // SAFETY: isolated host fixture recovery, never a production poison ACK.
    let activation = unsafe { (&mut *TURNS[cpu].activation.get()).take() };
    drop(activation);
    // SAFETY: isolated native fixture recovery, not a production ACK.
    drop(unsafe { (&mut *TURNS[cpu].placement.get()).take() });
    drop(unsafe { (&mut *TURNS[cpu].installed_placement.get()).take() });
    super::super::scheduler::dispatch_priority::reset_model(cpu);
    TURNS[cpu].phase.store(phase, Ordering::Release);
}
#[cfg(test)]
mod preemption_tests;
#[cfg(test)]
mod tests;
