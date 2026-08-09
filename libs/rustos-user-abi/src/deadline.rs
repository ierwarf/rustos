//! One monotonic absolute deadline for a multi-step transaction.
//!
//! - **Owner:** the transaction's initiator fixes the end instant once.
//! - **Boundary:** every nested call and every retry sleep derives its budget
//!   from that same end instant.
//! - **Lifecycle:** created at transaction start, consulted per step, terminal
//!   once `remaining_ns` is `None`.
//! - **Concurrency:** a plain value; it carries no interior mutability and no
//!   clock of its own.
//! - **Failure:** expiry is fail-closed and reported as [`DeadlineExpired`]; it
//!   is never rounded up to "one more attempt".
//! - **Forbidden:** a phase-local stopwatch, a fixed sleep longer than the
//!   remaining budget, or a wall-clock reading used as ordering authority.
//!
//! `V5-DEADLINE-012` is the failure this prevents: a five second transaction
//! budget with a fresh hundred millisecond timeout per attempt plus a fixed
//! backoff sleep overruns the budget it claims to respect, and the caller that
//! set the budget observes a false failure.
//!
//! The type is deliberately free of any clock. Services differ in how they read
//! monotonic time — a `std` service has `Instant`, a `no_std` service reads
//! `CLOCK_MONOTONIC` through the runtime — so the source of `now_ns` is the
//! caller's, and only the arithmetic is shared. What must not differ between
//! them is the arithmetic, which is exactly what was diverging.

/// Returns whether a reply observed across a destructive queue take may be
/// published to its caller.
///
/// Expiry is sampled immediately before and after the take. Publication is
/// admitted only when both samples remain unexpired, so a reply that becomes
/// visible during an expired take cannot revive the caller's authority.
#[must_use]
pub const fn reply_observation_allows_publication(
    expired_before_take: bool,
    expired_after_take: bool,
) -> bool {
    !expired_before_take && !expired_after_take
}

/// The transaction budget is exhausted. Callers fail closed; they do not retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeadlineExpired;

/// A transaction's single monotonic end instant, in nanoseconds.
///
/// `start_ns` is retained so a probe can report how much of the budget a step
/// consumed without a second time source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AbsoluteDeadline {
    start_ns: u64,
    end_ns: u64,
}

impl AbsoluteDeadline {
    /// Fixes the end instant at `now_ns + budget_ns`.
    ///
    /// A budget that would overflow saturates rather than wrapping: a wrapped
    /// end instant reads as already expired and would fail a healthy
    /// transaction immediately.
    #[must_use]
    pub const fn after(now_ns: u64, budget_ns: u64) -> Self {
        Self {
            start_ns: now_ns,
            end_ns: now_ns.saturating_add(budget_ns),
        }
    }

    /// The instant this transaction started.
    #[must_use]
    pub const fn start_ns(&self) -> u64 {
        self.start_ns
    }

    /// The instant this transaction must be finished by.
    #[must_use]
    pub const fn end_ns(&self) -> u64 {
        self.end_ns
    }

    /// Budget left at `now_ns`, or `None` once it is exhausted.
    ///
    /// Exhaustion includes the exact end instant: a zero remaining budget
    /// cannot admit a call, so returning `Some(0)` would only push the expiry
    /// decision onto every caller.
    #[must_use]
    pub const fn remaining_ns(&self, now_ns: u64) -> Option<u64> {
        if now_ns >= self.end_ns {
            return None;
        }
        Some(self.end_ns - now_ns)
    }

    /// How much of the budget has been consumed at `now_ns`, saturating at the
    /// full budget. Diagnostic only.
    #[must_use]
    pub const fn elapsed_ns(&self, now_ns: u64) -> u64 {
        if now_ns <= self.start_ns {
            return 0;
        }
        let elapsed = now_ns - self.start_ns;
        let budget = self.end_ns - self.start_ns;
        if elapsed > budget { budget } else { elapsed }
    }

    /// The timeout a nested call may use: the remaining budget, capped by that
    /// call's own limit.
    ///
    /// This is the whole point of the type. A nested call that starts its own
    /// full timeout can outlive the transaction that authorized it.
    pub const fn child_timeout_ns(&self, now_ns: u64, cap_ns: u64) -> Result<u64, DeadlineExpired> {
        match self.remaining_ns(now_ns) {
            None => Err(DeadlineExpired),
            Some(remaining) if remaining < cap_ns => Ok(remaining),
            Some(_) => Ok(cap_ns),
        }
    }

    /// `child_timeout_ns` in milliseconds, for the service call ABI.
    ///
    /// A sub-millisecond remainder rounds up to one millisecond rather than to
    /// zero: zero means "no timeout" in the call ABI, which would convert the
    /// last sliver of a budget into an unbounded wait.
    pub const fn child_timeout_ms(&self, now_ns: u64, cap_ms: u64) -> Result<u64, DeadlineExpired> {
        match self.child_timeout_ns(now_ns, cap_ms.saturating_mul(NANOS_PER_MILLI)) {
            Err(expired) => Err(expired),
            Ok(timeout_ns) => Ok(timeout_ns.div_ceil(NANOS_PER_MILLI)),
        }
    }

    /// The sleep a retry may take: the requested backoff, clamped so the sleep
    /// itself cannot consume the budget it is waiting to use.
    pub const fn retry_backoff_ns(
        &self,
        now_ns: u64,
        backoff_ns: u64,
    ) -> Result<u64, DeadlineExpired> {
        self.child_timeout_ns(now_ns, backoff_ns)
    }
}

pub const NANOS_PER_MILLI: u64 = 1_000_000;
pub const NANOS_PER_SEC: u64 = 1_000_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = NANOS_PER_MILLI;

    #[test]
    fn a_child_call_never_outlives_the_transaction_that_authorized_it() {
        // The V5-DEADLINE-012 counterexample: a 5 s budget, a 100 ms per-call
        // cap, and only a few ms left.
        let deadline = AbsoluteDeadline::after(0, 5_000 * MS);
        assert_eq!(deadline.child_timeout_ns(0, 100 * MS), Ok(100 * MS));
        assert_eq!(
            deadline.child_timeout_ns(4_000 * MS, 100 * MS),
            Ok(100 * MS)
        );
        // Three milliseconds left must not start a hundred millisecond call.
        assert_eq!(deadline.child_timeout_ns(4_997 * MS, 100 * MS), Ok(3 * MS));
        assert_eq!(deadline.child_timeout_ms(4_997 * MS, 100), Ok(3));
    }

    #[test]
    fn expiry_is_terminal_rather_than_a_zero_timeout() {
        let deadline = AbsoluteDeadline::after(1_000, 500);
        assert_eq!(deadline.remaining_ns(1_499), Some(1));
        // The exact end instant is already expired: a zero budget cannot admit
        // a call, and a zero timeout means "no timeout" in the call ABI.
        assert_eq!(deadline.remaining_ns(1_500), None);
        assert_eq!(deadline.remaining_ns(9_999), None);
        assert_eq!(deadline.child_timeout_ns(1_500, 100), Err(DeadlineExpired));
        assert_eq!(deadline.child_timeout_ms(1_500, 100), Err(DeadlineExpired));
        assert_eq!(deadline.retry_backoff_ns(1_500, 100), Err(DeadlineExpired));
    }

    #[test]
    fn a_sub_millisecond_remainder_rounds_up_so_it_cannot_mean_no_timeout() {
        let deadline = AbsoluteDeadline::after(0, 1);
        assert_eq!(deadline.child_timeout_ns(0, 100 * MS), Ok(1));
        assert_eq!(deadline.child_timeout_ms(0, 100), Ok(1));
    }

    #[test]
    fn a_retry_sleep_cannot_consume_the_budget_it_waits_to_use() {
        let deadline = AbsoluteDeadline::after(0, 50 * MS);
        assert_eq!(deadline.retry_backoff_ns(0, 160 * MS), Ok(50 * MS));
        assert_eq!(deadline.retry_backoff_ns(45 * MS, 160 * MS), Ok(5 * MS));
        assert_eq!(
            deadline.retry_backoff_ns(50 * MS, 10 * MS),
            Err(DeadlineExpired)
        );
    }

    #[test]
    fn an_overflowing_budget_saturates_rather_than_expiring_immediately() {
        let deadline = AbsoluteDeadline::after(u64::MAX - 10, NANOS_PER_SEC);
        assert_eq!(deadline.end_ns(), u64::MAX);
        assert_eq!(deadline.remaining_ns(u64::MAX - 10), Some(10));
    }

    #[test]
    fn elapsed_reports_consumption_without_a_second_time_source() {
        let deadline = AbsoluteDeadline::after(100, 400);
        assert_eq!(deadline.elapsed_ns(100), 0);
        assert_eq!(deadline.elapsed_ns(50), 0);
        assert_eq!(deadline.elapsed_ns(300), 200);
        assert_eq!(deadline.elapsed_ns(5_000), 400);
    }

    #[test]
    fn reply_publication_requires_two_unexpired_observations() {
        assert!(reply_observation_allows_publication(false, false));
        assert!(!reply_observation_allows_publication(true, false));
        assert!(!reply_observation_allows_publication(false, true));
        assert!(!reply_observation_allows_publication(true, true));
    }
}

#[cfg(kani)]
mod reply_observation_verification {
    use super::*;

    #[kani::proof]
    fn accepted_reply_observation_has_two_unexpired_samples() {
        let expired_before_take: bool = kani::any();
        let expired_after_take: bool = kani::any();
        let accepted =
            reply_observation_allows_publication(expired_before_take, expired_after_take);

        kani::cover!(accepted);
        if accepted {
            assert!(!expired_before_take);
            assert!(!expired_after_take);
        }
    }

    #[kani::proof]
    fn pre_expired_reply_observation_never_publishes() {
        let expired_after_take: bool = kani::any();
        let accepted = reply_observation_allows_publication(true, expired_after_take);

        kani::cover!(!expired_after_take);
        kani::cover!(expired_after_take);
        assert!(!accepted);
    }

    #[kani::proof]
    fn expiry_during_reply_take_never_publishes() {
        let accepted = reply_observation_allows_publication(false, true);

        kani::cover!(!accepted);
        assert!(!accepted);
    }
}
