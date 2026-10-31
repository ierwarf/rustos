//! Bounded non-blocking *in-memory* policy telemetry.
//!
//! A full telemetry queue drops the oldest record and increments a separate
//! counter; observability never changes an authorization result. This is not
//! persistent crash storage or a replacement for RustOS's existing logging.

use alloc::collections::VecDeque;

use crate::identity::ComponentId;

pub const MAX_EVENTS: usize = 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Prepared,
    Started,
    Stopped,
    Exit,
    RestartScheduled,
    RestartExhausted,
    PolicyDenied,
    QuotaDenied,
    RoutePublished,
    RouteRevoked,
    HealthCheckFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub component: ComponentId,
    pub incarnation: u64,
    pub at_tick: u64,
    pub kind: EventKind,
    pub detail: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelemetryError {
    InvalidCapacity,
    OutOfMemory,
}

pub struct Telemetry {
    events: VecDeque<Event>,
    max: usize,
    lost: u64,
}

impl Telemetry {
    pub fn new(max: usize) -> Result<Self, TelemetryError> {
        if max == 0 || max > MAX_EVENTS {
            return Err(TelemetryError::InvalidCapacity);
        }
        Ok(Self {
            events: VecDeque::new(),
            max,
            lost: 0,
        })
    }

    pub fn record(&mut self, event: Event) -> Result<(), TelemetryError> {
        // Reserve before evicting. Allocation errors must not silently lose
        // existing records or misreport a successfully delivered event.
        if self.events.len() < self.max {
            self.events
                .try_reserve(1)
                .map_err(|_| TelemetryError::OutOfMemory)?;
        } else {
            self.events.pop_front();
            self.lost = self.lost.saturating_add(1);
        }
        self.events.push_back(event);
        Ok(())
    }

    pub fn lost(&self) -> u64 {
        self.lost
    }
    pub fn len(&self) -> usize {
        self.events.len()
    }
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
    pub fn pop(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
    pub fn peek(&self) -> Option<Event> {
        self.events.front().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::ComponentId;

    #[test]
    fn overflow_keeps_newest_event_and_explicit_loss() {
        let mut q = Telemetry::new(2).unwrap();
        let component = ComponentId::new(1, 4).unwrap();
        for at_tick in 0..4 {
            q.record(Event {
                component,
                incarnation: 1,
                at_tick,
                kind: EventKind::Exit,
                detail: 0,
            })
            .unwrap();
        }
        assert_eq!(q.len(), 2);
        assert_eq!(q.lost(), 2);
        assert_eq!(q.pop().unwrap().at_tick, 2);
        assert_eq!(q.pop().unwrap().at_tick, 3);
        assert!(q.is_empty());
    }
}
