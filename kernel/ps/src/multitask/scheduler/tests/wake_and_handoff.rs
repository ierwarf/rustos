#[test]
fn raced_wake_never_validates_a_consumed_current_frame() {
    super::current_wait::tests::legacy_witnesses::race_and_typed_cancel();
}

#[test]
fn synchronous_ipc_prefers_the_callers_cpu_only_when_dispatch_eligible() {
    super::ipc_publication::tests::actual_cold_call_keeps_sender_unarmed_and_original_loans_through_receiver_fifo();
    super::ipc_publication::tests::original_call_trace();
    super::ipc_publication::tests::actual_cross_cpu_call_keeps_original_funding_wait_fifo_and_mailbox_generation();
}

#[test]
fn fast_ipc_commit_requires_exact_typed_waits_and_mutates_both_peers_once() {
    // Supersedes the scalar catalog projection with the ACTUAL production
    // producer and two born physical participants, original Current and FIFO.
    crate::multitask::scheduler::ipc_publication::tests::original_call_trace();
}

#[test]
fn pager_fault_handoff_requires_exact_waits_and_donates_after_worker_wake() {
    // Original public witness now exercises actual born Current + worker,
    // exact typed publication/FIFO and original-loan latency accounting.
    crate::multitask::scheduler::pager_handoff::tests::original_pager_trace();
}

#[test]
fn wake_transition_publishes_one_owner_before_commit_and_claims_once_after() {
    super::dispatcher::maintenance::tests::legacy_witnesses::transition_wake();
}
