//! Owner: original masked CPU return loan, not a catalog or observed queue grant.
//! Boundary: borrows unique CpuDispatchTurn and its actual physical Current bank.
//! Lifecycle: exact UID/frame capture -> once settlement/frame/queue publication -> drop before switch.
//! Concurrency: raced wait/wake/terminal use actual return CAS; failed peer claim retains THIS loan.
//! Failure: absent bank/binding or foreign initial queue custody refuses before effects.
//! Forbidden: recapture from Blocked/Local, new post-stop pins or telemetry as billing authority.
//! Evidence: original bank/resources and captured funding interval; incoming dispatch remains separate.
use super::{
    SCHED_MIN_GRANULARITY_NS, context_validation, current_identity,
    donation_ledger, execution_interval, frame_publication, runqueue, runtime_profile,
    scheduling_context, stack_resources, task_weight,
};
use crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn;

pub(in crate::multitask) struct CurrentReturn<'a> {
    turn: &'a CpuDispatchTurn<'static>,
    cpu: usize,
    slot: usize,
    id: u64,
    view: context_validation::PublishedFrameView,
    accounted: Option<u64>,
    frame_published: bool,
    return_rsp: Option<usize>,
    publication: Option<runqueue::ReturnPublication>,
    canary: core::cell::Cell<Option<bool>>,
}
impl<'a> CurrentReturn<'a> {
    pub(in crate::multitask) fn capture(turn: &'a CpuDispatchTurn<'static>) -> Option<Self> {
        let _phase = runtime_profile::phase_scope(super::SchedulerPhase::ReturnCapture);
        let (cpu, slot) = turn.original_current()?;
        let view = frame_publication::return_view(slot)?;
        let id = stack_resources::current_return_binding(cpu, slot, view)?;
        Self::captured(turn, cpu, slot, id, view)
    }
    /// Called after IPC/terminal/image producers, never by early SIMD/probes.
    pub(in crate::multitask) fn capture_dispatch(turn: &'a CpuDispatchTurn<'static>) -> Option<Self> {
        let image = turn.outgoing_image()?;
        Self::captured(turn, image.cpu, image.slot, image.id, image.view)
    }
    fn captured(turn: &'a CpuDispatchTurn<'static>, cpu: usize, slot: usize, id: u64,
        view: context_validation::PublishedFrameView) -> Option<Self> {
        let owner = runqueue::owner(slot);
        if !(owner.state == runqueue::RunOwnerState::Running && owner.cpu == Some(cpu))
            && !owner.state.is_terminal()
        {
            return None;
        }
        Some(Self {
            turn,
            cpu,
            slot,
            id,
            view,
            accounted: None,
            frame_published: false,
            return_rsp: None,
            publication: None,
            canary: core::cell::Cell::new(None),
        })
    }
    /// Probe only while THIS original Current bank/frame binding is borrowed.
    /// Kernel execution on a runnable user stack must finish at its safe point;
    /// explicit blocked/terminal intent may instead consume the original return.
    pub(in crate::multitask) fn kernel_continuation(&self, cs: u64) -> bool {
        self.check();
        if cs == crate::arch::gdt::user_code_selector().0 as u64 {
            return false;
        }
        let owner = runqueue::owner(self.slot);
        !owner.state.is_terminal() && owner.runnable && self.view.user_mode_task
    }
    /// The SAME original return loan retains physical backing across SIMD bytes.
    /// No scalar task lookup or directory serializer participates in this effect.
    pub(in crate::multitask) fn save_simd(&self) {
        self.check();
        // Phase timing is diagnosis-build only, like every `PhaseScope`: two
        // clock reads and a counter leaf on each switch are not shipping cost.
        #[cfg(rustos_scheduler_phase_profile)]
        let start = crate::arch::clock::monotonic_nanos();
        runqueue::simd_tls::save_simd_state(self.slot);
        #[cfg(rustos_scheduler_phase_profile)]
        {
            let elapsed = crate::arch::clock::monotonic_nanos().saturating_sub(start);
            super::runtime_profile::owner::local(|row| {
                row.phase(super::runtime_profile::SchedulerPhase::ArchSimd, elapsed);
                row.phase(super::runtime_profile::SchedulerPhase::ArchRestore, elapsed);
            });
        }
    }
    /// The binding was proven at capture, and the physical Current bank this
    /// loan borrows is what keeps it: no reset or rebinding can reach a slot
    /// that is still some CPU's Current. So each effect re-proves the bank
    /// (two CPU-local loads). Byte validation borrows the post-producer frozen
    /// image, qualified once on this turn; metadata mutators refuse after that
    /// boundary. Early SIMD/probe capture deliberately does not freeze it.
    fn check(&self) {
        assert_eq!(
            self.turn.original_current(),
            Some((self.cpu, self.slot)),
            "return loan lost original masked Current bank"
        );
        assert!(
            stack_resources::cpu_leases::owns_current(self.cpu, self.slot),
            "return loan lost original physical Current bank"
        );
    }
    fn check_binding(&self) {
        self.check();
        let image = self.turn.outgoing_image().expect("return lost original frozen image");
        assert!(image.cpu == self.cpu && image.slot == self.slot && image.id == self.id
            && image.matches_view(self.view), "return loan lost original UID/frame binding");
    }
    pub(super) fn original_turn(&self) -> &'a CpuDispatchTurn<'static> {
        self.check_binding();
        self.turn
    }
    pub(in crate::multitask) fn slot(&self) -> usize {
        self.slot
    }
    pub(in crate::multitask) fn id(&self) -> u64 {
        self.id
    }

    /// Original captured FundingPin settles once, even when fast IPC loses its
    /// peer claim after outgoing publication and falls back to ordinary pick.
    pub(in crate::multitask) fn account(&mut self, now_ticks: u64, force_min: bool) -> u64 {
        self.check();
        if let Some(elapsed) = self.accounted {
            return elapsed;
        }
        let interval = match execution_interval::consume(self.cpu, self.slot, self.id) {
            execution_interval::ConsumedInterval::Active(interval) => Some(interval),
            execution_interval::ConsumedInterval::AlreadySettled => {
                self.accounted = Some(0);
                return 0;
            }
            execution_interval::ConsumedInterval::Absent => None,
        };
        let elapsed = match interval.as_ref() {
            Some(interval) => interval.elapsed_ns(
                now_ticks,
                crate::arch::clock::scheduler_local_nanos(),
                ticks_to_ns,
            ),
            None => {
                assert!(
                    runqueue::is_idle_slot(self.slot),
                    "executing task has no CPU interval custody"
                );
                assert!(
                    current_identity::read_dispatch(self.slot)
                        .is_some_and(|identity| identity.pager_charge.is_none())
                );
                assert!(donation_ledger::borrowed_context_charge_token(self.slot).is_none());
                let start = runqueue::exec_start_ticks(self.slot);
                if start == 0 {
                    self.accounted = Some(0);
                    return 0;
                }
                ticks_to_ns(start, now_ticks)
            }
        };
        runtime_profile::owner::local(|row| row.account(self.slot, elapsed));
        #[cfg(rustos_scheduler_phase_profile)]
        let budget_start = crate::arch::clock::monotonic_nanos();
        if let Some((id, outcome)) = interval.and_then(|interval| {
            let span = interval.charge_elapsed_ns(
                now_ticks, crate::arch::clock::scheduler_local_nanos(), ticks_to_ns);
            interval.charge(crate::arch::clock::monotonic_nanos(), span)
        }) {
            if outcome.exhausted && outcome.charged_ns != 0 {
                scheduling_context::latch_budget_exhaustion(id, outcome.charged_ns);
            }
        }
        #[cfg(rustos_scheduler_phase_profile)]
        runtime_profile::owner::local(|row| {
            row.phase(
                runtime_profile::SchedulerPhase::AccountBudget,
                crate::arch::clock::monotonic_nanos().saturating_sub(budget_start),
            )
        });
        let fair = if force_min {
            elapsed.max(SCHED_MIN_GRANULARITY_NS)
        } else {
            elapsed
        };
        if fair != 0 {
            runqueue::add_vruntime(
                self.slot,
                task_weight::weighted_vruntime_delta(fair, runqueue::weight::value(self.slot)),
            );
        }
        runqueue::set_exec_start_ticks(self.slot, 0);
        runqueue::set_exec_start_local_ns(self.slot, 0);
        self.accounted = Some(elapsed);
        elapsed
    }

    pub(in crate::multitask) fn validate(&self, rsp: usize) -> Option<&'static str> {
        self.check_binding();
        if self.frame_published {
            // Only THIS producer's exact original IRQ frame was published.
            // A different pointer is not a fresh byte-validation grant.
            return if self.return_rsp == Some(rsp) {
                None
            } else {
                Some("saved context pointer is outside the task stack")
            };
        }
        context_validation::validate_published_saved_context(self.slot, rsp, self.live_view()).err()
    }
    /// The outgoing task does not run between `validate` and `publish_frame`,
    /// so its guard words cannot change between them: read them once.
    fn live_view(&self) -> context_validation::PublishedFrameView {
        let mut view = self.view;
        view.stack_canary_intact = match self.canary.get() {
            Some(intact) => intact,
            None => {
                let intact = self.read_canary();
                self.canary.set(Some(intact));
                intact
            }
        };
        view
    }
    fn read_canary(&self) -> bool {
        self.check_binding();
        self.turn.outgoing_image().expect("canary lost original image").canary_intact()
    }

    /// Publish fresh IRQ frame before releasing Running queue custody. No
    /// second fallback frame refresh and no clearing a raced completed wait.
    pub(in crate::multitask) fn publish_frame(&mut self, rsp: usize, ready: bool) {
        self.check();
        if self.frame_published {
            return;
        }
        assert!(
            self.publication.is_none(),
            "return frame follows queue release"
        );
        if ready {
            runqueue::set_runnable(self.slot, true);
            if runqueue::wait::ready_since_ticks(self.slot) == 0 {
                runqueue::wait::set_ready_since_ticks(self.slot, crate::arch::rtc::ticks().max(1));
            }
        } else {
            // Wait commit / terminal owns run-intent cancellation. Clearing it
            // here would erase a racing wake that the return CAS must observe.
            runqueue::wait::set_ready_since_ticks(self.slot, 0);
        }
        runqueue::set_saved_rsp(self.slot, rsp);
        let v = self.live_view();
        frame_publication::publish(
            self.slot,
            rsp,
            v.kernel_stack_base,
            v.kernel_stack_top,
            v.alternate_stack_base,
            v.alternate_stack_top,
            v.user_mode_task,
            v.kernel_process_task,
            v.stack_canary_intact,
            v.scheduler_storage_base,
            v.scheduler_storage_end,
        );
        self.return_rsp = Some(rsp);
        self.frame_published = true;
    }
    pub(in crate::multitask::scheduler) fn publish_queue(
        &mut self,
        allow_local: bool,
    ) -> runqueue::ReturnPublication {
        self.check();
        if let Some(publication) = self.publication {
            return publication;
        }
        assert!(
            self.accounted.is_some() && self.frame_published,
            "queue return precedes original interval/frame settlement"
        );
        let publication = runqueue::publish_return(
            self.slot,
            self.cpu,
            runqueue::weight::value(self.slot),
            allow_local,
        );
        self.publication = Some(publication);
        publication
    }
}
fn ticks_to_ns(start: u64, end: u64) -> u64 {
    end.saturating_sub(start)
        .saturating_mul(1_000_000_000)
        .saturating_div(crate::arch::rtc::ticks_per_second().max(1))
}
#[cfg(test)]
mod tests;
