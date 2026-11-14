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

#[test]
fn quota_refused_exec_preserves_old_mm_cloexec_and_generation() {
    use kernel_mm::api::kmem_charge::{self, Class};
    use crate::user::handles::FD_CLOEXEC;
    let _isolation = isolate_process_table();
    let job = jobs::job_create_child(jobs::ROOT_JOB).unwrap();
    let spawn = reserve_spawn_in(Some(job)).unwrap();
    let handle = spawn::publish_spawn(spawn, 921_003, None, new_state()).unwrap();
    let process = retain_process(handle).unwrap();
    let (old_pin, old_security) = process.with_state_mut(|_, state| {
        state.handles_mut().get_entry_mut(0).unwrap().set_fd_flags(FD_CLOEXEC);
        (state.address_space().pin_execution().unwrap(), state.security())
    });
    let before_generation = {
        let table = PROCESS_TABLE.lock();
        table.slots[handle.index()].object.as_deref().unwrap().mm_generation
    };
    let mut replacement = ProcessAddressSpace::empty_for_tests();
    replacement.add_immutable_metadata_for_tests();
    assert!(replacement.immutable_mapping_metadata_bytes() > 0);
    jobs::job_set_kmem_class_limit(job, Class::Vma, 0).unwrap();
    let reservation = begin_exec(handle).unwrap();
    assert!(authorize_exec(reservation));
    let result = stage_exec_state_checked(
        reservation,
        replacement,
        crate::user::linux::LinuxProcessState::default(),
        crate::user::linux::LinuxMemoryMapState::new(),
        crate::user::linux::LinuxRuntimeProfile::new(),
        "/denied.elf",
    );
    assert!(matches!(result, Err(StageRefusal::MappingQuota)));
    assert_eq!(kmem_charge::usage(Class::Vma, job.index() as usize), 0);
    assert!(old_pin.try_activate().is_some(), "old root admission remains open");
    process.with_state_mut(|_, state| {
        assert_eq!(state.exec_path(), "/test.elf");
        assert_eq!(state.security(), old_security);
        assert_eq!(state.handles().get_entry(0).unwrap().fd_flags(), FD_CLOEXEC, "cloexec handle remains open");
        assert_eq!(state.mapping_kmem_charged(), 0);
    });
    {
        let table = PROCESS_TABLE.lock();
        let object = table.slots[handle.index()].object.as_deref().unwrap();
        assert_eq!(object.mm_generation, before_generation);
        assert!(!object.exec_state_staged);
    }
    assert!(cancel_exec(reservation));
    assert!(process_state_is_visible(handle));
}

#[test]
fn exec_reserves_new_metadata_while_old_pinned_root_remains_billed() {
    use kernel_mm::api::kmem_charge::{self, Class};
    let _isolation = isolate_process_table();
    let job = jobs::job_create_child(jobs::ROOT_JOB).unwrap();
    let mut old_mm = ProcessAddressSpace::empty_for_tests();
    old_mm.add_immutable_metadata_for_tests();
    let old_bytes = old_mm.immutable_mapping_metadata_bytes() as u64;
    let old_state = crate::user::process_state::UserProcessState::new(
        old_mm, Some(crate::user::linux::LinuxProcessState::default()), None, None, None, false, "/old",
    );
    let spawn = reserve_spawn_in(Some(job)).unwrap();
    let handle = spawn::publish_spawn(spawn, 921_004, None, old_state).unwrap();
    let process = retain_process(handle).unwrap();
    let old_pin = process.with_state_mut(|_, state| state.address_space().pin_execution().unwrap());
    let mut denied_mm = ProcessAddressSpace::empty_for_tests();
    denied_mm.add_immutable_metadata_for_tests();
    let new_bytes = denied_mm.immutable_mapping_metadata_bytes() as u64;
    assert_eq!(kmem_charge::usage(Class::Vma, job.index() as usize), old_bytes);
    jobs::job_set_kmem_class_limit(job, Class::Vma, old_bytes).unwrap();
    let denied = begin_exec(handle).unwrap();
    assert!(authorize_exec(denied));
    assert!(matches!(stage_exec_state_checked(
        denied, denied_mm, crate::user::linux::LinuxProcessState::default(),
        crate::user::linux::LinuxMemoryMapState::new(),
        crate::user::linux::LinuxRuntimeProfile::new(), "/denied"
    ), Err(StageRefusal::MappingQuota)));
    assert_eq!(kmem_charge::usage(Class::Vma, job.index() as usize), old_bytes);
    assert!(cancel_exec(denied));
    jobs::job_set_kmem_class_limit(job, Class::Vma, old_bytes + new_bytes).unwrap();
    let mut replacement = ProcessAddressSpace::empty_for_tests();
    replacement.add_immutable_metadata_for_tests();
    let reservation = begin_exec(handle).unwrap();
    assert!(authorize_exec(reservation));
    let staged = stage_exec_state_checked(
        reservation, replacement, crate::user::linux::LinuxProcessState::default(),
        crate::user::linux::LinuxMemoryMapState::new(),
        crate::user::linux::LinuxRuntimeProfile::new(), "/new"
    ).unwrap();
    let finalized = finalize_exec_state(staged, test_publication(reservation, 0));
    let (_, _, _, old_state) = finalized.into_parts();
    old_state.retire_execution_state();
    kernel_mm::api::address_space::drain_retired_address_spaces(128);
    assert_eq!(kmem_charge::usage(Class::Vma, job.index() as usize), old_bytes + new_bytes);
    drop(old_pin);
    kernel_mm::api::address_space::drain_retired_address_spaces(128);
    assert_eq!(kmem_charge::usage(Class::Vma, job.index() as usize), new_bytes);
    drop(process);
    assert!(mark_process_exiting(921_004).is_some());
    detach_task(handle).unwrap();
    assert_eq!(reap_exited_processes(), 1);
    kernel_mm::api::address_space::drain_retired_address_spaces(128);
    assert_eq!(kmem_charge::usage(Class::Vma, job.index() as usize), 0);
}

#[test]
fn refused_resident_bind_keeps_staged_metadata_billed_while_new_root_pinned() {
    use kernel_mm::api::{charge, kmem_charge::{self, Class}};
    let _isolation = isolate_process_table();
    let job = jobs::job_create_child(jobs::ROOT_JOB).unwrap();
    let spawn = reserve_spawn_in(Some(job)).unwrap();
    let handle = spawn::publish_spawn(spawn, 921_005, None, new_state()).unwrap();
    let mut replacement = ProcessAddressSpace::empty_for_tests();
    replacement.add_immutable_metadata_for_tests();
    let bytes = replacement.immutable_mapping_metadata_bytes() as u64;
    let pin = replacement.pin_execution().unwrap();
    charge::set_limit(job.index() as usize, 0);
    let reservation = begin_exec(handle).unwrap();
    assert!(authorize_exec(reservation));
    assert!(matches!(stage_exec_state_checked(
        reservation, replacement, crate::user::linux::LinuxProcessState::default(),
        crate::user::linux::LinuxMemoryMapState::new(),
        crate::user::linux::LinuxRuntimeProfile::new(), "/denied"
    ), Err(StageRefusal::MappingQuota)));
    assert_eq!(kmem_charge::usage(Class::Vma, job.index() as usize), bytes);
    assert_eq!(charge::usage(job.index() as usize), 0);
    drop(pin);
    kernel_mm::api::address_space::drain_retired_address_spaces(128);
    assert_eq!(kmem_charge::usage(Class::Vma, job.index() as usize), 0);
    assert!(cancel_exec(reservation));
    assert!(mark_process_exiting(921_005).is_some());
    detach_task(handle).unwrap();
    assert_eq!(reap_exited_processes(), 1);
    kernel_mm::api::address_space::drain_retired_address_spaces(128);
}
