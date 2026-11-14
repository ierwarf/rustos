//! Immutable root-owned bootstrap and post-init scheduling manifest.

use rustos_component_model::personality::{Personality, SlowRequest};
use rustos_component_model::policy_ops::SlowBrokerOperation as Op;
use rustos_user_abi::syscall::{
    RustosSchedulingContextPolicy, ServiceCapability, IPC_SERVICE_ROOTD, IPC_SERVICE_DEVMGRD, IPC_SERVICE_INITD, IPC_SERVICE_INPUTD,
    IPC_SERVICE_LINUX_SYSCALLD, IPC_SERVICE_LOADERD, IPC_SERVICE_NETD, IPC_SERVICE_PAGERD,
    IPC_SERVICE_PROCD, IPC_SERVICE_SESSIOND, IPC_SERVICE_STORAGED, IPC_SERVICE_UISERVER,
    IPC_SERVICE_VFSD, TASK_WEIGHT_INTERACTIVE_FLAG,
};

pub(super) const CORE_SERVICE_WEIGHT_MICROS: u64 = TASK_WEIGHT_INTERACTIVE_FLAG | 4_000;
pub(super) const INITD_WEIGHT_MICROS: u64 = 4_000;
const _: () = assert!(CORE_SERVICE_WEIGHT_MICROS & TASK_WEIGHT_INTERACTIVE_FLAG != 0);
const _: () = assert!(INITD_WEIGHT_MICROS & TASK_WEIGHT_INTERACTIVE_FLAG == 0);
const SYSCALLD_EXEC: &[u8] = b"services/syscalld/syscalld.elf\0";
const PAGERD_EXEC: &[u8] = b"services/pagerd/pagerd.elf\0";
const VFSD_EXEC: &[u8] = b"services/vfsd/vfsd.elf\0";
const LOADERD_EXEC: &[u8] = b"services/loaderd/loaderd.elf\0";
const PROCD_EXEC: &[u8] = b"services/procd/procd.elf\0";
const INITD_EXEC: &[u8] = b"services/initd/initd.elf\0";
const NETD_EXEC: &[u8] = b"services/netd/netd.elf\0";
const DEVMGRD_EXEC: &[u8] = b"services/devmgrd/devmgrd.elf\0";
const INPUTD_EXEC: &[u8] = b"services/inputd/inputd.elf\0";
const STORAGED_EXEC: &[u8] = b"services/storaged/storaged.elf\0";
const RUNTIMED_EXEC: &[u8] = b"services/runtimed/runtimed.elf\0";
const UISERVER_EXEC: &[u8] = b"services/uiserver/uiserver.elf\0";
pub(super) const INITD_LEASE_ID: u64 = IPC_SERVICE_INITD;
pub(super) const INITD_LEASE_INDEX: usize = 5;
pub(super) const DEP_SYSCALLD: u16 = 1 << 0;
pub(super) const DEP_VFSD: u16 = 1 << 1;
pub(super) const DEP_LOADERD: u16 = 1 << 2;
pub(super) const DEP_PROCD: u16 = 1 << 3;
pub(super) const DEP_PAGERD: u16 = 1 << 4;
const SCHEDULING_MANIFEST_EPOCH: u64 = 1;
const SCHEDULING_PERIOD_NS: u64 = 10_000_000;
const USER_WORKLOAD_DOMAIN: u64 = 0x1_000;

const fn scheduling_policy(
    domain: u64,
    budget_ns: u64,
    criticality: u8,
) -> RustosSchedulingContextPolicy {
    RustosSchedulingContextPolicy::new(
        u64::MAX,
        budget_ns,
        SCHEDULING_PERIOD_NS,
        8,
        criticality,
        domain,
        SCHEDULING_MANIFEST_EPOCH,
    )
}

/// Ordinary applications: best-effort criticality with a full per-CPU share.
/// User-class selection already yields to every System service, so this budget
/// only accounts for how long one application holds a CPU; it must never idle
/// a CPU that has nothing else to run. A general-purpose scheduler is
/// work-conserving: the former 2 ms (20%) share and then the 8 ms (80%) share
/// parked the CPU on idle while the task waited for its refill, which made
/// every ordinary workload, ipcbench included, slower than its own cost (a
/// fifth of the CPU at 2 ms, and a forced 2 ms gap per 10 ms at 8 ms).
/// Budget equal to period is a valid policy: a continuous run is refilled one
/// period after it began, which is exactly when it would exhaust.
pub(super) const USER_WORKLOAD_SCHEDULING_POLICY: RustosSchedulingContextPolicy =
    scheduling_policy(USER_WORKLOAD_DOMAIN, SCHEDULING_PERIOD_NS, 0);

#[derive(Clone, Copy)]
pub(super) struct BootstrapServiceSpec {
    pub(super) service_id: u64,
    pub(super) exec_path: &'static [u8],
    pub(super) weight_micros: u64,
    pub(super) dependency_mask: u16,
    pub(super) bootstrap_direct: bool,
    pub(super) restart_direct: bool,
    pub(super) scheduling: RustosSchedulingContextPolicy,
}

pub(super) const BOOTSTRAP_MANIFEST: [BootstrapServiceSpec; 6] = [
    BootstrapServiceSpec {
        service_id: IPC_SERVICE_LINUX_SYSCALLD,
        exec_path: SYSCALLD_EXEC,
        weight_micros: CORE_SERVICE_WEIGHT_MICROS,
        dependency_mask: 0,
        bootstrap_direct: true,
        restart_direct: false,
        scheduling: scheduling_policy(IPC_SERVICE_LINUX_SYSCALLD, 4_000_000, 1),
    },
    BootstrapServiceSpec {
        service_id: IPC_SERVICE_VFSD,
        exec_path: VFSD_EXEC,
        weight_micros: CORE_SERVICE_WEIGHT_MICROS,
        dependency_mask: 0,
        bootstrap_direct: true,
        restart_direct: false,
        scheduling: scheduling_policy(IPC_SERVICE_VFSD, 4_000_000, 1),
    },
    BootstrapServiceSpec {
        service_id: IPC_SERVICE_LOADERD,
        exec_path: LOADERD_EXEC,
        weight_micros: CORE_SERVICE_WEIGHT_MICROS,
        dependency_mask: DEP_VFSD,
        bootstrap_direct: true,
        restart_direct: true,
        scheduling: scheduling_policy(IPC_SERVICE_LOADERD, 4_000_000, 1),
    },
    BootstrapServiceSpec {
        service_id: IPC_SERVICE_PROCD,
        exec_path: PROCD_EXEC,
        weight_micros: CORE_SERVICE_WEIGHT_MICROS,
        dependency_mask: 0,
        bootstrap_direct: true,
        restart_direct: false,
        scheduling: scheduling_policy(IPC_SERVICE_PROCD, 4_000_000, 1),
    },
    BootstrapServiceSpec {
        service_id: IPC_SERVICE_PAGERD,
        exec_path: PAGERD_EXEC,
        weight_micros: CORE_SERVICE_WEIGHT_MICROS,
        dependency_mask: 0,
        bootstrap_direct: true,
        restart_direct: true,
        // pagerd and uiserver overlap on every enabled CPU. Keep their
        // criticality-2 utilization at the kernel's 90% admission ceiling.
        scheduling: scheduling_policy(IPC_SERVICE_PAGERD, 3_000_000, 2),
    },
    BootstrapServiceSpec {
        service_id: INITD_LEASE_ID,
        exec_path: INITD_EXEC,
        weight_micros: INITD_WEIGHT_MICROS,
        dependency_mask: DEP_SYSCALLD | DEP_VFSD | DEP_LOADERD | DEP_PROCD | DEP_PAGERD,
        bootstrap_direct: false,
        restart_direct: false,
        scheduling: scheduling_policy(IPC_SERVICE_INITD, 4_000_000, 1),
    },
];

#[derive(Clone, Copy)]
pub(super) struct PostInitServiceSpec {
    pub(super) service_id: u64,
    pub(super) exec_path: &'static [u8],
    /// Exact service principal allowed to report and supervise this child.
    /// The ancestor must itself have a live, authenticated reporter chain.
    pub(super) reporter_service_id: u64,
    pub(super) scheduling: RustosSchedulingContextPolicy,
}

pub(super) const POST_INIT_MANIFEST: [PostInitServiceSpec; 6] = [
    PostInitServiceSpec {
        service_id: IPC_SERVICE_NETD,
        exec_path: NETD_EXEC,
        reporter_service_id: INITD_LEASE_ID,
        scheduling: scheduling_policy(IPC_SERVICE_NETD, 4_000_000, 1),
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_DEVMGRD,
        exec_path: DEVMGRD_EXEC,
        reporter_service_id: INITD_LEASE_ID,
        scheduling: scheduling_policy(IPC_SERVICE_DEVMGRD, 4_000_000, 1),
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_INPUTD,
        exec_path: INPUTD_EXEC,
        reporter_service_id: INITD_LEASE_ID,
        scheduling: scheduling_policy(IPC_SERVICE_INPUTD, 4_000_000, 1),
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_STORAGED,
        exec_path: STORAGED_EXEC,
        reporter_service_id: INITD_LEASE_ID,
        scheduling: scheduling_policy(IPC_SERVICE_STORAGED, 4_000_000, 1),
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_SESSIOND,
        exec_path: RUNTIMED_EXEC,
        reporter_service_id: INITD_LEASE_ID,
        scheduling: scheduling_policy(IPC_SERVICE_SESSIOND, 4_000_000, 1),
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_UISERVER,
        exec_path: UISERVER_EXEC,
        reporter_service_id: IPC_SERVICE_SESSIOND,
        scheduling: scheduling_policy(IPC_SERVICE_UISERVER, 6_000_000, 2),
    },
];

/// Directed service-lookup capabilities beyond bootstrap dependency offers.
/// This is an immutable rootd control-plane policy, **not** a kernel handle.
/// A caller must still satisfy exact live lease / sender-identity checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ServiceRouteSpec {
    pub(super) subject: u64,
    pub(super) target: u64,
}

/// One source of truth for named post-init and cross-service lookup routes.
/// These offers are checked during bootstrap and evaluated without allocation
/// at lookup time. Bootstrap dependency offers remain in dependency_mask.
pub(super) const SERVICE_ROUTING_MANIFEST: [ServiceRouteSpec; 16] = [
    ServiceRouteSpec {
        subject: INITD_LEASE_ID,
        target: IPC_SERVICE_NETD,
    },
    ServiceRouteSpec {
        subject: INITD_LEASE_ID,
        target: IPC_SERVICE_DEVMGRD,
    },
    ServiceRouteSpec {
        subject: INITD_LEASE_ID,
        target: IPC_SERVICE_INPUTD,
    },
    ServiceRouteSpec {
        subject: INITD_LEASE_ID,
        target: IPC_SERVICE_STORAGED,
    },
    ServiceRouteSpec {
        subject: INITD_LEASE_ID,
        target: IPC_SERVICE_SESSIOND,
    },
    ServiceRouteSpec {
        subject: IPC_SERVICE_VFSD,
        target: IPC_SERVICE_DEVMGRD,
    },
    ServiceRouteSpec {
        subject: IPC_SERVICE_VFSD,
        target: IPC_SERVICE_STORAGED,
    },
    // The pager's shared-library warm worker reads the immutable library files
    // listed in its authenticated manifest. pagerd starts before vfsd, so this
    // is a post-init route, never a bootstrap dependency.
    ServiceRouteSpec {
        subject: IPC_SERVICE_PAGERD,
        target: IPC_SERVICE_VFSD,
    },
    ServiceRouteSpec {
        subject: IPC_SERVICE_PROCD,
        target: IPC_SERVICE_LOADERD,
    },
    ServiceRouteSpec {
        subject: IPC_SERVICE_INPUTD,
        target: IPC_SERVICE_NETD,
    },
    ServiceRouteSpec {
        subject: IPC_SERVICE_SESSIOND,
        target: IPC_SERVICE_LOADERD,
    },
    ServiceRouteSpec {
        subject: IPC_SERVICE_SESSIOND,
        target: IPC_SERVICE_DEVMGRD,
    },
    ServiceRouteSpec {
        subject: IPC_SERVICE_SESSIOND,
        target: IPC_SERVICE_UISERVER,
    },
    ServiceRouteSpec {
        subject: IPC_SERVICE_UISERVER,
        target: IPC_SERVICE_INPUTD,
    },
    ServiceRouteSpec {
        subject: IPC_SERVICE_DEVMGRD,
        target: IPC_SERVICE_SESSIOND,
    },
    ServiceRouteSpec {
        subject: IPC_SERVICE_DEVMGRD,
        target: IPC_SERVICE_UISERVER,
    },
];

/// What a service may do for a personality: act on one class of slow request
/// inside one namespace (see `personality_policy`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PersonalityGrant {
    pub(super) personality: Personality,
    pub(super) request: SlowRequest,
    pub(super) namespace: u64,
}

/// The one immutable policy record of a service. Every authority rootd hands
/// out for a service is derived from this single row: the typed kernel role
/// installed with `SYS_RUSTOS_SERVICE_POLICY` (the route table comes from the
/// dependency and routing manifests above), the positive slow-operation grants
/// (`service_grants`) and the personality grants (`personality_policy`).
/// Keeping them in one row is what lets `service_policy_preflight` check them
/// against each other, which three separate tables could not. A row is an
/// upper bound only: the kernel-stamped sender and the live lease are still
/// required to exercise it. It is never writable by requests or applications.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ServicePolicy {
    pub(super) service_id: u64,
    pub(super) capability: rustos_user_abi::syscall::ServiceCapability,
    /// Slow operations beyond the ones every service holds
    /// (`service_grants::BASE_OPERATIONS`, plus recovery checkpoints when
    /// `checkpoints` is set).
    pub(super) slow_ops: &'static [Op],
    pub(super) checkpoints: bool,
    pub(super) personalities: &'static [PersonalityGrant],
}

pub(super) const DEFAULT_NAMESPACE: u64 = 1;
const EXECUTABLE_IMAGES: &[PersonalityGrant] = &[
    PersonalityGrant {
        personality: Personality::Linux,
        request: SlowRequest::ExecutableImage,
        namespace: DEFAULT_NAMESPACE,
    },
    PersonalityGrant {
        personality: Personality::Windows,
        request: SlowRequest::ExecutableImage,
        namespace: DEFAULT_NAMESPACE,
    },
];

const fn policy(
    service_id: u64,
    capability: rustos_user_abi::syscall::ServiceCapability,
    slow_ops: &'static [Op],
) -> ServicePolicy {
    ServicePolicy { service_id, capability, slow_ops, checkpoints: true, personalities: &[] }
}

pub(super) const SERVICE_POLICY_MANIFEST: [ServicePolicy; 13] = [
    policy(IPC_SERVICE_LINUX_SYSCALLD, ServiceCapability::LinuxSyscallPolicy, &[]),
    policy(
        IPC_SERVICE_VFSD,
        ServiceCapability::VfsPolicy,
        &[Op::MountFilesystem, Op::ModifyFilesystemPolicy, Op::ChargeClientSession],
    ),
    policy(
        IPC_SERVICE_NETD,
        ServiceCapability::NetPolicy,
        &[Op::ConfigureNetwork, Op::ChargeClientSession],
    ),
    policy(IPC_SERVICE_DEVMGRD, ServiceCapability::DevicePolicy, &[Op::ControlDevice]),
    ServicePolicy {
        personalities: EXECUTABLE_IMAGES,
        ..policy(
            IPC_SERVICE_LOADERD,
            ServiceCapability::ProcessLoader,
            &[Op::SpawnProcess, Op::TransferJobDescriptor, Op::UpgradeComponent],
        )
    },
    policy(IPC_SERVICE_STORAGED, ServiceCapability::StoragePolicy, &[]),
    policy(IPC_SERVICE_INPUTD, ServiceCapability::InputPolicy, &[]),
    policy(IPC_SERVICE_PROCD, ServiceCapability::ProcessPolicy, &[Op::SignalForeignProcess]),
    policy(IPC_SERVICE_ROOTD, ServiceCapability::RootSupervisor, &Op::ALL),
    policy(
        IPC_SERVICE_SESSIOND,
        ServiceCapability::SessionPolicy,
        &[Op::CreateSession, Op::RouteSession, Op::ChargeClientSession],
    ),
    policy(IPC_SERVICE_PAGERD, ServiceCapability::PagerPolicy, &[]),
    policy(IPC_SERVICE_UISERVER, ServiceCapability::UiPolicy, &[]),
    policy(
        IPC_SERVICE_INITD,
        ServiceCapability::InitPolicy,
        &[
            Op::ReclaimServiceLease,
            Op::ConfigureSupervisor,
            Op::UpgradeComponent,
            Op::RollbackComponent,
        ],
    ),
];
