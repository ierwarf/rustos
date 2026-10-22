use super::{
    ConsoleSessionHandle, MAX_TASK, SchedulerDispatch, TaskContext,
    align_kernel_stack_top,
};
use crate::memory::paging::ProcessAddressSpace;
use crate::multitask::{UserTaskBootstrap, process_table};
use crate::user::abi::UserAbi;
use crate::user::linux::LinuxThreadState;
use crate::user::process_state::UserProcessState;
use kernel_ipc_runtime::api::{EndpointResponseTake, IpcError};


/// Restores the shared runqueue witness even when one scheduler assertion
/// fails, so this test's exact custody records cannot leak into another
/// parallel unit test.
struct RunqueuePublicationReset;

impl Drop for RunqueuePublicationReset {
    fn drop(&mut self) {
        super::runqueue::reset_before_publication();
        crate::multitask::process_placement::reset_for_tests();
    }
}

/// Native namespace isolation only. This owns no task/catalog, copied backend,
/// frame or execution permission. Actual born resources supply every authority.
pub(super) struct SchedulerTestFixture {
    _runqueue_reset: RunqueuePublicationReset,
    _runtime_serial: std::sync::MutexGuard<'static, ()>,
    _runqueue_serial: std::sync::MutexGuard<'static, ()>,
    _cpu_publication: std::sync::MutexGuard<'static, ()>,
    _process_table: process_table::tests::ProcessTableTestIsolation,
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
    assert!(!switched.requires_address_space_restore(0x1234_0000, 0x1234_0000));
    assert!(switched.requires_address_space_restore(0x1234_0000, 0x5678_0000));
}

#[test]
fn slot_identity_keeps_exact_user_pid_and_tid_together() {
    catalog_witnesses::identity();
}

#[test]
fn task_slot_hint_collisions_and_stale_entries_fall_back_to_exact_identity() {
    catalog_witnesses::hint_collision();
}

#[test]
fn ipc_admission_exports_only_the_live_bound_scheduling_context() {
    super::donation_publication::tests::actual_noncurrent_donor_requires_original_bound_resource_and_full_context();
    super::donation_publication::tests::actual_participant_loans_survive_capture_and_terminal_seal_denies_new_admission();
}

#[test]
fn nested_passive_server_runtime_is_billed_to_the_root_caller_context() {
    super::donation_publication::tests::legacy_witnesses::nested_interval();
}

#[test]
fn deadline_domains_require_per_cpu_utilization_headroom() {
    super::domain_admission::tests::deadline_quota_sums_exact_overlapping_cpu_masks_and_rounds_up_without_partial_replacement();
}

#[test]
fn production_user_slot_publication_rejects_an_unbudgeted_context() {
    // Exercise actual independent admission with a genuine kernel Executor /
    // Current bank, rather than count deleted facade source branches.
    super::root_creation::tests::unbudgeted_refused();
}

pub(super) fn boxed_scheduler() -> SchedulerTestFixture {
    let process_table = process_table::tests::isolate_process_table();
    let cpu_publication = super::super::cpu_local::test_publication_lock();
    let runtime_serial = super::scheduling_runtime::test_serial_guard();
    let runqueue_serial = super::runqueue::test_serial_guard();
    super::runqueue::reset_before_publication();
    super::scheduling_runtime::reset_for_test();
    crate::multitask::process_placement::reset_for_tests();
    SchedulerTestFixture {
        _runqueue_reset: RunqueuePublicationReset,
        _runtime_serial: runtime_serial,
        _runqueue_serial: runqueue_serial,
        _cpu_publication: cpu_publication,
        _process_table: process_table,
    }
}

pub(super) fn test_user_context(handle: process_table::ProcessHandle) -> TaskContext {
    TaskContext {
        scheduling_context:
            crate::multitask::scheduler::scheduling_context::SchedulingContext::bind(0, 1),
        saved_rsp: 0,        address_space_root: 0,        user_mode: true,
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
    super::exec_transaction::quiescence::tests::actual_exec_census_retired_borrower_defers_same_detach_then_retries_to_both_consumer_publications();
    super::exec_transaction::quiescence::tests::legacy_witnesses::recycled_uid();
}

#[test]
fn process_stop_is_scheduler_wide_and_sigcont_resumes_before_delivery() {
    crate::multitask::scheduler::linux_thread_state::signals::tests::group_witness();
}

#[test]
fn unmasked_signal_revokes_a_pending_block_arm() {
    crate::multitask::scheduler::linux_thread_state::signals::tests::arm_witness();
}

#[test]
fn process_sigchld_prefers_leader_and_retains_exact_coalesced_causes() {
    crate::multitask::scheduler::linux_thread_state::signals::tests::sigchld_witness();
}

#[test]
fn terminate_user_process_retires_every_live_sibling() {
    crate::multitask::scheduler::current_terminal::tests::group_witness();
}

#[test]
fn terminating_the_last_task_marks_its_process_exiting() {
    crate::multitask::scheduler::current_terminal::tests::last_task_witness();
}

#[test]
fn retirement_revokes_task_and_process_ipc_authority() {
    catalog_witnesses::endpoint_revocation();
}

#[test]
fn retired_user_slot_waits_for_exact_runtime_cleanup_ack() {
    catalog_witnesses::exact_cleanup();
}

#[test]
fn retirement_cleanup_stamps_process_terminal_only_on_last_live_thread() {
    super::terminal_publication::tests::actual_terminal_process_donations_retire_by_original_members_without_catalog_rescan();
}

#[test]
fn exec_sibling_slot_stays_quarantined_until_runtime_cleanup() {
    catalog_witnesses::exact_cleanup();
    super::exec_transaction::quiescence::tests::actual_exec_census_retired_borrower_defers_same_detach_then_retries_to_both_consumer_publications();
    super::reaper::tests::pending_external_terminal_ack_refuses_reaper_before_metadata_or_resource_effects();
}

#[test]
fn exec_rejects_slot_with_unconsumed_side_effect_token() {
    super::exec_transaction::quiescence::tests::legacy_witnesses::pending_kernel();
}

#[test]
fn exec_cannot_rebind_a_live_thread_to_its_process_identity() {
    super::exec_transaction::quiescence::tests::legacy_witnesses::live_identity();
    super::stack_resources::context::intent_tests::independent_exec_candidate_claim_rechecks_exact_uid_handle_pid_tid_and_mode_before_producer();
}

#[test]
fn rejected_thread_attachment_releases_unpublished_stack() {
    super::thread_creation::tests::rejected_attachment();
}

#[test]
fn synchronous_ipc_donation_promotes_and_revokes_a_transitive_user_chain() {
    super::dispatcher::fifo_witnesses::synchronous_fifo();
    super::ordering_publication::tests::actual_original_ordering_preserves_spawn_fifo_and_current_floor_without_funding_transfer();
    super::donation_publication::tests::legacy_witnesses::transitive();
}

#[test]
fn scheduler_block_arm_is_exact_race_safe_and_terminally_revoked() {
    super::current_wait::tests::legacy_witnesses::race_and_typed_cancel();
    super::current_wait::tests::legacy_witnesses::independent_terminal_gates();
}
