#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use core::mem::size_of;

use rustos_svc_runtime::ipc;
use rustos_svc_runtime::syscall::{syscall1, syscall5};
use rustos_user_abi::syscall::{
    LifecycleDrainBrokerArgs, LifecycleEventWire, LinuxSyscallOffloadRequest,
    LinuxSyscallOffloadResponse, LoaderSpawnRequest, LoaderSpawnResponse, ProcdIpcRequest,
    ProcdIpcResponse, RustosProcAuthorizeExecBrokerArgs, RustosProcCancelExecBrokerArgs,
    RustosProcForkBrokerArgs, RustosProcSignalQueueBrokerArgs, IPC_SERVICE_LINUX_SYSCALLD,
    IPC_SERVICE_LOADERD, IPC_SERVICE_PROCD, LIFECYCLE_DRAIN_MAX_EVENTS, LIFECYCLE_EVENT_EXIT,
    LINUX_SIGACTION_SIZE, LOADER_OP_EXEC_TARGET, LOADER_REQUEST_ABI_VERSION,
    LOADER_SPAWN_MAX_ARG_COUNT, LOADER_SPAWN_MAX_ENV_COUNT, PROCD_ARG_BYTES, PROCD_ENV_BYTES,
    PROCD_IPC_ABI_VERSION, PROCD_OP_EXECVE, PROCD_OP_EXECVEAT, PROCD_OP_FORK,
    PROCD_OP_RT_SIGACTION, PROCD_OP_RT_SIGPROCMASK, PROCD_OP_SELECT_SIGNAL, PROCD_OP_SIGALTSTACK,
    PROCD_OP_TGKILL, PROCD_OP_WAIT4, PROCD_PATH_CAPACITY, PROCD_PAYLOAD_CAPACITY,
    PROCD_SELECT_SIGNAL_HANDLER, PROCD_SELECT_SIGNAL_IGNORE, PROCD_SELECT_SIGNAL_NONE,
    PROCD_SELECT_SIGNAL_TERMINATE, PROC_BROKER_ABI_VERSION, SYSCALL_OFFLOAD_ABI_VERSION,
    SYSCALL_OFFLOAD_OP_LINUX_PROCESS_EXIT, SYS_RUSTOS_IPC_CALL, SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER,
    SYS_RUSTOS_PROC_AUTHORIZE_EXEC_BROKER, SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER,
    SYS_RUSTOS_PROC_FORK_BROKER, SYS_RUSTOS_PROC_SIGNAL_QUEUE_BROKER,
};
use spin::Mutex;

const EINVAL: i32 = 22;
const ENOENT: i32 = 2;
const ENOMEM: i32 = 12;
const ENOSYS: i32 = 38;
const AT_FDCWD: u64 = -100_i64 as u64;
const WNOHANG: u64 = 1;
const SS_DISABLE: u32 = 2;
const MINSIGSTKSZ: u64 = 2048;
const SIG_BLOCK: u64 = 0;
const SIG_UNBLOCK: u64 = 1;
const SIG_SETMASK: u64 = 2;
const SIG_IGN: u64 = 1;
const SIGKILL: usize = 9;
const SIGSTOP: usize = 19;

static PROCESS_SIGNAL_POLICY: Mutex<BTreeMap<u64, ProcessSignalPolicy>> =
    Mutex::new(BTreeMap::new());
static THREAD_SIGNAL_POLICY: Mutex<BTreeMap<(u64, u64), ThreadSignalPolicy>> =
    Mutex::new(BTreeMap::new());

rustos_svc_runtime::entry!(service_main);

fn service_main() {
    let endpoint = ipc::endpoint_create();
    if endpoint < 0 {
        ipc::debug_line("procd: endpoint create failed");
        return;
    }
    if ipc::register_service_endpoint(IPC_SERVICE_PROCD, endpoint as u64) < 0 {
        ipc::debug_line("procd: endpoint register failed");
        return;
    }
    ipc::debug_line("procd: process policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    let mut drain_counter = 0u32;
    loop {
        let mut request = ProcdIpcRequest::default();
        let mut reply_cap = 0_u64;
        let received = unsafe {
            ipc::recv(
                endpoint,
                (&mut request as *mut ProcdIpcRequest).cast::<u8>(),
                size_of::<ProcdIpcRequest>(),
                &mut reply_cap as *mut u64,
            )
        };
        if received < 0 {
            rustos_svc_runtime::syscall::sleep_millis(1);
            continue;
        }

        let response = handle_request(received as usize, &request);
        let reply = unsafe {
            ipc::reply(
                reply_cap,
                (&response as *const ProcdIpcResponse).cast::<u8>(),
                size_of::<ProcdIpcResponse>(),
            )
        };
        if reply < 0 {
            ipc::debug_line("procd: reply failed");
        }

        drain_counter = drain_counter.wrapping_add(1);
        if drain_counter % 16 == 0 {
            drain_lifecycle_events();
        }
    }
}

fn drain_lifecycle_events() {
    let mut events = [LifecycleEventWire::default(); LIFECYCLE_DRAIN_MAX_EVENTS];
    let mut count: u32 = 0;
    let args = LifecycleDrainBrokerArgs {
        abi_version: PROC_BROKER_ABI_VERSION,
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
        return;
    }
    let drained = count as usize;
    let mut process_policy = PROCESS_SIGNAL_POLICY.lock();
    let mut thread_policy = THREAD_SIGNAL_POLICY.lock();
    let syscalld_endpoint = ipc::lookup_service_endpoint(IPC_SERVICE_LINUX_SYSCALLD);
    for event in &events[..drained] {
        if event.event == LIFECYCLE_EVENT_EXIT {
            process_policy.remove(&event.pid);
            thread_policy.retain(|(pid, _), _| *pid != event.pid);
            notify_syscalld_process_exit(syscalld_endpoint, event.pid);
        }
    }
}

fn notify_syscalld_process_exit(endpoint: i64, dead_pid: u64) {
    if endpoint < 0 || dead_pid == 0 {
        return;
    }
    let request = LinuxSyscallOffloadRequest {
        version: SYSCALL_OFFLOAD_ABI_VERSION,
        op: SYSCALL_OFFLOAD_OP_LINUX_PROCESS_EXIT,
        pid: dead_pid,
        tid: dead_pid,
        ..LinuxSyscallOffloadRequest::default()
    };
    let mut response = LinuxSyscallOffloadResponse::default();
    let _ = unsafe {
        syscall5(
            SYS_RUSTOS_IPC_CALL,
            endpoint as u64,
            (&request as *const LinuxSyscallOffloadRequest) as u64,
            size_of::<LinuxSyscallOffloadRequest>() as u64,
            (&mut response as *mut LinuxSyscallOffloadResponse) as u64,
            size_of::<LinuxSyscallOffloadResponse>() as u64,
        )
    };
}

fn handle_request(received: usize, request: &ProcdIpcRequest) -> ProcdIpcResponse {
    let mut response = ProcdIpcResponse {
        op: request.op,
        ..ProcdIpcResponse::default()
    };
    if let Err(errno) = validate_request(received, request) {
        response.status = errno;
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

fn handle_exec(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    if request.op == PROCD_OP_EXECVEAT && unsupported_execveat_flags(request.flags as u64) {
        response.status = ENOSYS;
        return;
    }
    if request.op == PROCD_OP_EXECVEAT && request.dirfd != AT_FDCWD {
        response.status = ENOSYS;
        return;
    }
    let path_len = request.path_len as usize;
    if path_len == 0 || request.path[..path_len].contains(&0) {
        response.status = EINVAL;
        return;
    }

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
        exec_path_len: request.path_len,
        argv_count: request.argv_count,
        env_count: request.env_count,
        argv_bytes_len: request.argv_bytes_len,
        env_bytes_len: request.env_bytes_len,
        ..LoaderSpawnRequest::default()
    };
    loader_request.exec_path[..path_len].copy_from_slice(&request.path[..path_len]);
    loader_request.argv_bytes[..request.argv_bytes_len as usize]
        .copy_from_slice(&request.argv_bytes[..request.argv_bytes_len as usize]);
    loader_request.env_bytes[..request.env_bytes_len as usize]
        .copy_from_slice(&request.env_bytes[..request.env_bytes_len as usize]);

    let mut loader_response = LoaderSpawnResponse::default();
    let call = unsafe {
        syscall5(
            SYS_RUSTOS_IPC_CALL,
            loader_endpoint as u64,
            (&loader_request as *const LoaderSpawnRequest) as u64,
            size_of::<LoaderSpawnRequest>() as u64,
            (&mut loader_response as *mut LoaderSpawnResponse) as u64,
            size_of::<LoaderSpawnResponse>() as u64,
        )
    };
    if call < 0 {
        cancel_exec_ticket(exec_ticket, request.pid, request.tid);
        response.status = (-call) as i32;
        return;
    }
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
    let clone_flags = request.arg0;
    if clone_flags & !0xff != 0 || clone_flags & process_clone_unsupported_mask() != 0 {
        response.status = EINVAL;
        return;
    }
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
        inherit_signal_policy_after_fork(request.pid, request.tid, pid as u64);
    }
}

fn process_clone_unsupported_mask() -> u64 {
    const CLONE_VM: u64 = 0x0000_0100;
    const CLONE_FS: u64 = 0x0000_0200;
    const CLONE_FILES: u64 = 0x0000_0400;
    const CLONE_SIGHAND: u64 = 0x0000_0800;
    const CLONE_THREAD: u64 = 0x0001_0000;
    CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD
}

fn handle_wait4(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    let pid = request.arg0 as i64;
    if request.arg1 & !WNOHANG != 0 || pid < -1 || pid == 0 {
        response.status = EINVAL;
    }
}

fn handle_rt_sigaction(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    let signal = request.arg0 as usize;
    let has_action = request.flags & 0x1 != 0;
    let wants_old = request.flags & 0x2 != 0;
    if signal == 0
        || signal >= 65
        || has_action && request.payload_len as usize != LINUX_SIGACTION_SIZE
    {
        response.status = EINVAL;
        return;
    }
    let mut policy = PROCESS_SIGNAL_POLICY.lock();
    let state = policy.entry(request.pid).or_default();
    let old = state.sigactions[signal];
    if has_action {
        if signal == SIGKILL || signal == SIGSTOP {
            response.status = EINVAL;
            return;
        }
        state.sigactions[signal] = read_sigaction(&request.payload[..LINUX_SIGACTION_SIZE]);
    }
    if wants_old {
        response.payload[..LINUX_SIGACTION_SIZE].copy_from_slice(sigaction_bytes(&old));
        response.payload_len = LINUX_SIGACTION_SIZE as u32;
    }
}

fn handle_rt_sigprocmask(request: &ProcdIpcRequest, response: &mut ProcdIpcResponse) {
    let has_set = request.flags & 0x1 != 0;
    let wants_old = request.flags & 0x2 != 0;
    if has_set && request.payload_len != 8 {
        response.status = EINVAL;
        return;
    }
    let requested = if has_set {
        read_u64(&request.payload[..8], 0)
    } else {
        0
    };
    let mut policy = THREAD_SIGNAL_POLICY.lock();
    let state = policy
        .entry((request.pid, request.tid))
        .or_insert_with(|| ThreadSignalPolicy::initial_from_request(request));
    let old = state.signal_mask;
    if has_set {
        let unblockable = signal_bit(SIGKILL).unwrap_or(0) | signal_bit(SIGSTOP).unwrap_or(0);
        match request.arg0 {
            SIG_BLOCK => state.signal_mask |= requested & !unblockable,
            SIG_UNBLOCK => state.signal_mask &= !requested,
            SIG_SETMASK => state.signal_mask = requested & !unblockable,
            _ => {
                response.status = EINVAL;
                return;
            }
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
        if extra.new_ss_flags & !SS_DISABLE != 0 {
            response.status = EINVAL;
            return;
        }
        if extra.new_ss_flags & SS_DISABLE != 0 {
            state.sigaltstack_sp = 0;
            state.sigaltstack_size = 0;
            state.sigaltstack_flags = SS_DISABLE;
        } else if extra.new_ss_size < MINSIGSTKSZ {
            response.status = ENOMEM;
            return;
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
    let signal = request.arg2 as u32;
    if signal > 64 {
        response.status = EINVAL;
        return;
    }
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
    let action = PROCESS_SIGNAL_POLICY
        .lock()
        .get(&request.pid)
        .map(|state| state.sigactions[signal])
        .unwrap_or_default();
    if action.handler == SIG_IGN {
        response.action = PROCD_SELECT_SIGNAL_IGNORE;
    } else if action.handler != 0 && action.restorer != 0 {
        response.action = PROCD_SELECT_SIGNAL_HANDLER;
        response.payload[..LINUX_SIGACTION_SIZE].copy_from_slice(sigaction_bytes(&action));
        response.payload_len = LINUX_SIGACTION_SIZE as u32;
    } else if default_ignore(signal) {
        response.action = PROCD_SELECT_SIGNAL_IGNORE;
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

fn default_ignore(signal: usize) -> bool {
    matches!(signal, 17 | 18 | 23 | 28)
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
    fn initial_from_request(request: &ProcdIpcRequest) -> Self {
        Self {
            signal_mask: request.arg5,
            ..Self::default()
        }
    }
}

fn inherit_signal_policy_after_fork(parent_pid: u64, parent_tid: u64, child_pid: u64) {
    let parent_policy = { PROCESS_SIGNAL_POLICY.lock().get(&parent_pid).copied() };
    if let Some(parent) = parent_policy {
        PROCESS_SIGNAL_POLICY.lock().insert(child_pid, parent);
    }
    let parent_thread = {
        THREAD_SIGNAL_POLICY
            .lock()
            .get(&(parent_pid, parent_tid))
            .copied()
    };
    if let Some(parent_thread) = parent_thread {
        THREAD_SIGNAL_POLICY
            .lock()
            .insert((child_pid, child_pid), parent_thread);
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
    threads.retain(|(entry_pid, _), _| *entry_pid != pid);
    threads.insert(
        (pid, pid),
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
