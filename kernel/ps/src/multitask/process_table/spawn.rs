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
    pub(super) transaction_id: u32,
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

    pub const fn transaction_id(self) -> u32 {
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
    let transaction_id = allocate_lifecycle_transaction_id()?;
    let mut table = PROCESS_TABLE.lock();
    let (index, slot) = table.slots.iter_mut().enumerate().find(|(_, slot)| {
        slot.object.is_none() && slot.spawn_transaction_id.is_none() && slot.generation != 0
    })?;
    let handle = ProcessHandle::new(index, slot.generation);
    slot.spawn_transaction_id = Some(transaction_id);
    drop(table);
    record_lifecycle_marker("lifecycle-spawn-reserve", handle, 1, transaction_id);
    Some(SpawnReservation {
        handle,
        transaction_id,
    })
}

/// Consume one exact pre-publication reservation by installing the fully
/// constructed process. This is the sole transition that makes a reserved
/// slot visible to PID/process-generation lookup.
pub fn publish_spawn(
    reservation: SpawnReservation,
    process_id: u64,
    parent_process_id: Option<u64>,
    state: UserProcessState,
) -> Option<ProcessHandle> {
    // Heap construction stays outside the raw table critical section; if it
    // fails, the caller still owns the reservation and must cancel it.
    let object = Box::new(ProcessObject::new(
        process_id,
        parent_process_id,
        state,
        reservation.transaction_id,
    ));
    let mut table = PROCESS_TABLE.lock();
    let slot = table.lookup_slot_mut(reservation.handle)?;
    if slot.object.is_some() || slot.spawn_transaction_id != Some(reservation.transaction_id) {
        return None;
    }
    slot.object = Some(object);
    slot.spawn_transaction_id = None;
    publish_slot_visibility(reservation.handle.index(), slot);
    drop(table);
    record_lifecycle_marker(
        "lifecycle-spawn-publish",
        reservation.handle,
        1,
        reservation.transaction_id,
    );
    Some(reservation.handle)
}

/// Abort an unpublished spawn transaction. Nothing can observe the reserved
/// slot as a live process, so cancellation releases only table/token custody;
/// the caller's dropped address space returns its own frames after shootdown.
pub fn cancel_spawn(reservation: SpawnReservation) -> bool {
    let mut table = PROCESS_TABLE.lock();
    let Some(slot) = table.lookup_slot_mut(reservation.handle) else {
        return false;
    };
    if slot.object.is_some() || slot.spawn_transaction_id != Some(reservation.transaction_id) {
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
