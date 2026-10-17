//! Actual owned activation and FIFO/physical-claim regressions.
//! Earlier native FIFO/IRQ inputs are not wake, frame or funding grants.
//! Producer/callback/capacity and consumer claims execute SAME born-resource
//! owners, not a replacement catalog or cfg-hidden legacy algorithm.
#[test]
fn spawn_handoff_is_fifo_deduplicated_and_precedes_ipc_handoff() {
    super::dispatcher::fifo_witnesses::first_turn_fifo();
    super::dispatcher::fifo_witnesses::incomplete_first_turn();
    super::dispatcher::fifo_witnesses::reader_exclusion();
    super::local_dispatch::policy_witnesses::overdue_system_continuation_precedes_unrelated_ipc_hint_without_losing_it();
    super::local_dispatch::policy_witnesses::stale_pick_hint_falls_through_without_mutating_task_state();
    super::activation::publication::tests::actual_activation_holds_original_frame_and_binding_through_callback_before_queue_and_ack();
    super::activation::publication::tests::actual_terminal_during_callback_keeps_hidden_original_retired_member_and_wakes_only_peer();
    super::activation::publication::tests::actual_callback_unwind_leaves_all_suspended_and_releases_frame_loans_for_exact_retry();
    super::activation::publication::tests::actual_single_activation_uses_spawn_fifo_and_rejects_foreign_duplicate_or_retired_members();
}
#[test]
fn authority_commit_is_checked_while_the_complete_cohort_is_still_suspended() {
    super::activation::publication::tests::actual_activation_holds_original_frame_and_binding_through_callback_before_queue_and_ack();
}
#[test]
fn atomic_activation_cohort_is_bounded_at_the_exact_queue_capacity() {
    super::activation::publication::tests::actual_activation_exact_capacity_commits_all_first_turn_records_without_overflow_or_replay();
    super::activation::publication::tests::actual_over_capacity_activation_refuses_before_callback_with_all_real_members_suspended();
}
