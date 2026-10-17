//! Real table/state staging and receipt consumption; empty host MMs deliberately
//! do not claim hardware CR3/TLB or complete image/dispatch refinement.
use super::tests::{isolate_process_table, new_state};
use super::*;
use crate::multitask::scheduler::exec_transaction::test_publication;

fn staged(handle: ProcessHandle) -> (ExecReservation, StagedExec) {
    let reservation = begin_exec(handle).unwrap();
    assert!(authorize_exec(reservation));
    let staged = stage_exec_state(
        reservation,
        ProcessAddressSpace::empty_for_tests(),
        crate::user::linux::LinuxProcessState::default(),
        crate::user::linux::LinuxMemoryMapState::new(),
        crate::user::linux::LinuxRuntimeProfile::new(),
        "/replacement.elf",
    )
    .unwrap();
    (reservation, staged)
}
#[test]
fn real_staging_hides_new_mm_until_exact_receipt_finalizes_and_preserves_pending_exit() {
    let _isolation = isolate_process_table();
    let handle = create_process(921_001, new_state()).unwrap();
    let (reservation, staged) = staged(handle);
    assert!(!process_state_is_visible(handle));
    {
        let table = PROCESS_TABLE.lock();
        let object = table.slots[handle.index()].object.as_deref().unwrap();
        assert!(object.exec_state_staged);
        assert_eq!(object.mm_generation, reservation.next_mm_generation());
    }
    assert_eq!(mark_process_exiting_once(921_001), Some(false));
    let finalized = finalize_exec_state(staged, test_publication(reservation, 0));
    let (pid, exit_pending, _, old_state) = finalized.into_parts();
    assert_eq!(pid, 921_001);
    assert!(exit_pending);
    drop(old_state);
    {
        let table = PROCESS_TABLE.lock();
        let object = table.slots[handle.index()].object.as_deref().unwrap();
        assert!(!object.exec_state_staged && !object.exec_in_progress);
        assert!(object.exiting);
    }
    detach_task(handle).unwrap();
    assert_eq!(reap_exited_processes(), 1);
}
#[test]
fn real_finalization_refuses_changed_live_transaction_before_visibility_or_old_mm_release() {
    let _isolation = isolate_process_table();
    let handle = create_process(921_002, new_state()).unwrap();
    let (reservation, staged) = staged(handle);
    // Isolate the final owner nonce even with matching process/MM generation
    // and a correct receipt. A real production mismatch fails stop.
    {
        let mut table = PROCESS_TABLE.lock();
        table
            .lookup_object_mut(handle)
            .unwrap()
            .lifecycle_transaction_id += 1;
    }
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        finalize_exec_state(staged, test_publication(reservation, 0))
    }))
    .is_err();
    assert!(refused);
    assert!(!process_state_is_visible(handle));
}
