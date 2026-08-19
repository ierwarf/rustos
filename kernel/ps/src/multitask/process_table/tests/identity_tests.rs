//! Typed process and lifecycle identity adapter tests.

use super::super::{ExecReservation, ProcessHandle};

#[test]
fn process_handle_adapts_table_slot_and_generation_to_typed_identity() {
    let identity = ProcessHandle::new(0, 7)
        .object_identity()
        .expect("one-based process identity");
    assert_eq!(
        identity.owner(),
        kernel_object::api::identity::ObjectOwner::Ps
    );
    assert_eq!(
        identity.kind(),
        kernel_object::api::identity::ObjectKind::Process
    );
    assert_eq!(identity.slot(), 1);
    assert_eq!(identity.generation(), 7);
    assert!(ProcessHandle::new(0, 0).object_identity().is_none());
    assert!(
        ProcessHandle::new(usize::MAX, 1)
            .object_identity()
            .is_none()
    );
}

#[test]
fn exec_reservation_adapts_exact_pre_exec_mm_generation_to_lifecycle_token() {
    let reservation = ExecReservation {
        handle: ProcessHandle::new(4, 9),
        expected_mm_generation: 13,
        next_mm_generation: 14,
    };
    let identity = reservation.object_identity().expect("lifecycle identity");
    assert_eq!(
        identity.owner(),
        kernel_object::api::identity::ObjectOwner::Ps
    );
    assert_eq!(
        identity.kind(),
        kernel_object::api::identity::ObjectKind::LifecycleToken
    );
    assert_eq!(identity.slot(), 5);
    assert_eq!(identity.generation(), 13);

    let stale = ExecReservation {
        expected_mm_generation: 0,
        ..reservation
    };
    assert!(stale.object_identity().is_none());
}
