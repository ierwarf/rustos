//! Pure userspace component policy model, designed for integration by rootd.
//!
//! This library deliberately performs **no syscalls** and has no kernel
//! privileges. In particular it cannot mint Job descriptors or bypass the
//! capability and process identity checks enforced in ring0.
//!
//! A single supervisor owns each Graph. Do not put it behind a global lock
//! held across IPC. Callers must authenticate the incoming service identity
//! against a live lease before mutating this state machine.
//!
//! This model is not yet a replacement for rootd's production bootstrap,
//! leases, or wire ABI. Its tests prove policy algebra, not live integration.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod generated_idl;
pub mod health;
pub mod health_wire;
pub mod identity;
pub mod idl;
pub mod lease_reclaim;
pub mod lease_snapshot;
pub mod client_session_wire;
pub mod manifest;
pub mod service_catalog;
pub mod policy;
pub mod policy_ops;
pub mod personality;
pub mod resource_ledger;
pub mod capability_escrow;
pub mod canonical_rpc;
pub mod component_activation;
pub mod provider_sessions;
pub mod durable_recovery;
pub mod namespace_policy;
pub mod image_custody;
pub mod reconcile;
pub mod provider_wire;
pub mod pressure_governor;
pub mod rollout_plan;
pub mod storage_replay;
pub mod restart_transaction;
pub mod pressure;
pub mod quota;
pub mod realm;
pub mod routing;
pub mod session;
pub mod supervision;
pub mod telemetry;
pub mod update;
pub mod update_wire;
pub mod wire;

pub use identity::{ComponentId, Moniker, MonikerError};
pub use policy::{JobCeiling, Operation, PolicyError};
pub use quota::{Charge, ChargeError, Quota, QuotaError};
pub use realm::{ComponentState, Graph, GraphError, RestartState};
pub use routing::{Capability, CapabilityError, CapabilityRoute};
pub use supervision::{ExitAction, ExitReason, RestartPolicy, SupervisorDecision};

#[cfg(test)]
mod tests;
