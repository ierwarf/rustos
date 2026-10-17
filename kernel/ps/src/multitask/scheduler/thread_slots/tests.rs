//! Historical thread-slot IDs use the SAME actual physical reservation,
//! cancellation and complete-mask publication owners, not a catalog bitmap.
use super::super::thread_creation::tests as actual;

#[test]
fn a_reserved_thread_slot_is_never_handed_to_a_process_allocation() {
    actual::legacy_witnesses::reserved_exclusion();
}

#[test]
fn sleepable_windows_thread_commit_joins_new_process_mask_not_obsolete_inheritance() {
    actual::legacy_witnesses::inheritance();
    actual::publication_inherits_new_complete_process_mask_after_reservation_not_old_snapshot();
    actual::final_retired_creator_or_wrong_abi_refusal_cancels_real_unpublished_ticket_and_attachment();
}
