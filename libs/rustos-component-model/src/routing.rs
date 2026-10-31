//! Explicit component capability publication and directed offers.
//!
//! Resolving a name never manufactures a handle. The broker must transfer a
//! separately authenticated kernel descriptor with no more than these rights.
//! This ledger denies transitive privilege by default: one hop is explicit,
//! and one instance's route never applies to a recycled slot.

use alloc::vec::Vec;

use crate::identity::{ComponentId, Moniker};
use crate::realm::{ComponentState, Graph, GraphError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityError {
    Missing,
    Duplicated,
    Denied,
    OwnerNotRunning,
    OutOfMemory,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    pub owner: ComponentId,
    pub name: Moniker,
    pub rights: u32,
    pub owner_incarnation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapabilityRoute {
    pub provider: ComponentId,
    pub consumer: ComponentId,
    pub name: Moniker,
    pub rights: u32,
    pub provider_incarnation: u64,
    pub consumer_incarnation: u64,
}

impl Graph {
    /// Publish an endpoint claim *after* the kernel has sealed the
    /// corresponding exact-lease owner. Names alone carry no authority.
    pub fn publish(
        &mut self,
        owner: ComponentId,
        name: Moniker,
        rights: u32,
    ) -> Result<(), CapabilityError> {
        let c = self
            .component(owner)
            .map_err(|_| CapabilityError::Missing)?;
        if c.state != ComponentState::Running {
            return Err(CapabilityError::OwnerNotRunning);
        }
        if self.require_running_chain(owner).is_err() {
            return Err(CapabilityError::OwnerNotRunning);
        }
        if rights == 0 {
            return Err(CapabilityError::Denied);
        }
        let incarnation = c.incarnation;
        if self
            .exports
            .iter()
            .any(|p| p.owner == owner && p.name == name)
        {
            return Err(CapabilityError::Duplicated);
        }
        if self.exports.len() >= self.max_routes {
            return Err(CapabilityError::Full);
        }
        self.exports
            .try_reserve(1)
            .map_err(|_| CapabilityError::OutOfMemory)?;
        self.exports.push(Capability {
            owner,
            name,
            rights,
            owner_incarnation: incarnation,
        });
        Ok(())
    }

    pub fn offer(
        &mut self,
        provider: ComponentId,
        consumer: ComponentId,
        name: Moniker,
        rights: u32,
    ) -> Result<(), CapabilityError> {
        let from = self
            .component(provider)
            .map_err(|_| CapabilityError::Missing)?;
        let to = self
            .component(consumer)
            .map_err(|_| CapabilityError::Missing)?;
        if from.state != ComponentState::Running || to.state != ComponentState::Running {
            return Err(CapabilityError::OwnerNotRunning);
        }
        if self.require_running_chain(provider).is_err()
            || self.require_running_chain(consumer).is_err()
        {
            return Err(CapabilityError::OwnerNotRunning);
        }
        let from_incarnation = from.incarnation;
        let to_incarnation = to.incarnation;
        if provider == consumer || rights == 0 {
            return Err(CapabilityError::Denied);
        }
        let source = self
            .exports
            .iter()
            .find(|p| {
                p.owner == provider && p.name == name && p.owner_incarnation == from_incarnation
            })
            .ok_or(CapabilityError::Missing)?;
        if (rights & !source.rights) != 0 {
            return Err(CapabilityError::Denied);
        }
        if self
            .routes
            .iter()
            .any(|r| r.provider == provider && r.consumer == consumer && r.name == name)
        {
            return Err(CapabilityError::Duplicated);
        }
        if self.routes.len() >= self.max_routes {
            return Err(CapabilityError::Full);
        }
        self.routes
            .try_reserve(1)
            .map_err(|_| CapabilityError::OutOfMemory)?;
        self.routes.push(CapabilityRoute {
            provider,
            consumer,
            name,
            rights,
            provider_incarnation: from_incarnation,
            consumer_incarnation: to_incarnation,
        });
        Ok(())
    }

    /// Resolve only a caller-specific offer, and revalidate both generations
    /// and incarnation numbers. This is a routing *decision*, not an fd.
    pub fn resolve(
        &self,
        consumer: ComponentId,
        provider: ComponentId,
        name: Moniker,
    ) -> Result<CapabilityRoute, CapabilityError> {
        let from = self
            .component(provider)
            .map_err(|_| CapabilityError::Missing)?;
        let to = self
            .component(consumer)
            .map_err(|_| CapabilityError::Missing)?;
        if from.state != ComponentState::Running || to.state != ComponentState::Running {
            return Err(CapabilityError::OwnerNotRunning);
        }
        if self.require_running_chain(provider).is_err()
            || self.require_running_chain(consumer).is_err()
        {
            return Err(CapabilityError::OwnerNotRunning);
        }
        let route = self
            .routes
            .iter()
            .find(|r| r.provider == provider && r.consumer == consumer && r.name == name)
            .copied()
            .ok_or(CapabilityError::Denied)?;
        if route.provider_incarnation != from.incarnation
            || route.consumer_incarnation != to.incarnation
        {
            return Err(CapabilityError::Denied);
        }
        let export = self
            .exports
            .iter()
            .find(|e| {
                e.owner == provider && e.name == name && e.owner_incarnation == from.incarnation
            })
            .ok_or(CapabilityError::Denied)?;
        if route.rights == 0 || (route.rights & !export.rights) != 0 {
            return Err(CapabilityError::Denied);
        }
        Ok(route)
    }

    /// Revoke all old routes when a provider restarts. No implicit replay or
    /// name-only recovery is allowed for a new service incarnation.
    pub fn revoke_instance(&mut self, id: ComponentId) {
        self.exports.retain(|e| e.owner != id);
        self.routes.retain(|r| r.provider != id && r.consumer != id);
        // Both producer and consumer routes are withdrawn on an incarnation
        // change. A restarted process never inherits a stale offer.
    }

    /// Only the route's exact consumer can be removed. The supervisor must
    /// authenticate that caller before reaching this pure state machine.
    pub fn withdraw_offer(
        &mut self,
        provider: ComponentId,
        consumer: ComponentId,
        name: Moniker,
    ) -> bool {
        if let Some(index) = self
            .routes
            .iter()
            .position(|r| r.provider == provider && r.consumer == consumer && r.name == name)
        {
            self.routes.swap_remove(index);
            true
        } else {
            false
        }
    }

    pub fn route_count(&self) -> usize {
        self.routes.len()
    }
    pub fn export_count(&self) -> usize {
        self.exports.len()
    }

    /// Produces a bounded snapshot for diagnostics; does not grant access.
    pub fn routes_for(&self, consumer: ComponentId) -> Result<Vec<CapabilityRoute>, GraphError> {
        self.component(consumer)?;
        let size = self
            .routes
            .iter()
            .filter(|r| r.consumer == consumer)
            .count();
        let mut out = Vec::new();
        out.try_reserve(size).map_err(|_| GraphError::OutOfMemory)?;
        out.extend(
            self.routes
                .iter()
                .copied()
                .filter(|r| r.consumer == consumer),
        );
        Ok(out)
    }
}
