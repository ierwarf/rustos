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
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use kernel_object::api::identity::{ObjectIdentity, ObjectKind, ObjectOwner};
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

use super::process_state_lock::ProcessStateLock;
use crate::memory::paging::ProcessAddressSpace;
use crate::user::process_state::UserProcessState;

const MAX_PROCESS_OBJECTS: usize = 32;
pub const MAX_THREADS_PER_PROCESS: usize = 32;
type ProcessTableLock = TrackedSpinLock<ProcessTable, { LockClass::ProcessTable as u8 }>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildStateChange {
    Stopped(u8),
    Continued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessHandle {
    index: usize,
    generation: u32,
}

/// Stable, live process authority used at capability boundaries. A PID is only
/// a routing label; callers retain both the process-table and address-space
/// generations so PID reuse and exec cannot inherit a prior grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    process_id: u64,
    process_generation: u32,
    mm_generation: u32,
}

impl ProcessIdentity {
    pub const fn process_id(self) -> u64 {
        self.process_id
    }

    pub const fn process_generation(self) -> u32 {
        self.process_generation
    }

    pub const fn mm_generation(self) -> u32 {
        self.mm_generation
    }
}

impl ProcessHandle {
    pub const fn new(index: usize, generation: u32) -> Self {
        Self { index, generation }
    }

    pub const fn index(self) -> usize {
        self.index
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }

    /// Typed process-table identity for capability boundaries.
    ///
    /// The table's zero-based slot is converted to the shared vocabulary's
    /// nonzero slot. This does not prove that the process remains live; users
    /// still retain or resolve this exact handle through the process table.
    pub fn object_identity(self) -> Option<ObjectIdentity> {
        let slot = u64::try_from(self.index).ok()?.checked_add(1)?;
        ObjectIdentity::new(
            ObjectOwner::Ps,
            ObjectKind::Process,
            slot,
            u64::from(self.generation),
        )
    }
}

pub struct ProcessRef {
    handle: ProcessHandle,
    process_id: u64,
    state_ptr: NonNull<ProcessStateLock<UserProcessState>>,
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
}

impl ExecReservation {
    /// One-shot lifecycle-token identity for the exact pre-exec MM generation.
    ///
    /// `handle` remains the process-table generation authority, while this
    /// token lets capability-facing code name the reservation without
    /// collapsing process and MM generations. Callers must still authorize or
    /// consume the reservation through the process table.
    pub fn object_identity(self) -> Option<ObjectIdentity> {
        let slot = u64::try_from(self.handle.index()).ok()?.checked_add(1)?;
        ObjectIdentity::new(
            ObjectOwner::Ps,
            ObjectKind::LifecycleToken,
            slot,
            u64::from(self.expected_mm_generation),
        )
    }
}

pub struct StagedExec {
    reservation: ExecReservation,
    process_id: u64,
    closed: alloc::vec::Vec<crate::user::handles::KernelHandle>,
    old_state: UserProcessState,
}

pub struct ExecFinalize {
    process_id: u64,
    exit_pending: bool,
    closed: alloc::vec::Vec<crate::user::handles::KernelHandle>,
    old_state: UserProcessState,
}

impl ExecFinalize {
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

impl ProcessRef {
    pub const fn process_id(&self) -> u64 {
        self.process_id
    }

    pub fn live_identity(&self) -> Option<ProcessIdentity> {
        live_process_identity(self.handle)
    }

    pub fn with_state<R>(&self, f: impl FnOnce(u64, &UserProcessState) -> R) -> R {
        let state = unsafe { self.state_ptr.as_ref() }.lock();
        f(self.process_id, &state)
    }

    pub fn with_state_mut<R>(&self, f: impl FnOnce(u64, &mut UserProcessState) -> R) -> R {
        let mut state = unsafe { self.state_ptr.as_ref() }.lock();
        f(self.process_id, &mut state)
    }

    pub fn with_visible_state<R>(&self, f: impl FnOnce(u64, &UserProcessState) -> R) -> Option<R> {
        // SAFETY: ProcessRef retains the owning Arc, so the NonNull state
        // pointer remains live for this complete guarded access.
        let state = unsafe { self.state_ptr.as_ref() }.lock();
        process_state_is_visible(self.handle).then(|| f(self.process_id, &state))
    }

    pub fn with_visible_state_mut<R>(
        &self,
        f: impl FnOnce(u64, &mut UserProcessState) -> R,
    ) -> Option<R> {
        // SAFETY: ProcessRef retains the owning Arc, and the per-process lock
        // is the unique mutable access authority for the pointed-to state.
        let mut state = unsafe { self.state_ptr.as_ref() }.lock();
        process_state_is_visible(self.handle).then(|| f(self.process_id, &mut state))
    }

    pub fn try_with_state_mut<R>(
        &self,
        f: impl FnOnce(u64, &mut UserProcessState) -> R,
    ) -> Option<R> {
        // SAFETY: ProcessRef's retained table reference pins the allocation;
        // ProcessStateLock supplies the exclusive mutable access.
        let mut state = unsafe { self.state_ptr.as_ref() }.try_lock()?;
        process_state_is_visible(self.handle).then(|| f(self.process_id, &mut state))
    }
}

impl Drop for ProcessRef {
    fn drop(&mut self) {
        release_process_ref(self.handle);
    }
}

struct ProcessObject {
    process_id: u64,
    parent_process_id: Option<u64>,
    ref_count: usize,
    thread_count: usize,
    mm_generation: u32,
    exec_in_progress: bool,
    exec_commit_authorized: bool,
    exec_state_staged: bool,
    exit_pending: bool,
    exiting: bool,
    queued_for_reap: bool,
    exit_status: Option<i32>,
    child_state_change: Option<ChildStateChange>,
    waited: bool,
    state: ProcessStateLock<UserProcessState>,
}

impl ProcessObject {
    fn new(process_id: u64, parent_process_id: Option<u64>, state: UserProcessState) -> Self {
        Self {
            process_id,
            parent_process_id,
            ref_count: 1,
            thread_count: 1,
            mm_generation: 1,
            exec_in_progress: false,
            exec_commit_authorized: false,
            exec_state_staged: false,
            exit_pending: false,
            exiting: false,
            queued_for_reap: false,
            exit_status: None,
            child_state_change: None,
            waited: parent_process_id.is_none(),
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
}

impl ProcessSlot {
    const fn empty() -> Self {
        Self {
            generation: 1,
            object: None,
        }
    }
}

struct ProcessTable {
    slots: [ProcessSlot; MAX_PROCESS_OBJECTS],
    reap_queue: [Option<ProcessHandle>; MAX_PROCESS_OBJECTS],
    reap_len: usize,
    reap_scan_pending: bool,
}

impl ProcessTable {
    const fn new() -> Self {
        Self {
            slots: [const { ProcessSlot::empty() }; MAX_PROCESS_OBJECTS],
            reap_queue: [None; MAX_PROCESS_OBJECTS],
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
            return;
        }
        self.reap_queue[self.reap_len] = Some(handle);
        self.reap_len += 1;
    }
}

static PROCESS_TABLE: ProcessTableLock = ProcessTableLock::new(ProcessTable::new());

/// Per-slot state visibility, readable without the table lock.
///
/// Encodes `(generation << 1) | staged`, and zero for a slot with no live
/// object. Testing visibility used to take the global process table, and a
/// user-memory copy did that on top of a retain and a release of the same
/// lock -- three acquisitions to answer a question about one slot. A reader
/// still compares the generation its handle carries, so a reused slot reads as
/// invisible exactly as the locked scan made it.
///
/// Every transition that changes object presence, slot generation, or the
/// staged flag republishes this word while holding the table lock.
static PROCESS_STATE_VISIBILITY: [AtomicU64; MAX_PROCESS_OBJECTS] =
    [const { AtomicU64::new(0) }; MAX_PROCESS_OBJECTS];

/// Per-slot process state pointer, readable without the table lock.
///
/// `state_ptr` addresses a field inside the boxed process object and
/// `replace_for_exec` mutates the state in place, so this stays valid for the
/// object's whole lifetime.
static PROCESS_STATE_PTR: [AtomicPtr<ProcessStateLock<UserProcessState>>; MAX_PROCESS_OBJECTS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_PROCESS_OBJECTS];

/// Republishes `index`'s unlocked view from the authoritative slot.
fn publish_slot_visibility(index: usize, slot: &ProcessSlot) {
    let (word, state) = match slot.object.as_deref() {
        Some(object) => {
            let generation = u64::from(slot.generation) << 1;
            let word = if object.exec_state_staged {
                generation | 1
            } else {
                generation
            };
            (word, object.state_ptr().as_ptr())
        }
        None => (0, core::ptr::null_mut()),
    };
    // ORDERING: Release publishes the object installation, generation bump, or
    // staged transition that preceded it before any unlocked reader can
    // observe either word. The pointer is stored first so a reader that
    // observes a live visibility word always observes the matching state.
    PROCESS_STATE_PTR[index].store(state, Ordering::Release);
    // ORDERING: Release orders the pointer store above before this word, so a
    // reader that observes a live generation also observes the matching state.
    PROCESS_STATE_VISIBILITY[index].store(word, Ordering::Release);
}

/// Runs `f` against the **current task's own** process state without touching
/// the process table.
///
/// # Contract
///
/// Sound only for a process that has a live thread on this CPU.
/// `reclaim_slot` refuses to reclaim while `thread_count != 0`, so a running
/// task keeps its own object alive and its published state pointer valid. No
/// other process carries that guarantee, and every other caller must keep
/// going through [`retain_process`].
///
/// Binding an address space for a user copy used to cost three acquisitions of
/// the global table -- retain, visibility, release -- to reach a pointer the
/// running task already owned.
pub(super) fn with_own_visible_state<R>(
    handle: ProcessHandle,
    f: impl FnOnce(&UserProcessState) -> R,
) -> Option<R> {
    // ORDERING: Acquire observes the object installation published before the
    // pointer store.
    let state = NonNull::new(
        PROCESS_STATE_PTR
            .get(handle.index())?
            // ORDERING: the acquire below observes the installed state pointer.
            .load(Ordering::Acquire),
    )?;
    // SAFETY: the caller's own live thread pins this object, so the published
    // pointer addresses a state lock that cannot be reclaimed under it.
    let state = unsafe { state.as_ref() }.lock();
    // Visibility is tested after the state lock exactly as the locked path did:
    // an exec that is staging state holds this lock, so the flag it publishes
    // cannot be observed early.
    process_state_is_visible(handle).then(|| f(&state))
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

    let object = slot.object.take()?;
    // Generation exhaustion permanently retires this slot. Reusing generation
    // one would let a stale ProcessHandle alias a new process.
    slot.generation = ProcessTable::next_generation(slot.generation).unwrap_or(0);
    publish_slot_visibility(index, slot);
    Some(object)
}

pub fn create_process(process_id: u64, state: UserProcessState) -> Option<ProcessHandle> {
    create_process_with_parent(process_id, None, state)
}

pub fn create_process_with_parent(
    process_id: u64,
    parent_process_id: Option<u64>,
    state: UserProcessState,
) -> Option<ProcessHandle> {
    let object = Box::new(ProcessObject::new(process_id, parent_process_id, state));
    // Allocation and construction may enter the heap. Publish only the
    // completed object while holding the process-table spin lock.
    let mut table = PROCESS_TABLE.lock();
    let (index, slot) = table
        .slots
        .iter_mut()
        .enumerate()
        .find(|(_, slot)| slot.object.is_none() && slot.generation != 0)?;
    slot.object = Some(object);
    publish_slot_visibility(index, slot);
    let handle = ProcessHandle::new(index, slot.generation);
    Some(handle)
}

pub fn attach_task(handle: ProcessHandle) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let object = table.lookup_object_mut(handle)?;
    if object.exiting || object.exec_in_progress {
        return None;
    }
    if object.thread_count >= MAX_THREADS_PER_PROCESS {
        return None;
    }
    let ref_count = object.ref_count.checked_add(1)?;
    let thread_count = object.thread_count.checked_add(1)?;
    object.ref_count = ref_count;
    object.thread_count = thread_count;
    Some(())
}

pub fn detach_task(handle: ProcessHandle) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let should_queue = {
        let object = table.lookup_object_mut(handle)?;
        if object.thread_count == 0 || object.ref_count == 0 {
            return Some(());
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
        should_queue
    };
    if should_queue {
        table.push_reap_handle(handle);
    }
    Some(())
}

pub fn retain_process(handle: ProcessHandle) -> Option<ProcessRef> {
    let mut table = PROCESS_TABLE.lock();
    let object = table.lookup_object_mut(handle)?;
    object.ref_count = object.ref_count.checked_add(1)?;
    let process_id = object.process_id;
    let state_ptr = object.state_ptr();
    Some(ProcessRef {
        handle,
        process_id,
        state_ptr,
    })
}

pub fn retain_process_by_pid(process_id: u64) -> Option<ProcessRef> {
    let mut table = PROCESS_TABLE.lock();
    for (index, slot) in table.slots.iter_mut().enumerate() {
        let generation = slot.generation;
        let Some(object) = slot.object.as_deref_mut() else {
            continue;
        };
        if object.process_id != process_id {
            continue;
        }
        object.ref_count = object.ref_count.checked_add(1)?;
        let state_ptr = object.state_ptr();
        return Some(ProcessRef {
            handle: ProcessHandle::new(index, generation),
            process_id,
            state_ptr,
        });
    }
    None
}

/// Resolve a PID to its live generational authority. Processes that are
/// exiting or transitioning address spaces are deliberately not identities:
/// publication during either interval would make an old capability survive a
/// teardown or exec boundary.
pub fn live_process_identity_by_pid(process_id: u64) -> Option<ProcessIdentity> {
    let table = PROCESS_TABLE.lock();
    table.slots.iter().find_map(|slot| {
        let object = slot.object.as_deref()?;
        (object.process_id == process_id
            && !object.exiting
            && !object.exec_in_progress
            && !object.exec_state_staged)
            .then_some(ProcessIdentity {
                process_id,
                process_generation: slot.generation,
                mm_generation: object.mm_generation,
            })
    })
}

/// Resolve one scheduler-held process handle to a live identity without
/// accepting a recycled slot generation.
pub fn live_process_identity(handle: ProcessHandle) -> Option<ProcessIdentity> {
    let table = PROCESS_TABLE.lock();
    let slot = table
        .slots
        .get(handle.index())
        .filter(|slot| slot.generation == handle.generation())?;
    let object = slot.object.as_deref()?;
    (!object.exiting && !object.exec_in_progress && !object.exec_state_staged).then_some(
        ProcessIdentity {
            process_id: object.process_id,
            process_generation: handle.generation(),
            mm_generation: object.mm_generation,
        },
    )
}

pub fn release_process_ref(handle: ProcessHandle) {
    let mut table = PROCESS_TABLE.lock();
    let should_queue = {
        let Some(object) = table.lookup_object_mut(handle) else {
            return;
        };
        if object.ref_count == 0 {
            return;
        }
        object.ref_count -= 1;
        let should_queue =
            object.thread_count == 0 && object.ref_count == 0 && !object.queued_for_reap;
        if should_queue {
            object.queued_for_reap = true;
        }
        should_queue
    };
    if should_queue {
        table.push_reap_handle(handle);
    }
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

pub fn stage_exec_state(
    reservation: ExecReservation,
    address_space: ProcessAddressSpace,
    linux_process_state: crate::user::linux::LinuxProcessState,
    linux_memory_map: crate::user::linux::LinuxMemoryMapState,
    linux_runtime_profile: crate::user::linux::LinuxRuntimeProfile,
    exec_path: &str,
) -> Option<StagedExec> {
    let process = retain_process(reservation.handle)?;
    process.with_state_mut(|process_id, state| {
        {
            let mut table = PROCESS_TABLE.lock();
            let object = table.lookup_object_mut(reservation.handle)?;
            if !exec_commit_may_transfer(object, reservation) {
                return None;
            }
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
            object.mm_generation = reservation.next_mm_generation;
            object.exec_state_staged = true;
            let index = reservation.handle.index();
            let slot = &table.slots[index];
            publish_slot_visibility(index, slot);
        }
        Some(StagedExec {
            reservation,
            process_id,
            closed,
            old_state,
        })
    })
}

pub fn finalize_exec_state(staged: StagedExec, published_handle: ProcessHandle) -> ExecFinalize {
    assert_eq!(
        published_handle, staged.reservation.handle,
        "exec scheduler publication changed process generation"
    );
    let exit_pending = {
        let mut table = PROCESS_TABLE.lock();
        let object = table
            .lookup_object_mut(staged.reservation.handle)
            .expect("published exec process disappeared before finalization");
        assert!(
            object.exec_in_progress
                && object.exec_commit_authorized
                && object.exec_state_staged
                && object.mm_generation == staged.reservation.next_mm_generation,
            "published exec transaction lost exact generation ownership"
        );
        object.exec_in_progress = false;
        object.exec_commit_authorized = false;
        object.exec_state_staged = false;
        let exit_pending = core::mem::take(&mut object.exit_pending);
        if exit_pending {
            object.exiting = true;
        }
        let index = staged.reservation.handle.index();
        publish_slot_visibility(index, &table.slots[index]);
        exit_pending
    };
    ExecFinalize {
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

fn process_state_is_visible(handle: ProcessHandle) -> bool {
    let Some(word) = PROCESS_STATE_VISIBILITY.get(handle.index()) else {
        return false;
    };
    // ORDERING: Acquire observes the object installation, generation bump, or
    // staged transition published before this word.
    let encoded = word.load(Ordering::Acquire);
    encoded != 0 && encoded >> 1 == u64::from(handle.generation()) && encoded & 1 == 0
}

pub fn begin_exec(handle: ProcessHandle) -> Option<ExecReservation> {
    let mut table = PROCESS_TABLE.lock();
    let object = table.lookup_object_mut(handle)?;
    if object.exiting || object.exec_in_progress {
        return None;
    }
    let next_mm_generation = ProcessTable::next_generation(object.mm_generation)?;
    object.exec_in_progress = true;
    object.exec_commit_authorized = false;
    Some(ExecReservation {
        handle,
        expected_mm_generation: object.mm_generation,
        next_mm_generation,
    })
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
    true
}

pub fn cancel_exec(reservation: ExecReservation) -> bool {
    let mut table = PROCESS_TABLE.lock();
    let Some(object) = table.lookup_object_mut(reservation.handle) else {
        return false;
    };
    if !exec_reservation_matches(object, reservation) {
        return false;
    }
    if object.exec_state_staged {
        panic!("exec invariant: staged process state cannot be cancelled after commit began");
    }
    object.exec_in_progress = false;
    object.exec_commit_authorized = false;
    object.exec_state_staged = false;
    true
}

pub fn note_process_exit_status(process_id: u64, status: i32) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let object = table
        .slots
        .iter_mut()
        .filter_map(|slot| slot.object.as_deref_mut())
        .find(|object| object.process_id == process_id)?;
    object.exit_status = Some(status);
    object.child_state_change = None;
    object.exiting = true;
    Some(())
}

pub fn note_process_stopped(process_id: u64, signal: u64) -> Option<()> {
    let signal = u8::try_from(signal).ok()?;
    let mut table = PROCESS_TABLE.lock();
    let object = table
        .slots
        .iter_mut()
        .filter_map(|slot| slot.object.as_deref_mut())
        .find(|object| object.process_id == process_id && !object.exiting)?;
    object.child_state_change = Some(ChildStateChange::Stopped(signal));
    Some(())
}

pub fn note_process_continued(process_id: u64) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let object = table
        .slots
        .iter_mut()
        .filter_map(|slot| slot.object.as_deref_mut())
        .find(|object| object.process_id == process_id && !object.exiting)?;
    object.child_state_change = Some(ChildStateChange::Continued);
    Some(())
}

/// Mark a process as exiting before callers tear down resources that it owns.
///
/// The process object remains available until its tasks retire and its parent
/// reaps it, but new authority publication must fail once this bit is set.
pub fn mark_process_exiting(process_id: u64) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let object = table
        .slots
        .iter_mut()
        .filter_map(|slot| slot.object.as_deref_mut())
        .find(|object| object.process_id == process_id)?;
    if object.exec_in_progress {
        object.exit_pending = true;
        return None;
    }
    object.exiting = true;
    Some(())
}

/// Atomically marks the process exiting and reports whether this call won the
/// transition. Resource owners use the `true` result to perform exactly-once
/// service-side open-description teardown under concurrent exit_group calls.
pub fn mark_process_exiting_once(process_id: u64) -> Option<bool> {
    let mut table = PROCESS_TABLE.lock();
    let object = table
        .slots
        .iter_mut()
        .filter_map(|slot| slot.object.as_deref_mut())
        .find(|object| object.process_id == process_id)?;
    if object.exec_in_progress {
        object.exit_pending = true;
        return Some(false);
    }
    let first = !object.exiting;
    object.exiting = true;
    Some(first)
}

pub fn is_process_exiting(process_id: u64) -> Option<bool> {
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
    let mut exited = None;

    for (index, slot) in table.slots.iter_mut().enumerate() {
        let Some(object) = slot.object.as_deref_mut() else {
            continue;
        };
        if object.parent_process_id != Some(parent_process_id) {
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
                queued_handle = Some(ProcessHandle::new(index, slot.generation));
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

    if let Some(result) = exited {
        return result;
    }

    if saw_child {
        return WaitResult::Pending;
    }
    WaitResult::NoMatchingChild
}

pub fn reap_exited_processes() -> usize {
    let mut reaped = 0usize;
    let mut reclaimed: [Option<Box<ProcessObject>>; MAX_PROCESS_OBJECTS] =
        [const { None }; MAX_PROCESS_OBJECTS];
    let mut reclaimed_len = 0usize;

    {
        let mut table = PROCESS_TABLE.lock();
        let queue = core::mem::replace(&mut table.reap_queue, [None; MAX_PROCESS_OBJECTS]);
        let queue_len = core::mem::replace(&mut table.reap_len, 0);
        let scan_all = core::mem::replace(&mut table.reap_scan_pending, false);
        for handle in queue.into_iter().take(queue_len).flatten() {
            let index = handle.index();
            let Some(slot) = table.lookup_slot_mut(handle) else {
                continue;
            };
            let Some(object) = reclaim_slot(index, slot) else {
                continue;
            };
            reclaimed[reclaimed_len] = Some(object);
            reclaimed_len += 1;
            reaped += 1;
        }
        if scan_all {
            for (index, slot) in table.slots.iter_mut().enumerate() {
                let Some(object) = reclaim_slot(index, slot) else {
                    continue;
                };
                reclaimed[reclaimed_len] = Some(object);
                reclaimed_len += 1;
                reaped += 1;
            }
        }
    }
    drop(reclaimed);
    reaped
}

#[cfg(test)]
fn reset_for_tests() {
    let retired = {
        let mut table = PROCESS_TABLE.lock();
        let retired = core::mem::replace(&mut *table, ProcessTable::new());
        // The unlocked visibility words mirror the table, so a reset that left
        // them behind would let one test read another's slot as live.
        for (index, slot) in table.slots.iter().enumerate() {
            publish_slot_visibility(index, slot);
        }
        retired
    };
    // LIFECYCLE: address spaces and process-owned allocations may release
    // memory while being dropped, so test reset follows the production reaper
    // rule and destroys retired objects after releasing the process-table lock.
    drop(retired);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        ExecReservation, ProcessHandle, ProcessObject, ProcessTable, WaitResult, attach_task,
        begin_exec, cancel_exec, create_process, create_process_with_parent, detach_task,
        is_process_exiting, mark_process_exiting, note_process_continued, note_process_exit_status,
        note_process_stopped, process_state_is_visible, reap_exited_processes, retain_process,
        thread_count_by_pid, try_with_process_state_mut, wait_for_child,
    };
    use crate::memory::paging::ProcessAddressSpace;
    use crate::user::process_state::UserProcessState;

    mod identity_tests;

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

    fn new_state() -> UserProcessState {
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

    #[test]
    fn process_generations_fail_closed_instead_of_aliasing_stale_handles() {
        assert_eq!(ProcessTable::next_generation(1), Some(2));
        assert_eq!(ProcessTable::next_generation(u32::MAX), None);
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

    #[test]
    fn process_address_space_and_exec_exit_are_serialized() {
        let mut object = ProcessObject::new(7, None, new_state());
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
    fn child_stop_and_continue_status_require_exact_wait_options() {
        let _guard = lock_process_table_for_tests();
        let parent = create_process(46, new_state()).expect("parent");
        let child =
            create_process_with_parent(47, Some(46), new_state()).expect("child process handle");

        note_process_stopped(47, 19).expect("record stopped");
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

        note_process_continued(47).expect("record continued");
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
