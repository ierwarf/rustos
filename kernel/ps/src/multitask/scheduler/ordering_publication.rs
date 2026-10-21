//! Owner: original masked CPU scope and exact recipient UID/backing loan.
//! Boundary: candidate scan -> original binding -> priority publication -> last effect.
//! Lifecycle: hints convey ordering only, never wake, frame, funding or execution rights.
//! Concurrency: one original priority publication excludes choice; raw Policy/FIFO
//! leaves end before budget/floor/notification work. Original Current lends donor state.
//! Failure: busy/closed/unqualified targets refuse; no catalog or scalar lifetime fallback.
//! Forbidden: saved-frame reads, fresh donor lookup, stale slot priority after reuse.
//! Evidence: born-resource producer/consumer tests; native IRQ/topology remain modeled.
use super::*;
use nucleus_core::util::lockdep::DispatchPreemption;

struct Target { binding: wake_publication::Binding, cpu: usize }
impl Target {
    fn capture(id: u64) -> Option<Self> {
        task_directory::candidates(id).find_map(|slot| {
            Self::from_binding(wake_publication::Binding::capture(slot, id)?)
        })
    }
    fn from_binding(binding: wake_publication::Binding) -> Option<Self> {
        let slot = binding.slot();
        let owner = runqueue::owner(slot);
        if !owner.runnable || runqueue::wait::blocked(slot)
            || !matches!(owner.state, runqueue::RunOwnerState::Local | runqueue::RunOwnerState::Running) {
            return None;
        }
        let cpu = owner.cpu?;
        if !ipc_publication::eligible(&binding, cpu) { return None; }
        Some(Self { binding, cpu })
    }
    fn record(&self) -> handoff_queue::FirstTurnHandoff {
        handoff_queue::FirstTurnHandoff { slot: self.binding.slot(), task_id: self.binding.id() }
    }
    fn ready(&self) -> bool {
        let owner = runqueue::owner(self.binding.slot());
        owner.runnable && owner.cpu == Some(self.cpu)
            && matches!(owner.state, runqueue::RunOwnerState::Local | runqueue::RunOwnerState::Running)
            && !runqueue::wait::blocked(self.binding.slot())
            && ipc_publication::eligible(&self.binding, self.cpu)
    }
}
fn scoped<R>(f: impl FnOnce(&DispatchPreemption<'_>) -> R) -> R {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    let result = f(&scope);
    #[cfg(not(test))]
    crate::multitask::irq::flush_deferred_target_reschedules();
    result
}
/// Optional latency floor borrows ORIGINAL Current, not a scalar task reacquisition.
/// Neither a hint nor this accounting effect moves funding or changes class.
fn floor(current: Option<&current_terminal::OriginalCurrent<'_, '_>>, target: &Target) {
    let Some(current) = current else { return; };
    let donor = current.member().0;
    let receiver = target.binding.slot();
    if donor == receiver || runqueue::is_idle_slot(donor) || !target.ready() { return; }
    let Ok(class) = local_dispatch::observed_class(receiver) else { return; };
    let caller = runqueue::vruntime(donor).saturating_sub(IPC_DONATION_BONUS_NS);
    let band = kernel_creation::ready_floor(target.cpu,
        Some(class == local_dispatch::LocalClass::System)).saturating_sub(IPC_DONATION_BONUS_NS);
    runqueue::lower_vruntime_ceiling(receiver, caller.min(band));
    current.check();
}
pub(in crate::multitask) fn target(id: u64) { scoped(|scope| { target_in(scope, id); }); }
fn target_in(scope: &DispatchPreemption<'_>, id: u64) -> bool {
    let Some(target) = Target::capture(id) else {
        let _publication = dispatch_priority::Publication::begin(scope, scope.cpu());
        dispatch_policy::lock(scope.cpu()).next_pick_hint = None;
        return false;
    };
    let current = current_terminal::OriginalCurrent::borrow(scope);
    let _publication = dispatch_priority::Publication::begin(scope, target.cpu);
    if !target.ready() { return false; }
    if target.cpu == scope.cpu() { floor(current.as_ref(), &target); }
    dispatch_policy::lock(target.cpu).next_pick_hint = Some(dispatch_policy::LatencyPickHint {
        slot: target.binding.slot(), task_id: target.binding.id(),
    });
    true
}
pub(in crate::multitask) fn spawn(id: u64) { scoped(|scope| { spawn_in(scope, id); }); }
fn spawn_in(scope: &DispatchPreemption<'_>, id: u64) -> bool {
    let Some(target) = Target::capture(id) else { return false; };
    let current = current_terminal::OriginalCurrent::borrow(scope);
    let _publication = dispatch_priority::Publication::begin(scope, target.cpu);
    if !target.ready() { return false; }
    let inserted = dispatch_policy::lock(target.cpu).spawn_pick_hints.enqueue(target.record())
        .expect("scheduler spawn handoff queue overflow");
    if inserted && target.cpu == scope.cpu() { floor(current.as_ref(), &target); }
    inserted
}
pub(in crate::multitask) fn latency(id: u64) -> bool { scoped(|scope| latency_in(scope, id)) }
fn latency_in(scope: &DispatchPreemption<'_>, id: u64) -> bool {
    let Some(target) = Target::capture(id) else { return false; };
    let _publication = dispatch_priority::Publication::begin(scope, target.cpu);
    let slot = target.binding.slot();
    if !target.ready() || !local_dispatch::stable_user_hint_class(slot) { return false; }
    let hint = dispatch_policy::LatencyPickHint { slot, task_id: target.binding.id() };
    let mut policy = dispatch_policy::lock(target.cpu);
    for offset in 0..policy.latency_pick_hint_len {
        let index = (policy.latency_pick_hint_head + offset) % MAX_LATENCY_HANDOFF_HINTS;
        if policy.latency_pick_hints[index].is_some_and(|previous| previous.slot == slot) {
            policy.latency_pick_hints[index] = Some(hint);
            return true;
        }
    }
    if policy.latency_pick_hint_len == MAX_LATENCY_HANDOFF_HINTS { return false; }
    let tail = (policy.latency_pick_hint_head + policy.latency_pick_hint_len) % MAX_LATENCY_HANDOFF_HINTS;
    policy.latency_pick_hints[tail] = Some(hint);
    policy.latency_pick_hint_len += 1;
    true
}
pub(in crate::multitask) fn process(pid: u64) -> Option<u64> { scoped(|scope| process_in(scope, pid)) }
fn process_in(scope: &DispatchPreemption<'_>, pid: u64) -> Option<u64> {
    // Keep the winning ORIGINAL target across the entire bounded comparison;
    // never remember its scalar slot then reacquire a potentially different task.
    // The published process id is an advisory pre-filter only: capturing a
    // binding pins the task, and doing that for every slot on every call
    // with no parked receiver cost ~150k cycles. The captured binding's exact
    // process id below remains the authority.
    let published = |slot: usize| super::super::current_identity::read_dispatch(slot)
        .is_some_and(|publication| publication.process_id == Some(pid));
    let capture = |slot: usize| {
        let id = stack_resources::task_start::read(slot)?.id;
        let target = Target::from_binding(wake_publication::Binding::capture(slot, id)?)?;
        (target.binding.context().process_id == Some(pid)).then_some(target)
    };
    // Rank on the advisory publication first and pin only the winner: each
    // pinned candidate copied a full task context twice, on every call. When
    // the winner cannot be pinned, fall back to pinning every candidate.
    let key = |slot: usize| (runqueue::vruntime(slot), slot);
    let target = match (0..MAX_TASK).filter(|&slot| published(slot)).min_by_key(|&slot| key(slot)) {
        Some(slot) => match capture(slot) {
            Some(target) => target,
            None => (0..MAX_TASK).filter(|&slot| published(slot)).filter_map(capture)
                .min_by_key(|target| key(target.binding.slot()))?,
        },
        None => return None,
    };
    let current = current_terminal::OriginalCurrent::borrow(scope);
    let _publication = dispatch_priority::Publication::begin(scope, target.cpu);
    if !target.ready() { return None; }
    floor(current.as_ref(), &target);
    let record = sync_handoff::SyncHandoffRecord::new(target.binding.slot(), target.binding.id());
    let _ = sync_handoff::enqueue(target.cpu, record);
    #[cfg(not(test))]
    crate::multitask::irq::request_target_reschedule(target.cpu);
    Some(target.binding.id())
}

#[cfg(test)]
pub(super) mod tests;
