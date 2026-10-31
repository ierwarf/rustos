//! Pure policy model for cgroup-style soft and hard memory pressure.
//!
//! These numbers are *observations* of the kernel Job memory ledger. They are
//! not an enforcement replacement for kernel frame admission, and do not
//! permit reclaim/kill without the independently authenticated Job handle.
//! In particular memory.min is protection preference, not an extra allocation.

use crate::identity::ComponentId;
use crate::realm::{ComponentState, Graph, GraphError};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressureError {
    InvalidLimit,
    UnknownComponent,
    Duplicate,
    OutOfMemory,
    Full,
    LiveUsageExceedsMax,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryBoundary {
    pub minimum_pages: u64,
    pub low_pages: u64,
    pub high_pages: u64,
    pub max_pages: u64,
}

impl MemoryBoundary {
    pub const UNBOUNDED: Self = Self {
        minimum_pages: 0,
        low_pages: 0,
        high_pages: u64::MAX,
        max_pages: u64::MAX,
    };

    pub fn validate(self) -> Result<Self, PressureError> {
        if self.minimum_pages > self.low_pages
            || self.low_pages > self.high_pages
            || self.high_pages > self.max_pages
        {
            Err(PressureError::InvalidLimit)
        } else {
            Ok(self)
        }
    }

    pub fn tightened_from(self, parent: Self) -> bool {
        self.minimum_pages <= parent.minimum_pages
            && self.low_pages <= parent.low_pages
            && self.high_pages <= parent.high_pages
            && self.max_pages <= parent.max_pages
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressureAction {
    Normal,
    ProtectedMinimum,
    ProtectedLow,
    SoftThrottle { excess_pages: u64 },
    HardRefuse { excess_pages: u64 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PressureEvents {
    pub high_crossings: u64,
    pub hard_refusals: u64,
    pub protected_skips: u64,
    pub last_pressure_tick: u64,
}

struct Entry {
    component: ComponentId,
    limits: MemoryBoundary,
    events: PressureEvents,
    last_action: PressureAction,
}

pub struct PressureIndex {
    entries: Vec<Entry>,
    capacity: usize,
}

impl PressureIndex {
    pub fn new(capacity: usize) -> Result<Self, PressureError> {
        if capacity == 0 || capacity > crate::realm::MAX_COMPONENTS {
            return Err(PressureError::InvalidLimit);
        }
        Ok(Self {
            entries: Vec::new(),
            capacity,
        })
    }

    pub fn install(
        &mut self,
        graph: &Graph,
        component: ComponentId,
        limits: MemoryBoundary,
    ) -> Result<(), PressureError> {
        limits.validate()?;
        let view = graph
            .inspect(component)
            .map_err(|_| PressureError::UnknownComponent)?;
        if view.state == ComponentState::Dead {
            return Err(PressureError::UnknownComponent);
        }
        if limits.max_pages > view.quota.maximum.user_pages {
            return Err(PressureError::InvalidLimit);
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.component == component)
        {
            return Err(PressureError::Duplicate);
        }
        if self.entries.len() >= self.capacity {
            return Err(PressureError::Full);
        }
        self.entries
            .try_reserve(1)
            .map_err(|_| PressureError::OutOfMemory)?;
        self.entries.push(Entry {
            component,
            limits,
            events: PressureEvents::default(),
            last_action: PressureAction::Normal,
        });
        Ok(())
    }

    pub fn tighten(
        &mut self,
        graph: &Graph,
        component: ComponentId,
        limits: MemoryBoundary,
    ) -> Result<(), PressureError> {
        limits.validate()?;
        let view = graph
            .inspect(component)
            .map_err(|_| PressureError::UnknownComponent)?;
        if limits.max_pages > view.quota.maximum.user_pages {
            return Err(PressureError::InvalidLimit);
        }
        if view.used.user_pages > limits.max_pages {
            return Err(PressureError::LiveUsageExceedsMax);
        }
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.component == component)
            .ok_or(PressureError::UnknownComponent)?;
        if !limits.tightened_from(entry.limits) {
            return Err(PressureError::InvalidLimit);
        }
        entry.limits = limits;
        entry.last_action = PressureAction::Normal;
        Ok(())
    }

    pub fn observe(
        &mut self,
        graph: &Graph,
        component: ComponentId,
        now: u64,
        global_pressure: bool,
    ) -> Result<PressureAction, PressureError> {
        let usage = graph
            .inspect(component)
            .map_err(|_| PressureError::UnknownComponent)?
            .used
            .user_pages;
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.component == component)
            .ok_or(PressureError::UnknownComponent)?;
        let bounds = entry.limits;
        // Hard limit is checked even when there is no global pressure.
        let action = if usage > bounds.max_pages {
            PressureAction::HardRefuse {
                excess_pages: usage - bounds.max_pages,
            }
        } else if !global_pressure {
            PressureAction::Normal
        } else if usage <= bounds.minimum_pages {
            PressureAction::ProtectedMinimum
        } else if usage <= bounds.low_pages {
            PressureAction::ProtectedLow
        } else if usage > bounds.high_pages {
            PressureAction::SoftThrottle {
                excess_pages: usage - bounds.high_pages,
            }
        } else {
            PressureAction::Normal
        };

        match action {
            PressureAction::HardRefuse { .. } => {
                if !matches!(entry.last_action, PressureAction::HardRefuse { .. }) {
                    entry.events.hard_refusals = entry.events.hard_refusals.saturating_add(1);
                }
            }
            PressureAction::SoftThrottle { .. } => {
                if !matches!(entry.last_action, PressureAction::SoftThrottle { .. }) {
                    entry.events.high_crossings = entry.events.high_crossings.saturating_add(1);
                }
            }
            PressureAction::ProtectedMinimum | PressureAction::ProtectedLow => {
                entry.events.protected_skips = entry.events.protected_skips.saturating_add(1);
            }
            PressureAction::Normal => {}
        }
        if action != PressureAction::Normal {
            entry.events.last_pressure_tick = now;
        }
        entry.last_action = action;
        Ok(action)
    }

    pub fn snapshot(&self, component: ComponentId) -> Option<(MemoryBoundary, PressureEvents)> {
        self.entries
            .iter()
            .find(|e| e.component == component)
            .map(|e| (e.limits, e.events))
    }

    pub fn remove(&mut self, component: ComponentId) -> bool {
        if let Some(i) = self.entries.iter().position(|e| e.component == component) {
            self.entries.swap_remove(i);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Charge, Graph, JobCeiling, Moniker, Quota, RestartPolicy};

    #[test]
    fn boundary_is_ordered_and_cannot_overrun_job_quota() {
        let mut g = Graph::new(Moniker::new("root").unwrap(), 1, 4).unwrap();
        let q = Quota {
            maximum: Charge {
                user_pages: 8,
                ..Quota::UNLIMITED.maximum
            },
        };
        let c = g
            .create(
                g.root(),
                Moniker::new("app").unwrap(),
                2,
                JobCeiling::ALL,
                q,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        g.activate(c).unwrap();
        let mut i = PressureIndex::new(2).unwrap();
        assert_eq!(
            i.install(
                &g,
                c,
                MemoryBoundary {
                    minimum_pages: 4,
                    low_pages: 2,
                    high_pages: 6,
                    max_pages: 8
                }
            ),
            Err(PressureError::InvalidLimit)
        );
        assert_eq!(
            i.install(
                &g,
                c,
                MemoryBoundary {
                    minimum_pages: 0,
                    low_pages: 0,
                    high_pages: 6,
                    max_pages: 10
                }
            ),
            Err(PressureError::InvalidLimit)
        );
        i.install(
            &g,
            c,
            MemoryBoundary {
                minimum_pages: 0,
                low_pages: 0,
                high_pages: 6,
                max_pages: 8,
            },
        )
        .unwrap();
        assert_eq!(
            i.observe(&g, c, 2, true),
            Ok(PressureAction::ProtectedMinimum)
        );
    }

    #[test]
    fn crossing_high_counts_only_edges_not_every_poll() {
        let mut g = Graph::new(Moniker::new("root").unwrap(), 1, 4).unwrap();
        let c = g
            .create(
                g.root(),
                Moniker::new("app").unwrap(),
                2,
                JobCeiling::ALL,
                Quota::UNLIMITED,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        g.activate(c).unwrap();
        let mut i = PressureIndex::new(2).unwrap();
        i.install(
            &g,
            c,
            MemoryBoundary {
                minimum_pages: 0,
                low_pages: 1,
                high_pages: 2,
                max_pages: 10,
            },
        )
        .unwrap();
        let t = g
            .charge(
                c,
                Charge {
                    user_pages: 4,
                    ..Charge::ZERO
                },
            )
            .unwrap();
        assert_eq!(
            i.observe(&g, c, 3, true),
            Ok(PressureAction::SoftThrottle { excess_pages: 2 })
        );
        assert_eq!(
            i.observe(&g, c, 4, true),
            Ok(PressureAction::SoftThrottle { excess_pages: 2 })
        );
        assert_eq!(i.snapshot(c).unwrap().1.high_crossings, 1);
        g.release(t).unwrap();
        assert_eq!(i.observe(&g, c, 5, false), Ok(PressureAction::Normal));
    }
}
