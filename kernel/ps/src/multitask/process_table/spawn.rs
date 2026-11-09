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
    publish_inner(reservation, process_id, parent_process_id, state, None).map(|v| v.0)
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
) -> Option<pin_owner::ProcessObjectPin> {
    if !loan.live {
        return None;
    }
    let key = loan.key;
    let (_, pin) = publish_inner(key.reservation, key.member.1, parent, state, Some(key))?;
    loan.live = false;
    Some(pin.expect("root process publication lost pre-visibility pin"))
}

/// Names which check refused a process publication. Every one of them reaches a
/// `fork` as `EAGAIN` through `NoFreeTaskSlot`, so without a code a Job whose
/// exited children have not been freed yet, a full kmem budget and a lost
/// reservation are indistinguishable. 1 Job unavailable, 2 Job process/object
/// limit (detail: objects in the subtree, of which already reaped), 3 handle-table
/// budget, 4 mapping-record budget, 5 reservation lost, 6 thread-stack budget.
/// Bounded, so a refusal storm stays cheap.
fn publish_refused<T>(site: u64, a: u64, b: u64) -> Option<T> {
    static RECORDED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    // ORDERING: Relaxed; a counter bounding telemetry volume.
    if RECORDED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 32 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "process-publish-refused",
            (site << 56) | (a & 0xffff_ffff) << 16 | (b & 0xffff),
            0,
        );
    }
    None
}

fn publish_inner(
    reservation: SpawnReservation,
    process_id: u64,
    parent_process_id: Option<u64>,
    state: UserProcessState,
    root: Option<RootSpawnKey>,
) -> Option<(ProcessHandle, Option<pin_owner::ProcessObjectPin>)> {
    // Heap construction and failed-state destruction stay outside the table
    // leaf. The table guard drops before this earlier-declared object on refusal.
    // The job is only known under the lock, so the object is built with the
    // root job and re-pointed before it becomes visible.
    let mut object = Box::new(ProcessObject::new(
        process_id,
        parent_process_id,
        state,
        reservation.transaction_id,
        jobs::ROOT_JOB,
    ));
    let mut table = PROCESS_TABLE.lock();
    // A process joins its parent's job, or the explicit job; a job being killed
    // refuses, and a refused birth leaves the reservation for the caller to
    // cancel exactly as for any other publication refusal.
    object.job = match table.resolve_new_member_job(reservation.job, parent_process_id) {
        Ok(job) => job,
        Err(_) => return publish_refused(1, 0, 0),
    };
    // The authoritative admission check: reservation could not know the job a
    // fork inherits, and counts may have moved since an explicit job was checked.
    if !table.admits_process(object.job) {
        let (all, pending_free) = table.refusal_census(object.job.index());
        return publish_refused(2, all, pending_free);
    }
    // Bill the new process's descriptor slots to its job. The object is still
    // private, so no state lock is taken, and the charge is lock-free: a
    // refusal here is a plain publication refusal and charges nothing.
    if !object
        .state
        .get_mut()
        .handles_mut()
        .bind_kmem_group(object.job.index() as usize)
    {
        return publish_refused(3, 0, 0);
    }
    // The shared-mapping bookkeeping (fork-cloned records included) is billed
    // the same way, so a Job's Vma budget refuses the publication.
    if !object
        .state
        .get_mut()
        .bind_mapping_kmem(object.job.index() as usize)
    {
        return publish_refused(4, 0, 0);
    }
    let Some(slot) = table.lookup_slot_mut(reservation.handle) else {
        return publish_refused(5, 0, 0);
    };
    if slot.object.is_some()
        || slot.spawn_transaction_id != Some(reservation.transaction_id)
        || slot.root_spawn != root
    {
        return publish_refused(5, 1, 0);
    }
    // The first thread's stack is billed with the process; nothing after this
    // point can refuse the publication, so a charge never needs unwinding.
    if !kernel_mm::api::kmem_charge::try_charge(
        kernel_mm::api::kmem_charge::Class::Thread,
        object.job.index() as usize,
        super::THREAD_KMEM_BYTES,
    ) {
        return publish_refused(6, 0, 0);
    }
    object.thread_kmem = super::THREAD_KMEM_BYTES;
    // The root is quiescent until the process is visible, so binding it now
    // charges the image pages already mapped to the job exactly once.
    let root_phys = object.mm_execution.root_phys();
    let job_index = object.job.index() as usize;
    slot.object = Some(object);
    let _ = kernel_mm::api::phys::bind_root_charge(root_phys, job_index);
    slot.spawn_transaction_id = None;
    // Acquire actual backing BEFORE the release that admits live PID lookup;
    // no capture-after-publication gap and no Class55 leaf spans this Class54.
    let pin = root.map(|_| {
        pin_owner::bind(reservation.handle.index(), reservation.handle.generation());
        pin_owner::pin(reservation.handle).expect("new root process pin admission failed")
    });
    slot.root_spawn = None;
    // Index the pid only after the object is stored; still under the table
    // lock, so no lookup can observe the object without its index entry.
    table.pid_index_link(reservation.handle.index(), process_id);
    publish_slot_visibility(
        reservation.handle.index(),
        &table.slots[reservation.handle.index()],
    );
    drop(table);
    record_lifecycle_marker(
        "lifecycle-spawn-publish",
        reservation.handle,
        1,
        reservation.transaction_id,
    );
    Some((reservation.handle, pin))
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
