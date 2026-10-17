use super::super::{runqueue, scheduling_runtime};
use super::*;
use crate::multitask::{cpu_local, current_identity as identity};
const SLOT: usize = 124;
const ID: u64 = 124001;
const CPU: usize = 4;
fn metadata(id: u64) -> identity::TaskIdentity {
    identity::TaskIdentity {
        task_id: Some(id),
        user_mode: false,
        abi: None,
        process_handle: None,
        process_id: None,
        console_session: crate::io::session::ConsoleSessionHandle::from_raw(0),
        pager_charge: None,
    }
}
fn fixture(f: impl FnOnce()) {
    let _cpu = cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    let banks = identity::execution::banks::isolate(CPU);
    runqueue::reset_before_publication();
    identity::clear(SLOT);
    tests::admit_context(SLOT, ID, None);
    identity::publish_metadata(SLOT, Some(metadata(ID)));
    runqueue::affinity_payload::initialize_affinity(SLOT, 1 << CPU, 1 << CPU);
    runqueue::admit_blocked(SLOT);
    runqueue::publish_local(SLOT, CPU, 100);
    f();
    drop(banks);
    assert!(close_for_reclaim(SLOT));
    task_start::clear(SLOT);
    drop(take_storage(SLOT));
    identity::clear(SLOT);
}

#[test]
fn initial_execution_and_physical_pins_refuse_every_constructor_phase_independently_before_bank_effects()
 {
    fixture(|| {
        for phase in [
            admission::Phase::Reserved,
            admission::Phase::Committing,
            admission::Phase::Canceling,
            admission::Phase::Poisoned,
        ] {
            admission::set_phase_for_test(SLOT, ID, Some(phase));
            assert!(identity::execution::initial(SLOT, ID).is_none());
            assert_eq!(identity::execution::count(SLOT), 0);
            assert!(cpu_leases::pin_initial(SLOT, ID).is_none());
            assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 0);
        }
        admission::set_phase_for_test(SLOT, ID, None);
        let execution = identity::execution::initial(SLOT, ID).unwrap();
        assert_eq!(identity::execution::count(SLOT), 1);
        drop(execution);
        let pin = cpu_leases::pin_initial(SLOT, ID).unwrap();
        assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 1);
        drop(pin);
    });
}

#[test]
fn initial_execution_and_physical_pins_refuse_writer_and_close_separately_without_spending_next_owner()
 {
    fixture(|| {
        for (writing, closing) in [(true, false), (false, true)] {
            RESOURCES[SLOT].with(|resources| {
                resources.frame_writing = writing;
                resources.closing = closing;
            });
            assert!(identity::execution::initial(SLOT, ID).is_none());
            assert_eq!(identity::execution::count(SLOT), 0);
            assert!(cpu_leases::pin_initial(SLOT, ID).is_none());
            assert_eq!(RESOURCES[SLOT].pins.load(Ordering::Acquire), 0);
        }
        RESOURCES[SLOT].with(|resources| {
            resources.frame_writing = false;
            resources.closing = false;
        });
        assert!(identity::execution::initial(SLOT, ID + 1).is_none());
        assert!(cpu_leases::pin_initial(SLOT, ID + 1).is_none());
        drop(identity::execution::initial(SLOT, ID).unwrap());
        drop(cpu_leases::pin_initial(SLOT, ID).unwrap());
    });
}
#[test]
fn lifecycle_stop_and_resume_gate_real_local_cas_without_directory_and_preserve_membership() {
    fixture(|| {
        for gate in [
            identity::LifecycleGate::Retired,
            identity::LifecycleGate::Suspended,
            identity::LifecycleGate::JobStopped,
            identity::LifecycleGate::ExecQuiesced,
        ] {
            identity::update_live_lifecycle(SLOT, ID, gate, true);
            let owner = runqueue::owner(SLOT);
            let sequence = runqueue::local_runnable_sequence(CPU);
            assert!(!runqueue::claim_dispatch_with_owner_generation(
                SLOT,
                CPU,
                100,
                Some(owner.generation)
            ));
            assert_eq!(runqueue::owner(SLOT), owner);
            assert_eq!(runqueue::local_runnable_sequence(CPU), sequence);
            assert_eq!(identity::execution::count(SLOT), 0);
            identity::update_live_lifecycle(SLOT, ID, gate, false);
        }
        assert!(runqueue::claim_dispatch_with_owner_generation(
            SLOT,
            CPU,
            100,
            Some(runqueue::owner(SLOT).generation)
        ));
        assert_eq!(identity::execution::count(SLOT), 1);
        assert!(
            super::super::smp::remote_task_requires_quiescence(SLOT, 0, false),
            "unpublished claim must delay remote exec/terminal completion"
        );
        assert!(!super::super::smp::remote_task_requires_quiescence(
            SLOT, SLOT, false
        ));
        assert!(
            !close_for_reclaim(SLOT),
            "claim-to-Prepared gap lost identity custody"
        );
        assert!(std::panic::catch_unwind(|| task_start::clear(SLOT)).is_err());
        assert!(std::panic::catch_unwind(|| identity::clear(SLOT)).is_err());
    });
}
#[test]
fn direct_handoff_refusal_preserves_exact_owner_and_failed_resource_admission_releases_permit() {
    fixture(|| {
        runqueue::publish_blocked(SLOT, CPU, 100);
        runqueue::publish_direct_handoff(SLOT, CPU);
        let owner = runqueue::owner(SLOT);
        identity::update_live_lifecycle(SLOT, ID, identity::LifecycleGate::JobStopped, true);
        assert!(!runqueue::claim_dispatch_with_owner_generation(
            SLOT,
            CPU,
            100,
            Some(owner.generation)
        ));
        assert_eq!(runqueue::owner(SLOT), owner);
        identity::update_live_lifecycle(SLOT, ID, identity::LifecycleGate::JobStopped, false);
        RESOURCES[SLOT].with(|r| r.frame_writing = true);
        assert!(!runqueue::claim_dispatch_with_owner_generation(
            SLOT,
            CPU,
            100,
            Some(owner.generation)
        ));
        assert_eq!(identity::execution::count(SLOT), 0);
        assert_eq!(runqueue::owner(SLOT), owner);
        RESOURCES[SLOT].with(|r| r.frame_writing = false);
        assert!(runqueue::claim_dispatch_with_owner_generation(
            SLOT,
            CPU,
            100,
            Some(owner.generation)
        ));
        assert_eq!(identity::execution::count(SLOT), 1);
    });
}
#[test]
fn claimed_lifecycle_excludes_peer_writer_even_before_stack_selection_is_parked() {
    fixture(|| {
        assert!(runqueue::claim_dispatch_with_owner_generation(
            SLOT,
            CPU,
            100,
            Some(runqueue::owner(SLOT).generation)
        ));
        identity::publish_lifecycle_change(
            SLOT,
            Some(metadata(ID)),
            identity::LifecycleGate::ExecQuiesced,
            true,
        );
        assert!(
            frame_access::with_exec(SLOT, ID, |_| panic!(
                "peer rewrote a claimed execution image"
            ))
            .is_none()
        );
        assert_eq!(identity::execution::count(SLOT), 1);
    });
}

#[test]
fn affinity_change_refuses_real_local_claim_without_directory_and_preserves_queue_generation() {
    fixture(|| {
        let before = runqueue::owner(SLOT);
        runqueue::affinity_payload::set_affinity(SLOT, 1, 1, true);
        assert!(!runqueue::claim_dispatch_with_owner_generation(
            SLOT,
            CPU,
            100,
            Some(before.generation)
        ));
        assert_eq!(runqueue::owner(SLOT), before);
        assert_eq!(identity::execution::count(SLOT), 0);
        assert!(runqueue::published_local_member_for_test(CPU, SLOT));
        runqueue::affinity_payload::set_affinity(SLOT, 1 << CPU, 1 << CPU, true);
        assert!(
            runqueue::claim_dispatch_with_owner_generation(SLOT, CPU, 100, Some(before.generation)),
            "migration intent is not a veto of an included destination CPU"
        );
        assert_eq!(identity::execution::count(SLOT), 1);
        assert!(!runqueue::published_local_member_for_test(CPU, SLOT));
    });
}
#[test]
fn affinity_change_refuses_real_direct_claim_without_consuming_handoff_custody() {
    fixture(|| {
        runqueue::publish_blocked(SLOT, CPU, 100);
        runqueue::publish_direct_handoff(SLOT, CPU);
        let before = runqueue::owner(SLOT);
        runqueue::affinity_payload::set_affinity(SLOT, 1, 1, false);
        assert!(!runqueue::claim_dispatch(SLOT, CPU, 100));
        assert_eq!(runqueue::owner(SLOT), before);
        assert_eq!(identity::execution::count(SLOT), 0);
        runqueue::affinity_payload::set_affinity(SLOT, 1 << CPU, 1 << CPU, false);
        assert!(runqueue::claim_dispatch(SLOT, CPU, 100));
        assert_eq!(identity::execution::count(SLOT), 1);
    });
}

#[test]
fn owned_cohort_gate_refuses_real_local_and_direct_cas_until_commit_preserving_exact_custody() {
    for direct in [false, true] {
        fixture(|| {
            if direct {
                runqueue::publish_blocked(SLOT, CPU, 100);
                runqueue::publish_direct_handoff(SLOT, CPU);
            }
            let before = runqueue::owner(SLOT);
            let mut cohort = identity::cohort::Cohort::begin(&[(SLOT, ID)]).unwrap();
            assert!(!runqueue::claim_dispatch(SLOT, CPU, 100));
            assert_eq!(runqueue::owner(SLOT), before);
            assert_eq!(identity::execution::count(SLOT), 1);
            cohort.begin_effects();
            assert!(
                std::panic::catch_unwind(|| runqueue::affinity_payload::set_affinity(
                    SLOT, 1, 1, true
                ))
                .is_err()
            );
            let other = SLOT + 1;
            identity::clear(other);
            identity::publish_metadata(other, Some(metadata(ID + 1)));
            let foreign = identity::cohort::Cohort::begin(&[(other, ID + 1)]).unwrap();
            let mask_before = runqueue::affinity_payload::affinity_snapshot(SLOT);
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    runqueue::affinity_payload::set_affinity_owned(SLOT, ID, 1, 1, true, &foreign)
                }))
                .is_err()
            );
            assert_eq!(
                runqueue::affinity_payload::affinity_snapshot(SLOT),
                mask_before
            );
            drop(foreign);
            identity::clear(other);
            runqueue::affinity_payload::set_affinity_owned(
                SLOT,
                ID,
                1 << CPU,
                1 << CPU,
                false,
                &cohort,
            );
            assert!(!runqueue::claim_dispatch(SLOT, CPU, 100));
            assert_eq!(runqueue::owner(SLOT), before);
            if !direct {
                assert!(runqueue::published_local_member_for_test(CPU, SLOT));
            }
            cohort.complete();
            assert!(runqueue::claim_dispatch(SLOT, CPU, 100));
            assert_eq!(identity::execution::count(SLOT), 1);
        });
    }
}

#[test]
fn reserved_slot_refuses_real_local_and_direct_cas_in_every_phase_without_spending_queue_or_uid() {
    for direct in [false, true] {
        fixture(|| {
            if direct {
                runqueue::publish_blocked(SLOT, CPU, 100);
                runqueue::publish_direct_handoff(SLOT, CPU);
            }
            let before = runqueue::owner(SLOT);
            for phase in [
                admission::Phase::Reserved,
                admission::Phase::Committing,
                admission::Phase::Canceling,
                admission::Phase::Poisoned,
            ] {
                // Isolated model of the producer's unpublished descriptor. All
                // other real Local/Direct qualifiers match this exact UID.
                admission::set_phase_for_test(SLOT, ID, Some(phase));
                assert!(!runqueue::claim_dispatch(SLOT, CPU, 100));
                assert_eq!(runqueue::owner(SLOT), before);
                assert_eq!(identity::execution::count(SLOT), 0);
                if !direct {
                    assert!(runqueue::published_local_member_for_test(CPU, SLOT));
                }
            }
            admission::set_phase_for_test(SLOT, ID, None); // model release, not production ACK
            assert!(runqueue::claim_dispatch(SLOT, CPU, 100));
        });
    }
}

#[test]
fn cold_binding_pin_refuses_active_writer_closing_and_foreign_resource_uid_before_any_seal() {
    fixture(|| {
        RESOURCES[SLOT].with(|resources| resources.frame_writing = true);
        assert!(identity::cohort::Cohort::begin(&[(SLOT, ID)]).is_none());
        RESOURCES[SLOT].with(|resources| {
            resources.frame_writing = false;
            resources.closing = true;
        });
        assert!(identity::cohort::Cohort::begin(&[(SLOT, ID)]).is_none());
        RESOURCES[SLOT].with(|resources| {
            resources.closing = false;
            resources.start.as_mut().unwrap().id = ID + 1;
        });
        assert!(identity::cohort::Cohort::begin(&[(SLOT, ID)]).is_none());
        assert!(!identity::cohort::busy(SLOT));
        assert_eq!(identity::execution::count(SLOT), 0);
        RESOURCES[SLOT].with(|resources| resources.start.as_mut().unwrap().id = ID);
    });
}
#[test]
fn affinity_reset_cannot_discard_captured_execution_and_same_uid_exec_preserves_binding() {
    fixture(|| {
        assert!(runqueue::claim_dispatch(SLOT, CPU, 100));
        let masks = runqueue::affinity_payload::affinity_snapshot(SLOT);
        assert!(
            std::panic::catch_unwind(|| runqueue::affinity_payload::reset_affinity(SLOT)).is_err()
        );
        assert_eq!(runqueue::affinity_payload::affinity_snapshot(SLOT), masks);
        runqueue::affinity_payload::set_affinity(SLOT, 1 << CPU, 1 << CPU, false);
        assert_eq!(identity::execution::count(SLOT), 1);
    });
}

#[test]
fn captured_placement_survives_new_exclusion_and_unchanged_ack_cannot_erase_new_migration() {
    fixture(|| {
        cpu_local::with_test_activation_return(CPU, SLOT, SLOT, |phase| {
            if phase == 0 {
                identity::execution::banks::initialize(
                    CPU,
                    identity::execution::initial(SLOT, ID).unwrap(),
                );
                assert!(runqueue::claim_dispatch(SLOT, CPU, 100));
                assert!(cpu_local::has_placement(CPU, SLOT));
                runqueue::affinity_payload::set_affinity(SLOT, 1, 1, true);
                assert!(
                    cpu_local::has_placement(CPU, SLOT),
                    "desired exclusion revoked captured return"
                );
            }
            assert!(runqueue::affinity_payload::affinity_snapshot(SLOT).2);
            if phase == 2 {
                assert!(identity::execution::banks::owns_execution(CPU, SLOT));
                assert!(
                    runqueue::affinity_payload::capture_current(SLOT, CPU).is_some(),
                    "installed Current permission was lost at the first ACK"
                );
                assert!(
                    runqueue::affinity_payload::with_placement_admission(
                        SLOT,
                        Some(ID),
                        CPU,
                        || panic!("new claim ignored desired exclusion")
                    )
                    .is_none()
                );
                runqueue::affinity_payload::set_affinity(SLOT, 1 << CPU, 1 << CPU, true);
                runqueue::set_runnable(SLOT, false);
                assert!(
                    runqueue::affinity_payload::with_placement_admission(
                        SLOT,
                        Some(ID),
                        CPU,
                        || ()
                    )
                    .is_some()
                );
                assert!(
                    runqueue::affinity_payload::capture_current(SLOT, CPU).is_none(),
                    "stale installed permission was treated as missing bootstrap authority"
                );
            }
        });
    });
}
#[test]
fn current_only_placement_capture_acknowledges_only_its_exact_migration_version() {
    fixture(|| {
        runqueue::retire(SLOT, 100);
        runqueue::release_retired(SLOT);
        runqueue::affinity_payload::initialize_affinity(SLOT, 1 << CPU, 1 << CPU);
        runqueue::admit_running(SLOT, CPU);
        runqueue::affinity_payload::set_affinity(SLOT, 1 << CPU, 1 << CPU, true);
        cpu_local::with_test_activation_return(CPU, SLOT, SLOT, |phase| {
            if phase == 0 {
                identity::execution::banks::initialize(
                    CPU,
                    identity::execution::initial(SLOT, ID).unwrap(),
                );
                assert!(runqueue::affinity_payload::capture_architecture(SLOT, CPU));
            }
            if phase <= 1 {
                assert!(runqueue::affinity_payload::affinity_snapshot(SLOT).2);
            } else {
                assert!(!runqueue::affinity_payload::affinity_snapshot(SLOT).2);
            }
        });
    });
}
#[test]
fn changed_rsp_ack_keeps_late_affinity_request_and_releases_only_outgoing_identity() {
    fixture(|| {
        let from = SLOT + 1;
        identity::clear(from);
        identity::publish_metadata(from, Some(metadata(ID + 1)));
        cpu_local::with_test_activation_return(CPU, from, SLOT, |phase| {
            if phase == 0 {
                identity::execution::banks::initialize(
                    CPU,
                    identity::execution::initial(from, ID + 1).unwrap(),
                );
                assert!(runqueue::claim_dispatch(SLOT, CPU, 100));
                runqueue::affinity_payload::set_affinity(SLOT, 1 << CPU, 1 << CPU, true);
            }
            assert!(runqueue::affinity_payload::affinity_snapshot(SLOT).2);
            assert_eq!(identity::execution::count(SLOT), 1);
            assert_eq!(identity::execution::count(from), u32::from(phase < 3));
        });
        identity::clear(from);
    });
}
#[test]
fn placement_owner_republication_refuses_return_before_any_placement_ack() {
    fixture(|| {
        cpu_local::with_test_rejected_activation_return(
            CPU,
            SLOT,
            SLOT,
            || {
                identity::execution::banks::initialize(
                    CPU,
                    identity::execution::initial(SLOT, ID).unwrap(),
                );
                assert!(runqueue::claim_dispatch(SLOT, CPU, 100));
                runqueue::affinity_payload::set_affinity(SLOT, 1 << CPU, 1 << CPU, true);
                runqueue::set_runnable(SLOT, false);
                assert!(!cpu_local::has_placement(CPU, SLOT));
            },
            || {
                assert!(runqueue::affinity_payload::affinity_snapshot(SLOT).2);
                assert_eq!(identity::execution::count(SLOT), 2);
            },
        );
    });
}
