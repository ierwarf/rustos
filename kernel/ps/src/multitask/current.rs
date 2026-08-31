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

mod ipc;

pub use ipc::{
    arm_block_current_task, arm_block_current_task_on_endpoint,
    arm_block_current_task_on_pager_fault, arm_block_current_task_on_reply,
    attach_reserved_ipc_priority, bind_ipc_priority_to_process_worker, bind_reserved_ipc_priority,
    cancel_block_current_task, cancel_ipc_priority_reservation,
    complete_fast_ipc_reply_wake_handoff_with_custody, complete_ipc_reply_wake_handoff,
    complete_ipc_reply_wake_handoff_with_custody, demote_current_user_task_to_user_class,
    inherit_ipc_priority, release_ipc_priorities_for_process, release_ipc_priority,
    reserve_ipc_call_donation, reserve_ipc_priority, settle_ipc_reply_scheduling_context,
    task_has_system_scheduling_class, wake_task,
};

use super::cpu_local;
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
/// The calling thread already pins its process object, so this takes no
/// reference count: the retain and its release were two acquisitions of the
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
        unsafe { scheduler_ref().current_user_process_binding() }
    })?;
    let identified =
        user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindIdentity, entry);
    let process_id = published_current_identity_process_id(process_handle)?;
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

fn published_current_pager_binding() -> Option<(
    u64,
    process_table::ProcessHandle,
    process_table::ProcessIdentity,
)> {
    let identity = published_current_identity()?;
    let (task_id, _, handle, _) = identity.user_binding()?;
    let process = process_table::published_live_process_identity(handle)?;
    Some((task_id, handle, process))
}

/// Lock-free authority used to bill one pager fault to the exact native or
/// IPC-donated scheduling context that was executing at exception entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PagerChargeSnapshot {
    pub context_slot: u64,
    pub context_generation: u64,
    pub scheduling_domain: u64,
    pub policy_epoch: u64,
    pub period_ns: u64,
    pub charge_token: u64,
}

/// Resolve pager billing without acquiring the scheduler lock.
///
/// The donation ledger publishes the effective owner and live reply token. The
/// selected owner's immutable policy stamp is then read through its seqlock
/// record. A native task uses its globally unique context generation as the
/// nonzero charge token; a passive server uses the live donation reply.
pub fn current_pager_charge_snapshot() -> Option<PagerChargeSnapshot> {
    interrupts::without_interrupts(|| {
        let current_slot = cpu_local::current_cpu_task_slot()?;
        let (owner_slot, donated_reply) =
            super::scheduler::borrowed_context_charge_token(current_slot)
                .unwrap_or((current_slot, 0));
        let stamp = super::current_identity::read(owner_slot)?.pager_charge?;
        let expected_context_slot = u64::try_from(owner_slot).ok()?.checked_add(1)?;
        if stamp.context_slot != expected_context_slot
            || stamp.context_generation == 0
            || stamp.scheduling_domain == 0
            || stamp.policy_epoch == 0
            || stamp.period_ns == 0
        {
            return None;
        }
        Some(PagerChargeSnapshot {
            context_slot: stamp.context_slot,
            context_generation: stamp.context_generation,
            scheduling_domain: stamp.scheduling_domain,
            policy_epoch: stamp.policy_epoch,
            period_ns: stamp.period_ns,
            charge_token: if donated_reply != 0 {
                donated_reply
            } else {
                stamp.context_generation
            },
        })
    })
}

/// Stamp and publish a pager region for the current exact process/MM epoch.
///
/// The input is an unbound template: all process and VMA generation fields
/// must be zero. Kernel-ps supplies them from lock-free current/process
/// publications, so neither syscalld nor pagerd can forge process authority.
pub fn publish_current_pager_vma(
    template: rustos_user_abi::pager::PagerVmRegionWire,
) -> Result<rustos_user_abi::pager::PagerVmRegionWire, super::pager_vma::PagerVmaError> {
    let process_id = current_user_process_id().ok_or(super::pager_vma::PagerVmaError::Stale)?;
    // VMA publication is normal-time policy work: it must prepare all
    // intermediate page-table leaves before the all-atomic fault snapshot can
    // authorize exception entry.  Reuse the target-process transaction so the
    // current and non-current paths cannot diverge on that prerequisite.
    publish_pager_vma_for_process(process_id, template)
}

/// Stamp and publish a pager VMA for one retained non-current process.
///
/// MM brokers invoke this before a pageable mapping becomes observable. The
/// retain pins the exact table slot while `live_identity` supplies the current
/// process/MM generations; therefore a recycled PID or an exec/exit boundary
/// cannot publish authority for another address space. This is normal
/// syscall-time work: exception entry consumes only the all-atomic VMA
/// snapshot and never performs this lookup or takes this lock.
pub fn publish_pager_vma_for_process(
    process_id: u64,
    template: rustos_user_abi::pager::PagerVmRegionWire,
) -> Result<rustos_user_abi::pager::PagerVmRegionWire, super::pager_vma::PagerVmaError> {
    let retained = process_table::retain_process_by_pid(process_id)
        .ok_or(super::pager_vma::PagerVmaError::Stale)?;
    let page_count = usize::try_from(
        (template.end.saturating_sub(template.start)) / rustos_user_abi::pager::PAGER_PAGE_BYTES,
    )
    .map_err(|_| super::pager_vma::PagerVmaError::Pressure)?;
    retained
        .with_state_mut(|_, state| {
            state
                .address_space_mut()
                .prepare_pager_fault_pages_at(VirtAddr::new(template.start), page_count)
        })
        .map_err(|_| super::pager_vma::PagerVmaError::Pressure)?;
    let identity = retained
        .live_identity()
        .ok_or(super::pager_vma::PagerVmaError::Stale)?;
    super::pager_vma::publish(retained.handle(), identity, template)
}

/// Resolve one current-task fault without the scheduler or process-state lock.
///
/// Exception entry already masks interrupts; masking here also makes direct
/// diagnostic/test callers obey the same current-slot stability contract.
pub fn current_pager_vma_snapshot(
    address: u64,
    access: u16,
) -> Result<super::pager_vma::PagerVmaSnapshot, super::pager_vma::PagerVmaError> {
    interrupts::without_interrupts(|| {
        let (task_id, handle, process) =
            published_current_pager_binding().ok_or(super::pager_vma::PagerVmaError::Stale)?;
        let region = super::pager_vma::lookup(handle, process, address, access)?;
        Ok(super::pager_vma::PagerVmaSnapshot {
            task_id,
            process_id: process.process_id(),
            region,
        })
    })
}

/// Revoke the exact current-process VMA generation before removing PTEs.
pub fn revoke_current_pager_vma(
    start: u64,
    vma_generation: u64,
) -> Result<rustos_user_abi::pager::PagerVmRegionWire, super::pager_vma::PagerVmaError> {
    interrupts::without_interrupts(|| {
        let (_, handle, process) =
            published_current_pager_binding().ok_or(super::pager_vma::PagerVmaError::Stale)?;
        super::pager_vma::revoke(handle, process, start, vma_generation)
    })
}

/// Withdraw one exact non-current pager VMA before its PTEs are removed.
///
/// This is the paired normal-context operation for
/// `publish_pager_vma_for_process`; stale PID, process/MM generation, start,
/// or VMA generation values fail closed before the publication is cleared.
pub fn revoke_pager_vma_for_process(
    process_id: u64,
    start: u64,
    vma_generation: u64,
) -> Result<rustos_user_abi::pager::PagerVmRegionWire, super::pager_vma::PagerVmaError> {
    let retained = process_table::retain_process_by_pid(process_id)
        .ok_or(super::pager_vma::PagerVmaError::Stale)?;
    let identity = retained
        .live_identity()
        .ok_or(super::pager_vma::PagerVmaError::Stale)?;
    super::pager_vma::revoke(retained.handle(), identity, start, vma_generation)
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

/// Log identity for an exact task, which the IPC receive path writes back to
/// the server for every request it takes.
///
/// The published per-slot record answers this without the catalog guard
/// whenever the shared task directory still resolves the id; a stale, absent,
/// or terminal slot falls back to the authoritative locked lookup.
pub fn user_log_ids_for_task(task_id: u64) -> Option<(u64, u64)> {
    interrupts::without_interrupts(|| {
        if let Some(published) = super::scheduler::published_user_log_ids(task_id) {
            return published;
        }
        // SAFETY: interrupts are masked and no scheduler-owned reference escapes.
        unsafe { scheduler_ref().user_log_ids_for_task(task_id) }
    })
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

/// Binds the current task's own process for one syscall.
///
/// The calling thread pins its own process object, so this takes no reference
/// count. See [`current_user_address_space`] and
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
        unsafe { scheduler_ref().current_user_process_binding() }
    })?;
    let identified =
        user_copy_profile::charge(user_copy_profile::UserCopyPhase::BindIdentity, entry);
    let process = match published_current_identity_process_id(process_handle) {
        Some(process_id) => process_table::own_process_ref(process_handle, process_id)?,
        // A slot whose published record is incomplete or mid-update cannot
        // prove the pin's premise, so it pays the counted retain.
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
    let binding = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().current_linux_thread_binding()
    })?;
    let process = process_table::retain_process(binding.process_handle)?;
    Some((process.process_id(), process, binding))
}

fn retain_current_process_ref() -> Option<process_table::ProcessRef> {
    let process_handle =
        interrupts::without_interrupts(|| unsafe { scheduler_ref().current_process_handle() })?;
    // Same own-thread pin as `retain_current_user_process_binding`; a slot the
    // published record cannot vouch for still pays the counted retain.
    match published_current_identity_process_id(process_handle) {
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
