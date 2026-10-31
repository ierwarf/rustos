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

pub mod health;
pub mod identity;
pub mod manifest;
pub mod policy;
pub mod pressure;
pub mod quota;
pub mod realm;
pub mod routing;
pub mod session;
pub mod supervision;
pub mod telemetry;
pub mod update;
pub mod wire;

pub use identity::{ComponentId, Moniker, MonikerError};
pub use policy::{JobCeiling, Operation, PolicyError};
pub use quota::{Charge, ChargeError, Quota, QuotaError};
pub use realm::{ComponentState, Graph, GraphError, RestartState};
pub use routing::{Capability, CapabilityError, CapabilityRoute};
pub use supervision::{ExitAction, ExitReason, RestartPolicy, SupervisorDecision};

#[cfg(test)]
mod tests;
