//! Lock-free process identity publication and retained process references.
//!
//! - **Owner:** the process-table lifecycle transaction is the only publisher;
//!   readers never mutate lifecycle or address-space generations.
//! - **Boundary:** a PID is only a label; a live identity includes the process
//!   slot generation and exact MM generation.
//! - **Publication:** writers revoke the committed word, update pointer/PID
//!   payloads, then release-store one generation word. Readers validate that
//!   word before and after reading the payload.
//! - **Lifetime:** counted references keep any process alive; an own-thread pin
//!   is valid only while that process still owns the calling task.
//! - **Failure:** an absent, stale, or torn publication falls back to the
//!   locked lifecycle authority. It is never interpreted as an exited process.
//! - **Forbidden:** this publication must not become a second lifecycle
//!   authority, keep an exec-visible stale MM, or permit slot-generation reuse.
//! - **Evidence:** process-address-space-lifecycle and the exact acquisition
//!   ceilings in process_table tests.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use kernel_object::api::identity::{ObjectIdentity, ObjectKind, ObjectOwner};

use super::{MAX_PROCESS_OBJECTS, PROCESS_TABLE};
use crate::multitask::process_state_lock::ProcessStateLock;
use crate::user::process_state::UserProcessState;

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
    pub(in crate::multitask) const fn from_parts(
        process_id: u64,
        process_generation: u32,
        mm_generation: u32,
    ) -> Self {
        Self {
            process_id,
            process_generation,
            mm_generation,
        }
    }

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

    pub(super) fn lifecycle_token_slot(self) -> Option<u64> {
        let slot = u64::try_from(self.index).ok()?.checked_add(1)?;
        (self.generation != 0 && slot <= u64::from(u32::MAX))
            .then_some((u64::from(self.generation) << 32) | slot)
    }
}

pub struct ProcessRef {
    handle: ProcessHandle,
    process_id: u64,
    pin: ProcessRefPin,
}

/// What keeps a ProcessRef object reachable.
#[derive(Clone, Copy)]
enum ProcessRefPin {
    Counted(NonNull<ProcessStateLock<UserProcessState>>),
    OwnThread,
}

/// Payload published with one process-table slot.
pub(super) struct SlotPublication {
    pub(super) process_id: u64,
    pub(super) mm_generation: u32,
    pub(super) live: bool,
    pub(super) state: *mut ProcessStateLock<UserProcessState>,
}

/// The committed generation pair. Zero means no live identity is published.
static PROCESS_IDENTITY: [AtomicU64; MAX_PROCESS_OBJECTS] =
    [const { AtomicU64::new(0) }; MAX_PROCESS_OBJECTS];
/// Payload committed by the release store to PROCESS_IDENTITY.
static PROCESS_ID: [AtomicU64; MAX_PROCESS_OBJECTS] =
    [const { AtomicU64::new(0) }; MAX_PROCESS_OBJECTS];
/// State pointer committed by the release store to PROCESS_IDENTITY.
static PROCESS_STATE_PTR: [AtomicPtr<ProcessStateLock<UserProcessState>>; MAX_PROCESS_OBJECTS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_PROCESS_OBJECTS];

#[inline]
const fn encode_identity(process_generation: u32, mm_generation: u32) -> u64 {
    (process_generation as u64) << 32 | mm_generation as u64
}

#[inline]
fn decode_identity(word: u64, process_id: u64) -> ProcessIdentity {
    ProcessIdentity {
        process_id,
        process_generation: (word >> 32) as u32,
        mm_generation: word as u32,
    }
}

/// Republishes one slot from lifecycle authority.
///
/// Revocation is first so no reader can combine an old committed generation
/// with a new PID or pointer. The final release store is the only commit point.
pub(super) fn publish_slot_visibility(
    index: usize,
    generation: u32,
    publication: Option<SlotPublication>,
) {
    PROCESS_IDENTITY[index].store(0, Ordering::Release);
    let (process_id, state, committed) =
        publication.map_or((0, core::ptr::null_mut(), 0), |publication| {
            let committed = publication
                .live
                .then_some(encode_identity(generation, publication.mm_generation))
                .unwrap_or(0);
            (publication.process_id, publication.state, committed)
        });
    PROCESS_STATE_PTR[index].store(state, Ordering::Relaxed);
    PROCESS_ID[index].store(process_id, Ordering::Relaxed);
    PROCESS_IDENTITY[index].store(committed, Ordering::Release);
}

/// Reads a committed exact identity without consulting the table.
pub(in crate::multitask) fn published_live_process_identity(
    handle: ProcessHandle,
) -> Option<ProcessIdentity> {
    let before = PROCESS_IDENTITY
        .get(handle.index())?
        .load(Ordering::Acquire);
    if before == 0 || before >> 32 != u64::from(handle.generation()) {
        return None;
    }
    let process_id = PROCESS_ID[handle.index()].load(Ordering::Relaxed);
    let after = PROCESS_IDENTITY[handle.index()].load(Ordering::Acquire);
    (before == after).then(|| decode_identity(before, process_id))
}

/// Whether some slot publishes `process_id` as live, without taking the table.
///
/// A committed publication means `!exiting && !exec_in_progress &&
/// !exec_state_staged`, so a live publication *proves* the process is not
/// exiting. The converse does not hold -- an absent publication may be an
/// exiting process, an exec transition, or an unknown PID -- so only the
/// affirmative answer is served here and everything else defers to the
/// lifecycle authority. That asymmetry is what lets this be a pure accelerator
/// rather than a second authority.
pub(super) fn published_process_is_live_by_pid(process_id: u64) -> bool {
    if process_id == 0 {
        return false;
    }
    (0..MAX_PROCESS_OBJECTS).any(|index| {
        // ORDERING: Acquire observes the committing release store before the
        // PID payload it commits; the re-read rejects a publication that was
        // revoked or replaced between the two payload reads.
        let before = PROCESS_IDENTITY[index].load(Ordering::Acquire);
        before != 0
            && PROCESS_ID[index].load(Ordering::Relaxed) == process_id
            && PROCESS_IDENTITY[index].load(Ordering::Acquire) == before
    })
}

fn locked_live_process_identity(handle: ProcessHandle) -> Option<ProcessIdentity> {
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

/// Resolves a handle through publication first and lifecycle authority only
/// when publication is absent, stale, revoked, or observed mid-transition.
pub fn live_process_identity(handle: ProcessHandle) -> Option<ProcessIdentity> {
    published_live_process_identity(handle).or_else(|| locked_live_process_identity(handle))
}

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

#[inline]
pub(super) fn process_state_is_visible(handle: ProcessHandle) -> bool {
    PROCESS_IDENTITY
        .get(handle.index())
        .is_some_and(|identity| {
            let word = identity.load(Ordering::Acquire);
            word != 0 && word >> 32 == u64::from(handle.generation())
        })
}

/// State pointer for a newly acquired own-thread pin.
fn published_process_state(
    handle: ProcessHandle,
) -> Option<NonNull<ProcessStateLock<UserProcessState>>> {
    let before = PROCESS_IDENTITY
        .get(handle.index())?
        .load(Ordering::Acquire);
    if before == 0 || before >> 32 != u64::from(handle.generation()) {
        return None;
    }
    let state = PROCESS_STATE_PTR[handle.index()].load(Ordering::Relaxed);
    let after = PROCESS_IDENTITY[handle.index()].load(Ordering::Acquire);
    (before == after).then(|| NonNull::new(state)).flatten()
}

impl ProcessRef {
    pub const fn process_id(&self) -> u64 {
        self.process_id
    }

    /// Returns the retained exact table handle for an internal lifecycle owner.
    ///
    /// The handle is deliberately confined to `multitask`: public callers use
    /// PID labels and cannot mint process-table authority from this accessor.
    pub(in crate::multitask) const fn handle(&self) -> ProcessHandle {
        self.handle
    }

    pub fn live_identity(&self) -> Option<ProcessIdentity> {
        live_process_identity(self.handle)
    }

    fn state(&self) -> Option<NonNull<ProcessStateLock<UserProcessState>>> {
        match self.pin {
            ProcessRefPin::Counted(state) => Some(state),
            ProcessRefPin::OwnThread => NonNull::new(
                PROCESS_STATE_PTR
                    .get(self.handle.index())?
                    .load(Ordering::Acquire),
            ),
        }
    }

    #[track_caller]
    pub fn with_state<R>(&self, f: impl FnOnce(u64, &UserProcessState) -> R) -> R {
        let site = core::panic::Location::caller();
        let state = self
            .state()
            .expect("process reference lost its state object");
        let state = unsafe { state.as_ref() }.lock_at(site);
        f(self.process_id, &state)
    }

    #[track_caller]
    pub fn with_state_mut<R>(&self, f: impl FnOnce(u64, &mut UserProcessState) -> R) -> R {
        let site = core::panic::Location::caller();
        let state = self
            .state()
            .expect("process reference lost its state object");
        let mut state = unsafe { state.as_ref() }.lock_at(site);
        f(self.process_id, &mut state)
    }

    #[track_caller]
    pub fn with_visible_state<R>(&self, f: impl FnOnce(u64, &UserProcessState) -> R) -> Option<R> {
        let state = unsafe { self.state()?.as_ref() }.lock_at(core::panic::Location::caller());
        process_state_is_visible(self.handle).then(|| f(self.process_id, &state))
    }

    /// Accesses state only while the exact process and MM generations retained
    /// by the caller remain the committed live identity.
    #[track_caller]
    pub fn with_exact_visible_state<R>(
        &self,
        expected: ProcessIdentity,
        f: impl FnOnce(u64, &UserProcessState) -> R,
    ) -> Option<R> {
        let state = unsafe { self.state()?.as_ref() }.lock_at(core::panic::Location::caller());
        (live_process_identity(self.handle) == Some(expected)).then(|| f(self.process_id, &state))
    }

    /// Mutable counterpart of with_exact_visible_state. The process-state lock
    /// closes exec/exit replacement while the caller commits a generation-bound
    /// address-space transaction.
    #[track_caller]
    pub fn with_exact_visible_state_mut<R>(
        &self,
        expected: ProcessIdentity,
        f: impl FnOnce(u64, &mut UserProcessState) -> R,
    ) -> Option<R> {
        let mut state = unsafe { self.state()?.as_ref() }.lock_at(core::panic::Location::caller());
        (live_process_identity(self.handle) == Some(expected))
            .then(|| f(self.process_id, &mut state))
    }

    #[track_caller]
    pub fn with_visible_state_mut<R>(
        &self,
        f: impl FnOnce(u64, &mut UserProcessState) -> R,
    ) -> Option<R> {
        let mut state = unsafe { self.state()?.as_ref() }.lock_at(core::panic::Location::caller());
        process_state_is_visible(self.handle).then(|| f(self.process_id, &mut state))
    }

    pub fn try_with_state_mut<R>(
        &self,
        f: impl FnOnce(u64, &mut UserProcessState) -> R,
    ) -> Option<R> {
        let mut state = unsafe { self.state()?.as_ref() }.try_lock()?;
        process_state_is_visible(self.handle).then(|| f(self.process_id, &mut state))
    }
}

impl Drop for ProcessRef {
    fn drop(&mut self) {
        if matches!(self.pin, ProcessRefPin::Counted(_)) {
            release_process_ref(self.handle);
        }
    }
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
        pin: ProcessRefPin::Counted(state_ptr),
    })
}

/// The current task's own process, without taking a reference count.
pub(in crate::multitask) fn own_process_ref(
    handle: ProcessHandle,
    process_id: u64,
) -> Option<ProcessRef> {
    published_process_state(handle)?;
    Some(ProcessRef {
        handle,
        process_id,
        pin: ProcessRefPin::OwnThread,
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
            pin: ProcessRefPin::Counted(state_ptr),
        });
    }
    None
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

/// Runs f against the current task's own process state without the table.
pub(in crate::multitask) fn with_own_visible_state<R>(
    handle: ProcessHandle,
    f: impl FnOnce(&UserProcessState) -> R,
) -> Option<R> {
    let state = published_process_state(handle)?;
    let state = unsafe { state.as_ref() }.lock();
    process_state_is_visible(handle).then(|| f(&state))
}

/// Exact-generation counterpart for a running task. The task pins the state
/// pointer; the post-lock publication comparison closes exec/reuse without
/// recursively acquiring the global process table.
pub(in crate::multitask) fn with_own_exact_visible_state<R>(
    handle: ProcessHandle,
    expected: ProcessIdentity,
    f: impl FnOnce(&UserProcessState) -> R,
) -> Option<R> {
    let state = published_process_state(handle)?;
    let state = unsafe { state.as_ref() }.lock();
    (published_live_process_identity(handle) == Some(expected)).then(|| f(&state))
}

/// Compares publication with locked lifecycle authority. Called only from the
/// out-of-scheduler-guard profile drain; a zero result emits nothing.
pub(in crate::multitask) fn publication_divergence_count() -> u64 {
    let table = PROCESS_TABLE.lock();
    table
        .slots
        .iter()
        .enumerate()
        .filter(|(index, slot)| {
            let handle = ProcessHandle::new(*index, slot.generation);
            let expected = slot.object.as_deref().and_then(|object| {
                (!object.exiting && !object.exec_in_progress && !object.exec_state_staged)
                    .then_some(ProcessIdentity {
                        process_id: object.process_id,
                        process_generation: slot.generation,
                        mm_generation: object.mm_generation,
                    })
            });
            published_live_process_identity(handle) != expected
        })
        .count() as u64
}

#[cfg(test)]
pub(super) fn clear_publication_for_test(index: usize) {
    PROCESS_IDENTITY[index].store(0, Ordering::Release);
}
