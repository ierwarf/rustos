//! Owner: scalar context/MM and exact physical Current allocation-bank witnesses.
//! Boundary: native Ticket/MM roots and CPU phase; no physical HAL/CR3/RSP equivalence.
//! Lifecycle: actual Current stack pins retain exact start/context through fallback and release.
//! Concurrency: canonical serialized owner fixtures and explicit mixed-record projections.
//! Failure: no absent, Prepared or foreign Current authority; descriptor mismatch refuses.
//! Forbidden: copied boolean/metadata as a production lifetime grant or external ACK.
//! Evidence: actual bank/pin fallback plus focused scalar/MM owner tests.
use super::super::super::{
    BlockReason, NICE_0_LOAD, TaskStart, scheduling_context, scheduling_runtime,
};
use super::super::tests::admit_context;
use super::super::{close_for_reclaim, pin_view, take_storage, task_start};
use super::*;

pub(in crate::multitask::scheduler) fn metadata(slot: usize, id: u64) -> TaskContext {
    TaskContext {
        scheduling_context: scheduling_context::SchedulingContext::bind(slot, id),
        saved_rsp: 0,
        test_ready: true,
        ready_since_ticks: 0,
        blocked: false,
        blocked_since_ticks: 0,
        wake_armed: false,
        block_reason: BlockReason::None,
        weight: NICE_0_LOAD,
        vruntime_ns: 0,
        exec_start_ticks: 0,
        address_space_root: 0,
        kernel_stack_base: 0,
        kernel_stack_top: 0,
        alternate_kernel_stack_base: 0,
        alternate_kernel_stack_top: 0,
        user_mode: true,
        user_abi: Some(UserAbi::Linux),
        console_session: ConsoleSessionHandle::SYSTEM,
        process_handle: None,
        process_id: Some(id),
        user_stack: None,
        windows_thread_state: None,
    }
}

fn release(slot: usize) {
    assert!(close_for_reclaim(slot));
    clear(slot);
    task_start::clear(slot);
    drop(take_storage(slot));
}

#[test]
fn pinned_context_cannot_be_cleared_replaced_or_detached_with_its_allocation() {
    let _serial = scheduling_runtime::test_serial_guard();
    let slot = 95;
    let context = metadata(slot, 96001);
    let view = admit_context(slot, 96001, Some(context));
    let snapshot = read(slot).unwrap();
    let pin = pin_view(slot, 96001, view).unwrap();
    assert!(!close_for_reclaim(slot));
    assert!(std::panic::catch_unwind(|| clear(slot)).is_err());
    assert!(std::panic::catch_unwind(|| install(slot, context)).is_err());
    assert!(std::panic::catch_unwind(|| take_storage(slot)).is_err());
    assert_eq!(read(slot).unwrap().process_id, Some(96001));
    drop(pin);
    release(slot);
    let fresh = metadata(slot, 96002);
    admit_context(slot, 96002, Some(fresh));
    assert_eq!(read(slot).unwrap().process_id, Some(96002));
    // A copied scalar snapshot is not a claim on the replacement identity.
    assert_eq!(snapshot.process_id, Some(96001));
    assert!(
        replace_exec(
            slot,
            96001,
            UserAbi::Windows,
            ConsoleSessionHandle::SYSTEM,
            None
        )
        .is_none()
    );
    release(slot);
}

#[test]
fn exec_patch_requires_exact_identity_and_preserves_non_exec_fields() {
    let _serial = scheduling_runtime::test_serial_guard();
    let slot = 95;
    let context = metadata(slot, 96101);
    let identity = context.scheduling_context.identity();
    let view = admit_context(slot, 96101, Some(context));
    let pin = pin_view(slot, 96101, view).unwrap();
    assert!(
        replace_exec(
            slot,
            96102,
            UserAbi::Windows,
            ConsoleSessionHandle::SYSTEM,
            None
        )
        .is_none()
    );
    assert_eq!(read(slot).unwrap().user_abi, Some(UserAbi::Linux));
    assert!(
        replace_exec(
            slot,
            96101,
            UserAbi::Windows,
            ConsoleSessionHandle::SYSTEM,
            None
        )
        .is_some()
    );
    let after = read(slot).unwrap();
    assert_eq!(after.user_abi, Some(UserAbi::Windows));
    assert_eq!(after.process_id, Some(96101));
    assert_eq!(after.scheduling_context.identity(), identity);
    assert_eq!(after.process_handle, context.process_handle);
    // ORDERING: acquire observes the retained physical loan before checking
    // that context replacement cannot make its backing appear drained.
    assert_eq!(
        super::super::RESOURCES[slot].pins.load(Ordering::Acquire),
        1
    );
    assert!(!close_for_reclaim(slot));
    assert!(
        replace_exec(
            slot,
            96101,
            UserAbi::Linux,
            ConsoleSessionHandle::SYSTEM,
            None
        )
        .is_none()
    );
    drop(pin);
    release(slot);
}

#[test]
fn exec_cannot_use_a_process_label_to_bypass_the_task_binding() {
    let _serial = scheduling_runtime::test_serial_guard();
    let slot = 95;
    let mut context = metadata(slot, 96602);
    context.process_id = Some(96601);
    admit_context(slot, 96602, Some(context));
    assert!(
        replace_exec(
            slot,
            96601,
            UserAbi::Windows,
            ConsoleSessionHandle::SYSTEM,
            None
        )
        .is_none()
    );
    assert!(
        replace_exec(
            slot,
            96602,
            UserAbi::Windows,
            ConsoleSessionHandle::SYSTEM,
            None
        )
        .is_none()
    );
    assert_eq!(read(slot).unwrap().user_abi, Some(UserAbi::Linux));
    assert_eq!(read(slot).unwrap().process_id, Some(96601));
    release(slot);
}

#[test]
fn frame_and_descriptor_custody_keep_process_backing_after_terminal_detach() {
    let _isolation = crate::multitask::process_table::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let slot = 95;
    let handle = super::super::super::tests::test_process(96801);
    let mut context = metadata(slot, 96801);
    context.process_handle = Some(handle);
    let view = admit_context(slot, 96801, Some(context));
    let frame = pin_view(slot, 96801, view).unwrap();
    detach_process(slot, 96801, handle).unwrap();
    crate::multitask::process_table::detach_task(handle).unwrap();
    assert_eq!(crate::multitask::process_table::reap_exited_processes(), 0);
    assert!(std::panic::catch_unwind(|| clear(slot)).is_err());
    assert!(super::super::RESOURCES[slot].with(|resources| resources.process_pin.is_some()));
    drop(frame);
    release(slot);
    assert_eq!(crate::multitask::process_table::reap_exited_processes(), 1);
}

#[test]
fn terminal_detach_is_exact_and_cannot_be_replayed_from_an_old_snapshot() {
    let _process = crate::multitask::process_table::tests::isolate_process_table();
    let _serial = scheduling_runtime::test_serial_guard();
    let slot = 95;
    // This is metadata custody, not a process/MM release proof.
    let handle = super::super::super::tests::test_process(96201);
    let mut context = metadata(slot, 96201);
    context.process_handle = Some(handle);
    admit_context(slot, 96201, Some(context));
    let old = read(slot).unwrap();
    assert!(detach_process(slot, 96202, handle).is_none());
    assert_eq!(read(slot).unwrap().process_handle, Some(handle));
    assert!(detach_process(slot, 96201, handle).is_some());
    assert!(read(slot).unwrap().process_handle.is_none());
    assert!(detach_process(slot, 96201, handle).is_none());
    assert!(
        replace_exec(
            slot,
            96201,
            UserAbi::Windows,
            ConsoleSessionHandle::SYSTEM,
            None
        )
        .is_some()
    );
    assert!(read(slot).unwrap().process_handle.is_none());
    assert_eq!(old.process_handle, Some(handle));
    release(slot);
}

#[test]
fn concurrent_readers_observe_complete_exec_metadata_without_catalog_borrows() {
    let _serial = scheduling_runtime::test_serial_guard();
    let slot = 95;
    let context = metadata(slot, 96301);
    let view = admit_context(slot, 96301, Some(context));
    let pin = pin_view(slot, 96301, view).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..1000 {
                replace_exec(
                    slot,
                    96301,
                    UserAbi::Windows,
                    ConsoleSessionHandle::SYSTEM,
                    None,
                )
                .unwrap();
                replace_exec(
                    slot,
                    96301,
                    UserAbi::Linux,
                    ConsoleSessionHandle::SYSTEM,
                    None,
                )
                .unwrap();
            }
        });
        for _ in 0..1000 {
            let snapshot = read(slot).unwrap();
            assert_eq!(snapshot.process_id, Some(96301));
            assert!(matches!(
                snapshot.user_abi,
                Some(UserAbi::Linux | UserAbi::Windows)
            ));
            assert_eq!(
                snapshot.scheduling_context.identity(),
                context.scheduling_context.identity()
            );
        }
    });
    assert!(!close_for_reclaim(slot));
    drop(pin);
    release(slot);
}

#[test]
fn current_scalar_snapshot_requires_joined_descriptor_and_current_custody() {
    let _serial = scheduling_runtime::test_serial_guard();
    let slot = 95;
    let context = metadata(slot, 96701);
    admit_context(slot, 96701, Some(context));
    assert!(joined_current(slot, false).is_none());
    assert_eq!(
        joined_current(slot, true).unwrap().1.process_id,
        Some(96701)
    );
    let replacement = metadata(slot, 96702);
    // Counterfactual mixed writer state must never become scalar authority.
    super::super::RESOURCES[slot].with(|resources| resources.context = Some(replacement));
    assert!(joined_current(slot, true).is_none());
    super::super::RESOURCES[slot].with(|resources| resources.context = Some(context));
    assert!(close_for_reclaim(slot));
    task_start::clear(slot);
    assert!(joined_current(slot, true).is_none());
    clear(slot);
    assert!(joined_current(slot, true).is_none());
    drop(take_storage(slot));
}

#[test]
fn actual_current_bank_owns_entry_log_and_wait_namespace_until_physical_pin_release() {
    use crate::multitask::scheduler::{self, PublishedFrameView};
    const SLOT: usize = 124;
    const PEER: usize = 125;
    const ID: u64 = 1245501;
    scheduler::process_affinity::tests::affinity_fixture(
        UserAbi::Linux,
        UserAbi::Linux,
        false,
        |_| {
            crate::multitask::cpu_local::with_test_activation_return(0, SLOT, PEER, |phase| {
                if phase != 0 {
                    return;
                }
                // A copied Current slot/entry without the physical allocation bank
                // never authorizes fallback, even with complete descriptors.
                assert!(current_task_start().is_none());
                assert!(current_user_log_ids().is_none());
                assert!(current_user_wait_binding().is_none());
                let base = super::super::storage_base(SLOT);
                let view = PublishedFrameView {
                    user_mode_task: true,
                    kernel_process_task: false,
                    kernel_stack_base: (base + scheduler::TASK_STACK_GUARD_BYTES) as u64,
                    kernel_stack_top: scheduler::align_kernel_stack_top(
                        base + scheduler::TASK_STACK_SIZE,
                    ) as u64,
                    alternate_stack_base: 0,
                    alternate_stack_top: 0,
                    stack_canary_intact: true,
                    scheduler_storage_base: 0,
                    scheduler_storage_end: 0,
                };
                let pin = super::super::pin_view(SLOT, ID, view).unwrap();
                super::super::cpu_leases::with_test_bank(0, pin, false, || {
                    assert_eq!(current_task_start().unwrap().id, ID);
                    assert_eq!(current_user_log_ids(), Some((ID, ID)));
                    assert_eq!(
                        current_user_wait_binding(),
                        Some((ID, UserAbi::Linux, 0x4000))
                    );
                    assert!(!super::super::close_for_reclaim(SLOT));
                    // Hiding observational publication cannot erase the owned
                    // physical Current/start/context fallback.
                    crate::multitask::current_identity::publish_metadata(SLOT, None);
                    assert_eq!(current_task_start().unwrap().id, ID);
                    assert_eq!(current_user_log_ids(), Some((ID, ID)));
                    assert_eq!(
                        current_user_wait_binding(),
                        Some((ID, UserAbi::Linux, 0x4000))
                    );
                });
                assert!(current_task_start().is_none());
                assert!(current_user_log_ids().is_none());
                assert!(current_user_wait_binding().is_none());
            });
        },
    );
}

#[test]
fn prepared_or_foreign_current_bank_cannot_authorize_scalar_fallback() {
    use crate::multitask::scheduler::{self, PublishedFrameView};
    const SLOT: usize = 124;
    const PEER: usize = 125;
    const ID: u64 = 1245501;
    scheduler::process_affinity::tests::affinity_fixture(
        UserAbi::Windows,
        UserAbi::Windows,
        false,
        |_| {
            crate::multitask::cpu_local::with_test_activation_return(0, SLOT, PEER, |phase| {
                if phase != 0 {
                    return;
                }
                for (slot, id, prepared) in [(SLOT, ID, true), (PEER, ID + 1, false)] {
                    let base = super::super::storage_base(slot);
                    let view = PublishedFrameView {
                        user_mode_task: true,
                        kernel_process_task: false,
                        kernel_stack_base: (base + scheduler::TASK_STACK_GUARD_BYTES) as u64,
                        kernel_stack_top: scheduler::align_kernel_stack_top(
                            base + scheduler::TASK_STACK_SIZE,
                        ) as u64,
                        alternate_stack_base: 0,
                        alternate_stack_top: 0,
                        stack_canary_intact: true,
                        scheduler_storage_base: 0,
                        scheduler_storage_end: 0,
                    };
                    let pin = super::super::pin_view(slot, id, view).unwrap();
                    super::super::cpu_leases::with_test_bank(0, pin, prepared, || {
                        assert!(current_task_start().is_none());
                        assert!(current_user_log_ids().is_none());
                        assert!(current_user_wait_binding().is_none());
                    });
                }
            });
        },
    );
}

#[test]
fn metadata_cannot_install_a_context_identity_from_a_foreign_slot() {
    let _serial = scheduling_runtime::test_serial_guard();
    let slot = 95;
    assert!(close_for_reclaim(slot));
    clear(slot);
    task_start::clear(slot);
    drop(take_storage(slot));
    let foreign = metadata(slot + 1, 96501);
    assert!(std::panic::catch_unwind(|| install(slot, foreign)).is_err());
    assert!(read(slot).is_none());
    install(slot, metadata(slot, 96501));
    assert!(read(slot).is_some());
    release(slot);
}

#[test]
fn start_identity_must_match_the_installed_funding_handle() {
    let _serial = scheduling_runtime::test_serial_guard();
    let slot = 96;
    assert!(close_for_reclaim(slot));
    clear(slot);
    task_start::clear(slot);
    drop(take_storage(slot));
    install(slot, metadata(slot, 96401));
    assert!(
        std::panic::catch_unwind(|| task_start::publish(
            slot,
            TaskStart {
                entry: |_| {},
                id: 96402
            }
        ))
        .is_err()
    );
    assert!(task_start::read(slot).is_none());
    task_start::publish(
        slot,
        TaskStart {
            entry: |_| {},
            id: 96401,
        },
    );
    assert_eq!(task_start::read(slot).unwrap().id, 96401);
    release(slot);
}
