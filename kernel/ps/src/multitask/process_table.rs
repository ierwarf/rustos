//! Generational process identity and address-space lifetime registry.
//!
//! - **Owner:** `kernel-ps` owns process generations and final retirement.
//! - **Boundary:** PIDs are labels; only a live `ProcessHandle` generation
//!   authorizes mutation or retained address-space access.
//! - **Lifecycle:** Reserve, publish, attach tasks, mark exiting, freeze final
//!   state, queue reap, and reclaim exactly once.
//! - **Concurrency:** Registry mutation is serialized; fallible allocation and
//!   cross-subsystem cleanup occur outside raw critical sections.
//! - **Failure:** Stale handles, exit races, capacity exhaustion, and duplicate
//!   retirement reject without aliasing a reused PID/slot.
//! - **Forbidden:** No “missing means exited,” leader-exit equals process-exit,
//!   or current-task state used to clean a foreign retired task.
//! - **Evidence:** `process-address-space-lifecycle`, `endpoint-lifecycle`, and
//!   `kernel-resource-lifecycle`.
use alloc::boxed::Box;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kernel_object::api::identity::{ObjectIdentity, ObjectKind, ObjectOwner};
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

use super::process_state_lock::ProcessStateLock;
use super::slot_dir::{ChunkTable, SLOT_CEILING};
use crate::memory::paging::ProcessAddressSpace;
use crate::user::process_state::UserProcessState;

/// Process slots the system admits until policy says otherwise. This is the
/// boot default of the root job's `processes.max`, not a structural limit: the
/// slot storage is chunked and grows on demand up to `slot_dir::SLOT_CEILING`.
pub const DEFAULT_PROCESS_LIMIT: usize = 32;
static PROCESS_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_PROCESS_LIMIT);
/// Deferred reap handles held before the reaper falls back to a full scan.
const REAP_QUEUE_CAPACITY: usize = 64;
/// Processes reclaimed per table-lock hold by the reaper.
const REAP_BATCH: usize = 32;
/// Slots examined per table-lock hold during a full reap scan.
const REAP_SCAN_STRIDE: usize = 256;

/// The admitted number of process slots (the root job's `processes.max`).
pub fn process_limit() -> usize {
    PROCESS_LIMIT.load(Ordering::Relaxed)
}

/// Sets the admitted number of process slots, clamped to what the slot storage
/// can hold. Lowering it never evicts a live process: it only stops new slots
/// beyond the limit from being handed out. Returns the value now in force.
pub fn set_process_limit(limit: usize) -> usize {
    let limit = limit.clamp(1, SLOT_CEILING);
    PROCESS_LIMIT.store(limit, Ordering::Relaxed);
    limit
}
pub const MAX_THREADS_PER_PROCESS: usize = 32;
type ProcessTableLock = TrackedSpinLock<ProcessTable, { LockClass::ProcessTable as u8 }>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildStateChange {
    Stopped(u8),
    Continued,
}

/// Generation-bound authority for one exec ownership transfer.
///
/// `begin_exec` seals thread attachment. `authorize_exec` is the linearization
/// point against process exit, and a scheduler commit carrying an authorized
/// reservation must be followed by an infallible process-state ownership
/// transfer even if exit is published immediately afterward.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecReservation {
    handle: ProcessHandle,
    expected_mm_generation: u32,
    next_mm_generation: u32,
    transaction_id: u32,
}

impl ExecReservation {
    /// One-shot lifecycle-token identity for this exact exec transaction.
    ///
    /// `handle` remains the process-table generation authority, while this
    /// token slot encodes the process-table slot and process generation, while
    /// its generation is a non-reusable transaction ID. The expected/next MM
    /// generations remain explicit fields, so none of the three identities is
    /// collapsed into another. Callers must still authorize or consume the
    /// reservation through the process table.
    #[cfg(test)]
    pub fn object_identity(self) -> Option<ObjectIdentity> {
        ObjectIdentity::new(
            ObjectOwner::Ps,
            ObjectKind::LifecycleToken,
            self.handle.lifecycle_token_slot()?,
            u64::from(self.transaction_id),
        )
    }

    #[cfg(test)]
    pub const fn transaction_id(self) -> u32 {
        self.transaction_id
    }

    pub const fn process_handle(self) -> ProcessHandle {
        self.handle
    }

    #[cfg(test)]
    pub const fn expected_mm_generation(self) -> u32 {
        self.expected_mm_generation
    }

    #[cfg(test)]
    pub const fn next_mm_generation(self) -> u32 {
        self.next_mm_generation
    }
}

pub struct StagedExec {
    reservation: ExecReservation,
    new_root: u64,
    new_mm: MmExecutionPin,
    process_id: u64,
    closed: alloc::vec::Vec<crate::user::handles::KernelHandle>,
    old_state: UserProcessState,
}

impl StagedExec {
    pub(in crate::multitask) fn pin_execution_mm(
        &self,
        reservation: ExecReservation,
        root: u64,
    ) -> MmExecutionPin {
        assert_eq!(
            self.reservation, reservation,
            "foreign staged MM reservation"
        );
        assert_eq!(self.new_root, root, "foreign staged MM root");
        self.new_mm
            .try_clone()
            .expect("staged MM closed before image publication")
    }
}

pub struct ExecFinalize {
    intent: Option<super::current_identity::exec_intent::Credential>,
    process_id: u64,
    exit_pending: bool,
    closed: alloc::vec::Vec<crate::user::handles::KernelHandle>,
    old_state: UserProcessState,
}

impl ExecFinalize {
    pub(in crate::multitask) fn intent(
        &self,
    ) -> Option<super::current_identity::exec_intent::Credential> {
        self.intent
    }
    pub(in crate::multitask) fn exit_pending(&self) -> bool {
        self.exit_pending
    }
    pub fn into_parts(
        self,
    ) -> (
        u64,
        bool,
        alloc::vec::Vec<crate::user::handles::KernelHandle>,
        UserProcessState,
    ) {
        (
            self.process_id,
            self.exit_pending,
            self.closed,
            self.old_state,
        )
    }
}

struct ProcessObject {
    process_id: u64,
    parent_process_id: Option<u64>,
    ref_count: usize,
    thread_count: usize,
    mm_generation: u32,
    mm_execution: kernel_mm::api::address_space::ExecutionPin,
    lifecycle_transaction_id: u32,
    pending_exit_transaction_id: Option<u32>,
    exec_in_progress: bool,
    exec_commit_authorized: bool,
    exec_state_staged: bool,
    exit_pending: bool,
    exiting: bool,
    queued_for_reap: bool,
    exit_status: Option<i32>,
    child_state_change: Option<ChildStateChange>,
    waited: bool,
    /// The job this process was created into. Fixed for the process's life.
    job: jobs::JobId,
    state: ProcessStateLock<UserProcessState>,
}

impl ProcessObject {
    fn new(
        process_id: u64,
        parent_process_id: Option<u64>,
        state: UserProcessState,
        lifecycle_transaction_id: u32,
        job: jobs::JobId,
    ) -> Self {
        Self {
            process_id,
            parent_process_id,
            ref_count: 1,
            thread_count: 1,
            mm_generation: 1,
            mm_execution: state
                .address_space()
                .pin_execution()
                .expect("new process missing MM execution owner"),
            lifecycle_transaction_id,
            pending_exit_transaction_id: None,
            exec_in_progress: false,
            exec_commit_authorized: false,
            exec_state_staged: false,
            exit_pending: false,
            exiting: false,
            queued_for_reap: false,
            exit_status: None,
            child_state_change: None,
            waited: parent_process_id.is_none(),
            job,
            state: ProcessStateLock::new(state),
        }
    }

    fn state_ptr(&self) -> NonNull<ProcessStateLock<UserProcessState>> {
        NonNull::from(&self.state)
    }
}

struct ProcessSlot {
    generation: u32,
    object: Option<Box<ProcessObject>>,
    spawn_transaction_id: Option<u32>,
    root_spawn: Option<spawn::RootSpawnKey>,
}

impl ProcessSlot {
    const fn empty() -> Self {
        Self {
            generation: 1,
            object: None,
            spawn_transaction_id: None,
            root_spawn: None,
        }
    }
}

struct ProcessTable {
    slots: ChunkTable<ProcessSlot>,
    jobs: [jobs::JobSlot; jobs::JOB_SLOTS],
    /// Kernel-owned exact-token resource custody. This survives a rootd
    /// process crash and cannot be forged by a guessed PID/JobId.
    session_tokens: [jobs::SessionToken; jobs::SESSION_TOKEN_SLOTS],
    session_owners: [jobs::SessionOwner; jobs::SESSION_OWNER_SLOTS],
    reap_queue: [Option<ProcessHandle>; REAP_QUEUE_CAPACITY],
    reap_len: usize,
    reap_scan_pending: bool,
}

impl ProcessTable {
    const fn new() -> Self {
        Self {
            slots: ChunkTable::new(),
            jobs: {
                let mut jobs = [const { jobs::JobSlot::empty() }; jobs::JOB_SLOTS];
                jobs[0] = jobs::JobSlot::root();
                jobs
            },
            session_tokens: [jobs::SessionToken::EMPTY; jobs::SESSION_TOKEN_SLOTS],
            session_owners: [jobs::SessionOwner::EMPTY; jobs::SESSION_OWNER_SLOTS],
            reap_queue: [None; REAP_QUEUE_CAPACITY],
            reap_len: 0,
            reap_scan_pending: false,
        }
    }

    fn lookup_slot_mut(&mut self, handle: ProcessHandle) -> Option<&mut ProcessSlot> {
        let slot = self.slots.get_mut(handle.index())?;
        (slot.generation == handle.generation()).then_some(slot)
    }

    fn lookup_object_mut(&mut self, handle: ProcessHandle) -> Option<&mut ProcessObject> {
        self.lookup_slot_mut(handle)?.object.as_deref_mut()
    }

    fn next_generation(current: u32) -> Option<u32> {
        current.checked_add(1).filter(|next| *next != 0)
    }

    fn push_reap_handle(&mut self, handle: ProcessHandle) {
        if self.reap_len >= self.reap_queue.len() {
            self.reap_scan_pending = true;
            super::kick_housekeeping();
            return;
        }
        self.reap_queue[self.reap_len] = Some(handle);
        self.reap_len += 1;
        super::kick_housekeeping();
    }
}

mod identity;
pub mod jobs;
mod mm_execution;
pub(in crate::multitask) mod pin_owner;
pub(super) use identity::{published_live_process_identity, published_process_job};
#[cfg(test)]
use identity::{clear_publication_for_test, process_state_is_visible};
pub use identity::{
    live_process_identity, live_process_identity_by_pid, retain_process, retain_process_by_pid,
    ProcessHandle, ProcessIdentity, ProcessRef,
};
pub(super) use identity::{
    own_process_ref, pin_process_object, publication_divergence_count, with_own_visible_state,
};
pub(in crate::multitask) use mm_execution::{pin_process_mm, MmActivationPin, MmExecutionPin};

/// Keep the exact-state lock bridge as an explicit process-table API instead
/// of leaking the private identity module through a re-export.
pub(super) fn with_own_exact_visible_state<R>(
    handle: ProcessHandle,
    expected: ProcessIdentity,
    f: impl FnOnce(&UserProcessState) -> R,
) -> Option<R> {
    identity::with_own_exact_visible_state(handle, expected, f)
}

pub(in crate::multitask) mod spawn;
pub use spawn::{cancel_spawn, reserve_spawn, reserve_spawn_in, SpawnReservation};
#[cfg(test)]
pub use spawn::{publish_spawn};

static PROCESS_TABLE: ProcessTableLock = ProcessTableLock::new(ProcessTable::new());

/// One namespace for spawn/exec/exit/reap transactions. IDs never wrap: after
/// the final u32 identity is consumed, new lifecycle admission fails closed
/// instead of letting a stale marker or one-shot token alias later work.
static NEXT_LIFECYCLE_TRANSACTION_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_lifecycle_transaction_id() -> Option<u32> {
    allocate_nonwrapping_lifecycle_transaction_id(&NEXT_LIFECYCLE_TRANSACTION_ID)
}

fn allocate_nonwrapping_lifecycle_transaction_id(counter: &AtomicU64) -> Option<u32> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0 && current <= u64::from(u32::MAX)).then(|| current + 1)
        })
        .ok()
        .and_then(|current| u32::try_from(current).ok())
}

/// A fixed lifecycle marker encodes all authority-bearing fields without
/// relying on PID labels: arg0 is `(process_generation << 32) | one_based_slot`
/// and arg1 is `(mm_generation << 32) | transaction_id`.
#[cfg(rustos_lifecycle_trace)]
fn record_lifecycle_marker(
    name: &'static str,
    handle: ProcessHandle,
    mm_generation: u32,
    transaction_id: u32,
) {
    let Some(process_token) = handle.lifecycle_token_slot() else {
        return;
    };
    let mm_transaction = (u64::from(mm_generation) << 32) | u64::from(transaction_id);
    crate::debug::record_milestone(
        crate::debug::LogCategory::Process,
        name,
        process_token,
        mm_transaction,
    );
}

#[cfg(not(rustos_lifecycle_trace))]
#[inline]
fn record_lifecycle_marker(
    _name: &'static str,
    _handle: ProcessHandle,
    _mm_generation: u32,
    _transaction_id: u32,
) {
}

fn publish_slot_visibility(index: usize, slot: &ProcessSlot) {
    let publication = slot
        .object
        .as_deref()
        .map(|object| identity::SlotPublication {
            process_id: object.process_id,
            job: object.job,
            mm_generation: object.mm_generation,
            live: !object.exiting && !object.exec_in_progress && !object.exec_state_staged,
            state: object.state_ptr().as_ptr(),
        });
    identity::publish_slot_visibility(index, slot.generation, publication);
}

fn reclaim_slot(index: usize, slot: &mut ProcessSlot) -> Option<Box<ProcessObject>> {
    let object = slot.object.as_mut()?;
    if object.ref_count != 0 || object.thread_count != 0 {
        object.queued_for_reap = false;
        return None;
    }
    if object.exit_status.is_some() && !object.waited {
        object.queued_for_reap = false;
        return None;
    }

    // Close-before-zero prevents a lock-free reader from entering between
    // the reaper's last reference observation and pointer revocation.
    if !pin_owner::close(index, slot.generation) {
        return None;
    }
    let object = slot.object.take()?;
    // Generation exhaustion permanently retires this slot. Reusing generation
    // one would let a stale ProcessHandle alias a new process.
    slot.generation = ProcessTable::next_generation(slot.generation).unwrap_or(0);
    publish_slot_visibility(index, slot);
    Some(object)
}

#[cfg(test)]
pub fn create_process(process_id: u64, state: UserProcessState) -> Option<ProcessHandle> {
    create_process_with_parent(process_id, None, state)
}

#[cfg(test)]
pub fn create_process_with_parent(
    process_id: u64,
    parent_process_id: Option<u64>,
    state: UserProcessState,
) -> Option<ProcessHandle> {
    let reservation = reserve_spawn()?;
    let handle = publish_spawn(reservation, process_id, parent_process_id, state);
    if handle.is_none() {
        let _ = cancel_spawn(reservation);
    }
    handle
}

pub fn attach_task(handle: ProcessHandle) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    if !pin_owner::is_open(handle) {
        return None;
    }
    let job_index = {
        let object = table.lookup_object_mut(handle)?;
        if object.exiting || object.exec_in_progress {
            return None;
        }
        if object.thread_count >= MAX_THREADS_PER_PROCESS {
            return None;
        }
        object.job.index()
    };
    // The per-process ceiling above is structural; this is the job's threads
    // limit, checked against every ancestor under the same lock as the count.
    if !table.admits_thread(job_index) {
        return None;
    }
    let object = table.lookup_object_mut(handle)?;
    let ref_count = object.ref_count.checked_add(1)?;
    let thread_count = object.thread_count.checked_add(1)?;
    object.ref_count = ref_count;
    object.thread_count = thread_count;
    Some(())
}

pub fn detach_task(handle: ProcessHandle) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let (should_queue, exit_marker) = {
        let object = table.lookup_object_mut(handle)?;
        if object.thread_count == 0 || object.ref_count == 0 {
            return Some(());
        }
        let final_thread_starts_exit = object.thread_count == 1 && !object.exiting;
        if final_thread_starts_exit {
            object.lifecycle_transaction_id = allocate_lifecycle_transaction_id()?;
        }
        object.thread_count -= 1;
        object.ref_count -= 1;
        if object.thread_count == 0 {
            object.exiting = true;
        }
        let should_queue =
            object.thread_count == 0 && object.ref_count == 0 && !object.queued_for_reap;
        if should_queue {
            object.queued_for_reap = true;
        }
        (
            should_queue,
            final_thread_starts_exit
                .then_some((object.mm_generation, object.lifecycle_transaction_id)),
        )
    };
    publish_slot_visibility(handle.index(), &table.slots[handle.index()]);
    if should_queue {
        table.push_reap_handle(handle);
    }
    drop(table);
    if let Some((mm_generation, transaction_id)) = exit_marker {
        record_lifecycle_marker(
            "lifecycle-exit-final-thread",
            handle,
            mm_generation,
            transaction_id,
        );
    }
    Some(())
}

pub fn parent_process_id_of(process_id: u64) -> Option<u64> {
    let table = PROCESS_TABLE.lock();
    for slot in table.slots.iter() {
        let Some(object) = slot.object.as_deref() else {
            continue;
        };
        if object.process_id == process_id {
            return object.parent_process_id;
        }
    }
    None
}

/// One live process in a memory census.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryCensusEntry {
    pub process_id: u64,
    pub parent_process_id: u64,
    /// Mapped data leaves in the address space, shared aliases included.
    pub resident_pages: u64,
    /// Raw `JobId` of the process's job.
    pub job: u64,
}

/// Inserts `entry` into `out[..len]`, kept ordered by `resident_pages`
/// (largest first, then larger `process_id`), dropping the smallest when full.
/// Returns the new length.
fn insert_largest(out: &mut [MemoryCensusEntry], len: usize, entry: MemoryCensusEntry) -> usize {
    let outranks = |a: &MemoryCensusEntry, b: &MemoryCensusEntry| {
        (a.resident_pages, a.process_id) > (b.resident_pages, b.process_id)
    };
    let mut at = len;
    while at > 0 && outranks(&entry, &out[at - 1]) {
        at -= 1;
    }
    if at == out.len() {
        return len;
    }
    let new_len = (len + 1).min(out.len());
    let mut index = new_len - 1;
    while index > at {
        out[index] = out[index - 1];
        index -= 1;
    }
    out[at] = entry;
    new_len
}

/// Fills `out` with the live, non-exiting processes that map the most memory,
/// largest first, and returns how many entries were written.
///
/// This allocates nothing and reads each resident count from the frame
/// descriptor ledger without taking any process-state lock, so it may run
/// while a process is faulting, exiting, or being retired. A root that has
/// already been unregistered counts as zero pages.
pub fn memory_census(out: &mut [MemoryCensusEntry]) -> usize {
    let table = PROCESS_TABLE.lock();
    let mut len = 0;
    for slot in table.slots.iter() {
        let Some(object) = slot.object.as_deref() else {
            continue;
        };
        if object.exiting || object.exit_pending {
            continue;
        }
        let resident_pages =
            kernel_mm::api::phys::data_leaf_count(object.mm_execution.root_phys()).unwrap_or(0);
        len = insert_largest(
            out,
            len,
            MemoryCensusEntry {
                process_id: object.process_id,
                parent_process_id: object.parent_process_id.unwrap_or(0),
                resident_pages,
                job: object.job.to_raw(),
            },
        );
    }
    len
}

pub enum WaitResult {
    Exited { pid: u64, status: i32 },
    StateChanged { pid: u64, status: i32 },
    Pending,
    NoMatchingChild,
}

pub fn with_process_state<R>(
    handle: ProcessHandle,
    f: impl FnOnce(u64, &UserProcessState) -> R,
) -> Option<R> {
    let process = retain_process(handle)?;
    process.with_visible_state(f)
}

pub fn with_process_state_mut<R>(
    handle: ProcessHandle,
    f: impl FnOnce(u64, &mut UserProcessState) -> R,
) -> Option<R> {
    let process = retain_process(handle)?;
    process.with_visible_state_mut(f)
}

/// Executes one nonblocking process-state mutation.
///
/// Exception recovery uses this instead of waiting on a task-owned lock: a
/// page fault may be interrupted by remote exec/exit, and parking the faulting
/// task before its exception frame has a committed continuation would lose the
/// sole recovery owner. `None` means either a stale process generation or
/// lock contention; both make the user fault fail closed without panicking the
/// kernel.
#[cfg(test)]
pub fn try_with_process_state_mut<R>(
    handle: ProcessHandle,
    f: impl FnOnce(u64, &mut UserProcessState) -> R,
) -> Option<R> {
    let process = retain_process(handle)?;
    process.try_with_state_mut(f)
}

pub fn with_process_state_by_pid_mut<R>(
    process_id: u64,
    f: impl FnOnce(&mut UserProcessState) -> R,
) -> Option<R> {
    let process = retain_process_by_pid(process_id)?;
    process.with_visible_state_mut(|_, state| f(state))
}

pub fn with_process_state_by_pid<R>(
    process_id: u64,
    f: impl FnOnce(&UserProcessState) -> R,
) -> Option<R> {
    let process = retain_process_by_pid(process_id)?;
    process.with_visible_state(|_, state| f(state))
}

/// Why staging a new exec image was refused. `SealBusy` is transient: the old
/// address space still has an activation grant in flight and closing it would
/// strand the process, so the caller must cancel before any image effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageRefusal {
    /// The new address space could not take an execution pin.
    NewImagePin,
    /// The process generation is no longer retainable.
    ProcessGone,
    /// The exec reservation no longer authorizes the transfer.
    Transfer,
    /// The old address space refused to close admission (grant in flight).
    SealBusy,
}

pub fn stage_exec_state(
    reservation: ExecReservation,
    address_space: ProcessAddressSpace,
    linux_process_state: crate::user::linux::LinuxProcessState,
    linux_memory_map: crate::user::linux::LinuxMemoryMapState,
    linux_runtime_profile: crate::user::linux::LinuxRuntimeProfile,
    exec_path: &str,
) -> Option<StagedExec> {
    stage_exec_state_checked(
        reservation,
        address_space,
        linux_process_state,
        linux_memory_map,
        linux_runtime_profile,
        exec_path,
    )
    .ok()
}

pub fn stage_exec_state_checked(
    reservation: ExecReservation,
    address_space: ProcessAddressSpace,
    linux_process_state: crate::user::linux::LinuxProcessState,
    linux_memory_map: crate::user::linux::LinuxMemoryMapState,
    linux_runtime_profile: crate::user::linux::LinuxRuntimeProfile,
    exec_path: &str,
) -> Result<StagedExec, StageRefusal> {
    let new_root = address_space.root_phys().as_u64();
    let new_mm = MmExecutionPin::new(
        reservation.handle,
        reservation.next_mm_generation,
        address_space
            .pin_execution()
            .ok_or(StageRefusal::NewImagePin)?,
    );
    let process = retain_process(reservation.handle).ok_or(StageRefusal::ProcessGone)?;
    let staged = process.with_state_mut(|process_id, state| {
        {
            let mut table = PROCESS_TABLE.lock();
            let Some(object) = table.lookup_object_mut(reservation.handle) else {
                return Err(StageRefusal::ProcessGone);
            };
            if !exec_commit_may_transfer(object, reservation) {
                return Err(StageRefusal::Transfer);
            }
        }

        if !state.address_space().seal_execution() {
            return Err(StageRefusal::SealBusy);
        }
        let (closed, old_state) = state.replace_for_exec(
            address_space,
            linux_process_state,
            linux_memory_map,
            linux_runtime_profile,
            exec_path,
        );

        {
            let mut table = PROCESS_TABLE.lock();
            let object = table
                .lookup_object_mut(reservation.handle)
                .expect("authorized exec process disappeared before generation publication");
            assert!(
                exec_commit_may_transfer(object, reservation),
                "authorized exec reservation changed before generation publication"
            );
            object.mm_execution = new_mm.pin_for_publication();
            object.mm_generation = reservation.next_mm_generation;
            // The new image's root is quiescent until the exec is installed, so
            // it can be bound and its already-mapped pages charged to the job.
            let _ = kernel_mm::api::phys::bind_root_charge(new_root, object.job.index() as usize);
            object.exec_state_staged = true;
            let index = reservation.handle.index();
            let slot = &table.slots[index];
            publish_slot_visibility(index, slot);
        }
        Ok(StagedExec {
            reservation,
            new_root,
            new_mm,
            process_id,
            closed,
            old_state,
        })
    });
    if staged.is_ok() {
        record_lifecycle_marker(
            "lifecycle-exec-stage",
            reservation.handle,
            reservation.next_mm_generation,
            reservation.transaction_id,
        );
    }
    staged
}

pub(super) fn finalize_exec_state(
    staged: StagedExec,
    publication: super::scheduler::exec_transaction::ExecPublication,
) -> ExecFinalize {
    publication.assert_matches(staged.reservation, staged.new_root);
    let exit_pending = {
        let mut table = PROCESS_TABLE.lock();
        let object = table
            .lookup_object_mut(staged.reservation.handle)
            .expect("published exec process disappeared before finalization");
        assert!(
            object.exec_in_progress
                && object.exec_commit_authorized
                && object.exec_state_staged
                && object.lifecycle_transaction_id == staged.reservation.transaction_id
                && object.mm_generation == staged.reservation.next_mm_generation,
            "published exec transaction lost exact generation ownership"
        );
        object.exec_in_progress = false;
        object.exec_commit_authorized = false;
        object.exec_state_staged = false;
        let exit_pending = core::mem::take(&mut object.exit_pending);
        if exit_pending {
            object.lifecycle_transaction_id = object
                .pending_exit_transaction_id
                .take()
                .expect("exec exit-pending latch lost its transaction identity");
            object.exiting = true;
        }
        let index = staged.reservation.handle.index();
        publish_slot_visibility(index, &table.slots[index]);
        exit_pending
    };
    record_lifecycle_marker(
        if exit_pending {
            "lifecycle-exec-publish-exit-pending"
        } else {
            "lifecycle-exec-publish"
        },
        staged.reservation.handle,
        staged.reservation.next_mm_generation,
        staged.reservation.transaction_id,
    );
    ExecFinalize {
        intent: publication.intent(),
        process_id: staged.process_id,
        exit_pending,
        closed: staged.closed,
        old_state: staged.old_state,
    }
}

fn exec_may_replace(object: &ProcessObject) -> bool {
    object.exec_in_progress
        && !object.exec_commit_authorized
        && !object.exiting
        && object.thread_count == 1
}

fn exec_reservation_matches(object: &ProcessObject, reservation: ExecReservation) -> bool {
    object.exec_in_progress
        && object.lifecycle_transaction_id == reservation.transaction_id
        && object.mm_generation == reservation.expected_mm_generation
        && ProcessTable::next_generation(object.mm_generation)
            == Some(reservation.next_mm_generation)
}

fn exec_commit_may_transfer(object: &ProcessObject, reservation: ExecReservation) -> bool {
    exec_reservation_matches(object, reservation)
        && object.exec_commit_authorized
        && !object.exec_state_staged
        && !object.exiting
}

pub fn begin_exec(handle: ProcessHandle) -> Option<ExecReservation> {
    let mut table = PROCESS_TABLE.lock();
    let object = table.lookup_object_mut(handle)?;
    if object.exiting || object.exec_in_progress {
        return None;
    }
    let next_mm_generation = ProcessTable::next_generation(object.mm_generation)?;
    let transaction_id = allocate_lifecycle_transaction_id()?;
    object.exec_in_progress = true;
    object.exec_commit_authorized = false;
    object.lifecycle_transaction_id = transaction_id;
    let reservation = ExecReservation {
        handle,
        expected_mm_generation: object.mm_generation,
        next_mm_generation,
        transaction_id,
    };
    publish_slot_visibility(handle.index(), &table.slots[handle.index()]);
    drop(table);
    record_lifecycle_marker(
        "lifecycle-exec-reserve",
        handle,
        reservation.expected_mm_generation,
        transaction_id,
    );
    Some(reservation)
}

pub fn authorize_exec(reservation: ExecReservation) -> bool {
    let mut table = PROCESS_TABLE.lock();
    let Some(object) = table.lookup_object_mut(reservation.handle) else {
        return false;
    };
    if !exec_reservation_matches(object, reservation) || !exec_may_replace(object) {
        return false;
    }
    object.exec_commit_authorized = true;
    drop(table);
    record_lifecycle_marker(
        "lifecycle-exec-authorize",
        reservation.handle,
        reservation.expected_mm_generation,
        reservation.transaction_id,
    );
    true
}

pub fn cancel_exec(reservation: ExecReservation) -> bool {
    cancel_exec_owned(reservation).is_some()
}

/// Non-Copy evidence that this exact process reservation was cancelled.
pub(in crate::multitask) struct ExecCancellation {
    reservation: ExecReservation,
    terminal: bool,
}
impl ExecCancellation {
    pub(in crate::multitask) fn terminal(&self) -> bool {
        self.terminal
    }
    pub(in crate::multitask) fn matches(&self, reservation: ExecReservation) -> bool {
        self.reservation == reservation
    }
}
pub(in crate::multitask) fn cancel_exec_owned(
    reservation: ExecReservation,
) -> Option<ExecCancellation> {
    let mut table = PROCESS_TABLE.lock();
    let Some(object) = table.lookup_object_mut(reservation.handle) else {
        return None;
    };
    if !exec_reservation_matches(object, reservation) {
        return None;
    }
    if object.exec_state_staged {
        panic!("exec invariant: staged process state cannot be cancelled after commit began");
    }
    object.exec_in_progress = false;
    object.exec_commit_authorized = false;
    object.exec_state_staged = false;
    let exit_transaction = if core::mem::take(&mut object.exit_pending) {
        let transaction_id = object
            .pending_exit_transaction_id
            .take()
            .expect("exec cancellation lost pending exit transaction identity");
        object.lifecycle_transaction_id = transaction_id;
        object.exiting = true;
        Some((object.mm_generation, transaction_id))
    } else {
        None
    };
    let terminal = object.exiting;
    publish_slot_visibility(
        reservation.handle.index(),
        &table.slots[reservation.handle.index()],
    );
    drop(table);
    record_lifecycle_marker(
        "lifecycle-exec-cancel",
        reservation.handle,
        reservation.expected_mm_generation,
        reservation.transaction_id,
    );
    if let Some((mm_generation, transaction_id)) = exit_transaction {
        record_lifecycle_marker(
            "lifecycle-exit-after-exec-cancel",
            reservation.handle,
            mm_generation,
            transaction_id,
        );
    }
    Some(ExecCancellation {
        reservation,
        terminal,
    })
}

#[cfg(test)]
pub(in crate::multitask) fn test_exec_cancellation(
    reservation: ExecReservation,
) -> ExecCancellation {
    ExecCancellation {
        reservation,
        terminal: false,
    }
}

pub fn note_process_exit_status(process_id: u64, status: i32) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let index = table.slots.iter().position(|slot| {
        slot.object
            .as_deref()
            .is_some_and(|object| object.process_id == process_id)
    })?;
    let object = table.slots[index].object.as_deref_mut()?;
    object.exit_status = Some(status);
    object.child_state_change = None;
    object.exiting = true;
    let parent = object.parent_process_id;
    publish_slot_visibility(index, &table.slots[index]);
    drop(table);
    // Publish first, wake second: a parent parked in `wait4` re-runs
    // `wait_for_child` and finds this status.
    if let Some(parent) = parent {
        super::wake_child_waiters(parent);
    }
    Some(())
}

/// Record child state under the original census' physical process pin. The
/// parent PID is a notification hint, not authority over a parent allocation.
/// No process-table leaf spans recipient acquisition or signal/wake work.
pub(in crate::multitask) fn note_job_control(
    pin: &pin_owner::ProcessObjectPin,
    process_id: u64,
    stop_signal: Option<u8>,
) -> Option<Option<u64>> {
    let handle = pin.handle();
    let mut table = PROCESS_TABLE.lock();
    let slot = table.slots.get_mut(handle.index())?;
    if slot.generation != handle.generation() { return None; }
    let object = slot.object.as_deref_mut()?;
    if object.process_id != process_id || object.exiting { return None; }
    object.child_state_change = Some(match stop_signal {
        Some(signal) => ChildStateChange::Stopped(signal),
        None => ChildStateChange::Continued,
    });
    Some(object.parent_process_id)
}

/// Mark a process as exiting before callers tear down resources that it owns.
/// The object stays available until its tasks retire and its parent reaps it.
pub fn mark_process_exiting(process_id: u64) -> Option<()> {
    mark_process_exiting_inner(process_id, None)
}

/// Captured group commands must name the exact physical process generation,
/// not silently recover a foreign allocation through a copied PID.
pub(in crate::multitask) fn mark_process_exiting_for_handle(
    handle: ProcessHandle,
    process_id: u64,
) -> Option<()> {
    mark_process_exiting_inner(process_id, Some(handle))
}

fn mark_process_exiting_inner(process_id: u64, expected: Option<ProcessHandle>) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let index = if let Some(handle) = expected {
        let slot = table.slots.get(handle.index())?;
        if slot.generation != handle.generation()
            || !slot
                .object
                .as_deref()
                .is_some_and(|o| o.process_id == process_id)
        {
            return None;
        }
        handle.index()
    } else {
        table.slots.iter().position(|slot| {
            slot.object
                .as_deref()
                .is_some_and(|object| object.process_id == process_id)
        })?
    };
    let handle = ProcessHandle::new(index, table.slots[index].generation);
    let object = table.slots[index].object.as_deref_mut()?;
    if object.exec_in_progress {
        let first = !object.exit_pending;
        if first {
            object.pending_exit_transaction_id = Some(allocate_lifecycle_transaction_id()?);
        }
        object.exit_pending = true;
        let marker = first.then_some((object.mm_generation, object.pending_exit_transaction_id?));
        publish_slot_visibility(handle.index(), &table.slots[handle.index()]);
        drop(table);
        if let Some((mm_generation, transaction_id)) = marker {
            record_lifecycle_marker(
                "lifecycle-exit-pending",
                handle,
                mm_generation,
                transaction_id,
            );
        }
        return None;
    }
    if !object.exiting {
        object.lifecycle_transaction_id = allocate_lifecycle_transaction_id()?;
    }
    object.exiting = true;
    let marker = (object.mm_generation, object.lifecycle_transaction_id);
    publish_slot_visibility(handle.index(), &table.slots[handle.index()]);
    drop(table);
    record_lifecycle_marker("lifecycle-exit-seal", handle, marker.0, marker.1);
    Some(())
}

/// Atomically marks the process exiting and reports whether this call won the
/// transition. Resource owners use the `true` result to perform exactly-once
/// service-side open-description teardown under concurrent exit_group calls.
pub fn mark_process_exiting_once(process_id: u64) -> Option<bool> {
    let mut table = PROCESS_TABLE.lock();
    let index = table.slots.iter().position(|slot| {
        slot.object
            .as_deref()
            .is_some_and(|object| object.process_id == process_id)
    })?;
    let handle = ProcessHandle::new(index, table.slots[index].generation);
    let object = table.slots[index].object.as_deref_mut()?;
    if object.exec_in_progress {
        let first = !object.exit_pending;
        if first {
            object.pending_exit_transaction_id = Some(allocate_lifecycle_transaction_id()?);
        }
        object.exit_pending = true;
        let marker = first.then_some((object.mm_generation, object.pending_exit_transaction_id?));
        publish_slot_visibility(handle.index(), &table.slots[handle.index()]);
        drop(table);
        if let Some((mm_generation, transaction_id)) = marker {
            record_lifecycle_marker(
                "lifecycle-exit-pending",
                handle,
                mm_generation,
                transaction_id,
            );
        }
        return Some(false);
    }
    let first = !object.exiting;
    if first {
        object.lifecycle_transaction_id = allocate_lifecycle_transaction_id()?;
    }
    object.exiting = true;
    let marker = first.then_some((object.mm_generation, object.lifecycle_transaction_id));
    publish_slot_visibility(handle.index(), &table.slots[handle.index()]);
    drop(table);
    if let Some((mm_generation, transaction_id)) = marker {
        record_lifecycle_marker("lifecycle-exit-seal", handle, mm_generation, transaction_id);
    }
    Some(first)
}

pub fn is_process_exiting(process_id: u64) -> Option<bool> {
    // Every IPC syscall asks this several times, and it was the single busiest
    // acquisition site in the kernel: a global table lock plus a full slot walk
    // to read one bool. A live publication already means "not exiting", so the
    // affirmative answer needs no lock at all. Only the cases the publication
    // cannot distinguish -- exiting, mid-exec, or unknown -- reach the table.
    if identity::published_process_is_live_by_pid(process_id) {
        return Some(false);
    }
    let table = PROCESS_TABLE.lock();
    table
        .slots
        .iter()
        .filter_map(|slot| slot.object.as_deref())
        .find(|object| object.process_id == process_id)
        .map(|object| object.exiting)
}

pub fn thread_count_by_pid(process_id: u64) -> Option<usize> {
    let table = PROCESS_TABLE.lock();
    table
        .slots
        .iter()
        .filter_map(|slot| slot.object.as_deref())
        .find(|object| object.process_id == process_id)
        .map(|object| object.thread_count)
}

pub fn wait_for_child(
    parent_process_id: u64,
    target_pid: i64,
    include_stopped: bool,
    include_continued: bool,
) -> WaitResult {
    let mut table = PROCESS_TABLE.lock();
    let mut saw_child = false;
    let mut queued_handle = None;
    let mut queued_identity = None;
    let mut exited = None;

    for (index, slot) in table.slots.iter_mut().enumerate() {
        let Some(object) = slot.object.as_deref_mut() else {
            continue;
        };
        // Logical wait consumption is final even while references or threads
        // keep the physical process object alive for teardown.
        if object.parent_process_id != Some(parent_process_id) || object.waited {
            continue;
        }
        let pid = object.process_id;
        let matches = match target_pid {
            -1 => true,
            pid_filter if pid_filter > 0 => pid == pid_filter as u64,
            _ => false,
        };
        if !matches {
            continue;
        }

        saw_child = true;
        if let Some(status) = object.exit_status {
            object.waited = true;
            if object.thread_count == 0 && object.ref_count == 0 && !object.queued_for_reap {
                object.queued_for_reap = true;
                let handle = ProcessHandle::new(index, slot.generation);
                queued_handle = Some(handle);
                queued_identity = Some((
                    handle,
                    object.mm_generation,
                    object.lifecycle_transaction_id,
                ));
            }
            exited = Some(WaitResult::Exited { pid, status });
            break;
        }
        let status = match object.child_state_change {
            Some(ChildStateChange::Stopped(signal)) if include_stopped => {
                Some((i32::from(signal) << 8) | 0x7f)
            }
            Some(ChildStateChange::Continued) if include_continued => Some(0xffff),
            _ => None,
        };
        if let Some(status) = status {
            object.child_state_change = None;
            exited = Some(WaitResult::StateChanged { pid, status });
            break;
        }
    }

    if let Some(handle) = queued_handle {
        table.push_reap_handle(handle);
    }

    drop(table);
    if let Some((handle, mm_generation, transaction_id)) = queued_identity {
        record_lifecycle_marker(
            "lifecycle-reap-queued",
            handle,
            mm_generation,
            transaction_id,
        );
    }

    if let Some(result) = exited {
        // Another parent thread may have armed after the exit publication but
        // before this one consumed the last matching child. It must recheck
        // eligibility (and return ECHILD), not wait for physical reclamation.
        super::child_wait::wake_child_waiters(parent_process_id);
        return result;
    }

    if saw_child {
        return WaitResult::Pending;
    }
    WaitResult::NoMatchingChild
}

pub fn reap_exited_processes() -> usize {
    // Consume before scanning: a concurrent last-pin release survives for
    // this scan or the next one, never gets erased after the scan.
    let pin_reap_pending = pin_owner::take_reap_notification();
    let mut reaped = 0usize;
    let (queue, queue_len, scan_all) = {
        let mut table = PROCESS_TABLE.lock();
        let queue = core::mem::replace(&mut table.reap_queue, [None; REAP_QUEUE_CAPACITY]);
        let queue_len = core::mem::replace(&mut table.reap_len, 0);
        let scan_all = core::mem::replace(&mut table.reap_scan_pending, false) || pin_reap_pending;
        (queue, queue_len, scan_all)
    };
    let mut queue_next = 0usize;
    let mut scan_next = 0usize;
    loop {
        // Each pass reclaims a bounded batch, so the table lock is never held
        // across an unbounded number of slots and the stack never scales with
        // the number of process slots.
        let mut reclaimed: [Option<Box<ProcessObject>>; REAP_BATCH] =
            [const { None }; REAP_BATCH];
        let mut markers: [Option<(ProcessHandle, u32, u32)>; REAP_BATCH] =
            [const { None }; REAP_BATCH];
        let mut reclaimed_len = 0usize;
        let finished = {
            let mut table = PROCESS_TABLE.lock();
            while queue_next < queue_len && reclaimed_len < REAP_BATCH {
                let handle = queue[queue_next];
                queue_next += 1;
                let Some(handle) = handle else { continue };
                let index = handle.index();
                let Some(slot) = table.lookup_slot_mut(handle) else {
                    continue;
                };
                let identity = slot.object.as_deref().map(|object| {
                    (handle, object.mm_generation, object.lifecycle_transaction_id)
                });
                let Some(object) = reclaim_slot(index, slot) else {
                    continue;
                };
                markers[reclaimed_len] = identity;
                reclaimed[reclaimed_len] = Some(object);
                reclaimed_len += 1;
            }
            let mut scanned = 0usize;
            while scan_all
                && queue_next >= queue_len
                && scan_next < table.slots.len()
                && reclaimed_len < REAP_BATCH
                && scanned < REAP_SCAN_STRIDE
            {
                let index = scan_next;
                scan_next += 1;
                scanned += 1;
                let slot = &mut table.slots[index];
                let handle = ProcessHandle::new(index, slot.generation);
                let identity = slot.object.as_deref().map(|object| {
                    (handle, object.mm_generation, object.lifecycle_transaction_id)
                });
                let Some(object) = reclaim_slot(index, slot) else {
                    continue;
                };
                markers[reclaimed_len] = identity;
                reclaimed[reclaimed_len] = Some(object);
                reclaimed_len += 1;
            }
            queue_next >= queue_len && (!scan_all || scan_next >= table.slots.len())
        };
        reaped += reclaimed_len;
        for (handle, mm_generation, transaction_id) in
            markers.into_iter().take(reclaimed_len).flatten()
        {
            record_lifecycle_marker(
                "lifecycle-reap-complete",
                handle,
                mm_generation,
                transaction_id,
            );
        }
        // Objects are destroyed after the table lock is released.
        drop(reclaimed);
        if finished {
            break;
        }
    }
    kernel_mm::api::address_space::drain_retired_address_spaces(16);
    reaped
}

/// Adds one chunk of process slots starting at slot index `expected_len`.
///
/// Runs in normal context with no lock held: every per-slot side array gets its
/// chunk published first, then the table chunk is built outside the lock and
/// installed under it without allocating. A slot therefore never becomes
/// allocatable before the storage that its index addresses exists. Two racing
/// growers both succeed; the loser's table chunk is dropped unused.
fn grow_slots(expected_len: usize) -> bool {
    if expected_len >= SLOT_CEILING {
        return false;
    }
    if !(identity::ensure_slot_storage(expected_len)
        && pin_owner::ensure_slot_storage(expected_len)
        && super::process_placement::ensure_slot_storage(expected_len)
        && super::pager_vma::ensure_slot_storage(expected_len))
    {
        return false;
    }
    let Some(chunk) = ChunkTable::<ProcessSlot>::build(ProcessSlot::empty) else {
        return false;
    };
    let mut table = PROCESS_TABLE.lock();
    if table.slots.len() != expected_len {
        // Someone else grew the table past `expected_len` meanwhile.
        return true;
    }
    table.slots.push_chunk(chunk)
}

#[cfg(test)]
fn reset_for_tests() {
    NEXT_LIFECYCLE_TRANSACTION_ID.store(1, Ordering::Relaxed);
    let retired = {
        let mut table = PROCESS_TABLE.lock();
        pin_owner::reset_for_tests();
        let retired = core::mem::replace(&mut *table, ProcessTable::new());
        jobs::reset_policy_for_tests();
        // The unlocked visibility words mirror the table, so a reset that left
        // them behind would let one test read another's slot as live.
        // The side arrays outlive the table, so republish every slot the old
        // table had grown as empty; the new table starts with none.
        let empty = ProcessSlot::empty();
        for index in 0..retired.slots.len() {
            publish_slot_visibility(index, &empty);
        }
        retired
    };
    // LIFECYCLE: address spaces and process-owned allocations may release
    // memory while being dropped, so test reset follows the production reaper
    // rule and destroys retired objects after releasing the process-table lock.
    drop(retired);
}

/// Fixture stamps carry no live process authority; production still uses begin_exec.
#[cfg(test)]
pub(super) fn test_exec_stamp(generation: u32, mm: u32, transaction: u32) -> ExecReservation {
    ExecReservation {
        handle: ProcessHandle::new(0, generation),
        expected_mm_generation: mm,
        next_mm_generation: mm + 1,
        transaction_id: transaction,
    }
}

/// Host-only image fixture; no live table/MM-generation publication authority.
#[cfg(test)]
pub(in crate::multitask) fn test_execution_mm(
    reservation: ExecReservation,
    mm: &ProcessAddressSpace,
) -> MmExecutionPin {
    MmExecutionPin::new(
        reservation.handle,
        reservation.next_mm_generation,
        mm.pin_execution().unwrap(),
    )
}
#[cfg(test)]
#[path = "process_table/exec_tests.rs"]
mod exec_tests;
#[cfg(test)]
pub(crate) mod tests {
    use core::sync::atomic::AtomicU64;

    use super::{
        attach_task, begin_exec, cancel_exec, clear_publication_for_test, create_process,
        create_process_with_parent, detach_task, is_process_exiting, mark_process_exiting,
        note_job_control, note_process_exit_status, own_process_ref, process_state_is_visible,
        reap_exited_processes, retain_process, thread_count_by_pid, try_with_process_state_mut,
        wait_for_child, ExecReservation, LockClass, ProcessHandle, ProcessObject, ProcessTable,
        WaitResult, live_process_identity, pin_owner, process_limit, set_process_limit,
        DEFAULT_PROCESS_LIMIT,
    };
    use crate::multitask::slot_dir::{SLOT_CEILING, SLOT_CHUNK};
    use crate::memory::paging::ProcessAddressSpace;
    use crate::user::process_state::UserProcessState;

    mod identity_tests;

    mod census_tests {
        use super::super::{MemoryCensusEntry, insert_largest};

        fn entry(process_id: u64, resident_pages: u64) -> MemoryCensusEntry {
            MemoryCensusEntry {
                process_id,
                parent_process_id: 1,
                resident_pages,
                job: 1,
            }
        }

        #[test]
        fn census_keeps_the_largest_first_and_breaks_ties_toward_the_younger_pid() {
            let mut out = [MemoryCensusEntry::default(); 3];
            let mut len = 0;
            for (pid, pages) in [(10, 5), (11, 50), (12, 5), (13, 20)] {
                len = insert_largest(&mut out, len, entry(pid, pages));
            }
            assert_eq!(len, 3);
            assert_eq!(
                [out[0].process_id, out[1].process_id, out[2].process_id],
                [11, 13, 12]
            );
        }

        #[test]
        fn the_live_census_lists_a_process_until_it_is_marked_exiting() {
            use super::super::{
                create_process_with_parent, mark_process_exiting, memory_census,
            };
            use super::{isolate_process_table, new_state};
            let _isolation = isolate_process_table();
            create_process_with_parent(8_101, Some(1), new_state()).expect("process 8101");
            create_process_with_parent(8_102, Some(8_101), new_state()).expect("process 8102");
            let mut out = [MemoryCensusEntry::default(); 4];
            let count = memory_census(&mut out);
            let mut live: std::vec::Vec<(u64, u64)> = out[..count]
                .iter()
                .map(|entry| (entry.process_id, entry.parent_process_id))
                .collect();
            live.sort_unstable();
            assert_eq!(live, [(8_101, 1), (8_102, 8_101)]);

            mark_process_exiting(8_101).expect("mark exiting");
            let count = memory_census(&mut out);
            assert_eq!(count, 1);
            assert_eq!(out[0].process_id, 8_102);
        }

        #[test]
        fn a_full_census_drops_an_entry_smaller_than_all_it_holds() {
            let mut out = [MemoryCensusEntry::default(); 2];
            let mut len = insert_largest(&mut out, 0, entry(1, 10));
            len = insert_largest(&mut out, len, entry(2, 20));
            len = insert_largest(&mut out, len, entry(3, 5));
            assert_eq!(len, 2);
            assert_eq!([out[0].process_id, out[1].process_id], [2, 1]);
        }
    }

    // These tests exercise the one global process table and call the global
    // reaper. Running them concurrently lets one test reap another test's
    // exited slot, turning exact lifecycle assertions into scheduler-order
    // flakes. Keep only this shared-state group serialized.
    pub(crate) static PROCESS_TABLE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes the shared-state lock, ignoring poisoning.
    ///
    /// The lock guards a group, so a poisoned lock fails every member of that
    /// group. One flaky test therefore reported as nineteen failures, all with
    /// the same `PoisonError` message and none of them naming the test that
    /// actually panicked -- the root cause was the only thing the output did not
    /// contain. Poisoning has nothing to protect here either: the guard's `Drop`
    /// calls `reset_for_tests`, so the next holder starts from a clean table
    /// whether or not the previous one unwound.
    pub(crate) fn lock_process_table_for_tests() -> std::sync::MutexGuard<'static, ()> {
        PROCESS_TABLE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) struct ProcessTableTestIsolation {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for ProcessTableTestIsolation {
        fn drop(&mut self) {
            super::reset_for_tests();
        }
    }

    pub(crate) fn isolate_process_table() -> ProcessTableTestIsolation {
        let guard = lock_process_table_for_tests();
        super::reset_for_tests();
        ProcessTableTestIsolation { _guard: guard }
    }

    pub(super) fn new_state() -> UserProcessState {
        UserProcessState::new(
            ProcessAddressSpace::empty_for_tests(),
            None,
            None,
            None,
            None,
            false,
            "/test.elf",
        )
    }

    /// Acquisitions of one tracked lock class during `body`.
    ///
    /// A performance invariant needs a number, not a comment: a path that
    /// quietly reopens a global lock still produces correct bytes, so nothing
    /// but a count objects to it. The per-class census is charged on the host
    /// path for exactly this.
    fn process_table_acquisitions_during<R>(body: impl FnOnce() -> R) -> (R, u64) {
        let class = LockClass::ProcessTable as usize;
        let _ = nucleus_core::util::lockdep::work_budget::take_class_census();
        let value = body();
        let census = nucleus_core::util::lockdep::work_budget::take_class_census();
        (value, census[class])
    }

    /// The hot path's whole reason to exist. A running thread already pins its
    /// own process object, so binding it must not enter the one global table
    /// that every process shares; a counted retain enters it twice, once to
    /// take the count and once to give it back.
    #[test]
    fn an_own_thread_pin_enters_the_global_process_table_zero_times() {
        let _isolation = isolate_process_table();
        let handle = create_process(0x9101, new_state()).expect("test process");
        attach_task(handle).expect("test thread");

        let (counted, counted_acquisitions) = process_table_acquisitions_during(|| {
            let reference = retain_process(handle).expect("counted retain");
            let process_id = reference.process_id();
            drop(reference);
            process_id
        });
        assert_eq!(counted, 0x9101);
        assert_eq!(
            counted_acquisitions, 2,
            "a counted retain is exactly the retain and its release"
        );

        let (pinned, pinned_acquisitions) = process_table_acquisitions_during(|| {
            let reference = own_process_ref(handle, 0x9101).expect("own-thread pin");
            let process_id = reference.process_id();
            drop(reference);
            process_id
        });
        assert_eq!(pinned, 0x9101);
        assert_eq!(
            pinned_acquisitions, 0,
            "the own-thread pin must not enter the global process table"
        );
    }

    #[test]
    fn an_own_thread_pin_answers_from_the_published_state_pointer() {
        let _isolation = isolate_process_table();
        let handle = create_process(0x9201, new_state()).expect("test process");
        attach_task(handle).expect("test thread");
        let reference = own_process_ref(handle, 0x9201).expect("own-thread pin");
        assert!(reference.with_visible_state(|_, _| ()).is_some());

        // A slot with no committed identity cannot be pinned at all, and an
        // already-pinned reference stops answering rather than dereferencing a
        // payload whose lifecycle publication was revoked.
        clear_publication_for_test(handle.index());
        assert!(own_process_ref(handle, 0x9201).is_none());
        assert!(reference.with_visible_state(|_, _| ()).is_none());
    }

    #[test]
    fn process_generations_fail_closed_instead_of_aliasing_stale_handles() {
        assert_eq!(ProcessTable::next_generation(1), Some(2));
        assert_eq!(ProcessTable::next_generation(u32::MAX), None);
    }

    #[test]
    fn lifecycle_transaction_ids_are_nonzero_unique_and_fail_closed_at_exhaustion() {
        let counter = AtomicU64::new(1);
        assert_eq!(
            super::allocate_nonwrapping_lifecycle_transaction_id(&counter),
            Some(1)
        );
        assert_eq!(
            super::allocate_nonwrapping_lifecycle_transaction_id(&counter),
            Some(2)
        );

        let exhausted = AtomicU64::new(u64::from(u32::MAX));
        assert_eq!(
            super::allocate_nonwrapping_lifecycle_transaction_id(&exhausted),
            Some(u32::MAX)
        );
        assert_eq!(
            super::allocate_nonwrapping_lifecycle_transaction_id(&exhausted),
            None
        );
    }

    #[test]
    fn stale_exec_transaction_id_cannot_authorize_or_cancel_live_reservation() {
        let _isolation = isolate_process_table();
        let handle = create_process(5_050, new_state()).expect("process handle");
        let reservation = begin_exec(handle).expect("exec reservation");
        let stale = ExecReservation {
            transaction_id: reservation.transaction_id.wrapping_add(1),
            ..reservation
        };
        assert!(!super::authorize_exec(stale));
        assert!(!cancel_exec(stale));
        assert!(cancel_exec(reservation));
    }

    #[test]
    fn exit_pending_wins_when_exec_is_cancelled() {
        let _isolation = isolate_process_table();
        let handle = create_process(5_051, new_state()).expect("process handle");
        let reservation = begin_exec(handle).expect("exec reservation");
        assert_eq!(mark_process_exiting(5_051), None);
        assert!(cancel_exec(reservation));
        assert_eq!(is_process_exiting(5_051), Some(true));
        assert_eq!(attach_task(handle), None);
        detach_task(handle).expect("detach exiting task");
        assert_eq!(reap_exited_processes(), 1);
    }

    #[test]
    fn parent_wait_is_required_before_child_reap() {
        let _isolation = isolate_process_table();
        let parent = create_process(5_060, new_state()).expect("parent");
        let child = create_process_with_parent(5_061, Some(5_060), new_state()).expect("child");
        note_process_exit_status(5_061, 0).expect("child exit status");
        detach_task(child).expect("child detach");
        assert_eq!(reap_exited_processes(), 0);
        assert!(matches!(
            wait_for_child(5_060, 5_061, false, false),
            WaitResult::Exited {
                pid: 5_061,
                status: 0
            }
        ));
        assert_eq!(reap_exited_processes(), 1);
        detach_task(parent).expect("parent detach");
        assert_eq!(reap_exited_processes(), 1);
    }

    #[test]
    fn retained_ref_delays_reclaim_until_drop() {
        let _guard = lock_process_table_for_tests();
        let handle = create_process(42, new_state()).expect("process handle");
        let retained = retain_process(handle).expect("retained process");
        detach_task(handle).expect("detach");
        assert_eq!(reap_exited_processes(), 0);
        drop(retained);
        assert_eq!(reap_exited_processes(), 1);
    }

    /// Restores the boot default so a growth test cannot leak its limit.
    struct ProcessLimitGuard;
    impl ProcessLimitGuard {
        fn raise(limit: usize) -> Self {
            assert_eq!(set_process_limit(limit), limit);
            Self
        }
    }
    impl Drop for ProcessLimitGuard {
        fn drop(&mut self) {
            set_process_limit(DEFAULT_PROCESS_LIMIT);
        }
    }

    #[test]
    fn the_default_limit_still_admits_exactly_the_boot_default_of_processes() {
        let _isolation = isolate_process_table();
        for pid in 0..DEFAULT_PROCESS_LIMIT as u64 {
            create_process(10_000 + pid, new_state()).expect("a slot below the limit");
        }
        assert!(
            create_process(99_999, new_state()).is_none(),
            "the 33rd process is refused at the default limit"
        );
    }

    #[test]
    fn a_raised_limit_grows_the_slot_table_and_every_side_array_follows() {
        let _isolation = isolate_process_table();
        let _limit = ProcessLimitGuard::raise(100);
        let mut handles = std::vec::Vec::new();
        for pid in 0..100_u64 {
            let handle = create_process(20_000 + pid, new_state())
                .unwrap_or_else(|| panic!("process {pid} below the raised limit"));
            handles.push(handle);
        }
        assert!(create_process(29_999, new_state()).is_none(), "the limit is exact, not chunk-rounded");
        let highest = handles.iter().map(|handle| handle.index()).max().unwrap();
        assert!(highest >= 2 * SLOT_CHUNK, "slots past the second chunk are in use");
        for (offset, &handle) in handles.iter().enumerate() {
            // Identity publication, pins, placement and the VMA tables all
            // address the same slot index, so each must exist for every slot.
            let identity = live_process_identity(handle).expect("published identity");
            assert_eq!(identity.process_id(), 20_000 + offset as u64);
            assert!(pin_owner::pin(handle).is_some(), "pin cell for slot {}", handle.index());
            assert!(
                crate::multitask::process_placement::cell_exists_for_tests(handle),
                "placement cell for slot {}",
                handle.index()
            );
            assert!(
                crate::multitask::pager_vma::slot_storage_exists_for_tests(handle),
                "pager VMA storage for slot {}",
                handle.index()
            );
        }
    }

    #[test]
    fn lowering_the_limit_never_evicts_a_live_process_and_admission_resumes_only_below_it() {
        let _isolation = isolate_process_table();
        let _limit = ProcessLimitGuard::raise(70);
        let handles: std::vec::Vec<_> = (0..70_u64)
            .map(|pid| create_process(30_000 + pid, new_state()).expect("below 70"))
            .collect();
        set_process_limit(40);
        for &handle in &handles {
            assert!(live_process_identity(handle).is_some(), "lowering must not evict");
        }
        assert!(
            create_process(39_999, new_state()).is_none(),
            "70 live processes exceed the lowered limit of 40, so nothing is admitted"
        );
        // The limit counts processes: admission resumes only once usage drops
        // below it, not merely when some low-numbered slot frees up.
        let mut reaped = 0;
        for &handle in handles.iter().take(30) {
            detach_task(handle).expect("detach");
            reaped += reap_exited_processes();
        }
        assert_eq!(reaped, 30);
        assert!(create_process(39_998, new_state()).is_none(), "40 live processes still fill a limit of 40");
        let next = handles[30];
        detach_task(next).expect("detach");
        assert_eq!(reap_exited_processes(), 1);
        assert!(create_process(39_997, new_state()).is_some(), "39 live processes leave room for one");
    }

    #[test]
    fn the_limit_is_clamped_to_the_storage_ceiling_and_never_zero() {
        let _isolation = isolate_process_table();
        let _limit = ProcessLimitGuard::raise(DEFAULT_PROCESS_LIMIT);
        assert_eq!(set_process_limit(usize::MAX), SLOT_CEILING);
        assert_eq!(set_process_limit(0), 1);
        assert_eq!(process_limit(), 1);
    }

    #[test]
    fn one_reap_pass_reclaims_more_processes_than_a_batch_and_than_the_queue_holds() {
        let _isolation = isolate_process_table();
        let _limit = ProcessLimitGuard::raise(150);
        let handles: std::vec::Vec<_> = (0..150_u64)
            .map(|pid| create_process(40_000 + pid, new_state()).expect("below 150"))
            .collect();
        for &handle in &handles {
            detach_task(handle).expect("detach");
        }
        // 150 exceeds both REAP_BATCH and REAP_QUEUE_CAPACITY, so this pass
        // must fall back to the scan and still finish in one call.
        assert_eq!(reap_exited_processes(), 150);
        for &handle in &handles {
            assert!(live_process_identity(handle).is_none());
        }
        assert_eq!(reap_exited_processes(), 0);
    }

    #[test]
    fn retained_ref_rejects_address_space_generation_substitution() {
        let _isolation = isolate_process_table();
        let handle = create_process(4_242, new_state()).expect("process handle");
        let retained = retain_process(handle).expect("retained process");
        let identity = retained.live_identity().expect("live identity");
        assert_eq!(
            retained.with_exact_visible_state(identity, |process_id, _| process_id),
            Some(4_242)
        );

        {
            let mut table = super::PROCESS_TABLE.lock();
            let object = table
                .lookup_object_mut(handle)
                .expect("live process object");
            object.mm_generation += 1;
            super::publish_slot_visibility(handle.index(), &table.slots[handle.index()]);
        }
        assert_eq!(
            retained.with_exact_visible_state(identity, |process_id, _| process_id),
            None
        );
    }

    #[test]
    fn process_address_space_and_exec_exit_are_serialized() {
        let mut object = ProcessObject::new(7, None, new_state(), 41, super::jobs::ROOT_JOB);
        let held = object.state.lock();
        assert!(object.state.try_lock().is_none());
        drop(held);
        assert!(object.state.try_lock().is_some());
        assert!(!super::exec_may_replace(&object));
        object.exec_in_progress = true;
        assert!(super::exec_may_replace(&object));
        object.exec_commit_authorized = true;
        assert!(!super::exec_may_replace(&object));
        let reservation = ExecReservation {
            handle: ProcessHandle::new(0, 1),
            expected_mm_generation: object.mm_generation,
            next_mm_generation: object.mm_generation + 1,
            transaction_id: object.lifecycle_transaction_id,
        };
        object.exit_pending = true;
        assert!(super::exec_commit_may_transfer(&object, reservation));
        object.exit_pending = false;
        object.exec_commit_authorized = false;
        object.thread_count = 2;
        assert!(!super::exec_may_replace(&object));
        object.thread_count = 1;
        object.exiting = true;
        assert!(!super::exec_may_replace(&object));
    }

    #[test]
    fn exception_process_state_try_lock_never_waits_on_contention() {
        let _isolation = isolate_process_table();
        let handle = create_process(49, new_state()).expect("process handle");
        let retained = retain_process(handle).expect("retained process");

        let contended = retained.with_state(|_, _| {
            try_with_process_state_mut(handle, |_, _| {
                panic!("contended exception mutation unexpectedly acquired process state")
            })
        });
        assert!(contended.is_none());

        detach_task(handle).expect("detach task");
        drop(retained);
        assert_eq!(reap_exited_processes(), 1);
    }

    #[test]
    fn exec_seal_rejects_thread_attachment_until_cancel() {
        let _isolation = isolate_process_table();
        let handle = create_process(48, new_state()).expect("process handle");

        let reservation = begin_exec(handle).expect("begin exec");
        assert_eq!(attach_task(handle), None);
        assert_eq!(thread_count_by_pid(48), Some(1));
        assert!(cancel_exec(reservation));
        attach_task(handle).expect("attach after exec cancellation");

        detach_task(handle).expect("detach sibling");
        detach_task(handle).expect("detach leader");
        assert_eq!(reap_exited_processes(), 1);
    }

    #[test]
    fn leader_thread_retirement_does_not_mark_live_process_exited() {
        let _guard = lock_process_table_for_tests();
        let handle = create_process(43, new_state()).expect("process handle");
        attach_task(handle).expect("second thread");
        assert_eq!(thread_count_by_pid(43), Some(2));

        detach_task(handle).expect("leader detach");

        assert_eq!(thread_count_by_pid(43), Some(1));
        assert_eq!(is_process_exiting(43), Some(false));
        detach_task(handle).expect("last thread detach");
        assert_eq!(is_process_exiting(43), Some(true));
        assert_eq!(reap_exited_processes(), 1);
    }

    #[test]
    fn exiting_process_rejects_new_thread_attachment() {
        let _guard = lock_process_table_for_tests();
        let handle = create_process(44, new_state()).expect("process handle");
        mark_process_exiting(44).expect("mark exiting");

        assert_eq!(attach_task(handle), None);
        assert_eq!(thread_count_by_pid(44), Some(1));
        assert_eq!(is_process_exiting(44), Some(true));

        detach_task(handle).expect("detach final thread");
        assert_eq!(reap_exited_processes(), 1);
    }

    #[test]
    fn one_process_cannot_consume_the_global_task_table() {
        let _guard = lock_process_table_for_tests();
        let handle = create_process(45, new_state()).expect("process handle");
        for _ in 1..super::MAX_THREADS_PER_PROCESS {
            attach_task(handle).expect("thread within per-process ceiling");
        }
        assert_eq!(
            thread_count_by_pid(45),
            Some(super::MAX_THREADS_PER_PROCESS)
        );
        assert_eq!(attach_task(handle), None);

        for _ in 0..super::MAX_THREADS_PER_PROCESS {
            detach_task(handle).expect("detach admitted thread");
        }
        assert_eq!(reap_exited_processes(), 1);
    }

    #[test]
    fn exited_child_is_consumed_once_even_while_pinned() {
        let _guard = lock_process_table_for_tests();
        let parent = create_process(5_070, new_state()).unwrap();
        let child = create_process_with_parent(5_071, Some(5_070), new_state()).unwrap();
        let pin = super::pin_owner::pin(child).unwrap();
        note_process_exit_status(5_071, 0x2a00).unwrap();
        detach_task(child).unwrap();
        assert!(matches!(
            wait_for_child(5_070, -1, false, false),
            WaitResult::Exited {
                pid: 5_071,
                status: 0x2a00
            }
        ));
        assert_eq!(reap_exited_processes(), 0);
        for target in [-1, 5_071] {
            assert!(matches!(
                wait_for_child(5_070, target, true, true),
                WaitResult::NoMatchingChild
            ));
        }
        drop(pin);
        assert_eq!(reap_exited_processes(), 1);
        detach_task(parent).unwrap();
        assert_eq!(reap_exited_processes(), 1);
    }

    #[test]
    fn concurrent_parent_waiters_cannot_replay_a_pinned_exit() {
        let _guard = lock_process_table_for_tests();
        let parent = create_process(5_080, new_state()).unwrap();
        let child = create_process_with_parent(5_081, Some(5_080), new_state()).unwrap();
        let pin = super::pin_owner::pin(child).unwrap();
        note_process_exit_status(5_081, 0x2a00).unwrap();
        detach_task(child).unwrap();
        let ready = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                ready.wait();
                wait_for_child(5_080, -1, false, false)
            });
            let b = scope.spawn(|| {
                ready.wait();
                wait_for_child(5_080, 5_081, false, false)
            });
            ready.wait();
            let results = [a.join().unwrap(), b.join().unwrap()];
            assert_eq!(
                results
                    .iter()
                    .filter(|r| matches!(
                        r,
                        WaitResult::Exited {
                            pid: 5_081,
                            status: 0x2a00
                        }
                    ))
                    .count(),
                1
            );
            assert_eq!(
                results
                    .iter()
                    .filter(|r| matches!(r, WaitResult::NoMatchingChild))
                    .count(),
                1
            );
        });
        drop(pin);
        assert_eq!(reap_exited_processes(), 1);
        detach_task(parent).unwrap();
        assert_eq!(reap_exited_processes(), 1);
    }

    #[test]
    fn child_stop_and_continue_status_require_exact_wait_options() {
        let _guard = lock_process_table_for_tests();
        let parent = create_process(46, new_state()).expect("parent");
        let child =
            create_process_with_parent(47, Some(46), new_state()).expect("child process handle");

        let pin = super::pin_owner::pin(child).unwrap();
        assert_eq!(note_job_control(&pin, 999, Some(19)), None);
        note_job_control(&pin, 47, Some(19)).expect("record stopped");
        drop(pin);
        assert!(matches!(
            wait_for_child(46, 47, false, false),
            WaitResult::Pending
        ));
        assert!(matches!(
            wait_for_child(46, 47, true, false),
            WaitResult::StateChanged {
                pid: 47,
                status: 0x137f
            }
        ));

        let pin = super::pin_owner::pin(child).unwrap();
        note_job_control(&pin, 47, None).expect("record continued");
        drop(pin);
        assert!(matches!(
            wait_for_child(46, 47, true, false),
            WaitResult::Pending
        ));
        assert!(matches!(
            wait_for_child(46, 47, false, true),
            WaitResult::StateChanged {
                pid: 47,
                status: 0xffff
            }
        ));

        note_process_exit_status(47, 0).expect("record child exit");
        detach_task(child).expect("detach child");
        assert!(matches!(
            wait_for_child(46, 47, true, true),
            WaitResult::Exited { pid: 47, status: 0 }
        ));
        assert_eq!(reap_exited_processes(), 1);
        detach_task(parent).expect("detach parent");
        assert_eq!(reap_exited_processes(), 1);
    }

    /// The unlocked visibility word must track the slot through its whole
    /// lifecycle. A publication site missed by a future change shows up here
    /// rather than as a user copy reading a staged or reaped address space.
    #[test]
    fn unlocked_visibility_tracks_creation_exec_staging_and_reclamation() {
        let _isolation = isolate_process_table();
        let handle = create_process(910_001, new_state()).expect("create process");
        assert!(
            process_state_is_visible(handle),
            "a freshly created process must be visible"
        );

        let stale = ProcessHandle::new(handle.index(), handle.generation().wrapping_add(1));
        assert!(
            !process_state_is_visible(stale),
            "a handle from another generation must never read as visible"
        );

        detach_task(handle).expect("detach task");
        assert_eq!(reap_exited_processes(), 1);
        assert!(
            !process_state_is_visible(handle),
            "a reclaimed slot must not read as visible"
        );
    }
}
