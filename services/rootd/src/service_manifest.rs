//! Immutable root-owned bootstrap and post-init scheduling manifest.

use rustos_user_abi::syscall::{
    RustosSchedulingContextPolicy, IPC_SERVICE_DEVMGRD, IPC_SERVICE_INITD, IPC_SERVICE_INPUTD,
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

pub(super) const USER_WORKLOAD_SCHEDULING_POLICY: RustosSchedulingContextPolicy =
    scheduling_policy(USER_WORKLOAD_DOMAIN, 2_000_000, 0);

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
    pub(super) scheduling: RustosSchedulingContextPolicy,
}

pub(super) const POST_INIT_MANIFEST: [PostInitServiceSpec; 6] = [
    PostInitServiceSpec {
        service_id: IPC_SERVICE_NETD,
        exec_path: NETD_EXEC,
        scheduling: scheduling_policy(IPC_SERVICE_NETD, 4_000_000, 1),
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_DEVMGRD,
        exec_path: DEVMGRD_EXEC,
        scheduling: scheduling_policy(IPC_SERVICE_DEVMGRD, 4_000_000, 1),
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_INPUTD,
        exec_path: INPUTD_EXEC,
        scheduling: scheduling_policy(IPC_SERVICE_INPUTD, 4_000_000, 1),
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_STORAGED,
        exec_path: STORAGED_EXEC,
        scheduling: scheduling_policy(IPC_SERVICE_STORAGED, 4_000_000, 1),
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_SESSIOND,
        exec_path: RUNTIMED_EXEC,
        scheduling: scheduling_policy(IPC_SERVICE_SESSIOND, 4_000_000, 1),
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_UISERVER,
        exec_path: UISERVER_EXEC,
        scheduling: scheduling_policy(IPC_SERVICE_UISERVER, 6_000_000, 2),
    },
];
