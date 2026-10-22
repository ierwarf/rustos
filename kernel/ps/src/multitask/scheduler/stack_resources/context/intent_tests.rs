use super::mm_tests::{PID, SLOT, admit, new_process, release};
use super::*;
use crate::multitask::current_identity::exec_intent;
use crate::multitask::scheduler::{
    ProcessAddressSpace, exec_transaction, runqueue, scheduling_runtime, stack_resources,
};
use crate::multitask::{current_identity as identity, process_table as process};
fn metadata(handle: process::ProcessHandle) -> identity::TaskIdentity {
    identity::TaskIdentity {
        task_id: Some(PID),
        user_mode: true,
        abi: Some(crate::user::abi::UserAbi::Linux),
        process_handle: Some(handle),
        process_id: Some(PID),
        console_session: crate::io::session::ConsoleSessionHandle::from_raw(0),
        pager_charge: None,
    }
}
fn initialize(handle: process::ProcessHandle) {
    admit(handle);
    runqueue::address_space::initialize(SLOT, 0x2000);
    runqueue::admit_blocked(SLOT);
    identity::clear(SLOT);
    identity::publish_metadata(SLOT, Some(metadata(handle)));
}
fn finish(handle: process::ProcessHandle) {
    release(SLOT);
    identity::clear(SLOT);
    process::detach_task(handle).unwrap();
    assert_eq!(process::reap_exited_processes(), 1);
}
#[test]
fn owned_peer_image_retains_gate_through_real_receipt_finalization_and_writer_release() {
    let _table = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    initialize(handle);
    let reservation = process::begin_exec(handle).unwrap();
    let intent = exec_intent::begin(SLOT, PID, reservation, false).unwrap();
    assert!(process::authorize_exec(reservation));
    assert!(!stack_resources::close_for_reclaim(SLOT));
    // Close was requested to witness refusal; restore only isolated fixture metadata.
    RESOURCES[SLOT].with(|r| r.closing = false);
    let finalized =
        exec_transaction::with_intent_image(&intent, reservation, 0x3000, false, |lease| {
            assert!(identity::lifecycle(SLOT).exec_quiesced);
            assert!(RESOURCES[SLOT].with(|r| r.frame_writing));
            let staged = process::stage_exec_state(
                reservation,
                ProcessAddressSpace::modeled_root_for_host(0x3000),
                crate::user::linux::LinuxProcessState::default(),
                crate::user::linux::LinuxMemoryMapState::new(),
                crate::user::linux::LinuxRuntimeProfile::new(),
                "/owned-intent.elf",
            )
            .unwrap();
            lease.bind_staged_mm(&staged);
            runqueue::address_space::replace(SLOT, 0x3000);
            let published = lease.publication_for_mm_test(handle);
            assert!(identity::lifecycle(SLOT).exec_quiesced);
            let finalized = process::finalize_exec_state(staged, published);
            assert!(identity::lifecycle(SLOT).exec_quiesced);
            assert!(exec_intent::busy(SLOT));
            finalized
        })
        .unwrap();
    assert!(!RESOURCES[SLOT].with(|r| r.frame_writing));
    assert!(identity::lifecycle(SLOT).exec_quiesced);
    intent.complete(&finalized);
    assert!(!exec_intent::busy(SLOT));
    assert!(!identity::lifecycle(SLOT).exec_quiesced);
    let (_, _, _, old) = finalized.into_parts();
    old.retire_execution_state();
    finish(handle);
}
#[test]
fn owned_intent_refuses_unqualified_writer_and_resource_teardown_before_any_effect() {
    let _table = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    initialize(handle);
    let reservation = process::begin_exec(handle).unwrap();
    let intent = exec_intent::begin(SLOT, PID, reservation, false).unwrap();
    assert!(
        stack_resources::frame_access::with_exec(SLOT, PID, |_| panic!(
            "unqualified image writer spent intent"
        ))
        .is_none()
    );
    assert!(std::panic::catch_unwind(|| clear(SLOT)).is_err());
    assert!(std::panic::catch_unwind(|| stack_resources::task_start::clear(SLOT)).is_err());
    assert!(std::panic::catch_unwind(|| identity::clear(SLOT)).is_err());
    assert!(std::panic::catch_unwind(|| runqueue::affinity_payload::reset_affinity(SLOT)).is_err());
    assert!(read(SLOT).is_some());
    assert!(
        exec_transaction::with_intent_image(
            &intent,
            process::test_exec_stamp(handle.generation(), 1, 99),
            0x3000,
            false,
            |_| panic!("foreign reservation entered image")
        )
        .is_none()
    );
    let cancelled = process::cancel_exec_owned(reservation).unwrap();
    intent.prepare_cancel();
    intent.cancel(cancelled);
    finish(handle);
}
#[test]
fn failed_image_admission_can_cancel_exact_process_but_old_credential_cannot_enter_next_exec() {
    let _table = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    initialize(handle);
    let first = process::begin_exec(handle).unwrap();
    let intent = exec_intent::begin(SLOT, PID, first, false).unwrap();
    let old = intent.credential();
    // ORDERING: native fixture models reader custody on the real counter.
    RESOURCES[SLOT].readers.store(1, Ordering::Release);
    assert!(
        exec_transaction::with_intent_image(&intent, first, 0x3000, false, |_| panic!(
            "reader admitted image"
        ))
        .is_none()
    );
    // ORDERING: native fixture models reader custody on the real counter.
    RESOURCES[SLOT].readers.store(0, Ordering::Release);
    intent.prepare_cancel();
    intent.cancel(process::cancel_exec_owned(first).unwrap());
    let next = process::begin_exec(handle).unwrap();
    let intent = exec_intent::begin(SLOT, PID, next, false).unwrap();
    assert!(!old.admits_image(SLOT, PID));
    assert!(!process::cancel_exec(first));
    assert!(
        stack_resources::frame_access::with_owned_exec(SLOT, PID, false, old, |_| panic!(
            "old credential mutated next exec image"
        ))
        .is_none()
    );
    assert!(identity::lifecycle(SLOT).exec_quiesced);
    intent.prepare_cancel();
    intent.cancel(process::cancel_exec_owned(next).unwrap());
    finish(handle);
}

#[test]
fn pending_exit_cancel_receipt_never_reopens_target_and_preserves_unrelated_stop() {
    let _table = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    initialize(handle);
    let reservation = process::begin_exec(handle).unwrap();
    let intent = exec_intent::begin(SLOT, PID, reservation, false).unwrap();
    identity::update_live_lifecycle(SLOT, PID, identity::LifecycleGate::JobStopped, true);
    assert_eq!(process::mark_process_exiting_once(PID), Some(false));
    let cancelled = process::cancel_exec_owned(reservation).unwrap();
    assert!(cancelled.terminal());
    intent.prepare_cancel();
    intent.cancel(cancelled);
    assert!(!exec_intent::busy(SLOT));
    assert!(identity::lifecycle(SLOT).exec_quiesced);
    assert!(identity::lifecycle(SLOT).job_stopped);
    assert!(process::begin_exec(handle).is_none());
    finish(handle);
}
#[test]
fn real_finalization_pending_exit_cannot_reopen_dispatch_after_owned_image_completion() {
    let _table = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    initialize(handle);
    let reservation = process::begin_exec(handle).unwrap();
    let intent = exec_intent::begin(SLOT, PID, reservation, false).unwrap();
    assert!(process::authorize_exec(reservation));
    let finalized =
        exec_transaction::with_intent_image(&intent, reservation, 0x3000, false, |lease| {
            let staged = process::stage_exec_state(
                reservation,
                ProcessAddressSpace::modeled_root_for_host(0x3000),
                crate::user::linux::LinuxProcessState::default(),
                crate::user::linux::LinuxMemoryMapState::new(),
                crate::user::linux::LinuxRuntimeProfile::new(),
                "/exit-intent.elf",
            )
            .unwrap();
            lease.bind_staged_mm(&staged);
            runqueue::address_space::replace(SLOT, 0x3000);
            let receipt = lease.publication_for_mm_test(handle);
            assert_eq!(process::mark_process_exiting_once(PID), Some(false));
            process::finalize_exec_state(staged, receipt)
        })
        .unwrap();
    assert!(finalized.exit_pending());
    intent.complete(&finalized);
    assert!(!exec_intent::busy(SLOT));
    assert!(identity::lifecycle(SLOT).exec_quiesced);
    runqueue::publish_local(SLOT, 3, 100);
    assert!(!runqueue::claim_dispatch(SLOT, 3, 100));
    let (_, _, _, old) = finalized.into_parts();
    old.retire_execution_state();
    finish(handle);
}
#[test]
fn self_intent_does_not_suspend_reschedulable_sibling_barrier_and_only_image_closes_gate() {
    let _table = process::tests::isolate_process_table();
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    initialize(handle);
    let current = crate::multitask::cpu_local::install_test_current_owner(0, SLOT);
    let reservation = process::begin_exec(handle).unwrap();
    let intent = exec_intent::begin(SLOT, PID, reservation, true).unwrap();
    assert!(!identity::lifecycle(SLOT).exec_quiesced);
    assert!(exec_intent::busy(SLOT));
    let image = intent.image(reservation).unwrap();
    assert!(identity::lifecycle(SLOT).exec_quiesced);
    image.finish(false);
    assert!(!identity::lifecycle(SLOT).exec_quiesced);
    intent.prepare_cancel();
    intent.cancel(process::cancel_exec_owned(reservation).unwrap());
    drop(current);
    finish(handle);
}

#[test]
fn self_cancellation_closes_before_process_wait_and_pins_cpu_until_matching_receipt() {
    let _table = process::tests::isolate_process_table();
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    initialize(handle);
    let current = crate::multitask::cpu_local::install_test_current_owner(0, SLOT);
    let reservation = process::begin_exec(handle).unwrap();
    let intent = exec_intent::begin(SLOT, PID, reservation, true).unwrap();
    assert!(!identity::lifecycle(SLOT).exec_quiesced);
    let preemption = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
    let outcome = intent.cancel_with(preemption, |stamp| {
        assert_eq!(stamp, reservation);
        assert!(identity::lifecycle(SLOT).exec_quiesced);
        assert_eq!(
            nucleus_core::util::lockdep::DispatchPreemption::model_depth_on(0),
            1
        );
        assert!(
            std::panic::catch_unwind(|| {
                nucleus_core::util::lockdep::DispatchPreemption::enter_model(0)
            })
            .is_err()
        );
        assert_eq!(
            nucleus_core::util::lockdep::DispatchPreemption::model_depth_on(0),
            1
        );
        process::cancel_exec_owned(stamp)
    });
    assert_eq!(outcome, (SLOT, PID, false));
    assert_eq!(
        nucleus_core::util::lockdep::DispatchPreemption::model_depth_on(0),
        0
    );
    assert!(!identity::lifecycle(SLOT).exec_quiesced);
    drop(current);
    finish(handle);
}

#[test]
fn completed_self_image_opens_only_after_writer_drop_inside_completion_scope() {
    let _table = process::tests::isolate_process_table();
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    let view = admit(handle);
    runqueue::address_space::initialize(SLOT, 0x2000);
    runqueue::admit_blocked(SLOT);
    identity::clear(SLOT);
    identity::publish_metadata(SLOT, Some(metadata(handle)));
    let current = crate::multitask::cpu_local::install_test_current_owner(0, SLOT);
    let reservation = process::begin_exec(handle).unwrap();
    let intent = exec_intent::begin(SLOT, PID, reservation, true).unwrap();
    assert!(process::authorize_exec(reservation));
    crate::multitask::process_placement::register_root(handle, SLOT, PID, 3).unwrap();
    let census =
        crate::multitask::process_placement::exec_membership::ExecMembership::begin(&intent)
            .unwrap();
    let pin = stack_resources::pin_view(SLOT, PID, view).unwrap();
    let finalized = stack_resources::cpu_leases::with_test_bank(0, pin, false, || {
        let result = exec_transaction::with_census_image(
            &intent,
            &census,
            reservation,
            0x3000,
            true,
            |lease| {
                let staged = process::stage_exec_state(
                    reservation,
                    ProcessAddressSpace::modeled_root_for_host(0x3000),
                    crate::user::linux::LinuxProcessState::default(),
                    crate::user::linux::LinuxMemoryMapState::new(),
                    crate::user::linux::LinuxRuntimeProfile::new(),
                    "/self-intent.elf",
                )
                .unwrap();
                census.mark_staged();
                lease.bind_staged_mm(&staged);
                runqueue::address_space::replace(SLOT, 0x3000);
                let receipt = lease.publication_for_mm_test(handle);
                let finalized = process::finalize_exec_state(staged, receipt);
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || intent.complete(&finalized)
                    ))
                    .is_err()
                );
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || census.assert_finalized(&finalized)
                    ))
                    .is_err()
                );
                assert!(identity::lifecycle(SLOT).exec_quiesced);
                assert!(exec_intent::busy(SLOT));
                Some(finalized)
            },
        )
        .unwrap()
        .unwrap();
        assert!(!RESOURCES[SLOT].with(|r| r.frame_writing));
        assert!(!exec_intent::busy(SLOT));
        assert!(!identity::lifecycle(SLOT).exec_quiesced);
        result
    });
    census.finish(&finalized);
    crate::multitask::process_placement::unregister(handle, SLOT, PID);
    drop(intent);
    drop(current);
    let (_, _, _, old) = finalized.into_parts();
    old.retire_execution_state();
    finish(handle);
}

#[test]
fn staged_self_exec_keeps_captured_placement_through_receipt_and_late_migration_until_cpu_ack() {
    let _table = process::tests::isolate_process_table();
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    let view = admit(handle);
    identity::clear(SLOT);
    identity::publish_metadata(SLOT, Some(metadata(handle)));
    runqueue::address_space::initialize(SLOT, 0x2000);
    runqueue::affinity_payload::initialize_affinity(SLOT, 1, 1);
    runqueue::admit_running(SLOT, 0);
    let mut retired = None;
    crate::multitask::cpu_local::with_test_activation_return(0, SLOT, SLOT, |phase| {
        if phase == 0 {
            identity::execution::banks::initialize(
                0,
                identity::execution::initial(SLOT, PID).unwrap(),
            );
            let reservation = process::begin_exec(handle).unwrap();
            let intent = exec_intent::begin(SLOT, PID, reservation, true).unwrap();
            assert!(process::authorize_exec(reservation));
            let pin = stack_resources::pin_view(SLOT, PID, view).unwrap();
            let finalized = stack_resources::cpu_leases::with_test_bank(0, pin, false, || {
                exec_transaction::with_completed_intent_image(
                    &intent,
                    reservation,
                    0x3000,
                    true,
                    |lease| {
                        let staged = process::stage_exec_state(
                            reservation,
                            ProcessAddressSpace::modeled_root_for_host(0x3000),
                            crate::user::linux::LinuxProcessState::default(),
                            crate::user::linux::LinuxMemoryMapState::new(),
                            crate::user::linux::LinuxRuntimeProfile::new(),
                            "/self-placement.elf",
                        )
                        .unwrap();
                        lease.bind_staged_mm(&staged);
                        runqueue::address_space::replace(SLOT, 0x3000);
                        runqueue::affinity_payload::set_affinity(SLOT, 2, 2, true);
                        let published = lease.publication_for_cpu_test(handle);
                        assert!(
                            crate::multitask::cpu_local::has_placement(0, SLOT),
                            "self receipt lost captured old placement"
                        );
                        Some(process::finalize_exec_state(staged, published))
                    },
                )
                .unwrap()
                .unwrap()
            });
            let (_, _, _, old) = finalized.into_parts();
            retired = Some(old);
        }
        assert!(runqueue::affinity_payload::affinity_snapshot(SLOT).2);
    });
    retired.take().unwrap().retire_execution_state();
    finish(handle);
}

#[test]
pub(in crate::multitask::scheduler) fn independent_exec_candidate_claim_rechecks_exact_uid_handle_pid_tid_and_mode_before_producer() {
    let _table = process::tests::isolate_process_table();
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    initialize(handle);
    assert_eq!(
        exec_transaction::peer_exec_candidate(PID, PID),
        Some((SLOT, handle))
    );
    assert!(exec_transaction::peer_exec_candidate(PID, PID + 1).is_none());
    let reservation = process::begin_exec(handle).unwrap();
    assert!(
        exec_transaction::claim_exec_candidate(Some((SLOT, PID, PID + 1)), reservation, true)
            .is_none()
    );
    assert!(
        exec_transaction::claim_exec_candidate(Some((SLOT, PID + 1, PID + 1)), reservation, true)
            .is_none()
    );
    assert!(!exec_intent::busy(SLOT));
    let (intent, current) =
        exec_transaction::claim_exec_candidate(Some((SLOT, PID, PID)), reservation, true).unwrap();
    assert!(!current);
    assert!(
        exec_transaction::claim_exec_candidate(Some((SLOT, PID, PID)), reservation, true).is_none()
    );
    intent.cancel_with(
        nucleus_core::util::lockdep::DispatchPreemption::enter_model(0),
        process::cancel_exec_owned,
    );
    let current_owner = crate::multitask::cpu_local::install_test_current_owner(0, SLOT);
    let reservation = process::begin_exec(handle).unwrap();
    let (intent, current) =
        exec_transaction::claim_exec_candidate(None, reservation, false).unwrap();
    assert!(current);
    intent.cancel_with(
        nucleus_core::util::lockdep::DispatchPreemption::enter_model(0),
        process::cancel_exec_owned,
    );
    drop(current_owner);
    finish(handle);
}

#[test]
fn terminal_producer_binding_retains_hidden_exact_uid_and_independently_denies_writer_close_and_physical_reuse_receipt()
 {
    let _table = process::tests::isolate_process_table();
    let _cpu = crate::multitask::cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let h = new_process();
    initialize(h);
    assert!(stack_resources::terminal_binding_matches(SLOT, Some(PID)));
    assert!(!stack_resources::terminal_binding_matches(SLOT, None));
    assert!(!stack_resources::terminal_binding_matches(
        SLOT,
        Some(PID + 1)
    ));
    for writer in [true, false] {
        RESOURCES[SLOT].with(|r| {
            if writer {
                r.frame_writing = true;
            } else {
                r.closing = true;
            }
        });
        assert!(!stack_resources::terminal_binding_matches(SLOT, Some(PID)));
        RESOURCES[SLOT].with(|r| {
            r.frame_writing = false;
            r.closing = false;
        });
    }
    let reclaim = stack_resources::reuse::begin(SLOT, Some(PID)).unwrap();
    RESOURCES[SLOT].with(|r| r.closing = false);
    assert!(!stack_resources::terminal_binding_matches(SLOT, Some(PID)));
    RESOURCES[SLOT].with(|r| r.closing = true);
    drop(reclaim);
    // Native recovery of injected close/poison, never production reopening.
    RESOURCES[SLOT].with(|r| {
        r.reclaim = None;
        r.closing = false;
    });
    let metadata = identity::read(SLOT);
    identity::publish_metadata(SLOT, None);
    assert!(stack_resources::terminal_binding_matches(SLOT, Some(PID)));
    identity::clear(SLOT);
    assert!(!stack_resources::terminal_binding_matches(SLOT, Some(PID)));
    identity::publish_metadata(SLOT, metadata);
    assert!(stack_resources::terminal_binding_matches(SLOT, Some(PID)));
    finish(h);
}

#[test]
fn independent_thread_creator_capture_requires_exact_context_start_and_live_unsealed_binding_before_facade()
 {
    let _table = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    initialize(handle);
    let pin = crate::multitask::scheduler::thread_creation::capture_creator(SLOT).unwrap();
    assert_eq!(pin.slot(), SLOT);
    assert_eq!(pin.id(), PID);
    drop(pin);
    identity::publish_metadata(SLOT, None);
    assert!(crate::multitask::scheduler::thread_creation::capture_creator(SLOT).is_none());
    identity::publish_metadata(SLOT, Some(metadata(handle)));
    let reservation = process::begin_exec(handle).unwrap();
    let intent = exec_intent::begin(SLOT, PID, reservation, false).unwrap();
    assert!(crate::multitask::scheduler::thread_creation::capture_creator(SLOT).is_none());
    let _ = intent.cancel_with(
        nucleus_core::util::lockdep::DispatchPreemption::enter_model(0),
        process::cancel_exec_owned,
    );
    identity::publish_lifecycle_change(
        SLOT,
        Some(metadata(handle)),
        identity::LifecycleGate::Retired,
        true,
    );
    assert!(crate::multitask::scheduler::thread_creation::capture_creator(SLOT).is_none());
    finish(handle);
}

#[test]
fn commit_grant_captures_real_ticket_epoch_parent_membership_and_original_mm_before_effects_and_stale_ticket_cannot_spend_it()
 {
    let _table = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    initialize(handle);
    crate::multitask::process_placement::register_root(handle, SLOT, PID, 3).unwrap();
    let mm = pin_thread_reservation_mm(SLOT, PID, handle, 0x2000).unwrap();
    const CHILD: usize = 114;
    let ticket = crate::multitask::scheduler::root_slots::reserve(CHILD, PID + 1).unwrap();
    assert!(
        crate::multitask::scheduler::thread_slots::CommitGrant::capture_ticket(
            &ticket,
            handle,
            (SLOT, PID),
            0x3000,
            crate::user::abi::UserAbi::Linux,
            &mm,
        )
        .is_none()
    );
    ticket.begin_commit();
    assert!(
        crate::multitask::scheduler::thread_slots::CommitGrant::capture_ticket(
            &ticket,
            handle,
            (SLOT, PID),
            0x2000,
            crate::user::abi::UserAbi::Linux,
            &mm
        )
        .is_none()
    );
    // Native-only phase recovery after independent final-publication rejection.
    ticket.set_phase_for_test(stack_resources::admission::Phase::Reserved);
    let grant = crate::multitask::scheduler::thread_slots::CommitGrant::capture_ticket(
        &ticket,
        handle,
        (SLOT, PID),
        0x2000,
        crate::user::abi::UserAbi::Linux,
        &mm,
    )
    .unwrap();
    assert!(grant.matches_ticket(&ticket));
    let pending = crate::multitask::process_placement::exit_membership::PendingExit::begin(
        handle,
        (SLOT, PID),
    )
    .unwrap();
    let pending = match pending.try_capture() {
        Err(p) => p,
        Ok(_) => panic!("exit ignored captured commit grant"),
    };
    let generation = ticket.generation();
    crate::multitask::scheduler::root_slots::cancel_unpublished(ticket);
    let next = crate::multitask::scheduler::root_slots::reserve(CHILD, PID + 1).unwrap();
    assert!(next.generation() > generation);
    assert!(!grant.matches_ticket(&next));
    drop(grant);
    let census = match pending.try_capture() {
        Ok(c) => c,
        Err(_) => panic!("cancelled commit retained membership"),
    };
    assert_eq!(census.members(), &[(SLOT, PID)]);
    drop(census);
    crate::multitask::scheduler::root_slots::cancel_unpublished(next);
    drop(mm);
    finish(handle);
}
#[test]
fn waiting_binding_retains_backing_but_terminal_stop_denies_fresh_admission_and_cannot_prove_drain_progress()
 {
    let _table = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    initialize(handle);
    let before = identity::execution::count(SLOT);
    let pin = identity::execution::pin_waiter(SLOT, PID).unwrap();
    assert_eq!(identity::execution::count(SLOT), before + 1);
    identity::publish_lifecycle_change(
        SLOT,
        Some(metadata(handle)),
        identity::LifecycleGate::Retired,
        true,
    );
    assert!(
        identity::execution::pin_creation(SLOT, PID, handle, crate::user::abi::UserAbi::Linux,)
            .is_none()
    );
    identity::publish_metadata(SLOT, None);
    assert!(stack_resources::reuse::begin(SLOT, Some(PID)).is_none());
    assert_eq!(identity::execution::count(SLOT), before + 1);
    drop(pin);
    assert_eq!(identity::execution::count(SLOT), before);
    finish(handle);
}
