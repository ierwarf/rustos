//! Generation-exact userspace component tree and transactional charge ledger.
//!
//! Every mutation is performed by one rootd-owned policy worker. No syscall,
//! mutex or scheduler operation is performed under the graph's borrow.
//! A component's job number is descriptive, **not** a kernel Job handle.
//! Kernel operations must authenticate an independent typed descriptor.

use alloc::vec::Vec;

use crate::identity::{ComponentId, Moniker};
use crate::policy::{JobCeiling, Operation, PolicyError};
use crate::quota::{Charge, Quota, QuotaError};
use crate::supervision::RestartPolicy;

pub const MAX_COMPONENTS: usize = 4_096;
pub const MAX_ACTIVE_CHARGES: usize = 16_384;
pub const MAX_ROUTE_RECORDS: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentState {
    Prepared,
    Running,
    Stopping,
    Stopped,
    Dead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphError {
    Missing,
    Stale,
    Full,
    InvalidParent,
    DuplicateName,
    InvalidJob,
    InvalidLimit,
    PermissionDenied,
    Busy,
    OutOfMemory,
    GenerationExhausted,
    ChargeOverflow,
    ChargeRefused,
    InvalidTicket,
    TicketExhausted,
    InvalidState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartState {
    pub attempts: u32,
    pub window_started_at: u64,
    pub last_exit_at: u64,
}

impl RestartState {
    pub const fn new() -> Self {
        Self {
            attempts: 0,
            window_started_at: 0,
            last_exit_at: 0,
        }
    }
}

pub(crate) struct Component {
    pub(crate) parent: Option<ComponentId>,
    pub(crate) name: Moniker,
    pub(crate) kernel_job_id: u64,
    pub(crate) state: ComponentState,
    pub(crate) ceiling: JobCeiling,
    pub(crate) quota: Quota,
    pub(crate) used: Charge,
    pub(crate) restart_policy: RestartPolicy,
    pub(crate) restart: RestartState,
    pub(crate) incarnation: u64,
    pub(crate) critical: bool,
    pub(crate) next_restart_at: Option<u64>,
}

struct Slot {
    generation: u32,
    value: Option<Component>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComponentView {
    pub id: ComponentId,
    pub parent: Option<ComponentId>,
    pub name: Moniker,
    pub kernel_job_id: u64,
    pub state: ComponentState,
    pub ceiling: JobCeiling,
    pub quota: Quota,
    pub used: Charge,
    pub incarnation: u64,
    pub critical: bool,
    pub next_restart_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChargeTicket {
    pub(crate) serial: u64,
    pub(crate) owner: ComponentId,
}

impl ChargeTicket {
    pub const fn serial(self) -> u64 {
        self.serial
    }
    pub const fn owner(self) -> ComponentId {
        self.owner
    }
}

struct OutstandingCharge {
    ticket: ChargeTicket,
    cost: Charge,
}

pub struct Graph {
    slots: Vec<Slot>,
    pub(crate) routes: Vec<crate::routing::CapabilityRoute>,
    pub(crate) exports: Vec<crate::routing::Capability>,
    outstanding: Vec<OutstandingCharge>,
    next_ticket: u64,
    pub(crate) next_route_epoch: u64,
    capacity: usize,
    pub(crate) max_routes: usize,
}

impl Graph {
    pub fn new(root_name: Moniker, root_job_id: u64, capacity: usize) -> Result<Self, GraphError> {
        if capacity == 0 || capacity > MAX_COMPONENTS {
            return Err(GraphError::Full);
        }
        if root_job_id == 0 {
            return Err(GraphError::InvalidJob);
        }
        let mut slots = Vec::new();
        slots.try_reserve(1).map_err(|_| GraphError::OutOfMemory)?;
        slots.push(Slot {
            generation: 1,
            value: Some(Component {
                parent: None,
                name: root_name,
                kernel_job_id: root_job_id,
                state: ComponentState::Running,
                ceiling: JobCeiling::ALL,
                quota: Quota::UNLIMITED,
                used: Charge::ZERO,
                restart_policy: RestartPolicy::NEVER,
                restart: RestartState::new(),
                incarnation: 1,
                critical: true,
                next_restart_at: None,
            }),
        });
        Ok(Self {
            slots,
            routes: Vec::new(),
            exports: Vec::new(),
            outstanding: Vec::new(),
            next_ticket: 1,
            next_route_epoch: 1,
            capacity,
            max_routes: MAX_ROUTE_RECORDS,
        })
    }

    pub fn root(&self) -> ComponentId {
        ComponentId::new(0, 1).expect("root id")
    }

    pub fn live_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.value.is_some())
            .count()
    }

    pub(crate) fn component(&self, id: ComponentId) -> Result<&Component, GraphError> {
        let slot = self.slots.get(id.slot()).ok_or(GraphError::Missing)?;
        if slot.generation != id.generation() {
            return Err(GraphError::Stale);
        }
        slot.value.as_ref().ok_or(GraphError::Missing)
    }

    pub(crate) fn component_mut(&mut self, id: ComponentId) -> Result<&mut Component, GraphError> {
        let slot = self.slots.get_mut(id.slot()).ok_or(GraphError::Missing)?;
        if slot.generation != id.generation() {
            return Err(GraphError::Stale);
        }
        slot.value.as_mut().ok_or(GraphError::Missing)
    }

    pub fn inspect(&self, id: ComponentId) -> Result<ComponentView, GraphError> {
        let c = self.component(id)?;
        Ok(ComponentView {
            id,
            parent: c.parent,
            name: c.name,
            kernel_job_id: c.kernel_job_id,
            state: c.state,
            ceiling: c.ceiling,
            quota: c.quota,
            used: c.used,
            incarnation: c.incarnation,
            critical: c.critical,
            next_restart_at: c.next_restart_at,
        })
    }

    /// Every ancestor must remain Running before a policy, launch or route
    /// decision can be considered. A stopped realm immediately denies its
    /// descendants, even before asynchronous child retirement completes.
    pub(crate) fn require_running_chain(&self, id: ComponentId) -> Result<(), GraphError> {
        let mut current = Some(id);
        for _ in 0..self.slots.len() {
            let Some(node) = current else { return Ok(()) };
            let component = self.component(node)?;
            if component.state != ComponentState::Running {
                return Err(GraphError::InvalidState);
            }
            current = component.parent;
        }
        if current.is_none() {
            Ok(())
        } else {
            Err(GraphError::InvalidParent)
        }
    }

    pub fn is_within(&self, id: ComponentId, ancestor: ComponentId) -> bool {
        let mut current = Some(id);
        for _ in 0..self.slots.len() {
            let Some(node) = current else { return false };
            if node == ancestor {
                return self.component(node).is_ok();
            }
            current = match self.component(node) {
                Ok(item) => item.parent,
                Err(_) => return false,
            };
        }
        false
    }

    /// Monikers are unique among siblings; Job identities are validated by
    /// the *calling* rootd Job descriptor transaction before publication.
    pub fn create(
        &mut self,
        parent: ComponentId,
        name: Moniker,
        kernel_job_id: u64,
        ceiling: JobCeiling,
        quota: Quota,
        policy: RestartPolicy,
        critical: bool,
    ) -> Result<ComponentId, GraphError> {
        if kernel_job_id == 0 {
            return Err(GraphError::InvalidJob);
        }
        self.require_running_chain(parent)?;
        let parent_ref = self.component(parent)?;
        if parent_ref.state != ComponentState::Running {
            return Err(GraphError::InvalidParent);
        }
        if !ceiling.subset_of(parent_ref.ceiling)
            || parent_ref.quota.validate_tightening(quota).is_err()
        {
            return Err(GraphError::InvalidLimit);
        }
        for slot in &self.slots {
            if let Some(c) = &slot.value {
                if c.parent == Some(parent) && c.name == name && c.state != ComponentState::Dead {
                    return Err(GraphError::DuplicateName);
                }
            }
        }
        if self.slots.iter().any(|s| {
            s.value
                .as_ref()
                .map_or(false, |c| c.kernel_job_id == kernel_job_id)
        }) {
            return Err(GraphError::InvalidJob);
        }
        let vacant = self.slots.iter().position(|s| s.value.is_none());
        if vacant.is_none() && self.slots.len() >= self.capacity {
            return Err(GraphError::Full);
        }
        if vacant.is_none() {
            self.slots
                .try_reserve(1)
                .map_err(|_| GraphError::OutOfMemory)?;
        }
        let entry = Component {
            parent: Some(parent),
            name,
            kernel_job_id,
            state: ComponentState::Prepared,
            ceiling,
            quota,
            used: Charge::ZERO,
            restart_policy: policy,
            restart: RestartState::new(),
            incarnation: 1,
            critical,
            next_restart_at: None,
        };
        let index = match vacant {
            Some(index) => {
                self.slots[index].value = Some(entry);
                index
            }
            None => {
                let index = self.slots.len();
                self.slots.push(Slot {
                    generation: 1,
                    value: Some(entry),
                });
                index
            }
        };
        ComponentId::new(index as u32, self.slots[index].generation)
            .ok_or(GraphError::GenerationExhausted)
    }

    /// Two-phase activation: the kernel Job/endpoint admission must finish
    /// before rootd marks the user-policy component Running.
    pub fn activate(&mut self, id: ComponentId) -> Result<(), GraphError> {
        if let Some(parent) = self.component(id)?.parent {
            self.require_running_chain(parent)?;
        }
        let c = self.component_mut(id)?;
        if c.state != ComponentState::Prepared {
            return Err(GraphError::InvalidState);
        }
        c.state = ComponentState::Running;
        Ok(())
    }

    /// Restart is valid only after supervisor approval and the exact delay.
    /// The caller must first obtain and validate a *new* kernel Job identity.
    /// A crash does not allow a service to resurrect itself.
    pub fn restart(
        &mut self,
        id: ComponentId,
        new_kernel_job_id: u64,
        now: u64,
    ) -> Result<(), GraphError> {
        if new_kernel_job_id == 0 {
            return Err(GraphError::InvalidJob);
        }
        if let Some(parent) = self.component(id)?.parent {
            self.require_running_chain(parent)?;
        }
        if self.slots.iter().any(|slot| {
            slot.value
                .as_ref()
                .map_or(false, |c| c.kernel_job_id == new_kernel_job_id)
        }) {
            return Err(GraphError::InvalidJob);
        }
        let c = self.component_mut(id)?;
        if c.state != ComponentState::Stopped {
            return Err(GraphError::InvalidState);
        }
        let Some(deadline) = c.next_restart_at else {
            return Err(GraphError::PermissionDenied);
        };
        if now < deadline {
            return Err(GraphError::Busy);
        }
        let next_incarnation = c
            .incarnation
            .checked_add(1)
            .ok_or(GraphError::GenerationExhausted)?;
        c.kernel_job_id = new_kernel_job_id;
        c.incarnation = next_incarnation;
        c.next_restart_at = None;
        c.state = ComponentState::Running;
        Ok(())
    }

    pub fn effective_ceiling(&self, id: ComponentId) -> Result<JobCeiling, GraphError> {
        let mut current = Some(id);
        let mut result = JobCeiling::ALL;
        for _ in 0..self.slots.len() {
            let Some(node) = current else {
                return Ok(result);
            };
            let c = self.component(node)?;
            if c.state != ComponentState::Running {
                return Err(GraphError::InvalidState);
            }
            result = result.intersect(c.ceiling);
            current = c.parent;
        }
        if current.is_none() {
            Ok(result)
        } else {
            Err(GraphError::InvalidParent)
        }
    }

    pub fn permits(&self, id: ComponentId, op: Operation) -> bool {
        self.effective_ceiling(id)
            .map_or(false, |mask| mask.permits(op))
    }

    /// A policy update may only *tighten* the stored own ceiling.
    /// Revocation is observed immediately through ancestor traversal.
    pub fn narrow_ceiling(&mut self, id: ComponentId, next: JobCeiling) -> Result<(), GraphError> {
        let current = self.component(id)?.ceiling;
        current.narrow(next).map_err(|e| match e {
            PolicyError::OutOfRange | PolicyError::WouldWiden => GraphError::PermissionDenied,
        })?;
        self.component_mut(id)?.ceiling = next;
        Ok(())
    }

    pub fn tighten_quota(&mut self, id: ComponentId, next: Quota) -> Result<(), GraphError> {
        let c = self.component(id)?;
        c.quota
            .validate_tightening(next)
            .map_err(|_| GraphError::InvalidLimit)?;
        if !c.used.fits_in(next.maximum) {
            return Err(GraphError::Busy);
        }
        self.component_mut(id)?.quota = next;
        Ok(())
    }

    pub fn begin_stop(&mut self, id: ComponentId) -> Result<(), GraphError> {
        if id == self.root() {
            return Err(GraphError::PermissionDenied);
        }
        let state = self.component(id)?.state;
        if state == ComponentState::Dead {
            return Err(GraphError::InvalidState);
        }
        for index in 0..self.slots.len() {
            let Some(slot) = self.slots.get(index) else {
                continue;
            };
            if slot.value.is_none() {
                continue;
            }
            let Some(other) = ComponentId::new(index as u32, slot.generation) else {
                continue;
            };
            if self.is_within(other, id) {
                if let Some(c) = self.slots[index].value.as_mut() {
                    if c.state != ComponentState::Dead {
                        c.state = ComponentState::Stopping;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn finish_stop(&mut self, id: ComponentId) -> Result<(), GraphError> {
        let c = self.component_mut(id)?;
        if c.state != ComponentState::Stopping {
            return Err(GraphError::InvalidState);
        }
        c.state = ComponentState::Stopped;
        c.next_restart_at = None;
        Ok(())
    }

    /// Reap only a fully quiescent leaf. Recycled slots always change generation;
    /// a serial exhausted at u64::MAX fails closed rather than aliasing.
    pub fn reap(&mut self, id: ComponentId) -> Result<(), GraphError> {
        if id == self.root() {
            return Err(GraphError::PermissionDenied);
        }
        let c = self.component(id)?;
        if c.state != ComponentState::Stopped && c.state != ComponentState::Dead {
            return Err(GraphError::InvalidState);
        }
        if c.used != Charge::ZERO
            || self.outstanding.iter().any(|t| t.ticket.owner == id)
            || self.slots.iter().any(|s| {
                s.value
                    .as_ref()
                    .map_or(false, |child| child.parent == Some(id))
            })
        {
            return Err(GraphError::Busy);
        }
        let index = id.slot();
        let next = self.slots[index]
            .generation
            .checked_add(1)
            .ok_or(GraphError::GenerationExhausted)?;
        self.slots[index].value = None;
        self.slots[index].generation = next;
        self.routes.retain(|r| r.provider != id && r.consumer != id);
        self.exports.retain(|e| e.owner != id);
        Ok(())
    }

    /// Reserve before writing usage, and allocate ticket storage before any
    /// mutation. Every ancestor's quota is checked using checked arithmetic.
    /// A failed admission has no side effects.
    pub fn charge(&mut self, owner: ComponentId, cost: Charge) -> Result<ChargeTicket, GraphError> {
        if self.outstanding.len() >= MAX_ACTIVE_CHARGES {
            return Err(GraphError::Full);
        }
        if self.next_ticket == u64::MAX {
            return Err(GraphError::TicketExhausted);
        }
        let mut chain = Vec::new();
        chain
            .try_reserve(self.slots.len())
            .map_err(|_| GraphError::OutOfMemory)?;
        let mut current = Some(owner);
        for _ in 0..self.slots.len() {
            let Some(id) = current else { break };
            let c = self.component(id)?;
            if c.state != ComponentState::Running {
                return Err(GraphError::InvalidState);
            }
            c.used.admit(cost, c.quota).map_err(|err| match err {
                QuotaError::Exceeded => GraphError::ChargeRefused,
                _ => GraphError::ChargeOverflow,
            })?;
            chain.push(id);
            current = c.parent;
        }
        if current.is_some() {
            return Err(GraphError::InvalidParent);
        }
        self.outstanding
            .try_reserve(1)
            .map_err(|_| GraphError::OutOfMemory)?;
        let ticket = ChargeTicket {
            serial: self.next_ticket,
            owner,
        };
        self.next_ticket += 1;
        for id in chain {
            let c = self
                .component_mut(id)
                .expect("validated immutable ancestry");
            c.used = c.used.checked_add(cost).expect("prevalidated sums");
        }
        self.outstanding.push(OutstandingCharge { ticket, cost });
        Ok(ticket)
    }

    /// The accounting cost is taken from trusted retained state; the caller
    /// never supplies a refund amount. A double release is rejected.
    pub fn release(&mut self, ticket: ChargeTicket) -> Result<(), GraphError> {
        let pos = self
            .outstanding
            .iter()
            .position(|t| t.ticket == ticket)
            .ok_or(GraphError::InvalidTicket)?;
        let cost = self.outstanding[pos].cost;
        let mut current = Some(ticket.owner);
        let mut chain = Vec::new();
        chain
            .try_reserve(self.slots.len())
            .map_err(|_| GraphError::OutOfMemory)?;
        for _ in 0..self.slots.len() {
            let Some(id) = current else { break };
            let c = self.component(id)?;
            c.used
                .checked_sub(cost)
                .map_err(|_| GraphError::ChargeOverflow)?;
            chain.push(id);
            current = c.parent;
        }
        if current.is_some() {
            return Err(GraphError::InvalidParent);
        }
        for id in chain {
            let c = self
                .component_mut(id)
                .expect("validated unchanged ancestry");
            c.used = c.used.checked_sub(cost).expect("prevalidated accounting");
        }
        self.outstanding.swap_remove(pos);
        Ok(())
    }

    pub fn active_charges(&self) -> usize {
        self.outstanding.len()
    }

    pub fn set_max_routes(&mut self, limit: usize) -> Result<(), GraphError> {
        if limit > MAX_ROUTE_RECORDS || self.routes.len() > limit || self.exports.len() > limit {
            return Err(GraphError::InvalidLimit);
        }
        self.max_routes = limit;
        Ok(())
    }
}
