use super::mm_tests::{PID, SLOT, admit, new_process, release};
use super::*;
use crate::memory::paging::ProcessAddressSpace;
use crate::multitask::scheduler::{exec_transaction, runqueue, scheduling_runtime};
use crate::multitask::{cpu_local, process_table as process};

fn attempt(reservation: process::ExecReservation) -> Option<process::StagedExec> {
    process::stage_exec_state(
        reservation,
        ProcessAddressSpace::modeled_root_for_host(0x3000),
        crate::user::linux::LinuxProcessState::default(),
        crate::user::linux::LinuxMemoryMapState::new(),
        crate::user::linux::LinuxRuntimeProfile::new(),
        "/activation-exec.elf",
    )
}
#[test]
fn local_and_direct_mm_grant_block_real_staging_through_publication_until_rsp_ack() {
    let _isolation = process::tests::isolate_process_table();
    let _cpu = cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    for direct in [false, true] {
        runqueue::reset_before_publication();
        let handle = new_process();
        let _view = admit(handle);
        runqueue::address_space::initialize(SLOT, 0x2000);
        if direct {
            runqueue::admit_blocked(SLOT);
            runqueue::publish_direct_handoff(SLOT, 3);
        } else {
            runqueue::admit_blocked(SLOT);
            runqueue::publish_local(SLOT, 3, 100);
        }
        let reservation = process::begin_exec(handle).unwrap();
        assert!(process::authorize_exec(reservation));
        let mut staged = None;
        cpu_local::with_test_activation_return(3, 116, SLOT, |phase| {
            if phase == 0 {
                let generation = runqueue::owner(SLOT).generation;
                assert!(runqueue::claim_dispatch_with_owner_generation(
                    SLOT,
                    3,
                    100,
                    Some(generation)
                ));
            }
            if phase < 3 {
                assert!(
                    attempt(reservation).is_none(),
                    "MM closed during activation phase={phase}"
                );
                let old = process::pin_process_mm(handle, PID).unwrap();
                assert!(old.is_open());
                assert_eq!(old.root(), 0x2000);
                assert!(RESOURCES[SLOT].with(|r| r.mm_pin.as_ref().unwrap().is_open()));
            } else {
                staged = attempt(reservation);
                assert!(staged.is_some());
            }
        });
        let result = process::finalize_exec_state(
            staged.unwrap(),
            exec_transaction::test_publication(reservation, 0x3000),
        );
        let (_, _, _, old) = result.into_parts();
        old.retire_execution_state();
        release(SLOT);
        process::detach_task(handle).unwrap();
        assert_eq!(process::reap_exited_processes(), 1);
    }
}
#[test]
fn architecture_only_capture_blocks_mm_close_without_a_fresh_runqueue_claim() {
    let _isolation = process::tests::isolate_process_table();
    let _cpu = cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    let view = admit(handle);
    runqueue::address_space::initialize(SLOT, 0x2000);
    runqueue::admit_blocked(SLOT);
    let pin = super::super::pin_view(SLOT, PID, view).unwrap();
    let reservation = process::begin_exec(handle).unwrap();
    assert!(process::authorize_exec(reservation));
    super::super::cpu_leases::with_test_bank(3, pin, false, || {
        cpu_local::with_test_activation_return(3, SLOT, SLOT, |phase| {
            if phase == 0 {
                let lease = lease_for_architecture(SLOT, 0x2000, 3).unwrap();
                assert_eq!(lease.root(), 0x2000);
                drop(lease);
                assert!(attempt(reservation).is_none());
            }
            if phase == 1 {
                assert!(attempt(reservation).is_none());
            }
            // Unchanged completion needs no RSP edge and releases the grant.
            if phase == 2 {
                assert!(process::cancel_exec(reservation));
            }
        });
    });
    let next = process::begin_exec(handle).unwrap();
    assert!(process::authorize_exec(next));
    let staged = attempt(next).unwrap();
    let result =
        process::finalize_exec_state(staged, exec_transaction::test_publication(next, 0x3000));
    let (_, _, _, old) = result.into_parts();
    old.retire_execution_state();
    release(SLOT);
    process::detach_task(handle).unwrap();
    assert_eq!(process::reap_exited_processes(), 1);
}

#[test]
fn failed_cas_releases_provisional_mm_grant_without_committing_owner_or_membership() {
    let _isolation = process::tests::isolate_process_table();
    let _cpu = cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    let _view = admit(handle);
    runqueue::admit_blocked(SLOT);
    runqueue::publish_local(SLOT, 3, 100);
    let owner = runqueue::owner(SLOT);
    let sequence = runqueue::local_runnable_sequence(3);
    let reservation = process::begin_exec(handle).unwrap();
    assert!(process::authorize_exec(reservation));
    let mut staged = None;
    cpu_local::with_test_activation_return(3, 116, 116, |phase| {
        if phase != 0 {
            return;
        }
        let refused =
            super::super::frame_readers::with_claim_activation(SLOT, 3, || Err::<(), ()>(()))
                .unwrap().into_claimed().unwrap();
        assert!(refused.0.is_err());
        assert!(refused.1.is_some());
        assert!(attempt(reservation).is_none());
        assert_eq!(runqueue::owner(SLOT), owner);
        assert_eq!(runqueue::local_runnable_sequence(3), sequence);
        assert_eq!(runqueue::published_runnable_count(3), 1);
        drop(refused); // no resource leaf spans cancellation
        staged = attempt(reservation);
        assert!(staged.is_some());
    });
    let result = process::finalize_exec_state(
        staged.unwrap(),
        exec_transaction::test_publication(reservation, 0x3000),
    );
    let (_, _, _, old) = result.into_parts();
    old.retire_execution_state();
    release(SLOT);
    process::detach_task(handle).unwrap();
    assert_eq!(process::reap_exited_processes(), 1);
}

#[test]
fn republished_owner_cannot_spend_same_uid_mm_grant_and_poison_keeps_close_excluded() {
    let _isolation = process::tests::isolate_process_table();
    let _cpu = cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    let _view = admit(handle);
    runqueue::admit_blocked(SLOT);
    runqueue::publish_local(SLOT, 3, 100);
    let reservation = process::begin_exec(handle).unwrap();
    assert!(process::authorize_exec(reservation));
    cpu_local::with_test_rejected_activation_return(
        3,
        116,
        SLOT,
        || {
            assert!(runqueue::claim_dispatch_with_owner_generation(
                SLOT,
                3,
                100,
                Some(runqueue::owner(SLOT).generation)
            ));
            let claimed = runqueue::owner(SLOT);
            runqueue::set_runnable(SLOT, false);
            assert_ne!(runqueue::owner(SLOT), claimed);
        },
        || {
            assert!(
                attempt(reservation).is_none(),
                "poison silently cancelled a committed MM grant"
            );
            assert!(process::pin_process_mm(handle, PID).unwrap().is_open());
        },
    );
    // Only isolated host fixture recovery released poison, not a production ACK.
    let staged = attempt(reservation).unwrap();
    let result = process::finalize_exec_state(
        staged,
        exec_transaction::test_publication(reservation, 0x3000),
    );
    let (_, _, _, old) = result.into_parts();
    old.retire_execution_state();
    release(SLOT);
    process::detach_task(handle).unwrap();
    assert_eq!(process::reap_exited_processes(), 1);
}

#[test]
fn replaced_uid_cannot_spend_unchanged_owner_and_mm_activation_identity() {
    let _isolation = process::tests::isolate_process_table();
    let _cpu = cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    let _view = admit(handle);
    runqueue::admit_blocked(SLOT);
    runqueue::publish_local(SLOT, 3, 100);
    let reservation = process::begin_exec(handle).unwrap();
    assert!(process::authorize_exec(reservation));
    cpu_local::with_test_rejected_activation_return(
        3,
        116,
        SLOT,
        || {
            assert!(runqueue::claim_dispatch_with_owner_generation(
                SLOT,
                3,
                100,
                Some(runqueue::owner(SLOT).generation)
            ));
            // Corrupt only the fixture descriptor: the production writer forbids
            // rebinding. This independently exercises the return UID qualifier.
            RESOURCES[SLOT].with(|r| r.start.as_mut().unwrap().id += 1);
        },
        || {
            assert!(attempt(reservation).is_none());
        },
    );
    RESOURCES[SLOT].with(|r| r.start.as_mut().unwrap().id = PID);
    let staged = attempt(reservation).unwrap();
    let result = process::finalize_exec_state(
        staged,
        exec_transaction::test_publication(reservation, 0x3000),
    );
    let (_, _, _, old) = result.into_parts();
    old.retire_execution_state();
    release(SLOT);
    process::detach_task(handle).unwrap();
    assert_eq!(process::reap_exited_processes(), 1);
}

#[test]
fn staged_exec_activation_spans_direct_hardware_receipt_and_cpu_return_completion() {
    let _isolation = process::tests::isolate_process_table();
    let _cpu = cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    let view = admit(handle);
    runqueue::address_space::initialize(SLOT, 0x2000);
    runqueue::admit_blocked(SLOT);
    let pin = super::super::pin_view(SLOT, PID, view).unwrap();
    let reservation = process::begin_exec(handle).unwrap();
    assert!(process::authorize_exec(reservation));
    let mut next = None;
    let mut second_staged = None;
    super::super::cpu_leases::with_test_bank(0, pin, false, || {
        cpu_local::with_test_activation_return(0, SLOT, SLOT, |phase| {
            if phase == 0 {
                assert!(
                    exec_transaction::with_image(SLOT, PID, reservation, 0x3000, true, |lease| {
                        let staged = attempt(reservation).unwrap();
                        lease.bind_staged_mm(&staged);
                        let held = process::retain_process(handle).unwrap();
                        assert!(
                            !held.with_state(|_, state| state.address_space().seal_execution()),
                            "staged nonce masked missing direct-hardware MM activation"
                        );
                        runqueue::address_space::replace(SLOT, 0x3000);
                        let receipt = lease.publication_for_cpu_test(handle);
                        let result = process::finalize_exec_state(staged, receipt);
                        let (_, _, _, old) = result.into_parts();
                        old.retire_execution_state();
                    })
                    .is_some()
                );
                let pending = process::begin_exec(handle).unwrap();
                assert!(process::authorize_exec(pending));
                next = Some(pending);
            }
            if phase < 2 {
                assert!(attempt(next.unwrap()).is_none());
            }
            if phase == 2 {
                second_staged = attempt(next.unwrap());
                assert!(second_staged.is_some());
            }
        });
    });
    let result = process::finalize_exec_state(
        second_staged.unwrap(),
        exec_transaction::test_publication(next.unwrap(), 0x3000),
    );
    let (_, _, _, old) = result.into_parts();
    old.retire_execution_state();
    release(SLOT);
    process::detach_task(handle).unwrap();
    assert_eq!(process::reap_exited_processes(), 1);
}
