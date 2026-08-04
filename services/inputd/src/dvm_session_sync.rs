//! Retained DVM input-session authority handoff.
//!
//! - **Owner:** inputd owns decoded input batches; netd owns the matching
//!   network-session grant and revoke policy.
//! - **Boundary:** A decoded SESSION_START/END is not publishable input until
//!   every ordered netd transition has returned an exact success.
//! - **Lifecycle:** Reset local provider state, ACK revoke then grant, publish
//!   retained events, or preserve the exact remaining suffix for retry.
//! - **Concurrency:** No policy-queue guard crosses service lookup or IPC.
//! - **Failure:** Transient failure retains the bounded batch; the caller owns
//!   the five-second fail-closed deadline.
//! - **Forbidden:** No decoder reset followed by later-ring drain, event
//!   publication before grant, or replay of an already-ACKed transition.

use std::time::{Duration, Instant};

use rustos_user_abi::syscall::{InputIngressWire, NETD_DVM_SESSION_GRANT, NETD_DVM_SESSION_REVOKE};

use super::dvm_protocol::DvmOutcome;
use super::{apply_dvm_ingress_wire, lock_input_queue_for_ingestion, SharedInputQueue};

pub(super) const RETRY_BACKOFF: Duration = Duration::from_millis(10);
pub(super) const RETRY_BACKOFF_MAX: Duration = Duration::from_millis(160);
pub(super) const TIMEOUT: Duration = Duration::from_secs(5);
/// A grant/revoke changes authenticated cross-service policy and can enter the
/// net packet broker. It is not a non-consuming readiness query, so each
/// attempt owns the interactive control budget while the retained batch keeps
/// the aggregate fail-closed deadline bounded by [`TIMEOUT`].
pub(super) const CALL_DEADLINE_MS: u64 =
    rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS;

/// One monotonic end-to-end deadline for the complete retained session batch.
/// Every nested service call and retry sleep derives its budget from the same
/// end instant; no phase may start a fresh stopwatch.
#[derive(Clone, Copy, Debug)]
pub(super) struct AbsoluteDeadline {
    end: Instant,
}

pub(super) fn retry_backoff_for_attempt(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(4);
    RETRY_BACKOFF
        .saturating_mul(1_u32 << shift)
        .min(RETRY_BACKOFF_MAX)
}

impl AbsoluteDeadline {
    pub(super) fn after(now: Instant, budget: Duration) -> Self {
        Self { end: now + budget }
    }

    fn remaining(self, now: Instant) -> Option<Duration> {
        self.end
            .checked_duration_since(now)
            .filter(|left| !left.is_zero())
    }

    pub(super) fn child_timeout_ms(self, now: Instant, cap_ms: u64) -> Option<u64> {
        let remaining_ms = u64::try_from(self.remaining(now)?.as_millis()).ok()?;
        (remaining_ms != 0).then_some(remaining_ms.min(cap_ms))
    }

    pub(super) fn retry_backoff(self, now: Instant, cap: Duration) -> Option<Duration> {
        self.remaining(now).map(|remaining| remaining.min(cap))
    }
}

pub(super) fn apply(
    queue: &SharedInputQueue,
    outcomes: &mut [DvmOutcome],
    pending_events: &mut Vec<InputIngressWire>,
    deadline: AbsoluteDeadline,
    mut notify_session: impl FnMut(u32, u64, u64) -> Result<(), i32>,
) -> Result<(bool, bool), i32> {
    pending_events.clear();
    for outcome in outcomes.iter_mut() {
        if outcome.reset_input {
            lock_input_queue_for_ingestion(queue).reset_dvm_input();
            outcome.reset_input = false;
        }
        for (epoch_slot, action) in [
            (&mut outcome.revoke_epoch, NETD_DVM_SESSION_REVOKE),
            (&mut outcome.grant_epoch, NETD_DVM_SESSION_GRANT),
        ] {
            let Some(epoch) = *epoch_slot else {
                continue;
            };
            // The queue lock stays local to reset/publication. A synchronous
            // service call here while holding it would deadlock readers and
            // make netd startup ordering an input liveness dependency.
            let Some(timeout_ms) = deadline.child_timeout_ms(Instant::now(), CALL_DEADLINE_MS)
            else {
                pending_events.clear();
                lock_input_queue_for_ingestion(queue).reset_dvm_input();
                return Err(libc::ETIMEDOUT);
            };
            if let Err(errno) = notify_session(epoch, action, timeout_ms) {
                pending_events.clear();
                lock_input_queue_for_ingestion(queue).reset_dvm_input();
                return Err(errno);
            }
            // Clear only the exact ACKed step. If grant fails after revoke,
            // the next attempt starts at grant and cannot replay stale revoke.
            *epoch_slot = None;
        }
    }
    for outcome in outcomes.iter_mut() {
        if let Some(wire) = outcome.event.take() {
            pending_events.push(wire);
        }
    }

    let mut queue = lock_input_queue_for_ingestion(queue);
    for wire in pending_events.drain(..) {
        apply_dvm_ingress_wire(&mut queue, &wire);
    }
    Ok(queue.take_dvm_ingress_observations())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rustos_user_abi::syscall::{
        InputIngressWire, InputPointerPacketWire, INPUTD_ACCESS_NATIVE,
        INPUTD_INGRESS_FLAG_DVM_SOURCE, INPUTD_INGRESS_KIND_POINTER_PACKET, NETD_DVM_SESSION_GRANT,
    };

    use super::{
        apply, retry_backoff_for_attempt, AbsoluteDeadline, CALL_DEADLINE_MS, RETRY_BACKOFF,
        RETRY_BACKOFF_MAX, TIMEOUT,
    };
    use crate::dvm_protocol::DvmOutcome;
    use crate::{lock_input_queue, lock_input_queue_for_ingestion, SharedInputQueueState};

    fn pointer_motion(dx: i32, dy: i32) -> input_evdev::InputEvent {
        input_evdev::InputEvent {
            kind: input_evdev::INPUT_KIND_POINTER_MOTION,
            action: input_evdev::INPUT_ACTION_NONE,
            code: 0,
            value0: dx,
            value1: dy,
            modifiers: 0,
            text: 0,
        }
    }

    #[test]
    fn session_authority_sync_never_holds_the_policy_queue_lock() {
        let queue = Arc::new(SharedInputQueueState::new());
        let mut pending_events = Vec::new();
        let mut outcomes = [DvmOutcome {
            grant_epoch: Some(7),
            ..DvmOutcome::default()
        }];
        let mut observed_unlocked = false;
        let deadline = AbsoluteDeadline::after(Instant::now(), TIMEOUT);

        let result = apply(
            &queue,
            &mut outcomes,
            &mut pending_events,
            deadline,
            |epoch, action, timeout_ms| {
                observed_unlocked = queue.queue.try_lock().is_ok();
                assert_eq!((epoch, action), (7, NETD_DVM_SESSION_GRANT));
                assert!(timeout_ms <= CALL_DEADLINE_MS);
                Ok(())
            },
        );

        assert_eq!(result, Ok((false, false)));
        assert!(observed_unlocked);
    }

    #[test]
    fn failed_session_authority_sync_resets_without_killing_ring_progress() {
        let queue = Arc::new(SharedInputQueueState::new());
        lock_input_queue_for_ingestion(&queue).push(pointer_motion(4, 2));
        let mut pending_events = Vec::new();
        let mut outcomes = [DvmOutcome {
            revoke_epoch: Some(9),
            reset_input: true,
            ..DvmOutcome::default()
        }];

        assert_eq!(
            apply(
                &queue,
                &mut outcomes,
                &mut pending_events,
                AbsoluteDeadline::after(Instant::now(), TIMEOUT),
                |_, _, _| { Err(libc::ETIMEDOUT) }
            ),
            Err(libc::ETIMEDOUT)
        );
        assert_eq!(lock_input_queue(&queue).len(), 1);
        assert!(pending_events.is_empty());
    }

    #[test]
    fn failed_session_grant_is_retryable_without_losing_following_input() {
        let queue = Arc::new(SharedInputQueueState::new());
        let mut pending_events = Vec::new();
        let mut outcomes = [DvmOutcome {
            event: Some(InputIngressWire {
                kind: INPUTD_INGRESS_KIND_POINTER_PACKET,
                access: INPUTD_ACCESS_NATIVE,
                flags: INPUTD_INGRESS_FLAG_DVM_SOURCE,
                pointer_packet: InputPointerPacketWire {
                    dx: 4,
                    dy: 2,
                    ..InputPointerPacketWire::default()
                },
                ..InputIngressWire::default()
            }),
            reset_input: true,
            grant_epoch: Some(7),
            ..DvmOutcome::default()
        }];

        assert_eq!(
            apply(
                &queue,
                &mut outcomes,
                &mut pending_events,
                AbsoluteDeadline::after(Instant::now(), TIMEOUT),
                |_, _, _| { Err(libc::ENOSYS) }
            ),
            Err(libc::ENOSYS)
        );
        assert_eq!(outcomes[0].grant_epoch, Some(7));
        assert!(outcomes[0].event.is_some());
        assert_eq!(lock_input_queue(&queue).len(), 0);

        assert_eq!(
            apply(
                &queue,
                &mut outcomes,
                &mut pending_events,
                AbsoluteDeadline::after(Instant::now(), TIMEOUT),
                |epoch, action, _| {
                    assert_eq!((epoch, action), (7, NETD_DVM_SESSION_GRANT));
                    Ok(())
                }
            ),
            Ok((false, true))
        );
        assert_eq!(outcomes[0].grant_epoch, None);
        assert!(outcomes[0].event.is_none());
        assert_eq!(lock_input_queue(&queue).len(), 1);
    }

    #[test]
    fn session_authority_retry_deadline_is_bounded() {
        assert_eq!(RETRY_BACKOFF, Duration::from_millis(10));
        assert_eq!(retry_backoff_for_attempt(1), RETRY_BACKOFF);
        assert_eq!(retry_backoff_for_attempt(32), RETRY_BACKOFF_MAX);
        assert_eq!(TIMEOUT, Duration::from_secs(5));
        assert_eq!(
            CALL_DEADLINE_MS,
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
        );
        assert!(CALL_DEADLINE_MS > rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS);
        assert!(Duration::from_millis(CALL_DEADLINE_MS) < TIMEOUT);
        assert!(RETRY_BACKOFF < TIMEOUT);

        let now = Instant::now();
        let deadline = AbsoluteDeadline::after(now, Duration::from_millis(37));
        assert_eq!(deadline.child_timeout_ms(now, 100), Some(37));
        assert_eq!(
            deadline.retry_backoff(now, Duration::from_millis(50)),
            Some(Duration::from_millis(37))
        );
        assert_eq!(
            deadline.child_timeout_ms(now + Duration::from_millis(37), 100),
            None
        );
    }
}
