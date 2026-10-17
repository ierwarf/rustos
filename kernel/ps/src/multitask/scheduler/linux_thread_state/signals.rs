//! Owner: original recipient binding and process membership/cold identity cohort.
//! Boundary: masked CPU -> exact recipient -> census -> gates/event -> signal/wake.
//! Lifecycle: original pins survive every effect; no process/table leaf crosses wake.
//! Concurrency: census excludes joins/exec/final terminal turn; cohort seals all dispatch admission.
//! Failure: untouched acquisition refuses; losing ownership after effects fails closed.
//! Forbidden: PID scans as lifetime, affinity credentials for job control, fresh wake pin after CONT.
//! Evidence: actual born descriptors, group/recipient regressions and source-refining cold models.
use super::{LINUX_THREAD_STATES, UserAbi, MAX_TASK, process_table, stack_resources};
use super::super::{current_terminal::OriginalCurrent,
    runqueue_policy, task_directory, wake_publication, TaskContext};
use crate::multitask::{current_identity as identity, process_placement};
use nucleus_core::util::lockdep::DispatchPreemption;

struct Recipient {
    pin: identity::execution::BindingPin,
    context: TaskContext,
}
impl Recipient {
    fn capture(slot: usize, id: u64, pid: u64) -> Option<Self> {
        // Metadata admission also permits suspended/job-stopped recipients.
        // This same original loan spans CONT's group effects and final wake.
        let pin = identity::execution::pin_donation(slot, id)?;
        let context = stack_resources::context::read(slot)?;
        if stack_resources::task_start::read(slot)?.id != id || !context.user_mode
            || context.user_abi != Some(UserAbi::Linux) || context.process_id != Some(pid)
            || context.process_handle.is_none() { return None; }
        Some(Self { pin, context })
    }
    fn resolve(pid: u64, id: u64) -> Option<Self> {
        task_directory::candidates(id).find_map(|slot| Self::capture(slot, id, pid))
    }
    fn member(&self) -> (usize, u64) { (self.pin.slot(), self.pin.id()) }
    fn queue(self, scope: &DispatchPreemption<'_>, signal: u64, events: u32,
        eligible_cpus: &dyn Fn(usize) -> u64) -> Result<bool, ()> {
        check_scope(scope);
        if signal == 0 { return Ok(true); }
        let Some(bit) = crate::user::sysops::linux::linux_signal_bit(signal) else { return Ok(false); };
        if signal == rustos_user_abi::linux::SIGCONT || signal == rustos_user_abi::linux::SIGKILL {
            let Some(group) = Group::capture(&self) else { return Err(()); };
            group.finish(scope, None, eligible_cpus, |_| {});
        }
        let slot = self.pin.slot();
        let wake = {
            let mut cell = LINUX_THREAD_STATES[slot].lock();
            if cell.owner_tid != Some(self.pin.id()) { return Ok(false); }
            let Some(state) = cell.state.as_mut() else { return Ok(false); };
            if signal == rustos_user_abi::linux::SIGCHLD { state.pending_sigchld_events |= events; }
            else if events != 0 { return Ok(false); }
            state.pending_signals |= bit;
            identity::raise_signal_pending(slot);
            state.signal_mask & bit == 0
        };
        if !wake { return Ok(true); }
        // Transfer the SAME original unit, never resolve a new PID/UID loan.
        let Some(binding) = wake_publication::Binding::from_original(self.pin) else { return Ok(false); };
        // SAFETY: same masked CPU; owned binding covers Linux through final wake effects.
        Ok(unsafe { wake_publication::guarded_owned(binding, eligible_cpus) })
    }
}

struct Group {
    change: process_placement::Change,
    cohort: identity::cohort::Cohort,
    members: [(usize, u64); MAX_TASK],
    len: usize,
    pid: u64,
}
impl Group {
    fn capture(target: &Recipient) -> Option<Self> {
        let handle = target.context.process_handle?;
        let change = process_placement::Change::begin(handle, target.member())?;
        change.process_pin()?;
        let mut members = [(0, 0); MAX_TASK];
        let mut len = 0;
        for member in change.live_members() { members[len] = member; len += 1; }
        let cohort = identity::cohort::Cohort::begin_job_control(&members[..len])?;
        let pid = target.context.process_id?;
        for &(slot, id) in &members[..len] {
            let context = stack_resources::context::read(slot)?;
            if stack_resources::task_start::read(slot)?.id != id || !context.user_mode
                || context.user_abi != Some(UserAbi::Linux) || context.process_handle != Some(handle)
                || context.process_id != Some(pid) { return None; }
        }
        Some(Self { change, cohort, members, len, pid })
    }
    fn finish(mut self, scope: &DispatchPreemption<'_>, stop_signal: Option<u8>,
        eligible_cpus: &dyn Fn(usize) -> u64, before_publish: impl FnOnce(&Self)) -> bool {
        check_scope(scope);
        self.change.begin_effects();
        self.cohort.begin_effects();
        let mut changed = false;
        for &(slot, id) in &self.members[..self.len] {
            let slot_changed = self.cohort.set_job_stopped(slot, id, stop_signal.is_some())
                .expect("job-control cohort lost original member");
            changed |= slot_changed;
            if slot_changed { runqueue_policy::request_owner_reschedule(slot); }
        }
        if changed {
            let pin = self.change.process_pin().expect("job-control census lost process pin");
            if let Some(Some(parent)) = process_table::note_job_control(pin, self.pid, stop_signal)
                && parent != 0 {
                let event = if stop_signal.is_some() {
                    rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_STOP
                } else { rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_CONTINUE };
                let _ = process_sigchld_in(scope, parent, event, eligible_cpus);
            }
        }
        before_publish(&self);
        // One release opens every member only AFTER all gates, reschedules,
        // process state and parent recipient effects. No synthetic mask write.
        self.change.publish_job_control(&mut self.cohort);
        self.cohort.finish_published();
        self.change.finish();
        changed
    }
}
fn check_scope(scope: &DispatchPreemption<'_>) {
    assert!(scope.is_masked());
    assert_eq!(scope.cpu(), nucleus_core::util::lockdep::current_cpu_index());
    assert_eq!(nucleus_core::util::lockdep::held_spin_lock_depth(), 0);
}

/// `Err` is untouched transient contention, never a missing recipient.
pub(in crate::multitask) fn queue(pid: u64, id: u64, signal: u64) -> Result<bool, ()> {
    let mut scope = DispatchPreemption::enter(); scope.mask_irqs();
    queue_in(&scope, pid, id, signal, &runqueue_policy::online_affinity_mask)
}
fn queue_in(scope: &DispatchPreemption<'_>, pid: u64, id: u64, signal: u64,
    eligible_cpus: &dyn Fn(usize) -> u64) -> Result<bool, ()> {
    check_scope(scope);
    let Some(target) = Recipient::resolve(pid, id) else { return Ok(false); };
    target.queue(scope, signal, 0, eligible_cpus)
}
pub(in crate::multitask) fn process_sigchld(pid: u64, events: u32) -> bool {
    let mut scope = DispatchPreemption::enter(); scope.mask_irqs();
    process_sigchld_in(&scope, pid, events, &runqueue_policy::online_affinity_mask)
}
fn process_sigchld_in(scope: &DispatchPreemption<'_>, pid: u64, events: u32,
    eligible_cpus: &dyn Fn(usize) -> u64) -> bool {
    check_scope(scope);
    if events == 0 || events & !rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_MASK != 0 { return false; }
    // Candidate IDs are discovery hints only. Every candidate must acquire an
    // original recipient unit and qualify its exact process before any effect.
    let mut candidates = [(usize::MAX, 0_u64); MAX_TASK];
    let mut len = 0;
    for slot in 0..MAX_TASK {
        if let Some(start) = stack_resources::task_start::read(slot) {
            candidates[len] = (slot, start.id); len += 1;
        }
    }
    candidates[..len].sort_unstable_by_key(|&(slot, id)| (id != pid, slot));
    for &(slot, id) in &candidates[..len] {
        if let Some(target) = Recipient::capture(slot, id, pid) {
            return target.queue(scope, rustos_user_abi::linux::SIGCHLD, events, eligible_cpus) == Ok(true);
        }
    }
    false
}
/// None leaves the pending STOP cause unconsumed for a later entry retry.
pub(in crate::multitask) fn stop_current(signal: u64) -> Option<bool> {
    let mut scope = DispatchPreemption::enter(); scope.mask_irqs();
    stop_in(&scope, signal, &runqueue_policy::online_affinity_mask)
}
fn stop_in(scope: &DispatchPreemption<'_>, signal: u64,
    eligible_cpus: &dyn Fn(usize) -> u64) -> Option<bool> {
    check_scope(scope);
    let Ok(signal) = u8::try_from(signal) else { return None; };
    let Some(current) = OriginalCurrent::borrow(scope) else { return None; };
    let context = current.context();
    let Some(pid) = context.process_id else { return None; };
    let (slot, id) = current.member();
    let Some(target) = Recipient::capture(slot, id, pid) else { return None; };
    let Some(group) = Group::capture(&target) else { return None; };
    let changed = group.finish(scope, Some(signal), eligible_cpus, |_| {});
    current.check();
    Some(changed)
}

#[cfg(test)]
pub(in crate::multitask::scheduler) mod tests;
