//! Exact lifecycle observation for the remaining catalog admission boundary.
//!
//! - **Owner:** current_identity publishes lifecycle; no catalog fallback grant.
//! - **Boundary:** only a complete identity-bearing publication answers admission.
//! - **Lifecycle:** missing identity is unknown, never ready.
//! - **Concurrency:** read-only observation, no lock spans validation or claim.
//! - **Failure:** incomplete identity refuses admission; no fabricated readiness.
//! - **Forbidden:** legacy FIFO selection, synthetic native readiness or grants.
//! - **Evidence:** historical FIFO IDs now drive actual dispatcher owners. The
//!   admission itself is `dispatcher::synchronous` (`p.task_id == Some(..) &&
//!   p.lifecycle_dispatchable()`); the test-only mirror of it that lived here
//!   was a mutation target no production path executed and is deleted.

#[cfg(test)]
mod published_identity_tests {
    use super::super::dispatcher::fifo_witnesses;

    // Historical name retained for registry continuity. The catalog fallback
    // itself is DELETED; unknown input must retain the actual original FIFO.
    #[test]
    fn fifo_publication_never_grants_identity_from_an_incomplete_record() {
        fifo_witnesses::incomplete_first_turn();
        fifo_witnesses::first_turn_fifo();
    }
    #[test]
    fn first_turn_cannot_steal_identity_after_a_slot_reuse() {
        fifo_witnesses::first_turn_fifo();
    }
    #[test]
    fn incomplete_fifo_budget_or_frame_cannot_grant_a_catalog_turn() {
        fifo_witnesses::incomplete_first_turn();
        fifo_witnesses::reader_exclusion();
    }
}
