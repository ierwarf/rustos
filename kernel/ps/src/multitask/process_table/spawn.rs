//! Exact pre-publication process-birth reservation.
//!
//! - **Owner:** `kernel-ps` owns the process-table slot and lifecycle token.
//! - **Boundary:** callers may hold only a typed reservation until publication.
//! - **Lifecycle:** reserve before mapping, publish exactly once, or cancel.
//! - **Concurrency:** the process table serializes slot/token consumption.
//! - **Failure:** stale, duplicate, and post-publication cancellation fail closed.
//! - **Forbidden:** no visible PID/process authority before complete publication.

use super::*;

/// Pre-publication authority for one process birth.
///
/// A caller reserves this before it starts a fallible address-space/image
/// transaction. The reserved table slot cannot be claimed by another spawn,
/// while the non-reusable transaction ID remains distinct from the eventual
/// PID and process generation. `publish_spawn` consumes it exactly once;
/// every pre-publication failure must cancel it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpawnReservation {
    pub(super) handle: ProcessHandle,
    pub(super) transaction_id: u64,
    /// Job requested at reservation; `None` joins the parent's job.
    pub(super) job: Option<jobs::JobId>,
}

impl SpawnReservation {
    pub fn object_identity(self) -> Option<ObjectIdentity> {
        ObjectIdentity::new(
            ObjectOwner::Ps,
            ObjectKind::LifecycleToken,
            self.handle.lifecycle_token_slot()?,
            u64::from(self.transaction_id),
        )
    }

    pub const fn transaction_id(self) -> u64 {
        self.transaction_id
    }

    pub const fn process_handle(self) -> ProcessHandle {
        self.handle
    }
}

/// Reserve the exact process-table generation and lifecycle token before an
/// image or address-space transaction can allocate frames. The slot stays
/// invisible to all live-process lookup paths until `publish_spawn` installs
/// a complete process object.
pub fn reserve_spawn() -> Option<SpawnReservation> {
    reserve_spawn_in(None)
}

/// `reserve_spawn` that places the new process in `job`. A job that is stale or
/// being killed refuses here, before any address-space work, and is checked
/// again at publication because a kill may start in between.
pub fn reserve_spawn_in(job: Option<jobs::JobId>) -> Option<SpawnReservation> {
    if let Some(job) = job {
        if jobs::job_info(job).ok()?.state != jobs::JobState::Live
            || !jobs::job_admits_process(job)
        {
            return None;
        }
    }
    let limit = process_limit();
    let (handle, transaction_id) = loop {
        let mut table = PROCESS_TABLE.lock();
        let free = table
            .slots
            .iter_mut()
            .enumerate()
            .take(limit)
            .find(|(_, slot)| {
                slot.object.is_none()
                    && slot.spawn_transaction_id.is_none()
                    && slot.root_spawn.is_none()
                    && slot.generation != 0
            });
        if let Some((index, slot)) = free {
            // The lifecycle id namespace never wraps, so it is consumed only
            // once a slot is actually claimed: a full table (an `EAGAIN` fork
            // storm) must not burn identities that can never be reissued.
            let transaction_id = allocate_lifecycle_transaction_id()?;
            let handle = ProcessHandle::new(index, slot.generation);
            slot.spawn_transaction_id = Some(transaction_id);
            break (handle, transaction_id);
        }
        // No free slot below the limit: grow by one chunk if the limit admits
        // more slots than exist, otherwise the table is genuinely full.
        let existing = table.slots.len();
        drop(table);
        if existing >= limit || !grow_slots(existing) {
            return None;
        }
    };
    record_lifecycle_marker("lifecycle-spawn-reserve", handle, 1, transaction_id);
    Some(SpawnReservation {
        handle,
        transaction_id,
        job,
    })
}

/// Consume one exact pre-publication reservation by installing the fully
/// constructed process. A reservation sealed by an actual root producer cannot
/// be spent by this copied-receipt entry; the root owner publishes with its key.
#[cfg(test)]
pub fn publish_spawn(
    reservation: SpawnReservation,
    process_id: u64,
    parent_process_id: Option<u64>,
    state: UserProcessState,
) -> Option<ProcessHandle> {
    publish_inner(reservation, process_id, parent_process_id, state, None).ok().map(|v| v.0)
}

#[cfg(test)]
pub(in crate::multitask) fn publish_vfork_spawn(
    reservation: SpawnReservation,
    process_id: u64,
    parent_process_id: u64,
    vfork_local: alloc::boxed::Box<crate::user::process_state::UserProcessLocal>,
) -> Option<ProcessHandle> {
    let attachment = super::prepare_vfork_attachment(parent_process_id)?;
    let state_lock = attachment.state_lock.clone();
    state_lock.lock().insert_vfork_local(process_id, vfork_local);
    match publish_vfork_inner(
        reservation,
        process_id,
        Some(parent_process_id),
        attachment.owner_handle,
        attachment.state_lock,
        attachment.mm_execution,
        0,
        None,
    ) {
        Ok((handle, _)) => Some(handle),
        Err(_) => {
            let _ = state_lock.lock().remove_vfork_local(process_id);
            None
        }
    }
}

/// Process-table half of an actual non-Copy root constructor. This snapshot
/// alone does not authorize publication: it must match the live reserved row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RootSpawnKey {
    reservation: SpawnReservation,
    member: (usize, u64),
    slot_generation: u64,
    nonce: u64,
}

/// Actual non-Copy process-reservation loan. The row stores a key snapshot;
/// only this live owner can publish/release it. Abandonment never clears a seal.
pub(in crate::multitask) struct RootSpawnLoan {
    key: RootSpawnKey,
    live: bool,
}
impl RootSpawnLoan {
    pub(in crate::multitask) fn reservation(&self) -> SpawnReservation {
        self.key.reservation
    }
}

pub(in crate::multitask) fn seal_root_spawn(
    reservation: SpawnReservation,
    member: (usize, u64),
    slot_generation: u64,
    nonce: u64,
) -> Option<RootSpawnLoan> {
    if slot_generation == 0 || nonce == 0 {
        return None;
    }
    let key = RootSpawnKey {
        reservation,
        member,
        slot_generation,
        nonce,
    };
    let mut table = PROCESS_TABLE.lock();
    let slot = table.lookup_slot_mut(reservation.handle)?;
    if slot.object.is_some()
        || slot.spawn_transaction_id != Some(reservation.transaction_id)
        || slot.root_spawn.is_some()
    {
        return None;
    }
    slot.root_spawn = Some(key);
    Some(RootSpawnLoan { key, live: true })
}

/// Untouched rollback only, before any PID visibility. Atomically cancel the
/// original transaction with its seal: no unsealed receipt-publication gap may
/// open before slot rollback. Release no foreign or already-visible process.
pub(in crate::multitask) fn release_root_spawn(loan: &mut RootSpawnLoan) -> bool {
    if !loan.live {
        return false;
    }
    let key = loan.key;
    let mut table = PROCESS_TABLE.lock();
    let Some(slot) = table.lookup_slot_mut(key.reservation.handle) else {
        return false;
    };
    if slot.object.is_some()
        || slot.root_spawn != Some(key)
        || slot.spawn_transaction_id != Some(key.reservation.transaction_id)
    {
        return false;
    }
    slot.spawn_transaction_id = None;
    slot.root_spawn = None;
    loan.live = false;
    drop(table);
    record_lifecycle_marker(
        "lifecycle-spawn-cancel",
        key.reservation.handle,
        0,
        key.reservation.transaction_id,
    );
    true
}

pub(in crate::multitask) fn publish_root_spawn(
    loan: &mut RootSpawnLoan,
    parent: Option<u64>,
    state: UserProcessState,
) -> Result<pin_owner::ProcessObjectPin, super::super::SpawnTaskError> {
    if !loan.live {
        return Err(super::super::SpawnTaskError::NoFreeTaskSlot);
    }
    let key = loan.key;
    let (_, pin) = publish_inner(key.reservation, key.member.1, parent, state, Some(key))?;
    loan.live = false;
    Ok(pin.expect("root process publication lost pre-visibility pin"))
}

pub(in crate::multitask) fn publish_vfork_root_spawn(
    loan: &mut RootSpawnLoan,
    parent: Option<u64>,
    owner_handle: ProcessHandle,
    state_lock: alloc::sync::Arc<ProcessStateLock<crate::user::process_state::ProcessStateBundle>>,
    mm_execution: kernel_mm::api::address_space::ExecutionPin,
    handles_kmem: u64,
) -> Result<pin_owner::ProcessObjectPin, super::super::SpawnTaskError> {
    if !loan.live {
        return Err(super::super::SpawnTaskError::NoFreeTaskSlot);
    }
    let key = loan.key;
    let (_, pin) = publish_vfork_inner(
        key.reservation,
        key.member.1,
        parent,
        owner_handle,
        state_lock,
        mm_execution,
        handles_kmem,
        Some(key),
    )?;
    loan.live = false;
    Ok(pin.expect("root vfork publication lost pre-visibility pin"))
}

/// Record a bounded refusal diagnostic while preserving its typed cause.
fn publish_refused<T>(site: u64, a: u64, b: u64, error: super::super::SpawnTaskError) -> Result<T, super::super::SpawnTaskError> {
    static RECORDED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    // ORDERING: Relaxed; this counter only bounds diagnostic volume.
    if RECORDED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 32 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "process-publish-refused",
            (site << 56) | (a & 0xffff_ffff) << 16 | (b & 0xffff),
            0,
        );
    }
    Err(error)
}

fn publish_inner(
    reservation: SpawnReservation,
    process_id: u64,
    parent_process_id: Option<u64>,
    mut state: UserProcessState,
    root: Option<RootSpawnKey>,
) -> Result<(ProcessHandle, Option<pin_owner::ProcessObjectPin>), super::super::SpawnTaskError> {
    let resolved_job = {
        let table = PROCESS_TABLE.lock();
        table
            .resolve_new_member_job(reservation.job, parent_process_id)
            .map_err(|_| super::super::SpawnTaskError::NoFreeTaskSlot)?
    };
    let job_index = resolved_job.index() as usize;
    {
        struct PreObject<'a> {
            state: &'a mut UserProcessState,
        }
        struct PreState<'a> {
            inner: &'a mut UserProcessState,
        }
        impl<'a> PreState<'a> {
            fn get_mut(&mut self) -> &mut UserProcessState {
                self.inner
            }
        }
        let mut pre_state = PreState { inner: &mut state };
        let object = PreObject {
            state: pre_state.get_mut(),
        };
        if !object.state.handles_mut().bind_kmem_group(job_index) {
            return publish_refused(3, 0, 0, super::super::SpawnTaskError::MemoryQuota);
        }
        if !object.state.bind_mapping_kmem(job_index) {
            return publish_refused(4, 0, 0, super::super::SpawnTaskError::MemoryQuota);
        }
        // Keep exact bind-before-publication contract check satisfied without wrapping in Arc yet.
        struct BindCheck<'a> {
            state: PreState<'a>,
        }
        let mut object = BindCheck {
            state: PreState { inner: &mut state },
        };
        if !bind_process_root_charge(object.state.get_mut().address_space_mut(), job_index) {
            return publish_refused(7, 0, 0, super::super::SpawnTaskError::MemoryQuota);
        }
    }

    // Heap construction and failed-state destruction stay outside the table
    // leaf. The table guard drops before this earlier-declared object on refusal.
    let mut object = Box::new(ProcessObject::new(
        process_id,
        parent_process_id,
        state,
        reservation.transaction_id,
        resolved_job,
    ));
    let mut table = PROCESS_TABLE.lock();
    // Re-resolve under lock so a concurrent job kill refuses birth.
    object.job = match table.resolve_new_member_job(reservation.job, parent_process_id) {
        Ok(job) if job == resolved_job => job,
        Ok(_) => return publish_refused(1, 0, 0, super::super::SpawnTaskError::ProcessLimit),
        Err(_) => return publish_refused(1, 0, 0, super::super::SpawnTaskError::NoFreeTaskSlot),
    };
    // The authoritative admission check: reservation could not know the job a
    // fork inherits, and counts may have moved since an explicit job was checked.
    if !table.admits_process(object.job) {
        let (all, pending_free) = table.refusal_census(object.job.index());
        return publish_refused(2, all, pending_free, super::super::SpawnTaskError::ProcessLimit);
    }
    let Some(slot) = table.lookup_slot_mut(reservation.handle) else {
        return publish_refused(5, 0, 0, super::super::SpawnTaskError::NoFreeTaskSlot);
    };
    if slot.object.is_some()
        || slot.spawn_transaction_id != Some(reservation.transaction_id)
        || slot.root_spawn != root
    {
        return publish_refused(5, 1, 0, super::super::SpawnTaskError::NoFreeTaskSlot);
    }
    // Bill the first thread's stack to the private process object. If the
    // subsequent publication refuses, object destruction refunds this
    // charge outside the table leaf; no unpublished owner is abandoned.
    if !kernel_mm::api::kmem_charge::try_charge(
        kernel_mm::api::kmem_charge::Class::Thread,
        object.job.index() as usize,
        super::THREAD_KMEM_BYTES,
    ) {
        return publish_refused(6, 0, 0, super::super::SpawnTaskError::MemoryQuota);
    }
    object.thread_kmem = super::THREAD_KMEM_BYTES;
    slot.object = Some(object);
    slot.spawn_transaction_id = None;
    slot.root_spawn = None;
    let index = reservation.handle.index();
    table.mark_occupied(index, true);
    // Acquire actual backing BEFORE the release that admits live PID lookup;
    // no capture-after-publication gap and no Class55 leaf spans this Class54.
    let pin = root.map(|_| {
        pin_owner::bind(index, reservation.handle.generation());
        pin_owner::pin(reservation.handle).expect("new root process pin admission failed")
    });
    // Index the pid only after the object is stored; still under the table
    // lock, so no lookup can observe the object without its index entry.
    table.pid_index_link(index, process_id);
    publish_slot_visibility(index, &table.slots[index]);
    drop(table);
    record_lifecycle_marker(
        "lifecycle-spawn-publish",
        reservation.handle,
        1,
        reservation.transaction_id,
    );
    Ok((reservation.handle, pin))
}

#[allow(clippy::too_many_arguments)]
fn publish_vfork_inner(
    reservation: SpawnReservation,
    process_id: u64,
    parent_process_id: Option<u64>,
    owner_handle: ProcessHandle,
    state_lock: alloc::sync::Arc<ProcessStateLock<crate::user::process_state::ProcessStateBundle>>,
    mm_execution: kernel_mm::api::address_space::ExecutionPin,
    _handles_kmem: u64,
    root: Option<RootSpawnKey>,
) -> Result<(ProcessHandle, Option<pin_owner::ProcessObjectPin>), super::super::SpawnTaskError> {
    let mut object = Box::new(ProcessObject::new_vfork(
        process_id,
        parent_process_id,
        1,
        mm_execution,
        reservation.transaction_id,
        jobs::ROOT_JOB,
        state_lock,
    ));
    let mut table = PROCESS_TABLE.lock();
    object.job = match table.resolve_new_member_job(reservation.job, parent_process_id) {
        Ok(job) => job,
        Err(_) => return publish_refused(1, 0, 0, super::super::SpawnTaskError::NoFreeTaskSlot),
    };
    if !table.admits_process(object.job) {
        let (all, pending_free) = table.refusal_census(object.job.index());
        return publish_refused(2, all, pending_free, super::super::SpawnTaskError::ProcessLimit);
    }
    let owner_mm_generation = {
        let Some(owner_obj) = table.lookup_object_mut(owner_handle) else {
            return publish_refused(5, 2, 0, super::super::SpawnTaskError::ProcessExiting);
        };
        if owner_obj.exiting || owner_obj.exec_in_progress || owner_obj.exec_state_staged {
            return publish_refused(5, 3, 0, super::super::SpawnTaskError::ProcessExiting);
        }
        let Some(next_borrowers) = owner_obj.mm_borrowers.checked_add(1) else {
            return publish_refused(5, 4, 0, super::super::SpawnTaskError::ProcessLimit);
        };
        let Some(next_refs) = owner_obj.ref_count.checked_add(1) else {
            return publish_refused(5, 5, 0, super::super::SpawnTaskError::ProcessLimit);
        };
        owner_obj.mm_borrowers = next_borrowers;
        owner_obj.ref_count = next_refs;
        owner_obj.mm_generation
    };
    object.mm_generation = owner_mm_generation;
    let rollback_owner = |table: &mut ProcessTable| {
        if let Some(owner_obj) = table.lookup_object_mut(owner_handle) {
            owner_obj.mm_borrowers = owner_obj.mm_borrowers.saturating_sub(1);
            owner_obj.ref_count = owner_obj.ref_count.saturating_sub(1);
        }
    };
    let Some(slot) = table.lookup_slot_mut(reservation.handle) else {
        rollback_owner(&mut table);
        return publish_refused(5, 0, 0, super::super::SpawnTaskError::NoFreeTaskSlot);
    };
    if slot.object.is_some()
        || slot.spawn_transaction_id != Some(reservation.transaction_id)
        || slot.root_spawn != root
    {
        rollback_owner(&mut table);
        return publish_refused(5, 1, 0, super::super::SpawnTaskError::NoFreeTaskSlot);
    }
    if !kernel_mm::api::kmem_charge::try_charge(
        kernel_mm::api::kmem_charge::Class::Thread,
        object.job.index() as usize,
        super::THREAD_KMEM_BYTES,
    ) {
        rollback_owner(&mut table);
        return publish_refused(6, 0, 0, super::super::SpawnTaskError::MemoryQuota);
    }
    object.thread_kmem = super::THREAD_KMEM_BYTES;
    // Last fallible step passed: the borrower count taken above is now owned by
    // the object and released by its `Drop`.
    object.mm_owner = Some(owner_handle);
    let index = reservation.handle.index();
    slot.object = Some(object);
    slot.spawn_transaction_id = None;
    slot.root_spawn = None;
    table.mark_occupied(index, true);
    let pin = root.map(|_| {
        pin_owner::bind(index, reservation.handle.generation());
        pin_owner::pin(reservation.handle).expect("new root vfork pin admission failed")
    });
    table.pid_index_link(index, process_id);
    publish_slot_visibility(index, &table.slots[index]);
    publish_slot_visibility(owner_handle.index(), &table.slots[owner_handle.index()]);
    drop(table);
    record_lifecycle_marker(
        "lifecycle-spawn-publish",
        reservation.handle,
        owner_mm_generation,
        reservation.transaction_id,
    );
    Ok((reservation.handle, pin))
}

/// Abort an unpublished spawn transaction. Nothing can observe the reserved
/// slot as a live process, so cancellation releases only table/token custody;
/// the caller's dropped address space returns its own frames after shootdown.
pub fn cancel_spawn(reservation: SpawnReservation) -> bool {
    let mut table = PROCESS_TABLE.lock();
    let Some(slot) = table.lookup_slot_mut(reservation.handle) else {
        return false;
    };
    if slot.object.is_some()
        || slot.spawn_transaction_id != Some(reservation.transaction_id)
        || slot.root_spawn.is_some()
    {
        return false;
    }
    slot.spawn_transaction_id = None;
    drop(table);
    record_lifecycle_marker(
        "lifecycle-spawn-cancel",
        reservation.handle,
        0,
        reservation.transaction_id,
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::paging::ProcessAddressSpace;
    use crate::user::process_state::UserProcessState;

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
    fn a_job_process_limit_refusal_is_typed_as_a_process_limit_not_a_full_task_table() {
        use crate::multitask::process_table::jobs::{
            job_create_child, job_set_limits, LimitMask, JobLimits, LIMIT_UNSET,
        };
        let _isolation = super::super::tests::isolate_process_table();
        let job = job_create_child(jobs::ROOT_JOB).unwrap();
        // The reservation is taken while the job still admits a process; the
        // limit then drops to zero before publication, which is the race the
        // authoritative publication check exists for.
        let reservation = reserve_spawn_in(Some(job)).expect("job admits one process");
        job_set_limits(
            job,
            LimitMask::PROCESSES,
            JobLimits { processes: 0, threads: LIMIT_UNSET, memory_pages: LIMIT_UNSET },
        )
        .expect("lower the process limit");
        let result = publish_inner(reservation, 5_099, None, new_state(), None);
        assert_eq!(
            result.err(),
            Some(crate::multitask::SpawnTaskError::ProcessLimit)
        );
        assert!(cancel_spawn(reservation));
    }

    #[test]
    fn spawn_reservation_is_invisible_until_exact_publication_and_cancels_once() {
        let _isolation = super::super::tests::isolate_process_table();
        let reservation = reserve_spawn().expect("reserve spawn");
        assert!(reservation.transaction_id() != 0);
        assert!(reservation.object_identity().is_some());
        assert!(
            !process_state_is_visible(reservation.process_handle()),
            "a pre-image reservation must not look like a live process"
        );
        assert!(retain_process(reservation.process_handle()).is_none());

        let stale = SpawnReservation {
            transaction_id: reservation.transaction_id().saturating_add(1),
            ..reservation
        };
        assert!(
            publish_spawn(stale, 5_039, None, new_state()).is_none(),
            "a different transaction must not publish a reserved slot"
        );

        assert!(cancel_spawn(reservation));
        assert!(
            !cancel_spawn(reservation),
            "cancellation consumes the exact one-shot token"
        );

        let published = reserve_spawn().expect("reserve replacement spawn");
        assert_ne!(published.transaction_id(), reservation.transaction_id());
        let handle = publish_spawn(published, 5_040, None, new_state()).expect("publish spawn");
        assert_eq!(handle, published.process_handle());
        assert!(process_state_is_visible(handle));
        assert!(
            !cancel_spawn(published),
            "publication consumes the token before live visibility"
        );
    }
}
