//! Owner: one original CPU turn and its physical Current/claimed banks.
//! Boundary: IRQ safe point -> original return -> claim/architecture -> Current/RSP.
//! Lifecycle: no directory is acquired; the same non-Copy turn survives final
//! publication and then parks its original outgoing custody for assembly ACK.
//! Concurrency: masked CPU excludes reentry; no raw leaf crosses SIMD or switch.
//! Failure: missing Current, overlapping turn or missing claim is a fail-stop.
//! Forbidden: scalar scratch as final return authority, re-created turns or pins.
//! Evidence: production IRQ integration; native earlier IRQ/RSP remain models.
use super::*;
use super::super::scheduler::{self, SchedulerDispatch};

pub(in crate::multitask) struct DispatchEntry {
    turn: Option<dispatch_turn::CpuDispatchTurn<'static>>,
    selected: Option<(usize, bool)>,
    simd_saved: core::cell::Cell<bool>,
}
impl DispatchEntry {
    pub(in crate::multitask) fn enter() -> Self {
        let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
        scope.mask_irqs();
        Self::from_scope(scope)
    }
    pub(in crate::multitask) fn from_scope(scope: nucleus_core::util::lockdep::DispatchPreemption<'static>) -> Self {
        assert!(scope.is_masked(), "IRQ entry requires its original masked scope");
        let cpu = scope.cpu();
        assert!(scheduler_initialized(), "IRQ entry before bootstrap publication");
        assert!(cpu_task_slot_admitted(cpu), "IRQ entry lacks admitted Current");
        // ORDERING: bootstrap/current publication precedes this same-CPU capture.
        let from = CURRENT_TASK_SLOTS[cpu].load(Ordering::Acquire);
        let turn = dispatch_turn::begin(scope, from);
        assert!(turn.original_current().is_some(), "IRQ entry lacks physical Current bank");
        let entry = Self { turn: Some(turn), selected: None, simd_saved: core::cell::Cell::new(false) };
        // A reply deferred past its replier's syscall is woken at the replier's next
        // scheduler entry, so this entry's dispatch sees the caller in the FIFO.
        if scheduler::pending_reply::present() {
            super::super::current::settle_deferred_fast_ipc_reply_in(entry.preemption());
        }
        // Exact deferred identities own no backing. Each independent wake
        // captures its own original binding, outside every raw leaf.
        #[cfg(rustos_scheduler_phase_profile)]
        let started = crate::arch::clock::monotonic_nanos();
        let wakes = super::super::deferred_wake::take_masked(entry.preemption());
        let count = wakes.as_ref().map_or(0, |wakes| wakes.len());
        for id in wakes.iter().flat_map(|wakes| wakes.iter()) {
            // A refused deferred wake used to be dropped here, leaving its
            // waiter parked until an unrelated event; retry its exact wait.
            if !scheduler::wake_publication::wake(
                entry.preemption(), id, &scheduler::runqueue_policy::online_affinity_mask,
            ) {
                let _ = scheduler::wake_retry::record_in(entry.preemption(), id, None);
            }
        }
        let _ = scheduler::wake_retry::service(entry.preemption());
        // Prologue wall time is a diagnosis-build phase; shipping keeps only
        // the deferred-wake count, and only when there was one.
        #[cfg(rustos_scheduler_phase_profile)]
        scheduler::runtime_profile::record_prologue(count,
            crate::arch::clock::monotonic_nanos().saturating_sub(started));
        #[cfg(not(rustos_scheduler_phase_profile))]
        if count != 0 {
            scheduler::runtime_profile::record_prologue(count, 0);
        }
        entry
    }
    pub(in crate::multitask) fn turn(&self) -> &dispatch_turn::CpuDispatchTurn<'static> {
        self.turn.as_ref().expect("IRQ entry already consumed its original turn")
    }
    pub(in crate::multitask) fn preemption(&self)
        -> &nucleus_core::util::lockdep::DispatchPreemption<'static> {
        self.turn().preemption()
    }
    pub(in crate::multitask) fn kernel_continuation(&self, cs: u64) -> bool {
        let outgoing = scheduler::current_return::CurrentReturn::capture(self.turn())
            .expect("IRQ probe lacks original Current frame/binding");
        outgoing.kernel_continuation(cs)
    }
    /// Save before an IPC producer can change Current's wait/funding intent.
    /// Repeated dispatch/fallback borrows the already saved original bank.
    pub(in crate::multitask) fn save_simd(&self) {
        if !self.simd_saved.get() {
            let outgoing = scheduler::current_return::CurrentReturn::capture(self.turn())
                .expect("IRQ SIMD lacks original physical Current");
            outgoing.save_simd();
            self.simd_saved.set(true);
        }
    }
    pub(in crate::multitask) fn dispatch(
        &self, rsp: usize, voluntary: bool, immediate: Option<u64>,
    ) -> (SchedulerDispatch, bool) {
        self.dispatch_switching(rsp, voluntary, immediate, None)
    }
    /// `dispatch` for a producer that kept its receiver frame loan. An unused
    /// loan restores the receiver's ordinary fair custody when it drops.
    pub(in crate::multitask) fn dispatch_switching(
        &self, rsp: usize, voluntary: bool, immediate: Option<u64>,
        switch: Option<scheduler::dispatch_claim::DirectSwitch>,
    ) -> (SchedulerDispatch, bool) {
        self.save_simd();
        let mut outgoing = scheduler::current_return::CurrentReturn::capture_dispatch(self.turn())
            .expect("IRQ dispatch lacks original post-producer Current image");
        if let Some(switch) = switch {
            if let Some(result) = scheduler::dispatcher::fast_switch(&mut outgoing, rsp, switch) {
                return (result, true);
            }
        } else if immediate.is_some() {
            if let Some(result) = scheduler::dispatcher::fast(&mut outgoing, rsp, immediate) {
                return (result, true);
            }
        }
        // Failed fast selection borrows THIS same outgoing loan, not a new
        // lookup after the first attempt accounted/published the old frame.
        (scheduler::dispatcher::dispatch(&mut outgoing, rsp, voluntary), false)
    }
    pub(in crate::multitask) fn install(&mut self, dispatch: SchedulerDispatch, fast: bool) -> bool {
        let turn = self.turn();
        match scheduler::return_architecture::ReturnArchitecture::capture_checked(turn, dispatch) {
            Ok(architecture) => architecture.install(fast),
            Err(refused_check) => {
                // Record which check refused before stopping: a single `expect`
                // used to collapse seventeen different invariants into one
                // message, which made an intermittent panic undiagnosable.
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Process,
                    "irq-architecture-capture-refused",
                    u64::from(refused_check),
                    0,
                );
                panic!(
                    "IRQ architecture lacks the original successful claim: check {refused_check} refused"
                );
            }
        }
        self.accept_installed_return()
    }
    pub(in crate::multitask) fn accept_installed_return(&mut self) -> bool {
        let turn = self.turn();
        assert!(turn.architecture_ready(), "IRQ selection publication precedes installed architecture");
        assert!(scheduler::execution_interval::original_claim_is_active(turn),
            "IRQ selection lost its original executing interval");
        let receipt = turn.claimed_return().expect("IRQ return lacks claimed receipt");
        let user = receipt.returns_to_user();
        let slot = receipt.slot();
        self.selected = Some((slot, scheduler::dispatcher::selected_is_idle(slot)));
        user
    }
}
impl Drop for DispatchEntry {
    fn drop(&mut self) {
        let turn = self.turn.take().expect("IRQ entry released its turn twice");
        let to = self.selected.unwrap_or_else(|| {
            let (cpu, slot) = turn.original_current().expect("control return lost physical Current");
            (slot, current_cpu_task_is_idle(cpu))
        });
        let completion = turn.publish_return(to.0, to.1);
        // Actual same-CPU custody remains live through notifications and the
        // possible park. Assembly, not this scope's Drop, completes changed RSP.
        super::super::irq::flush_deferred_target_reschedules();
        completion.after_directory_unlock();
    }
}
