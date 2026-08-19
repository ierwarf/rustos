use alloc::boxed::Box;

use super::{
    ConsoleSessionHandle, IpcDonationTarget, MAX_CONSECUTIVE_SYSTEM_DISPATCHES, MAX_TASK,
    MIN_LOAD_WEIGHT, NICE_0_LOAD, SCHED_CPU_LOCALITY_LAG_NS, SYSTEM_CLASS_WEIGHT_FLAG, SchedClass,
    Scheduler, SchedulerDispatch, TaskContext, TaskStart, align_kernel_stack_top,
};
use crate::memory::paging::ProcessAddressSpace;
use crate::multitask::{UserTaskBootstrap, noop_task_entry, process_table};
use crate::user::abi::UserAbi;
use crate::user::linux::LinuxThreadState;
use crate::user::process_state::UserProcessState;
use kernel_ipc_runtime::api::{EndpointResponseTake, IpcError};

static TEST_SCHEDULER_TEMPLATE: Scheduler = Scheduler::new();

/// Restores the shared runqueue witness even when one scheduler assertion
/// fails, so this test's exact custody records cannot leak into another
/// parallel unit test.
struct RunqueuePublicationReset;

impl Drop for RunqueuePublicationReset {
    fn drop(&mut self) {
        super::runqueue::reset_before_publication();
    }
}

#[test]
fn kernel_stack_top_is_aligned_for_sysv_rust_calls() {
    for low_bits in 0..16 {
        let top = align_kernel_stack_top(0x10_000 + low_bits);
        assert_eq!(top & 0xF, 0);
        assert!(top <= 0x10_000 + low_bits);
        assert!((0x10_000 + low_bits) - top < 16);
    }
}

#[test]
fn architectural_restore_is_required_exactly_for_a_task_switch() {
    let same = SchedulerDispatch::new(0x1000, 119, 7, 7);
    assert!(!same.requires_architectural_restore());

    let switched = SchedulerDispatch::new(0x2000, 119, 7, 9);
    assert!(switched.requires_architectural_restore());
}

#[test]
fn slot_identity_keeps_exact_user_pid_and_tid_together() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let process = test_process(0x1a2b);
    let slot = 1;
    scheduler.contexts[slot] = Some(test_user_context(process));
    scheduler.starts[slot] = Some(TaskStart {
        entry: noop_task_entry,
        id: 0x3c4d,
    });

    assert_eq!(
        scheduler
            .slot_identity(slot)
            .and_then(|identity| identity.complete_user_log_ids()),
        Some(Some((0x1a2b, 0x3c4d)))
    );
}

pub(super) fn boxed_scheduler() -> Box<Scheduler> {
    let mut scheduler = Box::<Scheduler>::new_uninit();
    unsafe {
        // The const template owns no heap allocation: every Vec-bearing
        // field is `None`. Copy it directly into the heap allocation so
        // debug test threads never materialize the large SIMD arrays on
        // their small harness stack.
        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!(TEST_SCHEDULER_TEMPLATE),
            scheduler.as_mut_ptr(),
            1,
        );
        scheduler.assume_init()
    }
}

pub(super) fn test_user_context(handle: process_table::ProcessHandle) -> TaskContext {
    TaskContext {
        saved_rsp: 0,
        test_ready: true,
        ready_since_ticks: 0,
        blocked: false,
        blocked_since_ticks: 0,
        wake_armed: false,
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
        process_handle: Some(handle),
        process_id: process_table::with_process_state(handle, |pid, _| pid),
        user_stack: None,
        windows_thread_state: None,
    }
}

pub(super) fn test_process(id: u64) -> process_table::ProcessHandle {
    process_table::create_process(
        id,
        UserProcessState::new(
            ProcessAddressSpace::empty_for_tests(),
            None,
            None,
            None,
            None,
            false,
            "/test.elf",
        ),
    )
    .expect("process handle")
}

#[derive(Debug, Eq, PartialEq)]
struct TerminalRejectionSnapshot {
    ready: bool,
    blocked: bool,
    wake_armed: bool,
    current_task: usize,
    retired: bool,
    start_suspended: bool,
    job_stopped: bool,
    exec_target_quiesced: bool,
    retire_reason_present: bool,
    retirement_side_effect_present: bool,
    pending_reap: bool,
    task_id: Option<u64>,
    execution_owner: Option<super::super::cpu_local::TaskExecutionOwner>,
}

/// Captures every scheduler-owned state a terminal block/wake rejection
/// must leave untouched. CPU-local execution ownership is independently
/// published, so the caller holds `test_publication_lock` while comparing
/// this snapshot.
fn terminal_rejection_snapshot(scheduler: &Scheduler, slot: usize) -> TerminalRejectionSnapshot {
    let context = scheduler.contexts[slot].expect("terminal test context");
    TerminalRejectionSnapshot {
        ready: context.test_ready,
        blocked: context.blocked,
        wake_armed: context.wake_armed,
        current_task: scheduler.current_task,
        retired: scheduler.retired[slot],
        start_suspended: scheduler.start_suspended[slot],
        job_stopped: scheduler.job_stopped[slot],
        exec_target_quiesced: scheduler.exec_target_quiesced[slot],
        retire_reason_present: scheduler.retire_reasons[slot].is_some(),
        retirement_side_effect_present: scheduler.retirement_side_effects[slot].is_some(),
        pending_reap: scheduler.pending_reap,
        task_id: scheduler.starts[slot].map(|start| start.id),
        execution_owner: super::super::cpu_local::task_execution_owner(slot),
    }
}

#[test]
fn ready_validation_accepts_only_immutable_published_frames() {
    use super::should_validate_published_ready_frame as should_validate;

    assert!(should_validate(2, 1, false, false, true, false));
    assert!(!should_validate(1, 1, false, false, true, false));
    assert!(!should_validate(2, 1, true, false, true, false));
    assert!(!should_validate(2, 1, false, true, true, false));
    assert!(!should_validate(2, 1, false, false, false, false));
    assert!(!should_validate(2, 1, false, false, true, true));
}

#[test]
fn ready_scanner_never_reads_a_frame_owned_by_any_cpu() {
    use super::published_frame_is_stable as stable;

    assert!(stable(2, 1, false));
    assert!(!stable(1, 1, false));
    assert!(!stable(2, 1, true));
}

#[test]
fn live_noncurrent_task_must_retain_one_scheduler_state_owner() {
    use super::live_task_state_is_partitioned as partitioned;

    assert!(partitioned(1, 1, false, false, false, false, false));
    assert!(partitioned(2, 1, false, false, false, true, false));
    assert!(partitioned(2, 1, false, false, false, false, true));
    assert!(partitioned(2, 1, true, false, false, false, false));
    assert!(partitioned(2, 1, false, true, false, false, false));
    assert!(partitioned(2, 1, false, false, true, false, false));
    assert!(!partitioned(2, 1, false, false, false, false, false));
}

#[test]
fn collect_process_sibling_slots_returns_matching_user_slots_only() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let owner = test_process(1);
    let other = test_process(2);

    scheduler.contexts[1] = Some(test_user_context(owner));
    scheduler.contexts[2] = Some(test_user_context(owner));
    scheduler.contexts[3] = Some(test_user_context(other));
    scheduler.contexts[4] = Some(TaskContext {
        user_mode: false,
        process_handle: Some(owner),
        ..test_user_context(owner)
    });

    scheduler.retired[2] = true;
    scheduler.contexts[5] = Some(test_user_context(owner));

    let (slots, count) = scheduler.collect_live_process_sibling_slots(1, owner);
    assert_eq!(count, 1);
    assert_eq!(slots[0], 5);
    assert!(slots[1..MAX_TASK].iter().all(|slot| *slot == 0));
}

#[test]
fn process_stop_is_scheduler_wide_and_sigcont_resumes_before_delivery() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let process = test_process(48);
    process_table::attach_task(process).expect("second thread");
    let leader = test_user_context(process);
    let worker = test_user_context(process);
    scheduler.contexts[1] = Some(leader);
    scheduler.contexts[2] = Some(worker);
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 48,
    });
    scheduler.starts[2] = Some(TaskStart {
        entry: noop_task_entry,
        id: 49,
    });
    scheduler.install_linux_thread_state(1, Some(48), Some(LinuxThreadState::default()));
    scheduler.install_linux_thread_state(2, Some(49), Some(LinuxThreadState::default()));
    scheduler.current_task = 1;

    assert!(scheduler.stop_current_linux_process(19));
    assert!(scheduler.job_stopped[1]);
    assert!(scheduler.job_stopped[2]);
    assert!(!scheduler.stop_current_linux_process(19));

    assert!(scheduler.queue_linux_signal(48, 48, rustos_user_abi::linux::SIGCONT));
    assert!(!scheduler.job_stopped[1]);
    assert!(!scheduler.job_stopped[2]);
    let pending = scheduler
        .linux_thread_state(1)
        .map(|state| state.pending_signals)
        .unwrap_or(0);
    assert_ne!(
        pending
            & crate::user::sysops::linux::linux_signal_bit(rustos_user_abi::linux::SIGCONT)
                .unwrap(),
        0
    );

    process_table::note_process_exit_status(48, 0).expect("record exit");
    process_table::detach_task(process).expect("detach leader");
    process_table::detach_task(process).expect("detach worker");
    assert_eq!(process_table::reap_exited_processes(), 1);
}

#[test]
fn unmasked_signal_revokes_a_pending_block_arm() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let process = test_process(52);
    let mut context = test_user_context(process);
    context.test_ready = false;
    scheduler.contexts[1] = Some(context);
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 52,
    });
    scheduler.install_linux_thread_state(1, Some(52), Some(LinuxThreadState::default()));
    scheduler.current_task = 1;

    assert!(scheduler.arm_block_current_task());
    assert!(scheduler.contexts[1].expect("armed context").wake_armed);
    assert!(scheduler.queue_linux_signal(52, 52, 15));
    let context = scheduler.contexts[1].expect("signalled context");
    assert!(!context.wake_armed);
    assert!(!context.blocked);
    assert!(!context.test_ready);
    assert_eq!(scheduler.commit_block_current_task(), Some(false));

    process_table::note_process_exit_status(52, 0).expect("record exit");
    process_table::detach_task(process).expect("detach thread");
    assert_eq!(process_table::reap_exited_processes(), 1);
}

#[test]
fn process_sigchld_prefers_leader_and_retains_exact_coalesced_causes() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let process = test_process(50);
    process_table::attach_task(process).expect("second thread");
    let leader = test_user_context(process);
    let worker = test_user_context(process);
    scheduler.contexts[1] = Some(leader);
    scheduler.contexts[2] = Some(worker);
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 50,
    });
    scheduler.starts[2] = Some(TaskStart {
        entry: noop_task_entry,
        id: 51,
    });
    scheduler.install_linux_thread_state(1, Some(50), Some(LinuxThreadState::default()));
    scheduler.install_linux_thread_state(2, Some(51), Some(LinuxThreadState::default()));
    scheduler.current_task = 1;

    assert!(
        scheduler
            .queue_linux_process_sigchld(50, rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_STOP)
    );
    assert_eq!(
        scheduler
            .linux_thread_state(1)
            .map(|state| state.pending_sigchld_events),
        Some(rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_STOP)
    );

    scheduler.current_task = 2;
    scheduler.transfer_pending_process_sigchld(1);
    assert_eq!(
        scheduler
            .linux_thread_state(1)
            .map(|state| state.pending_sigchld_events),
        Some(0)
    );
    assert_eq!(
        scheduler
            .linux_thread_state(2)
            .map(|state| state.pending_sigchld_events),
        Some(rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_STOP)
    );

    scheduler.retired[1] = true;
    assert!(
        scheduler.queue_linux_process_sigchld(
            50,
            rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_CONTINUE
        )
    );
    assert_eq!(
        scheduler
            .linux_thread_state(2)
            .map(|state| state.pending_sigchld_events),
        Some(
            rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_STOP
                | rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_CONTINUE
        )
    );

    process_table::note_process_exit_status(50, 0).expect("record exit");
    process_table::detach_task(process).expect("detach leader");
    process_table::detach_task(process).expect("detach worker");
    assert_eq!(process_table::reap_exited_processes(), 1);
}

#[test]
fn terminate_user_process_retires_every_live_sibling() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let owner = test_process(41);
    let other = test_process(42);

    scheduler.contexts[1] = Some(test_user_context(owner));
    scheduler.contexts[2] = Some(test_user_context(owner));
    scheduler.contexts[3] = Some(test_user_context(other));
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 41,
    });
    scheduler.starts[2] = Some(TaskStart {
        entry: noop_task_entry,
        id: 43,
    });
    scheduler.starts[3] = Some(TaskStart {
        entry: noop_task_entry,
        id: 42,
    });

    assert!(scheduler.terminate_user_process(41, Some(7)));
    assert_eq!(process_table::is_process_exiting(41), Some(true));
    assert!(scheduler.retired[1]);
    assert!(scheduler.retired[2]);
    assert!(!scheduler.retired[3]);
}

#[test]
fn terminating_the_last_task_marks_its_process_exiting() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let owner = test_process(45);
    scheduler.contexts[1] = Some(test_user_context(owner));
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 451,
    });

    assert!(scheduler.terminate_user_task(451, Some(7)));
    assert_eq!(process_table::is_process_exiting(45), Some(true));
    assert!(scheduler.retired[1]);
}

#[test]
fn retirement_revokes_task_and_process_ipc_authority() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let owner = test_process(94);
    scheduler.contexts[1] = Some(test_user_context(owner));
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 941,
    });

    let task_endpoint =
        kernel_ipc_runtime::api::create_endpoint_for_task(941).expect("task-owned endpoint");
    let process_endpoint =
        kernel_ipc_runtime::api::create_endpoint_for_process(94).expect("process-owned endpoint");
    let (task_reply, _) =
        kernel_ipc_runtime::api::enqueue_endpoint_call(task_endpoint, 951, b"task")
            .expect("task call");
    let (process_reply, _) =
        kernel_ipc_runtime::api::enqueue_endpoint_call(process_endpoint, 952, b"process")
            .expect("process call");

    scheduler.retire_slot(
        1,
        super::TaskRetireReason::Terminated {
            requested_by_pid: None,
        },
    );
    scheduler
        .take_retirement_side_effect()
        .expect("retirement side effects")
        .complete(|task_id| {
            let _ = scheduler.wake_task(task_id);
        });

    assert!(matches!(
        kernel_ipc_runtime::api::take_endpoint_response_detailed(task_reply, 0),
        Ok(EndpointResponseTake::Error {
            error: IpcError::PeerClosed,
            discarded_request_handles,
        }) if discarded_request_handles.is_empty()
    ));
    assert!(matches!(
        kernel_ipc_runtime::api::take_endpoint_response_detailed(process_reply, 0),
        Ok(EndpointResponseTake::Error {
            error: IpcError::PeerClosed,
            discarded_request_handles,
        }) if discarded_request_handles.is_empty()
    ));
    assert_eq!(
        kernel_ipc_runtime::api::enqueue_endpoint_call(task_endpoint, 953, b"late-task"),
        Err(IpcError::InvalidHandle)
    );
    assert_eq!(
        kernel_ipc_runtime::api::enqueue_endpoint_call(process_endpoint, 954, b"late-process"),
        Err(IpcError::InvalidHandle)
    );
}

#[test]
fn retired_user_slot_waits_for_exact_runtime_cleanup_ack() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let owner = test_process(96);
    scheduler.contexts[1] = Some(test_user_context(owner));
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 961,
    });

    scheduler.retire_slot(1, super::TaskRetireReason::Exited);
    let cleanup = scheduler
        .next_retired_task_cleanup()
        .expect("retired user task cleanup");
    assert_eq!(cleanup.task_id(), 961);
    assert_eq!(cleanup.process_id(), 96);
    assert!(cleanup.process_terminal());
    // Retain the external side-effect token locally so this assertion
    // isolates the runtime-cleanup acknowledgement gate. If that gate is
    // removed, the retired stack becomes reclaimable before its exact
    // userspace cleanup acknowledgement.
    let side_effect = scheduler
        .take_retirement_side_effect()
        .expect("retirement side effects");
    assert!(scheduler.reap_inactive_retired_slots().is_none());
    assert!(scheduler.contexts[1].is_some());

    assert!(
        !scheduler.complete_retired_task_cleanup(crate::multitask::RetiredTaskCleanup {
            task_id: 962,
            process_id: 96,
            process_terminal: true,
            clear_child_tid: 0,
            robust_list_head: 0,
            robust_list_len: 0,
        })
    );
    assert!(scheduler.complete_retired_task_cleanup(cleanup));
    side_effect.complete(|task_id| {
        let _ = scheduler.wake_task(task_id);
    });
    let reclaim = scheduler
        .reap_inactive_retired_slots()
        .expect("retired slot reclaim");
    assert!(scheduler.contexts[1].is_none());
    assert_eq!(process_table::thread_count_by_pid(96), Some(1));
    reclaim.complete();
    assert_eq!(process_table::thread_count_by_pid(96), Some(0));
    assert_eq!(process_table::reap_exited_processes(), 1);
}

#[test]
fn retirement_cleanup_stamps_process_terminal_only_on_last_live_thread() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let owner = test_process(97);
    scheduler.contexts[1] = Some(test_user_context(owner));
    scheduler.contexts[2] = Some(test_user_context(owner));
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 971,
    });
    scheduler.starts[2] = Some(TaskStart {
        entry: noop_task_entry,
        id: 972,
    });

    scheduler.retire_slot(1, super::TaskRetireReason::Exited);
    let first = scheduler
        .next_retired_task_cleanup()
        .expect("first thread cleanup");
    assert_eq!(first.task_id(), 971);
    assert!(!first.process_terminal());
    assert!(scheduler.complete_retired_task_cleanup(first));

    scheduler.retire_slot(2, super::TaskRetireReason::Exited);
    let last = scheduler
        .next_retired_task_cleanup()
        .expect("last thread cleanup");
    assert_eq!(last.task_id(), 972);
    assert!(last.process_terminal());
}

#[test]
fn exec_sibling_slot_stays_quarantined_until_runtime_cleanup() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let owner = test_process(98);
    scheduler.contexts[1] = Some(test_user_context(owner));
    scheduler.contexts[2] = Some(test_user_context(owner));
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 981,
    });
    scheduler.starts[2] = Some(TaskStart {
        entry: noop_task_entry,
        id: 982,
    });

    scheduler.retire_exec_sibling_slot(2);
    assert!(scheduler.retired[2]);
    assert!(scheduler.contexts[2].is_some());
    assert_eq!(scheduler.contexts[2].unwrap().process_handle, None);
    assert_eq!(
        scheduler
            .next_retired_task_cleanup()
            .map(|cleanup| cleanup.task_id()),
        Some(982)
    );
    let _ = scheduler.reap_inactive_retired_slots();
    assert!(scheduler.contexts[2].is_some());
}

#[test]
fn exec_rejects_slot_with_unconsumed_side_effect_token() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let owner = test_process(0xeca1);
    let slot = 1;
    let task_id = 0xec01;
    let mut context = test_user_context(owner);
    context.address_space_root = 0x1234_5000;
    context.test_ready = true;
    scheduler.contexts[slot] = Some(context);
    scheduler.starts[slot] = Some(TaskStart {
        entry: noop_task_entry,
        id: task_id,
    });
    scheduler.current_task = slot;
    scheduler.retirement_side_effects[slot] =
        Some(super::RetirementSideEffect::new(Some(task_id), None));
    let before = scheduler.contexts[slot].expect("live exec target");
    let before_start = scheduler.starts[slot].expect("live exec identity");
    let base = crate::memory::paging::USER_SPACE_BASE;
    let mut replacement = UserTaskBootstrap::new(
        UserAbi::Linux,
        x86_64::VirtAddr::new(base + 0x8_000),
        x86_64::VirtAddr::new(base + 0xa_000),
    );

    assert_eq!(
        scheduler.exec_current_user_process(0x9876_5000, &mut replacement),
        None,
        "unconsumed retirement side effects must reject exec admission"
    );
    let after = scheduler.contexts[slot].expect("rejected exec target remains live");
    assert_eq!(after.address_space_root, before.address_space_root);
    assert_eq!(after.user_abi, before.user_abi);
    assert_eq!(after.test_ready, before.test_ready);
    assert_eq!(after.blocked, before.blocked);
    assert_eq!(
        scheduler.starts[slot].expect("identity preserved").id,
        before_start.id
    );
    assert!(!scheduler.retired[slot]);
    assert!(
        scheduler.retirement_side_effects[slot].is_some(),
        "rejected exec must retain its unconsumed retirement token"
    );
}

#[test]
fn rejected_thread_attachment_releases_unpublished_stack() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let owner = test_process(95);
    scheduler.current_task = 1;
    scheduler.contexts[1] = Some(test_user_context(owner));
    process_table::mark_process_exiting(95).expect("mark exiting");
    assert!(scheduler.reserve_user_thread_slot(951).is_none());
    assert!(scheduler.contexts[2].is_none());
    assert!(scheduler.stacks[2].is_none());
}

#[test]
fn synchronous_ipc_donation_promotes_and_revokes_a_transitive_user_chain() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let interactive = test_process(61);
    let broker = test_process(62);
    let policy = test_process(63);

    let mut interactive_context = test_user_context(interactive);
    interactive_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
    scheduler.contexts[1] = Some(interactive_context);
    scheduler.contexts[2] = Some(test_user_context(broker));
    scheduler.contexts[3] = Some(test_user_context(policy));
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 601,
    });
    scheduler.starts[2] = Some(TaskStart {
        entry: noop_task_entry,
        id: 602,
    });
    scheduler.starts[3] = Some(TaskStart {
        entry: noop_task_entry,
        id: 603,
    });

    assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));
    assert_eq!(scheduler.slot_class(3), Some(SchedClass::User));
    assert!(scheduler.inherit_ipc_priority(10, 601, 602));
    assert_eq!(scheduler.slot_class(2), Some(SchedClass::System));

    // The broker's nested synchronous call must pass the original
    // interactive class through to the final policy server.
    assert!(scheduler.inherit_ipc_priority(11, 602, 603));
    assert_eq!(scheduler.slot_class(3), Some(SchedClass::System));

    // A completed outer reply immediately restores both servers to their
    // manifest-derived class; no priority boost can leak past capability
    // lifetime.
    assert!(scheduler.release_ipc_priority(10));
    assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));
    assert_eq!(scheduler.slot_class(3), Some(SchedClass::User));
    assert!(scheduler.release_ipc_priority(11));
    assert_eq!(scheduler.slot_class(3), Some(SchedClass::User));

    // A process-owned endpoint without a sleeping receiver must select an
    // exact eligible worker before installing a System-class donation.
    // Use saved contexts created by the real allocation path: apply must
    // reject an invalid frame, so a copied test context cannot observe the
    // live application site below.
    scheduler.contexts[1]
        .as_mut()
        .expect("released interactive context")
        .weight = NICE_0_LOAD;
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let allocate = |scheduler: &mut Scheduler, task_id, offset| {
        scheduler
            .allocate_user_slot(
                task_id,
                ProcessAddressSpace::empty_for_tests(),
                UserTaskBootstrap::new(
                    UserAbi::Linux,
                    x86_64::VirtAddr::new(base + offset),
                    x86_64::VirtAddr::new(base + offset + 0x1_000),
                ),
                None,
                crate::arch::pit::divisor_from_micros(100),
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("donation task")
    };
    let caller = allocate(&mut scheduler, 604, 0x10_000);
    let target = allocate(&mut scheduler, 605, 0x14_000);
    scheduler.current_task = caller;
    scheduler.contexts[caller]
        .as_mut()
        .expect("interactive caller context")
        .weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
    scheduler.contexts[caller]
        .as_mut()
        .expect("interactive caller context")
        .vruntime_ns = 10_000_000;
    scheduler.contexts[target]
        .as_mut()
        .expect("target worker context")
        .vruntime_ns = 50_000_000;

    assert_eq!(scheduler.slot_class(caller), Some(SchedClass::System));
    assert_eq!(scheduler.slot_class(target), Some(SchedClass::User));
    assert!(scheduler.reserve_ipc_priority(604));
    let selected = scheduler
        .bind_ipc_priority_to_process_worker(12, 604, 605)
        .expect("eligible process worker");
    assert_eq!(selected, 605);
    assert_eq!(scheduler.slot_class(target), Some(SchedClass::System));
    assert_eq!(
        scheduler.contexts[target]
            .expect("donated broker context")
            .vruntime_ns,
        8_000_000,
        "the exact reply/worker bind must immediately donate dispatch vruntime"
    );
    assert!(
        scheduler.handoff_slot_ready(target),
        "the exact bound worker must remain dispatch-eligible"
    );
    assert_eq!(
        scheduler.take_next_synchronous_pick_hint_ready_slot(),
        Some(target)
    );
    assert_eq!(scheduler.synchronous_handoff_len_for_tests(), 0);
    assert!(scheduler.release_ipc_priority(12));
    assert_eq!(scheduler.slot_class(target), Some(SchedClass::User));
    assert!(
        scheduler.handoff_slot_ready(target),
        "reply revocation must restore the worker's manifest-derived class without revoking its runnable state"
    );
    scheduler.contexts[1]
        .as_mut()
        .expect("interactive caller context")
        .weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;

    // If the server is executing between receive calls, the reservation
    // follows the reply until that exact worker dequeues the request.
    scheduler.contexts[2].as_mut().unwrap().test_ready = false;
    assert!(scheduler.reserve_ipc_priority(601));
    assert!(
        scheduler
            .bind_ipc_priority_to_process_worker(13, 601, 62)
            .is_none()
    );
    assert!(scheduler.attach_reserved_ipc_priority(13, 601));
    assert!(
        scheduler
            .ipc_priority_donations
            .iter()
            .flatten()
            .any(|entry| {
                entry.reply == 13 && matches!(entry.target, IpcDonationTarget::AwaitingReceiver)
            })
    );
    assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));
    assert!(scheduler.release_ipc_priority(13));
    assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));

    assert!(scheduler.reserve_ipc_priority(601));
    assert!(scheduler.attach_reserved_ipc_priority(14, 601));
    assert!(scheduler.inherit_ipc_priority(14, 601, 602));
    assert_eq!(scheduler.slot_class(2), Some(SchedClass::System));
    assert!(scheduler.release_ipc_priority(14));
    assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));
}

#[test]
fn scheduler_block_arm_is_exact_race_safe_and_terminally_revoked() {
    let _process_table = process_table::tests::isolate_process_table();
    let _publication_lock = super::super::cpu_local::test_publication_lock();
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;

    // Root, retired, and start-suspended are independent terminal gates.
    // Populate a normally armable root-shaped context so the root gate is
    // the only reason this attempt is rejected.
    scheduler.contexts[super::ROOT_TASK_SLOT] = Some(test_user_context(test_process(689)));
    scheduler.starts[super::ROOT_TASK_SLOT] = Some(TaskStart {
        entry: noop_task_entry,
        id: 689,
    });
    scheduler.current_task = super::ROOT_TASK_SLOT;
    let root_before = terminal_rejection_snapshot(&scheduler, super::ROOT_TASK_SLOT);
    assert!(!scheduler.arm_block_current_task());
    assert_eq!(
        terminal_rejection_snapshot(&scheduler, super::ROOT_TASK_SLOT),
        root_before,
        "root preflight rejection must not change task or execution ownership"
    );

    let slot = scheduler
        .allocate_user_slot(
            690,
            ProcessAddressSpace::empty_for_tests(),
            UserTaskBootstrap::new(
                UserAbi::Linux,
                x86_64::VirtAddr::new(base + 0x2_000),
                x86_64::VirtAddr::new(base + 0x4_000),
            ),
            None,
            crate::arch::pit::divisor_from_micros(100),
            user_cs,
            user_ss,
            super::RFLAGS_RESERVED_BIT_1,
            false,
            noop_task_entry,
        )
        .expect("user slot");
    scheduler.current_task = slot;
    scheduler.contexts[slot]
        .as_mut()
        .expect("dispatched context")
        .test_ready = false;

    assert!(scheduler.arm_block_current_task());
    assert!(scheduler.contexts[slot].expect("context").wake_armed);
    assert!(scheduler.wake_task(690));
    assert!(!scheduler.contexts[slot].expect("context").wake_armed);
    assert_eq!(scheduler.commit_block_current_task(), Some(false));
    assert!(!scheduler.contexts[slot].expect("context").test_ready);

    assert!(scheduler.arm_block_current_task());
    assert_eq!(scheduler.commit_block_current_task(), Some(true));
    let blocked = scheduler.contexts[slot].expect("context");
    assert!(blocked.blocked);
    assert!(!blocked.test_ready);
    assert!(!scheduler.arm_block_current_task());
    assert!(!scheduler.cancel_block_current_task());

    scheduler.current_task = super::ROOT_TASK_SLOT;
    assert!(scheduler.wake_task(690));
    assert!(scheduler.contexts[slot].expect("context").test_ready);
    scheduler.contexts[slot]
        .as_mut()
        .expect("redispatched context")
        .test_ready = false;
    scheduler.current_task = slot;
    assert!(scheduler.arm_block_current_task());
    scheduler.retire_slot(slot, super::TaskRetireReason::Exited);
    let retired = scheduler.contexts[slot].expect("retired context");
    assert!(scheduler.retired[slot]);
    assert!(!retired.wake_armed);
    assert!(!scheduler.start_suspended[slot]);
    let retired_before = terminal_rejection_snapshot(&scheduler, slot);
    assert!(!scheduler.arm_block_current_task());
    assert_eq!(
        terminal_rejection_snapshot(&scheduler, slot),
        retired_before,
        "retired-only block rejection must preserve lifecycle and run ownership"
    );

    scheduler.current_task = super::ROOT_TASK_SLOT;
    let retired_wake_before = terminal_rejection_snapshot(&scheduler, slot);
    assert!(!scheduler.wake_task(690));
    assert_eq!(
        terminal_rejection_snapshot(&scheduler, slot),
        retired_wake_before,
        "retired-only wake rejection must preserve lifecycle and run ownership"
    );

    // `start_suspended` is a separate preflight bit. Keep this context
    // otherwise armable so the suspended gate itself is required for both
    // rejection paths rather than being masked by `blocked` state.
    let suspended_slot = scheduler
        .allocate_user_slot(
            691,
            ProcessAddressSpace::empty_for_tests(),
            UserTaskBootstrap::new(
                UserAbi::Linux,
                x86_64::VirtAddr::new(base + 0x6_000),
                x86_64::VirtAddr::new(base + 0x8_000),
            ),
            None,
            crate::arch::pit::divisor_from_micros(100),
            user_cs,
            user_ss,
            super::RFLAGS_RESERVED_BIT_1,
            false,
            noop_task_entry,
        )
        .expect("suspended user slot");
    scheduler.start_suspended[suspended_slot] = true;
    assert!(!scheduler.retired[suspended_slot]);
    scheduler.current_task = suspended_slot;
    let suspended_before = terminal_rejection_snapshot(&scheduler, suspended_slot);
    assert!(!scheduler.arm_block_current_task());
    assert_eq!(
        terminal_rejection_snapshot(&scheduler, suspended_slot),
        suspended_before,
        "suspended-only block rejection must preserve lifecycle and run ownership"
    );

    scheduler.current_task = super::ROOT_TASK_SLOT;
    let suspended_wake_before = terminal_rejection_snapshot(&scheduler, suspended_slot);
    assert!(!scheduler.wake_task(691));
    assert_eq!(
        terminal_rejection_snapshot(&scheduler, suspended_slot),
        suspended_wake_before,
        "suspended-only wake rejection must preserve lifecycle and run ownership"
    );
}
