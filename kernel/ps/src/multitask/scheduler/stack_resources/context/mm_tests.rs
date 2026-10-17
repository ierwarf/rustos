//! Real resource/process/MM owners; modeled host roots do not execute CR3/TLB.
use super::tests::metadata;
use super::*;
pub(super) fn release(slot: usize) {
    assert!(super::super::close_for_reclaim(slot));
    clear(slot);
    super::super::task_start::clear(slot);
    drop(super::super::take_storage(slot));
}
use crate::memory::paging::ProcessAddressSpace;
use crate::multitask::scheduler::{
    exec_transaction, runqueue, scheduling_runtime, stack_resources,
};
use crate::multitask::{current_identity, process_table as process};
use process::ProcessHandle;
pub(super) const SLOT: usize = 115;
pub(super) const PID: u64 = 921_115;
pub(super) fn new_process() -> ProcessHandle {
    process::create_process(
        PID,
        crate::multitask::UserProcessState::new(
            ProcessAddressSpace::modeled_root_for_host(0x2000),
            Some(crate::user::linux::LinuxProcessState::default()),
            None,
            None,
            None,
            false,
            "/mm-test.elf",
        ),
    )
    .unwrap()
}

fn stage(handle: ProcessHandle, root: u64) -> (process::ExecReservation, process::StagedExec) {
    let reservation = process::begin_exec(handle).unwrap();
    assert!(process::authorize_exec(reservation));
    let staged = process::stage_exec_state(
        reservation,
        ProcessAddressSpace::modeled_root_for_host(root),
        crate::user::linux::LinuxProcessState::default(),
        crate::user::linux::LinuxMemoryMapState::new(),
        crate::user::linux::LinuxRuntimeProfile::new(),
        "/mm-owner.elf",
    )
    .unwrap();
    (reservation, staged)
}
pub(super) fn admit(
    handle: ProcessHandle,
) -> super::super::super::context_validation::PublishedFrameView {
    let mut context = metadata(SLOT, PID);
    context.process_handle = Some(handle);
    super::super::tests::admit_context(SLOT, PID, Some(context))
}
#[test]
fn staged_mm_closes_old_selection_but_cpu_bank_retains_old_root_until_context_release() {
    let _isolation = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    let view = admit(handle);
    runqueue::address_space::initialize(SLOT, 0x2000);
    runqueue::admit_blocked(SLOT);
    let selected = stack_resources::pin_view(SLOT, PID, view).unwrap();
    let captured_root = stack_resources::cpu_leases::with_test_bank(3, selected, false, || {
        let captured_root = lease_execution_root(SLOT, 0x2000, 3).unwrap();
        assert_eq!(captured_root.root(), 0x2000);
        assert!(execution_root_is_current(SLOT, 0x2000, 3));
        assert!(!execution_root_is_current(SLOT, 0x3000, 3));
        let (reservation, staged) = stage(handle, 0x3000);
        assert!(!execution_root_is_current(SLOT, 0x2000, 3));
        assert!(stack_resources::pin_view(SLOT, PID, view).is_none());
        assert!(process::pin_process_mm(handle, PID).is_none());
        let next = staged.pin_execution_mm(reservation, 0x3000);
        assert!(next.matches_exec(reservation, 0x3000));
        assert!(!next.matches_exec(reservation, 0));
        let finalized = process::finalize_exec_state(
            staged,
            exec_transaction::test_publication(reservation, 0x3000),
        );
        let (_, _, _, old) = finalized.into_parts();
        old.retire_execution_state(); // old owner gone; the real resource MM pin still retains root
        assert!(RESOURCES[SLOT].with(|resources| {
            resources
                .mm_pin
                .as_ref()
                .is_some_and(|pin| pin.root() == 0x2000 && !pin.is_open())
        }));
        assert!(std::panic::catch_unwind(|| clear(SLOT)).is_err());
        assert!(!captured_root._pin.as_ref().unwrap().is_open());
        assert!(lease_execution_root(SLOT, 0x2000, 3).is_none());
        captured_root
    });
    assert!(!execution_root_is_current(SLOT, 0x2000, 3));
    release(SLOT);
    // Copied root authority retains its own physical MM outside the resource
    // leaf/context; it is not fresh permission to reactivate a sealed root.
    assert_eq!(captured_root._pin.as_ref().unwrap().root(), 0x2000);
    drop(captured_root);
    process::detach_task(handle).unwrap();
    assert_eq!(process::reap_exited_processes(), 1);
}
#[test]
fn exec_receipt_moves_mm_generation_after_root_publication_and_before_old_state_release() {
    let _isolation = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let handle = new_process();
    let view = admit(handle);
    runqueue::address_space::initialize(SLOT, 0x2000);
    runqueue::admit_blocked(SLOT);
    current_identity::clear(SLOT);
    current_identity::publish_lifecycle_change(
        SLOT,
        None,
        current_identity::LifecycleGate::ExecQuiesced,
        true,
    );
    let old_mm = process::pin_process_mm(handle, PID).unwrap();
    let reservation = process::begin_exec(handle).unwrap();
    assert!(!old_mm.matches_exec(reservation, 0x2000));
    assert!(process::authorize_exec(reservation));
    let finalized = exec_transaction::with_image(SLOT, PID, reservation, 0x3000, false, |lease| {
        let staged = process::stage_exec_state(
            reservation,
            ProcessAddressSpace::modeled_root_for_host(0x3000),
            crate::user::linux::LinuxProcessState::default(),
            crate::user::linux::LinuxMemoryMapState::new(),
            crate::user::linux::LinuxRuntimeProfile::new(),
            "/joint-mm.elf",
        )
        .unwrap();
        lease.bind_staged_mm(&staged);
        assert!(RESOURCES[SLOT].with(|resources| !resources.mm_pin.as_ref().unwrap().is_open()));
        runqueue::address_space::replace(SLOT, 0x3000);
        let receipt = lease.publication_for_mm_test(handle);
        assert!(RESOURCES[SLOT].with(|resources| {
            resources
                .mm_pin
                .as_ref()
                .is_some_and(|pin| pin.matches_exec(reservation, 0x3000))
        }));
        process::finalize_exec_state(staged, receipt)
    })
    .unwrap();
    let (_, _, _, old) = finalized.into_parts();
    old.retire_execution_state();
    let frame = stack_resources::pin_view(SLOT, PID, view).unwrap();
    stack_resources::cpu_leases::with_test_bank(3, frame, true, || {
        assert!(execution_root_is_current(SLOT, 0x3000, 3));
        assert!(!execution_root_is_current(SLOT, 0x2000, 3));
        RESOURCES[SLOT].with(|resources| resources.context.as_mut().unwrap().process_handle = None);
        assert!(!execution_root_is_current(SLOT, 0x3000, 3));
        RESOURCES[SLOT]
            .with(|resources| resources.context.as_mut().unwrap().process_handle = Some(handle));
    });
    assert!(!execution_root_is_current(SLOT, 0x3000, 3));
    drop(old_mm);
    release(SLOT);
    current_identity::clear(SLOT);
    process::detach_task(handle).unwrap();
    assert_eq!(process::reap_exited_processes(), 1);
}

fn stage_attempt(reservation: process::ExecReservation) -> Option<process::StagedExec> {
    process::stage_exec_state(
        reservation,
        ProcessAddressSpace::modeled_root_for_host(0x3000),
        crate::user::linux::LinuxProcessState::default(),
        crate::user::linux::LinuxMemoryMapState::new(),
        crate::user::linux::LinuxRuntimeProfile::new(),
        "/thread-mm-owner.elf",
    )
}

#[test]
fn unpublished_thread_mm_capture_retains_old_root_and_closed_exec_cannot_reactivate_it() {
    let _table = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    let handle = new_process();
    admit(handle);
    let ticket_mm = pin_thread_reservation_mm(SLOT, PID, handle, 0x2000).unwrap();
    assert!(pin_thread_reservation_mm(SLOT, PID, handle, 0x3000).is_none());
    assert!(pin_thread_reservation_mm(SLOT, PID + 1, handle, 0x2000).is_none());
    assert!(
        pin_thread_reservation_mm(
            SLOT,
            PID,
            process::ProcessHandle::new(handle.index(), handle.generation() + 1),
            0x2000
        )
        .is_none()
    );
    let reservation = process::begin_exec(handle).unwrap();
    assert!(process::authorize_exec(reservation));
    let staged = stage_attempt(reservation).unwrap();
    assert!(
        crate::multitask::scheduler::thread_slots::capture_commit_mm(Some(&ticket_mm)).is_none(),
        "closed reservation MM reopened after exec"
    );
    assert!(ticket_mm.root() == 0x2000);
    let finalized = process::finalize_exec_state(
        staged,
        crate::multitask::scheduler::exec_transaction::test_publication(reservation, 0x3000),
    );
    let (_, _, _, old) = finalized.into_parts();
    old.retire_execution_state();
    release(SLOT);
    drop(ticket_mm);
    process::detach_task(handle).unwrap();
    assert_eq!(process::reap_exited_processes(), 1);
}
#[test]
fn unpublished_thread_commit_mm_admission_excludes_exec_staging_until_commit_releases() {
    let _table = process::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    let handle = new_process();
    admit(handle);
    let ticket_mm = pin_thread_reservation_mm(SLOT, PID, handle, 0x2000).unwrap();
    let commit = crate::multitask::scheduler::thread_slots::capture_commit_mm(Some(&ticket_mm))
        .unwrap()
        .unwrap();
    let reservation = process::begin_exec(handle).unwrap();
    assert!(process::authorize_exec(reservation));
    assert!(
        stage_attempt(reservation).is_none(),
        "exec crossed admitted prepublication thread commit"
    );
    assert!(ticket_mm.is_open());
    drop(commit);
    let staged = stage_attempt(reservation).unwrap();
    let finalized = process::finalize_exec_state(
        staged,
        crate::multitask::scheduler::exec_transaction::test_publication(reservation, 0x3000),
    );
    let (_, _, _, old) = finalized.into_parts();
    old.retire_execution_state();
    release(SLOT);
    drop(ticket_mm);
    process::detach_task(handle).unwrap();
    assert_eq!(process::reap_exited_processes(), 1);
}
