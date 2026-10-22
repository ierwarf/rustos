//! Exact lifecycle observation for the remaining catalog admission boundary.
//!
//! - **Owner:** current_identity publishes lifecycle; no catalog fallback grant.
//! - **Boundary:** only a complete identity-bearing publication answers admission.
//! - **Lifecycle:** missing identity is unknown, never ready.
//! - **Concurrency:** read-only observation, no lock spans validation or claim.
//! - **Failure:** incomplete identity refuses admission; no fabricated readiness.
//! - **Forbidden:** legacy FIFO selection, synthetic native readiness or grants.
//! - **Evidence:** historical FIFO IDs now drive actual dispatcher owners.

/// Absence is unknown, never a catalog grant. Remaining cold catalog users
/// consume only this observation; FIFO selection has no Scheduler authority.
#[cfg(test)]
pub(super) fn published_handoff_lifecycle(
    publication: Option<crate::multitask::current_identity::DispatchPublication>,
) -> Option<bool> {
    publication
        .filter(|identity| identity.task_id.is_some())
        .map(|identity| identity.lifecycle_dispatchable())
}

#[cfg(test)]
mod published_identity_tests {
    use super::super::dispatcher::fifo_witnesses;

    // Historical name retained for registry continuity. The catalog fallback
    // itself is DELETED; unknown input must retain the actual original FIFO.
    #[test]
    fn exact_fifo_identity_only_falls_back_on_incomplete_publication() {
        fifo_witnesses::incomplete_first_turn();
        fifo_witnesses::first_turn_fifo();
    }
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
    fn incomplete_handoff_lifecycle_cannot_become_production_admission() {
        use super::published_handoff_lifecycle;
        use crate::multitask::current_identity::DispatchPublication;
        assert_eq!(published_handoff_lifecycle(None), None);
        let mut publication = DispatchPublication::default();
        assert_eq!(published_handoff_lifecycle(Some(publication)), None);
        publication.task_id = Some(37);
        assert_eq!(published_handoff_lifecycle(Some(publication)), Some(true));
    }
    #[test]
    fn incomplete_fifo_budget_or_frame_cannot_grant_a_catalog_turn() {
        fifo_witnesses::incomplete_first_turn();
        fifo_witnesses::reader_exclusion();
    }
}
