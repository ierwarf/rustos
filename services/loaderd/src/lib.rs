//! Terminal-reply custody for loaderd scheduling-authority release.
//!
//! - **Owner:** loaderd owns the one-shot decision to relinquish its bootstrap
//!   System class after creating the UI server.
//! - **Boundary:** Request handling may propose demotion; only the service loop
//!   observes the kernel's terminal reply result.
//! - **Lifecycle:** A successful UI spawn records intent, a successful reply
//!   consumes it, and failed/cancelled replies retain bootstrap authority.
//! - **Concurrency:** The loader service loop is the sole intent consumer.
//! - **Failure:** A negative reply status never authorizes demotion.
//! - **Forbidden:** Request handlers must not execute self-demotion directly.
//! - **Evidence:** `scheduler-thread-demotion/SchedulerThreadDemotion`.

#![no_std]

/// Returns whether the service loop may consume a previously recorded
/// post-UI demotion intent.
///
/// Keeping this predicate outside the no-main service binary gives formal
/// conformance a host-testable witness for the terminal-reply boundary.
pub fn completion_demotion_due(reply: i64, demote_after_reply: bool) -> bool {
    reply >= 0 && demote_after_reply
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_bootstrap_demotion_is_custodied_until_terminal_reply() {
        assert!(!completion_demotion_due(-22, true));
        assert!(!completion_demotion_due(0, false));
        assert!(completion_demotion_due(0, true));
    }
}
