//! Owner: original faulting Current loan and exact registered worker binding.
//! Boundary: masked exception ingress -> typed service phase -> reader/FIFO CAS.
//! Lifecycle: committed fault -> worker publication -> latency floor -> loans drop.
//! Concurrency: SAME sender/receiver backing survives every effect; no raw leaf
//! spans validation, floor, mailbox or notification. No ledger mutation here.
//! Failure: exact wait mismatch/capacity refuses BEFORE clear, no generic fallback.
//! Forbidden: catalog Scheduler, scalar donor reacquisition or frame reads of Current.
//! Evidence: born-resource witnesses; exception IRQ/register/RSP inputs are models.
use super::*;
use crate::multitask::cpu_local;
use nucleus_core::util::lockdep::DispatchPreemption;

/// Fixed pager rendezvous result; no endpoint/reply object is owned here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PagerFaultHandoffOutcome {
    DirectSameCpu,
    DirectCrossCpu,
    WakeOnly,
    SenderMismatch,
    ReceiverMismatch,
    EligibilityUnavailable,
    DirectCustodyUnavailable,
    OrderingUnavailable,
}

/// One-shot latency accounting, not a funding/class grant. Both ORIGINAL
/// physical loans remain borrowed through the last vruntime effect. Durable
/// funding still belongs to the pager waiter syscall and donation ledger.
fn latency_floor(sender: &CurrentWait, token: u64, worker: &wake_publication::Binding, cpu: usize) {
    let donor = sender.original_pin().slot();
    let receiver = worker.original_pin().slot();
    if !sender.committed_pager(token) || donor == receiver || runqueue::is_idle_slot(donor)
        || runqueue::is_idle_slot(receiver) || !runqueue::owner(receiver).runnable {
        return;
    }
    let Ok(class) = local_dispatch::observed_class(receiver) else { return; };
    let donor_floor = runqueue::vruntime(donor).saturating_sub(IPC_DONATION_BONUS_NS);
    let class_floor = kernel_creation::ready_floor(cpu,
        Some(class == local_dispatch::LocalClass::System)).saturating_sub(IPC_DONATION_BONUS_NS);
    runqueue::lower_vruntime_ceiling(receiver, donor_floor.min(class_floor));
}

/// The caller keeps its ORIGINAL committed CurrentWait until this entire
/// producer finishes, then drops it BEFORE the blocking software trap.
pub(in crate::multitask) fn handoff(
    scope: &DispatchPreemption<'_>, sender: &CurrentWait, token: u64, worker_id: u64,
) -> PagerFaultHandoffOutcome {
    let outcome =
        handoff_with(scope, sender, token, worker_id, &runqueue_policy::online_affinity_mask);
    let _ = retry_stranded_worker(scope, outcome, worker_id);
    outcome
}

/// A refusal after the worker left the waiter registry leaves the fault
/// published but nothing to wake the worker, whose PagerService wait has no
/// deadline. Retry that exact wait so the worker wakes and claims the fault.
fn retry_stranded_worker(
    scope: &DispatchPreemption<'_>, outcome: PagerFaultHandoffOutcome, worker_id: u64,
) -> bool {
    use PagerFaultHandoffOutcome as Outcome;
    matches!(
        outcome,
        Outcome::ReceiverMismatch
            | Outcome::EligibilityUnavailable
            | Outcome::DirectCustodyUnavailable
            | Outcome::OrderingUnavailable
    ) && wake_retry::record_in(scope, worker_id, Some((runqueue::wait::REASON_PAGER_SERVICE, 0)))
}

fn handoff_with(
    scope: &DispatchPreemption<'_>, sender: &CurrentWait, token: u64, worker_id: u64,
    eligible_cpus: &dyn Fn(usize) -> u64,
) -> PagerFaultHandoffOutcome {
    use PagerFaultHandoffOutcome as Outcome;
    if !ipc_publication::scoped(scope) || scope.cpu() != sender.cpu()
        || !sender.committed_pager(token) {
        return Outcome::SenderMismatch;
    }
    // Only the fixed registered identity is resolved; no process/endpoint scan.
    let Some(worker) = wake_publication::Binding::resolve(worker_id) else {
        return Outcome::ReceiverMismatch;
    };
    if worker.slot() == sender.slot() || runqueue::is_idle_slot(worker.slot()) {
        return Outcome::ReceiverMismatch;
    }
    if !runqueue::wait::blocked(worker.slot())
        || cpu_local::task_execution_owner(worker.slot()).is_some() {
        // Precommit wake/assembly-owned worker: retain exact PagerService epoch,
        // never consume its frame or clear an unrelated/newly armed wait.
        // SAFETY: SAME masked scope and original sender survive all effects;
        // the common typed publisher retains worker/phase through its callback.
        return match unsafe {
            wake_publication::retained_wake_matching(worker,
                Some((runqueue::wait::REASON_PAGER_SERVICE, 0)),
                eligible_cpus, |worker| {
                    latency_floor(sender, token, worker, scope.cpu());
                    true
                })
        } {
            Ok(_) => Outcome::WakeOnly,
            Err(_) => Outcome::ReceiverMismatch,
        };
    }
    let Some(typed) = runqueue::wait::TypedWake::capture(&worker,
        runqueue::wait::REASON_PAGER_SERVICE, 0) else {
        return Outcome::ReceiverMismatch;
    };
    let target = runqueue_policy::affinity_target_cpu(worker.slot(),
        eligible_cpus(worker.slot()));
    if !ipc_publication::eligible(&worker, target) { return Outcome::EligibilityUnavailable; }
    // The shared original receiver producer preflights capacity and joins ONE
    // exact owner CAS + typed clear + FIFO. Floor runs before reader/publication
    // release. A refusal does not roll back, re-resolve or clear another epoch.
    if ipc_publication::publish_with(scope, &worker, &typed, target,
        || latency_floor(sender, token, &worker, target)).is_err() {
        return Outcome::DirectCustodyUnavailable;
    }
    drop(typed);
    drop(worker);
    if target == scope.cpu() { Outcome::DirectSameCpu } else { Outcome::DirectCrossCpu }
}

#[cfg(test)]
pub(super) mod tests;
