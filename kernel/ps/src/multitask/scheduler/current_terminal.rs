//! Owner: original Current fault/exit and exact target task termination.
//! Boundary: original parked allocation/MM, UID permit and interval, not a catalog.
//! Lifecycle: original Current/target loan -> terminal Producer -> durable consumers.
//! Concurrency: original masked CPU borrows both banks through final stop effects;
//! target binding transfers to Producer before borrower drain. No raw leaf crosses work.
//! Failure: incomplete Current, overlapping interval borrow or foreign target refuses.
//! Forbidden: scalar UID/generation as lifetime, new MM activation, early interval charge.
//! Evidence: born resources/native original bank tests; hardware RSP ACK remains separate.
use super::{TaskContext, TaskRetireReason, execution_interval, retirement_cleanup, runqueue,
    runqueue_policy, stack_resources, terminal_publication, terminal_quarantine};
use crate::multitask::{UserFaultDisposition, cpu_local, current_identity as identity, process_table};
use nucleus_core::util::lockdep::DispatchPreemption;

/// All three loans borrow ORIGINAL parked non-Copy units under the SAME scope.
/// The interval's unique borrow excludes consumption until the last effect;
/// terminal close does not force fresh identity/physical/MM admission.
pub(super) struct OriginalCurrent<'scope, 'irq> {
    allocation: stack_resources::cpu_leases::CurrentAllocation<'scope, 'irq>,
    permit: identity::execution::banks::CurrentPermit<'scope, 'irq>,
    interval: execution_interval::CurrentInterval<'scope, 'irq>,
    scope: &'scope DispatchPreemption<'irq>,
}
impl<'scope, 'irq> OriginalCurrent<'scope, 'irq> {
    pub(super) fn borrow(scope: &'scope DispatchPreemption<'irq>) -> Option<Self> {
        let allocation = stack_resources::cpu_leases::CurrentAllocation::borrow(scope)?;
        let slot = allocation.slot();
        if cpu_local::current_cpu_task_slot() != Some(slot) { return None; }
        let permit = identity::execution::banks::CurrentPermit::borrow(scope, slot)?;
        let owner = runqueue::owner(slot);
        if !(owner.state == runqueue::RunOwnerState::Running && owner.cpu == Some(scope.cpu()))
            && !owner.state.is_terminal() { return None; }
        let interval = execution_interval::CurrentInterval::borrow(scope, slot, allocation.id())?;
        Some(Self { allocation, permit, interval, scope })
    }
    pub(super) fn check(&self) {
        assert!(self.scope.is_masked());
        assert_eq!(self.scope.cpu(), nucleus_core::util::lockdep::current_cpu_index());
        self.allocation.check();
        self.permit.check();
        self.interval.check();
        assert_eq!(cpu_local::current_cpu_task_slot(), Some(self.allocation.slot()));
    }
    pub(super) fn context(&self) -> TaskContext { self.check(); self.allocation.context() }
    pub(super) fn runtime_snapshot(&self) -> Option<crate::multitask::SchedulingContextRuntimeSnapshot> {
        self.check();
        self.interval.runtime_snapshot()
    }
    pub(super) fn member(&self) -> (usize, u64) {
        self.check(); (self.allocation.slot(), self.allocation.id())
    }
    fn stopped(&self) -> bool {
        self.check();
        runqueue::owner(self.allocation.slot()).state.is_terminal()
            || identity::terminal_retired_matches(self.allocation.slot(), self.allocation.id())
    }
    fn stop(&self, reason: TaskRetireReason) -> bool {
        self.check();
        let slot = self.allocation.slot();
        let id = self.allocation.id();
        assert_ne!(slot, super::ROOT_TASK_SLOT, "scheduler root kernel task cannot exit");
        if self.stopped() {
            retirement_cleanup::mark_reap_pending();
            return true;
        }
        let Some(producer) = terminal_quarantine::begin(slot, Some(id), reason, false) else {
            // Only an actual retained first Producer, not a numerical reason,
            // can defer a kernel caller which still owns an executor reference.
            return retirement_cleanup::producer::Producer::waiting_for_original(slot, id);
        };
        // SAFETY: SAME original masked scope and both parked Current units plus
        // unconsumed interval remain borrowed through source and recipient work.
        unsafe { terminal_publication::guarded(producer, &runqueue_policy::online_affinity_mask); }
        self.check();
        true
    }
}

pub(in crate::multitask) fn fault(
    vector: u8, error_code: Option<u64>, cr2: u64, rip: u64, _rsp: u64,
) -> UserFaultDisposition {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    fault_in(&scope, vector, error_code, cr2, rip)
}
fn fault_in(scope: &DispatchPreemption<'_>, vector: u8, error_code: Option<u64>, cr2: u64, rip: u64)
    -> UserFaultDisposition {
    let Some(current) = OriginalCurrent::borrow(scope) else { return UserFaultDisposition::Unhandled; };
    if !current.context().user_mode { return UserFaultDisposition::Unhandled; }
    if current.stopped() || current.stop(TaskRetireReason::UserFault { vector, error_code, cr2, rip }) {
        UserFaultDisposition::Retired
    } else { UserFaultDisposition::Unhandled }
}

pub(in crate::multitask) fn exit() {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    let current = OriginalCurrent::borrow(&scope).expect("task exit lacks original Current custody");
    assert!(current.stop(TaskRetireReason::Exited), "task exit lost original terminal producer");
}

struct TargetStop {
    binding: identity::execution::BindingPin,
    context: TaskContext,
}
impl TargetStop {
    fn capture(id: u64) -> Option<Self> {
        super::task_directory::candidates(id).find_map(|slot| {
            let binding = identity::execution::pin_donation(slot, id)?;
            let context = stack_resources::context::read(binding.slot())?;
            if !context.user_mode { return None; }
            Some(Self { binding, context })
        })
    }
    fn producer(self, requested_by_pid: Option<u64>) -> Option<retirement_cleanup::producer::Producer> {
        let (slot, id) = (self.binding.slot(), self.binding.id());
        if let Some((handle, pid)) = self.context.process_handle.zip(self.context.process_id) {
            let authorization = crate::multitask::process_placement::terminal::Turn::begin(handle, (slot, id))?;
            if authorization.last_desired()
                && process_table::mark_process_exiting_for_handle(handle, pid).is_none() { return None; }
            // Exit authorization is not final stop election. This original
            // census lives through process mark; final Producer elects afresh
            // after old binding drain while retaining its own original census.
            drop(authorization);
        }
        let producer = terminal_quarantine::begin(slot, Some(id),
            TaskRetireReason::Terminated { requested_by_pid }, false);
        // Transfer to ORIGINAL terminal producer before releasing target loan.
        drop(self);
        producer
    }
}

pub(in crate::multitask) fn terminate(id: u64) -> bool {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    terminate_in(&scope, id)
}
fn terminate_in(scope: &DispatchPreemption<'_>, id: u64) -> bool {
    if !scope.is_masked() || scope.cpu() != nucleus_core::util::lockdep::current_cpu_index()
        || nucleus_core::util::lockdep::held_spin_lock_depth() != 0 { return false; }
    let requested_by_pid = {
        let current = OriginalCurrent::borrow(scope);
        current.as_ref().and_then(|current| current.context().process_id)
    };
    let Some(target) = TargetStop::capture(id) else { return false; };
    let Some(producer) = target.producer(requested_by_pid) else { return false; };
    // SAFETY: original target Producer owns all stop effects; SAME masked caller
    // remains alive until recipient work/consumer publication completes.
    unsafe { terminal_publication::guarded(producer, &runqueue_policy::online_affinity_mask); }
    true
}

#[cfg(test)]
pub(super) mod tests;
