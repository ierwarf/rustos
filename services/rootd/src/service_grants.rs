//! Declarative positive grants for rootd slow control-plane operations.
//!
//! The existing service "capability_mask" is a compatibility presentation,
//! not authorization. Effective authorization is the intersection of:
//!   (1) exact live PID/lease/reporter chain
//!   (2) this immutable per-service operation grant
//!   (3) the service's current kernel JobPolicy ceiling.
//! No operation is granted by a name or by an untrusted scalar role bit.
//! This table is intentionally never consulted in scheduler/IPC/futex/fault
//! mechanism paths.
use rustos_component_model::policy_ops::SlowBrokerOperation as Op;
use rustos_user_abi::syscall::{
    IPC_SERVICE_DEVMGRD, IPC_SERVICE_INITD, IPC_SERVICE_INPUTD, IPC_SERVICE_LINUX_SYSCALLD,
    IPC_SERVICE_LOADERD, IPC_SERVICE_NETD, IPC_SERVICE_PAGERD, IPC_SERVICE_PROCD,
    IPC_SERVICE_ROOTD, IPC_SERVICE_SESSIOND, IPC_SERVICE_STORAGED,
    IPC_SERVICE_UISERVER, IPC_SERVICE_VFSD,
};

#[derive(Clone, Copy)]
pub(super) struct PositiveGrant {
    pub service: u64,
    pub operations: u64,
}

const fn op_mask(op: Op) -> u64 { 1u64 << (op as u16) }
const fn service_reads() -> u64 {
    op_mask(Op::ResolveServiceRoute) | op_mask(Op::ReadServicePrincipal)
        | op_mask(Op::ReportComponentHealth)
}

const fn checkpoint() -> u64 {
    op_mask(Op::WriteRecoveryCheckpoint)
        | op_mask(Op::CompactRecoveryCheckpoint)
        | op_mask(Op::ReadRecoveryCheckpoint)
}

const fn grant(service: u64, operations: u64) -> PositiveGrant {
    PositiveGrant { service, operations: operations | service_reads() }
}

pub(super) const GRANTS: [PositiveGrant; 13] = [
    grant(IPC_SERVICE_LINUX_SYSCALLD, checkpoint()),
    grant(IPC_SERVICE_VFSD, checkpoint() | op_mask(Op::MountFilesystem) | op_mask(Op::ModifyFilesystemPolicy) | op_mask(Op::ChargeClientSession)),
    grant(IPC_SERVICE_LOADERD, checkpoint() | op_mask(Op::SpawnProcess)
        | op_mask(Op::TransferJobDescriptor) | op_mask(Op::UpgradeComponent)),
    grant(IPC_SERVICE_PROCD, checkpoint() | op_mask(Op::SignalForeignProcess)),
    grant(IPC_SERVICE_PAGERD, checkpoint()),
    grant(IPC_SERVICE_INITD, checkpoint() | op_mask(Op::ReclaimServiceLease)
        | op_mask(Op::ConfigureSupervisor) | op_mask(Op::UpgradeComponent)
        | op_mask(Op::RollbackComponent)),
    grant(IPC_SERVICE_NETD, checkpoint() | op_mask(Op::ConfigureNetwork) | op_mask(Op::ChargeClientSession)),
    grant(IPC_SERVICE_DEVMGRD, checkpoint() | op_mask(Op::ControlDevice)),
    grant(IPC_SERVICE_INPUTD, checkpoint()),
    grant(IPC_SERVICE_STORAGED, checkpoint()),
    grant(IPC_SERVICE_SESSIOND, checkpoint() | op_mask(Op::CreateSession)
        | op_mask(Op::RouteSession) | op_mask(Op::ChargeClientSession)),
    grant(IPC_SERVICE_UISERVER, checkpoint()),
    grant(IPC_SERVICE_ROOTD, u64::MAX),
];

pub(super) fn permits(service: u64, op: Op) -> bool {
    GRANTS.iter()
        .find(|grant| grant.service == service)
        .is_some_and(|grant| (grant.operations & op_mask(op)) != 0)
}

pub(super) fn preflight() -> Result<(), ()> {
    use super::service_manifest::SERVICE_ROLE_MANIFEST;
    for (index, entry) in GRANTS.iter().enumerate() {
        if entry.service == 0
            || entry.operations == 0
            || SERVICE_ROLE_MANIFEST.iter().all(|role| role.service_id != entry.service)
            || GRANTS[..index].iter().any(|prev| prev.service == entry.service)
        {
            return Err(());
        }
    }
    for role in SERVICE_ROLE_MANIFEST {
        if !GRANTS.iter().any(|grant| grant.service == role.service_id) {
            return Err(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_service_has_one_positive_grant_and_no_cross_service_reclaim() {
        assert_eq!(preflight(), Ok(()));
        assert!(permits(IPC_SERVICE_INITD, Op::ReclaimServiceLease));
        assert!(!permits(IPC_SERVICE_NETD, Op::ReclaimServiceLease));
        assert!(!permits(IPC_SERVICE_SESSIOND, Op::ReclaimServiceLease));
        assert!(permits(IPC_SERVICE_SESSIOND, Op::CreateSession));
        assert!(!permits(IPC_SERVICE_VFSD, Op::CreateSession));
        assert!(!permits(0, Op::WriteRecoveryCheckpoint));
        assert!(permits(IPC_SERVICE_NETD, Op::ResolveServiceRoute));
        assert!(permits(IPC_SERVICE_SESSIOND, Op::ChargeClientSession));
        assert!(permits(IPC_SERVICE_VFSD, Op::ChargeClientSession));
    }
}
