//! Typed process and lifecycle identity adapter tests.

use super::super::{
    ExecReservation, ProcessHandle, live_process_identity, publication_divergence_count,
};
use super::{
    attach_task, begin_exec, cancel_exec, clear_publication_for_test, create_process, detach_task,
    is_process_exiting, isolate_process_table, mark_process_exiting, new_state, own_process_ref,
    process_state_is_visible, process_table_acquisitions_during, reap_exited_processes,
    retain_process,
};
use rustos_user_abi::performance::{
    EXACT_PROCESS_IDENTITY_MAX_PROCESS_TABLE_ACQUISITIONS,
    LIVE_PROCESS_EXIT_QUERY_MAX_PROCESS_TABLE_ACQUISITIONS,
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

/// Exact identity validation is an ordinary user-copy and IPC operation. Once
/// the running thread has pinned its own process, it must not reacquire the one
/// global process table merely to prove that the same MM generation is live.
#[test]
fn exact_live_identity_validation_never_reenters_the_process_table() {
    let _isolation = isolate_process_table();
    let handle = create_process(80_101, new_state()).expect("process");
    attach_task(handle).expect("thread pin");
    let reference = own_process_ref(handle, 80_101).expect("own-thread reference");
    let expected = reference.live_identity().expect("published exact identity");

    let (process_id, acquisitions) = process_table_acquisitions_during(|| {
        reference.with_exact_visible_state(expected, |process_id, _| process_id)
    });
    assert_eq!(process_id, Some(80_101));
    assert_eq!(
        acquisitions,
        u64::from(EXACT_PROCESS_IDENTITY_MAX_PROCESS_TABLE_ACQUISITIONS),
        "the exact live path must remain publication-only"
    );
}

/// Every lifecycle transition that hides or restores the process must update
/// the exact generation publication in the same ProcessTable transaction.
#[test]
fn exact_identity_publication_tracks_exec_cancel_and_exit() {
    let _isolation = isolate_process_table();
    let handle = create_process(80_102, new_state()).expect("process");
    attach_task(handle).expect("thread pin");
    let expected = live_process_identity(handle).expect("initial identity");
    assert_eq!(publication_divergence_count(), 0);

    let reservation = begin_exec(handle).expect("exec reservation");
    assert_eq!(live_process_identity(handle), None);
    assert_eq!(publication_divergence_count(), 0);

    assert!(cancel_exec(reservation));
    assert_eq!(live_process_identity(handle), Some(expected));
    assert_eq!(publication_divergence_count(), 0);

    assert_eq!(mark_process_exiting(80_102), Some(()));
    assert_eq!(live_process_identity(handle), None);
    assert_eq!(publication_divergence_count(), 0);
}

/// The out-of-guard sweep detects publication damage while the exact locked
/// fallback continues to preserve correctness for the affected access.
#[test]
fn missing_identity_publication_is_detected_and_falls_back_to_authority() {
    let _isolation = isolate_process_table();
    let handle = create_process(80_103, new_state()).expect("process");
    let expected = live_process_identity(handle).expect("initial identity");

    clear_publication_for_test(handle.index());
    assert_eq!(publication_divergence_count(), 1);
    let (actual, acquisitions) =
        process_table_acquisitions_during(|| live_process_identity(handle));
    assert_eq!(actual, Some(expected));
    assert_eq!(
        acquisitions, 1,
        "damaged publication must use exact authority"
    );
}

/// The liveness query is asked several times per synchronous IPC syscall, so
/// the live answer must cost no global table acquisition at all. The same test
/// pins the asymmetry that makes that sound: publication may only *prove* a
/// process live, and every answer it cannot prove still reaches the locked
/// lifecycle authority.
#[test]
fn a_live_process_exit_query_never_enters_the_table_and_exiting_still_reaches_authority() {
    let _isolation = isolate_process_table();
    let handle = create_process(0x9601, new_state()).expect("test process");
    attach_task(handle).expect("test thread");

    let (live, live_acquisitions) =
        process_table_acquisitions_during(|| is_process_exiting(0x9601));
    assert_eq!(live, Some(false));
    assert_eq!(
        live_acquisitions,
        u64::from(LIVE_PROCESS_EXIT_QUERY_MAX_PROCESS_TABLE_ACQUISITIONS),
        "a live process must be proven live by publication alone"
    );

    // An unknown PID has no publication, and publication must never be read as
    // evidence of absence: the answer comes from the table and is `None`.
    assert_eq!(is_process_exiting(0x9602), None);

    mark_process_exiting(0x9601).expect("mark exiting");
    assert_eq!(
        is_process_exiting(0x9601),
        Some(true),
        "publication must not serve a negative liveness answer"
    );

    // Revoked publication is what forces the fallback, so the exiting answer
    // is necessarily a locked one rather than an accelerated guess.
    let (_, exiting_acquisitions) =
        process_table_acquisitions_during(|| is_process_exiting(0x9601));
    assert!(
        exiting_acquisitions > 0,
        "an exiting process must be answered by the lifecycle authority"
    );

    detach_task(handle).expect("leader detach");
    detach_task(handle).expect("last thread detach");
    assert_eq!(reap_exited_processes(), 1);
}
