//! Owner: durable process-row group exit admission, not a yielding caller's stack.
//! Boundary: exact Current/ledger input -> short seal -> process mark -> queued pin -> Copy receipt.
//! Lifecycle: non-yielding admission transfers ownership before any wait; worker owns census/ACK.
//! Concurrency: approved creators ACK, fresh joins refuse; waits carry no references or raw guards.
//! Failure: external pending-exec refusal preserves its existing owner; partial worker effects poison.
//! Forbidden: backing pins as resume permission, directory-held drain, absence as completion.
//! Evidence: independent original-census/terminal worker; native CPU/IRQ ACKs remain modeled.
use super::super::process_placement::exit_membership::{self, PendingExit, Reason, Receipt};
use super::*;

/// Original non-yielding admission transfers its membership pin to the row.
/// The receipt alone may cross a yield; requester PID is diagnostic data.
pub(in crate::multitask) fn request(
    scope: &nucleus_core::util::lockdep::DispatchPreemption<'_>,
    pid: u64,
    requested_by_pid: Option<u64>,
) -> Option<Receipt> {
    if scope.cpu() != nucleus_core::util::lockdep::current_cpu_index()
        || nucleus_core::util::lockdep::held_spin_lock_depth() != 0 { return None; }
    let (handle, member) = exit_membership::target_by_pid(pid)?;
    if let Some(receipt) = exit_membership::queued_for(handle) { return Some(receipt); }
    let pending = PendingExit::begin(handle, member)?;
    // Non-sleeping exact-handle command; no census leaf spans it. Do not mask
    // caller-enabled IPIs during the process table leaf acquisition.
    super::super::process_table::mark_process_exiting_for_handle(handle, pid)?;
    Some(pending.publish(pid, Reason::Terminated { requested_by_pid }))
}

pub(super) fn terminate(pid: u64) -> bool {
    let receipt = {
        let preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
        let Some(receipt) = request(&preemption, pid, super::current_user_id()) else { return false; };
        receipt
    };
    wait(receipt);
    true
}

pub(super) fn exit_current() -> ! {
    let receipt = {
        let preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
        let binding = interrupts::without_interrupts(|| {
            let slot = cpu_local::current_cpu_task_slot()?;
            let (tid, _, handle, _) = owned_current_user_binding()?;
            let pid = current_process_id_for_handle(handle)?;
            Some((slot, tid, handle, pid))
        });
        let Some((slot, tid, handle, pid)) = binding else {
            drop(preemption);
            super::exit_current_task();
        };
        if let Some(receipt) = exit_membership::queued_for(handle) {
            receipt
        } else {
            let pending = PendingExit::begin(handle, (slot, tid))
                .expect("self exit lost exact publication seal");
            // A concurrent exec can retain exit_pending. The durable work waits
            // for its census release and successful process mark before effects.
            let _ = super::super::process_table::mark_process_exiting_for_handle(handle, pid);
            pending.publish(
                pid,
                Reason::Exited {
                    slot,
                    task_id: tid,
                    process_id: pid,
                    logical_cpu: preemption.cpu(),
                },
            )
        }
    };
    wait(receipt);
    // Preserve existing eager kernel-side revocation before the terminal halt.
    // This consumer still belongs to the separate cleanup ownership cutover.
    super::complete_retirement_side_effects();
    super::halt_current_retired_task()
}

fn wait(receipt: Receipt) {
    let start = crate::arch::clock::monotonic_nanos();
    while !receipt.complete() {
        // Only this non-yielding effect transaction retains CPU custody.
        // The worker ACKs or hands the ORIGINAL census/pin back to its row;
        // neither raw guards nor binding loans survive the following safe point.
        let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
        scope.mask_irqs();
        let _ = super::super::scheduler::group_exit::service(&scope);
        drop(scope);
        if receipt.complete() {
            break;
        }
        assert!(
            crate::arch::clock::monotonic_nanos().saturating_sub(start) < 2_000_000_000,
            "group exit timed out; durable process-row ownership retained"
        );
        // The ACK comes from tasks on this or another CPU; spinning on an
        // already-requested reschedule would hold a single CPU for a quantum.
        super::super::yield_now();
    }
}
