//! Owner: historical census IDs use the SAME independent frozen-membership engine.
//! Boundary: original resource/terminal loans; earlier Current/IRQ and external ACK modeled.
//! Lifecycle: born roots -> immutable census -> stop/drain/detach -> original cleanup.
//! Concurrency: actual parent fixture serializes namespace, not production dispatch.
//! Failure: recycled UID and retired borrowers cannot stop a foreign task or ACK drain.
//! Forbidden: native entry or external ACK as architectural/hardware acceptance.
//! Evidence: actual birth, original loans, same terminal pipeline and preserved mutation IDs.
use super::super::quiescence::tests as actual;

#[test]
fn retired_captured_sibling_with_execution_refs_is_not_quiescent_and_is_never_retired_twice() {
    actual::actual_exec_census_retired_borrower_defers_same_detach_then_retries_to_both_consumer_publications();
}

#[test]
fn recycled_catalog_uid_cannot_be_retired_by_old_census_and_peer_request_cannot_retire_own_live_caller() {
    actual::legacy_witnesses::recycled_uid();
    actual::actual_exec_census_cannot_retire_its_live_caller_before_spending_any_sibling_producer();
}
