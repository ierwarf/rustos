//! Owner: affinity result types; original thread/process owners implement mutations.
//! Boundary: caller and target physical loans, complete membership and full process handle.
//! Lifecycle: original cohorts span mask publication, queue effects and exact membership ACK.
//! Concurrency: no global catalog or raw leaf spans rehome/reschedule work.
//! Failure: offline/empty/foreign/terminal requests refuse before authority effects.
//! Forbidden: scalar UID rechecks or copied process masks as lifetime or partial group commit.
//! Evidence: historical IDs use the same independent production owners with actual births.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AffinityError { InvalidMask, MissingTask, PermissionDenied, WrongAbi }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AffinityCommit {
    pub previous_mask: u64,
    pub reschedule_required: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessAffinitySnapshot {
    pub process_mask: u64,
    pub system_mask: u64,
}

#[cfg(test)]
mod tests {
    use super::super::{process_affinity::tests as process, thread_affinity::tests as thread};
    #[test]
    fn task_affinity_snapshot_is_exact_and_online_bounded() {
        thread::linux_query_and_peer_set_preserve_process_mask_and_other_thread();
        thread::invalid_online_or_empty_mask_refuses_before_any_effect();
    }
    #[test]
    fn linux_thread_affinity_commits_exact_mask_and_previous_value() {
        thread::linux_query_and_peer_set_preserve_process_mask_and_other_thread();
        thread::windows_self_alias_owns_two_physical_refs_and_changes_only_thread_mask();
    }
    #[test]
    fn invalid_affinity_changes_leave_all_authority_unchanged() {
        thread::invalid_online_or_empty_mask_refuses_before_any_effect();
        thread::process_mask_escape_refuses_after_actual_complete_group_change_without_effects();
        process::invalid_requested_mask_and_foreign_full_handle_or_caller_uid_cannot_spend_real_membership();
    }
    #[test]
    fn foreign_task_affinity_is_permission_denied() {
        thread::foreign_physical_process_refuses_after_pin_without_spending_either_binding();
    }
    #[test]
    fn dispatch_panics_when_current_mask_excludes_cpu() {
        // Independent final admission refuses BEFORE claim; this is not a
        // native catalog assertion or a claim of hardware return acceptance.
        super::super::dispatch_claim::tests::actual_selected_affinity_exclusion_refuses_real_claim_and_original_funding_releases();
    }
    #[test]
    fn excluded_running_cpu_requires_remote_reschedule() {
        thread::actual_running_target_keeps_current_custody_and_notifies_owner_cpu();
        process::running_affinity_never_steals_current_custody_but_marks_exclusion_and_requests_reschedule();
    }
    #[test]
    fn child_task_inherits_effective_parent_affinity() {
        super::super::thread_creation::tests::publication_inherits_new_complete_process_mask_after_reservation_not_old_snapshot();
    }
    #[test]
    fn exec_preserves_task_and_process_affinity() {
        super::super::exec_transaction::publication::tests::actual_peer_exec_publishes_owned_frame_metadata_tls_and_staged_mm_before_finalization();
        super::super::exec_transaction::publication::tests::actual_self_exec_never_rewrites_consumed_frame_or_publishes_ready_and_retains_original_bank();
    }
    #[test]
    fn windows_process_affinity_updates_every_live_thread_atomically() {
        process::actual_group_capture_owns_every_live_binding_and_excludes_reuse_until_queue_and_membership_ack();
        process::queued_rehome_keeps_old_process_mask_and_closed_admission_until_actual_owner_effect_returns();
        process::terminal_stop_during_queue_effect_cannot_reuse_or_reanimate_captured_peer_and_survives_final_affinity_ack();
    }
    #[test]
    fn windows_thread_affinity_returns_previous_and_rejects_process_escape() {
        thread::windows_self_alias_owns_two_physical_refs_and_changes_only_thread_mask();
        thread::process_mask_escape_refuses_after_actual_complete_group_change_without_effects();
    }
}
