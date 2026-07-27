#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
// The host test target intentionally omits the no_std entrypoint, so the
// complete production supervisor graph is unreachable only in that harness.
#![cfg_attr(test, allow(dead_code, unused_imports))]

extern crate alloc;

mod service_checkpoint;

use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::size_of;
#[cfg(not(test))]
use core::panic::PanicInfo;
use core::slice;
use core::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};

use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, CoreServiceLeaseWire,
    LifecycleDrainBrokerArgs, LifecycleEventWire, LoaderSpawnRequest, LoaderSpawnResponse,
    RustosProcValidateDeferredSpawnBrokerArgs, RustosRootdTerminateBrokerArgs,
    ServiceCheckpointRecordWire, COMMERCIAL_MAX_CAPABILITY_OP_LEASE_GRANT,
    COMMERCIAL_MAX_CAPABILITY_OP_LEASE_RENEW, COMMERCIAL_MAX_CAPABILITY_OP_LEASE_REVOKE,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_CAPABILITY,
    COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS, COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_BOOTSTRAP_MANIFEST, COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE,
    COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH, COMMERCIAL_MAX_ROOTD_OP_LOADER_WORKER_COMPLETE,
    COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_QUERY, COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_RECLAIM,
    COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL, COMMERCIAL_MAX_ROOTD_OP_RESTART_POLICY,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY, COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_COMPACT,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_MUTATE,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_SCAN, COMMERCIAL_MAX_ROOTD_OP_SERVICE_LOOKUP,
    IPC_SERVICE_DEVMGRD, IPC_SERVICE_INITD, IPC_SERVICE_INPUTD, IPC_SERVICE_LINUX_SYSCALLD,
    IPC_SERVICE_LOADERD, IPC_SERVICE_NETD, IPC_SERVICE_PAGERD, IPC_SERVICE_PROCD,
    IPC_SERVICE_ROOTD, IPC_SERVICE_SESSIOND, IPC_SERVICE_STORAGED, IPC_SERVICE_UISERVER,
    IPC_SERVICE_VFSD, LIFECYCLE_DRAIN_BROKER_ABI_VERSION, LIFECYCLE_DRAIN_MAX_EVENTS,
    LIFECYCLE_EVENT_EXIT, LOADER_OP_ACTIVATE, LOADER_OP_SPAWN_EXEC, LOADER_REQUEST_ABI_VERSION,
    LOADER_SPAWN_ARG_BYTES, LOADER_SPAWN_ENV_BYTES, LOADER_SPAWN_EXEC_PATH_CAPACITY,
    LOADER_SPAWN_FLAG_DEFER_START, PRODUCT_MILESTONE_ROOT_CORE_READY, ROOTD_LEASE_STATE_EXITED,
    ROOTD_LEASE_STATE_FAILED, ROOTD_LEASE_STATE_RESTART_PENDING, ROOTD_LEASE_STATE_RUNNING,
    ROOTD_TERMINATE_BROKER_ABI_VERSION, SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_IPC_CALL,
    SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_RECV_WITH_SENDER, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER, SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER,
    SYS_RUSTOS_PROC_VALIDATE_DEFERRED_SPAWN_BROKER, SYS_RUSTOS_PRODUCT_MILESTONE,
    SYS_RUSTOS_ROOTD_TERMINATE_BROKER, SYS_RUSTOS_ROOTD_WAIT_BROKER, SYS_RUSTOS_SPAWN_EXEC,
    TASK_WEIGHT_INTERACTIVE_FLAG,
};
use service_checkpoint::ServiceCheckpointStore;

const SYS_SCHED_YIELD: u64 = 24;
const SYS_GETPID: u64 = 39;
const SYS_CLONE: u64 = 56;
const SYS_EXIT: u64 = 60;
const SYS_GETTID: u64 = 186;
const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;
const SPAWN_FLAG_LOGICAL_ADMIN: u64 = 1;
// Bootstrap IPC hosts sit on the syscall/loader/VFS causal path. Their strict
// latency admission is fixed in rootd's immutable manifest, never accepted
// from a package or desktop entry. This lets the scheduler's bounded System
// ready-wait rail break priority inversion when an ordinary caller blocks on
// one of these servers under sustained display/input load.
const CORE_SERVICE_WEIGHT_MICROS: u64 = TASK_WEIGHT_INTERACTIVE_FLAG | 4_000;
const INITD_WEIGHT_MICROS: u64 = 4_000;
const _: () = assert!(CORE_SERVICE_WEIGHT_MICROS & TASK_WEIGHT_INTERACTIVE_FLAG != 0);
const _: () = assert!(INITD_WEIGHT_MICROS & TASK_WEIGHT_INTERACTIVE_FLAG == 0);
const BOOTSTRAP_SPAWN_MAX_ATTEMPTS: u32 = 64;
const INITD_SPAWN_MAX_ATTEMPTS: u32 = 64;
const CORE_READINESS_POLL_INTERVAL_MS: u32 = 250;
const CORE_READINESS_POLL_MAX: u32 = 20;
/// A readiness poll turn must drain the already-queued control burst before
/// entering the 250 ms hardware/service readiness backoff. Core services start
/// concurrently and publish through this one endpoint; sleeping after one
/// request serializes their registrations into a multi-second boot delay.
const ROOTD_REQUEST_DRAIN_BUDGET: usize = 32;

const SYSCALLD_EXEC: &[u8] = b"services/syscalld/syscalld.elf\0";
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
const INITD_LEASE_ID: u64 = IPC_SERVICE_INITD;
const INITD_LEASE_INDEX: usize = 4;
const DEP_SYSCALLD: u16 = 1 << 0;
const DEP_VFSD: u16 = 1 << 1;
const DEP_LOADERD: u16 = 1 << 2;
const DEP_PROCD: u16 = 1 << 3;
const DEP_PAGERD: u16 = 1 << 4;

#[derive(Clone, Copy)]
struct BootstrapServiceSpec {
    service_id: u64,
    exec_path: &'static [u8],
    weight_micros: u64,
    dependency_mask: u16,
    bootstrap_direct: bool,
    restart_direct: bool,
}

const BOOTSTRAP_MANIFEST: [BootstrapServiceSpec; 5] = [
    BootstrapServiceSpec {
        service_id: IPC_SERVICE_LINUX_SYSCALLD,
        exec_path: SYSCALLD_EXEC,
        weight_micros: CORE_SERVICE_WEIGHT_MICROS,
        dependency_mask: 0,
        bootstrap_direct: true,
        restart_direct: false,
    },
    BootstrapServiceSpec {
        service_id: IPC_SERVICE_VFSD,
        exec_path: VFSD_EXEC,
        weight_micros: CORE_SERVICE_WEIGHT_MICROS,
        dependency_mask: 0,
        bootstrap_direct: true,
        restart_direct: false,
    },
    BootstrapServiceSpec {
        service_id: IPC_SERVICE_LOADERD,
        exec_path: LOADERD_EXEC,
        weight_micros: CORE_SERVICE_WEIGHT_MICROS,
        // loaderd requests terminally sealed executable snapshots from vfsd;
        // it never reads mutable path-backed bytes into a commit transaction.
        dependency_mask: DEP_VFSD,
        bootstrap_direct: true,
        restart_direct: true,
    },
    BootstrapServiceSpec {
        service_id: IPC_SERVICE_PROCD,
        exec_path: PROCD_EXEC,
        weight_micros: CORE_SERVICE_WEIGHT_MICROS,
        dependency_mask: 0,
        bootstrap_direct: true,
        restart_direct: false,
    },
    BootstrapServiceSpec {
        service_id: INITD_LEASE_ID,
        exec_path: INITD_EXEC,
        weight_micros: INITD_WEIGHT_MICROS,
        dependency_mask: DEP_SYSCALLD | DEP_VFSD | DEP_LOADERD | DEP_PROCD | DEP_PAGERD,
        bootstrap_direct: false,
        restart_direct: false,
    },
];

#[derive(Clone, Copy)]
struct PostInitServiceSpec {
    service_id: u64,
    exec_path: &'static [u8],
}

const POST_INIT_MANIFEST: [PostInitServiceSpec; 6] = [
    PostInitServiceSpec {
        service_id: IPC_SERVICE_NETD,
        exec_path: NETD_EXEC,
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_DEVMGRD,
        exec_path: DEVMGRD_EXEC,
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_INPUTD,
        exec_path: INPUTD_EXEC,
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_STORAGED,
        exec_path: STORAGED_EXEC,
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_SESSIOND,
        exec_path: RUNTIMED_EXEC,
    },
    PostInitServiceSpec {
        service_id: IPC_SERVICE_UISERVER,
        exec_path: UISERVER_EXEC,
    },
];

struct RootdIpcCell<T>(UnsafeCell<T>);

unsafe impl<T> Sync for RootdIpcCell<T> {}

#[derive(Clone, Copy)]
struct IpcSenderIdentity {
    pid: u64,
    tid: u64,
}

static INITD_LOADER_REQUEST: RootdIpcCell<LoaderSpawnRequest> =
    RootdIpcCell(UnsafeCell::new(empty_loader_spawn_request()));
static INITD_LOADER_RESPONSE: RootdIpcCell<LoaderSpawnResponse> =
    RootdIpcCell(UnsafeCell::new(empty_loader_spawn_response()));

const LOADER_WORKER_IDLE: usize = 0;
const LOADER_WORKER_RUNNING: usize = 1;
const LOADER_WORKER_RESULT_READY: usize = 2;
const LOADER_WORKER_COMPLETE: usize = 3;
const LOADER_WORKER_EXITED: usize = 4;
const LOADER_WORKER_STACK_BYTES: usize = 64 * 1024;

static LOADER_WORKER_STATE: AtomicUsize = AtomicUsize::new(LOADER_WORKER_IDLE);
static LOADER_WORKER_ENDPOINT: AtomicU64 = AtomicU64::new(0);
static LOADER_WORKER_LEASES_PTR: AtomicUsize = AtomicUsize::new(0);
static LOADER_WORKER_LEASES_LEN: AtomicUsize = AtomicUsize::new(0);
static LOADER_WORKER_POST_INIT_PTR: AtomicUsize = AtomicUsize::new(0);
static LOADER_WORKER_POST_INIT_LEN: AtomicUsize = AtomicUsize::new(0);
static LOADER_WORKER_CHECKPOINTS_PTR: AtomicUsize = AtomicUsize::new(0);
static LOADER_WORKER_RESULT: AtomicI64 = AtomicI64::new(0);

#[repr(align(16))]
struct LoaderWorkerStack([u8; LOADER_WORKER_STACK_BYTES]);

#[cfg(not(test))]
static mut LOADER_WORKER_STACK: LoaderWorkerStack =
    LoaderWorkerStack([0; LOADER_WORKER_STACK_BYTES]);

#[cfg(not(test))]
const ROOTD_HEAP_BYTES: usize = 8 * 1024 * 1024;

#[cfg(not(test))]
#[repr(align(4096))]
struct RootdHeap([u8; ROOTD_HEAP_BYTES]);

#[cfg(not(test))]
static mut ROOTD_HEAP: RootdHeap = RootdHeap([0; ROOTD_HEAP_BYTES]);

#[derive(Clone, Copy)]
struct Lease {
    service_id: u64,
    exec_path: &'static [u8],
    pid: u64,
    restart_budget: u32,
    backoff_ms: u32,
    state: u16,
    exit_status: i32,
    weight_micros: u64,
    readiness_polls_remaining: u32,
}

#[derive(Clone, Copy)]
struct PostInitLease {
    service_id: u64,
    exec_path: &'static [u8],
    pid: u64,
    reporter_pid: u64,
    state: u16,
    exit_status: i32,
}

#[cfg(not(test))]
core::arch::global_asm!(
    ".global _start",
    ".type _start, @function",
    "_start:",
    "    xor rbp, rbp",
    "    and rsp, -16",
    "    call __rustos_rootd_start",
    "    ud2",
);

#[cfg(not(test))]
#[no_mangle]
pub extern "C" fn __rustos_rootd_start() -> ! {
    unsafe {
        rustos_svc_runtime::allocator::init(rustos_svc_runtime::BootstrapHeap {
            base: core::ptr::addr_of_mut!(ROOTD_HEAP.0).cast::<u8>() as usize,
            len: ROOTD_HEAP_BYTES,
        });
    }
    debug_line(b"rootd: bootstrap enter\n");
    let endpoint = create_rootd_endpoint();
    let mut leases = [
        lease(BOOTSTRAP_MANIFEST[0]),
        lease(BOOTSTRAP_MANIFEST[1]),
        lease(BOOTSTRAP_MANIFEST[2]),
        lease(BOOTSTRAP_MANIFEST[3]),
        lease(BOOTSTRAP_MANIFEST[4]),
    ];
    let mut post_init_leases = [
        post_init_lease(POST_INIT_MANIFEST[0]),
        post_init_lease(POST_INIT_MANIFEST[1]),
        post_init_lease(POST_INIT_MANIFEST[2]),
        post_init_lease(POST_INIT_MANIFEST[3]),
        post_init_lease(POST_INIT_MANIFEST[4]),
        post_init_lease(POST_INIT_MANIFEST[5]),
    ];
    let mut service_checkpoints = ServiceCheckpointStore::new();

    // The bootstrap manifest is the rootd-owned policy boundary for direct
    // early spawn, dependency readiness, restart fallback, and advertised
    // supervisor descriptors.
    for index in 0..leases.len() {
        if BOOTSTRAP_MANIFEST[index].bootstrap_direct {
            match index {
                0 => debug_line(b"rootd: spawn syscalld\n"),
                1 => debug_line(b"rootd: spawn vfsd\n"),
                2 => debug_line(b"rootd: spawn loaderd\n"),
                3 => debug_line(b"rootd: spawn procd\n"),
                _ => {}
            }
            spawn_core_service_without_wait(&mut leases[index]);
        }
    }

    debug_line(b"rootd: core services spawned, waiting for readiness\n");
    while !service_dependencies_ready(INITD_LEASE_INDEX) {
        drain_lifecycle_events(&mut leases, &mut post_init_leases);
        let mut served = 0;
        while served < ROOTD_REQUEST_DRAIN_BUDGET
            && serve_rootd_once(
                endpoint,
                &leases,
                &mut post_init_leases,
                &mut service_checkpoints,
                false,
            )
        {
            served += 1;
        }
        restart_failed_leases(
            endpoint,
            &mut leases,
            &mut post_init_leases,
            &mut service_checkpoints,
        );
        supervise_core_readiness(&mut leases);
        if !service_dependencies_ready(INITD_LEASE_INDEX) && served == 0 {
            wait_for_restart_backoff(CORE_READINESS_POLL_INTERVAL_MS);
        } else if !service_dependencies_ready(INITD_LEASE_INDEX) {
            yield_now();
        }
    }

    debug_line(b"rootd: core services ready, spawning initd via loaderd\n");
    let _ = syscall3(
        SYS_RUSTOS_PRODUCT_MILESTONE,
        PRODUCT_MILESTONE_ROOT_CORE_READY,
        0,
        0,
    );
    spawn_initd_via_loaderd(
        endpoint,
        &mut leases,
        &mut post_init_leases,
        &mut service_checkpoints,
    );

    debug_line(b"rootd: initd spawned\n");
    yield_now();
    loop {
        drain_lifecycle_events(&mut leases, &mut post_init_leases);
        let _ = serve_rootd_once(
            endpoint,
            &leases,
            &mut post_init_leases,
            &mut service_checkpoints,
            false,
        );
        restart_failed_leases(
            endpoint,
            &mut leases,
            &mut post_init_leases,
            &mut service_checkpoints,
        );
        supervisor_idle();
    }
}

fn lease(spec: BootstrapServiceSpec) -> Lease {
    Lease {
        service_id: spec.service_id,
        exec_path: spec.exec_path,
        pid: 0,
        restart_budget: 3,
        backoff_ms: 250,
        state: rustos_user_abi::syscall::ROOTD_LEASE_STATE_EMPTY,
        exit_status: 0,
        weight_micros: spec.weight_micros,
        readiness_polls_remaining: CORE_READINESS_POLL_MAX,
    }
}

fn post_init_lease(spec: PostInitServiceSpec) -> PostInitLease {
    PostInitLease {
        service_id: spec.service_id,
        exec_path: spec.exec_path,
        pid: 0,
        reporter_pid: 0,
        state: rustos_user_abi::syscall::ROOTD_LEASE_STATE_EMPTY,
        exit_status: 0,
    }
}

fn create_rootd_endpoint() -> u64 {
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        fail_closed(b"rootd: fatal endpoint create failed\n");
    }
    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_ROOTD,
        endpoint as u64,
    );
    if register < 0 {
        fail_closed(b"rootd: fatal endpoint register failed\n");
    }
    debug_line(b"rootd: supervisor endpoint registered\n");
    endpoint as u64
}

fn spawn_core_service_without_wait(lease: &mut Lease) {
    if service_ready(lease.service_id) {
        lease.state = ROOTD_LEASE_STATE_RUNNING;
        return;
    }
    if spawn_tracked_with_attempts(lease, BOOTSTRAP_SPAWN_MAX_ATTEMPTS).is_err() {
        fail_closed(b"rootd: fatal bootstrap service spawn failed\n");
    }
}

fn spawn_tracked_with_attempts(lease: &mut Lease, max_attempts: u32) -> Result<(), i64> {
    let mut last_errno = 11;
    let mut attempt = 0;
    while attempt < max_attempts {
        match spawn_exec(lease.exec_path, lease.weight_micros) {
            Ok(pid) => {
                lease.pid = pid;
                lease.state = ROOTD_LEASE_STATE_RUNNING;
                lease.exit_status = 0;
                lease.readiness_polls_remaining = CORE_READINESS_POLL_MAX;
                return Ok(());
            }
            Err(errno) => {
                last_errno = errno;
                yield_now();
            }
        }
        attempt += 1;
    }
    Err(last_errno)
}

fn spawn_exec(path: &'static [u8], weight_micros: u64) -> Result<u64, i64> {
    let argv = [path.as_ptr(), core::ptr::null()];
    let result = syscall6(
        SYS_RUSTOS_SPAWN_EXEC,
        path.as_ptr() as u64,
        argv.as_ptr() as u64,
        0,
        SPAWN_FLAG_LOGICAL_ADMIN,
        0,
        weight_micros,
    );
    if result < 0 {
        Err(-result)
    } else {
        Ok(result as u64)
    }
}

fn spawn_initd_via_loaderd(
    endpoint: u64,
    leases: &mut [Lease],
    post_init_leases: &mut [PostInitLease],
    service_checkpoints: &mut ServiceCheckpointStore,
) {
    let mut attempts = 0_u64;
    while attempts < INITD_SPAWN_MAX_ATTEMPTS as u64 {
        debug_line(b"rootd: initd spawn request to loaderd\n");
        let initd_path = leases[INITD_LEASE_INDEX].exec_path;
        let initd_weight = leases[INITD_LEASE_INDEX].weight_micros;
        match spawn_exec_via_loaderd_cooperative(
            endpoint,
            leases,
            post_init_leases,
            service_checkpoints,
            initd_path,
            initd_weight,
        ) {
            Ok(pid) => {
                debug_line(b"rootd: initd loader spawn complete\n");
                leases[INITD_LEASE_INDEX].pid = pid;
                leases[INITD_LEASE_INDEX].state = ROOTD_LEASE_STATE_RUNNING;
                leases[INITD_LEASE_INDEX].exit_status = 0;
                if activate_exec_via_loaderd(pid).is_ok() {
                    debug_line(b"rootd: initd activated\n");
                    return;
                }
                cleanup_failed_initial_activation(
                    &mut leases[INITD_LEASE_INDEX],
                    terminate_service_process,
                )
                .unwrap_or_else(|_| {
                    fail_closed(b"rootd: fatal initial-activation child cleanup rejected\n")
                });
                fail_closed(b"rootd: fatal initd activation failed\n");
            }
            Err(_) => {
                attempts = attempts.saturating_add(1);
                if attempts <= 8 || attempts.is_power_of_two() {
                    debug_line(b"rootd: initd loader spawn retry\n");
                }
                drain_lifecycle_events(leases, post_init_leases);
                serve_rootd_once(
                    endpoint,
                    leases,
                    post_init_leases,
                    service_checkpoints,
                    false,
                );
                restart_failed_leases(endpoint, leases, post_init_leases, service_checkpoints);
                yield_now();
            }
        }
    }
    fail_closed(b"rootd: fatal initd spawn attempts exhausted\n");
}

fn service_ready(service_id: u64) -> bool {
    if service_id == INITD_LEASE_ID {
        return false;
    }
    syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, service_id) > 0
}

fn readiness_timeout_due(lease: &mut Lease, ready: bool) -> bool {
    if ready {
        lease.readiness_polls_remaining = CORE_READINESS_POLL_MAX;
        return false;
    }
    if lease.state != ROOTD_LEASE_STATE_RUNNING || lease.pid == 0 {
        return false;
    }
    if lease.readiness_polls_remaining == 0 {
        return true;
    }
    lease.readiness_polls_remaining -= 1;
    false
}

fn supervise_core_readiness(leases: &mut [Lease]) {
    for lease in leases.iter_mut().take(INITD_LEASE_INDEX) {
        let ready = service_ready(lease.service_id);
        if !readiness_timeout_due(lease, ready) {
            continue;
        }
        if terminate_service_process(lease.pid).is_err() {
            fail_closed(b"rootd: fatal unready core-service cleanup rejected\n");
        }
        lease.state = ROOTD_LEASE_STATE_EXITED;
        lease.exit_status = -110;
        debug_line(b"rootd: core service readiness timed out\n");
    }
}

fn service_dependencies_ready(index: usize) -> bool {
    let mask = BOOTSTRAP_MANIFEST[index].dependency_mask;
    (mask & DEP_SYSCALLD == 0 || service_ready(IPC_SERVICE_LINUX_SYSCALLD))
        && (mask & DEP_VFSD == 0 || service_ready(IPC_SERVICE_VFSD))
        && (mask & DEP_LOADERD == 0 || service_ready(IPC_SERVICE_LOADERD))
        && (mask & DEP_PROCD == 0 || service_ready(IPC_SERVICE_PROCD))
        && (mask & DEP_PAGERD == 0 || service_ready(IPC_SERVICE_PAGERD))
}

fn drain_lifecycle_events(leases: &mut [Lease], post_init_leases: &mut [PostInitLease]) {
    let mut events = [LifecycleEventWire::default(); LIFECYCLE_DRAIN_MAX_EVENTS];
    let mut count = 0_u32;
    let args = LifecycleDrainBrokerArgs {
        abi_version: LIFECYCLE_DRAIN_BROKER_ABI_VERSION,
        reserved0: 0,
        reserved1: 0,
        out_events_ptr: events.as_mut_ptr() as u64,
        out_capacity: events.len() as u32,
        reserved2: 0,
        out_count_ptr: (&mut count as *mut u32) as u64,
    };
    if syscall1(
        SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER,
        (&args as *const LifecycleDrainBrokerArgs) as u64,
    ) < 0
    {
        fail_closed(b"rootd: fatal lifecycle evidence drain rejected\n");
    }
    for event in events.iter().take(count as usize) {
        if event.event != LIFECYCLE_EVENT_EXIT {
            continue;
        }
        for lease in leases.iter_mut() {
            if lease.pid == event.pid {
                lease.state = ROOTD_LEASE_STATE_EXITED;
                lease.exit_status = event.exit_status;
                break;
            }
        }
        for lease in post_init_leases.iter_mut() {
            if lease.pid == event.pid {
                lease.state = ROOTD_LEASE_STATE_EXITED;
                lease.exit_status = event.exit_status;
                break;
            }
        }
        revoke_post_init_dependents(post_init_leases, event.pid);
    }
}

/// A running child inherits supervisor admission only while that supervisor is
/// alive.  In particular, uiserver must not keep SESSION_POLICY-derived
/// authority after the session service that reported it has exited.
fn revoke_post_init_dependents(post_init_leases: &mut [PostInitLease], reporter_pid: u64) {
    if reporter_pid == 0 {
        return;
    }
    revoke_post_init_dependents_with(
        post_init_leases,
        reporter_pid,
        revoke_service_endpoint,
        terminate_service_process,
    )
    .unwrap_or_else(|_| {
        fail_closed(b"rootd: fatal dependent post-init lease termination rejected\n")
    });
}

fn revoke_post_init_dependents_with(
    post_init_leases: &mut [PostInitLease],
    reporter_pid: u64,
    mut revoke_endpoint: impl FnMut(u64) -> Result<(), i32>,
    mut terminate: impl FnMut(u64) -> Result<(), i32>,
) -> Result<(), i32> {
    // The graph is fixed and acyclic by admission policy: initd reports the
    // post-init services, and sessiond alone may report uiserver. Revoke the
    // complete descendant closure in this rootd turn so a child cannot retain
    // capability authority until its parent's later lifecycle notification.
    let mut reporters = [0_u64; POST_INIT_MANIFEST.len() + 1];
    reporters[0] = reporter_pid;
    let mut reporter_count = 1_usize;
    let mut cursor = 0_usize;
    while cursor < reporter_count {
        let current_reporter = reporters[cursor];
        cursor += 1;
        for lease in post_init_leases.iter_mut() {
            if lease.reporter_pid != current_reporter || lease.state != ROOTD_LEASE_STATE_RUNNING {
                continue;
            }
            let child_pid = lease.pid;
            // Endpoint revocation is the authority linearization point. A
            // terminate request may return before the scheduler completes
            // process teardown, so waiting for exit cleanup would leave a
            // stale cached capability window.
            revoke_endpoint(lease.service_id)?;
            terminate(child_pid)?;
            lease.state = ROOTD_LEASE_STATE_EXITED;
            lease.exit_status = 9;
            if child_pid != 0 && !reporters[..reporter_count].contains(&child_pid) {
                if reporter_count == reporters.len() {
                    return Err(75);
                }
                reporters[reporter_count] = child_pid;
                reporter_count += 1;
            }
        }
    }
    Ok(())
}

fn revoke_service_endpoint(service_id: u64) -> Result<(), i32> {
    let status = syscall2(SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, service_id, 0);
    if status < 0 {
        Err((-status) as i32)
    } else {
        Ok(())
    }
}

fn serve_rootd_once(
    endpoint: u64,
    leases: &[Lease],
    post_init_leases: &mut [PostInitLease],
    service_checkpoints: &mut ServiceCheckpointStore,
    blocking: bool,
) -> bool {
    if endpoint == 0 {
        fail_closed(b"rootd: fatal missing supervisor endpoint\n");
    }
    let mut request = CommercialMaxProtocolRequest::default();
    let mut reply_cap = 0_u64;
    let mut sender_pid = 0_u64;
    let mut sender_tid = 0_u64;
    let recv_syscall = if blocking {
        SYS_RUSTOS_IPC_RECV_WITH_SENDER
    } else {
        SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER
    };
    let received = syscall6(
        recv_syscall,
        endpoint,
        (&mut request as *mut CommercialMaxProtocolRequest) as u64,
        size_of::<CommercialMaxProtocolRequest>() as u64,
        (&mut reply_cap as *mut u64) as u64,
        (&mut sender_pid as *mut u64) as u64,
        (&mut sender_tid as *mut u64) as u64,
    );
    if received < 0 {
        if blocking {
            fail_closed(b"rootd: fatal supervisor blocking recv failed\n");
        }
        return false;
    }
    if received as usize != size_of::<CommercialMaxProtocolRequest>() {
        reply_commercial_max_error(reply_cap, &request, 22);
        return true;
    }
    reply_commercial_max_request(
        reply_cap,
        &request,
        leases,
        post_init_leases,
        service_checkpoints,
        IpcSenderIdentity {
            pid: sender_pid,
            tid: sender_tid,
        },
    );
    true
}

fn reply_commercial_max_error(reply_cap: u64, request: &CommercialMaxProtocolRequest, status: i32) {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        status,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    let _ = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
}

fn reply_commercial_max_request(
    reply_cap: u64,
    request: &CommercialMaxProtocolRequest,
    leases: &[Lease],
    post_init_leases: &mut [PostInitLease],
    service_checkpoints: &mut ServiceCheckpointStore,
    sender: IpcSenderIdentity,
) {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        value0: leases.len() as u64,
        ..CommercialMaxProtocolResponse::default()
    };
    if request.header.op == COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL {
        debug_line(b"rootd: readiness request received\n");
    }
    if request.header.op == COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY {
        debug_line(b"rootd: service capability request received\n");
    }
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = match validate_commercial_max_request(request, sender) {
        Ok(()) => match fill_commercial_max_response(
            request,
            leases,
            post_init_leases,
            service_checkpoints,
            &mut response,
            sender,
        ) {
            Ok(()) => 0,
            Err(errno) => errno,
        },
        Err(errno) => errno,
    };
    if request.header.op == COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY && response.status != 0 {
        debug_line(b"rootd: service capability denied\n");
    }
    if request.header.op == COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL && response.status != 0 {
        debug_line(b"rootd: readiness denied\n");
    }
    let replied = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
    if request.header.op == COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL && replied < 0 {
        debug_line(b"rootd: readiness reply failed\n");
    }
    if request.header.op == COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL
        && response.status == 0
        && replied >= 0
    {
        debug_line(b"rootd: readiness replied ok\n");
    }
    if request.header.op == COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY && replied < 0 {
        debug_line(b"rootd: service capability reply failed\n");
    }
    if request.header.op == COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY
        && response.status == 0
        && replied >= 0
    {
        debug_line(b"rootd: service capability replied ok\n");
    }
}

fn validate_commercial_max_request(
    request: &CommercialMaxProtocolRequest,
    sender: IpcSenderIdentity,
) -> Result<(), i32> {
    if !request.has_valid_envelope() {
        return Err(22);
    }
    validate_sender_subject(request, sender)?;
    match request.header.protocol {
        COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR => match request.header.op {
            COMMERCIAL_MAX_ROOTD_OP_BOOTSTRAP_MANIFEST
            | COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH => {
                validate_empty_request_body(request, false)
            }
            COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE | COMMERCIAL_MAX_ROOTD_OP_RESTART_POLICY => {
                validate_empty_request_body(request, true)
            }
            COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL
            | COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY
            | COMMERCIAL_MAX_ROOTD_OP_SERVICE_LOOKUP
            | COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_QUERY
            | COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_RECLAIM
            | COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_MUTATE
            | COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_COMPACT
            | COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_SCAN
            | COMMERCIAL_MAX_ROOTD_OP_LOADER_WORKER_COMPLETE => Ok(()),
            _ => Err(22),
        },
        COMMERCIAL_MAX_PROTOCOL_CAPABILITY => match request.header.op {
            COMMERCIAL_MAX_CAPABILITY_OP_LEASE_GRANT
            | COMMERCIAL_MAX_CAPABILITY_OP_LEASE_REVOKE
            | COMMERCIAL_MAX_CAPABILITY_OP_LEASE_RENEW => {
                if request.path_len != 0
                    || request.payload_len != 0
                    || request.arg2 != 0
                    || request.arg3 != 0
                    || request.arg0 == 0 && request.arg1 != 0
                {
                    Err(22)
                } else {
                    Ok(())
                }
            }
            _ => Err(22),
        },
        _ => Err(22),
    }
}

fn validate_empty_request_body(
    request: &CommercialMaxProtocolRequest,
    allow_arg0: bool,
) -> Result<(), i32> {
    if request.path_len != 0
        || request.payload_len != 0
        || (!allow_arg0 && request.arg0 != 0)
        || request.arg1 != 0
        || request.arg2 != 0
        || request.arg3 != 0
    {
        Err(22)
    } else {
        Ok(())
    }
}

fn validate_sender_subject(
    request: &CommercialMaxProtocolRequest,
    sender: IpcSenderIdentity,
) -> Result<(), i32> {
    if request.header.subject_pid == 0
        || request.header.subject_tid == 0
        || sender.pid == 0
        || sender.tid == 0
    {
        return Err(22);
    }
    if request.header.subject_pid != sender.pid || request.header.subject_tid != sender.tid {
        return Err(13);
    }
    Ok(())
}

fn fill_commercial_max_response(
    request: &CommercialMaxProtocolRequest,
    leases: &[Lease],
    post_init_leases: &mut [PostInitLease],
    service_checkpoints: &mut ServiceCheckpointStore,
    response: &mut CommercialMaxProtocolResponse,
    sender: IpcSenderIdentity,
) -> Result<(), i32> {
    if request.header.protocol == COMMERCIAL_MAX_PROTOCOL_CAPABILITY {
        return fill_capability_response(request, leases, response);
    }
    match request.header.op {
        COMMERCIAL_MAX_ROOTD_OP_BOOTSTRAP_MANIFEST => {
            fill_manifest_descriptors(leases, response);
            response.payload_len = write_manifest_payload(leases, &mut response.payload) as u32;
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE => {
            let lease = lease_by_index(leases, request.arg0 as usize)?;
            response.descriptor_count = 1;
            response.descriptors[0] = lease_descriptor(lease, request.header.op, 0);
            response.capability = lease_capability(lease, request.header.op);
            response.payload_len = write_payload_struct(&lease_wire(lease), &mut response.payload);
            response.value1 = lease.pid;
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH => {
            fill_dependency_graph(leases, response);
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_RESTART_POLICY => {
            let lease = lease_by_index(leases, request.arg0 as usize)?;
            response.descriptor_count = 1;
            response.descriptors[0] = lease_descriptor(lease, request.header.op, 0);
            response.value0 = lease.restart_budget as u64;
            response.value1 = lease.backoff_ms as u64;
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL => {
            register_post_init_service_lease(leases, post_init_leases, request, sender)?;
            response.value0 = request.arg0;
            response.value1 = request.arg1;
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY => {
            let capability = service_capability_for_subject(leases, post_init_leases, request)?;
            response.value0 = capability;
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_SERVICE_LOOKUP => {
            authorize_service_lookup_for_subject(leases, post_init_leases, request)?;
            response.value0 = request.arg0;
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_LOADER_WORKER_COMPLETE => {
            complete_loader_worker(request, sender)?;
            response.value0 = 1;
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_QUERY => {
            let lease = query_post_init_lease(leases, post_init_leases, request, sender)?;
            response.value0 = lease.pid;
            response.value1 = u64::from(lease.state);
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_RECLAIM => {
            reclaim_post_init_lease(leases, post_init_leases, request, sender)?;
            response.value0 = request.arg0;
            response.value1 = 0;
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_MUTATE => {
            authorize_current_service_namespace(leases, post_init_leases, request.arg0, sender)?;
            if request.path_len != 0
                || request.payload_len as usize != size_of::<ServiceCheckpointRecordWire>()
                || request.arg1 != 0
                || request.arg2 != 0
                || request.arg3 != 0
            {
                return Err(22);
            }
            let record = unsafe {
                core::ptr::read_unaligned(
                    request
                        .payload
                        .as_ptr()
                        .cast::<ServiceCheckpointRecordWire>(),
                )
            };
            let duplicate = service_checkpoints.mutate(request.arg0, record)?;
            response.value0 = record.revision;
            response.value1 = u64::from(duplicate);
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_COMPACT => {
            authorize_current_service_namespace(leases, post_init_leases, request.arg0, sender)?;
            if request.path_len != 0
                || request.payload_len as usize != size_of::<ServiceCheckpointRecordWire>()
                || request.arg1 != 0
                || request.arg2 != 0
                || request.arg3 != 0
            {
                return Err(22);
            }
            let proof = unsafe {
                core::ptr::read_unaligned(
                    request
                        .payload
                        .as_ptr()
                        .cast::<ServiceCheckpointRecordWire>(),
                )
            };
            let already_compacted = service_checkpoints.compact(request.arg0, proof)?;
            response.value0 = proof.revision;
            response.value1 = u64::from(already_compacted);
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_SCAN => {
            authorize_current_service_namespace(leases, post_init_leases, request.arg0, sender)?;
            if request.path_len != 0
                || request.payload_len != 0
                || request.arg2 != 0
                || request.arg3 != 0
            {
                return Err(22);
            }
            let wire_size = size_of::<ServiceCheckpointRecordWire>();
            let max = response.payload.len() / wire_size;
            let cursor = usize::try_from(request.arg1).map_err(|_| 22)?;
            let (records, next) = service_checkpoints.scan(request.arg0, cursor, max)?;
            for (index, record) in records.iter().enumerate() {
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        (record as *const ServiceCheckpointRecordWire).cast::<u8>(),
                        wire_size,
                    )
                };
                let offset = index * wire_size;
                response.payload[offset..offset + wire_size].copy_from_slice(bytes);
            }
            response.value0 = next as u64;
            response.value1 = records.len() as u64;
            response.payload_len = (records.len() * wire_size) as u32;
            Ok(())
        }
        _ => Err(22),
    }
}

fn complete_loader_worker(
    request: &CommercialMaxProtocolRequest,
    sender: IpcSenderIdentity,
) -> Result<(), i32> {
    if request.path_len != 0
        || request.payload_len != 0
        || request.arg0 != 0
        || request.arg1 != 0
        || request.arg2 != 0
        || request.arg3 != 0
    {
        return Err(22);
    }
    let rootd_pid = syscall0(SYS_GETPID);
    if rootd_pid <= 0 || sender.pid != rootd_pid as u64 {
        return Err(13);
    }
    LOADER_WORKER_STATE
        .compare_exchange(
            LOADER_WORKER_RESULT_READY,
            LOADER_WORKER_COMPLETE,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|_| 16)
}

fn fill_capability_response(
    request: &CommercialMaxProtocolRequest,
    leases: &[Lease],
    response: &mut CommercialMaxProtocolResponse,
) -> Result<(), i32> {
    match request.header.op {
        COMMERCIAL_MAX_CAPABILITY_OP_LEASE_GRANT => {
            if request.arg0 == 0 {
                fill_capability_descriptors(leases, response);
                return Ok(());
            }
            let lease = lease_by_service_or_index(leases, request.arg0, request.arg1 as usize)?;
            response.descriptor_count = 1;
            response.descriptors[0] = capability_descriptor(lease, request.header.op, 0);
            response.capability = service_capability_lease(lease);
            response.value0 = service_policy_capability(lease.service_id);
            response.value1 = lease.pid;
            Ok(())
        }
        COMMERCIAL_MAX_CAPABILITY_OP_LEASE_REVOKE => {
            let lease = lease_by_service_or_index(leases, request.arg0, request.arg1 as usize)?;
            response.descriptor_count = 1;
            response.descriptors[0] = capability_descriptor(lease, request.header.op, 0);
            response.value0 = u64::from(lease.state == ROOTD_LEASE_STATE_FAILED);
            response.value1 = lease.pid;
            Ok(())
        }
        COMMERCIAL_MAX_CAPABILITY_OP_LEASE_RENEW => {
            let lease = lease_by_service_or_index(leases, request.arg0, request.arg1 as usize)?;
            response.descriptor_count = 1;
            response.descriptors[0] = capability_descriptor(lease, request.header.op, 0);
            response.capability = service_capability_lease(lease);
            response.value0 = service_policy_capability(lease.service_id);
            response.value1 = u64::from(lease.state == ROOTD_LEASE_STATE_RUNNING);
            Ok(())
        }
        _ => Err(22),
    }
}

fn lease_wire(lease: &Lease) -> CoreServiceLeaseWire {
    let mut wire = CoreServiceLeaseWire {
        service_id: lease.service_id,
        pid: lease.pid,
        restart_budget: lease.restart_budget,
        backoff_ms: lease.backoff_ms,
        state: lease.state,
        exit_status: lease.exit_status,
        ..CoreServiceLeaseWire::default()
    };
    let path = trim_nul(lease.exec_path);
    let len = path.len().min(wire.exec_path.len());
    wire.exec_path_len = len as u32;
    wire.exec_path[..len].copy_from_slice(&path[..len]);
    wire
}

fn lease_by_index(leases: &[Lease], index: usize) -> Result<&Lease, i32> {
    if index < leases.len() {
        Ok(&leases[index])
    } else {
        Err(34)
    }
}

fn lease_by_service_or_index(
    leases: &[Lease],
    service_id: u64,
    fallback_index: usize,
) -> Result<&Lease, i32> {
    if service_id != 0 {
        for lease in leases {
            if lease.service_id == service_id {
                return Ok(lease);
            }
        }
        return Err(34);
    }
    lease_by_index(leases, fallback_index)
}

fn fill_manifest_descriptors(leases: &[Lease], response: &mut CommercialMaxProtocolResponse) {
    let count = leases.len().min(COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS);
    response.descriptor_count = count as u16;
    let mut index = 0usize;
    while index < count {
        let lease = &leases[index];
        response.descriptors[index] = lease_descriptor(
            lease,
            COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE,
            index as u64,
        );
        index += 1;
    }
}

fn fill_dependency_graph(leases: &[Lease], response: &mut CommercialMaxProtocolResponse) {
    let count = leases.len().min(COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS);
    response.descriptor_count = count as u16;
    let mut index = 0usize;
    while index < count {
        let lease = &leases[index];
        let mut descriptor = lease_descriptor(
            lease,
            COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH,
            index as u64,
        );
        descriptor.value1 = BOOTSTRAP_MANIFEST[index].dependency_mask as u64;
        response.descriptors[index] = descriptor;
        index += 1;
    }
}

fn fill_capability_descriptors(leases: &[Lease], response: &mut CommercialMaxProtocolResponse) {
    let count = leases.len().min(COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS);
    response.descriptor_count = count as u16;
    let mut index = 0usize;
    while index < count {
        response.descriptors[index] = capability_descriptor(
            &leases[index],
            COMMERCIAL_MAX_CAPABILITY_OP_LEASE_GRANT,
            index as u64,
        );
        index += 1;
    }
}

fn lease_descriptor(lease: &Lease, op: u16, index: u64) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
        op,
        service_id: lease.service_id,
        capability_mask: rootd_capability_mask(op),
        value0: index,
        value1: lease.pid,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    let name = service_name(lease.exec_path);
    copy_label(name, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn lease_capability(lease: &Lease, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: lease.pid,
        service_id: lease.service_id,
        subject_pid: lease.pid,
        capability_mask: rootd_capability_mask(op),
        rights_mask: rootd_capability_mask(op),
        generation: lease.pid,
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    let name = service_name(lease.exec_path);
    copy_label(name, &mut capability.label, &mut capability.label_len);
    capability
}

fn capability_descriptor(
    lease: &Lease,
    op: u16,
    index: u64,
) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_CAPABILITY,
        op,
        service_id: lease.service_id,
        capability_mask: service_policy_capability(lease.service_id),
        value0: index,
        value1: lease.pid,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    let name = service_name(lease.exec_path);
    copy_label(name, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn service_capability_lease(lease: &Lease) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_CAPABILITY as u64) << 32) | lease.service_id,
        service_id: lease.service_id,
        subject_pid: lease.pid,
        capability_mask: service_policy_capability(lease.service_id),
        rights_mask: service_policy_capability(lease.service_id),
        generation: lease.pid,
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    let name = service_name(lease.exec_path);
    copy_label(name, &mut capability.label, &mut capability.label_len);
    capability
}

fn rootd_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_ROOTD_OP_BOOTSTRAP_MANIFEST => 1 << 0,
        COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE => 1 << 1,
        COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH => 1 << 2,
        COMMERCIAL_MAX_ROOTD_OP_RESTART_POLICY => 1 << 3,
        COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL => 1 << 4,
        COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY => 1 << 5,
        COMMERCIAL_MAX_ROOTD_OP_SERVICE_LOOKUP => 1 << 6,
        COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_QUERY => 1 << 7,
        COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_RECLAIM => 1 << 8,
        COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_MUTATE => 1 << 9,
        COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_SCAN => 1 << 10,
        COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_COMPACT => 1 << 11,
        _ => 0,
    }
}

fn service_policy_capability(service_id: u64) -> u64 {
    match service_id {
        IPC_SERVICE_LINUX_SYSCALLD => {
            rustos_user_abi::syscall::IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY
        }
        IPC_SERVICE_VFSD => rustos_user_abi::syscall::IPC_SERVICE_CAP_VFS_POLICY,
        IPC_SERVICE_NETD => rustos_user_abi::syscall::IPC_SERVICE_CAP_NET_POLICY,
        IPC_SERVICE_DEVMGRD => rustos_user_abi::syscall::IPC_SERVICE_CAP_DEVICE_POLICY,
        IPC_SERVICE_LOADERD => rustos_user_abi::syscall::IPC_SERVICE_CAP_PROCESS_LOADER,
        IPC_SERVICE_STORAGED => rustos_user_abi::syscall::IPC_SERVICE_CAP_STORAGE_POLICY,
        IPC_SERVICE_INPUTD => rustos_user_abi::syscall::IPC_SERVICE_CAP_INPUT_POLICY,
        IPC_SERVICE_PROCD => rustos_user_abi::syscall::IPC_SERVICE_CAP_PROCESS_POLICY,
        IPC_SERVICE_ROOTD => rustos_user_abi::syscall::IPC_SERVICE_CAP_ROOT_SUPERVISOR,
        IPC_SERVICE_SESSIOND => rustos_user_abi::syscall::IPC_SERVICE_CAP_SESSION_POLICY,
        IPC_SERVICE_PAGERD => rustos_user_abi::syscall::IPC_SERVICE_CAP_PAGER_POLICY,
        IPC_SERVICE_UISERVER => rustos_user_abi::syscall::IPC_SERVICE_CAP_UI_POLICY,
        INITD_LEASE_ID => rustos_user_abi::syscall::IPC_SERVICE_CAP_INIT_POLICY,
        _ => 0,
    }
}

fn service_capability_for_subject(
    leases: &[Lease],
    post_init_leases: &mut [PostInitLease],
    request: &CommercialMaxProtocolRequest,
) -> Result<u64, i32> {
    if request.arg0 == 0
        || request.arg2 != 0
        || request.arg3 != 0
        || request.path_len != 0
        || request.payload_len != 0
        || request.header.subject_pid == 0
        || request.header.subject_tid == 0
    {
        return Err(22);
    }
    if let Some(capability) = post_init_reported_service_capability(
        leases,
        post_init_leases,
        request.arg0,
        request.header.subject_pid,
    ) {
        return Ok(capability);
    }
    if request.arg0 == IPC_SERVICE_PAGERD {
        let syscalld = lease_by_service_or_index(leases, IPC_SERVICE_LINUX_SYSCALLD, 0)?;
        if syscalld.pid == request.header.subject_pid && syscalld.state == ROOTD_LEASE_STATE_RUNNING
        {
            return Ok(rustos_user_abi::syscall::IPC_SERVICE_CAP_PAGER_POLICY);
        }
        return Err(13);
    }
    let lease = lease_by_service_or_index(leases, request.arg0, request.arg1 as usize)?;
    if lease.pid != request.header.subject_pid || lease.state != ROOTD_LEASE_STATE_RUNNING {
        return Err(13);
    }
    let capability = service_policy_capability(lease.service_id);
    if capability == 0 {
        return Err(22);
    }
    Ok(capability)
}

fn register_post_init_service_lease(
    leases: &[Lease],
    post_init_leases: &mut [PostInitLease],
    request: &CommercialMaxProtocolRequest,
    sender: IpcSenderIdentity,
) -> Result<(), i32> {
    validate_deferred_spawn_provenance(request.arg1, sender.pid)?;
    debug_line(b"rootd: post-init deferred-spawn provenance verified\n");
    register_post_init_service_lease_with_provenance(
        leases,
        post_init_leases,
        request,
        sender,
        true,
    )
}

fn validate_deferred_spawn_provenance(target_pid: u64, requester_pid: u64) -> Result<(), i32> {
    let args = RustosProcValidateDeferredSpawnBrokerArgs {
        abi_version: rustos_user_abi::syscall::PROC_BROKER_ABI_VERSION,
        target_pid,
        requester_pid,
        ..RustosProcValidateDeferredSpawnBrokerArgs::default()
    };
    let status = syscall1(
        SYS_RUSTOS_PROC_VALIDATE_DEFERRED_SPAWN_BROKER,
        (&args as *const RustosProcValidateDeferredSpawnBrokerArgs) as u64,
    );
    if status < 0 {
        Err((-status) as i32)
    } else if status == 0 {
        Ok(())
    } else {
        Err(5)
    }
}

fn register_post_init_service_lease_with_provenance(
    leases: &[Lease],
    post_init_leases: &mut [PostInitLease],
    request: &CommercialMaxProtocolRequest,
    sender: IpcSenderIdentity,
    provenance_proven: bool,
) -> Result<(), i32> {
    if !provenance_proven {
        return Err(13);
    }
    if request.arg0 == 0
        || request.arg1 == 0
        || request.arg2 != 0
        || request.arg3 != 0
        || request.payload_len != 0
    {
        return Err(22);
    }
    authorize_post_init_lease_reporter(leases, post_init_leases, request.arg0, sender)?;
    let Some(lease) = post_init_leases
        .iter_mut()
        .find(|lease| lease.service_id == request.arg0)
    else {
        return Err(22);
    };
    if request.path_len == 0 || request.path_len as usize > request.path.len() {
        return Err(22);
    }
    let path_len = request.path_len as usize;
    let expected = trim_nul(lease.exec_path);
    if path_len != expected.len() || &request.path[..path_len] != expected {
        return Err(13);
    }
    if lease.state == ROOTD_LEASE_STATE_RUNNING {
        if lease.pid == request.arg1 {
            if lease.reporter_pid != sender.pid {
                return Err(13);
            }
            return Ok(());
        }
        return Err(16);
    }
    lease.pid = request.arg1;
    lease.reporter_pid = sender.pid;
    lease.state = ROOTD_LEASE_STATE_RUNNING;
    lease.exit_status = 0;
    Ok(())
}

fn query_post_init_lease<'a>(
    leases: &[Lease],
    post_init_leases: &'a [PostInitLease],
    request: &CommercialMaxProtocolRequest,
    sender: IpcSenderIdentity,
) -> Result<&'a PostInitLease, i32> {
    if request.arg0 == 0
        || request.arg1 != 0
        || request.arg2 != 0
        || request.arg3 != 0
        || request.path_len != 0
        || request.payload_len != 0
    {
        return Err(22);
    }
    authorize_current_initd(leases, sender)?;
    if !initd_manages_post_init_service(request.arg0) {
        return Err(13);
    }
    post_init_leases
        .iter()
        .find(|lease| lease.service_id == request.arg0)
        .ok_or(22)
}

fn reclaim_post_init_lease(
    leases: &[Lease],
    post_init_leases: &mut [PostInitLease],
    request: &CommercialMaxProtocolRequest,
    sender: IpcSenderIdentity,
) -> Result<(), i32> {
    if request.arg0 == 0
        || request.arg1 == 0
        || request.arg2 != 0
        || request.arg3 != 0
        || request.path_len != 0
        || request.payload_len != 0
    {
        return Err(22);
    }
    authorize_current_initd(leases, sender)?;
    if !initd_manages_post_init_service(request.arg0) {
        return Err(13);
    }
    let Some(index) = post_init_leases
        .iter()
        .position(|lease| lease.service_id == request.arg0)
    else {
        return Err(22);
    };
    let target = post_init_leases[index];
    if target.pid != request.arg1
        || target.state == rustos_user_abi::syscall::ROOTD_LEASE_STATE_EMPTY
    {
        return Err(16);
    }

    // A session policy service may have admitted a UI child.  Reclaim the
    // dependent child first, so a dead session supervisor cannot leave a live
    // cross-domain UI endpoint behind when initd replaces the session lease.
    for lease in post_init_leases.iter() {
        if lease.reporter_pid == target.pid && lease.state == ROOTD_LEASE_STATE_RUNNING {
            revoke_service_endpoint(lease.service_id)?;
            terminate_service_process(lease.pid)?;
        }
    }
    if target.state == ROOTD_LEASE_STATE_RUNNING {
        revoke_service_endpoint(target.service_id)?;
        terminate_service_process(target.pid)?;
    }
    for lease in post_init_leases.iter_mut() {
        if lease.pid == target.pid || lease.reporter_pid == target.pid {
            lease.pid = 0;
            lease.reporter_pid = 0;
            lease.state = rustos_user_abi::syscall::ROOTD_LEASE_STATE_EMPTY;
            lease.exit_status = 0;
        }
    }
    Ok(())
}

fn terminate_service_process(pid: u64) -> Result<(), i32> {
    if pid == 0 {
        return Err(22);
    }
    let args = RustosRootdTerminateBrokerArgs {
        abi_version: ROOTD_TERMINATE_BROKER_ABI_VERSION,
        target_pid: pid,
        ..RustosRootdTerminateBrokerArgs::default()
    };
    let status = syscall1(
        SYS_RUSTOS_ROOTD_TERMINATE_BROKER,
        (&args as *const RustosRootdTerminateBrokerArgs) as u64,
    );
    if status >= 0 || status == -3 {
        // ESRCH means lifecycle observation raced the reclaim; it is already
        // unable to publish authority and can be cleared safely.
        return Ok(());
    }
    Err((-status) as i32)
}

fn authorize_current_initd(leases: &[Lease], sender: IpcSenderIdentity) -> Result<(), i32> {
    let initd = lease_by_service_or_index(leases, INITD_LEASE_ID, INITD_LEASE_INDEX)?;
    if initd.pid == sender.pid && initd.state == ROOTD_LEASE_STATE_RUNNING {
        Ok(())
    } else {
        Err(13)
    }
}

fn initd_manages_post_init_service(service_id: u64) -> bool {
    matches!(
        service_id,
        IPC_SERVICE_NETD
            | IPC_SERVICE_DEVMGRD
            | IPC_SERVICE_INPUTD
            | IPC_SERVICE_STORAGED
            | IPC_SERVICE_SESSIOND
    )
}

fn authorize_post_init_lease_reporter(
    leases: &[Lease],
    post_init_leases: &[PostInitLease],
    service_id: u64,
    sender: IpcSenderIdentity,
) -> Result<(), i32> {
    if service_id == IPC_SERVICE_UISERVER {
        let Some(sessiond) = post_init_leases
            .iter()
            .find(|lease| lease.service_id == IPC_SERVICE_SESSIOND)
        else {
            return Err(13);
        };
        if sessiond.pid == sender.pid && sessiond.state == ROOTD_LEASE_STATE_RUNNING {
            return Ok(());
        }
        return Err(13);
    }
    authorize_current_initd(leases, sender)?;
    if initd_manages_post_init_service(service_id) {
        Ok(())
    } else {
        Err(13)
    }
}

fn post_init_reported_service_capability(
    leases: &[Lease],
    post_init_leases: &[PostInitLease],
    service_id: u64,
    subject_pid: u64,
) -> Option<u64> {
    post_init_leases
        .iter()
        .find(|lease| {
            lease.service_id == service_id
                && lease.pid == subject_pid
                && lease.state == ROOTD_LEASE_STATE_RUNNING
                && post_init_lease_reporter_is_live(leases, post_init_leases, lease.reporter_pid)
        })
        .map(|lease| service_policy_capability(lease.service_id))
        .filter(|capability| *capability != 0)
}

fn post_init_lease_reporter_is_live(
    leases: &[Lease],
    post_init_leases: &[PostInitLease],
    reporter_pid: u64,
) -> bool {
    let mut current = reporter_pid;
    for _ in 0..=post_init_leases.len() {
        if current == 0 {
            return false;
        }
        if leases
            .iter()
            .any(|lease| lease.pid == current && lease.state == ROOTD_LEASE_STATE_RUNNING)
        {
            return true;
        }
        let Some(reporter_lease) = post_init_leases
            .iter()
            .find(|lease| lease.pid == current && lease.state == ROOTD_LEASE_STATE_RUNNING)
        else {
            return false;
        };
        current = reporter_lease.reporter_pid;
    }
    // A cycle or chain longer than the fixed lease set has no trusted root.
    false
}

fn authorize_service_lookup_for_subject(
    leases: &[Lease],
    post_init_leases: &[PostInitLease],
    request: &CommercialMaxProtocolRequest,
) -> Result<(), i32> {
    if request.arg0 == 0
        || request.arg1 != 0
        || request.arg2 != 0
        || request.arg3 != 0
        || request.path_len != 0
        || request.payload_len != 0
        || request.header.subject_pid == 0
        || request.header.subject_tid == 0
    {
        return Err(22);
    }
    if service_policy_capability(request.arg0) == 0 {
        return Err(22);
    }
    let subject_service = current_service_id_for_sender(
        leases,
        post_init_leases,
        IpcSenderIdentity {
            pid: request.header.subject_pid,
            tid: request.header.subject_tid,
        },
    )?;
    if service_dependency_allowed(subject_service, request.arg0) {
        Ok(())
    } else {
        Err(13)
    }
}

fn authorize_current_service_namespace(
    leases: &[Lease],
    post_init_leases: &[PostInitLease],
    service_id: u64,
    sender: IpcSenderIdentity,
) -> Result<(), i32> {
    if service_id == 0
        || current_service_id_for_sender(leases, post_init_leases, sender)? != service_id
    {
        return Err(13);
    }
    Ok(())
}

fn current_service_id_for_sender(
    leases: &[Lease],
    post_init_leases: &[PostInitLease],
    sender: IpcSenderIdentity,
) -> Result<u64, i32> {
    if sender.pid == 0 || sender.tid == 0 {
        return Err(22);
    }
    if let Some(lease) = leases
        .iter()
        .find(|lease| lease.pid == sender.pid && lease.state == ROOTD_LEASE_STATE_RUNNING)
    {
        return Ok(lease.service_id);
    }
    post_init_leases
        .iter()
        .find(|lease| {
            lease.pid == sender.pid
                && lease.state == ROOTD_LEASE_STATE_RUNNING
                && post_init_lease_reporter_is_live(leases, post_init_leases, lease.reporter_pid)
        })
        .map(|lease| lease.service_id)
        .ok_or(13)
}

fn service_dependency_allowed(subject: u64, target: u64) -> bool {
    if bootstrap_dependency_allowed(subject, target) {
        return true;
    }
    match subject {
        INITD_LEASE_ID => matches!(
            target,
            IPC_SERVICE_NETD
                | IPC_SERVICE_DEVMGRD
                | IPC_SERVICE_INPUTD
                | IPC_SERVICE_STORAGED
                | IPC_SERVICE_SESSIOND
        ),
        IPC_SERVICE_VFSD => matches!(target, IPC_SERVICE_DEVMGRD | IPC_SERVICE_STORAGED),
        IPC_SERVICE_PROCD => target == IPC_SERVICE_LOADERD,
        // Inputd alone validates the authenticated RDI session lifecycle.
        // Its only outbound policy edge is the bounded grant/revoke handoff
        // to netd; it cannot discover arbitrary service endpoints.
        IPC_SERVICE_INPUTD => target == IPC_SERVICE_NETD,
        IPC_SERVICE_SESSIOND => matches!(
            target,
            IPC_SERVICE_LOADERD | IPC_SERVICE_DEVMGRD | IPC_SERVICE_UISERVER
        ),
        IPC_SERVICE_UISERVER => target == IPC_SERVICE_INPUTD,
        IPC_SERVICE_DEVMGRD => {
            matches!(target, IPC_SERVICE_SESSIOND | IPC_SERVICE_UISERVER)
        }
        _ => false,
    }
}

fn bootstrap_dependency_allowed(subject: u64, target: u64) -> bool {
    let dependency_bit = match target {
        IPC_SERVICE_LINUX_SYSCALLD => DEP_SYSCALLD,
        IPC_SERVICE_VFSD => DEP_VFSD,
        IPC_SERVICE_LOADERD => DEP_LOADERD,
        IPC_SERVICE_PROCD => DEP_PROCD,
        IPC_SERVICE_PAGERD => DEP_PAGERD,
        _ => 0,
    };
    dependency_bit != 0
        && BOOTSTRAP_MANIFEST
            .iter()
            .find(|service| service.service_id == subject)
            .is_some_and(|service| service.dependency_mask & dependency_bit != 0)
}

fn service_name(path: &'static [u8]) -> &'static [u8] {
    let path = trim_nul(path);
    let mut start = 0;
    let mut index = 0;
    while index < path.len() {
        if path[index] == b'/' {
            start = index + 1;
        }
        index += 1;
    }
    &path[start..]
}

fn copy_label(src: &[u8], dest: &mut [u8], len: &mut u16) {
    let count = src.len().min(dest.len());
    dest[..count].copy_from_slice(&src[..count]);
    *len = count as u16;
}

fn write_manifest_payload(leases: &[Lease], dest: &mut [u8]) -> usize {
    let mut written = 0;
    for lease in leases {
        let path = trim_nul(lease.exec_path);
        if written != 0 {
            if written >= dest.len() {
                break;
            }
            dest[written] = b'\n';
            written += 1;
        }
        let remaining = dest.len().saturating_sub(written);
        let count = path.len().min(remaining);
        dest[written..written + count].copy_from_slice(&path[..count]);
        written += count;
        if count < path.len() {
            break;
        }
    }
    written
}

fn write_payload_struct<T>(value: &T, dest: &mut [u8]) -> u32 {
    let bytes = unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    let count = bytes.len().min(dest.len());
    dest[..count].copy_from_slice(&bytes[..count]);
    count as u32
}

fn trim_nul(bytes: &'static [u8]) -> &'static [u8] {
    if bytes.last() == Some(&0) {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    }
}

fn restart_failed_leases(
    endpoint: u64,
    leases: &mut [Lease],
    post_init_leases: &mut [PostInitLease],
    service_checkpoints: &mut ServiceCheckpointStore,
) {
    let mut restart_backoff_ms = 0_u32;
    for index in 0..leases.len() {
        let lease = &mut leases[index];
        if !matches!(
            lease.state,
            ROOTD_LEASE_STATE_EXITED | ROOTD_LEASE_STATE_RESTART_PENDING
        ) {
            continue;
        }
        if lease.restart_budget == 0 {
            lease.state = ROOTD_LEASE_STATE_FAILED;
            continue;
        }
        // An observed process exit is itself a failed service turn. Defer its
        // first replacement as well as failed spawn/activation attempts, or a
        // crash loop can consume the entire restart budget in scheduler turns
        // despite the lease advertising a real backoff policy.
        if lease.state == ROOTD_LEASE_STATE_EXITED {
            lease.state = ROOTD_LEASE_STATE_RESTART_PENDING;
            restart_backoff_ms = restart_backoff_ms.max(lease.backoff_ms);
            continue;
        }
        let result = restart_lease(
            index,
            endpoint,
            leases,
            post_init_leases,
            service_checkpoints,
        );
        let lease = &mut leases[index];
        match result {
            Ok((pid, deferred_start)) => {
                lease.pid = pid;
                lease.restart_budget -= 1;
                lease.state = ROOTD_LEASE_STATE_RUNNING;
                lease.exit_status = 0;
                lease.readiness_polls_remaining = CORE_READINESS_POLL_MAX;
                if deferred_start && activate_exec_via_loaderd(pid).is_err() {
                    cleanup_failed_restart_activation(lease, terminate_service_process)
                        .unwrap_or_else(|_| {
                            fail_closed(b"rootd: fatal failed-activation child cleanup rejected\n")
                        });
                    restart_backoff_ms = restart_backoff_ms.max(lease.backoff_ms);
                }
            }
            Err(_) => {
                lease.restart_budget -= 1;
                if lease.restart_budget == 0 {
                    lease.state = ROOTD_LEASE_STATE_FAILED;
                    lease.exit_status = -11;
                    debug_line(b"rootd: restart budget exhausted\n");
                } else {
                    lease.state = ROOTD_LEASE_STATE_RESTART_PENDING;
                    restart_backoff_ms = restart_backoff_ms.max(lease.backoff_ms);
                }
            }
        }
    }
    if restart_backoff_ms != 0 {
        wait_for_restart_backoff(restart_backoff_ms);
    }
}

fn cleanup_failed_restart_activation(
    lease: &mut Lease,
    terminate: impl FnOnce(u64) -> Result<(), i32>,
) -> Result<(), i32> {
    let pid = lease.pid;
    if pid == 0 {
        return Err(22);
    }
    // A failed ACTIVATE leaves the loader-created child suspended. Retrying
    // without retiring that exact PID leaks a task/process slot on every
    // attempt and eventually prevents all service recovery.
    terminate(pid)?;
    lease.pid = 0;
    lease.state = ROOTD_LEASE_STATE_RESTART_PENDING;
    lease.exit_status = -11;
    Ok(())
}

fn cleanup_failed_initial_activation(
    lease: &mut Lease,
    terminate: impl FnOnce(u64) -> Result<(), i32>,
) -> Result<(), i32> {
    if lease.pid == 0 {
        return Err(22);
    }
    terminate(lease.pid)?;
    lease.pid = 0;
    lease.state = ROOTD_LEASE_STATE_FAILED;
    lease.exit_status = -11;
    Ok(())
}

/// Apply the rootd-owned retry delay with a capability-gated timer substrate.
/// A rejected wait would make the published restart contract unenforceable, so
/// it is a supervisor-fatal condition rather than a busy-loop fallback.
fn wait_for_restart_backoff(backoff_ms: u32) {
    if syscall1(SYS_RUSTOS_ROOTD_WAIT_BROKER, u64::from(backoff_ms)) < 0 {
        fail_closed(b"rootd: fatal restart backoff wait rejected\n");
    }
}

fn restart_lease(
    index: usize,
    endpoint: u64,
    leases: &mut [Lease],
    post_init_leases: &mut [PostInitLease],
    service_checkpoints: &mut ServiceCheckpointStore,
) -> Result<(u64, bool), i64> {
    let lease = leases[index];
    if BOOTSTRAP_MANIFEST[index].restart_direct {
        return spawn_exec(lease.exec_path, lease.weight_micros).map(|pid| (pid, false));
    }
    if !service_dependencies_ready(index) || !service_ready(IPC_SERVICE_LOADERD) {
        return Err(11);
    }
    spawn_exec_via_loaderd_cooperative(
        endpoint,
        leases,
        post_init_leases,
        service_checkpoints,
        lease.exec_path,
        lease.weight_micros,
    )
    .map(|pid| (pid, true))
}

fn spawn_exec_via_loaderd_cooperative(
    endpoint: u64,
    leases: &mut [Lease],
    post_init_leases: &mut [PostInitLease],
    service_checkpoints: &mut ServiceCheckpointStore,
    path: &'static [u8],
    weight_micros: u64,
) -> Result<u64, i64> {
    start_loader_supervisor_worker(endpoint, leases, post_init_leases, service_checkpoints)?;
    debug_line(b"rootd: loader supervisor worker started\n");
    let result = spawn_exec_via_loaderd_blocking(path, weight_micros);
    let encoded = match result {
        Ok(pid) => i64::try_from(pid).unwrap_or(-75),
        Err(errno) => errno.checked_neg().unwrap_or(-75),
    };
    LOADER_WORKER_RESULT.store(encoded, Ordering::Relaxed);
    LOADER_WORKER_STATE.store(LOADER_WORKER_RESULT_READY, Ordering::Release);
    if signal_loader_worker_completion().is_err() {
        fail_closed(b"rootd: fatal loader supervisor completion rejected\n");
    }
    while LOADER_WORKER_STATE.load(Ordering::Acquire) != LOADER_WORKER_EXITED {
        yield_now();
    }
    let result = decode_loader_worker_result(LOADER_WORKER_RESULT.load(Ordering::Acquire));
    LOADER_WORKER_RESULT.store(0, Ordering::Relaxed);
    LOADER_WORKER_ENDPOINT.store(0, Ordering::Relaxed);
    LOADER_WORKER_LEASES_PTR.store(0, Ordering::Relaxed);
    LOADER_WORKER_LEASES_LEN.store(0, Ordering::Relaxed);
    LOADER_WORKER_POST_INIT_PTR.store(0, Ordering::Relaxed);
    LOADER_WORKER_POST_INIT_LEN.store(0, Ordering::Relaxed);
    LOADER_WORKER_CHECKPOINTS_PTR.store(0, Ordering::Relaxed);
    LOADER_WORKER_STATE.store(LOADER_WORKER_IDLE, Ordering::Release);
    debug_line(b"rootd: loader supervisor worker completed\n");
    result
}

fn start_loader_supervisor_worker(
    endpoint: u64,
    leases: &mut [Lease],
    post_init_leases: &mut [PostInitLease],
    service_checkpoints: &mut ServiceCheckpointStore,
) -> Result<(), i64> {
    LOADER_WORKER_STATE
        .compare_exchange(
            LOADER_WORKER_IDLE,
            LOADER_WORKER_RUNNING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map_err(|_| 16_i64)?;
    LOADER_WORKER_ENDPOINT.store(endpoint, Ordering::Release);
    LOADER_WORKER_LEASES_PTR.store(leases.as_mut_ptr() as usize, Ordering::Release);
    LOADER_WORKER_LEASES_LEN.store(leases.len(), Ordering::Release);
    LOADER_WORKER_POST_INIT_PTR.store(post_init_leases.as_mut_ptr() as usize, Ordering::Release);
    LOADER_WORKER_POST_INIT_LEN.store(post_init_leases.len(), Ordering::Release);
    LOADER_WORKER_CHECKPOINTS_PTR.store(
        service_checkpoints as *mut ServiceCheckpointStore as usize,
        Ordering::Release,
    );
    LOADER_WORKER_RESULT.store(0, Ordering::Relaxed);
    if let Err(errno) = spawn_loader_worker_thread() {
        LOADER_WORKER_STATE.store(LOADER_WORKER_IDLE, Ordering::Release);
        return Err(errno);
    }
    Ok(())
}

#[cfg(not(test))]
fn spawn_loader_worker_thread() -> Result<(), i64> {
    let stack_top = unsafe {
        core::ptr::addr_of_mut!(LOADER_WORKER_STACK.0)
            .cast::<u8>()
            .add(LOADER_WORKER_STACK_BYTES) as u64
    };
    let flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            "test rax, rax",
            "jnz 2f",
            "call {entry}",
            "ud2",
            "2:",
            entry = sym loader_supervisor_worker_entry,
            inlateout("rax") SYS_CLONE as i64 => result,
            in("rdi") flags,
            in("rsi") stack_top,
            in("rdx") 0_u64,
            in("r10") 0_u64,
            in("r8") 0_u64,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    if result <= 0 {
        Err(if result < 0 { -result } else { 11 })
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn spawn_loader_worker_thread() -> Result<(), i64> {
    Err(38)
}

#[cfg(not(test))]
extern "C" fn loader_supervisor_worker_entry() -> ! {
    let endpoint = LOADER_WORKER_ENDPOINT.load(Ordering::Acquire);
    let leases_ptr = LOADER_WORKER_LEASES_PTR.load(Ordering::Acquire) as *mut Lease;
    let leases_len = LOADER_WORKER_LEASES_LEN.load(Ordering::Acquire);
    let post_init_ptr = LOADER_WORKER_POST_INIT_PTR.load(Ordering::Acquire) as *mut PostInitLease;
    let post_init_len = LOADER_WORKER_POST_INIT_LEN.load(Ordering::Acquire);
    let checkpoints_ptr =
        LOADER_WORKER_CHECKPOINTS_PTR.load(Ordering::Acquire) as *mut ServiceCheckpointStore;
    if endpoint == 0 || leases_ptr.is_null() || post_init_ptr.is_null() || checkpoints_ptr.is_null()
    {
        LOADER_WORKER_RESULT.store(-22, Ordering::Relaxed);
        LOADER_WORKER_STATE.store(LOADER_WORKER_EXITED, Ordering::Release);
    } else {
        let leases = unsafe { slice::from_raw_parts_mut(leases_ptr, leases_len) };
        let post_init_leases = unsafe { slice::from_raw_parts_mut(post_init_ptr, post_init_len) };
        let service_checkpoints = unsafe { &mut *checkpoints_ptr };
        while LOADER_WORKER_STATE.load(Ordering::Acquire) != LOADER_WORKER_COMPLETE {
            drain_lifecycle_events(leases, post_init_leases);
            serve_rootd_once(
                endpoint,
                leases,
                post_init_leases,
                service_checkpoints,
                true,
            );
        }
        LOADER_WORKER_STATE.store(LOADER_WORKER_EXITED, Ordering::Release);
    }
    let _ = syscall1(SYS_EXIT, 0);
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(not(test))]
fn signal_loader_worker_completion() -> Result<(), i64> {
    let endpoint = syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, IPC_SERVICE_ROOTD);
    let pid = syscall0(SYS_GETPID);
    let tid = syscall0(SYS_GETTID);
    if endpoint <= 0 || pid <= 0 || tid <= 0 {
        return Err(5);
    }
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = COMMERCIAL_MAX_ROOTD_OP_LOADER_WORKER_COMPLETE;
    request.header.subject_pid = pid as u64;
    request.header.subject_tid = tid as u64;
    let mut response = CommercialMaxProtocolResponse::default();
    let received = syscall5(
        SYS_RUSTOS_IPC_CALL,
        endpoint as u64,
        (&request as *const CommercialMaxProtocolRequest) as u64,
        size_of::<CommercialMaxProtocolRequest>() as u64,
        (&mut response as *mut CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
    if received as usize != size_of::<CommercialMaxProtocolResponse>()
        || !response.is_valid_envelope_for(&request)
        || response.status != 0
        || response.descriptor_count != 0
        || response.payload_len != 0
        || response.value0 != 1
        || response.value1 != 0
    {
        return Err(5);
    }
    Ok(())
}

#[cfg(test)]
fn signal_loader_worker_completion() -> Result<(), i64> {
    Err(38)
}

fn decode_loader_worker_result(encoded: i64) -> Result<u64, i64> {
    if encoded > 0 {
        Ok(encoded as u64)
    } else if encoded < 0 {
        Err(encoded.checked_neg().ok_or(75)?)
    } else {
        Err(5)
    }
}

fn spawn_exec_via_loaderd_blocking(path: &'static [u8], weight_micros: u64) -> Result<u64, i64> {
    let endpoint = syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, IPC_SERVICE_LOADERD);
    if endpoint <= 0 {
        return Err(if endpoint < 0 { -endpoint } else { 11 });
    }
    let path = trim_nul(path);
    if path.is_empty()
        || path.len() > LOADER_SPAWN_EXEC_PATH_CAPACITY
        || path.len() >= LOADER_SPAWN_ARG_BYTES
        || contains_nul(path)
    {
        return Err(22);
    }
    let request = unsafe { &mut *INITD_LOADER_REQUEST.0.get() };
    *request = empty_loader_spawn_request();
    request.version = LOADER_REQUEST_ABI_VERSION;
    request.op = LOADER_OP_SPAWN_EXEC;
    let requester_pid = syscall0(SYS_GETPID);
    if requester_pid <= 0 {
        return Err(22);
    }
    request.requester_pid = requester_pid as u64;
    request.flags = SPAWN_FLAG_LOGICAL_ADMIN as u32 | LOADER_SPAWN_FLAG_DEFER_START;
    request.weight_micros = weight_micros;
    request.exec_path_len = path.len() as u32;
    request.argv_count = 1;
    request.argv_bytes_len = (path.len() + 1) as u32;
    copy_bytes(path, &mut request.exec_path);
    copy_bytes(path, &mut request.argv_bytes);
    request.argv_bytes[path.len()] = 0;

    let response = unsafe { &mut *INITD_LOADER_RESPONSE.0.get() };
    *response = empty_loader_spawn_response();
    let result = syscall5(
        SYS_RUSTOS_IPC_CALL,
        endpoint as u64,
        (request as *const LoaderSpawnRequest) as u64,
        size_of::<LoaderSpawnRequest>() as u64,
        (response as *mut LoaderSpawnResponse) as u64,
        size_of::<LoaderSpawnResponse>() as u64,
    );
    if result < 0 {
        return Err(-result);
    }
    if result as usize != size_of::<LoaderSpawnResponse>()
        || response.version != LOADER_REQUEST_ABI_VERSION
        || response.op != LOADER_OP_SPAWN_EXEC
    {
        return Err(22);
    }
    if response.status != 0 {
        return Err(response.status as i64);
    }
    if response.pid <= 0 {
        return Err(22);
    }
    Ok(response.pid as u64)
}

fn activate_exec_via_loaderd(pid: u64) -> Result<(), i64> {
    let endpoint = syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, IPC_SERVICE_LOADERD);
    if endpoint <= 0 {
        return Err(if endpoint < 0 { -endpoint } else { 11 });
    }
    let request = unsafe { &mut *INITD_LOADER_REQUEST.0.get() };
    *request = empty_loader_spawn_request();
    request.version = LOADER_REQUEST_ABI_VERSION;
    request.op = LOADER_OP_ACTIVATE;
    let requester_pid = syscall0(SYS_GETPID);
    if requester_pid <= 0 {
        return Err(22);
    }
    request.requester_pid = requester_pid as u64;
    request.target_pid = pid;

    let response = unsafe { &mut *INITD_LOADER_RESPONSE.0.get() };
    *response = empty_loader_spawn_response();
    let result = syscall5(
        SYS_RUSTOS_IPC_CALL,
        endpoint as u64,
        (request as *const LoaderSpawnRequest) as u64,
        size_of::<LoaderSpawnRequest>() as u64,
        (response as *mut LoaderSpawnResponse) as u64,
        size_of::<LoaderSpawnResponse>() as u64,
    );
    if result < 0 {
        return Err(-result);
    }
    if result as usize != size_of::<LoaderSpawnResponse>()
        || response.version != LOADER_REQUEST_ABI_VERSION
        || response.op != LOADER_OP_ACTIVATE
        || response.pid != pid as i64
    {
        return Err(22);
    }
    if response.status != 0 {
        return Err(response.status as i64);
    }
    Ok(())
}

const fn empty_loader_spawn_request() -> LoaderSpawnRequest {
    LoaderSpawnRequest {
        version: 0,
        op: 0,
        flags: 0,
        console_session: 0,
        weight_micros: 0,
        target_pid: 0,
        target_tid: 0,
        exec_ticket: 0,
        exec_path_len: 0,
        argv_count: 0,
        env_count: 0,
        argv_bytes_len: 0,
        env_bytes_len: 0,
        requester_pid: 0,
        exec_path: [0; LOADER_SPAWN_EXEC_PATH_CAPACITY],
        argv_bytes: [0; LOADER_SPAWN_ARG_BYTES],
        env_bytes: [0; LOADER_SPAWN_ENV_BYTES],
    }
}

const fn empty_loader_spawn_response() -> LoaderSpawnResponse {
    LoaderSpawnResponse {
        version: 0,
        op: 0,
        status: 0,
        pid: 0,
        reserved0: 0,
    }
}

fn contains_nul(bytes: &[u8]) -> bool {
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == 0 {
            return true;
        }
        index += 1;
    }
    false
}

fn copy_bytes(src: &[u8], dest: &mut [u8]) {
    let mut index = 0usize;
    while index < src.len() {
        dest[index] = src[index];
        index += 1;
    }
}

fn debug_line(bytes: &[u8]) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        bytes.as_ptr() as u64,
        bytes.len() as u64,
    );
}

fn yield_now() {
    let _ = syscall0(SYS_SCHED_YIELD);
}

fn supervisor_idle() {
    yield_now();
}

fn fail_closed(message: &[u8]) -> ! {
    debug_line(message);
    loop {
        yield_now();
    }
}

fn syscall0(number: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall5(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("r10") arg3,
            in("r8") arg4,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall6(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("r10") arg3,
            in("r8") arg4,
            in("r9") arg5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    fail_closed(b"rootd: panic\n");
}

#[cfg(not(test))]
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

#[cfg(not(test))]
#[no_mangle]
/// # Safety
///
/// `lhs` and `rhs` must be valid for `len` bytes.
pub unsafe extern "C" fn bcmp(lhs: *const u8, rhs: *const u8, len: usize) -> i32 {
    let mut offset = 0usize;
    while offset < len {
        let left = lhs.add(offset).read();
        let right = rhs.add(offset).read();
        if left != right {
            return left as i32 - right as i32;
        }
        offset += 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_entry_aligns_stack_before_calling_rust() {
        let source = include_str!("main.rs");
        let trampoline = source
            .split("core::arch::global_asm!(")
            .nth(1)
            .and_then(|rest| rest.split(");").next())
            .expect("rootd raw entry trampoline");
        assert!(trampoline.contains("\"    and rsp, -16\""));
        assert!(trampoline.contains("\"    call __rustos_rootd_start\""));
    }

    #[test]
    fn production_root_installs_reclaiming_heap_before_first_allocation() {
        let source = include_str!("main.rs");
        let entry = source
            .split("pub extern \"C\" fn __rustos_rootd_start() -> ! {")
            .nth(1)
            .expect("rootd production entry");
        let allocator = entry
            .find("rustos_svc_runtime::allocator::init")
            .expect("reclaiming allocator initialization");
        let first_service_action = entry
            .find("debug_line(b\"rootd: bootstrap enter")
            .expect("first rootd service action");
        assert!(allocator < first_service_action);
        assert_eq!(source.matches("RootdBumpAllocator").count(), 1);
    }

    #[test]
    fn loader_worker_result_is_fail_closed_and_preserves_errno() {
        assert_eq!(decode_loader_worker_result(41), Ok(41));
        assert_eq!(decode_loader_worker_result(-11), Err(11));
        assert_eq!(decode_loader_worker_result(0), Err(5));
        assert_eq!(decode_loader_worker_result(i64::MIN), Err(75));
    }

    #[test]
    fn root_supervisor_requests_require_exact_sender_and_canonical_unused_fields() {
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
        request.header.op = COMMERCIAL_MAX_ROOTD_OP_BOOTSTRAP_MANIFEST;
        request.header.subject_pid = 41;
        request.header.subject_tid = 7;
        let sender = IpcSenderIdentity { pid: 41, tid: 7 };
        assert_eq!(validate_commercial_max_request(&request, sender), Ok(()));

        assert_eq!(
            validate_commercial_max_request(&request, IpcSenderIdentity { pid: 42, tid: 7 }),
            Err(13)
        );
        request.arg0 = 1;
        assert_eq!(validate_commercial_max_request(&request, sender), Err(22));
    }

    #[test]
    fn loader_worker_completion_is_same_process_and_exact_state_only() {
        let pid = syscall0(SYS_GETPID) as u64;
        let tid = syscall0(SYS_GETTID) as u64;
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
        request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
        request.header.op = COMMERCIAL_MAX_ROOTD_OP_LOADER_WORKER_COMPLETE;
        request.header.subject_pid = pid;
        request.header.subject_tid = tid;
        let sender = IpcSenderIdentity { pid, tid };

        LOADER_WORKER_STATE.store(LOADER_WORKER_RESULT_READY, Ordering::Release);
        assert_eq!(complete_loader_worker(&request, sender), Ok(()));
        assert_eq!(
            LOADER_WORKER_STATE.load(Ordering::Acquire),
            LOADER_WORKER_COMPLETE
        );
        assert_eq!(complete_loader_worker(&request, sender), Err(16));
        LOADER_WORKER_STATE.store(LOADER_WORKER_IDLE, Ordering::Release);

        assert_eq!(
            complete_loader_worker(
                &request,
                IpcSenderIdentity {
                    pid: pid.saturating_add(1),
                    tid,
                },
            ),
            Err(13)
        );
    }

    fn leases_with_live_initd(initd_pid: u64) -> [Lease; 5] {
        let mut leases = [
            lease(BOOTSTRAP_MANIFEST[0]),
            lease(BOOTSTRAP_MANIFEST[1]),
            lease(BOOTSTRAP_MANIFEST[2]),
            lease(BOOTSTRAP_MANIFEST[3]),
            lease(BOOTSTRAP_MANIFEST[4]),
        ];
        leases[INITD_LEASE_INDEX].pid = initd_pid;
        leases[INITD_LEASE_INDEX].state = ROOTD_LEASE_STATE_RUNNING;
        leases
    }

    fn post_init_request(service_id: u64, pid: u64) -> CommercialMaxProtocolRequest {
        let mut request = CommercialMaxProtocolRequest {
            arg0: service_id,
            arg1: pid,
            ..CommercialMaxProtocolRequest::default()
        };
        let spec = POST_INIT_MANIFEST
            .iter()
            .find(|spec| spec.service_id == service_id)
            .expect("declared post-init service");
        let path = trim_nul(spec.exec_path);
        request.path[..path.len()].copy_from_slice(path);
        request.path_len = path.len() as u32;
        request
    }

    #[test]
    fn service_lookup_uses_the_declared_dependency_edge_not_generic_liveness() {
        let mut leases = leases_with_live_initd(41);
        leases[1].pid = 52;
        leases[1].state = ROOTD_LEASE_STATE_RUNNING;
        let post_init = [];
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.subject_pid = 52;
        request.header.subject_tid = 7;
        request.arg0 = IPC_SERVICE_DEVMGRD;
        assert_eq!(
            authorize_service_lookup_for_subject(&leases, &post_init, &request),
            Ok(())
        );
        request.arg0 = IPC_SERVICE_NETD;
        assert_eq!(
            authorize_service_lookup_for_subject(&leases, &post_init, &request),
            Err(13)
        );
    }

    #[test]
    fn vfsd_lookup_authority_includes_only_device_and_storage_policy() {
        assert!(service_dependency_allowed(
            IPC_SERVICE_VFSD,
            IPC_SERVICE_DEVMGRD
        ));
        assert!(service_dependency_allowed(
            IPC_SERVICE_VFSD,
            IPC_SERVICE_STORAGED
        ));
        assert!(!service_dependency_allowed(
            IPC_SERVICE_VFSD,
            IPC_SERVICE_NETD
        ));
    }

    #[test]
    fn loaderd_lookup_authority_includes_only_immutable_vfs_source() {
        assert!(service_dependency_allowed(
            IPC_SERVICE_LOADERD,
            IPC_SERVICE_VFSD
        ));
        assert!(!service_dependency_allowed(
            IPC_SERVICE_LOADERD,
            IPC_SERVICE_STORAGED
        ));
        assert!(!service_dependency_allowed(
            IPC_SERVICE_LOADERD,
            IPC_SERVICE_DEVMGRD
        ));
    }

    #[test]
    fn inputd_lookup_authority_is_only_the_netd_lifecycle_handoff() {
        assert!(service_dependency_allowed(
            IPC_SERVICE_INPUTD,
            IPC_SERVICE_NETD
        ));
        assert!(!service_dependency_allowed(
            IPC_SERVICE_INPUTD,
            IPC_SERVICE_DEVMGRD
        ));
        assert!(!service_dependency_allowed(
            IPC_SERVICE_INPUTD,
            IPC_SERVICE_UISERVER
        ));
    }

    #[test]
    fn initd_lookup_authority_includes_every_declared_bootstrap_dependency() {
        for service_id in [
            IPC_SERVICE_LINUX_SYSCALLD,
            IPC_SERVICE_VFSD,
            IPC_SERVICE_LOADERD,
            IPC_SERVICE_PROCD,
            IPC_SERVICE_PAGERD,
        ] {
            assert!(bootstrap_dependency_allowed(INITD_LEASE_ID, service_id));
            assert!(service_dependency_allowed(INITD_LEASE_ID, service_id));
        }
        assert!(!bootstrap_dependency_allowed(
            INITD_LEASE_ID,
            IPC_SERVICE_UISERVER
        ));
        assert!(!service_dependency_allowed(
            INITD_LEASE_ID,
            IPC_SERVICE_UISERVER
        ));
    }

    #[test]
    fn checkpoint_namespace_is_bound_to_the_current_service_lease() {
        let mut leases = leases_with_live_initd(41);
        leases[1].pid = 52;
        leases[1].state = ROOTD_LEASE_STATE_RUNNING;
        let post_init = [];
        assert_eq!(
            authorize_current_service_namespace(
                &leases,
                &post_init,
                IPC_SERVICE_VFSD,
                IpcSenderIdentity { pid: 52, tid: 7 },
            ),
            Ok(())
        );
        assert_eq!(
            authorize_current_service_namespace(
                &leases,
                &post_init,
                IPC_SERVICE_PROCD,
                IpcSenderIdentity { pid: 52, tid: 7 },
            ),
            Err(13)
        );
    }

    #[test]
    fn post_init_lease_cannot_be_rebound_by_a_different_reporter() {
        let initd_pid = 41;
        let leases = leases_with_live_initd(initd_pid);
        let request = post_init_request(IPC_SERVICE_NETD, 71);
        let mut post_init = [post_init_lease(POST_INIT_MANIFEST[0])];

        assert_eq!(
            register_post_init_service_lease_with_provenance(
                &leases,
                &mut post_init,
                &request,
                IpcSenderIdentity {
                    pid: initd_pid,
                    tid: 1,
                },
                true,
            ),
            Ok(())
        );
        assert_eq!(post_init[0].reporter_pid, initd_pid);

        assert_eq!(
            register_post_init_service_lease_with_provenance(
                &leases,
                &mut post_init,
                &request,
                IpcSenderIdentity { pid: 99, tid: 2 },
                true,
            ),
            Err(13)
        );
        assert_eq!(post_init[0].reporter_pid, initd_pid);
    }

    #[test]
    fn post_init_lease_requires_the_exact_declared_executable_path() {
        let initd_pid = 41;
        let leases = leases_with_live_initd(initd_pid);
        let mut post_init = [post_init_lease(POST_INIT_MANIFEST[0])];
        let sender = IpcSenderIdentity {
            pid: initd_pid,
            tid: 1,
        };

        let mut missing = post_init_request(IPC_SERVICE_NETD, 71);
        missing.path_len = 0;
        assert_eq!(
            register_post_init_service_lease_with_provenance(
                &leases,
                &mut post_init,
                &missing,
                sender,
                true,
            ),
            Err(22)
        );

        let mut foreign = post_init_request(IPC_SERVICE_NETD, 71);
        foreign.path[0] ^= 1;
        assert_eq!(
            register_post_init_service_lease_with_provenance(
                &leases,
                &mut post_init,
                &foreign,
                sender,
                true,
            ),
            Err(13)
        );

        let exact = post_init_request(IPC_SERVICE_NETD, 71);
        assert_eq!(
            register_post_init_service_lease_with_provenance(
                &leases,
                &mut post_init,
                &exact,
                sender,
                true,
            ),
            Ok(())
        );
        let mut another = [post_init_lease(POST_INIT_MANIFEST[0])];
        assert_eq!(
            register_post_init_service_lease_with_provenance(
                &leases,
                &mut another,
                &exact,
                sender,
                false,
            ),
            Err(13)
        );
    }

    #[test]
    fn uiserver_admission_requires_its_live_sessiond_reporter() {
        let leases = leases_with_live_initd(41);
        let mut post_init = [post_init_lease(POST_INIT_MANIFEST[4])];
        post_init[0].pid = 73;
        post_init[0].state = ROOTD_LEASE_STATE_RUNNING;

        assert_eq!(
            authorize_post_init_lease_reporter(
                &leases,
                &post_init,
                IPC_SERVICE_UISERVER,
                IpcSenderIdentity { pid: 73, tid: 1 },
            ),
            Ok(())
        );
        assert_eq!(
            authorize_post_init_lease_reporter(
                &leases,
                &post_init,
                IPC_SERVICE_UISERVER,
                IpcSenderIdentity { pid: 41, tid: 1 },
            ),
            Err(13)
        );
    }

    #[test]
    fn reporter_exit_cascades_and_capability_requires_live_reporter_chain() {
        let initd_pid = 41;
        let mut leases = leases_with_live_initd(initd_pid);
        let mut post_init = [
            post_init_lease(POST_INIT_MANIFEST[4]),
            post_init_lease(POST_INIT_MANIFEST[5]),
        ];
        post_init[0].pid = 73;
        post_init[0].reporter_pid = initd_pid;
        post_init[0].state = ROOTD_LEASE_STATE_RUNNING;
        post_init[1].pid = 74;
        post_init[1].reporter_pid = 73;
        post_init[1].state = ROOTD_LEASE_STATE_RUNNING;

        assert_eq!(
            post_init_reported_service_capability(&leases, &post_init, IPC_SERVICE_UISERVER, 74,),
            Some(rustos_user_abi::syscall::IPC_SERVICE_CAP_UI_POLICY)
        );
        leases[INITD_LEASE_INDEX].state = ROOTD_LEASE_STATE_EXITED;
        assert_eq!(
            post_init_reported_service_capability(&leases, &post_init, IPC_SERVICE_UISERVER, 74,),
            None
        );

        let mut terminated = [0_u64; 2];
        let mut terminated_count = 0_usize;
        let mut revoked = [0_u64; 2];
        let mut revoked_count = 0_usize;
        assert_eq!(
            revoke_post_init_dependents_with(
                &mut post_init,
                initd_pid,
                |service_id| {
                    revoked[revoked_count] = service_id;
                    revoked_count += 1;
                    Ok(())
                },
                |pid| {
                    terminated[terminated_count] = pid;
                    terminated_count += 1;
                    Ok(())
                },
            ),
            Ok(())
        );
        assert_eq!(
            &revoked[..revoked_count],
            &[IPC_SERVICE_SESSIOND, IPC_SERVICE_UISERVER]
        );
        assert_eq!(&terminated[..terminated_count], &[73, 74]);
        assert!(post_init
            .iter()
            .all(|lease| lease.state == ROOTD_LEASE_STATE_EXITED));
    }

    #[test]
    fn failed_restart_activation_retires_exact_suspended_child() {
        let mut failed = lease(BOOTSTRAP_MANIFEST[INITD_LEASE_INDEX]);
        failed.pid = 77;
        failed.state = ROOTD_LEASE_STATE_RUNNING;
        failed.exit_status = 0;
        let mut terminated = 0;

        assert_eq!(
            cleanup_failed_restart_activation(&mut failed, |pid| {
                terminated = pid;
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(terminated, 77);
        assert_eq!(failed.pid, 0);
        assert_eq!(failed.state, ROOTD_LEASE_STATE_RESTART_PENDING);
        assert_eq!(failed.exit_status, -11);

        let mut uncertain = lease(BOOTSTRAP_MANIFEST[INITD_LEASE_INDEX]);
        uncertain.pid = 88;
        uncertain.state = ROOTD_LEASE_STATE_RUNNING;
        assert_eq!(
            cleanup_failed_restart_activation(&mut uncertain, |_| Err(5)),
            Err(5)
        );
        assert_eq!(uncertain.pid, 88);
        assert_eq!(uncertain.state, ROOTD_LEASE_STATE_RUNNING);

        let mut initial = lease(BOOTSTRAP_MANIFEST[INITD_LEASE_INDEX]);
        initial.pid = 91;
        initial.state = ROOTD_LEASE_STATE_RUNNING;
        assert_eq!(
            cleanup_failed_initial_activation(&mut initial, |pid| {
                assert_eq!(pid, 91);
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(initial.pid, 0);
        assert_eq!(initial.state, ROOTD_LEASE_STATE_FAILED);
        assert_eq!(initial.exit_status, -11);
    }

    #[test]
    fn core_readiness_budget_is_bounded_and_resets_only_on_readiness() {
        let mut core = lease(BOOTSTRAP_MANIFEST[0]);
        core.pid = 101;
        core.state = ROOTD_LEASE_STATE_RUNNING;

        for _ in 0..CORE_READINESS_POLL_MAX {
            assert!(!readiness_timeout_due(&mut core, false));
        }
        assert!(readiness_timeout_due(&mut core, false));

        assert!(!readiness_timeout_due(&mut core, true));
        assert_eq!(core.readiness_polls_remaining, CORE_READINESS_POLL_MAX);
        assert!(!readiness_timeout_due(&mut core, false));
    }
}
