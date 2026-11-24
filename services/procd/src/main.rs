#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
// TEST-HARNESS: the host test target omits the service entry, so the production
// request graph is unreachable only in that harness.
#![cfg_attr(test, allow(dead_code, unused_imports))]

extern crate alloc;

use alloc::collections::BTreeMap;
use core::mem::size_of;
#[cfg(not(test))]
use core::panic::PanicInfo;

use rustos_svc_runtime::ipc;
use rustos_svc_runtime::syscall::{syscall0, syscall1};
use rustos_user_abi::syscall::{
    identity_is_exact_sender, procd_sigchld_is_suppressed, CommercialMaxCapabilityLeaseWire,
    CommercialMaxProtocolDescriptorWire, CommercialMaxProtocolRequest,
    CommercialMaxProtocolResponse, LifecycleDrainBrokerArgs, LifecycleEventWire,
    LoaderSpawnRequest, LoaderSpawnResponse, ProcdIpcRequest, ProcdIpcResponse,
    RustosProcAuthorizeExecBrokerArgs, RustosProcCancelExecBrokerArgs, RustosProcForkBrokerArgs,
    RustosProcSignalQueueBrokerArgs, COMMERCIAL_MAX_PROCD_OP_EXEC_TICKET,
    COMMERCIAL_MAX_PROCD_OP_FORK_PLAN, COMMERCIAL_MAX_PROCD_OP_SESSION_MEMBERSHIP,
    COMMERCIAL_MAX_PROCD_OP_SIGNAL_POLICY, COMMERCIAL_MAX_PROCD_OP_THREAD_PLAN,
    COMMERCIAL_MAX_PROCD_OP_WAIT_NAMESPACE, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_PROCD, IPC_MAX_INLINE_BYTES, IPC_SERVICE_LOADERD, IPC_SERVICE_PROCD,
    LIFECYCLE_DRAIN_BROKER_ABI_VERSION, LIFECYCLE_DRAIN_MAX_EVENTS, LIFECYCLE_EVENT_EXIT,
    LIFECYCLE_EVENT_THREAD_EXIT, LIFECYCLE_EVENT_THREAD_EXIT_LOST, LINUX_SIGACTION_SIZE, LOADER_OP_EXEC_TARGET, LOADER_REQUEST_ABI_VERSION,
    LOADER_SPAWN_MAX_ARG_COUNT, LOADER_SPAWN_MAX_ENV_COUNT, PROCD_ARG_BYTES, PROCD_ENV_BYTES,
    PROCD_IPC_ABI_VERSION, PROCD_OP_EXECVE, PROCD_OP_EXECVEAT, PROCD_OP_FORK,
    PROCD_OP_RT_SIGACTION, PROCD_OP_RT_SIGPROCMASK, PROCD_OP_SELECT_SIGNAL, PROCD_OP_SIGALTSTACK,
    PROCD_OP_TGKILL, PROCD_OP_WAIT4, PROCD_PATH_CAPACITY,
    PROCD_PAYLOAD_CAPACITY, PROCD_SELECT_SIGNAL_HANDLER, PROCD_SELECT_SIGNAL_IGNORE,
    PROCD_SELECT_SIGNAL_NONE, PROCD_SELECT_SIGNAL_STOP, PROCD_SELECT_SIGNAL_TERMINATE,
    PROCD_SIGCHLD_EVENT_MASK, PROC_BROKER_USER_SPACE_BASE, PROC_BROKER_USER_SPACE_END_EXCLUSIVE,
    SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER, SYS_RUSTOS_PROC_AUTHORIZE_EXEC_BROKER,
    SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER, SYS_RUSTOS_PROC_FORK_BROKER,
    SYS_RUSTOS_PROC_SIGNAL_QUEUE_BROKER,
};
use spin::Mutex;

const EINVAL: i32 = 22;
const EACCES: i32 = 13;
const ENOENT: i32 = 2;
const ENOMEM: i32 = 12;
const ENOSYS: i32 = 38;
const SYS_GETPID: u64 = 39;
const AT_FDCWD: u64 = -100_i64 as u64;
const WNOHANG: u64 = 1;
const WUNTRACED: u64 = 2;
const WCONTINUED: u64 = 8;
const SS_DISABLE: u32 = 2;
const MINSIGSTKSZ: u64 = 2048;
const SIG_BLOCK: u64 = 0;
const SIG_UNBLOCK: u64 = 1;
const SIG_SETMASK: u64 = 2;
const SIG_IGN: u64 = 1;
const SIGKILL: usize = 9;
const SIGCHLD: usize = 17;
const SIGSTOP: usize = 19;
const LINUX_USER_SPACE_BASE: u64 = PROC_BROKER_USER_SPACE_BASE;
const LINUX_USER_SPACE_END: u64 = PROC_BROKER_USER_SPACE_END_EXCLUSIVE;

static PROCESS_SIGNAL_POLICY: Mutex<BTreeMap<u64, ProcessSignalPolicy>> =
    Mutex::new(BTreeMap::new());
static THREAD_SIGNAL_POLICY: Mutex<BTreeMap<(u64, u64), ThreadSignalPolicy>> =
    Mutex::new(BTreeMap::new());
/// Serializes "drain the kernel's lifecycle queue and apply it" across the
/// receiver threads, so a request never runs before exit evidence another
/// receiver already took from the queue has been applied.
static LIFECYCLE_DRAIN: Mutex<()> = Mutex::new(());
/// Parents with a fork in progress, with the number of forks outstanding.
static FORKS_IN_FLIGHT: Mutex<BTreeMap<u64, u32>> = Mutex::new(BTreeMap::new());
/// Monotonic epoch bumped whenever a `ForkInFlight` finishes, so child
/// requests sleep on a futex instead of spinning on `sched_yield`.
static FORK_IN_FLIGHT_EPOCH: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
/// Upper bound a child's first request waits for its parent's fork to finish
/// inheriting policy. A fork that is still unpublished never has a running
/// child, so reaching this bound means procd itself is wedged.
const FORK_INHERIT_WAIT_NS: u64 = 5_000_000_000;
const EAGAIN: i32 = 11;

#[cfg(not(test))]
rustos_svc_runtime::entry!(service_main);

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}

#[cfg(not(test))]
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

fn service_main() {
    let endpoint = ipc::endpoint_create_with_limits(IPC_MAX_INLINE_BYTES as u64, 0);
    if endpoint < 0 {
        ipc::debug_line("procd: endpoint create failed");
        return;
    }
    if ipc::register_service_endpoint(IPC_SERVICE_PROCD, endpoint as u64) < 0 {
        ipc::debug_line("procd: endpoint register failed");
        return;
    }
    ipc::debug_line("procd: process policy endpoint registered");
    if let Err(errno) = rustos_svc_runtime::heartbeat::spawn_thread(rustos_user_abi::syscall::IPC_SERVICE_PROCD) {
        ipc::debug_line(&alloc::format!("procd: heartbeat thread unavailable errno={errno}"));
    }
    // Rebase this generation's private fan-out queue before serving policy.
    // A replacement procd must not wait for sixteen new requests before it
    // acknowledges evidence accumulated while its predecessor was absent.
    drain_lifecycle_events();
    ipc::debug_line("procd: lifecycle drain ready");
    pool::publish_endpoint(endpoint as u64);
    serve(endpoint as u64);
}

/// Main receive loop. Long-running fork/exec calls spawn transient overflow
/// receivers (see `pool`) so signal-policy calls do not queue behind them
/// without occupying permanent task-table slots when procd is idle.
fn serve(endpoint: u64) {
    let mut request = [0_u8; IPC_MAX_INLINE_BYTES];
    let mut reply_cap = 0_u64;
    let mut sender_pid = 0_u64;
    let mut sender_tid = 0_u64;
    let mut received = receive_request(endpoint, &mut request, &mut reply_cap, &mut sender_pid, &mut sender_tid);
    let mut failed_receives = 0_u32;
    loop {
        if received < 0 {
            // A persistently failing receive must not become a 1 kHz spin;
            // widen the pause to 16 ms, restarting at 1 ms after any success.
            rustos_svc_runtime::syscall::sleep_millis(1 << failed_receives.min(4));
            failed_receives = failed_receives.saturating_add(1);
            received = receive_request(endpoint, &mut request, &mut reply_cap, &mut sender_pid, &mut sender_tid);
            continue;
        }
        failed_receives = 0;

        // Apply queued exit evidence before any PID-keyed policy lookup. The
        // drain is a bounded local broker call and never discovers syscalld or
        // rootd, so it cannot recreate the bootstrap authorization cycle.
        drain_lifecycle_events();
        let response = handle_wire_request(received as usize, &request, sender_pid, sender_tid);
        // Reply and receive in one syscall: a receiver already parked on the
        // endpoint is what lets the kernel's same-CPU lane switch directly.
        // SAFETY: request and response storage are live and disjoint until the
        // fused call returns; every output is exclusively borrowed.
        let next = unsafe {
            ipc::reply_then_recv(
                endpoint,
                reply_cap,
                response.as_ptr(),
                response.len(),
                request.as_mut_ptr(),
                request.len(),
                &mut reply_cap as *mut u64,
                &mut sender_pid as *mut u64,
                &mut sender_tid as *mut u64,
                ipc::debug_line_static,
            )
        };
        received = if next < 0 {
            receive_request(endpoint, &mut request, &mut reply_cap, &mut sender_pid, &mut sender_tid)
        } else {
            next
        };
    }
}

/// Transient overflow receive loop run only while a fork or exec is in flight.
/// Replies on the same thread that received each request, preserving the
/// rendezvous reply-thread invariant, and exits as soon as no slow op remains.
#[cfg(not(test))]
fn serve_transient(endpoint: u64) {
    const TRANSIENT_IDLE_TIMEOUT_MS: u64 = 5;
    const EINTR: i64 = 4;
    let mut request = [0_u8; IPC_MAX_INLINE_BYTES];
    let mut reply_cap = 0_u64;
    let mut sender_pid = 0_u64;
    let mut sender_tid = 0_u64;
    loop {
        // Drain any immediately queued request before parking on the endpoint.
        // SAFETY: every output is exclusive and live for the non-blocking receive.
        let mut received = unsafe {
            ipc::try_recv_with_sender(
                endpoint,
                request.as_mut_ptr(),
                request.len(),
                &mut reply_cap as *mut u64,
                &mut sender_pid as *mut u64,
                &mut sender_tid as *mut u64,
            )
        };
        if received < 0 {
            if pool::slow_ops_in_flight() == 0 {
                break;
            }
            let args = rustos_user_abi::syscall::IpcRecvWithSenderArgs {
                endpoint,
                request_ptr: request.as_mut_ptr() as u64,
                request_capacity: request.len() as u64,
                reply_cap_ptr: (&mut reply_cap as *mut u64) as u64,
                sender_pid_ptr: (&mut sender_pid as *mut u64) as u64,
                sender_tid_ptr: (&mut sender_tid as *mut u64) as u64,
            };
            // Park on the endpoint with a short bounded budget instead of spinning
            // on `sched_yield`, so idle helpers consume zero scheduling budget.
            // SAFETY: `args` and all output buffers are live and exclusive.
            received = unsafe {
                rustos_svc_runtime::syscall::syscall2(
                    rustos_user_abi::syscall::SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED,
                    (&args as *const rustos_user_abi::syscall::IpcRecvWithSenderArgs) as u64,
                    TRANSIENT_IDLE_TIMEOUT_MS,
                )
            };
        }
        if received >= 0 {
            drain_lifecycle_events();
            let response = handle_wire_request(received as usize, &request, sender_pid, sender_tid);
            // SAFETY: `reply_cap` is the one-shot capability from the receive
            // above and `response` lives for the synchronous reply copy.
            let _ = unsafe { ipc::reply(reply_cap, response.as_ptr(), response.len()) };
            continue;
        }
        if pool::slow_ops_in_flight() == 0 {
            break;
        }
        if received != -i64::from(EAGAIN) && received != -EINTR {
            break;
        }
    }
}

fn receive_request(
    endpoint: u64,
    request: &mut [u8; IPC_MAX_INLINE_BYTES],
    reply_cap: &mut u64,
    sender_pid: &mut u64,
    sender_tid: &mut u64,
) -> i64 {
    // SAFETY: every output is exclusive and live for the whole blocking receive.
    unsafe {
        ipc::recv_with_sender(
            endpoint,
            request.as_mut_ptr(),
            request.len(),
            reply_cap as *mut u64,
            sender_pid as *mut u64,
            sender_tid as *mut u64,
        )
    }
}

// The reply is written directly to the IPC buffer and must not be boxed:
// rustos-svc-runtime's bootstrap allocator is bump-only during early boot.
#[allow(clippy::large_enum_variant)]
enum ProcdReply {
    Ipc(ProcdIpcResponse),
    Commercial(CommercialMaxProtocolResponse),
}

impl ProcdReply {
    fn as_ptr(&self) -> *const u8 {
        match self {
            Self::Ipc(response) => (response as *const ProcdIpcResponse).cast::<u8>(),
            Self::Commercial(response) => {
                (response as *const CommercialMaxProtocolResponse).cast::<u8>()
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Ipc(_) => size_of::<ProcdIpcResponse>(),
            Self::Commercial(_) => size_of::<CommercialMaxProtocolResponse>(),
        }
    }
}

fn handle_wire_request(
    received: usize,
    bytes: &[u8],
    sender_pid: u64,
    sender_tid: u64,
) -> ProcdReply {
    if received == size_of::<CommercialMaxProtocolRequest>() {
        let request = read_unaligned::<CommercialMaxProtocolRequest>(bytes);
        return ProcdReply::Commercial(handle_commercial_request(&request, sender_pid, sender_tid));
    }
    if received == size_of::<ProcdIpcRequest>() {
        let request = read_unaligned::<ProcdIpcRequest>(bytes);
        return ProcdReply::Ipc(handle_request(received, &request, sender_pid, sender_tid));
    }
    ProcdReply::Ipc(ProcdIpcResponse {
        status: EINVAL,
        ..ProcdIpcResponse::default()
    })
}

fn drain_lifecycle_events() {
    let _serialized = LIFECYCLE_DRAIN.lock();
    let mut events = [LifecycleEventWire::default(); LIFECYCLE_DRAIN_MAX_EVENTS];
    let mut count: u32 = 0;
    let args = LifecycleDrainBrokerArgs {
        abi_version: LIFECYCLE_DRAIN_BROKER_ABI_VERSION,
        out_events_ptr: events.as_mut_ptr() as u64,
        out_capacity: LIFECYCLE_DRAIN_MAX_EVENTS as u32,
        out_count_ptr: &mut count as *mut u32 as u64,
        ..LifecycleDrainBrokerArgs::default()
    };
    let ret = unsafe {
        syscall1(
            SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER,
            (&args as *const LifecycleDrainBrokerArgs) as u64,
        )
    };
    if ret < 0 {
        // This consumer has independent lifecycle evidence. Any failed drain,
        // especially EOVERFLOW, makes cached per-process policy identities
        // suspect; forget them all before accepting another request.
        PROCESS_SIGNAL_POLICY.lock().clear();
        THREAD_SIGNAL_POLICY.lock().clear();
        ipc::debug_line("procd: lifecycle evidence reset after drain failure");
        return;
    }
    for event in &events[..(count as usize).min(LIFECYCLE_DRAIN_MAX_EVENTS)] {
        apply_lifecycle_event(event);
    }
}

fn remove_process_threads(threads: &mut BTreeMap<(u64, u64), ThreadSignalPolicy>, pid: u64) {
    while let Some((&key, _)) = threads.range((pid, 0)..=(pid, u64::MAX)).next() {
        threads.remove(&key);
    }
}

fn apply_lifecycle_event(event: &LifecycleEventWire) {
    match event.event {
        LIFECYCLE_EVENT_EXIT => {
            PROCESS_SIGNAL_POLICY.lock().remove(&event.pid);
            let mut threads = THREAD_SIGNAL_POLICY.lock();
            remove_process_threads(&mut threads, event.pid);
        }
        LIFECYCLE_EVENT_THREAD_EXIT => {
            THREAD_SIGNAL_POLICY.lock().remove(&(event.pid, event.tid));
        }
        LIFECYCLE_EVENT_THREAD_EXIT_LOST => {
            // The kernel dropped thread-exit events, so some dead threads are
            // unnamed. Without a liveness probe, drop exactly the entries a
            // live thread can recreate unchanged: the mask is re-seeded from the
            // kernel's own copy on the thread's next request. Entries holding a
            // sigaltstack stay until their process exits.
            THREAD_SIGNAL_POLICY
                .lock()
                .retain(|_, state| state.has_sigaltstack());
        }
        _ => {}
    }
}

fn handle_request(
    received: usize,
    request: &ProcdIpcRequest,
    sender_pid: u64,
    sender_tid: u64,
) -> ProcdIpcResponse {
    let mut response = ProcdIpcResponse {
        op: request.op,
        ..ProcdIpcResponse::default()
    };
    if !identity_is_exact_sender(request.pid, request.tid, sender_pid, sender_tid) {
        response.status = EACCES;
        return response;
    }
    if let Err(errno) = validate_request(received, request) {
        response.status = errno;
        return response;
    }
    // A child's first request can be received on another thread before the
    // forking thread has copied the parent's dispositions to it.
    if request.op != PROCD_OP_FORK && !wait_for_parent_fork(request.parent_pid) {
        response.status = EAGAIN;
        return response;
    }

    match request.op {
        PROCD_OP_EXECVE | PROCD_OP_EXECVEAT => handle_exec(request, &mut response),
        PROCD_OP_FORK => handle_fork(request, &mut response),
        PROCD_OP_WAIT4 => handle_wait4(request, &mut response),
        PROCD_OP_RT_SIGACTION => handle_rt_sigaction(request, &mut response),
        PROCD_OP_RT_SIGPROCMASK => handle_rt_sigprocmask(request, &mut response),
        PROCD_OP_SIGALTSTACK => handle_sigaltstack(request, &mut response),
        PROCD_OP_TGKILL => handle_tgkill(request, &mut response),
        PROCD_OP_SELECT_SIGNAL => handle_select_signal(request, &mut response),
        _ => response.status = EINVAL,
    }
    response
}

fn handle_commercial_request(
    request: &CommercialMaxProtocolRequest,
    sender_pid: u64,
    sender_tid: u64,
) -> CommercialMaxProtocolResponse {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    if !request.subject_is_exact_sender(sender_pid, sender_tid) {
        response.status = EACCES;
        return response;
    }
    if let Err(errno) = validate_commercial_request(request) {
        response.status = errno;
        return response;
    }
    match request.header.op {
        COMMERCIAL_MAX_PROCD_OP_EXEC_TICKET => {
            response.descriptor_count = 1;
            response.descriptors[0] =
                procd_descriptor("exec-ticket", request.header.op, request.header.subject_pid);
            response.capability = procd_capability("exec-ticket", request.header.op);
        }
        COMMERCIAL_MAX_PROCD_OP_FORK_PLAN => {
            response.descriptor_count = 1;
            response.descriptors[0] =
                procd_descriptor("fork-plan", request.header.op, request.header.subject_pid);
            response.capability = procd_capability("fork-plan", request.header.op);
        }
        COMMERCIAL_MAX_PROCD_OP_THREAD_PLAN => {
            let count = THREAD_SIGNAL_POLICY.lock().len() as u64;
            response.value0 = count;
            response.descriptor_count = 1;
            response.descriptors[0] = procd_descriptor("thread-plan", request.header.op, count);
        }
        COMMERCIAL_MAX_PROCD_OP_SIGNAL_POLICY => {
            let process_count = PROCESS_SIGNAL_POLICY.lock().len() as u64;
            let thread_count = THREAD_SIGNAL_POLICY.lock().len() as u64;
            response.value0 = process_count;
            response.value1 = thread_count;
            response.descriptor_count = 1;
            response.descriptors[0] =
                procd_descriptor("signal-policy", request.header.op, process_count);
        }
        COMMERCIAL_MAX_PROCD_OP_WAIT_NAMESPACE => {
            response.descriptor_count = 1;
            response.descriptors[0] = procd_descriptor(
                "wait-namespace",
                request.header.op,
                request.header.subject_pid,
            );
        }
        COMMERCIAL_MAX_PROCD_OP_SESSION_MEMBERSHIP => {
            response.descriptor_count = 1;
            response.descriptors[0] = procd_descriptor(
                "session-membership",
                request.header.op,
                request.header.subject_pid,
            );
        }
        _ => response.status = EINVAL,
    }
    response
}

fn validate_request(received: usize, request: &ProcdIpcRequest) -> Result<(), i32> {
    if received != size_of::<ProcdIpcRequest>()
        || request.version != PROCD_IPC_ABI_VERSION
        || request.reserved0 != 0
        || request.path_len as usize > PROCD_PATH_CAPACITY
        || request.argv_bytes_len as usize > PROCD_ARG_BYTES
        || request.env_bytes_len as usize > PROCD_ENV_BYTES
        || request.payload_len as usize > PROCD_PAYLOAD_CAPACITY
        || request.argv_count as usize > LOADER_SPAWN_MAX_ARG_COUNT
        || request.env_count as usize > LOADER_SPAWN_MAX_ENV_COUNT
        || request.pid == 0
        || request.tid == 0
    {
        return Err(EINVAL);
    }
    Ok(())
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if !request.has_valid_envelope()
        || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_PROCD
        || request.path_len != 0
        || request.payload_len != 0
    {
        return Err(EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_PROCD_OP_EXEC_TICKET
        | COMMERCIAL_MAX_PROCD_OP_FORK_PLAN
        | COMMERCIAL_MAX_PROCD_OP_THREAD_PLAN
        | COMMERCIAL_MAX_PROCD_OP_SIGNAL_POLICY
        | COMMERCIAL_MAX_PROCD_OP_WAIT_NAMESPACE
        | COMMERCIAL_MAX_PROCD_OP_SESSION_MEMBERSHIP
            if request.arg0 == 0 && request.arg1 == 0 && request.arg2 == 0 && request.arg3 == 0 =>
        {
            Ok(())
        }
        _ => Err(EINVAL),
    }
}

fn procd_descriptor(label: &str, op: u16, value0: u64) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_PROCD,
        op,
        flags: 0,
        service_id: IPC_SERVICE_PROCD,
        value0,
        value1: 0,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(label, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn procd_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_PROCD as u64) << 32) | u64::from(op),
        service_id: IPC_SERVICE_PROCD,
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

fn copy_label(label: &str, target: &mut [u8], len: &mut u16) {
    let bytes = label.as_bytes();
    let count = bytes.len().min(target.len());
    target[..count].copy_from_slice(&bytes[..count]);
    *len = count as u16;
}

fn handle_exec(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    // The existing exec control lease starts before ticket/endpoint work and
    // is carried unchanged through loader staging; it is not a bulk-app grant.
    let deadline_ns = rustos_svc_runtime::syscall::monotonic_nanos().saturating_add(
        rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS * 1_000_000);
    if request.op == PROCD_OP_EXECVEAT && unsupported_execveat_flags(request.flags as u64) {
        response.status = ENOSYS;
        return;
    }
    if request.op == PROCD_OP_EXECVEAT && request.dirfd != AT_FDCWD {
        response.status = ENOSYS;
        return;
    }
    let raw_path_len = request.path_len as usize;
    if raw_path_len == 0
        || raw_path_len > request.path.len()
        || request.path[..raw_path_len].contains(&0)
    {
        response.status = EINVAL;
        return;
    }
    let exec_path = match core::str::from_utf8(&request.path[..raw_path_len]) {
        Ok(path) => path,
        Err(_) => {
            response.status = EINVAL;
            return;
        }
    };
    let exec_path_len = exec_path.len();
    if exec_path_len > PROCD_PATH_CAPACITY {
        response.status = EINVAL;
        return;
    }
    let _slow = pool::SlowOpGuard::enter();

    let ticket_args = RustosProcAuthorizeExecBrokerArgs {
        abi_version: rustos_user_abi::syscall::PROC_BROKER_ABI_VERSION,
        target_pid: request.pid,
        target_tid: request.tid,
        ..RustosProcAuthorizeExecBrokerArgs::default()
    };
    let ticket = unsafe {
        syscall1(
            SYS_RUSTOS_PROC_AUTHORIZE_EXEC_BROKER,
            (&ticket_args as *const RustosProcAuthorizeExecBrokerArgs) as u64,
        )
    };
    if ticket < 0 {
        response.status = (-ticket) as i32;
        return;
    }
    let exec_ticket = ticket as u64;

    let loader_endpoint = ipc::lookup_service_endpoint(IPC_SERVICE_LOADERD);
    if loader_endpoint < 0 {
        cancel_exec_ticket(exec_ticket, request.pid, request.tid);
        response.status = ENOENT;
        return;
    }
    let mut loader_request = LoaderSpawnRequest {
        version: LOADER_REQUEST_ABI_VERSION,
        op: LOADER_OP_EXEC_TARGET,
        flags: request.flags,
        target_pid: request.pid,
        target_tid: request.tid,
        exec_ticket,
        exec_dirfd: AT_FDCWD,
        deadline_ns,
        exec_path_len: exec_path_len as u32,
        argv_count: request.argv_count,
        env_count: request.env_count,
        argv_bytes_len: request.argv_bytes_len,
        env_bytes_len: request.env_bytes_len,
        requester_pid: {
            let pid = unsafe { syscall0(SYS_GETPID) };
            if pid <= 0 {
                response.status = EINVAL;
                cancel_exec_ticket(exec_ticket, request.pid, request.tid);
                return;
            }
            pid as u64
        },
        ..LoaderSpawnRequest::default()
    };
    loader_request.exec_path[..exec_path_len].copy_from_slice(exec_path.as_bytes());
    loader_request.argv_bytes[..request.argv_bytes_len as usize]
        .copy_from_slice(&request.argv_bytes[..request.argv_bytes_len as usize]);
    loader_request.env_bytes[..request.env_bytes_len as usize]
        .copy_from_slice(&request.env_bytes[..request.env_bytes_len as usize]);

    let loader_response: LoaderSpawnResponse = match rustos_svc_runtime::loader_client::call(loader_endpoint as u64, &loader_request, None) {
        Ok(reply) => reply,
        Err(errno) => {
            cancel_exec_ticket(exec_ticket, request.pid, request.tid);
            response.status = errno;
            return;
        }
    };
    if loader_response.version != LOADER_REQUEST_ABI_VERSION
        || loader_response.op != LOADER_OP_EXEC_TARGET
        || loader_response.reserved0 != 0
    {
        cancel_exec_ticket(exec_ticket, request.pid, request.tid);
        response.status = EINVAL;
        return;
    }
    response.status = loader_response.status;
    response.result = loader_response.pid;
    if response.status == 0 {
        reset_signal_policy_after_exec(request.pid, request.tid);
    } else {
        cancel_exec_ticket(exec_ticket, request.pid, request.tid);
    }
}

fn cancel_exec_ticket(exec_ticket: u64, target_pid: u64, target_tid: u64) {
    let args = RustosProcCancelExecBrokerArgs {
        abi_version: rustos_user_abi::syscall::PROC_BROKER_ABI_VERSION,
        exec_ticket,
        target_pid,
        target_tid,
        ..RustosProcCancelExecBrokerArgs::default()
    };
    let _ = unsafe {
        syscall1(
            SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER,
            (&args as *const RustosProcCancelExecBrokerArgs) as u64,
        )
    };
}

fn unsupported_execveat_flags(flags: u64) -> bool {
    flags != 0
}

fn handle_fork(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    if !valid_process_fork_plan(request) {
        response.status = EINVAL;
        return;
    }
    let clone_flags = request.arg0;
    let _fork = ForkInFlight::begin(request.pid);
    let _slow = pool::SlowOpGuard::enter();
    let args = RustosProcForkBrokerArgs {
        abi_version: rustos_user_abi::syscall::PROC_BROKER_ABI_VERSION,
        source_pid: request.pid,
        source_tid: request.tid,
        clone_flags,
        stack_ptr: request.arg1,
        ptid_ptr: request.arg2,
        ctid_ptr: request.arg3,
        tls: request.arg4,
        registers: request.registers,
        ..RustosProcForkBrokerArgs::default()
    };
    let pid = unsafe {
        syscall1(
            SYS_RUSTOS_PROC_FORK_BROKER,
            (&args as *const RustosProcForkBrokerArgs) as u64,
        )
    };
    if pid < 0 {
        response.status = (-pid) as i32;
    } else {
        response.result = pid;
        inherit_signal_policy_after_fork(request.pid, request.tid, pid as u64, clone_flags);
    }
}

/// Marks `parent` as forking from before the kernel can publish the child
/// until its signal policy has been inherited.
struct ForkInFlight(u64);

impl ForkInFlight {
    fn begin(parent: u64) -> Self {
        *FORKS_IN_FLIGHT.lock().entry(parent).or_insert(0) += 1;
        Self(parent)
    }
}

impl Drop for ForkInFlight {
    fn drop(&mut self) {
        {
            let mut forks = FORKS_IN_FLIGHT.lock();
            if let Some(count) = forks.get_mut(&self.0) {
                *count -= 1;
                if *count == 0 {
                    forks.remove(&self.0);
                }
            }
        }
        // ORDERING: Release ensures the FORKS_IN_FLIGHT removal above is visible
        // before any woken waiter re-reads `parent_is_forking`.
        FORK_IN_FLIGHT_EPOCH.fetch_add(1, core::sync::atomic::Ordering::Release);
        #[cfg(not(test))]
        // SAFETY: `FORK_IN_FLIGHT_EPOCH` is a static aligned `AtomicU32`.
        unsafe {
            let _ = rustos_svc_runtime::syscall::futex_wake_private(
                FORK_IN_FLIGHT_EPOCH.as_ptr(),
                u32::MAX,
            );
        }
    }
}

fn parent_is_forking(parent: u64) -> bool {
    parent != 0 && FORKS_IN_FLIGHT.lock().contains_key(&parent)
}

/// Blocks while `parent_pid` is inside a fork, so the child it may be creating
/// never observes (or overwrites) policy before inheritance. `false` means
/// the bound expired and the request must fail rather than run on state that
/// may be missing.
fn wait_for_parent_fork(parent_pid: u64) -> bool {
    if !parent_is_forking(parent_pid) {
        return true;
    }
    // Keep endpoint service capacity available while this worker waits for the
    // parent's fork inheritance to settle.
    let _slow = pool::SlowOpGuard::enter();
    let deadline = rustos_svc_runtime::syscall::monotonic_nanos().saturating_add(FORK_INHERIT_WAIT_NS);
    loop {
        // ORDERING: Acquire pairs with the Release bump in `ForkInFlight::drop`
        // so we cannot miss a concurrent completion between checking the map
        // and entering the futex sleep.
        let epoch = FORK_IN_FLIGHT_EPOCH.load(core::sync::atomic::Ordering::Acquire);
        if !parent_is_forking(parent_pid) {
            return true;
        }
        let now = rustos_svc_runtime::syscall::monotonic_nanos();
        if now >= deadline {
            return false;
        }
        #[cfg(not(test))]
        {
            const FUTEX_WAIT_PRIVATE: u64 = 128;
            let wait_ns = deadline.saturating_sub(now).min(100_000_000);
            let ts = rustos_svc_runtime::syscall::KTimespec {
                tv_sec: (wait_ns / 1_000_000_000) as i64,
                tv_nsec: (wait_ns % 1_000_000_000) as i64,
            };
            // SAFETY: `FORK_IN_FLIGHT_EPOCH` and `ts` are live, aligned, and
            // read-only for the kernel across the bounded private futex wait.
            let _ = unsafe {
                rustos_svc_runtime::syscall::syscall4(
                    rustos_svc_runtime::syscall::SYS_FUTEX,
                    FORK_IN_FLIGHT_EPOCH.as_ptr() as u64,
                    FUTEX_WAIT_PRIVATE,
                    u64::from(epoch),
                    (&ts as *const rustos_svc_runtime::syscall::KTimespec) as u64,
                )
            };
        }
        #[cfg(test)]
        {
            let _ = epoch;
            std::thread::yield_now();
        }
    }
}

/// Extra receive threads on procd's endpoint. A thread is created through the
/// kernel's own policy-owner path for procd, exactly like the heartbeat
/// thread, so it never needs procd to plan it. If the kernel refuses (clone
/// Transient overflow receive workers for slow fork/exec operations.
///
/// Every resident service thread consumes one slot of the 256-slot task table
/// that application launch policy reserves against (Chrome requests 144 slots).
/// Keeping zero resident workers preserves full launch headroom while still
/// spawning up to `TRANSIENT_WORKERS` short-lived helpers onto static stacks
/// whenever a fork or exec is in flight, releasing the stack in the same asm
/// block that exits the thread (mirroring vfsd's snapshot stage threads).
#[cfg(not(test))]
mod pool {
    use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

    const TRANSIENT_WORKERS: usize = 6;
    const STACK_BYTES: usize = 256 * 1024;
    const STACK_FREE: u32 = 0;
    const STACK_BUSY: u32 = 1;
    const SYS_EXIT: u64 = 60;

    #[repr(align(16))]
    // LAYOUT: the field is only addressed, never read, as a worker's stack.
    struct Stack(#[allow(dead_code)] [u8; STACK_BYTES]);

    // Each stack slot is exclusively owned by one transient worker from the
    // `STACK_FREE -> STACK_BUSY` CAS until `exit_releasing_stack` stores
    // `STACK_FREE` immediately before `SYS_EXIT`.
    static mut STACKS: [Stack; TRANSIENT_WORKERS] = [const { Stack([0; STACK_BYTES]) }; TRANSIENT_WORKERS];
    static STACK_STATE: [AtomicU32; TRANSIENT_WORKERS] = [const { AtomicU32::new(STACK_FREE) }; TRANSIENT_WORKERS];
    static SLOW_OPS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    static ENDPOINT: AtomicU64 = AtomicU64::new(0);

    pub struct SlowOpGuard;

    impl SlowOpGuard {
        pub fn enter() -> Self {
            // ORDERING: AcqRel publishes the active slow-op count before a
            // newly spawned or running helper checks `slow_ops_in_flight`.
            SLOW_OPS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
            maybe_spawn_helper();
            Self
        }
    }

    impl Drop for SlowOpGuard {
        fn drop(&mut self) {
            // ORDERING: Release pairs with the Acquire load in
            // `slow_ops_in_flight` so helpers observe completion before exit.
            SLOW_OPS_IN_FLIGHT.fetch_sub(1, Ordering::Release);
        }
    }

    pub fn publish_endpoint(endpoint: u64) {
        // ORDERING: Release pairs with the Acquire load in `entry`.
        ENDPOINT.store(endpoint, Ordering::Release);
    }

    pub fn slow_ops_in_flight() -> usize {
        // ORDERING: Acquire pairs with the Release decrement in `SlowOpGuard::drop`.
        SLOW_OPS_IN_FLIGHT.load(Ordering::Acquire)
    }

    fn current_worker_index() -> usize {
        let sp: usize;
        // SAFETY: reads the current thread's stack pointer only.
        unsafe {
            core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags));
        }
        let base = core::ptr::addr_of!(STACKS) as usize;
        (sp - base - 1) / STACK_BYTES
    }

    fn exit_releasing_stack(state: &AtomicU32) -> ! {
        // SAFETY: `state` is a static atomic; storing `STACK_FREE` has release
        // semantics on x86 and the immediately following `SYS_EXIT` never
        // touches the stack again, so a subsequent clone onto this stack
        // cannot race with this exiting thread.
        unsafe {
            core::arch::asm!(
                "mov dword ptr [{state}], {free}",
                "syscall",
                state = in(reg) state.as_ptr(),
                free = const STACK_FREE,
                in("rax") SYS_EXIT,
                in("rdi") 0_u64,
                options(noreturn),
            );
        }
    }

    extern "C" fn entry() -> ! {
        let index = current_worker_index();
        // ORDERING: Acquire pairs with the Release store in `publish_endpoint`.
        let endpoint = ENDPOINT.load(Ordering::Acquire);
        if endpoint != 0 {
            super::serve_transient(endpoint);
        }
        exit_releasing_stack(&STACK_STATE[index])
    }

    fn maybe_spawn_helper() {
        // ORDERING: Acquire pairs with `publish_endpoint`.
        if ENDPOINT.load(Ordering::Acquire) == 0 {
            return;
        }
        let target = SLOW_OPS_IN_FLIGHT
            .load(Ordering::Acquire)
            .min(TRANSIENT_WORKERS);
        let mut active = STACK_STATE
            .iter()
            .filter(|state| state.load(Ordering::Acquire) == STACK_BUSY)
            .count();
        for index in 0..TRANSIENT_WORKERS {
            if active >= target {
                return;
            }
            // ORDERING: AcqRel claims exclusive ownership of `STACKS[index]`
            // after the previous worker released it in `exit_releasing_stack`.
            if STACK_STATE[index]
                .compare_exchange(STACK_FREE, STACK_BUSY, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            // SAFETY: `index < TRANSIENT_WORKERS` and the CAS above granted
            // exclusive ownership of this stack slot until `entry` releases it.
            let top = unsafe {
                (core::ptr::addr_of_mut!(STACKS) as *mut Stack).add(index) as *mut u8
            }
            .wrapping_add(STACK_BYTES) as u64;
            // SAFETY: `top` ends an exclusively owned 16-byte aligned static
            // stack, and `entry` never returns into this frame.
            if unsafe { rustos_svc_runtime::worker::spawn(top, entry) }.is_err() {
                // ORDERING: Release returns the unused stack slot to `STACK_FREE`.
                STACK_STATE[index].store(STACK_FREE, Ordering::Release);
                return;
            }
            active += 1;
        }
    }
}

#[cfg(test)]
mod pool {
    pub struct SlowOpGuard;
    impl SlowOpGuard {
        pub fn enter() -> Self {
            Self
        }
    }
    pub fn publish_endpoint(_endpoint: u64) {}
}

fn user_pointer_in_range(ptr: u64) -> bool {
    (LINUX_USER_SPACE_BASE..LINUX_USER_SPACE_END).contains(&ptr)
}

fn valid_process_fork_plan(request: &ProcdIpcRequest) -> bool {
    const CSIGNAL: u64 = 0xff;
    const SIGCHLD: u64 = 17;
    const CLONE_VM: u64 = 0x0000_0100;
    const CLONE_VFORK: u64 = 0x0000_4000;
    const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
    const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;

    let flags = request.arg0;
    let is_vfork = flags & (CLONE_VM | CLONE_VFORK) != 0;
    if is_vfork && (flags & (CLONE_VM | CLONE_VFORK)) != (CLONE_VM | CLONE_VFORK) {
        return false;
    }
    let supported = if is_vfork {
        CSIGNAL
            | CLONE_VM
            | CLONE_VFORK
            | CLONE_CLEAR_SIGHAND
            | CLONE_PARENT_SETTID
            | CLONE_CHILD_SETTID
            | CLONE_CHILD_CLEARTID
    } else {
        CSIGNAL | CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID
    };
    let exit_signal = flags & CSIGNAL;
    flags & !supported == 0
        && (exit_signal == 0 || exit_signal == SIGCHLD)
        && (request.arg1 == 0 || user_pointer_in_range(request.arg1))
        && ((flags & CLONE_PARENT_SETTID == 0 && request.arg2 == 0)
            || (flags & CLONE_PARENT_SETTID != 0 && user_pointer_in_range(request.arg2)))
        && request.arg4 == 0
        && (flags & (CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID) == 0
            || user_pointer_in_range(request.arg3))
}

fn handle_wait4(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    let pid = request.arg0 as i64;
    if request.arg1 & !(WNOHANG | WUNTRACED | WCONTINUED) != 0 || pid < -1 || pid == 0 {
        response.status = EINVAL;
    }
}

fn handle_rt_sigaction(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    let has_action = request.flags & 0x1 != 0;
    let wants_old = request.flags & 0x2 != 0;
    if request.arg0 == 0
        || request.arg0 >= 65
        || has_action && request.payload_len as usize != LINUX_SIGACTION_SIZE
    {
        response.status = EINVAL;
        return;
    }
    let signal = request.arg0 as usize;
    if has_action && (signal == SIGKILL || signal == SIGSTOP) {
        response.status = EINVAL;
        return;
    }
    let mut policy = PROCESS_SIGNAL_POLICY.lock();
    let old = if has_action {
        // Only a mutation needs a per-process record; a pure query must not
        // allocate a 2 KiB table for a process that never changed a handler.
        let state = policy.entry(request.pid).or_default();
        let old = state.sigactions[signal];
        state.sigactions[signal] = read_sigaction(&request.payload[..LINUX_SIGACTION_SIZE]);
        old
    } else {
        policy
            .get(&request.pid)
            .map(|state| state.sigactions[signal])
            .unwrap_or_default()
    };
    if wants_old {
        response.payload[..LINUX_SIGACTION_SIZE].copy_from_slice(sigaction_bytes(&old));
        response.payload_len = LINUX_SIGACTION_SIZE as u32;
    }
}

fn handle_rt_sigprocmask(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    let has_set = request.flags & 0x1 != 0;
    let wants_old = request.flags & 0x2 != 0;
    if has_set
        && (request.payload_len != 8
            || !matches!(request.arg0, SIG_BLOCK | SIG_UNBLOCK | SIG_SETMASK))
    {
        response.status = EINVAL;
        return;
    }
    let requested = if has_set {
        read_u64(&request.payload[..8], 0)
    } else {
        0
    };
    let mut policy = THREAD_SIGNAL_POLICY.lock();
    if !has_set {
        // A pure query reports the cached mask or the kernel-seeded one
        // without creating a record.
        let old = policy
            .get(&(request.pid, request.tid))
            .copied()
            .unwrap_or_else(|| ThreadSignalPolicy::initial_from_request(request))
            .signal_mask;
        if wants_old {
            response.payload[..8].copy_from_slice(&old.to_le_bytes());
            response.payload_len = 8;
        }
        return;
    }
    let state = policy
        .entry((request.pid, request.tid))
        .or_insert_with(|| ThreadSignalPolicy::initial_from_request(request));
    let old = state.signal_mask;
    if has_set {
        let unblockable = unblockable_signal_bits();
        match request.arg0 {
            SIG_BLOCK => state.signal_mask |= requested & !unblockable,
            SIG_UNBLOCK => state.signal_mask &= !requested,
            SIG_SETMASK => state.signal_mask = requested & !unblockable,
            _ => unreachable!(),
        }
    }
    if wants_old {
        response.payload[..8].copy_from_slice(&old.to_le_bytes());
        response.payload_len = 8;
    }
}

fn handle_sigaltstack(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    let wants_old = request.flags & 0x1 != 0;
    if request.payload_len as usize != size_of::<SigaltstackRequest>() {
        response.status = EINVAL;
        return;
    }
    let extra = read_unaligned::<SigaltstackRequest>(&request.payload);
    if extra.has_new != 0 {
        if extra.new_ss_flags & !SS_DISABLE != 0 {
            response.status = EINVAL;
            return;
        }
        if extra.new_ss_flags & SS_DISABLE == 0 && extra.new_ss_size < MINSIGSTKSZ {
            response.status = ENOMEM;
            return;
        }
    }
    let mut policy = THREAD_SIGNAL_POLICY.lock();
    let state = policy
        .entry((request.pid, request.tid))
        .or_insert_with(|| ThreadSignalPolicy::initial_from_request(request));
    let old = SigaltstackResponse {
        has_old: 1,
        old_ss_sp: state.sigaltstack_sp,
        old_ss_size: state.sigaltstack_size,
        old_ss_flags: if state.sigaltstack_size == 0 {
            SS_DISABLE
        } else {
            state.sigaltstack_flags
        },
        ..SigaltstackResponse::default()
    };
    if extra.has_new != 0 {
        if extra.new_ss_flags & SS_DISABLE != 0 {
            state.sigaltstack_sp = 0;
            state.sigaltstack_size = 0;
            state.sigaltstack_flags = SS_DISABLE;
        } else {
            state.sigaltstack_sp = extra.new_ss_sp;
            state.sigaltstack_size = extra.new_ss_size;
            state.sigaltstack_flags = extra.new_ss_flags;
        }
    }
    if wants_old {
        copy_payload(response, &old);
    }
}

fn handle_tgkill(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    if request.arg2 > 64 {
        response.status = EINVAL;
        return;
    }
    let signal = request.arg2 as u32;
    let args = RustosProcSignalQueueBrokerArgs {
        abi_version: rustos_user_abi::syscall::PROC_BROKER_ABI_VERSION,
        signal,
        target_pid: request.arg0,
        target_tid: request.arg1,
        sender_pid: request.pid,
        sender_tid: request.tid,
        ..RustosProcSignalQueueBrokerArgs::default()
    };
    let status = unsafe {
        syscall1(
            SYS_RUSTOS_PROC_SIGNAL_QUEUE_BROKER,
            (&args as *const RustosProcSignalQueueBrokerArgs) as u64,
        )
    };
    if status < 0 {
        response.status = (-status) as i32;
    }
}

fn handle_select_signal(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    let pending = request.arg0;
    if pending == 0 {
        response.action = PROCD_SELECT_SIGNAL_NONE;
        return;
    }
    let mask = {
        let mut threads = THREAD_SIGNAL_POLICY.lock();
        threads
            .entry((request.pid, request.tid))
            .or_insert_with(|| ThreadSignalPolicy::initial_from_request(request))
            .signal_mask
    };
    let deliverable = pending & !mask;
    if deliverable == 0 {
        response.action = PROCD_SELECT_SIGNAL_NONE;
        return;
    }
    let signal = deliverable.trailing_zeros() as usize + 1;
    if signal >= 65 {
        response.action = PROCD_SELECT_SIGNAL_NONE;
        return;
    }
    response.signal = signal as u16;
    let sigchld_events = if signal == SIGCHLD {
        let events = request.arg1 as u32;
        if request.arg1 > u64::from(u32::MAX) || events & !PROCD_SIGCHLD_EVENT_MASK != 0 {
            response.status = EINVAL;
            return;
        }
        response.reserved0 = events;
        events
    } else {
        if request.arg1 != 0 {
            response.status = EINVAL;
            return;
        }
        0
    };
    let action = PROCESS_SIGNAL_POLICY
        .lock()
        .get(&request.pid)
        .map(|state| state.sigactions[signal])
        .unwrap_or_default();
    if (signal == SIGCHLD && procd_sigchld_is_suppressed(sigchld_events, action.flags))
        || action.handler == SIG_IGN
    {
        response.action = PROCD_SELECT_SIGNAL_IGNORE;
    } else if action.handler != 0 && action.restorer != 0 {
        response.action = PROCD_SELECT_SIGNAL_HANDLER;
        response.payload[..LINUX_SIGACTION_SIZE].copy_from_slice(sigaction_bytes(&action));
        response.payload_len = LINUX_SIGACTION_SIZE as u32;
    } else if default_ignore(signal) {
        response.action = PROCD_SELECT_SIGNAL_IGNORE;
    } else if default_stop(signal) {
        response.action = PROCD_SELECT_SIGNAL_STOP;
    } else {
        response.action = PROCD_SELECT_SIGNAL_TERMINATE;
    }
}

fn signal_bit(signal: usize) -> Option<u64> {
    if signal == 0 || signal >= 65 {
        return None;
    }
    Some(1_u64 << (signal - 1))
}

fn unblockable_signal_bits() -> u64 {
    signal_bit(SIGKILL).unwrap_or(0) | signal_bit(SIGSTOP).unwrap_or(0)
}

fn default_ignore(signal: usize) -> bool {
    matches!(signal, 17 | 18 | 23 | 28)
}

fn default_stop(signal: usize) -> bool {
    matches!(signal, 19..=22)
}

#[derive(Clone, Copy, Debug)]
struct ProcessSignalPolicy {
    sigactions: [LinuxSigAction; 65],
}

impl Default for ProcessSignalPolicy {
    fn default() -> Self {
        Self {
            sigactions: [LinuxSigAction::default(); 65],
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ThreadSignalPolicy {
    signal_mask: u64,
    sigaltstack_sp: u64,
    sigaltstack_size: u64,
    sigaltstack_flags: u32,
}

impl Default for ThreadSignalPolicy {
    fn default() -> Self {
        Self {
            signal_mask: 0,
            sigaltstack_sp: 0,
            sigaltstack_size: 0,
            sigaltstack_flags: SS_DISABLE,
        }
    }
}

impl ThreadSignalPolicy {
    /// True when the entry carries state the kernel cannot re-supply.
    fn has_sigaltstack(&self) -> bool {
        let default = Self::default();
        self.sigaltstack_sp != default.sigaltstack_sp
            || self.sigaltstack_size != default.sigaltstack_size
            || self.sigaltstack_flags != default.sigaltstack_flags
    }

    fn initial_from_request(request: &ProcdIpcRequest) -> Self {
        // The seed mask is peer-supplied; SIGKILL and SIGSTOP are never
        // blockable, exactly as in `handle_rt_sigprocmask`.
        Self {
            signal_mask: request.arg5 & !unblockable_signal_bits(),
            ..Self::default()
        }
    }
}

fn inherit_signal_policy_after_fork(
    parent_pid: u64,
    parent_tid: u64,
    child_pid: u64,
    clone_flags: u64,
) {
    const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;
    // A child pid is a fresh identity: it either inherits the parent's state or
    // starts empty. Replace (or drop) any entry left by an earlier owner of the
    // same pid so a missed exit event can never leak its dispositions or mask.
    {
        let mut processes = PROCESS_SIGNAL_POLICY.lock();
        let inherited = processes.get(&parent_pid).copied().map(|mut parent| {
            if clone_flags & CLONE_CLEAR_SIGHAND != 0 {
                // CLONE_CLEAR_SIGHAND resets caught handlers; SIG_IGN stays ignored.
                for action in &mut parent.sigactions {
                    if action.handler != 1 {
                        *action = LinuxSigAction::default();
                    }
                }
            }
            parent
        });
        match inherited {
            Some(parent) => {
                processes.insert(child_pid, parent);
            }
            None => {
                processes.remove(&child_pid);
            }
        }
    }
    let mut threads = THREAD_SIGNAL_POLICY.lock();
    remove_process_threads(&mut threads, child_pid);
    if let Some(parent_thread) = threads.get(&(parent_pid, parent_tid)).copied() {
        threads.insert((child_pid, child_pid), parent_thread);
    }
}

fn reset_signal_policy_after_exec(pid: u64, tid: u64) {
    if let Some(policy) = PROCESS_SIGNAL_POLICY.lock().get_mut(&pid) {
        for action in &mut policy.sigactions {
            if action.handler != 1 {
                *action = LinuxSigAction::default();
            }
        }
    }
    let mut threads = THREAD_SIGNAL_POLICY.lock();
    let mask = threads
        .get(&(pid, tid))
        .map(|state| state.signal_mask)
        .unwrap_or(0);
    remove_process_threads(&mut threads, pid);
    threads.insert(
        (pid, tid),
        ThreadSignalPolicy {
            signal_mask: mask,
            ..ThreadSignalPolicy::default()
        },
    );
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxSigAction {
    handler: u64,
    flags: u64,
    restorer: u64,
    mask: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SigaltstackRequest {
    has_new: u8,
    _pad: [u8; 7],
    new_ss_sp: u64,
    new_ss_size: u64,
    new_ss_flags: u32,
    _pad2: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SigaltstackResponse {
    has_old: u8,
    _pad: [u8; 7],
    old_ss_sp: u64,
    old_ss_size: u64,
    old_ss_flags: u32,
    _pad2: [u8; 4],
}

fn read_sigaction(bytes: &[u8]) -> LinuxSigAction {
    LinuxSigAction {
        handler: read_u64(bytes, 0),
        flags: read_u64(bytes, 8),
        restorer: read_u64(bytes, 16),
        mask: read_u64(bytes, 24),
    }
}

fn sigaction_bytes(action: &LinuxSigAction) -> &[u8] {
    unsafe {
        core::slice::from_raw_parts(
            (action as *const LinuxSigAction).cast::<u8>(),
            size_of::<LinuxSigAction>(),
        )
    }
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn copy_payload<T>(response: &mut ProcdIpcResponse, value: &T) {
    let len = size_of::<T>();
    let bytes = unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), len) };
    response.payload[..len].copy_from_slice(bytes);
    response.payload_len = len as u32;
}

fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    assert!(bytes.len() >= size_of::<T>());
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tgkill_rejects_signal_before_narrowing() {
        let mut request = ProcdIpcRequest::default();
        request.arg2 = (1_u64 << 32) | 9;
        let mut response = ProcdIpcResponse::default();
        handle_tgkill(&request, &mut response);
        assert_eq!(response.status, EINVAL);
    }

    #[test]
    fn peer_seeded_thread_mask_never_blocks_sigkill_or_sigstop() {
        let mut request = ProcdIpcRequest::default();
        request.arg5 = u64::MAX;
        let state = ThreadSignalPolicy::initial_from_request(&request);
        assert_eq!(state.signal_mask & unblockable_signal_bits(), 0);
        assert_eq!(state.signal_mask, !unblockable_signal_bits());
    }

    #[test]
    fn fork_in_flight_gates_only_children_of_the_forking_parent() {
        let parent = 7_001;
        assert!(!parent_is_forking(parent));
        assert!(wait_for_parent_fork(parent));
        let outer = ForkInFlight::begin(parent);
        let sibling = ForkInFlight::begin(parent);
        assert!(parent_is_forking(parent));
        // Unrelated processes and a parentless request are never delayed.
        assert!(!parent_is_forking(7_002));
        assert!(!parent_is_forking(0));
        drop(outer);
        assert!(parent_is_forking(parent), "one fork is still outstanding");
        drop(sibling);
        assert!(!parent_is_forking(parent));
        assert!(FORKS_IN_FLIGHT.lock().get(&parent).is_none());
    }

    #[test]
    fn child_request_waits_for_parent_inheritance_then_runs() {
        let parent = 7_101;
        let epoch_before = FORK_IN_FLIGHT_EPOCH.load(core::sync::atomic::Ordering::Relaxed);
        let fork = ForkInFlight::begin(parent);
        let waiter = std::thread::spawn(move || wait_for_parent_fork(parent));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!waiter.is_finished(), "child must wait while the fork is open");
        drop(fork);
        assert!(waiter.join().unwrap());
        assert_ne!(
            FORK_IN_FLIGHT_EPOCH.load(core::sync::atomic::Ordering::Relaxed),
            epoch_before,
            "completing an in-flight fork must advance the wake epoch"
        );
    }

    #[test]
    fn fork_replaces_stale_child_pid_policy() {
        let (parent, child) = (u64::MAX - 10, u64::MAX - 11);
        PROCESS_SIGNAL_POLICY.lock().insert(child, ProcessSignalPolicy::default());
        THREAD_SIGNAL_POLICY.lock().insert((child, 100), ThreadSignalPolicy::default());
        inherit_signal_policy_after_fork(parent, parent, child, 0);
        assert!(!PROCESS_SIGNAL_POLICY.lock().contains_key(&child));
        assert!(THREAD_SIGNAL_POLICY.lock().keys().all(|(pid, _)| *pid != child));
    }

    fn lifecycle_event(event: u16, pid: u64, tid: u64) -> LifecycleEventWire {
        LifecycleEventWire {
            event,
            pid,
            tid,
            ..LifecycleEventWire::default()
        }
    }

    #[test]
    fn thread_exit_frees_only_that_thread() {
        let pid = u64::MAX - 20;
        // Entries own a sigaltstack so a concurrent loss-marker test, which
        // prunes the shared map, cannot remove them.
        let mut state = ThreadSignalPolicy::default();
        state.sigaltstack_size = 0x4000;
        {
            let mut threads = THREAD_SIGNAL_POLICY.lock();
            threads.insert((pid, 1), state);
            threads.insert((pid, 2), state);
        }
        PROCESS_SIGNAL_POLICY.lock().insert(pid, ProcessSignalPolicy::default());
        apply_lifecycle_event(&lifecycle_event(LIFECYCLE_EVENT_THREAD_EXIT, pid, 2));
        let threads = THREAD_SIGNAL_POLICY.lock();
        assert!(threads.contains_key(&(pid, 1)));
        assert!(!threads.contains_key(&(pid, 2)));
        assert!(PROCESS_SIGNAL_POLICY.lock().contains_key(&pid));
        drop(threads);
        apply_lifecycle_event(&lifecycle_event(LIFECYCLE_EVENT_EXIT, pid, 0));
        assert!(THREAD_SIGNAL_POLICY.lock().keys().all(|(entry, _)| *entry != pid));
    }

    #[test]
    fn lost_thread_exit_keeps_only_state_the_kernel_cannot_resupply() {
        let pid = u64::MAX - 21;
        let mut with_stack = ThreadSignalPolicy::default();
        with_stack.sigaltstack_sp = 0x7000;
        with_stack.sigaltstack_size = 0x4000;
        {
            let mut threads = THREAD_SIGNAL_POLICY.lock();
            threads.insert((pid, 1), ThreadSignalPolicy::default());
            threads.insert((pid, 2), with_stack);
        }
        apply_lifecycle_event(&lifecycle_event(LIFECYCLE_EVENT_THREAD_EXIT_LOST, 0, 0));
        let threads = THREAD_SIGNAL_POLICY.lock();
        assert!(!threads.contains_key(&(pid, 1)));
        assert!(threads.contains_key(&(pid, 2)));
    }

    #[test]
    fn select_signal_invalid_event_flags_use_positive_errno() {
        let mut request = ProcdIpcRequest::default();
        request.pid = u64::MAX;
        request.tid = u64::MAX;
        request.arg0 = 1 << (SIGCHLD - 1);
        request.arg1 = 1 << 32;
        let mut response = ProcdIpcResponse::default();
        handle_select_signal(&request, &mut response);
        assert_eq!(response.status, EINVAL);
    }

    #[test]
    fn select_signal_zero_pending_skips_thread_policy_allocation() {
        let pid = u64::MAX - 30;
        let mut request = ProcdIpcRequest::default();
        request.pid = pid;
        request.tid = pid;
        request.arg0 = 0;
        let mut response = ProcdIpcResponse::default();
        handle_select_signal(&request, &mut response);
        assert_eq!(response.action, PROCD_SELECT_SIGNAL_NONE);
        assert!(!THREAD_SIGNAL_POLICY.lock().contains_key(&(pid, pid)));
    }

    #[test]
    fn invalid_signal_ops_do_not_allocate_policy_entries() {
        let pid = u64::MAX - 31;
        let mut sigact_req = ProcdIpcRequest::default();
        sigact_req.pid = pid;
        sigact_req.tid = pid;
        sigact_req.arg0 = SIGKILL as u64;
        sigact_req.flags = 0x1;
        sigact_req.payload_len = LINUX_SIGACTION_SIZE as u32;
        let mut resp = ProcdIpcResponse::default();
        handle_rt_sigaction(&sigact_req, &mut resp);
        assert_eq!(resp.status, EINVAL);
        assert!(!PROCESS_SIGNAL_POLICY.lock().contains_key(&pid));

        let mut mask_req = ProcdIpcRequest::default();
        mask_req.pid = pid;
        mask_req.tid = pid;
        mask_req.arg0 = 99;
        mask_req.flags = 0x1;
        mask_req.payload_len = 8;
        let mut mask_resp = ProcdIpcResponse::default();
        handle_rt_sigprocmask(&mask_req, &mut mask_resp);
        assert_eq!(mask_resp.status, EINVAL);
        assert!(!THREAD_SIGNAL_POLICY.lock().contains_key(&(pid, pid)));
    }
    #[test]
    fn pure_signal_queries_do_not_allocate_policy_entries() {
        let pid = u64::MAX - 40;
        let mut act = ProcdIpcRequest::default();
        act.pid = pid;
        act.tid = pid;
        act.arg0 = 2;
        act.flags = 0x2;
        let mut resp = ProcdIpcResponse::default();
        handle_rt_sigaction(&act, &mut resp);
        assert_eq!(resp.status, 0);
        assert_eq!(resp.payload_len as usize, LINUX_SIGACTION_SIZE);
        assert!(!PROCESS_SIGNAL_POLICY.lock().contains_key(&pid));

        let mut mask = ProcdIpcRequest::default();
        mask.pid = pid;
        mask.tid = pid;
        mask.flags = 0x2;
        mask.arg5 = 0b100;
        let mut mresp = ProcdIpcResponse::default();
        handle_rt_sigprocmask(&mask, &mut mresp);
        assert_eq!(mresp.payload_len, 8);
        assert_eq!(read_u64(&mresp.payload[..8], 0), 0b100);
        assert!(!THREAD_SIGNAL_POLICY.lock().contains_key(&(pid, pid)));
    }

    #[test]
    fn valid_process_fork_plan_admits_vfork_with_clear_sighand_and_tids() {
        const CLONE_VM: u64 = 0x0000_0100;
        const CLONE_VFORK: u64 = 0x0000_4000;
        const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
        const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
        const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
        const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;
        let ptr = LINUX_USER_SPACE_BASE + 0x2000;

        let mut req = ProcdIpcRequest::default();
        req.arg0 = CLONE_VM
            | CLONE_VFORK
            | CLONE_CLEAR_SIGHAND
            | CLONE_PARENT_SETTID
            | CLONE_CHILD_SETTID
            | CLONE_CHILD_CLEARTID
            | (SIGCHLD as u64);
        req.arg1 = ptr;
        req.arg2 = ptr + 8;
        req.arg3 = ptr + 16;
        assert!(valid_process_fork_plan(&req));

        // Zero stack/exit signal is valid for vfork().
        let mut bare_vfork = ProcdIpcRequest::default();
        bare_vfork.arg0 = CLONE_VM | CLONE_VFORK | (SIGCHLD as u64);
        assert!(valid_process_fork_plan(&bare_vfork));

        // CLONE_VM without CLONE_VFORK or vice versa must be refused.
        req.arg0 = CLONE_VM | (SIGCHLD as u64);
        assert!(!valid_process_fork_plan(&req));
        req.arg0 = CLONE_VFORK | (SIGCHLD as u64);
        assert!(!valid_process_fork_plan(&req));
        // CLONE_PARENT_SETTID requires an in-range pointer.
        req.arg0 = CLONE_VM | CLONE_VFORK | CLONE_PARENT_SETTID;
        req.arg2 = 0;
        assert!(!valid_process_fork_plan(&req));
    }

    #[test]
    fn clear_sighand_fork_resets_caught_handlers_but_keeps_sig_ign() {
        const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;
        let (parent, child) = (u64::MAX - 50, u64::MAX - 51);
        let mut policy = ProcessSignalPolicy::default();
        policy.sigactions[2] = LinuxSigAction {
            handler: 0x401000,
            flags: 0,
            restorer: 0,
            mask: 0,
        };
        policy.sigactions[13] = LinuxSigAction {
            handler: 1, // SIG_IGN
            flags: 0,
            restorer: 0,
            mask: 0,
        };
        PROCESS_SIGNAL_POLICY.lock().insert(parent, policy);
        inherit_signal_policy_after_fork(parent, parent, child, CLONE_CLEAR_SIGHAND);
        let processes = PROCESS_SIGNAL_POLICY.lock();
        let child_policy = processes.get(&child).expect("child policy");
        assert_eq!(child_policy.sigactions[2].handler, 0);
        assert_eq!(child_policy.sigactions[13].handler, 1);
    }

    #[test]
    fn reset_signal_policy_after_exec_preserves_mask_on_calling_tid() {
        let pid = u64::MAX - 60;
        let tid = pid + 1;
        {
            let mut threads = THREAD_SIGNAL_POLICY.lock();
            threads.insert(
                (pid, tid),
                ThreadSignalPolicy {
                    signal_mask: 0x55,
                    sigaltstack_sp: 0x8000,
                    sigaltstack_size: 0x4000,
                    sigaltstack_flags: 0,
                },
            );
            threads.insert((pid, pid), ThreadSignalPolicy::default());
        }
        reset_signal_policy_after_exec(pid, tid);
        let threads = THREAD_SIGNAL_POLICY.lock();
        let state = threads.get(&(pid, tid)).expect("exec tid policy");
        assert_eq!(state.signal_mask, 0x55);
        assert_eq!(state.sigaltstack_sp, 0);
        assert!(!threads.contains_key(&(pid, pid)));
    }
}
