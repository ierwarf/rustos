//! Typed, bounded admission wait for one post-init service endpoint.
//!
//! - **Owner:** `initd` owns exact `(service_id, pid)` endpoint admission.
//! - **Boundary:** The kernel `ipc_wait_service_endpoint` wait is one slice;
//!   this machine owns the budget, the slice length and the terminal verdict.
//! - **Lifecycle:** `Waiting` -> `Ready` | `Failed`. `Failed` is terminal and
//!   the supervisor stays fail-closed: the child is cleaned up and initd
//!   stops, exactly as before. Nothing here downgrades or skips a dependency.
//! - **Concurrency:** Pure state, no syscall and no clock; the caller feeds the
//!   elapsed time so the transitions are deterministic and unit-testable.
//! - **Failure:** A slice that times out is progress information, not a
//!   verdict. Only an exhausted budget, an exited child, a foreign endpoint
//!   owner or a rejected request end the wait.
//! - **Forbidden:** A budget shorter than two worst-case IPC deadlines. One
//!   stalled reply (`IPC_BOOT_CONTROL_HARD_LIMIT_MS`) must never be the whole
//!   budget: under CPU contention a lone attempt can consume a full deadline.
//! - **Evidence:** `docs/ai/ui-startup-readiness.md` (lane0 #45: devmgrd was
//!   alive and idle for more than 5 s between `service start` and endpoint
//!   registration while the rest of the system progressed).
//!
//! Why this is local and not shared with `uiserver::startup`: that machine
//! tracks several concurrent dependencies with an aggregate verdict and retry
//! backoff for operations that fail fast. This wait is a single event-driven
//! kernel wait that never fails fast, so it needs only slicing and a budget.
//! A shared crate would carry the aggregate and backoff concepts into a place
//! that must not have them.

use rustos_user_abi::{
    performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS, syscall::IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS,
};

/// Total time one service has to publish its endpoint after activation.
pub(super) const ENDPOINT_ADMISSION_BUDGET_MS: u64 = IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS;
/// One kernel wait. Slices exist so a long wait stays observable.
pub(super) const ENDPOINT_ADMISSION_SLICE_MS: u64 = IPC_BOOT_CONTROL_HARD_LIMIT_MS;
/// Pause before re-waiting after a slice that returned early without a verdict.
const EARLY_RETURN_PAUSE_MS: u64 = 10;

const _: () = assert!(ENDPOINT_ADMISSION_BUDGET_MS >= 2 * IPC_BOOT_CONTROL_HARD_LIMIT_MS);
const _: () = assert!(ENDPOINT_ADMISSION_SLICE_MS <= IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FailCause {
    /// Every slice timed out and the budget is spent.
    BudgetExhausted,
    /// The expected pid exited before it published the endpoint (`ESRCH`).
    ServiceExited,
    /// Another pid owns the service id (`EBUSY`).
    ForeignOwner,
    /// The kernel rejected the request itself.
    Rejected,
}

impl FailCause {
    pub(super) const fn label(self) -> &'static str {
        match self {
            FailCause::BudgetExhausted => "budget-exhausted",
            FailCause::ServiceExited => "service-exited",
            FailCause::ForeignOwner => "foreign-owner",
            FailCause::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Failure {
    pub(super) cause: FailCause,
    pub(super) errno: i32,
    pub(super) waited_ms: u64,
    pub(super) slices: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// Wait in the kernel for at most `timeout_ms` after sleeping `pause_ms`.
    Wait {
        timeout_ms: u64,
        pause_ms: u64,
    },
    Ready,
    Failed(Failure),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct EndpointAdmission {
    budget_ms: u64,
    slice_ms: u64,
    slices: u32,
}

impl EndpointAdmission {
    pub(super) const fn new() -> Self {
        Self::with_limits(ENDPOINT_ADMISSION_BUDGET_MS, ENDPOINT_ADMISSION_SLICE_MS)
    }

    pub(super) const fn clipped(parent_remaining_ms: u64) -> Self {
        Self::with_limits(
            if parent_remaining_ms < ENDPOINT_ADMISSION_BUDGET_MS {
                parent_remaining_ms
            } else {
                ENDPOINT_ADMISSION_BUDGET_MS
            },
            ENDPOINT_ADMISSION_SLICE_MS,
        )
    }

    const fn with_limits(budget_ms: u64, slice_ms: u64) -> Self {
        Self {
            budget_ms,
            slice_ms,
            slices: 0,
        }
    }

    pub(super) const fn slices(&self) -> u32 {
        self.slices
    }

    /// The first (or next) step given the time already spent.
    fn next_wait(&mut self, elapsed_ms: u64, pause_ms: u64) -> Step {
        let remaining = self.budget_ms.saturating_sub(elapsed_ms);
        if remaining == 0 {
            return Step::Failed(Failure {
                cause: FailCause::BudgetExhausted,
                errno: libc::ETIMEDOUT,
                waited_ms: elapsed_ms,
                slices: self.slices,
            });
        }
        self.slices = self.slices.saturating_add(1);
        Step::Wait {
            timeout_ms: remaining.min(self.slice_ms).max(1),
            pause_ms,
        }
    }

    pub(super) fn start(&mut self) -> Step {
        self.next_wait(0, 0)
    }

    /// Fold the result of one kernel slice (`Ok` = endpoint admitted).
    pub(super) fn on_slice(&mut self, result: Result<(), i32>, elapsed_ms: u64) -> Step {
        if elapsed_ms >= self.budget_ms {
            return Step::Failed(Failure {
                cause: FailCause::BudgetExhausted,
                errno: libc::ETIMEDOUT,
                waited_ms: elapsed_ms,
                slices: self.slices,
            });
        }
        let errno = match result {
            Ok(()) => return Step::Ready,
            Err(errno) => errno,
        };
        let fail = |cause| {
            Step::Failed(Failure {
                cause,
                errno,
                waited_ms: elapsed_ms,
                slices: self.slices,
            })
        };
        match errno {
            libc::ETIMEDOUT => self.next_wait(elapsed_ms, 0),
            libc::EINTR | libc::EAGAIN => self.next_wait(elapsed_ms, EARLY_RETURN_PAUSE_MS),
            libc::ESRCH => fail(FailCause::ServiceExited),
            libc::EBUSY => fail(FailCause::ForeignOwner),
            _ => fail(FailCause::Rejected),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_remaining_clips_first_and_last_slices() {
        let mut wait = EndpointAdmission::clipped(6_000);
        assert_eq!(
            wait.start(),
            Step::Wait {
                timeout_ms: 5_000,
                pause_ms: 0
            }
        );
        assert_eq!(
            wait.on_slice(Err(libc::ETIMEDOUT), 5_000),
            Step::Wait {
                timeout_ms: 1_000,
                pause_ms: 0
            }
        );
        assert!(matches!(
            wait.on_slice(Err(libc::ETIMEDOUT), 6_000),
            Step::Failed(_)
        ));
        assert!(matches!(
            EndpointAdmission::clipped(0).start(),
            Step::Failed(_)
        ));
    }

    #[test]
    fn budget_is_at_least_two_ipc_deadlines() {
        assert!(ENDPOINT_ADMISSION_BUDGET_MS >= 2 * IPC_BOOT_CONTROL_HARD_LIMIT_MS);
    }

    #[test]
    fn a_single_timed_out_slice_is_not_a_verdict() {
        let mut wait = EndpointAdmission::new();
        let Step::Wait {
            timeout_ms,
            pause_ms,
        } = wait.start()
        else {
            panic!("first step waits");
        };
        assert_eq!(timeout_ms, ENDPOINT_ADMISSION_SLICE_MS);
        assert_eq!(pause_ms, 0);
        assert!(matches!(
            wait.on_slice(Err(libc::ETIMEDOUT), timeout_ms),
            Step::Wait { .. }
        ));
    }

    #[test]
    fn a_late_endpoint_inside_the_budget_is_admitted() {
        let mut wait = EndpointAdmission::new();
        let mut elapsed = 0;
        let mut step = wait.start();
        for _ in 0..4 {
            let Step::Wait { timeout_ms, .. } = step else {
                panic!("still waiting");
            };
            elapsed += timeout_ms;
            step = wait.on_slice(Err(libc::ETIMEDOUT), elapsed);
        }
        assert!(matches!(step, Step::Wait { .. }));
        assert_eq!(wait.on_slice(Ok(()), elapsed + 100), Step::Ready);
    }

    #[test]
    fn exhausting_the_budget_fails_with_the_waited_time() {
        let mut wait = EndpointAdmission::with_limits(12_000, 5_000);
        let mut elapsed = 0;
        let mut step = wait.start();
        let mut waits = Vec::new();
        while let Step::Wait { timeout_ms, .. } = step {
            waits.push(timeout_ms);
            elapsed += timeout_ms;
            step = wait.on_slice(Err(libc::ETIMEDOUT), elapsed);
        }
        assert_eq!(waits, [5_000, 5_000, 2_000], "last slice is clipped");
        assert_eq!(
            step,
            Step::Failed(Failure {
                cause: FailCause::BudgetExhausted,
                errno: libc::ETIMEDOUT,
                waited_ms: 12_000,
                slices: 3,
            })
        );
    }

    #[test]
    fn a_slice_never_exceeds_the_kernel_maximum() {
        let mut wait = EndpointAdmission::new();
        let mut step = wait.start();
        let mut elapsed = 0;
        while let Step::Wait { timeout_ms, .. } = step {
            assert!((1..=IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS).contains(&timeout_ms));
            elapsed += timeout_ms;
            step = wait.on_slice(Err(libc::ETIMEDOUT), elapsed);
        }
    }

    #[test]
    fn exit_foreign_owner_and_rejection_are_terminal_immediately() {
        for (errno, cause) in [
            (libc::ESRCH, FailCause::ServiceExited),
            (libc::EBUSY, FailCause::ForeignOwner),
            (libc::EINVAL, FailCause::Rejected),
            (libc::EPERM, FailCause::Rejected),
        ] {
            let mut wait = EndpointAdmission::new();
            wait.start();
            let Step::Failed(failure) = wait.on_slice(Err(errno), 40) else {
                panic!("errno {errno} must be terminal");
            };
            assert_eq!(failure.cause, cause);
            assert_eq!(failure.errno, errno);
            assert_eq!(failure.waited_ms, 40);
        }
    }

    #[test]
    fn early_returns_retry_after_a_pause_and_still_spend_the_budget() {
        let mut wait = EndpointAdmission::with_limits(100, 50);
        wait.start();
        assert_eq!(
            wait.on_slice(Err(libc::EINTR), 1),
            Step::Wait {
                timeout_ms: 50,
                pause_ms: EARLY_RETURN_PAUSE_MS
            }
        );
        let Step::Failed(failure) = wait.on_slice(Err(libc::EINTR), 100) else {
            panic!("an interrupted wait cannot outlive the budget");
        };
        assert_eq!(failure.cause, FailCause::BudgetExhausted);
    }
}
