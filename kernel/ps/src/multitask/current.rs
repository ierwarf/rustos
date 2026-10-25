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

pub(super) mod group_exit;
mod owner_bindings;
use owner_bindings::{
    current_process_id_for_handle, owned_current_process_handle, owned_current_user_binding,
};

mod ipc;
mod pager;

pub use ipc::{
    arm_block_current_task, arm_block_current_task_on_endpoint,
    arm_block_current_task_on_pager_fault, arm_block_current_task_on_pager_service,
    arm_block_current_task_on_reply, attach_reserved_ipc_priority,
    bind_reserved_ipc_priority, cancel_block_current_task,
    cancel_ipc_priority_reservation, complete_fast_ipc_reply_wake_handoff_with_custody,
    endpoint_receiver_still_waiting,
    complete_ipc_reply_wake_handoff, complete_ipc_reply_wake_handoff_with_custody,
    complete_pager_fault_wake_handoff, demote_current_user_task_to_user_class,
    inherit_ipc_priority, inherit_pager_fault_priority,
    release_ipc_priority, release_pager_fault_priority, reserve_ipc_call_donation,
    reserve_ipc_priority, settle_cancelled_ipc_reply_scheduling_context,
    settle_ipc_reply_scheduling_context, task_has_system_scheduling_class,
    wake_pager_service_waiter_for_process, wake_task,
};
pub use pager::{
    PagerChargeSnapshot, current_pager_charge_snapshot, current_pager_fault_install_permit,
    current_pager_vma_snapshot, pager_vma_regions_for_process, publish_current_pager_vma,
    publish_inherited_pager_vma_for_process, publish_pager_vma_for_process,
    revoke_current_pager_vma, revoke_pager_vma_for_process,
};

// Native witnesses model the earlier masked IRQ entry, not privileged CLI/STI.
#[cfg(test)]
pub(in crate::multitask) use pager::published_current_pager_binding as test_current_pager_binding;

use super::cpu_local;
use super::{
    CurrentUserSnapshot, RetainedCurrentUserAddressSpace, RetainedCurrentUserProcessState,
    UserFaultDisposition, WaitChildResult, current_cpu_task_slot_admitted, process_table,
};
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

/// Binds the current task's own address space for one syscall.
///
/// An exact atomic allocation pin replaces process-table counted retain/release;
/// those previously required two acquisitions of the
/// one global process table, and the per-class census measured that table as
/// the most-acquired lock class under a synchronous IPC round trip. Every
/// accessor on the returned value validates the exact process and MM
/// generation, which is what makes the uncounted pin safe across an exec.
pub fn current_user_address_space() -> Option<RetainedCurrentUserAddressSpace> {
    let entry = user_copy_profile::now();
    let (thread_id, abi, process_handle, _) = interrupts::without_interrupts(|| {
        if let Some(identity) = published_current_identity() {
            return identity.user_binding();
        }
        // SAFETY: interrupts are masked, so the current slot is stable.
        owned_current_user_binding()
    })?;
    let identified =
        user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindIdentity, entry);
    let process_id = current_process_id_for_handle(process_handle)?;
    let process = process_table::own_process_ref(process_handle, process_id)?;
    let identity = process.live_identity()?;
    user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindRetain, identified);
    Some(RetainedCurrentUserAddressSpace {
        abi,
        process_id,
        thread_id,
        identity,
        process,
    })
}

/// A pointer-free IPC syscall needs exact live identity, not an address-space
/// allocation loan. Join the installed Current user binding to the process
/// generation publication while this CPU cannot switch. No PID-only diagnostic
/// identity, process-state borrow, MM bind, allocation pin or user page walk.
/// The returned snapshot authorizes no subsequent user-memory access.
pub fn current_user_ipc_identity() -> Option<(u64, process_table::ProcessIdentity)> {
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return None;
    }
    interrupts::without_interrupts(|| {
        let (task_id, _, handle, _) = match published_current_identity() {
            Some(identity) => identity.user_binding(),
            None => owned_current_user_binding(),
        }?;
        ipc_identity_for_binding(task_id, handle)
    })
}

pub(super) fn ipc_identity_for_binding(
    task_id: u64, handle: process_table::ProcessHandle,
) -> Option<(u64, process_table::ProcessIdentity)> {
    let identity = process_table::live_process_identity(handle)?;
    Some((task_id, identity))
}

/// The current task's published process id, when it names the same process
/// handle the binding did.
fn published_current_identity_process_id(handle: process_table::ProcessHandle) -> Option<u64> {
    let identity = interrupts::without_interrupts(published_current_identity)?;
    (identity.process_handle == Some(handle))
        .then_some(identity.process_id)
        .flatten()
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
        super::scheduler::stack_resources::context::current_task_start().map(|start| start.id)
    })
}

/// The identity published for the task this CPU is running, if any.
///
/// Callers must already have interrupts masked. A `None` result means the
/// record was never published or a writer was mid-update. The caller needs an
/// independently owned fallback or refusal, never authority from absence.
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
    // Independent caller/target pins cover this non-yielding snapshot turn.
    interrupts::without_interrupts(|| {
        super::scheduler::thread_affinity::linux_mask(target_task_id, online_mask)
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
    // Caller/target custody survives masks and queue ACK without a directory loan.
    let commit = interrupts::without_interrupts(|| {
        super::scheduler::thread_affinity::set(
            target_task_id,
            requested_mask,
            online_mask,
            crate::user::abi::UserAbi::Linux,
        )
    })?;
    if commit.reschedule_required {
        super::request_deferred_reschedule();
    }
    Ok(commit)
}

pub fn windows_process_affinity(
    online_mask: u64,
) -> Result<super::scheduler::ProcessAffinitySnapshot, super::scheduler::AffinityError> {
    use super::scheduler::{AffinityError, ProcessAffinitySnapshot};
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return Err(AffinityError::MissingTask);
    }
    interrupts::without_interrupts(|| {
        let (_, abi, handle, _) =
            super::scheduler::stack_resources::context::current_user_binding()
                .ok_or(AffinityError::MissingTask)?;
        if abi != crate::user::abi::UserAbi::Windows {
            return Err(AffinityError::MissingTask);
        }
        let process_mask =
            super::process_placement::mask(handle).ok_or(AffinityError::MissingTask)? & online_mask;
        if process_mask == 0 {
            return Err(AffinityError::InvalidMask);
        }
        Ok(ProcessAffinitySnapshot {
            process_mask,
            system_mask: online_mask,
        })
    })
}

pub fn set_windows_process_affinity(
    requested_mask: u64,
    online_mask: u64,
) -> Result<super::scheduler::AffinityCommit, super::scheduler::AffinityError> {
    if nucleus_core::util::lockdep::preemption_disabled() || !current_cpu_task_slot_admitted() {
        return Err(super::scheduler::AffinityError::MissingTask);
    }
    // Current metadata capture through exact membership + queue ACK is one
    // non-yielding IRQ-excluded owner turn; no global directory guard.
    let commit = interrupts::without_interrupts(|| {
        super::scheduler::process_affinity::set_current(requested_mask, online_mask)
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
    // Full caller/process/ABI and target binding remain owned through queue ACK.
    let commit = interrupts::without_interrupts(|| {
        super::scheduler::thread_affinity::set(
            0,
            requested_mask,
            online_mask,
            crate::user::abi::UserAbi::Windows,
        )
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
            super::scheduler::stack_resources::context::current_user_log_ids()
        })
    })
}

/// Uses a complete per-CPU diagnostic identity before consulting scheduler
/// fallback. Production fallback is exact allocation-bank Current/context/start
/// custody, not a directory lock. The outer `None` is reserved for an absent, odd,
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
/// `None` when the seqlock record must be joined to physical Current custody.
/// Callers must already have interrupts masked.
fn published_current_user_log_ids() -> Option<Option<(u64, u64)>> {
    published_current_identity()?.complete_user_log_ids()
}

/// Log identity for an exact task, which the IPC receive path writes back to
/// the server for every request it takes.
///
/// A complete per-slot publication is diagnostic only. Its fallback retains
/// an exact original target UID through joined metadata, never a catalog guard.
pub fn user_log_ids_for_task(task_id: u64) -> Option<(u64, u64)> {
    if let Some(published) = interrupts::without_interrupts(||
        super::scheduler::published_user_log_ids(task_id)) {
        return published;
    }
    super::scheduler::current_observation::log_ids(task_id)
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
    interrupts::without_interrupts(super::scheduler::linux_thread_state::current_state)
}

pub fn current_user_stack_state() -> Option<super::UserStackState> {
    interrupts::without_interrupts(
        super::scheduler::stack_resources::context::current_user_stack_state,
    )
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
/// one coherent observation. Missing publication falls back only to joined
/// allocation-bank Current/context/entry custody, not a Scheduler catalog.
pub fn current_user_abi() -> Option<UserAbi> {
    interrupts::without_interrupts(|| {
        if let Some(abi) = cpu_local::current_cpu_task_slot()
            .and_then(super::current_identity::read_user_abi)
        {
            return abi;
        }
        super::scheduler::stack_resources::context::current_user_abi()
    })
}

/// Return the current task, ABI, and address-space identity without acquiring
/// the process-state lock. Scheduler-owned wait paths use this snapshot before
/// they have installed a waiter or deadline and must not inherit unrelated
/// same-process lock latency.
pub fn current_user_wait_binding() -> Option<(u64, UserAbi, u64)> {
    interrupts::without_interrupts(
        super::scheduler::stack_resources::context::current_user_wait_binding,
    )
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
/// Missing/torn publication uses joined resource-owner Current/context/start
/// custody, not directory recovery. Process state is accessed only after its
/// physical allocation is retained; missing owned evidence refuses.
pub fn current_user_snapshot() -> Option<CurrentUserSnapshot> {
    let (thread_id, abi, process_handle, console_session) = interrupts::without_interrupts(|| {
        if let Some(identity) = published_current_identity() {
            return identity.user_binding();
        }
        // SAFETY: interrupts are masked, so the current slot is stable.
        owned_current_user_binding()
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
    super::scheduler::current_observation::runtime()
}

pub fn is_user_task_alive(task_id: u64) -> bool {
    super::scheduler::current_observation::alive(task_id)
}

pub fn activate_suspended_user_task(task_id: u64) -> bool {
    activate_suspended_user_tasks(core::slice::from_ref(&task_id))
}

pub fn activate_suspended_user_tasks(task_ids: &[u64]) -> bool {
    activate_suspended_user_tasks_with_commit(task_ids, || Some(()))
}

/// Preflight retains original Current, Cohort, target allocations and readers
/// BEFORE bounded authority consumption. Enter with NO raw registry guard.
/// The callback may take bounded registry leaves but must release them before
/// returning the original owned grant. `None` refuses before any runnable effect;
/// `Some` consumes permission once and retains that SAME grant through the final
/// queue/prefix publication. It must not allocate, log or block. Exact target
/// backing survives even if concurrent terminal stop wins after consumption.
pub fn activate_suspended_user_tasks_with_commit<G, F>(task_ids: &[u64], commit_authority: F) -> bool
where
    F: FnOnce() -> Option<G>,
{
    super::scheduler::activation::activate(task_ids, commit_authority)
}

pub fn terminate_user_task(task_id: u64) -> bool {
    let terminated = super::scheduler::current_terminal::terminate(task_id);
    if terminated {
        complete_retirement_side_effects();
    }
    terminated
}

pub fn terminate_user_process(process_id: u64) -> bool {
    let terminated = group_exit::terminate(process_id);
    if terminated {
        complete_retirement_side_effects();
    }
    terminated
}

pub fn wake_user_task(task_id: u64) -> bool {
    if nucleus_core::util::lockdep::preemption_disabled() {
        return super::deferred_wake::defer_current_cpu(task_id);
    }
    super::scheduler::wake_publication::wake_user(task_id)
}

/// Non-authoritative ordering preference. Original Current may lend a latency
/// floor; this never donates funding, wakes a task or grants execution custody.
pub fn set_next_pick_hint(task_id: u64) {
    super::scheduler::ordering_publication::target(task_id)
}


/// Cold synchronous call: exact endpoint receive phase, original Current and
/// caller funding survive bind -> typed wake -> FIFO. Sender reply wait is NOT
/// armed here. Caller rejects raw/overlapping scopes before IPC publication and
/// cancels the exact reply on any later refusal; no scalar deferred wake grant.
pub fn commit_ipc_call_handoff(
    endpoint: u64,
    reply: u64,
    donor_task_id: u64,
    receiver_task_id: u64,
) -> Result<super::scheduler::IpcCallHandoffOutcome, super::scheduler::FastIpcCallHandoffOutcome> {
    if nucleus_core::util::lockdep::preemption_disabled()
        || nucleus_core::util::lockdep::held_spin_lock_depth() != 0 {
        return Err(super::scheduler::FastIpcCallHandoffOutcome::SenderMismatch);
    }
    let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
    scope.mask_irqs();
    let result = super::scheduler::ipc_publication::call_before_arm(
        &scope, endpoint, reply, donor_task_id, receiver_task_id);
    #[cfg(not(test))]
    crate::multitask::irq::flush_deferred_target_reschedules();
    result
}

pub fn set_next_latency_pick_hint(task_id: u64) -> bool {
    super::scheduler::ordering_publication::latency(task_id)
}

pub fn set_next_process_pick_hint(process_id: u64) -> Option<u64> {
    super::scheduler::ordering_publication::process(process_id)
}

pub fn set_next_spawn_pick_hint(task_id: u64) {
    super::scheduler::ordering_publication::spawn(task_id)
}

pub fn current_console_session() -> Option<ConsoleSessionHandle> {
    interrupts::without_interrupts(
        super::scheduler::stack_resources::context::current_console_session,
    )
}

mod exec;
pub use exec::{
    ExecRefusal, exec_current_user_process, exec_user_process_by_pid,
    exec_user_process_by_pid_checked,
};

pub fn linux_thread_snapshot_by_ids(
    process_id: u64,
    thread_id: u64,
) -> Option<super::LinuxThreadSnapshot> {
    super::scheduler::current_observation::linux(process_id, thread_id)
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
    super::scheduler::current_observation::set_tls(value);
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

/// Serializes a Linux fork snapshot against both process-state page-table
/// mutation and the parent's complete VMA publication. Every live VMA remains
/// at an odd sequence while `f` runs, so concurrent faults retry instead of
/// observing a temporary reservation hole.
pub fn with_fork_parent_state<R>(
    process_id: u64,
    f: impl FnOnce(&mut UserProcessState, &[rustos_user_abi::pager::PagerVmRegionWire]) -> R,
) -> Result<R, super::pager_vma::PagerVmaError> {
    let retained = process_table::retain_process_by_pid(process_id)
        .ok_or(super::pager_vma::PagerVmaError::Stale)?;
    let identity = retained
        .live_identity()
        .ok_or(super::pager_vma::PagerVmaError::Stale)?;
    retained
        .with_visible_state_mut(|_, state| {
            super::pager_vma::with_fork_held_regions(retained.handle(), identity, |regions| {
                f(state, regions)
            })
        })
        .ok_or(super::pager_vma::PagerVmaError::Stale)?
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
        owned_current_user_binding()
    })?;
    let identified =
        user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindIdentity, entry);
    process_table::with_own_visible_state(process_handle, |state| {
        user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindVisible, identified);
        f(state.address_space())
    })
}

/// Write-capable user-copy bind. Exact identity is sampled before the state
/// lock and rechecked after acquisition, so the COW slow path never takes
/// lifecycle authority recursively under ProcessStateLock.
pub(crate) fn with_current_user_copy_mm<R>(
    f: impl FnOnce(&super::UserCopyAddressSpace<'_>) -> R,
) -> Option<R> {
    let (_, _, process_handle, _) = interrupts::without_interrupts(|| {
        if let Some(identity) = published_current_identity() {
            return identity.user_binding();
        }
        // SAFETY: interrupts are masked, so the current slot is stable.
        owned_current_user_binding()
    })?;
    let identity = process_table::published_live_process_identity(process_handle)?;
    super::process_table::with_own_exact_visible_state(process_handle, identity, |state| {
        f(&super::UserCopyAddressSpace::new(
            state.address_space(),
            process_handle,
            identity,
        ))
    })
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

/// A transient original group/cold owner returns Err, not a missing-task result.
pub fn queue_linux_signal(process_id: u64, task_id: u64, signal: u64) -> Result<bool, ()> {
    super::scheduler::linux_thread_state::signals::queue(process_id, task_id, signal)
}

pub fn queue_linux_process_sigchld(process_id: u64, events: u32) -> bool {
    super::scheduler::linux_thread_state::signals::process_sigchld(process_id, events)
}

/// None means retry without consuming the pending stop cause.
pub fn stop_current_linux_process(signal: u64) -> Option<bool> {
    super::scheduler::linux_thread_state::signals::stop_current(signal)
}

/// Binds the current task's own process for one syscall.
///
/// A generation/PID-bound atomic allocation pin avoids process-table counted
/// retain/release. See [`current_user_address_space`] and
/// `rustos_user_abi::performance::IPC_SYSCALL_MAX_PROCESS_TABLE_ACQUISITIONS`
/// for why the two acquisitions a retain/release pair costs are worth
/// removing, and `process_table::own_process_ref` for why the pin is sound.
fn retain_current_user_process_binding() -> Option<(u64, UserAbi, process_table::ProcessRef)> {
    let entry = user_copy_profile::now();
    let (thread_id, abi, process_handle, _) = interrupts::without_interrupts(|| {
        if let Some(identity) = published_current_identity() {
            return identity.user_binding();
        }
        // SAFETY: interrupts are masked, so the current slot is stable.
        owned_current_user_binding()
    })?;
    let identified =
        user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindIdentity, entry);
    let process = match current_process_id_for_handle(process_handle) {
        Some(process_id) => process_table::own_process_ref(process_handle, process_id)?,
        // Neither publication nor joined Current metadata supplied a PID.
        // The handle still requires counted, table-serialized retention.
        None => process_table::retain_process(process_handle)?,
    };
    user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindRetain, identified);
    Some((thread_id, abi, process))
}

type RetainedLinuxThreadBinding = (
    u64,
    process_table::ProcessRef,
    super::scheduler::CurrentLinuxThreadBinding,
);

fn retain_current_linux_thread_binding() -> Option<RetainedLinuxThreadBinding> {
    let binding = interrupts::without_interrupts(|| {
        super::scheduler::linux_thread_state::current_binding()
    })?;
    let process = match current_process_id_for_handle(binding.process_handle) {
        Some(pid) => process_table::own_process_ref(binding.process_handle, pid)?,
        None => process_table::retain_process(binding.process_handle)?,
    };
    Some((process.process_id(), process, binding))
}

fn retain_current_process_ref() -> Option<process_table::ProcessRef> {
    let process_handle = interrupts::without_interrupts(owned_current_process_handle)?;
    // Exact atomic allocation custody when publication or joined Current
    // supplies the PID; otherwise only counted retention is available.
    match current_process_id_for_handle(process_handle) {
        Some(process_id) => process_table::own_process_ref(process_handle, process_id),
        None => process_table::retain_process(process_handle),
    }
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
    // This callback still runs on the exception stack with IF clear. Never
    // wait for the independent kernel cleanup executor here: its MM teardown
    // can need this CPU's TLB ACK. Return so compat records the terminal status
    // and HAL reaches the interruptible retired-task halt. The original banks
    // and durable cleanup rows remain owned until their separate RSP/worker ACKs.
    super::scheduler::current_terminal::fault(vector, error_code, cr2, rip, rsp)
}

pub fn halt_current_retired_task() -> ! {
    loop {
        interrupts::enable_and_hlt();
    }
}

pub(crate) fn exit_current_task() -> ! {
    super::scheduler::current_terminal::exit();
    complete_retirement_side_effects();
    halt_current_retired_task()
}

pub fn exit_current_user_task() -> ! {
    exit_current_task()
}

pub fn exit_current_user_process() -> ! {
    group_exit::exit_current()
}

pub fn service_deferred_work() -> usize {
    #[cfg(not(test))]
    let group_exit = {
        let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
        scope.mask_irqs();
        usize::from(super::scheduler::group_exit::service_deferred(&scope))
    };
    #[cfg(test)]
    let group_exit = 0;
    use super::scheduler::retirement_cleanup::{self, executor};
    let Some(executor) = executor::capture_current() else {
        // User callers may service durable non-yielding group commands but
        // never acquire external cleanup/reaper loans across a safe point.
        return group_exit;
    };
    let materialized_thread = super::scheduler::thread_creation::work::service(&executor);
    // A burst of exits (fork/wait loops, thread pools) retires many tasks
    // between two housekeeping turns. Draining a bounded batch per turn keeps
    // reclaim rate independent of how often this low-weight task is scheduled.
    const RETIREMENT_BATCH: usize = 16;
    let mut completed_side_effect = 0;
    while completed_side_effect < RETIREMENT_BATCH {
        let retirement_side_effect =
            interrupts::without_interrupts(|| retirement_cleanup::claim_next_kernel(&executor));
        let Some(retirement_side_effect) = retirement_side_effect else {
            break;
        };
        completed_side_effect += 1;
        retirement_side_effect.complete(|task_id| {
            let _ = wake_task(task_id);
        });
    }
    // Actual per-slot reset/resource seals, not the directory, own detachment.
    // The borrowed executor and both seals survive external completion.
    let mut reaped_slot = 0;
    while reaped_slot < RETIREMENT_BATCH {
        let retired_slot =
            interrupts::without_interrupts(|| super::scheduler::reaper::claim_next(&executor));
        let Some(retired_slot) = retired_slot else {
            break;
        };
        reaped_slot += 1;
        executor.assert_current();
        retired_slot.complete();
        executor.assert_current();
    }
    group_exit
        + materialized_thread
        + completed_side_effect
        + reaped_slot
        + process_table::reap_exited_processes()
}

fn complete_retirement_side_effects() {
    use super::scheduler::retirement_cleanup::{self, executor};
    if let Some(executor) = executor::capture_current() {
        loop {
            let side_effect =
                interrupts::without_interrupts(|| retirement_cleanup::claim_next_kernel(&executor));
            let Some(side_effect) = side_effect else {
                return;
            };
            side_effect.complete(|task_id| {
                let _ = wake_task(task_id);
            });
        }
    }
    // A retired user caller may never resume. It carries only observations,
    // while the independent kernel executor owns every external effect loan.
    let fence = interrupts::without_interrupts(executor::Fence::capture);
    while !interrupts::without_interrupts(|| fence.complete()) {
        // A real yield: the executor is a task on this CPU, and a spin that only
        // honours already-requested reschedules burns the whole quantum on a
        // single-CPU system before the executor can run.
        super::kick_housekeeping();
        super::yield_now();
    }
}

pub fn next_retired_task_cleanup() -> Option<super::RetiredTaskCleanup> {
    interrupts::without_interrupts(super::scheduler::retirement_cleanup::claim_next)
}

pub fn complete_retired_task_cleanup(cleanup: super::RetiredTaskCleanup) -> bool {
    interrupts::without_interrupts(|| super::scheduler::retirement_cleanup::complete(cleanup))
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
