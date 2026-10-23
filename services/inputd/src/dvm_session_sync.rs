//! Retained DVM input-session authority handoff.
//!
//! - **Owner:** inputd owns decoded input batches; netd owns the matching
//!   network-session grant and revoke policy.
//! - **Boundary:** Input after a decoded SESSION_START/END is not publishable
//!   until that record's netd transitions have returned an exact success.
//! - **Lifecycle:** Publish preceding events, reset local provider state, ACK
//!   revoke then grant in record order, or retain the exact suffix for retry.
//! - **Concurrency:** No policy-queue guard crosses service lookup or IPC.
//! - **Failure:** Transient failure retains the bounded batch; the caller owns
//!   the five-second fail-closed deadline.
//! - **Forbidden:** No decoder reset followed by later-ring drain, event
//!   publication before grant, or replay of an already-ACKed transition.

use rustos_user_abi::deadline::{AbsoluteDeadline, NANOS_PER_MILLI, NANOS_PER_SEC};
use rustos_user_abi::syscall::{InputIngressWire, NETD_DVM_SESSION_GRANT, NETD_DVM_SESSION_REVOKE};

use super::dvm_protocol::DvmOutcome;
use super::{apply_dvm_ingress_wire, lock_input_queue_for_ingestion, SharedInputQueue};

pub(super) const RETRY_BACKOFF_NS: u64 = 10 * NANOS_PER_MILLI;
pub(super) const RETRY_BACKOFF_MAX_NS: u64 = 160 * NANOS_PER_MILLI;
pub(super) const TIMEOUT_NS: u64 = 5 * NANOS_PER_SEC;
/// A grant/revoke changes authenticated cross-service policy and can enter the
/// net packet broker. It is not a non-consuming readiness query, so each
/// attempt owns the interactive control budget while the retained batch keeps
/// the aggregate fail-closed deadline bounded by [`TIMEOUT`].
pub(super) const CALL_DEADLINE_MS: u64 =
    rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS;

/// `CLOCK_MONOTONIC` in nanoseconds.
///
/// The same base the `no_std` services read through
/// `rustos_svc_runtime::syscall::monotonic_nanos`, so a deadline that crosses
/// a service boundary keeps one time base. `Instant` cannot be used here: it
/// exposes no absolute value, so it cannot express a deadline another service
/// is meant to honour.
pub(super) fn monotonic_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the call writes exactly one `timespec` through this exclusive
    // borrow and takes no other pointer.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    u64::try_from(ts.tv_sec)
        .unwrap_or(0)
        .saturating_mul(NANOS_PER_SEC)
        .saturating_add(u64::try_from(ts.tv_nsec).unwrap_or(0))
}

pub(super) fn retry_backoff_for_attempt(attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(4);
    RETRY_BACKOFF_NS
        .saturating_mul(1_u64 << shift)
        .min(RETRY_BACKOFF_MAX_NS)
}

pub(super) fn apply(
    queue: &SharedInputQueue,
    outcomes: &mut [DvmOutcome],
    pending_events: &mut Vec<InputIngressWire>,
    deadline: AbsoluteDeadline,
    mut notify_session: impl FnMut(u32, u64, AbsoluteDeadline) -> Result<(), i32>,
) -> Result<(bool, bool), i32> {
    pending_events.clear();
    for outcome in outcomes.iter_mut() {
        if !pending_events.is_empty()
            && (outcome.reset_input
                || outcome.revoke_epoch.is_some()
                || outcome.grant_epoch.is_some())
        {
            // Publish the consumed run before this record's reset/authority
            // barrier. Its events must neither cross the barrier nor replay
            // if a later notification is refused. Keep IPC outside the lock.
            let mut queue = lock_input_queue_for_ingestion(queue);
            for wire in pending_events.drain(..) {
                apply_dvm_ingress_wire(&mut queue, &wire);
            }
        }
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
            if deadline.remaining_ns(monotonic_nanos()).is_none() {
                pending_events.clear();
                lock_input_queue_for_ingestion(queue).reset_dvm_input();
                return Err(libc::ETIMEDOUT);
            }
            if let Err(errno) = notify_session(epoch, action, deadline) {
                pending_events.clear();
                lock_input_queue_for_ingestion(queue).reset_dvm_input();
                return Err(errno);
            }
            // Clear only the exact ACKed step. If grant fails after revoke,
            // the next attempt starts at grant and cannot replay stale revoke.
            *epoch_slot = None;
        }
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

    use rustos_user_abi::syscall::{
        InputDvmRecordWire, InputIngressWire, InputPointerPacketWire, INPUTD_ACCESS_NATIVE,
        INPUTD_DVM_RECORD_BYTES, INPUTD_INGRESS_FLAG_DVM_SOURCE,
        INPUTD_INGRESS_KIND_POINTER_PACKET, NETD_DVM_SESSION_GRANT, NETD_DVM_SESSION_REVOKE,
    };

    use super::{
        apply, monotonic_nanos, retry_backoff_for_attempt, AbsoluteDeadline, CALL_DEADLINE_MS,
        RETRY_BACKOFF_MAX_NS, RETRY_BACKOFF_NS, TIMEOUT_NS,
    };
    use crate::dvm_protocol::{DvmDecoder, DvmOutcome};
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

    fn record(kind: u8, epoch: u32, sequence: u32, payload: &[u8]) -> InputDvmRecordWire {
        let mut record = InputDvmRecordWire {
            transport_generation: 7,
            len: INPUTD_DVM_RECORD_BYTES as u16,
            ..InputDvmRecordWire::default()
        };
        record.bytes[..4].copy_from_slice(b"RDI1");
        record.bytes[4] = 3;
        record.bytes[5] = kind;
        record.bytes[8..12].copy_from_slice(&epoch.to_be_bytes());
        record.bytes[12..16].copy_from_slice(&sequence.to_be_bytes());
        record.bytes[16..16 + payload.len()].copy_from_slice(payload);
        let mut crc = !0_u32;
        for byte in &record.bytes[..28] {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ ((crc & 1).wrapping_neg() & 0xedb8_8320);
            }
        }
        record.bytes[28..32].copy_from_slice(&(!crc).to_be_bytes());
        record
    }

    fn decode(records: &[InputDvmRecordWire]) -> Vec<DvmOutcome> {
        let mut decoder = DvmDecoder::default();
        records
            .iter()
            .map(|record| decoder.consume(record))
            .collect()
    }

    fn assert_released_presses(queue: &crate::SharedInputQueue) {
        let mut queue = lock_input_queue(queue);
        let events = queue.events.drain(..).collect::<Vec<_>>();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].kind, input_evdev::INPUT_KIND_POINTER_BUTTON);
        assert_eq!(events[1].kind, input_evdev::INPUT_KIND_KEYBOARD);
        assert_eq!(events[2].kind, input_evdev::INPUT_KIND_KEYBOARD);
        assert_eq!(events[3].kind, input_evdev::INPUT_KIND_POINTER_BUTTON);
        assert_eq!(events[0].action, input_evdev::INPUT_ACTION_PRESSED);
        assert_eq!(events[1].action, input_evdev::INPUT_ACTION_PRESSED);
        assert_eq!(events[2].action, input_evdev::INPUT_ACTION_RELEASED);
        assert_eq!(events[3].action, input_evdev::INPUT_ACTION_RELEASED);
        assert_eq!(events[1].code, events[2].code);
        assert_eq!(events[0].code, events[3].code);
        assert_eq!(queue.dvm_pointer_buttons, 0);
        // A second reset must not find any held keyboard or pointer state.
        queue.reset_dvm_input();
        assert!(queue.is_empty());
    }

    #[test]
    fn same_batch_session_end_releases_pressed_key_and_button_in_record_order() {
        let queue = Arc::new(SharedInputQueueState::new());
        let mut outcomes = decode(&[
            record(0, 7, 0, &[]),
            record(2, 7, 1, &[0, 0, 0, 0, 0, 0, 0, 0, 1]),
            record(1, 7, 2, &[0, 30, 1]),
            record(3, 7, 3, &[]),
        ]);
        let mut pending_events = Vec::new();
        let mut calls = Vec::new();
        assert_eq!(
            apply(
                &queue,
                &mut outcomes,
                &mut pending_events,
                AbsoluteDeadline::after(monotonic_nanos(), TIMEOUT_NS),
                |epoch, action, _| {
                    assert!(queue.queue.try_lock().is_ok());
                    calls.push((epoch, action));
                    assert_eq!(
                        lock_input_queue(&queue).len(),
                        if calls.len() == 1 { 0 } else { 4 }
                    );
                    Ok(())
                },
            ),
            Ok((true, true))
        );
        assert_eq!(
            calls,
            [(7, NETD_DVM_SESSION_GRANT), (7, NETD_DVM_SESSION_REVOKE)]
        );
        assert!(outcomes
            .iter()
            .all(|outcome| *outcome == DvmOutcome::default()));
        assert!(pending_events.is_empty());
        assert_released_presses(&queue);
    }

    #[test]
    fn refused_notifications_retry_only_the_unaccepted_ordered_suffix() {
        let queue = Arc::new(SharedInputQueueState::new());
        let mut outcomes = decode(&[
            record(0, 7, 0, &[]),
            record(2, 7, 1, &[0, 0, 0, 0, 0, 0, 0, 0, 1]),
            record(1, 7, 2, &[0, 30, 1]),
            record(0, 8, 0, &[]),
            record(2, 8, 1, &[0, 0, 0, 0, 0, 0, 0, 0, 1]),
            record(1, 8, 2, &[0, 31, 1]),
            record(3, 8, 3, &[]),
        ]);
        let mut pending_events = Vec::new();
        let deadline = AbsoluteDeadline::after(monotonic_nanos(), TIMEOUT_NS);
        let mut accepted = Vec::new();
        for attempt in 0..2 {
            let mut calls = Vec::new();
            assert_eq!(
                apply(
                    &queue,
                    &mut outcomes,
                    &mut pending_events,
                    deadline,
                    |epoch, action, received| {
                        assert!(queue.queue.try_lock().is_ok());
                        assert_eq!(received, deadline);
                        calls.push((epoch, action));
                        if (epoch, action) == (8, NETD_DVM_SESSION_GRANT) {
                            return Err(libc::EAGAIN);
                        }
                        accepted.push((epoch, action));
                        Ok(())
                    }
                ),
                Err(libc::EAGAIN)
            );
            if attempt == 0 {
                assert_eq!(
                    calls,
                    [
                        (7, NETD_DVM_SESSION_GRANT),
                        (7, NETD_DVM_SESSION_REVOKE),
                        (8, NETD_DVM_SESSION_GRANT)
                    ]
                );
                assert_released_presses(&queue);
            } else {
                assert_eq!(calls, [(8, NETD_DVM_SESSION_GRANT)]);
                assert!(lock_input_queue(&queue).is_empty());
            }
            assert!(pending_events.is_empty());
            assert!(outcomes[..3]
                .iter()
                .all(|outcome| *outcome == DvmOutcome::default()));
            assert_eq!(outcomes[3].revoke_epoch, None);
            assert_eq!(outcomes[3].grant_epoch, Some(8));
            assert!(!outcomes[3].reset_input);
            assert!(outcomes[4].event.is_some());
        }

        // The new grant succeeds, but its end notification is refused after
        // the second session's input has already been published and reset.
        assert_eq!(
            apply(
                &queue,
                &mut outcomes,
                &mut pending_events,
                deadline,
                |epoch, action, _| {
                    assert!(queue.queue.try_lock().is_ok());
                    if action == NETD_DVM_SESSION_REVOKE {
                        assert_eq!(epoch, 8);
                        return Err(libc::ENOSYS);
                    }
                    accepted.push((epoch, action));
                    Ok(())
                }
            ),
            Err(libc::ENOSYS)
        );
        assert_released_presses(&queue);
        assert!(outcomes[..6]
            .iter()
            .all(|outcome| *outcome == DvmOutcome::default()));
        assert_eq!(outcomes[6].revoke_epoch, Some(8));
        assert!(!outcomes[6].reset_input);
        assert_eq!(
            apply(
                &queue,
                &mut outcomes,
                &mut pending_events,
                deadline,
                |epoch, action, _| {
                    accepted.push((epoch, action));
                    Ok(())
                }
            ),
            Ok((true, true))
        );
        assert_eq!(
            accepted,
            [
                (7, NETD_DVM_SESSION_GRANT),
                (7, NETD_DVM_SESSION_REVOKE),
                (8, NETD_DVM_SESSION_GRANT),
                (8, NETD_DVM_SESSION_REVOKE)
            ]
        );
        assert_eq!(
            apply(
                &queue,
                &mut outcomes,
                &mut pending_events,
                deadline,
                |_, _, _| panic!("replayed ACK")
            ),
            Ok((false, false))
        );
        assert!(lock_input_queue(&queue).is_empty());
        assert!(pending_events.is_empty());
    }

    #[test]
    fn expired_session_sync_resets_held_input_without_notifying_or_replaying() {
        let queue = Arc::new(SharedInputQueueState::new());
        {
            let mut queue = lock_input_queue_for_ingestion(&queue);
            queue.push_dvm_pointer_packet(InputPointerPacketWire {
                buttons: 1,
                ..InputPointerPacketWire::default()
            });
            queue.push_dvm_linux_key(30, input_evdev::INPUT_ACTION_PRESSED);
        }
        let mut outcomes = [DvmOutcome {
            revoke_epoch: Some(7),
            ..DvmOutcome::default()
        }];
        let mut pending_events = Vec::new();
        assert_eq!(
            apply(
                &queue,
                &mut outcomes,
                &mut pending_events,
                AbsoluteDeadline::after(monotonic_nanos(), 0),
                |_, _, _| panic!("expired notification")
            ),
            Err(libc::ETIMEDOUT)
        );
        assert_eq!(outcomes[0].revoke_epoch, Some(7));
        assert!(pending_events.is_empty());
        assert_released_presses(&queue);
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
        let deadline = AbsoluteDeadline::after(monotonic_nanos(), TIMEOUT_NS);

        let result = apply(
            &queue,
            &mut outcomes,
            &mut pending_events,
            deadline,
            |epoch, action, received_deadline| {
                observed_unlocked = queue.queue.try_lock().is_ok();
                assert_eq!((epoch, action), (7, NETD_DVM_SESSION_GRANT));
                assert_eq!(received_deadline, deadline);
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
                AbsoluteDeadline::after(monotonic_nanos(), TIMEOUT_NS),
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
                AbsoluteDeadline::after(monotonic_nanos(), TIMEOUT_NS),
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
                AbsoluteDeadline::after(monotonic_nanos(), TIMEOUT_NS),
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
        assert_eq!(retry_backoff_for_attempt(1), RETRY_BACKOFF_NS);
        assert_eq!(retry_backoff_for_attempt(32), RETRY_BACKOFF_MAX_NS);
        assert_eq!(
            CALL_DEADLINE_MS,
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
        );
        assert!(CALL_DEADLINE_MS > rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS);
        // One per-call cap and one retry backoff must both fit inside the
        // transaction budget, or the aggregate can only be met by luck.
        assert!(CALL_DEADLINE_MS * super::NANOS_PER_MILLI < TIMEOUT_NS);
        assert!(RETRY_BACKOFF_MAX_NS < TIMEOUT_NS);

        // The budget is shared, not restarted: 37 ms of budget caps a 100 ms
        // call, and the exact end instant is terminal rather than a zero
        // timeout, which the call ABI reads as "no timeout".
        let deadline = AbsoluteDeadline::after(1_000, 37 * super::NANOS_PER_MILLI);
        assert_eq!(deadline.child_timeout_ms(1_000, 100), Ok(37));
        assert_eq!(
            deadline.retry_backoff_ns(1_000, 50 * super::NANOS_PER_MILLI),
            Ok(37 * super::NANOS_PER_MILLI)
        );
        assert!(deadline
            .child_timeout_ms(1_000 + 37 * super::NANOS_PER_MILLI, 100)
            .is_err());
    }

    #[test]
    fn the_monotonic_base_is_readable_and_never_moves_backwards() {
        let first = monotonic_nanos();
        let second = monotonic_nanos();
        assert!(first != 0, "CLOCK_MONOTONIC must be readable");
        assert!(second >= first);
    }
}
