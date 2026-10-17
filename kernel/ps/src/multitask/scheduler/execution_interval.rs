//! Owner: CPU-local once-consumed execution and original funding custody.
//! Boundary: original frame claim/CPU turn -> active grant -> architecture/return.
//! Lifecycle: successful claim moves its non-Copy funding directly; settle once.
//! Concurrency: retained allocation/execution banks exclude reuse through return;
//! Context/Domain pins keep funder/reply identities through cancellation or charge.
//! Failure: refused preclaim funding drops outside leaves; no preparation mailbox.
//! Forbidden: scalar postclaim donor lookup, Scheduler opener, local clock across CPUs.
//! Evidence: physical root/claim/turn/bank tests; cold/native projections are not
//! complete joint guard-deletion refinement or hardware/register/RSP acceptance.
use super::scheduling_runtime::FundingPin;
use super::{current_identity, donation_ledger, local_dispatch, runqueue, scheduling_runtime};
use nucleus_core::util::lockdep::{LockClass, MAX_TRACKED_CPUS};
use crate::multitask::irq_leaf::IrqLeaf;

pub(super) struct ExecutionInterval {
    slot: usize,
    task_id: u64,
    running_generation: u64,
    start_ticks: u64,
    start_local_ns: u64,
    funding: Option<FundingPin>,
}

impl ExecutionInterval {
    pub(super) fn elapsed_ns(
        &self,
        now_ticks: u64,
        now_local_ns: Option<u64>,
        ticks_to_ns: impl FnOnce(u64, u64) -> u64,
    ) -> u64 {
        now_local_ns
            .filter(|now| self.start_local_ns != 0 && *now >= self.start_local_ns)
            .map(|now| now - self.start_local_ns)
            .unwrap_or_else(|| {
                if now_ticks > self.start_ticks {
                    ticks_to_ns(self.start_ticks, now_ticks)
                } else {
                    0
                }
            })
    }

    pub(super) fn charge(
        self,
        now_ns: u64,
        elapsed_ns: u64,
    ) -> Option<(u64, super::scheduling_context::ChargeOutcome)> {
        // An elapsed-zero interval still relinquishes its pins exactly once.
        if elapsed_ns == 0 {
            return None;
        }
        self.funding
            .map(|funding| funding.charge(now_ns, elapsed_ns))
    }
}

struct IntervalCell {
    active: Option<ExecutionInterval>,
    settled: Option<(usize, u64)>,
    current_borrowed: bool,
}

pub(super) enum ConsumedInterval {
    Active(ExecutionInterval),
    AlreadySettled,
    Absent,
}

static INTERVALS: [IrqLeaf<IntervalCell, { LockClass::CpuExecutionInterval as u8 }>;
    MAX_TRACKED_CPUS] = [const {
    IrqLeaf::new(IntervalCell {
        active: None,
        settled: None,
        current_borrowed: false,
    })
}; MAX_TRACKED_CPUS];

fn with_interval<R>(cpu: usize, f: impl FnOnce(&mut IntervalCell) -> R) -> R {
    #[cfg(not(test))]
    {
        x86_64::instructions::interrupts::without_interrupts(|| f(&mut INTERVALS[cpu].lock()))
    }
    #[cfg(test)]
    {
        f(&mut INTERVALS[cpu].lock())
    }
}

/// Original CPU interval remains in its bank, including its non-Copy FundingPin.
/// This unique borrow excludes settlement/replacement through the last cold
/// Current effect; no scalar donor lookup or new funding admission occurs.
pub(super) struct CurrentInterval<'scope, 'irq> {
    scope: &'scope nucleus_core::util::lockdep::DispatchPreemption<'irq>,
    slot: usize,
    id: u64,
    generation: u64,
    affinity: core::marker::PhantomData<*mut ()>,
}
/// Non-Copy read projection constructed ONLY from this original bank borrow.
/// Private fields bind descriptive keys to the SAME still-retained funding;
/// another owner cannot pair a foreign scalar stamp with an unrelated loan.
pub(super) struct FundingObservation<'borrow, 'scope, 'irq> {
    original: &'borrow CurrentInterval<'scope, 'irq>,
    stamp: current_identity::PagerChargeStamp,
    owner: u64,
}
impl FundingObservation<'_, '_, '_> {
    pub(super) fn metadata(&self) -> (current_identity::PagerChargeStamp, u64, u64) {
        self.original.check();
        (self.stamp, self.original.id, self.owner)
    }
}

impl<'scope, 'irq> CurrentInterval<'scope, 'irq> {
    pub(super) fn borrow(
        scope: &'scope nucleus_core::util::lockdep::DispatchPreemption<'irq>,
        slot: usize, id: u64,
    ) -> Option<Self> {
        if !scope.is_masked() || scope.cpu() != nucleus_core::util::lockdep::current_cpu_index()
            || nucleus_core::util::lockdep::held_spin_lock_depth() != 0 { return None; }
        let generation = with_interval(scope.cpu(), |cell| {
            let original = cell.active.as_ref()?;
            if cell.current_borrowed || (original.slot, original.task_id) != (slot, id) {
                return None;
            }
            let generation = original.running_generation;
            cell.current_borrowed = true;
            Some(generation)
        })?;
        Some(Self { scope, slot, id, generation, affinity: core::marker::PhantomData })
    }
    /// The same original FundingPin stays parked and unconsumable until this
    /// loan drops. Only copy descriptive keys under Interval52; paired budget
    /// leaves are acquired AFTER that leaf ends. No fresh donor/epoch admission.
    pub(super) fn runtime_snapshot(&self) -> Option<crate::multitask::SchedulingContextRuntimeSnapshot> {
        self.check();
        let (stamp, owner) = with_interval(self.scope.cpu(), |cell| {
            cell.active.as_ref()?.funding.as_ref().map(FundingPin::observation)
        })?;
        let observation = FundingObservation { original: self, stamp, owner };
        let result = scheduling_runtime::observe_current_funding(&observation);
        self.check();
        result
    }
    pub(super) fn check(&self) {
        assert!(self.scope.is_masked());
        assert_eq!(self.scope.cpu(), nucleus_core::util::lockdep::current_cpu_index());
        with_interval(self.scope.cpu(), |cell| {
            let original = cell.active.as_ref().expect("borrowed Current interval vanished");
            assert!(cell.current_borrowed);
            assert_eq!((original.slot, original.task_id, original.running_generation),
                (self.slot, self.id, self.generation));
        });
    }
}
impl Drop for CurrentInterval<'_, '_> {
    fn drop(&mut self) {
        self.check();
        with_interval(self.scope.cpu(), |cell| cell.current_borrowed = false);
    }
}

fn capture_funding(slot: usize, cpu: usize) -> Option<Option<FundingPin>> {
    for _ in 0..4 {
        let own = current_identity::read_dispatch(slot)?;
        let token = donation_ledger::borrowed_context_charge_token(slot);
        let owner = match token {
            Some((owner_slot, _)) => current_identity::read_dispatch(owner_slot)?,
            None => own,
        };
        let owner_task_id = owner.task_id?;
        let stamp =
            local_dispatch::effective_charge_stamp(own.pager_charge, token.map(|_| Some(owner)))?;
        let funding = match stamp {
            Some(stamp) => Some(scheduling_runtime::pin_published_funding(
                stamp,
                cpu,
                owner_task_id,
                token.map_or(0, |(_, reply)| reply),
            )?),
            None => None,
        };
        let own_now = current_identity::read_dispatch(slot)?;
        let owner_now = match token {
            Some((owner_slot, _)) => current_identity::read_dispatch(owner_slot)?,
            None => own_now,
        };
        if donation_ledger::borrowed_context_charge_token(slot) == token
            && own_now.task_id == own.task_id
            && own_now.pager_charge == own.pager_charge
            && owner_now.task_id == owner.task_id
            && owner_now.pager_charge == owner.pager_charge
        {
            return Some(funding);
        }
        // Drop outside the CPU cell, in Context→Domain order, before retrying.
        drop(funding);
    }
    None
}

/// A successful frame claim transfers its ORIGINAL funding directly into the
/// executing bank under the same non-Copy CPU turn. There is no scalar-keyed
/// preparation mailbox, postclaim donor lookup, or later Scheduler opener.
/// The allocation/execution/placement banks have already been installed by the
/// claim. They retain backing even if terminal publication races this handoff.
pub(super) fn open_claimed(
    turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'static>,
    funding: Option<FundingPin>,
) {
    let (cpu, _) = turn.original_current().expect("interval lost original CPU turn");
    let receipt = turn.claimed_return().expect("interval lacks successful original claim");
    let slot = receipt.slot();
    assert!(super::stack_resources::cpu_leases::owns_prepared(cpu, slot),
        "claimed interval lost original allocation bank");
    assert!(crate::multitask::current_identity::execution::banks::owns_execution(cpu, slot),
        "claimed interval lost original execution bank");
    let start_ticks = crate::arch::rtc::ticks();
    let start_local_ns = crate::arch::clock::scheduler_local_nanos().unwrap_or(0);
    install(cpu, ExecutionInterval {
        slot,
        task_id: receipt.id,
        running_generation: receipt.running_generation,
        start_ticks,
        start_local_ns,
        funding,
    });
    // These are diagnostic baselines, never funding or interval authority.
    runqueue::set_exec_start_ticks(slot, start_ticks);
    runqueue::set_exec_start_local_ns(slot, start_local_ns);
}

fn exact_interval(cpu: usize, to: usize, task_id: u64, running_generation: u64) -> bool {
    with_interval(cpu, |cell| {
        cell.active.as_ref().is_some_and(|interval| {
            interval.slot == to
                && interval.task_id == task_id
                && interval.running_generation == running_generation
        })
    })
}

fn require_exact_interval(cpu: usize, to: usize, task_id: u64, running_generation: u64) {
    assert!(exact_interval(cpu, to, task_id, running_generation),
        "CPU return lacks its exact executing grant interval");
}

/// Architecture borrows the same executing funding, never a fresh donor or
/// budget pin. Settlement revokes this admission even while frame banks remain.
pub(in crate::multitask) fn original_claim_is_active(
    turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'static>,
) -> bool {
    let Some((cpu, _)) = turn.original_current() else { return false };
    let Some(receipt) = turn.claimed_return() else { return false };
    exact_interval(cpu, receipt.slot(), receipt.id, receipt.running_generation)
}

/// The ORIGINAL claim/turn, not a fresh identity or owner-word lookup, qualifies
/// the active grant through its last Current publication. A control-only turn
/// neither cancels nor settles another interval. Failed preclaim selections
/// drop their funding directly, outside every CPU leaf.
pub(in crate::multitask) fn finish_return(
    turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'_>,
    to: usize,
) {
    let (cpu, from) = turn.original_current().expect("return lost original CPU turn");
    if let Some(receipt) = turn.claimed_return() {
        assert_eq!(receipt.slot(), to, "return replaced original claim destination");
        assert!(super::stack_resources::cpu_leases::owns_prepared(cpu, to),
            "return lost original allocation custody");
        assert!(crate::multitask::current_identity::execution::banks::owns_execution(cpu, to),
            "return lost original execution custody");
        require_exact_interval(cpu, to, receipt.id, receipt.running_generation);
    } else {
        assert_eq!(from, to, "changed return lacks original claimed interval");
    }
}

/// Architecture borrows the original initial Current; it cannot manufacture a
/// new interval or use a later scheduler claim as bootstrap permission.
pub(super) fn original_initial_is_active(
    turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'static>,
) -> bool {
    let Some((cpu, slot)) = turn.original_current() else { return false; };
    if turn.claimed_return().is_some() { return false; }
    let Some(id) = current_identity::read_dispatch(slot).and_then(|p| p.task_id) else {
        return false;
    };
    let owner = runqueue::owner(slot);
    if owner.state != runqueue::RunOwnerState::Running || owner.cpu != Some(cpu) {
        return false;
    }
    with_interval(cpu, |cell| cell.active.as_ref().is_some_and(|i|
        (i.slot, i.task_id, i.running_generation) == (slot, id, owner.generation)
        && i.funding.is_none()))
}

/// Cold initial Running admission is distinct from dispatch. It can never
/// capture a donor or mint budget authority; ordinary selections cannot use it.
pub(super) fn begin_bootstrap(image: &super::bootstrap::InitialCpu<'_>, start_ticks: u64) {
    image.assert_materializing();
    let slot = image.slot();
    let task_id = image.id();
    assert!(runqueue::is_idle_slot(slot) || slot == super::ROOT_TASK_SLOT,
        "bootstrap interval used for ordinary selected execution");
    let owner = runqueue::owner(slot);
    assert_eq!(owner.state, runqueue::RunOwnerState::Running,
        "bootstrap interval opened outside initial Running custody");
    let cpu = owner.cpu.expect("bootstrap interval lacks CPU custody");
    assert_eq!(cpu, image.cpu(), "initial interval moved to another CPU");
    assert_eq!(current_identity::read_dispatch(slot).and_then(|identity| identity.task_id),
        Some(task_id), "initial interval lost original image UID");
    assert!(capture_funding(slot, cpu).expect("bootstrap funding is incomplete").is_none(),
        "bootstrap interval acquired budget authority");
    install(cpu, ExecutionInterval {
        slot, task_id, running_generation: owner.generation,
        start_ticks, start_local_ns: 0, funding: None,
    });
}

fn install(cpu: usize, interval: ExecutionInterval) {
    // Keep pin destruction outside the higher-ranked CPU cell even on panic.
    let mut incoming = Some(interval);
    with_interval(cpu, |cell| {
        assert!(!cell.current_borrowed, "interval replaced during original Current effects");
        assert!(
            cell.active.is_none(),
            "execution interval overwritten before settlement"
        );
        cell.settled = None;
        cell.active = incoming.take();
    });
}

pub(super) fn consume(cpu: usize, slot: usize, task_id: u64) -> ConsumedInterval {
    with_interval(cpu, |cell| {
        assert!(!cell.current_borrowed, "interval settled before last original Current effect");
        let Some(interval) = cell.active.as_ref() else {
            return if cell.settled == Some((slot, task_id)) {
                ConsumedInterval::AlreadySettled
            } else {
                ConsumedInterval::Absent
            };
        };
        assert_eq!(interval.slot, slot, "foreign slot consumed a CPU interval");
        assert_eq!(
            interval.task_id, task_id,
            "reused identity consumed a CPU interval"
        );
        assert_ne!(
            interval.running_generation, 0,
            "execution interval lacked a running generation"
        );
        let interval = cell.active.take().expect("checked interval vanished");
        cell.settled = Some((slot, task_id));
        ConsumedInterval::Active(interval)
    })
}

/// Native earlier-selection projection, retaining actual captured FundingPin.
/// This does not model hardware admission or grant a new CPU return scope.
#[cfg(test)]
pub(super) fn test_open_captured(slot: usize, start_ticks: u64) {
    let owner = runqueue::owner(slot);
    let cpu = owner.cpu.expect("modeled selection lacks CPU");
    let id = current_identity::read_dispatch(slot).unwrap().task_id.unwrap();
    let funding = capture_funding(slot, cpu).expect("actual root funding capture refused");
    install(cpu, ExecutionInterval {
        slot, task_id: id, running_generation: owner.generation,
        start_ticks, start_local_ns: 0, funding,
    });
}

#[cfg(test)]
mod tests;
