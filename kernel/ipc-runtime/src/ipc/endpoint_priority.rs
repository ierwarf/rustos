//! Scheduler-derived endpoint delivery lanes.
//!
//! - **Owner:** IPC runtime owns bounded transport ordering; the scheduler
//!   owns task-class decisions.
//! - **Boundary:** Compat samples the live kernel class before enqueue; ring3
//!   request bytes never become scheduling authority.
//! - **Lifecycle:** Enqueue in one lane, select one head, validate, consume,
//!   reply or cancel the exact generational message.
//! - **Concurrency:** Both lanes, the burst counter, and committed head pop
//!   remain under one endpoint slot guard.
//! - **Failure:** Capacity and receive-buffer failures preserve both heads and
//!   the burst counter; an impossible selected-head mismatch panics.
//! - **Forbidden:** No untrusted priority field, allocation under the slot
//!   guard, cross-lane FIFO claim, or unbounded strict-priority starvation.
//! - **Evidence:** `IpcPriorityQueue.tla`, its semantic mutant, and
//!   `endpoint_system_calls_bypass_backlog_without_starving_ordinary_lane`.

use alloc::collections::VecDeque;

use super::EndpointOwner;

/// A strict-priority sender may pass at most this many queued ordinary calls
/// before one ordinary call must be delivered. This mirrors the scheduler's
/// two-System/one-User dispatch contract.
const MAX_CONSECUTIVE_PRIORITY_ENDPOINT_CALLS: u8 = 2;

/// Trusted queue class sampled from the kernel scheduler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointCallPriority {
    Ordinary,
    System,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EndpointQueueLane {
    Ordinary,
    System,
}

#[derive(Default)]
pub(super) struct EndpointObject {
    pub(super) owner: Option<EndpointOwner>,
    pub(super) pending_messages: VecDeque<u64>,
    pub(super) pending_system_messages: VecDeque<u64>,
    consecutive_system_deliveries: u8,
    pub(super) waiting_receivers: VecDeque<u64>,
}

impl EndpointObject {
    pub(super) fn new(
        owner: Option<EndpointOwner>,
        pending_messages: VecDeque<u64>,
        pending_system_messages: VecDeque<u64>,
        waiting_receivers: VecDeque<u64>,
    ) -> Self {
        Self {
            owner,
            pending_messages,
            pending_system_messages,
            consecutive_system_deliveries: 0,
            waiting_receivers,
        }
    }

    pub(super) fn pending_len(&self) -> usize {
        self.pending_messages
            .len()
            .saturating_add(self.pending_system_messages.len())
    }

    pub(super) fn has_pending(&self) -> bool {
        self.pending_len() != 0
    }

    pub(super) fn push_pending(&mut self, priority: EndpointCallPriority, message_id: u64) {
        match priority {
            EndpointCallPriority::Ordinary => self.pending_messages.push_back(message_id),
            EndpointCallPriority::System => self.pending_system_messages.push_back(message_id),
        }
    }

    pub(super) fn next_pending(&self) -> Option<(EndpointQueueLane, u64)> {
        let deliver_system = !self.pending_system_messages.is_empty()
            && (self.pending_messages.is_empty()
                || self.consecutive_system_deliveries
                    < MAX_CONSECUTIVE_PRIORITY_ENDPOINT_CALLS);
        if deliver_system {
            self.pending_system_messages
                .front()
                .copied()
                .map(|message_id| (EndpointQueueLane::System, message_id))
        } else {
            self.pending_messages
                .front()
                .copied()
                .map(|message_id| (EndpointQueueLane::Ordinary, message_id))
        }
    }

    pub(super) fn consume_pending(&mut self, lane: EndpointQueueLane, message_id: u64) {
        let removed = match lane {
            EndpointQueueLane::Ordinary => {
                self.consecutive_system_deliveries = 0;
                self.pending_messages.pop_front()
            }
            EndpointQueueLane::System => {
                self.consecutive_system_deliveries = self
                    .consecutive_system_deliveries
                    .saturating_add(1)
                    .min(MAX_CONSECUTIVE_PRIORITY_ENDPOINT_CALLS);
                self.pending_system_messages.pop_front()
            }
        };
        assert_eq!(
            removed,
            Some(message_id),
            "endpoint priority lane changed before committed receive"
        );
    }
}
