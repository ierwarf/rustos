//! Stable semantic operation IDs for **slow control-plane broker** policy.
//!
//! No IDs are assigned to futex, clock_gettime, getpid/gettid, scheduler
//! yield, memory fault, usercopy or page-table/TLB mechanism. Those continue
//! through their existing ring0 substrates without added policy IPC.
//! A set bit in a Job ceiling only *restricts* an operation. It never
//! replaces the positive descriptor, exact process generation or service
//! lease grant that authorizes the syscall.

use crate::policy::{JobCeiling, Operation};

#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlowBrokerOperation {
    SpawnProcess = 0,
    CreateJob = 1,
    SetJobLimits = 2,
    SetJobPolicy = 3,
    TerminateProcess = 4,
    ChangeCredentials = 5,
    ChangeNamespace = 6,
    MountFilesystem = 7,
    OpenDevice = 8,
    ControlDevice = 9,
    AttachDriverDomain = 10,
    CreateNetworkSocket = 11,
    BindNetworkSocket = 12,
    ConnectNetworkSocket = 13,
    ConfigureNetwork = 14,
    CreateSession = 15,
    RouteSession = 16,
    CreateCapabilityRoute = 17,
    RevokeCapabilityRoute = 18,
    UpgradeComponent = 19,
    RollbackComponent = 20,
    ReadCrashRecords = 21,
    ConfigureSupervisor = 22,
    ConfigureMemoryLimits = 23,
    ConfigureCpuPolicy = 24,
    SignalForeignProcess = 25,
    IntrospectForeignJob = 26,
    PublishServiceEndpoint = 27,
    TransferJobDescriptor = 28,
    CreateSharedMemory = 29,
    ModifyFilesystemPolicy = 30,
    InstallIdentityPolicy = 31,
}

impl SlowBrokerOperation {
    pub const ALL: [Self; 32] = [
        Self::SpawnProcess,
        Self::CreateJob,
        Self::SetJobLimits,
        Self::SetJobPolicy,
        Self::TerminateProcess,
        Self::ChangeCredentials,
        Self::ChangeNamespace,
        Self::MountFilesystem,
        Self::OpenDevice,
        Self::ControlDevice,
        Self::AttachDriverDomain,
        Self::CreateNetworkSocket,
        Self::BindNetworkSocket,
        Self::ConnectNetworkSocket,
        Self::ConfigureNetwork,
        Self::CreateSession,
        Self::RouteSession,
        Self::CreateCapabilityRoute,
        Self::RevokeCapabilityRoute,
        Self::UpgradeComponent,
        Self::RollbackComponent,
        Self::ReadCrashRecords,
        Self::ConfigureSupervisor,
        Self::ConfigureMemoryLimits,
        Self::ConfigureCpuPolicy,
        Self::SignalForeignProcess,
        Self::IntrospectForeignJob,
        Self::PublishServiceEndpoint,
        Self::TransferJobDescriptor,
        Self::CreateSharedMemory,
        Self::ModifyFilesystemPolicy,
        Self::InstallIdentityPolicy,
    ];

    pub const fn operation(self) -> Operation {
        Operation(self as u16)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrokerDecision {
    DeniedByCeiling,
    SubjectGrantStillRequired,
}

/// Does **not** grant an action. Successful policy checks must still pass
/// the kernel's existing positive capability/role authority check.
pub fn inspect_ceiling(effective: JobCeiling, operation: SlowBrokerOperation) -> BrokerDecision {
    if effective.permits(operation.operation()) {
        BrokerDecision::SubjectGrantStillRequired
    } else {
        BrokerDecision::DeniedByCeiling
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_ids_are_unique_and_stay_inside_256_bit_ceiling() {
        let mut seen = [false; 256];
        for op in SlowBrokerOperation::ALL {
            let id = op.operation().0 as usize;
            assert!(!seen[id]);
            seen[id] = true;
            assert_eq!(
                inspect_ceiling(JobCeiling::ALL, op),
                BrokerDecision::SubjectGrantStillRequired
            );
            assert_eq!(
                inspect_ceiling(JobCeiling::NONE, op),
                BrokerDecision::DeniedByCeiling
            );
        }
    }
}
