//! Interrupt-excluded access to the current CPU task and process generation.
//!
//! - **Owner:** `kernel-ps` owns the current-task identity published by the
//!   scheduler.
//! - **Boundary:** Cross-crate callers receive bounded snapshots or execute a
//!   closure while the exact current identity is stable.
//! - **Lifecycle:** Dispatch publishes the new owner before it becomes
//!   observable; retirement withdraws it before slot reuse.
//! - **Concurrency:** Every mutable access uses the scheduler's interrupt
//!   exclusion and lockdep owner token; no returned reference may escape.
//! - **Failure:** Missing, retired, or generation-mismatched state returns no
//!   authority.
//! - **Forbidden:** No cached raw scheduler pointer, AP-local inference, or
//!   service call while borrowing mutable scheduler state.
//! - **Evidence:** `scheduler-lifecycle`, `scheduler-dispatch`,
//!   `monotonic-deadline-lifecycle`, and `user-memory-access`.
use x86_64::{VirtAddr, instructions::interrupts};

use super::cpu_local;
use super::{
    CurrentUserSnapshot, RetainedCurrentUserAddressSpace, RetainedCurrentUserProcessState,
    UserFaultDisposition, WaitChildResult, current_cpu_task_slot_admitted, process_table,
    scheduler_mut, scheduler_ref,
};
use crate::debug;
use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging::ProcessAddressSpace;
use crate::user::abi::UserAbi;
use crate::user::linux::{LinuxProcessState, LinuxThreadState};
use crate::user::process_state::{ProcessSecurityContext, UserProcessState};
use crate::user::sysops::usermem_profile as user_copy_profile;

impl CurrentUserSnapshot {
    pub(crate) const fn new(
        abi: UserAbi,
        thread_id: u64,
        process_id: u64,
        process_generation: u64,
        console_session: ConsoleSessionHandle,
        security: ProcessSecurityContext,
    ) -> Self {
        Self {
            abi,
            thread_id,
            process_id,
            process_generation,
            console_session,
            security,
        }
    }

    pub const fn abi(self) -> UserAbi {
        self.abi
    }

    pub const fn thread_id(self) -> u64 {
        self.thread_id
    }

    pub const fn process_id(self) -> u64 {
        self.process_id
    }

    pub const fn process_generation(self) -> u64 {
        self.process_generation
    }

    pub const fn console_session(self) -> ConsoleSessionHandle {
        self.console_session
    }

    pub const fn security(self) -> ProcessSecurityContext {
        self.security
    }
}

pub fn current_user_address_space() -> Option<RetainedCurrentUserAddressSpace> {
    let (thread_id, abi, process) = retain_current_user_process_binding()?;
    let identity = process.live_identity()?;
    Some(RetainedCurrentUserAddressSpace {
        abi,
        process_id: process.process_id(),
        thread_id,
        identity,
        process,
    })
}

pub fn current_user_id() -> Option<u64> {
    current_user_log_ids().map(|(_, thread_id)| thread_id)
}

pub fn current_task_id() -> Option<u64> {
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return None;
    }
    interrupts::without_interrupts(|| {
        if let Some(identity) = published_current_identity() {
            return identity.task_id;
        }
        // SAFETY: interrupts are masked, so the current slot is stable.
        unsafe { scheduler_ref().current_task_id() }
    })
}

/// The identity published for the task this CPU is running, if any.
///
/// Callers must already have interrupts masked. A `None` result means the
/// record was never published or a writer was mid-update, and the caller must
/// fall back to the locked scheduler query.
fn published_current_identity() -> Option<super::current_identity::TaskIdentity> {
    super::current_identity::read(cpu_local::current_cpu_task_slot()?)
}

pub fn linux_task_affinity(
    target_task_id: u64,
    online_mask: u64,
) -> Result<u64, super::scheduler::AffinityError> {
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return Err(super::scheduler::AffinityError::MissingTask);
    }
    // SAFETY: local interrupts are excluded for the complete scheduler guard;
    // the CPU has an admitted current slot and no reference escapes the call.
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref().linux_task_affinity(target_task_id, online_mask)
    })
}

pub fn set_linux_task_affinity(
    target_task_id: u64,
    requested_mask: u64,
    online_mask: u64,
) -> Result<super::scheduler::AffinityCommit, super::scheduler::AffinityError> {
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return Err(super::scheduler::AffinityError::MissingTask);
    }
    // SAFETY: local interrupts are excluded for the complete scheduler guard;
    // the exact current CPU slot is admitted and the commit returns by value.
    let commit = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().set_linux_task_affinity(target_task_id, requested_mask, online_mask)
    })?;
    if commit.reschedule_required {
        super::request_deferred_reschedule();
    }
    Ok(commit)
}

pub fn windows_process_affinity(
    online_mask: u64,
) -> Result<super::scheduler::ProcessAffinitySnapshot, super::scheduler::AffinityError> {
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return Err(super::scheduler::AffinityError::MissingTask);
    }
    // SAFETY: local interrupts are excluded for the complete scheduler guard;
    // the CPU-local current task binding remains stable during the snapshot.
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref().windows_process_affinity(online_mask)
    })
}

pub fn set_windows_process_affinity(
    requested_mask: u64,
    online_mask: u64,
) -> Result<super::scheduler::AffinityCommit, super::scheduler::AffinityError> {
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return Err(super::scheduler::AffinityError::MissingTask);
    }
    // SAFETY: local interrupts are excluded for the complete scheduler guard;
    // scheduler mutation and current-slot publication remain one critical section.
    let commit = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().set_windows_process_affinity(requested_mask, online_mask)
    })?;
    if commit.reschedule_required {
        super::request_deferred_reschedule();
    }
    Ok(commit)
}

pub fn set_windows_current_thread_affinity(
    requested_mask: u64,
    online_mask: u64,
) -> Result<super::scheduler::AffinityCommit, super::scheduler::AffinityError> {
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return Err(super::scheduler::AffinityError::MissingTask);
    }
    // SAFETY: local interrupts are excluded for the complete scheduler guard;
    // the scheduler owns the exact current Windows thread for this mutation.
    let commit = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().set_windows_current_thread_affinity(requested_mask, online_mask)
    })?;
    if commit.reschedule_required {
        super::request_deferred_reschedule();
    }
    Ok(commit)
}

pub fn current_user_log_ids() -> Option<(u64, u64)> {
    // Early AP boot deliberately records lifecycle diagnostics before the BSP
    // admits an idle task for that CPU. Logging is observational and must not
    // manufacture scheduler authority or turn that valid phase into a panic.
    // The logger may run while an arbitrary raw lock is held. It must not
    // recurse into or wait for the scheduler merely to decorate a record.
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return None;
    }
    interrupts::without_interrupts(|| {
        published_or_scheduler_user_log_ids(published_current_user_log_ids(), || {
            // SAFETY: interrupts are masked, so the current slot is stable.
            unsafe { scheduler_ref().current_user_log_ids() }
        })
    })
}

/// Uses a complete per-CPU diagnostic identity before consulting scheduler
/// authority. The outer `None` is deliberately reserved for an absent, odd,
/// or incomplete publication; a complete kernel-task identity is `Some(None)`
/// and must not take the scheduler lock merely to confirm it has no user PID.
#[inline]
fn published_or_scheduler_user_log_ids<F>(
    published: Option<Option<(u64, u64)>>,
    scheduler_fallback: F,
) -> Option<(u64, u64)>
where
    F: FnOnce() -> Option<(u64, u64)>,
{
    published.unwrap_or_else(scheduler_fallback)
}

/// Returns a complete user log pair, a definitive kernel-task absence, or
/// `None` when the seqlock record must be retried through scheduler authority.
/// Callers must already have interrupts masked.
fn published_current_user_log_ids() -> Option<Option<(u64, u64)>> {
    published_current_identity()?.complete_user_log_ids()
}

pub fn user_log_ids_for_task(task_id: u64) -> Option<(u64, u64)> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().user_log_ids_for_task(task_id) })
}

pub fn current_user_process_id() -> Option<u64> {
    current_user_log_ids().map(|(process_id, _)| process_id)
}

/// Snapshot the exact process and address-space authority of the active user
/// task. This deliberately fails during exec or process teardown rather than
/// handing a caller a PID-only identity that could be reused.
pub fn current_user_process_identity() -> Option<process_table::ProcessIdentity> {
    let (_, _, process) = retain_current_user_process_binding()?;
    process.live_identity()
}

/// Return a live, generation-bound process identity for a non-current PID.
/// The scheduler and process table own the liveness decision; callers must not
/// infer it from a PID map or an old retained state reference.
pub fn live_user_process_identity_by_pid(
    process_id: u64,
) -> Option<process_table::ProcessIdentity> {
    process_table::live_process_identity_by_pid(process_id)
}

/// Resolve one live process only when its current executable path is exactly
/// the expected kernel-private path. The identity is sampled before and after
/// the state observation, so a concurrent exec/exit cannot publish authority
/// based on a stale pathname.
pub fn live_user_process_identity_with_exact_exec_path(
    process_id: u64,
    expected_exec_path: &str,
) -> Option<process_table::ProcessIdentity> {
    let identity = live_user_process_identity_by_pid(process_id)?;
    let matches_path =
        with_process_state_by_pid(process_id, |state| state.exec_path() == expected_exec_path)?;
    (matches_path && live_user_process_identity_by_pid(process_id) == Some(identity))
        .then_some(identity)
}

pub fn current_user_process_thread_count() -> Option<usize> {
    let process_id = current_user_process_id()?;
    process_table::thread_count_by_pid(process_id)
}

/// Whether the running thread may have a pending Linux signal.
///
/// Syscall return calls this on every exit. A `true` answer only means the
/// authoritative state has to be consulted; a `false` answer is conclusive and
/// costs no scheduler lock.
pub fn current_thread_may_have_pending_signals() -> bool {
    interrupts::without_interrupts(|| {
        cpu_local::current_cpu_task_slot().is_none_or(super::current_identity::signal_pending)
    })
}

pub fn current_linux_thread_state() -> Option<LinuxThreadState> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_linux_thread_state() })
}

pub fn current_user_stack_state() -> Option<super::UserStackState> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_user_stack_state() })
}

pub fn current_user_thread_id() -> Option<u64> {
    current_user_log_ids().map(|(_, thread_id)| thread_id)
}

/// Return the ABI bound to the active user task.
///
/// Syscall entry/return validation runs on the current task's kernel stack and
/// needs the scheduler binding, not a mutable credential snapshot. Taking the
/// process-state lock here made every syscall contend with unrelated threads
/// mutating handles, mappings, signals, and other process state. The scheduler
/// binding is read with interrupts masked so the current slot and its ABI are
/// one coherent observation.
pub fn current_user_abi() -> Option<UserAbi> {
    interrupts::without_interrupts(|| {
        if let Some(identity) = published_current_identity() {
            return identity.user_binding().map(|(_, abi, _, _)| abi);
        }
        // SAFETY: interrupts are masked, so the current slot is stable.
        unsafe {
            scheduler_ref()
                .current_user_process_binding()
                .map(|(_, abi, _, _)| abi)
        }
    })
}

/// Return the current task, ABI, and address-space identity without acquiring
/// the process-state lock. Scheduler-owned wait paths use this snapshot before
/// they have installed a waiter or deadline and must not inherit unrelated
/// same-process lock latency.
pub fn current_user_wait_binding() -> Option<(u64, UserAbi, u64)> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_user_wait_binding() })
}

/// Snapshot the current user task's identity and security state.
///
/// # Why this reads the published record first
///
/// This asks only about the task already running on the asking CPU, so the
/// answer is per-CPU by construction and the global scheduler lock adds nothing
/// but serialization. The acquisition census measured this exact call site at
/// 7,197 acquisitions per second at 8 vCPU - 6.8% of all global scheduler lock
/// traffic - purely because it took the locked path while
/// `current_user_abi` and `retain_current_user_process_binding` beside it
/// already took the published one. `TaskIdentity::user_binding` returns the
/// same four fields `current_user_process_binding` does; the only difference
/// was the lock.
///
/// The fallback is not optional and must not be removed: `published_current_identity`
/// returns `None` for a slot that was never published *and* for a reader that
/// caught a writer between the two halves of a seqlock update. Both cases are
/// ordinary, and both must resolve through scheduler authority rather than be
/// reported as "no user task" - answering `None` here would tell a syscall that
/// its own caller does not exist.
pub fn current_user_snapshot() -> Option<CurrentUserSnapshot> {
    let (thread_id, abi, process_handle, console_session) = interrupts::without_interrupts(|| {
        if let Some(identity) = published_current_identity() {
            return identity.user_binding();
        }
        // SAFETY: interrupts are masked, so the current slot is stable.
        unsafe { scheduler_ref().current_user_process_binding() }
    })?;
    process_table::with_process_state(process_handle, |process_id, process_state| {
        CurrentUserSnapshot::new(
            abi,
            thread_id,
            process_id,
            u64::from(process_handle.generation()),
            console_session,
            process_state.security(),
        )
    })
}

pub fn current_scheduling_context_runtime_snapshot()
-> Option<super::SchedulingContextRuntimeSnapshot> {
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return None;
    }
    // SAFETY: local interrupts are excluded for the complete read-only
    // scheduler snapshot, so execution and borrowed-context custody cannot
    // change while their paired accounting ledgers are copied.
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref().current_scheduling_context_runtime_snapshot()
    })
}

pub fn is_user_task_alive(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().is_user_task_alive(task_id) })
}

pub fn activate_suspended_user_task(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().activate_suspended_user_task(task_id)
    })
}

pub fn activate_suspended_user_tasks(task_ids: &[u64]) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().activate_suspended_user_tasks(task_ids)
    })
}

/// Runs one bounded authority commit between complete scheduler preflight and
/// runnable publication while retaining the global scheduler owner.
///
/// The callback executes under the caller's outer capability-registry guard
/// and the scheduler raw lock. It must only perform infallible, allocation-free
/// authority consumption and must never log, block, or acquire another lock.
/// The scheduler asserts that every target remains suspended after the callback
/// and before it publishes the complete runnable cohort.
pub fn activate_suspended_user_tasks_with_commit<F>(task_ids: &[u64], commit_authority: F) -> bool
where
    F: FnOnce(),
{
    // SAFETY: interrupt exclusion prevents same-CPU reentry and
    // `scheduler_mut` retains the global scheduler owner across the bounded
    // authority callback and runnable publication.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().activate_suspended_user_tasks_with_commit(task_ids, commit_authority)
    })
}

pub fn terminate_user_task(task_id: u64) -> bool {
    let terminated = interrupts::without_interrupts(|| unsafe {
        let requested_by_pid = scheduler_ref().current_user_id();
        scheduler_mut().terminate_user_task(task_id, requested_by_pid)
    });
    if terminated {
        complete_retirement_side_effects();
    }
    terminated
}

pub fn terminate_user_process(process_id: u64) -> bool {
    let terminated = interrupts::without_interrupts(|| unsafe {
        let requested_by_pid = scheduler_ref().current_user_id();
        scheduler_mut().terminate_user_process(process_id, requested_by_pid)
    });
    if terminated {
        complete_retirement_side_effects();
    }
    terminated
}

pub fn wake_user_task(task_id: u64) -> bool {
    if nucleus_core::util::lockdep::preemption_disabled() {
        return super::deferred_wake::defer_current_cpu(task_id);
    }
    interrupts::without_interrupts(|| unsafe { scheduler_mut().wake_user_task(task_id) })
}

/// Arms a race-free block on the current task; must be paired with
/// `commit_block_current_task`. Returns false if the slot is invalid or this is
/// the root task.
pub fn arm_block_current_task() -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().arm_block_current_task() })
}

/// Arms the current task for one exact endpoint receive epoch. Only this typed
/// wait may be consumed by the same-CPU direct IPC handoff path.
pub fn arm_block_current_task_on_endpoint(endpoint: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().arm_block_current_task_on_endpoint(endpoint)
    })
}

/// Arms the caller for one exact synchronous reply epoch.  The typed reason is
/// consumed by the fast rendezvous commit; ordinary wake/block code preserves
/// its existing race semantics.
pub fn arm_block_current_task_on_reply(reply: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().arm_block_current_task_on_reply(reply)
    })
}

/// Cancels a previously armed block without marking the current task blocked.
pub fn cancel_block_current_task() -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().cancel_block_current_task() })
}

pub fn wake_task(task_id: u64) -> bool {
    if nucleus_core::util::lockdep::preemption_disabled() {
        return super::deferred_wake::defer_current_cpu(task_id);
    }
    interrupts::without_interrupts(|| unsafe { scheduler_mut().wake_task(task_id) })
}

/// Permanently removes the current user task's base System-class admission and
/// caps its permanent fair weight without ever increasing a lower weight. A
/// reply-scoped IPC priority donation, if any, remains owned by that reply
/// capability and therefore remains effective until the normal release path.
pub fn demote_current_user_task_to_user_class() -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().demote_current_user_task_to_user_class()
    })
}

/// Reports whether `task_id` currently carries the kernel's effective System
/// class, including a live reply-scoped donation. This value is sampled before
/// endpoint enqueue; the reply capability remains the authoritative donation
/// lifetime after publication.
pub fn task_has_system_scheduling_class(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref().task_has_system_scheduling_class(task_id)
    })
}

/// Associates a live synchronous IPC reply with a caller-to-server priority
/// donation. The reply/cancellation paths revoke it before waking the caller.
pub fn inherit_ipc_priority(reply: u64, donor_task_id: u64, receiver_task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().inherit_ipc_priority(reply, donor_task_id, receiver_task_id)
    })
}

/// One scheduler acquisition for the class query and the reservation an IPC
/// call needs before it can enqueue. See [`scheduler::IpcCallAdmission`].
pub fn reserve_ipc_call_donation(donor_task_id: u64) -> super::scheduler::IpcCallAdmission {
    // SAFETY: interrupt exclusion and the scheduler access guard serialize the
    // exact class query and donor reservation; no borrow escapes.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().reserve_ipc_call_donation(donor_task_id)
    })
}

pub fn reserve_ipc_priority(donor_task_id: u64) -> bool {
    // SAFETY: interrupt exclusion and the scheduler access guard serialize the
    // exact donor reservation; no scheduler-owned reference escapes.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().reserve_ipc_priority(donor_task_id)
    })
}

pub fn cancel_ipc_priority_reservation(donor_task_id: u64) -> bool {
    // SAFETY: interrupt exclusion and the scheduler access guard serialize the
    // exact donor reservation cancellation; no borrow escapes.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().cancel_ipc_priority_reservation(donor_task_id)
    })
}

pub fn attach_reserved_ipc_priority(reply: u64, donor_task_id: u64) -> bool {
    // SAFETY: interrupt exclusion and the scheduler access guard transfer one
    // temporary donor reservation to one immutable reply identity.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().attach_reserved_ipc_priority(reply, donor_task_id)
    })
}

pub fn bind_reserved_ipc_priority(reply: u64, donor_task_id: u64, receiver_task_id: u64) -> bool {
    // SAFETY: interrupt exclusion and the scheduler access guard make binding
    // the reserved donor to one reply/receiver an atomic scheduler mutation.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().bind_reserved_ipc_priority(reply, donor_task_id, receiver_task_id)
    })
}

/// Selects and binds one exact worker for a process-owned endpoint in the same
/// scheduler transaction that reserves its reply-scoped donation.
pub fn bind_ipc_priority_to_process_worker(
    reply: u64,
    donor_task_id: u64,
    receiver_process_id: u64,
) -> Option<u64> {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().bind_ipc_priority_to_process_worker(
            reply,
            donor_task_id,
            receiver_process_id,
        )
    })
}

/// Revokes the bounded priority donation owned by a completed or cancelled IPC
/// reply capability. It is safe to call more than once for terminal races.
pub fn release_ipc_priority(reply: u64) -> bool {
    interrupts::without_interrupts(|| super::scheduler::release_reply_donation(reply))
}

/// Returns the exact caller scheduling-context custody carried by one terminal
/// reply. The IPC runtime guarantees one-shot extraction; PS revalidates the
/// live slot/generation before releasing any reply-scoped donation state.
pub fn settle_ipc_reply_scheduling_context(
    reply: u64,
    custody: kernel_ipc_runtime::api::ReplySchedulingContextCustody,
) -> bool {
    let identity = custody.identity();
    let valid = interrupts::without_interrupts(|| unsafe {
        scheduler_ref().scheduling_context_matches(custody.context_owner_task_id(), identity)
    });
    let _ = interrupts::without_interrupts(|| super::scheduler::release_reply_donation(reply));
    valid
}

pub fn complete_ipc_reply_wake_handoff_with_custody(
    reply: u64,
    completion: kernel_ipc_runtime::api::ReplyCompletion,
) -> bool {
    let custody = completion
        .scheduling_context
        .expect("synchronous IPC reply completed without scheduling-context custody");
    assert_eq!(
        custody.caller_task_id(),
        completion.caller_task_id,
        "reply returned scheduling-context custody to a different caller"
    );
    assert!(
        settle_ipc_reply_scheduling_context(reply, custody),
        "reply returned stale scheduling-context custody"
    );
    complete_ipc_reply_wake_handoff(reply, completion.caller_task_id)
}

pub fn complete_fast_ipc_reply_wake_handoff_with_custody(
    reply: u64,
    completion: kernel_ipc_runtime::api::ReplyCompletion,
) -> bool {
    let custody = completion
        .scheduling_context
        .expect("fast synchronous IPC reply completed without scheduling-context custody");
    assert_eq!(
        custody.caller_task_id(),
        completion.caller_task_id,
        "fast reply returned scheduling-context custody to a different caller"
    );
    let outcome = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().settle_and_complete_fast_ipc_reply_handoff(
            reply,
            completion.caller_task_id,
            custody.context_owner_task_id(),
            custody.identity(),
        )
    })
    .expect("fast reply returned stale scheduling-context custody");
    match outcome {
        super::scheduler::FastIpcReplyHandoffOutcome::Direct
        | super::scheduler::FastIpcReplyHandoffOutcome::LocalFallback => true,
        super::scheduler::FastIpcReplyHandoffOutcome::Rejected => {
            complete_ipc_reply_wake_handoff(reply, completion.caller_task_id)
        }
    }
}

/// Completes the scheduling side of a terminal reply with one Scheduler
/// acquisition, then publishes only its opaque exact wake token to the
/// target CPU's handoff owner.  A stale token deliberately loses urgency; it
/// never falls back to the catalog hint path and cannot create execution
/// authority.
pub fn complete_ipc_reply_wake_handoff(reply: u64, task_id: u64) -> bool {
    let _ = interrupts::without_interrupts(|| super::scheduler::release_reply_donation(reply));
    let token = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().complete_ipc_reply_wake_handoff(reply, task_id)
    });
    interrupts::without_interrupts(|| {
        token.is_some_and(super::scheduler::enqueue_reply_wake_handoff)
    })
}

pub fn release_ipc_priorities_for_process(process_id: u64) {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().release_ipc_priorities_for_process(process_id)
    });
}

/// Biases the next scheduler pick toward `task_id`. Combine with `wake_task` +
/// `yield_now` to implement direct hand-off (caller donates remaining quantum
/// to the receiver), eliminating round-robin latency on IPC roundtrips.
pub fn set_next_pick_hint(task_id: u64) {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().set_next_pick_hint(task_id) })
}

/// Retains one exact synchronous call/reply handoff until the receiver/caller
/// receives its bounded direct turn or retires.
/// Commits one synchronous-IPC call handoff under a single scheduler
/// acquisition.
///
/// `wake_task` defers to the per-CPU wake queue when preemption is disabled,
/// and that fallback exists because the caller may hold a raw lock. This entry
/// point keeps it: with preemption disabled it performs the same three
/// operations through their individual paths rather than taking the scheduler
/// here, so the fused path never changes who may call it.
pub fn commit_ipc_call_handoff(
    reply: u64,
    donor_task_id: u64,
    receiver_task_id: u64,
    donation_required: bool,
) -> super::scheduler::IpcCallHandoffOutcome {
    if nucleus_core::util::lockdep::preemption_disabled() {
        let inherited = if donation_required {
            bind_reserved_ipc_priority(reply, donor_task_id, receiver_task_id)
        } else {
            true
        };
        let woke = wake_task(receiver_task_id);
        let hinted = set_next_synchronous_pick_hint(receiver_task_id);
        return super::scheduler::IpcCallHandoffOutcome {
            inherited,
            woke,
            hinted,
        };
    }
    // SAFETY: interrupt exclusion plus the scheduler access guard make the
    // bind/wake/hint sequence one atomic scheduler mutation, exactly as each
    // step was individually.
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().commit_ipc_call_handoff(
            reply,
            donor_task_id,
            receiver_task_id,
            donation_required,
        )
    })
}

pub fn set_next_synchronous_pick_hint(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().set_next_synchronous_pick_hint(task_id)
    })
}

pub fn set_next_latency_pick_hint(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().set_next_latency_pick_hint(task_id)
    })
}

pub fn set_next_process_pick_hint(process_id: u64) -> Option<u64> {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().set_next_process_pick_hint(process_id)
    })
}

pub fn set_next_spawn_pick_hint(task_id: u64) {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().set_next_spawn_pick_hint(task_id) })
}

pub fn current_console_session() -> Option<ConsoleSessionHandle> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_console_session() })
}

pub fn exec_current_user_process(
    address_space: ProcessAddressSpace,
    mut bootstrap: super::UserTaskBootstrap,
) -> Option<alloc::vec::Vec<crate::user::handles::KernelHandle>> {
    const EXEC_REMOTE_QUIESCE_TIMEOUT_NS: u64 = 2_000_000_000;
    let linux_process_state = bootstrap.linux_process_state.take()?;
    let linux_memory_map = bootstrap.linux_memory_map.take()?;
    let linux_runtime_profile = bootstrap.linux_runtime_profile.take()?;
    let exec_path = alloc::string::String::from(bootstrap.exec_path());
    let new_root = address_space.root_phys().as_u64();

    // SAFETY: interrupt exclusion prevents same-CPU scheduler reentry; the
    // scheduler access guard serializes every remote CPU.
    let process_handle =
        interrupts::without_interrupts(|| unsafe { scheduler_ref().current_process_handle() })?;
    let exec_reservation = super::process_table::begin_exec(process_handle)?;
    let start = crate::arch::clock::monotonic_nanos();
    loop {
        // SAFETY: one bounded barrier step holds the same scheduler exclusion.
        let ready = interrupts::without_interrupts(|| unsafe {
            scheduler_mut().quiesce_current_exec_siblings()
        });
        match ready {
            Some(true) => break,
            Some(false) => {}
            None => {
                let _ = super::process_table::cancel_exec(exec_reservation);
                return None;
            }
        }
        if crate::arch::clock::monotonic_nanos().saturating_sub(start)
            >= EXEC_REMOTE_QUIESCE_TIMEOUT_NS
        {
            panic!(
                "scheduler invariant: exec timed out quiescing remote process threads handle={process_handle:?}"
            );
        }
        super::cond_resched();
        core::hint::spin_loop();
    }

    complete_retirement_side_effects();
    if !super::process_table::authorize_exec(exec_reservation) {
        let _ = super::process_table::cancel_exec(exec_reservation);
        return None;
    }

    let staged = super::process_table::stage_exec_state(
        exec_reservation,
        address_space,
        linux_process_state,
        linux_memory_map,
        linux_runtime_profile,
        exec_path.as_str(),
    );
    let Some(staged) = staged else {
        let _ = super::process_table::cancel_exec(exec_reservation);
        return None;
    };
    // SAFETY: ProcessStateLock is released before IRQ exclusion and the raw
    // scheduler owner; ordinary state readers retry until finalization.
    let published = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().exec_current_user_process(new_root, &mut bootstrap)
    })
    .expect("authorized self exec lost its reserved scheduler target");
    let finalized = super::process_table::finalize_exec_state(staged, published);
    let (process_id, exit_pending, closed, old_state) = finalized.into_parts();
    // Scheduler publication removed the last old-root execution owner.
    drop(old_state);
    if exit_pending {
        assert!(
            terminate_user_process(process_id),
            "exec-finalized pending exit lost scheduler ownership"
        );
        super::request_user_return_reschedule();
    }
    complete_retirement_side_effects();
    Some(closed)
}

pub fn exec_user_process_by_pid(
    process_id: u64,
    thread_id: u64,
    address_space: ProcessAddressSpace,
    mut bootstrap: super::UserTaskBootstrap,
) -> Option<alloc::vec::Vec<crate::user::handles::KernelHandle>> {
    const EXEC_REMOTE_QUIESCE_TIMEOUT_NS: u64 = 2_000_000_000;
    let linux_process_state = bootstrap.linux_process_state.take()?;
    let linux_memory_map = bootstrap.linux_memory_map.take()?;
    let linux_runtime_profile = bootstrap.linux_runtime_profile.take()?;
    let exec_path = alloc::string::String::from(bootstrap.exec_path());
    let new_root = address_space.root_phys().as_u64();

    // SAFETY: interrupt exclusion prevents same-CPU scheduler reentry; the
    // scheduler access guard serializes every remote CPU.
    let process_handle = interrupts::without_interrupts(|| unsafe {
        scheduler_ref().process_handle_for_thread(process_id, thread_id)
    })?;
    let exec_reservation = super::process_table::begin_exec(process_handle)?;
    let start = crate::arch::clock::monotonic_nanos();
    loop {
        // SAFETY: one bounded barrier step holds the same scheduler exclusion.
        let ready = interrupts::without_interrupts(|| unsafe {
            scheduler_mut().quiesce_exec_target_and_siblings(process_id, thread_id, process_handle)
        });
        match ready {
            Some(true) => break,
            Some(false) => {}
            None => {
                let _ = super::process_table::cancel_exec(exec_reservation);
                // SAFETY: exact target state is restored under scheduler
                // serialization before returning failure.
                interrupts::without_interrupts(|| unsafe {
                    scheduler_mut().cancel_exec_target_quiesce(
                        process_id,
                        thread_id,
                        process_handle,
                    );
                });
                return None;
            }
        }
        if crate::arch::clock::monotonic_nanos().saturating_sub(start)
            >= EXEC_REMOTE_QUIESCE_TIMEOUT_NS
        {
            panic!(
                "scheduler invariant: target exec timed out quiescing remote threads process={process_id} thread={thread_id}"
            );
        }
        super::cond_resched();
        core::hint::spin_loop();
    }

    complete_retirement_side_effects();
    if !super::process_table::authorize_exec(exec_reservation) {
        let _ = super::process_table::cancel_exec(exec_reservation);
        // SAFETY: IRQ exclusion and the global scheduler guard serialize the
        // exact target-quiesce rollback; no process-state lock is acquired.
        interrupts::without_interrupts(|| unsafe {
            scheduler_mut().cancel_exec_target_quiesce(process_id, thread_id, process_handle);
        });
        return None;
    }

    let staged = super::process_table::stage_exec_state(
        exec_reservation,
        address_space,
        linux_process_state,
        linux_memory_map,
        linux_runtime_profile,
        exec_path.as_str(),
    );
    let Some(staged) = staged else {
        let _ = super::process_table::cancel_exec(exec_reservation);
        // SAFETY: no ProcessState lock is held; IRQ exclusion and the global
        // scheduler owner serialize exact target-quiesce rollback.
        interrupts::without_interrupts(|| unsafe {
            scheduler_mut().cancel_exec_target_quiesce(process_id, thread_id, process_handle);
        });
        return None;
    };
    // SAFETY: staging released ProcessStateLock; IRQ exclusion and the raw
    // scheduler owner serialize exact reserved-target publication.
    let published = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().exec_user_process_by_pid(process_id, thread_id, new_root, &mut bootstrap)
    })
    .expect("authorized target exec lost its reserved scheduler slot");
    let finalized = super::process_table::finalize_exec_state(staged, published);
    let (finalized_pid, exit_pending, closed, old_state) = finalized.into_parts();
    drop(old_state);
    if exit_pending {
        assert_eq!(finalized_pid, process_id);
        assert!(
            terminate_user_process(process_id),
            "target exec-finalized pending exit lost scheduler ownership"
        );
    }
    complete_retirement_side_effects();
    Some(closed)
}

pub fn linux_thread_snapshot_by_ids(
    process_id: u64,
    thread_id: u64,
) -> Option<super::LinuxThreadSnapshot> {
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref().linux_thread_snapshot_by_ids(process_id, thread_id)
    })
}

pub fn with_current_user_linux_state_mut<R>(
    f: impl FnOnce(
        u64,
        u64,
        UserAbi,
        &mut ProcessAddressSpace,
        &mut Option<LinuxProcessState>,
        &mut Option<LinuxThreadState>,
    ) -> R,
) -> Option<R> {
    let (process_id, process, binding) = retain_current_linux_thread_binding()?;
    process.with_visible_state_mut(|_, state| {
        let (address_space, linux_process_state) =
            state.address_space_and_linux_process_state_mut();
        binding.with_thread_state_mut(|linux_thread_state| {
            f(
                process_id,
                binding.tid,
                binding.abi,
                address_space,
                linux_process_state,
                linux_thread_state,
            )
        })
    })?
}

/// Updates the dispatch-only FS-base cache after the generation-bound Linux
/// thread state has accepted a new architectural TLS base.
pub fn set_current_linux_tls_fs_base(value: u64) {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().set_current_tls_fs_base(value);
    });
}

pub fn with_current_user_process_state_mut<R>(
    f: impl FnOnce(u64, UserAbi, &mut UserProcessState) -> R,
) -> Option<R> {
    let (thread_id, abi, process) = retain_current_user_process_binding()?;
    process.with_visible_state_mut(|_, process_state| f(thread_id, abi, process_state))
}

pub fn with_current_user_process_state<R>(
    f: impl FnOnce(u64, UserAbi, &UserProcessState) -> R,
) -> Option<R> {
    let (thread_id, abi, process) = retain_current_user_process_binding()?;
    process.with_visible_state(|_, process_state| f(thread_id, abi, process_state))
}

pub fn with_process_state_by_pid_mut<R>(
    process_id: u64,
    f: impl FnOnce(&mut UserProcessState) -> R,
) -> Option<R> {
    process_table::with_process_state_by_pid_mut(process_id, f)
}

pub fn note_process_exit_status(process_id: u64, status: i32) -> Option<()> {
    process_table::note_process_exit_status(process_id, status)
}

/// Prevent new process-owned authority from being published while this process
/// is draining its final resource teardown.
pub fn mark_user_process_exiting(process_id: u64) -> bool {
    process_table::mark_process_exiting(process_id).is_some()
}

pub fn mark_user_process_exiting_once(process_id: u64) -> Option<bool> {
    process_table::mark_process_exiting_once(process_id)
}

/// Unknown process IDs fail closed: callers must never publish authority for a
/// process that is absent from the process table.
pub fn is_user_process_exiting(process_id: u64) -> bool {
    process_table::is_process_exiting(process_id).unwrap_or(true)
}

pub fn parent_process_id_of(process_id: u64) -> Option<u64> {
    process_table::parent_process_id_of(process_id)
}

pub fn wait_for_child(
    parent_process_id: u64,
    target_pid: i64,
    include_stopped: bool,
    include_continued: bool,
) -> WaitChildResult {
    match process_table::wait_for_child(
        parent_process_id,
        target_pid,
        include_stopped,
        include_continued,
    ) {
        process_table::WaitResult::Exited { pid, status } => {
            WaitChildResult::Exited { pid, status }
        }
        process_table::WaitResult::StateChanged { pid, status } => {
            WaitChildResult::StateChanged { pid, status }
        }
        process_table::WaitResult::Pending => WaitChildResult::Pending,
        process_table::WaitResult::NoMatchingChild => WaitChildResult::NoMatchingChild,
    }
}

/// Runs `f` against the current task's address space.
///
/// This is the hottest user-copy step in the kernel, and it used to retain the
/// process, test visibility, and release the retain -- three acquisitions of
/// the global process table to reach a pointer the running task already owned.
/// A live thread pins its own process object, so the published per-slot state
/// pointer is enough; the visibility test still runs, still under the process
/// state lock, and now reads one atomic instead of taking the table.
pub fn with_current_mm<R>(f: impl FnOnce(&ProcessAddressSpace) -> R) -> Option<R> {
    let entry = user_copy_profile::now();
    let (_, _, process_handle, _) = interrupts::without_interrupts(|| {
        if let Some(identity) = published_current_identity() {
            return identity.user_binding();
        }
        // SAFETY: interrupts are masked, so the current slot is stable.
        unsafe { scheduler_ref().current_user_process_binding() }
    })?;
    let identified =
        user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindIdentity, entry);
    let result = process_table::with_own_visible_state(process_handle, |state| {
        // Charged inside the closure so the phase ends once the per-process
        // state lock is held and visibility has been re-tested, not when the
        // caller's own work finishes.
        user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindVisible, identified);
        f(state.address_space())
    });
    result
}

pub fn with_current_process_credentials<R>(
    f: impl FnOnce(ProcessSecurityContext) -> R,
) -> Option<R> {
    with_current_user_process_state(|_, _, process_state| f(process_state.security()))
}

pub fn retain_current_user_process_state() -> Option<RetainedCurrentUserProcessState> {
    let (_, abi, process) = retain_current_user_process_binding()?;
    Some(RetainedCurrentUserProcessState {
        process_id: process.process_id(),
        abi,
        process,
    })
}

pub fn with_current_process_state_mut<R>(
    f: impl FnOnce(u64, &mut UserProcessState) -> R,
) -> Option<R> {
    let process = retain_current_process_ref()?;
    process.with_visible_state_mut(f)
}

pub fn with_current_process_state<R>(f: impl FnOnce(u64, &UserProcessState) -> R) -> Option<R> {
    let process = retain_current_process_ref()?;
    process.with_visible_state(f)
}

pub fn with_process_state_by_pid<R>(
    process_id: u64,
    f: impl FnOnce(&UserProcessState) -> R,
) -> Option<R> {
    process_table::with_process_state_by_pid(process_id, f)
}

pub fn with_current_user_process_and_linux_thread_state_mut<R>(
    f: impl FnOnce(u64, u64, UserAbi, &mut UserProcessState, &mut Option<LinuxThreadState>) -> R,
) -> Option<R> {
    let (process_id, process, binding) = retain_current_linux_thread_binding()?;
    process.with_visible_state_mut(|_, state| {
        binding.with_thread_state_mut(|linux_thread_state| {
            f(
                process_id,
                binding.tid,
                binding.abi,
                state,
                linux_thread_state,
            )
        })
    })?
}

pub fn queue_linux_signal(process_id: u64, task_id: u64, signal: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().queue_linux_signal(process_id, task_id, signal)
    })
}

pub fn queue_linux_process_sigchld(process_id: u64, events: u32) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().queue_linux_process_sigchld(process_id, events)
    })
}

pub fn stop_current_linux_process(signal: u64) -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().stop_current_linux_process(signal) })
}

pub fn any_user_process_state(mut f: impl FnMut(u64, &UserProcessState) -> bool) -> bool {
    let (handles, len) = interrupts::without_interrupts(|| unsafe {
        scheduler_ref().user_process_handles_snapshot()
    });
    for handle in handles.into_iter().take(len).flatten() {
        let Some(process) = process_table::retain_process(handle) else {
            continue;
        };
        if process
            .with_visible_state(|process_id, process_state| f(process_id, process_state))
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn retain_current_user_process_binding() -> Option<(u64, UserAbi, process_table::ProcessRef)> {
    let entry = user_copy_profile::now();
    let (thread_id, abi, process_handle, _) = interrupts::without_interrupts(|| {
        if let Some(identity) = published_current_identity() {
            return identity.user_binding();
        }
        // SAFETY: interrupts are masked, so the current slot is stable.
        unsafe { scheduler_ref().current_user_process_binding() }
    })?;
    let identified =
        user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindIdentity, entry);
    let process = process_table::retain_process(process_handle)?;
    user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindRetain, identified);
    Some((thread_id, abi, process))
}

type RetainedLinuxThreadBinding = (
    u64,
    process_table::ProcessRef,
    super::scheduler::CurrentLinuxThreadBinding,
);

fn retain_current_linux_thread_binding() -> Option<RetainedLinuxThreadBinding> {
    let binding = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().current_linux_thread_binding()
    })?;
    let process = process_table::retain_process(binding.process_handle)?;
    Some((process.process_id(), process, binding))
}

fn retain_current_process_ref() -> Option<process_table::ProcessRef> {
    let process_handle =
        interrupts::without_interrupts(|| unsafe { scheduler_ref().current_process_handle() })?;
    process_table::retain_process(process_handle)
}

pub fn retire_current_user_task_due_to_fault(
    vector: u8,
    error_code: Option<u64>,
    cr2: u64,
    rip: u64,
    rsp: u64,
) -> UserFaultDisposition {
    // Release stacks eagerly map every usable page above the permanent guard.
    // There is therefore no supported lazy-growth fault in the enabled
    // topology. The former dormant path used a nonblocking ProcessState lock
    // and collapsed transient contention into task retirement; retaining it
    // would silently reactivate that bug if stack reservation policy changed.
    // A future lazy profile must introduce an explicit deferred-fault owner
    // and generation-bound retry before this exception path may resume faults.
    let disposition = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().retire_current_user_task_due_to_fault(vector, error_code, cr2, rip, rsp)
    });
    complete_retirement_side_effects();
    disposition
}

pub fn halt_current_retired_task() -> ! {
    loop {
        interrupts::enable_and_hlt();
    }
}

pub(crate) fn exit_current_task() -> ! {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().exit_current_task();
    });
    complete_retirement_side_effects();
    halt_current_retired_task()
}

pub fn exit_current_user_task() -> ! {
    exit_current_task()
}

pub fn exit_current_user_process() -> ! {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().exit_current_process();
    });
    complete_retirement_side_effects();
    halt_current_retired_task()
}

pub fn service_deferred_work() -> usize {
    // SAFETY: one fixed cleanup token is detached while local interrupts and
    // every remote scheduler mutation are excluded; no borrow escapes.
    let retirement_side_effect =
        interrupts::without_interrupts(|| unsafe { scheduler_mut().take_retirement_side_effect() });
    let completed_side_effect = usize::from(retirement_side_effect.is_some());
    if let Some(retirement_side_effect) = retirement_side_effect {
        retirement_side_effect.complete(|task_id| {
            let _ = wake_task(task_id);
        });
    }
    let retired_slot =
        interrupts::without_interrupts(|| unsafe { scheduler_mut().reap_inactive_retired_slots() });
    let reaped_slot = usize::from(retired_slot.is_some());
    if let Some(retired_slot) = retired_slot {
        retired_slot.complete();
    }
    completed_side_effect + reaped_slot + process_table::reap_exited_processes()
}

fn complete_retirement_side_effects() {
    loop {
        // SAFETY: token detachment is one bounded scheduler mutation and the
        // owned token is completed only after this guard has been dropped.
        let side_effect = interrupts::without_interrupts(|| unsafe {
            scheduler_mut().take_retirement_side_effect()
        });
        let Some(side_effect) = side_effect else {
            return;
        };
        side_effect.complete(|task_id| {
            let _ = wake_task(task_id);
        });
    }
}

pub fn next_retired_task_cleanup() -> Option<super::RetiredTaskCleanup> {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().next_retired_task_cleanup() })
}

pub fn complete_retired_task_cleanup(cleanup: super::RetiredTaskCleanup) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().complete_retired_task_cleanup(cleanup)
    })
}

#[cfg(test)]
mod log_identity_tests {
    use core::cell::Cell;

    use super::published_or_scheduler_user_log_ids;

    #[test]
    fn complete_published_log_identity_skips_scheduler_fallback() {
        let fallback_calls = Cell::new(0);
        assert_eq!(
            published_or_scheduler_user_log_ids(Some(Some((23, 41))), || {
                fallback_calls.set(fallback_calls.get() + 1);
                Some((1, 2))
            }),
            Some((23, 41))
        );
        assert_eq!(fallback_calls.get(), 0);

        assert_eq!(
            published_or_scheduler_user_log_ids(Some(None), || {
                fallback_calls.set(fallback_calls.get() + 1);
                Some((1, 2))
            }),
            None
        );
        assert_eq!(fallback_calls.get(), 0);
    }

    #[test]
    fn absent_or_incomplete_log_identity_uses_scheduler_fallback() {
        let fallback_calls = Cell::new(0);
        assert_eq!(
            published_or_scheduler_user_log_ids(None, || {
                fallback_calls.set(fallback_calls.get() + 1);
                Some((29, 43))
            }),
            Some((29, 43))
        );
        assert_eq!(fallback_calls.get(), 1);
    }
}
