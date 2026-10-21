//! Lock-free identity publication for the task running on each CPU.
//!
//! Syscall entry asks "who am I?" far more often than the scheduler ever
//! dispatches. The per-caller acquisition census measured 167k of these
//! queries per second against roughly 4k dispatches, and every one of them
//! took the exclusive global scheduler lock to read fields belonging to the
//! task already running on the asking CPU. Linux answers the same question
//! from `current`, FreeBSD from `curthread`, and Zircon from
//! `Thread::Current::Get()` — a published cell, never the run-queue lock.
//!
//! Each task slot owns identity/lifecycle writer custody under a separate
//! lock. Its seqlock-protected atomic record is a read-only publication; reads
//! run with interrupts masked, so the asking CPU's current slot cannot change
//! underneath the observation. A reader that catches a writer mid-update, or
//! that finds a slot which was never published, returns `None` and its caller
//! falls back to the locked query.
//!
//! Publication is not optional. A write site that mutates identity without
//! republishing leaves a stale record, which is a correctness fault and not
//! merely a slow path, so `matches_authority` re-derives each slot from the
//! directory metadata and independently owned lifecycle while directory
//! custody stabilizes metadata. This detects missed publication; it does not
//! confer frame/process lifetime or license removing the directory guard.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};

use super::MAX_SCHEDULER_TASKS;
use super::process_table::ProcessHandle;
use crate::io::session::ConsoleSessionHandle;
use crate::user::abi::UserAbi;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

const FLAG_PUBLISHED: u32 = 1 << 0;
const FLAG_TASK_ID: u32 = 1 << 1;
const FLAG_USER_MODE: u32 = 1 << 2;
const FLAG_PROCESS: u32 = 1 << 3;
const FLAG_PROCESS_ID: u32 = 1 << 4;
const FLAG_PAGER_CHARGE: u32 = 1 << 5;
const ABI_SHIFT: u32 = 6;
const ABI_MASK: u32 = 0b11 << ABI_SHIFT;
const ABI_NONE: u32 = 0;
const ABI_LINUX: u32 = 1;
const ABI_WINDOWS: u32 = 2;

const DISPATCH_RETIRED: u32 = 1 << 0;
const DISPATCH_SUSPENDED: u32 = 1 << 1;
const DISPATCH_JOB_STOPPED: u32 = 1 << 2;
const DISPATCH_EXEC_QUIESCED: u32 = 1 << 3;
const DISPATCH_INELIGIBLE: u32 =
    DISPATCH_RETIRED | DISPATCH_SUSPENDED | DISPATCH_JOB_STOPPED | DISPATCH_EXEC_QUIESCED;

/// Lifecycle state published with an identity in one seqlock commit.
///
/// This is a read view, never a second dispatch authority: readers that catch
/// an unavailable publication must retain their locked fallback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DispatchPublication {
    pub(super) task_id: Option<u64>,
    pub(super) user_mode: bool,
    pub(super) process_id: Option<u64>,
    pub(super) pager_charge: Option<PagerChargeStamp>,
    dispatch_flags: u32,
}

impl DispatchPublication {
    #[inline]
    pub(super) const fn lifecycle_dispatchable(self) -> bool {
        self.dispatch_flags & DISPATCH_INELIGIBLE == 0
    }

    pub(super) const fn retired(self) -> bool {
        self.dispatch_flags & DISPATCH_RETIRED != 0
    }

    pub(super) const fn job_stopped(self) -> bool {
        self.dispatch_flags & DISPATCH_JOB_STOPPED != 0
    }

    pub(super) const fn exec_quiesced(self) -> bool {
        self.dispatch_flags & DISPATCH_EXEC_QUIESCED != 0
    }
}

/// Authoritative lifecycle fields copied into `DispatchPublication`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DispatchLifecycle {
    pub(super) retired: bool,
    pub(super) suspended: bool,
    pub(super) job_stopped: bool,
    pub(super) exec_quiesced: bool,
}

impl DispatchLifecycle {
    const fn flags(self) -> u32 {
        (self.retired as u32 * DISPATCH_RETIRED)
            | (self.suspended as u32 * DISPATCH_SUSPENDED)
            | (self.job_stopped as u32 * DISPATCH_JOB_STOPPED)
            | (self.exec_quiesced as u32 * DISPATCH_EXEC_QUIESCED)
    }

    #[inline]
    pub(super) const fn dispatchable(self) -> bool {
        self.flags() & DISPATCH_INELIGIBLE == 0
    }
}

/// Immutable scheduling authority copied into the slot publication record.
/// A faulting task resolves IPC donation first, then reads the selected owner's
/// stamp without taking the scheduler lock.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PagerChargeStamp {
    pub(super) context_slot: u64,
    pub(super) context_generation: u64,
    pub(super) scheduling_domain: u64,
    pub(super) domain_generation: u64,
    pub(super) policy_epoch: u64,
    pub(super) period_ns: u64,
}

/// The identity fields a running task can be asked for without the scheduler
/// lock. Everything here is bound when the slot is installed and changes only
/// through the publication sites, never through ordinary task execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct TaskIdentity {
    pub(super) task_id: Option<u64>,
    pub(super) user_mode: bool,
    pub(super) abi: Option<UserAbi>,
    pub(super) process_handle: Option<ProcessHandle>,
    /// Immutable PID bound to this task slot. This is diagnostic identity
    /// only; the scheduler's slot/current-owner state remains authoritative.
    pub(super) process_id: Option<u64>,
    pub(super) console_session: ConsoleSessionHandle,
    pub(super) pager_charge: Option<PagerChargeStamp>,
}

impl TaskIdentity {
    /// The binding shape `current_user_process_binding` returns: user tasks
    /// only, and only once every field it needs is present.
    pub(super) fn user_binding(
        &self,
    ) -> Option<(u64, UserAbi, ProcessHandle, ConsoleSessionHandle)> {
        if !self.user_mode {
            return None;
        }
        Some((
            self.task_id?,
            self.abi?,
            self.process_handle?,
            self.console_session,
        ))
    }

    /// Returns a definitive log identity when this record is complete.
    ///
    /// The outer `None` means a user record is incomplete and callers must
    /// use the locked authority. `Some(None)` is a complete kernel-task
    /// record, which conclusively has no user log identity.
    pub(super) fn complete_user_log_ids(&self) -> Option<Option<(u64, u64)>> {
        if !self.user_mode {
            return Some(None);
        }
        Some(Some((self.process_id?, self.task_id?)))
    }
}

/// Serialized identity/lifecycle writer custody. Atomic cells below are a
/// read-only publication, not a second lifecycle authority.
struct IdentityWriter {
    bound_task_id: Option<u64>,
    exec_generation: u64,
    kernel_executors: usize,
    exec_intent: Option<exec_intent::Record>,
    identity: Option<TaskIdentity>,
    lifecycle: DispatchLifecycle,
}

impl IdentityWriter {
    const fn empty() -> Self {
        Self {
            bound_task_id: None,
            exec_generation: 0,
            kernel_executors: 0,
            exec_intent: None,
            identity: None,
            lifecycle: DispatchLifecycle {
                retired: false,
                suspended: false,
                job_stopped: false,
                exec_quiesced: false,
            },
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum LifecycleGate {
    Retired,
    Suspended,
    JobStopped,
    ExecQuiesced,
}

impl DispatchLifecycle {
    fn set(&mut self, gate: LifecycleGate, value: bool) {
        match gate {
            LifecycleGate::Retired => self.retired = value,
            LifecycleGate::Suspended => self.suspended = value,
            LifecycleGate::JobStopped => self.job_stopped = value,
            LifecycleGate::ExecQuiesced => self.exec_quiesced = value,
        }
    }
}

static WRITERS: [TrackedSpinLock<IdentityWriter, { LockClass::TaskIdentityLifecycle as u8 }>;
    MAX_SCHEDULER_TASKS] =
    [const { TrackedSpinLock::new(IdentityWriter::empty()) }; MAX_SCHEDULER_TASKS];

fn with_writer<R>(slot: usize, f: impl FnOnce(&mut IdentityWriter) -> R) -> Option<R> {
    let cell = WRITERS.get(slot)?;
    #[cfg(not(test))]
    {
        x86_64::instructions::interrupts::without_interrupts(|| {
            // SAFETY: IRQs stay masked until the guard drops inside this closure.
            let mut writer = unsafe { cell.lock_masked() };
            Some(f(&mut writer))
        })
    }
    #[cfg(test)]
    {
        let mut writer = cell.lock();
        Some(f(&mut writer))
    }
}

/// Scalar liveness at this writer-leaf linearization point, NOT a backing,
/// execution or wake grant. Transient exec/cohort admission refusal is not
/// death: waiter registries must not replace a still-live user on that basis.
pub(in crate::multitask) fn observe_user_liveness(slot: usize, id: u64) -> bool {
    with_writer(slot, |writer| {
        writer.bound_task_id == Some(id)
            && writer.identity.is_some_and(|entry| entry.task_id == Some(id) && entry.user_mode)
            && !writer.lifecycle.retired
    }).unwrap_or(false)
}

/// Original Identity51 custody for one scalar reply/object comparison only.
/// No execution/funding grant, allocation or hardware callback may escape f.
pub(in crate::multitask) fn with_reply_custody(
    slot: usize,
    id: u64,
    f: impl FnOnce(bool) -> Option<(bool, bool)>,
) -> Option<(bool, bool)> {
    with_writer(slot, |writer| {
        writer.identity.filter(|identity| identity.task_id == Some(id))?;
        f(!writer.lifecycle.retired)
    })?
}

/// Join a constructor's exact published UID to its scalar Resources53 ACK.
/// No callback, queue, allocation, hardware or cross-owner destruction in f.
pub(in crate::multitask) fn with_exact_publication<R>(
    slot: usize,
    id: u64,
    f: impl FnOnce() -> R,
) -> Option<R> {
    with_writer(slot, |writer| {
        writer
            .identity
            .filter(|identity| identity.task_id == Some(id))?;
        Some(f())
    })?
}

/// A complete exact desired-stop observation; hidden metadata preserves the
/// binding. False includes missing/foreign binding and is never absence proof.
pub(in crate::multitask) fn terminal_retired_matches(slot: usize, id: u64) -> bool {
    with_writer(slot, |writer| {
        writer.bound_task_id == Some(id) && writer.lifecycle.retired
    })
    .unwrap_or(false)
}

/// Only a scalar Resources53 binding check may run while Identity51 is held.
/// Terminal producer custody remains in Class50, never nested with this leaf.
pub(in crate::multitask) fn with_terminal_binding<R>(
    slot: usize,
    id: u64,
    f: impl FnOnce() -> Option<R>,
) -> Option<R> {
    with_writer(slot, |writer| {
        if writer.bound_task_id != Some(id) || writer.kernel_executors != 0 {
            return None;
        }
        f()
    })?
}

/// Identity51 joins exact descriptor UID and cold cohort seal to scalar Affinity54 writes.
/// Refusal precedes masks/epochs; no observations, queue/process work or Drop inside f.
pub(in crate::multitask) fn with_affinity_replacement<R>(
    slot: usize,
    id: Option<u64>,
    owner: Option<&cohort::Cohort>,
    f: impl FnOnce() -> R,
) -> Option<R> {
    with_writer(slot, |writer| {
        #[cfg(test)]
        let fixture = writer.bound_task_id.is_none() && writer.identity.is_none();
        #[cfg(not(test))]
        let fixture = false;
        if !fixture && (id.is_none() || writer.bound_task_id != id) {
            return None;
        }
        let granted = match owner {
            Some(cohort) => id.is_some_and(|id| cohort.owns_affinity(slot, id)),
            None => !cohort::busy(slot),
        };
        if !granted {
            return None;
        }
        Some(f())
    })?
}

/// ORIGINAL pinned IPC receiver joins exact UID/lifecycle and cohort exclusion
/// to Affinity54 -> FIFO56 -> Wait58 scalar publication. The caller retains its
/// physical binding and reader OUTSIDE these leaves. No resource/frame/budget,
/// allocation, process, destruction or notification work may enter `publish`.
pub(in crate::multitask) fn with_ipc_handoff_binding<R>(
    slot: usize,
    id: u64,
    publish: impl FnOnce() -> R,
) -> Option<R> {
    with_writer(slot, |writer| {
        if writer.bound_task_id != Some(id)
            || !writer.lifecycle.dispatchable()
            || writer.exec_intent.is_some()
            || cohort::busy(slot)
        {
            return None;
        }
        Some(publish())
    })?
}

/// Exact hidden-or-visible lifetime binding for producer-last reclaim/reuse.
/// f only joins the scalar resource leaf; no external work or destruction.
pub(in crate::multitask) fn with_reclaim_binding<R>(
    slot: usize,
    expected: Option<u64>,
    f: impl FnOnce() -> R,
) -> Option<R> {
    with_reclaim_binding_qualified(slot, expected, false, f)
}

/// Retired reapers are distinct from unpublished constructor rollback.
/// Desired stop and all execution custody are checked in this exact identity turn.
pub(in crate::multitask) fn with_reclaim_binding_qualified<R>(
    slot: usize,
    expected: Option<u64>,
    retired: bool,
    f: impl FnOnce() -> R,
) -> Option<R> {
    with_writer(slot, |writer| {
        if writer.bound_task_id != expected
            || writer.exec_intent.is_some()
            || execution::count(slot) != 0
            || cohort::busy(slot)
            || (retired
                && (expected.is_none()
                    || !writer.lifecycle.retired
                    || writer.kernel_executors != 0))
        {
            return None;
        }
        Some(f())
    })?
}

/// Explicit old lifetime clear after owned resource drain, before reuse ACK.
/// validate only authenticates the exact drained Resources53 receipt.
pub(in crate::multitask) fn clear_reclaimed(
    slot: usize,
    expected: Option<u64>,
    validate: impl FnOnce() -> bool,
) -> Option<()> {
    with_writer(slot, |writer| {
        if writer.bound_task_id != expected
            || writer.exec_intent.is_some()
            || execution::count(slot) != 0
            || cohort::busy(slot)
            || !validate()
        {
            return None;
        }
        let generation = writer.exec_generation;
        *writer = IdentityWriter::empty();
        writer.exec_generation = generation;
        clear_publication(slot);
        Some(())
    })?
}

/// Metadata writers cannot replay an obsolete lifecycle snapshot. Directory
/// admission/rebind still owns constructing identity and stack geometry.
pub(super) fn publish_metadata(slot: usize, identity: Option<TaskIdentity>) {
    with_writer(slot, |writer| {
        if let Some(task_id) = writer.bound_task_id {
            assert!(
                identity.is_none_or(|next| next.task_id == Some(task_id)),
                "bound execution identity changed before clear"
            );
        }
        check_binding(writer.identity, identity);
        execution::kernel_executor::check_metadata(writer, identity);
        writer.identity = identity;
        commit_writer(slot, writer);
    });
}

/// Cold-directory adapter: flags and the current identity (including absence
/// during partial initialization/teardown) become visible in one transaction.
pub(super) fn publish_lifecycle_change(
    slot: usize,
    identity: Option<TaskIdentity>,
    gate: LifecycleGate,
    value: bool,
) {
    with_writer(slot, |writer| {
        if let Some(task_id) = writer.bound_task_id {
            assert!(
                identity.is_none_or(|next| next.task_id == Some(task_id)),
                "bound execution identity changed before clear"
            );
        }
        check_binding(writer.identity, identity);
        execution::kernel_executor::check_metadata(writer, identity);
        assert!(
            writer.kernel_executors == 0 || !value,
            "lifecycle stop crossed live kernel executor custody"
        );
        exec_intent::check_unowned_gate(writer, gate, value);
        cohort::check_unowned_gate(slot, gate);
        writer.lifecycle.set(gate, value);
        writer.identity = identity;
        commit_writer(slot, writer);
    });
}

/// An exact live-identity command never rebinds metadata or modifies a recycled
/// task. Owner locks are dropped before wake/reschedule/other external work.
pub(super) fn update_live_lifecycle(
    slot: usize,
    task_id: u64,
    gate: LifecycleGate,
    value: bool,
) -> bool {
    with_writer(slot, |writer| {
        if writer.identity.and_then(|identity| identity.task_id) != Some(task_id)
            || (writer.kernel_executors != 0 && value)
        {
            return false;
        }
        exec_intent::check_unowned_gate(writer, gate, value);
        cohort::check_unowned_gate(slot, gate);
        writer.lifecycle.set(gate, value);
        commit_writer(slot, writer);
        true
    })
    .unwrap_or(false)
}

pub(super) fn lifecycle(slot: usize) -> DispatchLifecycle {
    with_writer(slot, |writer| writer.lifecycle).unwrap_or(DispatchLifecycle {
        retired: true,
        suspended: true,
        job_stopped: true,
        exec_quiesced: true,
    })
}

fn check_binding(previous: Option<TaskIdentity>, next: Option<TaskIdentity>) {
    if let (Some(previous), Some(next)) = (previous, next) {
        if let Some(task_id) = previous.task_id {
            assert_eq!(next.task_id, Some(task_id), "identity rebind before clear");
        }
    }
}

fn commit_writer(slot: usize, writer: &mut IdentityWriter) {
    if let Some(task_id) = writer.identity.and_then(|identity| identity.task_id) {
        assert!(
            writer.bound_task_id.is_none_or(|old| old == task_id),
            "execution identity rebound without explicit drained reset"
        );
        writer.bound_task_id = Some(task_id);
    }
    match writer.identity {
        Some(identity) => commit_identity(slot, identity, writer.lifecycle),
        None => clear_publication(slot),
    }
}

struct PublishedIdentity {
    /// Even when stable, odd while a writer is between the two halves of an
    /// update. Readers that observe an odd or changed value take the lock.
    version: AtomicU64,
    task_id: AtomicU64,
    /// `index << 32 | generation`, meaningful only under `FLAG_PROCESS`.
    process: AtomicU64,
    /// Meaningful only under `FLAG_PROCESS_ID`.
    process_id: AtomicU64,
    console: AtomicU64,
    pager_context_slot: AtomicU64,
    pager_context_generation: AtomicU64,
    pager_scheduling_domain: AtomicU64,
    pager_domain_generation: AtomicU64,
    pager_policy_epoch: AtomicU64,
    pager_period_ns: AtomicU64,
    flags: AtomicU32,
    dispatch_flags: AtomicU32,
}

impl PublishedIdentity {
    const fn empty() -> Self {
        Self {
            version: AtomicU64::new(0),
            task_id: AtomicU64::new(0),
            process: AtomicU64::new(0),
            process_id: AtomicU64::new(0),
            console: AtomicU64::new(0),
            pager_context_slot: AtomicU64::new(0),
            pager_context_generation: AtomicU64::new(0),
            pager_scheduling_domain: AtomicU64::new(0),
            pager_domain_generation: AtomicU64::new(0),
            pager_policy_epoch: AtomicU64::new(0),
            pager_period_ns: AtomicU64::new(0),
            flags: AtomicU32::new(0),
            dispatch_flags: AtomicU32::new(0),
        }
    }
}

#[expect(
    clippy::declare_interior_mutable_const,
    reason = "array initializer for per-slot atomics"
)]
const EMPTY_IDENTITY: PublishedIdentity = PublishedIdentity::empty();

static IDENTITIES: [PublishedIdentity; MAX_SCHEDULER_TASKS] = [EMPTY_IDENTITY; MAX_SCHEDULER_TASKS];

/// Conservative per-slot "this thread may have a pending signal" hint, the
/// same role `TIF_SIGPENDING` plays in Linux: syscall return checks it on every
/// exit and must not pay for the run-queue lock to learn that nothing is
/// pending.
///
/// The hint is allowed to read `true` when nothing is pending, which costs one
/// locked recheck. It can never read `false` while something is pending: the
/// only site that raises a pending signal and the only site that lowers this
/// hint both run under the scheduler lock, so they are serialized against each
/// other, and the raise stores the hint after the state it describes.
static SIGNAL_PENDING: [AtomicBool; MAX_SCHEDULER_TASKS] =
    [const { AtomicBool::new(false) }; MAX_SCHEDULER_TASKS];

/// Raises the hint for `slot`. Callers must hold the scheduler lock and must
/// already have written the pending state this advertises.
pub(super) fn raise_signal_pending(slot: usize) {
    if let Some(flag) = SIGNAL_PENDING.get(slot) {
        // ORDERING: Release publishes the pending-signal state written before
        // this call to any reader that observes the raised hint.
        flag.store(true, Ordering::Release);
    }
}

/// Re-derives the hint for `slot` from authoritative thread state. Production
/// callers hold the Linux-thread leaf through mutation/snapshot and this
/// store, which serializes every raise without the Scheduler guard.
pub(super) fn sync_signal_pending(slot: usize, pending: bool) {
    if let Some(flag) = SIGNAL_PENDING.get(slot) {
        flag.store(pending, Ordering::Release);
    }
}

/// Reads the hint for `slot`. An unknown slot reports `true` so the caller
/// falls back to the authoritative locked query.
pub(super) fn signal_pending(slot: usize) -> bool {
    SIGNAL_PENDING
        .get(slot)
        // ORDERING: Acquire pairs with the raising Release store.
        .is_none_or(|flag| flag.load(Ordering::Acquire))
}

fn encode_process(handle: Option<ProcessHandle>) -> (u64, u32) {
    match handle {
        Some(handle) => {
            let index = u64::try_from(handle.index()).expect("process index exceeds u64");
            ((index << 32) | u64::from(handle.generation()), FLAG_PROCESS)
        }
        None => (0, 0),
    }
}

fn decode_process(word: u64, flags: u32) -> Option<ProcessHandle> {
    if flags & FLAG_PROCESS == 0 {
        return None;
    }
    let index = usize::try_from(word >> 32).expect("published process index exceeds usize");
    let generation = u32::try_from(word & u64::from(u32::MAX)).expect("generation exceeds u32");
    Some(ProcessHandle::new(index, generation))
}

fn encode_abi(abi: Option<UserAbi>) -> u32 {
    let code = match abi {
        None => ABI_NONE,
        Some(UserAbi::Linux) => ABI_LINUX,
        Some(UserAbi::Windows) => ABI_WINDOWS,
    };
    code << ABI_SHIFT
}

fn decode_abi(flags: u32) -> Option<UserAbi> {
    match (flags & ABI_MASK) >> ABI_SHIFT {
        ABI_LINUX => Some(UserAbi::Linux),
        ABI_WINDOWS => Some(UserAbi::Windows),
        _ => None,
    }
}

/// Test-only compatibility helper. Production code must commit the explicit
/// lifecycle with `publish_state`; an implicit all-clear lifecycle is unsafe.
#[cfg(test)]
pub(super) fn publish_test_identity(slot: usize, identity: TaskIdentity) {
    publish_state(slot, identity, DispatchLifecycle::default());
}

/// Isolated host fixture publisher. Production metadata cannot supply flags.
#[cfg(test)]
pub(super) fn publish_state(slot: usize, identity: TaskIdentity, lifecycle: DispatchLifecycle) {
    with_writer(slot, |writer| {
        if writer.bound_task_id != identity.task_id {
            assert_eq!(
                execution::count(slot),
                0,
                "fixture rebind discards execution custody"
            );
            writer.bound_task_id = None;
        }
        writer.identity = Some(identity);
        writer.lifecycle = lifecycle;
        commit_writer(slot, writer);
    });
}

/// Called only with the matching per-slot writer lock held.
fn commit_identity(slot: usize, identity: TaskIdentity, lifecycle: DispatchLifecycle) {
    let Some(cell) = IDENTITIES.get(slot) else {
        return;
    };
    // ORDERING: the per-slot writer owner excludes every other writer; the
    // global scheduler directory is not publication custody.
    let version = cell.version.load(Ordering::Relaxed);
    assert_eq!(
        version & 1,
        0,
        "task identity: concurrent publication for slot {slot}"
    );
    let (process, process_flag) = encode_process(identity.process_handle);
    let mut flags = FLAG_PUBLISHED | process_flag | encode_abi(identity.abi);
    if identity.user_mode {
        flags |= FLAG_USER_MODE;
    }
    if identity.task_id.is_some() {
        flags |= FLAG_TASK_ID;
    }
    if identity.process_id.is_some() {
        flags |= FLAG_PROCESS_ID;
    }
    if identity.pager_charge.is_some() {
        flags |= FLAG_PAGER_CHARGE;
    }

    // ORDERING: the odd version must be visible before any field changes, or a
    // reader could load a half-updated record and still see a stable version.
    cell.version
        .store(version.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    cell.task_id
        .store(identity.task_id.unwrap_or(0), Ordering::Relaxed);
    cell.process.store(process, Ordering::Relaxed);
    cell.process_id
        .store(identity.process_id.unwrap_or(0), Ordering::Relaxed);
    cell.console
        .store(identity.console_session.raw(), Ordering::Relaxed);
    let pager_charge = identity.pager_charge.unwrap_or_default();
    cell.pager_context_slot
        .store(pager_charge.context_slot, Ordering::Relaxed);
    cell.pager_context_generation
        .store(pager_charge.context_generation, Ordering::Relaxed);
    cell.pager_scheduling_domain
        .store(pager_charge.scheduling_domain, Ordering::Relaxed);
    cell.pager_domain_generation
        .store(pager_charge.domain_generation, Ordering::Relaxed);
    cell.pager_policy_epoch
        .store(pager_charge.policy_epoch, Ordering::Relaxed);
    cell.pager_period_ns
        .store(pager_charge.period_ns, Ordering::Relaxed);
    cell.dispatch_flags
        .store(lifecycle.flags(), Ordering::Relaxed);
    cell.flags.store(flags, Ordering::Relaxed);
    // ORDERING: Release publishes every field store above to any reader that
    // observes this even version.
    cell.version
        .store(version.wrapping_add(2), Ordering::Release);
}

/// Explicit admission/reset boundary; normal absent-metadata publication must
/// preserve pending suspension/stop intent instead of silently clearing it.
pub(super) fn clear(slot: usize) {
    with_writer(slot, |writer| {
        assert_eq!(
            execution::count(slot),
            0,
            "identity reset discards CPU execution custody"
        );
        assert!(
            writer.exec_intent.is_none(),
            "identity reset discards cold exec intent custody"
        );
        let generation = writer.exec_generation;
        *writer = IdentityWriter::empty();
        writer.exec_generation = generation;
        clear_publication(slot);
    });
}

fn clear_publication(slot: usize) {
    let Some(cell) = IDENTITIES.get(slot) else {
        return;
    };
    let version = cell.version.load(Ordering::Relaxed);
    assert_eq!(
        version & 1,
        0,
        "task identity: concurrent retirement for slot {slot}"
    );
    cell.version
        .store(version.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    // Clearing every scalar while the record is odd makes a stale PID/task
    // pair unrepresentable even to future maintenance code that adds flags.
    cell.task_id.store(0, Ordering::Relaxed);
    cell.process.store(0, Ordering::Relaxed);
    cell.process_id.store(0, Ordering::Relaxed);
    cell.console.store(0, Ordering::Relaxed);
    cell.pager_context_slot.store(0, Ordering::Relaxed);
    cell.pager_context_generation.store(0, Ordering::Relaxed);
    cell.pager_scheduling_domain.store(0, Ordering::Relaxed);
    cell.pager_domain_generation.store(0, Ordering::Relaxed);
    cell.pager_policy_epoch.store(0, Ordering::Relaxed);
    cell.pager_period_ns.store(0, Ordering::Relaxed);
    cell.dispatch_flags.store(0, Ordering::Relaxed);
    cell.flags.store(0, Ordering::Relaxed);
    cell.version
        .store(version.wrapping_add(2), Ordering::Release);
}

/// Reads the published identity for `slot`, or `None` when the slot was never
/// published or a writer is mid-update.
pub(super) fn read(slot: usize) -> Option<TaskIdentity> {
    read_state(slot).map(|(identity, _)| identity)
}

/// Reads the immutable identity and lifecycle state from one stable commit.
fn read_state(slot: usize) -> Option<(TaskIdentity, u32)> {
    let cell = IDENTITIES.get(slot)?;
    // ORDERING: Acquire pairs with the publishing Release store and orders the
    // field loads below after this version observation.
    let before = cell.version.load(Ordering::Acquire);
    if before & 1 != 0 {
        return None;
    }
    let flags = cell.flags.load(Ordering::Relaxed);
    let task_id = cell.task_id.load(Ordering::Relaxed);
    let process = cell.process.load(Ordering::Relaxed);
    let process_id = cell.process_id.load(Ordering::Relaxed);
    let console = cell.console.load(Ordering::Relaxed);
    let pager_context_slot = cell.pager_context_slot.load(Ordering::Relaxed);
    let pager_context_generation = cell.pager_context_generation.load(Ordering::Relaxed);
    let pager_scheduling_domain = cell.pager_scheduling_domain.load(Ordering::Relaxed);
    let pager_domain_generation = cell.pager_domain_generation.load(Ordering::Relaxed);
    let pager_policy_epoch = cell.pager_policy_epoch.load(Ordering::Relaxed);
    let pager_period_ns = cell.pager_period_ns.load(Ordering::Relaxed);
    let dispatch_flags = cell.dispatch_flags.load(Ordering::Relaxed);
    // ORDERING: this fence keeps the field loads above from being reordered
    // after the version re-read that validates them.
    fence(Ordering::Acquire);
    if cell.version.load(Ordering::Relaxed) != before || flags & FLAG_PUBLISHED == 0 {
        return None;
    }
    Some((
        TaskIdentity {
            task_id: (flags & FLAG_TASK_ID != 0).then_some(task_id),
            user_mode: flags & FLAG_USER_MODE != 0,
            abi: decode_abi(flags),
            process_handle: decode_process(process, flags),
            process_id: (flags & FLAG_PROCESS_ID != 0).then_some(process_id),
            console_session: ConsoleSessionHandle::from_raw(console),
            pager_charge: (flags & FLAG_PAGER_CHARGE != 0).then_some(PagerChargeStamp {
                context_slot: pager_context_slot,
                context_generation: pager_context_generation,
                scheduling_domain: pager_scheduling_domain,
                domain_generation: pager_domain_generation,
                policy_epoch: pager_policy_epoch,
                period_ns: pager_period_ns,
            }),
        },
        dispatch_flags,
    ))
}

/// Reads the dispatch-relevant subset of a stable publication. `None` means
/// callers must use their existing locked authority rather than treating the
/// slot as ineligible.
pub(super) fn read_dispatch(slot: usize) -> Option<DispatchPublication> {
    read_state(slot).map(|(identity, dispatch_flags)| DispatchPublication {
        task_id: identity.task_id,
        user_mode: identity.user_mode,
        process_id: identity.process_id,
        pager_charge: identity.pager_charge,
        dispatch_flags,
    })
}

/// Independent telemetry compares the published scalar descriptor against
/// its ORIGINAL Identity51 writer in one bounded leaf. No descriptor value
/// authorizes backing, execution or a new loan; no raw leaf crosses sampling.
pub(super) fn binding_publication_diverges(slot: usize) -> bool {
    with_writer(slot, |writer| !matches_authority(slot, writer.identity, writer.lifecycle))
        .unwrap_or(false)
}
pub(super) fn matches_authority(
    slot: usize,
    authority: Option<TaskIdentity>,
    lifecycle: DispatchLifecycle,
) -> bool {
    match (read_state(slot), authority) {
        (Some((published, dispatch_flags)), Some(authority)) => {
            published == authority && dispatch_flags == lifecycle.flags()
        }
        (None, None) => true,
        // A cleared slot that the scheduler still binds is a missed
        // publication; a published slot the scheduler no longer binds is a
        // missed retirement. Both are faults.
        _ => false,
    }
}

pub(in crate::multitask) mod cohort;
pub(in crate::multitask) mod exec_intent;
pub(in crate::multitask) mod execution;
#[cfg(test)]
mod owner_tests;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_identity_round_trips_and_clears() {
        let _namespace = crate::multitask::scheduler::execution_test_serial_guard();
        let slot = 3;
        let identity = TaskIdentity {
            task_id: Some(41),
            user_mode: true,
            abi: Some(UserAbi::Linux),
            process_handle: Some(ProcessHandle::new(7, 9)),
            process_id: Some(23),
            console_session: ConsoleSessionHandle::from_raw(5),
            pager_charge: Some(PagerChargeStamp {
                context_slot: 4,
                context_generation: 42,
                scheduling_domain: 43,
                domain_generation: 46,
                policy_epoch: 47,
                period_ns: 53,
            }),
        };
        publish_test_identity(slot, identity);
        assert_eq!(read(slot), Some(identity));
        assert!(
            read_dispatch(slot)
                .expect("published dispatch state")
                .lifecycle_dispatchable()
        );
        assert_eq!(
            read(slot).and_then(|read| read.complete_user_log_ids()),
            Some(Some((23, 41)))
        );
        assert_eq!(
            read(slot).and_then(|read| read.user_binding()),
            Some((
                41,
                UserAbi::Linux,
                ProcessHandle::new(7, 9),
                ConsoleSessionHandle::from_raw(5)
            ))
        );
        clear(slot);
        assert_eq!(read(slot), None);
        let cleared = &IDENTITIES[slot];
        assert_eq!(cleared.task_id.load(Ordering::Relaxed), 0);
        assert_eq!(cleared.process_id.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dispatch_lifecycle_is_committed_with_identity() {
        let _namespace = crate::multitask::scheduler::execution_test_serial_guard();
        let slot = 6;
        let identity = TaskIdentity {
            task_id: Some(91),
            user_mode: true,
            abi: Some(UserAbi::Linux),
            process_handle: Some(ProcessHandle::new(4, 7)),
            process_id: Some(12),
            console_session: ConsoleSessionHandle::from_raw(3),
            pager_charge: None,
        };
        publish_state(
            slot,
            identity,
            DispatchLifecycle {
                retired: false,
                suspended: true,
                job_stopped: false,
                exec_quiesced: false,
            },
        );
        assert_eq!(read(slot), Some(identity));
        assert!(
            !read_dispatch(slot)
                .expect("published dispatch state")
                .lifecycle_dispatchable()
        );
        publish_state(
            slot,
            identity,
            DispatchLifecycle {
                job_stopped: true,
                exec_quiesced: true,
                ..DispatchLifecycle::default()
            },
        );
        let dispatch = read_dispatch(slot).expect("complete lifecycle publication");
        assert!(dispatch.job_stopped() && dispatch.exec_quiesced());
        assert!(!dispatch.lifecycle_dispatchable());
        assert!(!binding_publication_diverges(slot));
        // Model a missing final descriptor publication, not backing permission.
        // The actual original writer changes while its last published scalar
        // still describes the old lifecycle; telemetry must expose that split.
        with_writer(slot, |writer| writer.lifecycle.job_stopped = false).unwrap();
        assert!(binding_publication_diverges(slot));
        with_writer(slot, |writer| writer.lifecycle.job_stopped = true).unwrap();
        assert!(!binding_publication_diverges(slot));
        clear(slot);
    }

    #[test]
    fn kernel_task_publishes_no_user_binding() {
        let _namespace = crate::multitask::scheduler::execution_test_serial_guard();
        let slot = 4;
        publish_test_identity(
            slot,
            TaskIdentity {
                task_id: Some(12),
                user_mode: false,
                abi: None,
                process_handle: None,
                process_id: None,
                console_session: ConsoleSessionHandle::SYSTEM,
                pager_charge: None,
            },
        );
        let published = read(slot).expect("published identity");
        assert_eq!(published.task_id, Some(12));
        assert_eq!(published.user_binding(), None);
        clear(slot);
    }

    #[test]
    fn actual_current_pager_fault_keeps_original_bank_identity_during_metadata_publication() {
        use crate::multitask::{current, scheduler::root_creation};
        use rustos_user_abi::pager::*;
        let resolved = core::cell::Cell::new(false);
        let id = 132001;
        root_creation::tests::exec_root(id, true,
            root_creation::tests::bootstrap(UserAbi::Linux), |slot, handle| {
                let (task_id, bound_handle, process) = current::test_current_pager_binding().unwrap();
                assert_eq!((task_id, bound_handle), (id, handle));
                let start = crate::memory::paging::USER_SPACE_BASE + 0x60_000;
                let template = PagerVmRegionWire {
                    start, end: start + PAGER_PAGE_BYTES,
                    object: PagerObjectIdentityWire {
                        object_type: VM_OBJECT_ANONYMOUS, rights: VM_PROT_READ | VM_PROT_WRITE,
                        slot: 17, generation: 19, pager_epoch: 23, backing_generation: 29,
                        ..PagerObjectIdentityWire::default()
                    },
                    prot: VM_PROT_READ | VM_PROT_WRITE, sharing: VM_SHARING_PRIVATE,
                    commit_state: VM_COMMIT_COMMITTED,
                    fault_endpoint: PagerEndpointCapabilityWire { slot: 31, generation: 37, rights: 1 },
                    ..PagerVmRegionWire::default()
                };
                let region = super::super::pager_vma::publish(handle, process, template, 1).unwrap();
                assert!(super::super::pager_vma::lookup(handle, process, start, VM_ACCESS_WRITE).is_ok());
                let cell = &IDENTITIES[slot];
                let before = cell.version.load(Ordering::Relaxed);
                // Native input models the earlier metadata writer interval;
                // the actual born Current bank, process/MM and VMA stay owned.
                cell.version.store(before + 1, Ordering::Release);
                assert!(read(slot).is_none());
                let observed = current::test_current_pager_binding().and_then(|(task, handle, process)| {
                    super::super::pager_vma::lookup(handle, process, start, VM_ACCESS_WRITE)
                        .ok().map(|fault| (task, fault))
                });
                cell.version.store(before, Ordering::Release);
                resolved.set(observed == Some((id, region)));
                super::super::pager_vma::revoke(handle, process, start, region.vma_generation).unwrap();
            });
        assert!(resolved.get(), "odd metadata publication is not an unmapped user page");
    }

    #[test]
    fn incomplete_user_or_odd_publication_requires_locked_log_lookup() {
        let _namespace = crate::multitask::scheduler::execution_test_serial_guard();
        let slot = 5;
        clear(slot);
        assert_eq!(
            read(slot).and_then(|identity| identity.complete_user_log_ids()),
            None
        );

        let incomplete = TaskIdentity {
            task_id: Some(73),
            user_mode: true,
            abi: Some(UserAbi::Linux),
            process_handle: Some(ProcessHandle::new(2, 4)),
            process_id: None,
            console_session: ConsoleSessionHandle::from_raw(7),
            pager_charge: None,
        };
        publish_test_identity(slot, incomplete);
        assert_eq!(
            read(slot).and_then(|identity| identity.complete_user_log_ids()),
            None
        );

        let complete = TaskIdentity {
            process_id: Some(31),
            ..incomplete
        };
        publish_test_identity(slot, complete);
        let cell = &IDENTITIES[slot];
        let stable_version = cell.version.load(Ordering::Relaxed);
        cell.version
            .store(stable_version.wrapping_add(1), Ordering::Relaxed);
        assert_eq!(
            read(slot).and_then(|identity| identity.complete_user_log_ids()),
            None
        );
        cell.version.store(stable_version, Ordering::Relaxed);
        clear(slot);
    }

    #[test]
    fn unpublished_slot_reads_as_absent() {
        let _process = crate::multitask::process_table::tests::isolate_process_table();
        let _cpu = crate::multitask::cpu_local::test_publication_lock();
        let _runtime = crate::multitask::scheduler::execution_test_serial_guard();
        clear(MAX_SCHEDULER_TASKS - 1);
        assert_eq!(read(MAX_SCHEDULER_TASKS - 1), None);
        assert_eq!(read(MAX_SCHEDULER_TASKS), None);
    }
}
