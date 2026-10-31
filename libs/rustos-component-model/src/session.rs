//! Client-funded user-space sessions, with exact generation and incarnation.
//!
//! A service's policy cost is attributed to the initiating client. This does
//! not charge kernel memory or mint real Job descriptors. The kernel's existing
//! resource mechanism remains authoritative, and a caller must authenticate
//! the IPC sender before invoking this policy layer.

use crate::identity::{ComponentId, Moniker};
use crate::quota::Charge;
use crate::realm::{ChargeTicket, Graph, GraphError};
use crate::routing::CapabilityError;
use alloc::vec::Vec;

pub const MAX_SESSIONS: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionError {
    Denied,
    Missing,
    Stale,
    Busy,
    Full,
    Overflow,
    Allocation,
    Invalid,
    Charge(GraphError),
}

impl From<GraphError> for SessionError {
    fn from(err: GraphError) -> Self {
        Self::Charge(err)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionId {
    pub slot: u32,
    pub generation: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionView {
    pub id: SessionId,
    pub client: ComponentId,
    pub provider: ComponentId,
    pub capability: Moniker,
    pub opened_at: u64,
    pub expires_at: u64,
    pub client_incarnation: u64,
    pub provider_incarnation: u64,
}

#[derive(Clone, Copy, Debug)]
struct Session {
    view: SessionView,
    charge: ChargeTicket,
    /// The route must still be the *same issuance*, not merely the same name.
    route_epoch: u64,
}

struct Slot {
    generation: u32,
    item: Option<Session>,
}

pub struct Sessions {
    slots: Vec<Slot>,
    capacity: usize,
    // Rotating bounded cleanup cursor: repeatedly expiring one early slot
    // cannot starve an older session at the end of the table.
    cleanup_cursor: usize,
}

impl Sessions {
    pub fn new(capacity: usize) -> Result<Self, SessionError> {
        if capacity == 0 || capacity > MAX_SESSIONS {
            return Err(SessionError::Invalid);
        }
        Ok(Self {
            slots: Vec::new(),
            capacity,
            cleanup_cursor: 0,
        })
    }

    pub fn open(
        &mut self,
        graph: &mut Graph,
        client: ComponentId,
        provider: ComponentId,
        capability: Moniker,
        cost: Charge,
        now: u64,
        expires_at: u64,
    ) -> Result<SessionId, SessionError> {
        // Every admitted session consumes exactly one of the client's
        // session slots; a zero-cost or double-counted session is invalid.
        if client == provider || expires_at <= now || cost.sessions != 1 {
            return Err(SessionError::Invalid);
        }
        let route = graph
            .resolve(client, provider, capability)
            .map_err(|e| match e {
                CapabilityError::Missing => SessionError::Missing,
                _ => SessionError::Denied,
            })?;
        if route.rights == 0 {
            return Err(SessionError::Denied);
        }
        let client_incarnation = graph.inspect(client)?.incarnation;
        let provider_incarnation = graph.inspect(provider)?.incarnation;
        // Exhausted generations are retired, not candidates for reuse.
        let vacant = self
            .slots
            .iter()
            .position(|s| s.item.is_none() && s.generation != u32::MAX);
        if vacant.is_none() && self.slots.len() >= self.capacity {
            return Err(SessionError::Full);
        }
        if vacant.is_none() {
            self.slots
                .try_reserve(1)
                .map_err(|_| SessionError::Allocation)?;
        }
        let slot_index = vacant.unwrap_or(self.slots.len());
        let generation = vacant.map_or(1, |index| self.slots[index].generation);
        let ticket = graph.charge(client, cost)?;
        let id = SessionId {
            slot: slot_index as u32,
            generation,
        };
        let item = Session {
            charge: ticket,
            route_epoch: route.epoch,
            view: SessionView {
                id,
                client,
                provider,
                capability,
                opened_at: now,
                expires_at,
                client_incarnation,
                provider_incarnation,
            },
        };
        if vacant.is_some() {
            self.slots[slot_index].item = Some(item);
        } else {
            self.slots.push(Slot {
                generation,
                item: Some(item),
            });
        }
        Ok(id)
    }

    fn session(&self, id: SessionId) -> Result<&Session, SessionError> {
        let s = self
            .slots
            .get(id.slot as usize)
            .ok_or(SessionError::Missing)?;
        if s.generation != id.generation {
            return Err(SessionError::Stale);
        }
        s.item.as_ref().ok_or(SessionError::Missing)
    }

    pub fn inspect(
        &self,
        graph: &Graph,
        id: SessionId,
        now: u64,
    ) -> Result<SessionView, SessionError> {
        let view = self.session(id)?.view;
        if now >= view.expires_at {
            return Err(SessionError::Stale);
        }
        let client = graph.inspect(view.client)?;
        let provider = graph.inspect(view.provider)?;
        if client.incarnation != view.client_incarnation
            || provider.incarnation != view.provider_incarnation
        {
            return Err(SessionError::Stale);
        }
        let route = graph
            .resolve(view.client, view.provider, view.capability)
            .map_err(|_| SessionError::Denied)?;
        if route.epoch != self.session(id)?.route_epoch {
            return Err(SessionError::Stale);
        }
        Ok(view)
    }

    /// Release exact cost before consuming the session slot.
    pub fn close(&mut self, graph: &mut Graph, id: SessionId) -> Result<(), SessionError> {
        let ticket = self.session(id)?.charge;
        let index = id.slot as usize;
        // Refund exactly once even when the external generation has reached
        // its final value. That slot stays permanently retired; wrapping it
        // would revalidate an ancient SessionId (ABA).
        graph.release(ticket)?;
        self.slots[index].item = None;
        self.slots[index].generation = self.slots[index].generation.saturating_add(1);
        Ok(())
    }

    /// Expire a bounded number of sessions while rotating the scan start.
    /// Each call walks at most the populated slot directory once; a finite
    /// budget cannot repeatedly favor low indexes and starve later entries.
    pub fn expire(
        &mut self,
        graph: &mut Graph,
        now: u64,
        budget: usize,
    ) -> Result<usize, SessionError> {
        if budget == 0 || self.slots.is_empty() {
            return Ok(0);
        }
        let len = self.slots.len();
        let mut index = self.cleanup_cursor % len;
        let mut scanned = 0;
        let mut closed = 0;
        while scanned < len && closed < budget {
            if let Some(session) = self.slots[index].item {
                let view = session.view;
                let client = graph.inspect(view.client).ok();
                let provider = graph.inspect(view.provider).ok();
                let route_invalid = graph
                    .resolve(view.client, view.provider, view.capability)
                    .map_or(true, |route| route.epoch != session.route_epoch);
                if now >= view.expires_at
                    || route_invalid
                    || client
                        .as_ref()
                        .map_or(true, |c| c.incarnation != view.client_incarnation)
                    || provider
                        .as_ref()
                        .map_or(true, |c| c.incarnation != view.provider_incarnation)
                {
                    self.close(graph, view.id)?;
                    closed += 1;
                }
            }
            index = if index + 1 == len { 0 } else { index + 1 };
            scanned += 1;
        }
        self.cleanup_cursor = index;
        Ok(closed)
    }

    pub fn revoke_component(
        &mut self,
        graph: &mut Graph,
        id: ComponentId,
        budget: usize,
    ) -> Result<usize, SessionError> {
        if budget == 0 || self.slots.is_empty() {
            return Ok(0);
        }
        let len = self.slots.len();
        let mut index = self.cleanup_cursor % len;
        let mut scanned = 0;
        let mut closed = 0;
        while scanned < len && closed < budget {
            if let Some(session) = self.slots[index].item {
                if session.view.client == id || session.view.provider == id {
                    self.close(graph, session.view.id)?;
                    closed += 1;
                }
            }
            index = if index + 1 == len { 0 } else { index + 1 };
            scanned += 1;
        }
        self.cleanup_cursor = index;
        Ok(closed)
    }

    pub fn live_count(&self) -> usize {
        self.slots.iter().filter(|s| s.item.is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JobCeiling, Quota, RestartPolicy};

    #[test]
    fn sessions_charge_client_not_server_and_single_close() {
        let mut g = Graph::new(Moniker::new("root").unwrap(), 1, 6).unwrap();
        let mut add = |name: &str, job| {
            let id = g
                .create(
                    g.root(),
                    Moniker::new(name).unwrap(),
                    job,
                    JobCeiling::ALL,
                    Quota::UNLIMITED,
                    RestartPolicy::NEVER,
                    false,
                )
                .unwrap();
            g.activate(id).unwrap();
            id
        };
        let c = add("client", 2);
        let p = add("provider", 3);
        let name = Moniker::new("storage").unwrap();
        g.publish(p, name, 1).unwrap();
        g.offer(p, c, name, 1).unwrap();
        let mut s = Sessions::new(2).unwrap();
        let id = s
            .open(
                &mut g,
                c,
                p,
                name,
                Charge {
                    sessions: 1,
                    kernel_bytes: 42,
                    ..Charge::ZERO
                },
                100,
                200,
            )
            .unwrap();
        assert_eq!(g.inspect(c).unwrap().used.kernel_bytes, 42);
        assert_eq!(g.inspect(p).unwrap().used.kernel_bytes, 0);
        assert!(s.inspect(&g, id, 100).is_ok());
        assert!(s.inspect(&g, id, 200).is_err());
        s.close(&mut g, id).unwrap();
        assert_eq!(g.inspect(c).unwrap().used.kernel_bytes, 0);
        assert_eq!(s.close(&mut g, id), Err(SessionError::Stale));
    }

    #[test]
    fn revoked_offer_never_revalidates_old_client_session_after_reissue() {
        let mut g = Graph::new(Moniker::new("root").unwrap(), 1, 4).unwrap();
        let client = g
            .create(
                g.root(),
                Moniker::new("client").unwrap(),
                2,
                JobCeiling::ALL,
                Quota::UNLIMITED,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        let provider = g
            .create(
                g.root(),
                Moniker::new("server").unwrap(),
                3,
                JobCeiling::ALL,
                Quota::UNLIMITED,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        g.activate(client).unwrap();
        g.activate(provider).unwrap();
        let cap = Moniker::new("storage").unwrap();
        g.publish(provider, cap, 0b11).unwrap();
        g.offer(provider, client, cap, 0b01).unwrap();
        let original_epoch = g.resolve(client, provider, cap).unwrap().epoch;
        let mut sessions = Sessions::new(4).unwrap();
        let charge = Charge {
            sessions: 1,
            ..Charge::ZERO
        };
        let first = sessions
            .open(&mut g, client, provider, cap, charge, 10, 200)
            .unwrap();
        assert!(sessions.inspect(&g, first, 11).is_ok());

        assert!(g.withdraw_offer(provider, client, cap));
        assert_eq!(sessions.inspect(&g, first, 12), Err(SessionError::Denied));
        g.offer(provider, client, cap, 0b01).unwrap();
        assert_ne!(
            original_epoch,
            g.resolve(client, provider, cap).unwrap().epoch
        );
        assert_eq!(sessions.inspect(&g, first, 13), Err(SessionError::Stale));

        // Expiry scavenging also revokes the old charge without waiting for
        // its TTL, but preserves the newly issued provider offer.
        assert_eq!(sessions.expire(&mut g, 14, 1), Ok(1));
        assert_eq!(g.inspect(client).unwrap().used.sessions, 0);
        assert_eq!(sessions.close(&mut g, first), Err(SessionError::Stale));
        let second = sessions
            .open(&mut g, client, provider, cap, charge, 15, 200)
            .unwrap();
        assert_ne!(first.generation, second.generation);
        assert!(sessions.inspect(&g, second, 15).is_ok());
    }

    #[test]
    fn session_admission_requires_one_client_session_unit() {
        let mut g = Graph::new(Moniker::new("root").unwrap(), 1, 4).unwrap();
        let client = g
            .create(
                g.root(),
                Moniker::new("client").unwrap(),
                2,
                JobCeiling::ALL,
                Quota::UNLIMITED,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        let provider = g
            .create(
                g.root(),
                Moniker::new("server").unwrap(),
                3,
                JobCeiling::ALL,
                Quota::UNLIMITED,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        g.activate(client).unwrap();
        g.activate(provider).unwrap();
        let name = Moniker::new("cap").unwrap();
        g.publish(provider, name, 1).unwrap();
        g.offer(provider, client, name, 1).unwrap();
        let mut sessions = Sessions::new(4).unwrap();
        assert_eq!(
            sessions.open(&mut g, client, provider, name, Charge::ZERO, 1, 30),
            Err(SessionError::Invalid)
        );
        assert_eq!(
            sessions.open(
                &mut g,
                client,
                provider,
                name,
                Charge {
                    sessions: 2,
                    ..Charge::ZERO
                },
                1,
                30
            ),
            Err(SessionError::Invalid)
        );
        assert_eq!(sessions.live_count(), 0);
        assert_eq!(g.inspect(client).unwrap().used.sessions, 0);
    }

    #[test]
    fn exhausted_generation_refunds_and_retires_slot_without_aba() {
        let mut g = Graph::new(Moniker::new("root").unwrap(), 1, 4).unwrap();
        let client = g
            .create(
                g.root(),
                Moniker::new("client").unwrap(),
                2,
                JobCeiling::ALL,
                Quota::UNLIMITED,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        let provider = g
            .create(
                g.root(),
                Moniker::new("provider").unwrap(),
                3,
                JobCeiling::ALL,
                Quota::UNLIMITED,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        g.activate(client).unwrap();
        g.activate(provider).unwrap();
        let cap = Moniker::new("fs").unwrap();
        g.publish(provider, cap, 1).unwrap();
        g.offer(provider, client, cap, 1).unwrap();
        let cost = Charge {
            sessions: 1,
            ..Charge::ZERO
        };
        let mut sessions = Sessions::new(2).unwrap();
        let id = sessions
            .open(&mut g, client, provider, cap, cost, 1, 100)
            .unwrap();
        // Simulate the last allowed incarnation without billions of opens.
        sessions.slots[0].generation = u32::MAX;
        sessions.slots[0].item.as_mut().unwrap().view.id.generation = u32::MAX;
        let last_id = SessionId {
            slot: 0,
            generation: u32::MAX,
        };
        assert!(sessions.inspect(&g, last_id, 2).is_ok());
        sessions.close(&mut g, last_id).unwrap();
        assert_eq!(g.inspect(client).unwrap().used.sessions, 0);
        assert_eq!(sessions.live_count(), 0);
        assert_eq!(sessions.close(&mut g, last_id), Err(SessionError::Missing));
        let next = sessions
            .open(&mut g, client, provider, cap, cost, 3, 100)
            .unwrap();
        assert_eq!(next.slot, 1);
        assert_ne!(next, id);
        assert_eq!(sessions.slots[0].generation, u32::MAX);
        sessions.close(&mut g, next).unwrap();
        assert_eq!(g.inspect(client).unwrap().used.sessions, 0);
    }

    #[test]
    fn rotating_cleanup_cursor_reaches_late_slots_under_single_step_budget() {
        let mut g = Graph::new(Moniker::new("root").unwrap(), 1, 4).unwrap();
        let client = g
            .create(
                g.root(),
                Moniker::new("client").unwrap(),
                2,
                JobCeiling::ALL,
                Quota::UNLIMITED,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        let provider = g
            .create(
                g.root(),
                Moniker::new("provider").unwrap(),
                3,
                JobCeiling::ALL,
                Quota::UNLIMITED,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        g.activate(client).unwrap();
        g.activate(provider).unwrap();
        let cap = Moniker::new("fs").unwrap();
        g.publish(provider, cap, 1).unwrap();
        g.offer(provider, client, cap, 1).unwrap();
        let charge = Charge {
            sessions: 1,
            ..Charge::ZERO
        };
        let mut sessions = Sessions::new(4).unwrap();
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(
                sessions
                    .open(&mut g, client, provider, cap, charge, 1, 10)
                    .unwrap(),
            );
        }
        assert_eq!(sessions.expire(&mut g, 20, 1), Ok(1));
        assert_eq!(sessions.cleanup_cursor, 1);
        let fresh = sessions
            .open(&mut g, client, provider, cap, charge, 21, 100)
            .unwrap();
        assert_eq!(fresh.slot, 0);
        assert_eq!(sessions.expire(&mut g, 22, 1), Ok(1));
        assert_eq!(sessions.cleanup_cursor, 2);
        assert_eq!(sessions.expire(&mut g, 22, 1), Ok(1));
        assert_eq!(sessions.cleanup_cursor, 3);
        assert_eq!(sessions.expire(&mut g, 22, 1), Ok(1));
        assert_eq!(sessions.live_count(), 1);
        assert_eq!(g.inspect(client).unwrap().used.sessions, 1);
        assert!(sessions.inspect(&g, fresh, 22).is_ok());
        for stale in &ids {
            assert!(sessions.inspect(&g, *stale, 22).is_err());
        }
        assert_eq!(sessions.expire(&mut g, 22, 1), Ok(0));
        assert_eq!(sessions.revoke_component(&mut g, provider, 1), Ok(1));
        assert_eq!(sessions.live_count(), 0);
        assert_eq!(g.inspect(client).unwrap().used.sessions, 0);
    }

    #[test]
    fn expiration_is_bounded_and_cannot_double_refund() {
        let mut g = Graph::new(Moniker::new("root").unwrap(), 1, 4).unwrap();
        let client = g
            .create(
                g.root(),
                Moniker::new("a").unwrap(),
                2,
                JobCeiling::ALL,
                Quota::UNLIMITED,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        let provider = g
            .create(
                g.root(),
                Moniker::new("b").unwrap(),
                3,
                JobCeiling::ALL,
                Quota::UNLIMITED,
                RestartPolicy::NEVER,
                false,
            )
            .unwrap();
        g.activate(client).unwrap();
        g.activate(provider).unwrap();
        let cap = Moniker::new("fs").unwrap();
        g.publish(provider, cap, 1).unwrap();
        g.offer(provider, client, cap, 1).unwrap();
        let mut sessions = Sessions::new(3).unwrap();
        for expiry in [10, 11] {
            sessions
                .open(
                    &mut g,
                    client,
                    provider,
                    cap,
                    Charge {
                        sessions: 1,
                        ..Charge::ZERO
                    },
                    1,
                    expiry,
                )
                .unwrap();
        }
        assert_eq!(sessions.expire(&mut g, 20, 1), Ok(1));
        assert_eq!(g.inspect(client).unwrap().used.sessions, 1);
        assert_eq!(sessions.expire(&mut g, 20, 2), Ok(1));
        assert_eq!(sessions.expire(&mut g, 20, 2), Ok(0));
        assert_eq!(g.inspect(client).unwrap().used.sessions, 0);
    }
}
