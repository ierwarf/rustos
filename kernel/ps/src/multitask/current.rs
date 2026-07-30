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
use x86_64::instructions::interrupts;

use super::{
    CurrentUserSnapshot, RetainedCurrentUserAddressSpace, RetainedCurrentUserProcessState,
    UserFaultDisposition, WaitChildResult, current_cpu_task_slot_admitted, process_table,
    scheduler_mut, scheduler_ref,
};
use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging::ProcessAddressSpace;
use crate::user::abi::UserAbi;
use crate::user::linux::{LinuxProcessState, LinuxThreadState};
use crate::user::process_state::{ProcessSecurityContext, UserProcessState};

pub fn current_user_address_space() -> Option<RetainedCurrentUserAddressSpace> {
    let (_, abi, process) = retain_current_user_process_binding()?;
    Some(RetainedCurrentUserAddressSpace {
        abi,
        process_id: process.process_id(),
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
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_task_id() })
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
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_user_log_ids() })
}

pub fn user_log_ids_for_task(task_id: u64) -> Option<(u64, u64)> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().user_log_ids_for_task(task_id) })
}

pub fn current_user_process_id() -> Option<u64> {
    current_user_log_ids().map(|(process_id, _)| process_id)
}

pub fn current_user_process_thread_count() -> Option<usize> {
    let process_id = current_user_process_id()?;
    process_table::thread_count_by_pid(process_id)
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
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref()
            .current_user_process_binding()
            .map(|(_, abi, _, _)| abi)
    })
}

/// Return the current task, ABI, and address-space identity without acquiring
/// the process-state lock. Scheduler-owned wait paths use this snapshot before
/// they have installed a waiter or deadline and must not inherit unrelated
/// same-process lock latency.
pub fn current_user_wait_binding() -> Option<(u64, UserAbi, u64)> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_user_wait_binding() })
}

pub fn current_user_snapshot() -> Option<CurrentUserSnapshot> {
    let (thread_id, abi, process_handle, console_session) =
        interrupts::without_interrupts(|| unsafe {
            scheduler_ref().current_user_process_binding()
        })?;
    process_table::with_process_state(process_handle, |process_id, process_state| {
        CurrentUserSnapshot::new(
            abi,
            thread_id,
            process_id,
            console_session,
            process_state.security(),
        )
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

/// Permanently removes the current user task's base System-class admission.
/// A reply-scoped IPC priority donation, if any, remains owned by that reply
/// capability and therefore remains effective until the normal release path.
pub fn demote_current_user_task_to_user_class() -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().demote_current_user_task_to_user_class()
    })
}

/// Associates a live synchronous IPC reply with a caller-to-server priority
/// donation. The reply/cancellation paths revoke it before waking the caller.
pub fn inherit_ipc_priority(reply: u64, donor_task_id: u64, receiver_task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().inherit_ipc_priority(reply, donor_task_id, receiver_task_id)
    })
}

/// Starts the same reply-scoped donation for a process-owned endpoint before a
/// concrete receiver worker has entered `IPC_RECV`.
pub fn inherit_ipc_priority_for_process(
    reply: u64,
    donor_task_id: u64,
    receiver_process_id: u64,
) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().inherit_ipc_priority_for_process(reply, donor_task_id, receiver_process_id)
    })
}

/// Revokes the bounded priority donation owned by a completed or cancelled IPC
/// reply capability. It is safe to call more than once for terminal races.
pub fn release_ipc_priority(reply: u64) -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().release_ipc_priority(reply) })
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
    bootstrap: super::UserTaskBootstrap,
) -> Option<alloc::vec::Vec<crate::user::handles::KernelHandle>> {
    const EXEC_REMOTE_QUIESCE_TIMEOUT_NS: u64 = 2_000_000_000;

    // SAFETY: interrupt exclusion prevents same-CPU scheduler reentry; the
    // scheduler access guard serializes every remote CPU.
    let process_handle =
        interrupts::without_interrupts(|| unsafe { scheduler_ref().current_process_handle() })?;
    super::process_table::begin_exec(process_handle)?;
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
                let _ = super::process_table::cancel_exec(process_handle);
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

    // SAFETY: replacement is one scheduler-serialized state transition.
    let result = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().exec_current_user_process(address_space, bootstrap)
    });
    complete_retirement_side_effects();
    if result.is_none() {
        let _ = super::process_table::cancel_exec(process_handle);
    }
    result
}

pub fn exec_user_process_by_pid(
    process_id: u64,
    thread_id: u64,
    address_space: ProcessAddressSpace,
    bootstrap: super::UserTaskBootstrap,
) -> Option<alloc::vec::Vec<crate::user::handles::KernelHandle>> {
    const EXEC_REMOTE_QUIESCE_TIMEOUT_NS: u64 = 2_000_000_000;

    // SAFETY: interrupt exclusion prevents same-CPU scheduler reentry; the
    // scheduler access guard serializes every remote CPU.
    let process_handle = interrupts::without_interrupts(|| unsafe {
        scheduler_ref().process_handle_for_thread(process_id, thread_id)
    })?;
    super::process_table::begin_exec(process_handle)?;
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
                let _ = super::process_table::cancel_exec(process_handle);
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

    // SAFETY: replacement is one scheduler-serialized state transition.
    let result = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().exec_user_process_by_pid(process_id, thread_id, address_space, bootstrap)
    });
    complete_retirement_side_effects();
    if result.is_none() {
        let _ = super::process_table::cancel_exec(process_handle);
        // SAFETY: exact target quiesce state is cleared under the same guard.
        interrupts::without_interrupts(|| unsafe {
            scheduler_mut().cancel_exec_target_quiesce(process_id, thread_id, process_handle);
        });
    }
    result
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
    let (process_id, tid, abi, process, mut linux_thread_state) =
        retain_current_linux_thread_binding()?;
    let linux_thread_state = unsafe { linux_thread_state.as_mut() };
    Some(process.with_state_mut(|_, state| {
        let (address_space, linux_process_state) =
            state.address_space_and_linux_process_state_mut();
        f(
            process_id,
            tid,
            abi,
            address_space,
            linux_process_state,
            linux_thread_state,
        )
    }))
}

pub fn with_current_user_process_state_mut<R>(
    f: impl FnOnce(u64, UserAbi, &mut UserProcessState) -> R,
) -> Option<R> {
    let (thread_id, abi, process) = retain_current_user_process_binding()?;
    Some(process.with_state_mut(|_, process_state| f(thread_id, abi, process_state)))
}

pub fn with_current_user_process_state<R>(
    f: impl FnOnce(u64, UserAbi, &UserProcessState) -> R,
) -> Option<R> {
    let (thread_id, abi, process) = retain_current_user_process_binding()?;
    Some(process.with_state(|_, process_state| f(thread_id, abi, process_state)))
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

pub fn with_current_mm<R>(f: impl FnOnce(&ProcessAddressSpace) -> R) -> Option<R> {
    let (_, _, process) = retain_current_user_process_binding()?;
    Some(process.with_state(|_, state| f(state.address_space())))
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
    Some(process.with_state_mut(f))
}

pub fn with_current_process_state<R>(f: impl FnOnce(u64, &UserProcessState) -> R) -> Option<R> {
    let process = retain_current_process_ref()?;
    Some(process.with_state(f))
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
    let (process_id, tid, abi, process, mut linux_thread_state) =
        retain_current_linux_thread_binding()?;
    let linux_thread_state = unsafe { linux_thread_state.as_mut() };
    Some(process.with_state_mut(|_, state| f(process_id, tid, abi, state, linux_thread_state)))
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
        if process.with_state(|process_id, process_state| f(process_id, process_state)) {
            return true;
        }
    }
    false
}

fn retain_current_user_process_binding() -> Option<(u64, UserAbi, process_table::ProcessRef)> {
    let (thread_id, abi, process_handle, _) = interrupts::without_interrupts(|| unsafe {
        scheduler_ref().current_user_process_binding()
    })?;
    let process = process_table::retain_process(process_handle)?;
    Some((thread_id, abi, process))
}

type RetainedLinuxThreadBinding = (
    u64,
    u64,
    UserAbi,
    process_table::ProcessRef,
    core::ptr::NonNull<Option<LinuxThreadState>>,
);

fn retain_current_linux_thread_binding() -> Option<RetainedLinuxThreadBinding> {
    let binding = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().current_linux_thread_binding()
    })?;
    let process = process_table::retain_process(binding.process_handle)?;
    Some((
        process.process_id(),
        binding.tid,
        binding.abi,
        process,
        binding.linux_thread_state,
    ))
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
    interrupts::without_interrupts(|| unsafe { scheduler_ref().next_retired_task_cleanup() })
}

pub fn complete_retired_task_cleanup(cleanup: super::RetiredTaskCleanup) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().complete_retired_task_cleanup(cleanup)
    })
}
