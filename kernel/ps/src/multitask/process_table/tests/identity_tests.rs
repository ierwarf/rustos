//! Typed process and lifecycle identity adapter tests.

use super::super::{ExecReservation, ProcessHandle};
use super::{
    attach_task, create_process, detach_task, isolate_process_table, new_state,
    process_state_is_visible, reap_exited_processes, retain_process,
};

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
fn exec_reservation_binds_process_generation_and_unique_transaction_token() {
    let reservation = ExecReservation {
        handle: ProcessHandle::new(4, 9),
        expected_mm_generation: 13,
        next_mm_generation: 14,
        transaction_id: 77,
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
    assert_eq!(identity.slot(), (9_u64 << 32) | 5);
    assert_eq!(identity.generation(), 77);
    assert_eq!(reservation.expected_mm_generation(), 13);
    assert_eq!(reservation.next_mm_generation(), 14);
    assert_eq!(reservation.transaction_id(), 77);

    let stale = ExecReservation {
        transaction_id: 0,
        ..reservation
    };
    assert!(stale.object_identity().is_none());
}

#[test]
fn reaped_slot_reuse_changes_generation_and_rejects_every_stale_bearer() {
    let _isolation = isolate_process_table();
    let stale = create_process(80_001, new_state()).expect("first process");
    detach_task(stale).expect("retire first process");
    assert_eq!(reap_exited_processes(), 1);

    let replacement = create_process(80_002, new_state()).expect("replacement process");
    assert_eq!(replacement.index(), stale.index());
    assert_ne!(replacement.generation(), stale.generation());
    assert!(!process_state_is_visible(stale));
    assert!(retain_process(stale).is_none());
    assert!(attach_task(stale).is_none());

    detach_task(replacement).expect("retire replacement");
    assert_eq!(reap_exited_processes(), 1);
}
