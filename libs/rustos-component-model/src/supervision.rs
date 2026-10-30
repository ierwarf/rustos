//! Bounded per-component restart decisions and terminal fail-closed behavior.
//!
//! This is policy only. Rootd is responsible for applying a decision using
//! exact Job descriptors and for proving endpoint revocation before restart.

use crate::identity::ComponentId;
use crate::realm::{ComponentState, Graph, GraphError, RestartState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    Clean,
    Crash,
    Watchdog,
    DependencyLost,
    Revoked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartPolicy {
    pub max_restarts: u32,
    pub window_ticks: u64,
    pub backoff_ticks: u64,
    pub backoff_max_ticks: u64,
    pub on_failure_escalate: bool,
}

impl RestartPolicy {
    pub const NEVER: Self = Self {
        max_restarts: 0,
        window_ticks: 0,
        backoff_ticks: 0,
        backoff_max_ticks: 0,
        on_failure_escalate: false,
    };

    /// Refuse malformed policy instead of permitting unlimited restarts.
    pub fn new(
        max_restarts: u32,
        window_ticks: u64,
        backoff_ticks: u64,
        backoff_max_ticks: u64,
        on_failure_escalate: bool,
    ) -> Result<Self, GraphError> {
        if max_restarts > 0
            && (window_ticks == 0 || backoff_ticks == 0 || backoff_max_ticks < backoff_ticks)
        {
            return Err(GraphError::InvalidLimit);
        }
        Ok(Self {
            max_restarts,
            window_ticks,
            backoff_ticks,
            backoff_max_ticks,
            on_failure_escalate,
        })
    }

    fn backoff(self, attempt: u32) -> u64 {
        let shift = (attempt.saturating_sub(1)).min(63);
        let factor = 1u64.checked_shl(shift).unwrap_or(0);
        self.backoff_ticks
            .saturating_mul(factor)
            .min(self.backoff_max_ticks)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitAction {
    KeepStopped,
    RestartAt(u64),
    EscalateToParent(ComponentId),
    FailClosed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupervisorDecision {
    pub component: ComponentId,
    pub reason: ExitReason,
    pub action: ExitAction,
    pub attempt: u32,
}

impl Graph {
    /// Observe only the exit of the **exact current service incarnation**.
    /// The caller must authenticate the kernel lifecycle event. Duplicate or
    /// stale notifications are refused and cannot spend a restart budget.
    pub fn on_exit(
        &mut self,
        id: ComponentId,
        incarnation: u64,
        reason: ExitReason,
        now: u64,
    ) -> Result<SupervisorDecision, GraphError> {
        if id == self.root() {
            return Err(GraphError::PermissionDenied);
        }
        let c = self.component(id)?;
        if c.incarnation != incarnation {
            return Err(GraphError::Stale);
        }
        if c.state != ComponentState::Running && c.state != ComponentState::Stopping {
            return Err(GraphError::InvalidState);
        }
        let mut restart = c.restart;
        let policy = c.restart_policy;
        let parent = c.parent.expect("non-root component always has a parent");
        let critical = c.critical;
        if now < restart.last_exit_at {
            return Err(GraphError::InvalidState);
        }
        if restart.attempts == 0
            || now.saturating_sub(restart.window_started_at) >= policy.window_ticks
        {
            restart.window_started_at = now;
            restart.attempts = 0;
        }
        restart.last_exit_at = now;

        let action = if reason == ExitReason::Clean {
            ExitAction::KeepStopped
        } else if restart.attempts < policy.max_restarts {
            let next = restart
                .attempts
                .checked_add(1)
                .ok_or(GraphError::GenerationExhausted)?;
            restart.attempts = next;
            ExitAction::RestartAt(now.saturating_add(policy.backoff(next)))
        } else if critical || policy.on_failure_escalate {
            ExitAction::EscalateToParent(parent)
        } else {
            ExitAction::FailClosed
        };

        self.revoke_instance(id);
        let c = self.component_mut(id)?;
        c.state = ComponentState::Stopped;
        c.restart = restart;
        c.next_restart_at = match action {
            ExitAction::RestartAt(deadline) => Some(deadline),
            _ => None,
        };
        Ok(SupervisorDecision {
            component: id,
            reason,
            action,
            attempt: restart.attempts,
        })
    }

    /// This is only a request to restart a stopped component. Rootd must
    /// finish kernel lease/Job retirement and fresh descriptor verification.
    /// No implicit capability offer is re-established.
    pub fn ready_for_restart(&self, id: ComponentId, now: u64) -> Result<bool, GraphError> {
        let c = self.component(id)?;
        if c.state != ComponentState::Stopped {
            return Err(GraphError::InvalidState);
        }
        Ok(c.next_restart_at.map_or(false, |deadline| now >= deadline))
    }

    pub fn restart_snapshot(&self, id: ComponentId) -> Result<RestartState, GraphError> {
        Ok(self.component(id)?.restart)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded_even_after_32_attempts() {
        let p = RestartPolicy::new(40, 1_000_000, 2, 100, true).unwrap();
        assert_eq!(p.backoff(1), 2);
        assert_eq!(p.backoff(3), 8);
        assert_eq!(p.backoff(40), 100);
    }

    #[test]
    fn invalid_restart_window_is_rejected() {
        assert!(RestartPolicy::new(1, 0, 1, 1, false).is_err());
        assert!(RestartPolicy::new(1, 1, 0, 1, false).is_err());
    }
}
