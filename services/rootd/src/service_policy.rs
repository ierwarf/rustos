//! The single place rootd decides what a service may do, and checks that the
//! pieces of that decision agree.
//!
//! Each service has exactly one `ServicePolicy` row in the service manifest.
//! Three authorities are read from it and from nothing else: the typed kernel
//! role (`component_policy::service_policy_capability`, installed with
//! `SYS_RUSTOS_SERVICE_POLICY`), the positive slow-operation grants
//! (`permits`, "(2)" of the intersection below) and the personality grants
//! (`personality_policy`). Effective slow authorization is the intersection of
//! (1) the exact live PID/lease/reporter chain, (2) this row, and (3) the
//! service's current kernel JobPolicy ceiling. No operation is granted by a
//! name or an untrusted scalar. Never consulted from scheduler, IPC, futex or
//! fault paths.
use rustos_component_model::personality::SlowRequest;
use rustos_component_model::policy_ops::SlowBrokerOperation as Op;
use rustos_component_model::service_catalog::{self, ROOT_SERVICE_ID, SERVICES};

use super::service_manifest::{ServicePolicy, SERVICE_POLICY_MANIFEST};

/// Held by every service: route resolution, principal lookup and health.
pub(super) const BASE_OPERATIONS: [Op; 3] =
    [Op::ResolveServiceRoute, Op::ReadServicePrincipal, Op::ReportComponentHealth];

/// Recovery checkpoints, held by every service that keeps restart state.
pub(super) const CHECKPOINT_OPERATIONS: [Op; 3] =
    [Op::WriteRecoveryCheckpoint, Op::CompactRecoveryCheckpoint, Op::ReadRecoveryCheckpoint];

pub(super) fn policy_of(service: u64) -> Option<&'static ServicePolicy> {
    SERVICE_POLICY_MANIFEST.iter().find(|row| row.service_id == service)
}

pub(super) fn permits(service: u64, op: Op) -> bool {
    policy_of(service).is_some_and(|row| {
        BASE_OPERATIONS.contains(&op)
            || (row.checkpoints && CHECKPOINT_OPERATIONS.contains(&op))
            || row.slow_ops.contains(&op)
    })
}

/// Operations only their natural holders may have.
const EXCLUSIVE: &[(Op, &[u64])] = &[
    (Op::ChargeClientSession, &[2, 3, 11]),
    (Op::ReclaimServiceLease, &[15]),
    (Op::ConfigureNetwork, &[3]),
    (Op::ModifyFilesystemPolicy, &[2]),
    (Op::UpgradeComponent, &[6, 15]),
    (Op::RollbackComponent, &[15]),
];

/// Runs before rootd publishes its endpoint. Every cross-table invariant that
/// used to be split between three files is checked here, against one table.
pub(super) fn preflight() -> Result<(), &'static str> {
    service_catalog::validate().map_err(|_| "service catalog invalid")?;
    let rows = &SERVICE_POLICY_MANIFEST;
    for (index, row) in rows.iter().enumerate() {
        let known = row.service_id == ROOT_SERVICE_ID
            || SERVICES.iter().any(|service| service.id == row.service_id);
        if !known {
            return Err("policy row for an unknown service");
        }
        // One row per service, and a role is exactly one typed capability, so
        // only injectivity remains to be checked.
        if rows[..index]
            .iter()
            .any(|prev| prev.service_id == row.service_id || prev.capability == row.capability)
        {
            return Err("duplicate service or role");
        }
        if row.personalities.is_empty() {
            continue;
        }
        // A personality is consulted through the SpawnProcess-gated rootd op,
        // so a row that grants one without that operation could never use it.
        if !permits(row.service_id, Op::SpawnProcess) {
            return Err("personality grant without the SpawnProcess operation");
        }
        for (i, grant) in row.personalities.iter().enumerate() {
            if grant.namespace == 0
                || row.personalities[..i].iter().any(|prev| {
                    prev.personality == grant.personality
                        && prev.request == grant.request
                        && prev.namespace == grant.namespace
                })
                // Mechanisms stay in Ring0; a slow policy can only gate
                // these request classes, never reclassify a kernel path.
                || !matches!(
                    grant.request,
                    SlowRequest::ExecutableImage
                        | SlowRequest::FileAssociation
                        | SlowRequest::MountPolicy
                        | SlowRequest::NetworkPolicy
                        | SlowRequest::CredentialPolicy
                        | SlowRequest::NamespacePolicy
                        | SlowRequest::CompatibilityProfile
                )
            {
                return Err("invalid personality grant");
            }
        }
    }
    // Every catalog component has a row, and so does rootd.
    if SERVICES.iter().any(|service| policy_of(service.id).is_none())
        || policy_of(ROOT_SERVICE_ID).is_none()
    {
        return Err("a catalog service has no policy row");
    }
    for service in SERVICES {
        if !permits(service.id, Op::ResolveServiceRoute) {
            return Err("a service cannot resolve routes");
        }
    }
    for (op, holders) in EXCLUSIVE {
        for service in SERVICES {
            if permits(service.id, *op) != holders.contains(&service.id) {
                return Err("an exclusive operation has the wrong holders");
            }
        }
    }
    // The Job ceiling is a second gate indexed by operation number; reordering
    // the enum must not alias two operations onto one bit.
    for (i, op) in Op::ALL.iter().enumerate() {
        if *op as u16 >= 64 || Op::ALL[..i].iter().any(|prev| *prev as u16 == *op as u16) {
            return Err("operation ids collide or exceed the ceiling width");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::*;

    #[test]
    fn the_manifest_passes_its_own_preflight() {
        assert_eq!(preflight(), Ok(()));
    }

    #[test]
    fn every_service_has_one_positive_grant_and_no_cross_service_reclaim() {
        assert!(permits(IPC_SERVICE_INITD, Op::ReclaimServiceLease));
        assert!(!permits(IPC_SERVICE_NETD, Op::ReclaimServiceLease));
        assert!(!permits(IPC_SERVICE_SESSIOND, Op::ReclaimServiceLease));
        assert!(permits(IPC_SERVICE_SESSIOND, Op::CreateSession));
        assert!(!permits(IPC_SERVICE_VFSD, Op::CreateSession));
        assert!(!permits(0, Op::WriteRecoveryCheckpoint));
        assert!(permits(IPC_SERVICE_NETD, Op::ResolveServiceRoute));
        assert!(permits(IPC_SERVICE_SESSIOND, Op::ChargeClientSession));
        assert!(permits(IPC_SERVICE_VFSD, Op::ChargeClientSession));
        assert!(permits(IPC_SERVICE_ROOTD, Op::RollbackComponent), "rootd holds every operation");
    }

    #[test]
    fn the_roster_has_no_implicit_charging_authority() {
        assert!(permits(11, Op::ChargeClientSession));
        assert!(permits(3, Op::ChargeClientSession));
        assert!(permits(2, Op::ChargeClientSession));
        assert!(!permits(IPC_SERVICE_PAGERD, Op::ChargeClientSession));
    }

    #[test]
    fn only_the_loader_holds_personality_grants() {
        for row in SERVICE_POLICY_MANIFEST {
            assert_eq!(
                !row.personalities.is_empty(),
                row.service_id == IPC_SERVICE_LOADERD,
                "service {}",
                row.service_id
            );
        }
    }
}
