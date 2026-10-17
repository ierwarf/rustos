#[test]
fn strict_class_requires_explicit_admission_not_a_large_cfs_weight() {
    super::donation_publication::tests::actual_class_admission_uses_trusted_bit_and_preserves_ordinary_custody();
}

/// No userspace topology may cause dispatch stack growth. The actual owner
/// compresses the root at each owned edge instead of recursively truncating it.
#[test]
fn ipc_donation_chain_depth_is_bounded() {
    super::donation_publication::tests::legacy_witnesses::bounded_chain();
}

#[test]
fn bounded_system_burst_reserves_a_ready_user_turn() {
    super::local_dispatch::policy_witnesses::bounded_system_burst_reserves_a_ready_user_turn();
}

#[test]
fn user_reservation_obeys_vruntime_without_a_wall_clock_bypass() {
    super::local_dispatch::policy_witnesses::user_reservation_obeys_vruntime_without_a_wall_clock_bypass();
}

#[test]
fn fair_locality_is_bounded_by_class_and_vruntime_lag() {
    super::local_dispatch::policy_witnesses::fair_locality_is_bounded_by_class_and_vruntime_lag();
}

#[test]
fn overdue_system_task_is_forced_after_latency_bound() {
    super::local_dispatch::policy_witnesses::overdue_system_task_is_forced_after_latency_bound();
}

#[test]
fn overdue_system_continuation_precedes_unrelated_ipc_hint_without_losing_it() {
    super::local_dispatch::policy_witnesses::overdue_system_continuation_precedes_unrelated_ipc_hint_without_losing_it();
}

#[test]
fn stale_pick_hint_falls_through_without_mutating_task_state() {
    super::local_dispatch::policy_witnesses::stale_pick_hint_falls_through_without_mutating_task_state();
}

#[test]
fn overdue_system_continuation_precedes_a_fresh_latency_handoff() {
    super::local_dispatch::policy_witnesses::overdue_system_continuation_precedes_a_fresh_latency_handoff();
}

#[test]
fn event_wait_handoff_is_fifo_deduplicated_and_burst_bounded() {
    // Exact original producer and SAME production FIFO/burst consumer.
    super::ordering_publication::tests::actual_latency_and_process_ordering_use_exact_live_original_targets_without_waking();
}

#[test]
fn dispatch_fairness_and_handoff_state_is_cpu_isolated() {
    super::local_dispatch::policy_witnesses::dispatch_fairness_and_handoff_state_is_cpu_isolated();
}

#[test]
fn ipc_donation_rejects_self_referential_and_retired_targets() {
    super::donation_publication::tests::legacy_witnesses::self_and_terminal_refusal();
}

#[test]
fn ipc_priority_donation_capacity_is_bounded_by_max_task() {
    super::donation_publication::tests::legacy_witnesses::bounded_capacity();
}

/// Bounded donation: a direct handoff floors the target's vruntime
/// toward the caller's (so it is picked promptly) but must never raise
/// it above what it already had - a donation can only help a receiver,
/// never push it backward in the fair queue.
#[test]
fn ipc_donation_floors_target_vruntime_and_never_raises_it() {
    // Original Current and born receiver, including distinct strict-band floor,
    // never-raise, self/idle exclusion and refusal to manufacture a donor loan.
    super::ordering_publication::tests::actual_original_ordering_preserves_spawn_fifo_and_current_floor_without_funding_transfer();
    super::ordering_publication::tests::actual_missing_current_cannot_supply_latency_floor_and_foreign_uid_cannot_publish();
}

#[test]
fn the_authority_sweep_is_silent_for_a_task_executing_on_another_cpu() {
    // Remote tables are not a coherent snapshot after global-guard deletion.
    // Actual diagnostic checks ONLY the entering CPU's original physical bank.
    super::dispatcher::tests::diagnostic_foreign_input();
}
