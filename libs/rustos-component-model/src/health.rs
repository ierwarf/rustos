//! Bounded, generation-exact liveness and health checks.
//!
//! Health checks never spawn, suspend or kill a process themselves. They
//! produce a supervisor decision for rootd's existing validated lease owner.
//! Clock input must be monotonic and from a trusted scheduler time source.

use alloc::vec::Vec;
use crate::identity::ComponentId;
use crate::realm::{ComponentState, Graph};

pub const MAX_HEALTH_WATCHES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthError {
    Invalid,
    Missing,
    Duplicate,
    Stale,
    Full,
    OutOfMemory,
    BackwardsTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthPolicy {
    pub interval_ticks: u64,
    pub timeout_ticks: u64,
    pub allowed_misses: u32,
}

impl HealthPolicy {
    pub fn validate(self) -> Result<Self, HealthError> {
        if self.interval_ticks == 0 || self.timeout_ticks == 0
            || self.allowed_misses == 0 || self.timeout_ticks > self.interval_ticks {
            return Err(HealthError::Invalid);
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    pub component: ComponentId,
    pub incarnation: u64,
    pub nonce: u64,
    pub sent_at: u64,
    pub deadline: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthDecision {
    Healthy,
    Probe(Probe),
    AwaitingReply,
    Missed { count: u32 },
    Fail { count: u32 },
}

struct Watch {
    component: ComponentId,
    incarnation: u64,
    policy: HealthPolicy,
    last_tick: u64,
    nonce: u64,
    outstanding: Option<Probe>,
    misses: u32,
}

pub struct Health {
    watches: Vec<Watch>,
    capacity: usize,
}

impl Health {
    pub fn new(capacity: usize) -> Result<Self, HealthError> {
        if capacity == 0 || capacity > MAX_HEALTH_WATCHES {
            return Err(HealthError::Invalid);
        }
        Ok(Self { watches: Vec::new(), capacity })
    }

    pub fn watch(&mut self, graph: &Graph, component: ComponentId, policy: HealthPolicy, now: u64)
        -> Result<(), HealthError>
    {
        policy.validate()?;
        if self.watches.len() >= self.capacity { return Err(HealthError::Full); }
        if self.watches.iter().any(|w| w.component == component) {
            return Err(HealthError::Duplicate);
        }
        let view = graph.inspect(component).map_err(|_| HealthError::Missing)?;
        if view.state != ComponentState::Running { return Err(HealthError::Invalid); }
        self.watches.try_reserve(1).map_err(|_| HealthError::OutOfMemory)?;
        self.watches.push(Watch {
            component, incarnation: view.incarnation, policy,
            last_tick: now, nonce: 0, outstanding: None, misses: 0,
        });
        Ok(())
    }

    pub fn poll(&mut self, graph: &Graph, component: ComponentId, now: u64)
        -> Result<HealthDecision, HealthError>
    {
        let view = graph.inspect(component).map_err(|_| HealthError::Missing)?;
        let w = self.watches.iter_mut().find(|w| w.component == component)
            .ok_or(HealthError::Missing)?;
        if view.incarnation != w.incarnation || view.state != ComponentState::Running {
            return Err(HealthError::Stale);
        }
        if now < w.last_tick { return Err(HealthError::BackwardsTime); }
        if let Some(probe) = w.outstanding {
            if now < probe.deadline { return Ok(HealthDecision::AwaitingReply); }
            w.outstanding = None;
            w.last_tick = now;
            w.misses = w.misses.saturating_add(1);
            return Ok(if w.misses >= w.policy.allowed_misses {
                HealthDecision::Fail { count: w.misses }
            } else {
                HealthDecision::Missed { count: w.misses }
            });
        }
        if now - w.last_tick < w.policy.interval_ticks {
            return Ok(HealthDecision::Healthy);
        }
        let nonce = w.nonce.checked_add(1).ok_or(HealthError::Invalid)?;
        let deadline = now.checked_add(w.policy.timeout_ticks)
            .ok_or(HealthError::Invalid)?;
        let probe = Probe { component, incarnation: view.incarnation,
            nonce, sent_at: now, deadline };
        w.nonce = nonce;
        w.outstanding = Some(probe);
        w.last_tick = now;
        Ok(HealthDecision::Probe(probe))
    }

    pub fn acknowledge(&mut self, graph: &Graph, probe: Probe, now: u64)
        -> Result<(), HealthError>
    {
        let view = graph.inspect(probe.component).map_err(|_| HealthError::Missing)?;
        let w = self.watches.iter_mut().find(|w| w.component == probe.component)
            .ok_or(HealthError::Missing)?;
        if view.incarnation != w.incarnation || view.state != ComponentState::Running
            || probe.incarnation != w.incarnation
            || w.outstanding != Some(probe) || now < probe.sent_at
            || now >= probe.deadline {
            return Err(HealthError::Stale);
        }
        w.outstanding = None;
        w.last_tick = now;
        w.misses = 0;
        Ok(())
    }

    pub fn unwatch(&mut self, component: ComponentId) -> bool {
        if let Some(i) = self.watches.iter().position(|w| w.component == component) {
            self.watches.swap_remove(i);
            true
        } else { false }
    }

    pub fn missed(&self, component: ComponentId) -> Option<u32> {
        self.watches.iter().find(|w| w.component == component).map(|w| w.misses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JobCeiling, Moniker, Quota, RestartPolicy};

    #[test]
    fn authenticated_incarnation_and_nonce_required() {
        let mut graph = Graph::new(Moniker::new("root").unwrap(), 1, 4).unwrap();
        let id = graph.create(graph.root(), Moniker::new("server").unwrap(), 2,
            JobCeiling::ALL, Quota::UNLIMITED, RestartPolicy::NEVER, false).unwrap();
        graph.activate(id).unwrap();
        let mut h = Health::new(4).unwrap();
        h.watch(&graph, id, HealthPolicy { interval_ticks: 10, timeout_ticks: 5, allowed_misses: 2 }, 0).unwrap();
        let p = match h.poll(&graph, id, 10).unwrap() { HealthDecision::Probe(p) => p, _ => panic!("no probe") };
        assert_eq!(h.poll(&graph, id, 13), Ok(HealthDecision::AwaitingReply));
        assert_eq!(h.acknowledge(&graph, Probe { nonce: p.nonce + 1, ..p }, 13), Err(HealthError::Stale));
        h.acknowledge(&graph, p, 13).unwrap();
        assert_eq!(h.poll(&graph, id, 14), Ok(HealthDecision::Healthy));
        assert_eq!(h.poll(&graph, id, 23).unwrap(), HealthDecision::Probe(Probe {
            component: id, incarnation: 1, nonce: 2, sent_at: 23, deadline: 28,
        }));
        assert_eq!(h.poll(&graph, id, 28), Ok(HealthDecision::Missed { count: 1 }));
        assert_eq!(h.poll(&graph, id, 38).unwrap(), HealthDecision::Probe(Probe {
            component: id, incarnation: 1, nonce: 3, sent_at: 38, deadline: 43,
        }));
        assert_eq!(h.poll(&graph, id, 43), Ok(HealthDecision::Fail { count: 2 }));
    }
}
