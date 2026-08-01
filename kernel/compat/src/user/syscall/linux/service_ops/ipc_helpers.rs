//! Bounded service-call helpers and deferred mutation settlement.
//!
//! - **Owner:** Compat owns envelope transport; each target service owns the
//!   remote object and mutation policy.
//! - **Boundary:** Service replies and local deferred-work records are admitted
//!   against the exact request, process, and provider generation.
//! - **Lifecycle:** Stage mutation, issue a finite call, commit or enqueue one
//!   retry owner, then settle/cancel on restart, exec, close, or exit.
//! - **Concurrency:** No local state lock is held across discovery or
//!   synchronous IPC; empty maintenance turns perform no discovery.
//! - **Failure:** Timeout, EPIPE, malformed response, queue full, and provider
//!   revoke retain or withdraw exactly the correct reference.
//! - **Forbidden:** No unbounded retry, allocate-under-lock queue growth,
//!   fabricated success, or stale provider replay.
//! - **Evidence:** `vfs-open-description`.
use super::*;
use alloc::string::String;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use nucleus_core::util::{
    lockdep::{LockClass, TrackedSpinLock},
    ring::RingBuffer,
};

const EARLY_SERVICE_CALL_SAMPLES: usize = 6;
const SLOW_SERVICE_CALL_THRESHOLD_MS: u64 = 10;
// Typed service calls share the synchronous debug sink with the generic IPC
// path. Bound diagnostic work independently to one representative sample per
// second so the act of reporting overload cannot sustain that overload.
const MAX_SLOW_SERVICE_CALL_LOGS_PER_SECOND: usize = 1;
// A readiness probe cannot consume an event: it only transfers authenticated
// ingress into inputd's policy queue. Bound that safe, idempotent operation so
// a wedged inputd cannot freeze uiserver's input loop for the generic service
// IPC deadline. Stateful authorize/read calls retain their completion wait
// until the ABI carries a cancellable authorization lease.
const PENDING_VFS_MUTATION_CAPACITY: usize = 32 * 1024;
const PENDING_VFS_MUTATION_STORAGE_CAPACITY: usize = PENDING_VFS_MUTATION_CAPACITY + 1;
const PENDING_VFS_PAYLOAD_CAPACITY: usize = 64;
const HOUSEKEEPING_VFS_MAINTENANCE_ATTEMPTS: usize = 1;

#[derive(Clone, Copy)]
struct PendingVfsMutation {
    op: u16,
    fd: u64,
    remote_id: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    operation_hi: u64,
    operation_lo: u64,
    payload_len: u32,
    payload: [u8; PENDING_VFS_PAYLOAD_CAPACITY],
}

static PENDING_VFS_MUTATIONS: TrackedSpinLock<
    RingBuffer<PendingVfsMutation, PENDING_VFS_MUTATION_STORAGE_CAPACITY>,
    { LockClass::VfsDeferredMutation as u8 },
> = TrackedSpinLock::new(RingBuffer::new());

static SERVICE_CALL_SAMPLE_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_SERVICE_CALL_LOG_RATE_STATE: AtomicU64 = AtomicU64::new(u64::MAX);

// RING3-MIGRATION-REFERENCE START: bootstrap-device-route exception: rootd owns
// the bootstrap manifest/restart policy and loaderd owns normal spawn policy.
// Ring0 keeps the direct spawn substrate only for the fixed pre-loaderd core
// service bootstrap plus loaderd recovery path.
pub fn syscall_linux_loader_spawn_exec(
    path_ptr: u64,
    _argv_ptr: u64,
    _envp_ptr: u64,
    flags: u64,
    console_session: u64,
    weight_micros: u64,
) -> u64 {
    const BOOTSTRAP_SPAWN_ALLOWED_FLAGS: u64 = 0x1;

    let exec_path = match copy_current_user_path(path_ptr, LOADER_SPAWN_EXEC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    if flags & !BOOTSTRAP_SPAWN_ALLOWED_FLAGS != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if !current_process_can_bootstrap_spawn() {
        return linux_errno(LINUX_EACCES);
    }
    if !can_bootstrap_spawn_direct(exec_path.as_str()) {
        return linux_errno(LINUX_EACCES);
    }

    match spawn_bootstrap_exec_direct(exec_path.as_str(), flags, console_session, weight_micros) {
        Ok(pid) => pid,
        Err(errno) => linux_errno(errno),
    }
}

fn current_process_can_bootstrap_spawn() -> bool {
    const ROOTD_EXEC_PATH: &str = "services/rootd/rootd.elf";

    multitask::with_current_process_state(|_, process_state| {
        process_state.security().is_logical_admin()
            && process_state.exec_path().trim_start_matches('/') == ROOTD_EXEC_PATH
    })
    .unwrap_or(false)
}

fn can_bootstrap_spawn_direct(exec_path: &str) -> bool {
    let path = exec_path.strip_prefix('/').unwrap_or(exec_path);
    matches!(
        path,
        "services/syscalld/syscalld.elf"
            | "services/vfsd/vfsd.elf"
            | "services/loaderd/loaderd.elf"
            | "services/procd/procd.elf"
    )
}

fn spawn_bootstrap_exec_direct(
    exec_path: &str,
    flags: u64,
    console_session: u64,
    weight_micros: u64,
) -> Result<u64, i64> {
    let loaded = crate::user::console_host::load_executable_image_by_path(exec_path)
        .map_err(console_host_error_to_linux_errno)?;
    let session = if console_session == 0 {
        multitask::current_user_snapshot()
            .map(|snapshot| snapshot.console_session())
            .ok_or(LINUX_EINVAL)?
    } else {
        crate::io::session::ConsoleSessionHandle::from_raw(console_session)
    };
    let logical_admin = flags & 0x1 != 0;
    let program = crate::user::console_host::ConsoleProgramSpec::new(
        &loaded.bytes,
        loaded.path,
        weight_micros,
    )
    .with_logical_admin(logical_admin);
    crate::user::console_host::spawn_program_in_session(session, program)
        .map(|spawned| spawned.pid)
        .map_err(console_host_error_to_linux_errno)
}
// RING3-MIGRATION-REFERENCE END: rootd/loaderd bootstrap spawn substrate exception.

fn console_host_error_to_linux_errno(error: crate::user::console_host::ConsoleHostError) -> i64 {
    match error {
        crate::user::console_host::ConsoleHostError::BootstrapBlocked => LINUX_EAGAIN,
        crate::user::console_host::ConsoleHostError::Load { error, .. } => match error {
            crate::vfs::VfsError::BadFileDescriptor => LINUX_EBADF,
            crate::vfs::VfsError::InvalidArgument => LINUX_EINVAL,
            crate::vfs::VfsError::NotFound => LINUX_ENOENT,
            crate::vfs::VfsError::NotDirectory => LINUX_ENOTDIR,
            crate::vfs::VfsError::PermissionDenied => LINUX_EACCES,
            crate::vfs::VfsError::ReadOnlyFilesystem => LINUX_EROFS,
            crate::vfs::VfsError::Unsupported => LINUX_ENOSYS,
        },
        crate::user::console_host::ConsoleHostError::Spawn { .. } => LINUX_ENOEXEC,
    }
}

pub fn copy_string_vector(
    vector_ptr: u64,
    max_count: usize,
    dest: &mut [u8],
    dest_len: &mut u32,
    dest_count: &mut u16,
) -> Result<(), i64> {
    *dest_len = 0;
    *dest_count = 0;
    if vector_ptr == 0 {
        return Ok(());
    }
    let mut cursor = vector_ptr;
    let mut offset = 0usize;
    for count in 0..max_count {
        let mut ptr_bytes = [0_u8; size_of::<u64>()];
        usermem::copy_from_current_user_exact(cursor, &mut ptr_bytes)
            .map_err(address_space_error_to_linux_errno)?;
        let value_ptr = u64::from_ne_bytes(ptr_bytes);
        if value_ptr == 0 {
            *dest_len = offset as u32;
            *dest_count = count as u16;
            return Ok(());
        }
        let value = usermem::read_current_user_c_string(value_ptr, dest.len())
            .map_err(address_space_error_to_linux_errno)?;
        let needed = value.len().checked_add(1).ok_or(LINUX_E2BIG)?;
        if offset.checked_add(needed).ok_or(LINUX_E2BIG)? > dest.len() {
            return Err(LINUX_E2BIG);
        }
        dest[offset..offset + value.len()].copy_from_slice(value.as_bytes());
        offset += value.len();
        dest[offset] = 0;
        offset += 1;
        cursor = cursor
            .checked_add(size_of::<u64>() as u64)
            .ok_or(LINUX_EINVAL)?;
    }
    Err(LINUX_E2BIG)
}

pub fn procd_exec(
    frame: &mut SyscallFrame,
    op: u16,
    dirfd: u64,
    path_ptr: u64,
    argv_ptr: u64,
    envp_ptr: u64,
    flags: u64,
) -> u64 {
    if op == PROCD_OP_EXECVEAT && (flags != 0 || !is_linux_at_fdcwd(dirfd)) {
        return linux_errno(LINUX_ENOSYS);
    }
    let raw_exec_path = match copy_current_user_path(path_ptr, PROCD_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let current_cwd = match multitask::with_current_user_process_state(|_, _, process_state| {
        String::from(process_state.cwd())
    }) {
        Some(cwd) => cwd,
        None => return linux_errno(LINUX_ESRCH),
    };
    let mut request = new_procd_request(op);
    request.dirfd = (linux_abi::AT_FDCWD as i64) as u64;
    request.flags = flags as u32;
    if raw_exec_path.len() > request.path.len() || current_cwd.len() > request.payload.len() {
        return linux_errno(LINUX_EINVAL);
    }
    request.path_len = raw_exec_path.len() as u32;
    request.path[..raw_exec_path.len()].copy_from_slice(raw_exec_path.as_bytes());
    request.payload_len = current_cwd.len() as u32;
    request.payload[..current_cwd.len()].copy_from_slice(current_cwd.as_bytes());
    if let Err(errno) = copy_string_vector(
        argv_ptr,
        LOADER_SPAWN_MAX_ARG_COUNT,
        &mut request.argv_bytes,
        &mut request.argv_bytes_len,
        &mut request.argv_count,
    ) {
        return linux_errno(errno);
    }
    if let Err(errno) = copy_string_vector(
        envp_ptr,
        LOADER_SPAWN_MAX_ENV_COUNT,
        &mut request.env_bytes,
        &mut request.env_bytes_len,
        &mut request.env_count,
    ) {
        return linux_errno(errno);
    }
    match call_procd(&request).and_then(|response| ensure_empty_procd_response(&response)) {
        Ok(()) if apply_pending_exec_transition(frame) => frame.rax,
        Ok(()) => linux_errno(LINUX_EINVAL),
        Err(errno) => linux_errno(errno),
    }
}

fn is_linux_at_fdcwd(dirfd: u64) -> bool {
    const AT_FDCWD_I64: u64 = (-100_i64) as u64;
    const AT_FDCWD_I32: u64 = 0xffff_ff9c;
    dirfd == AT_FDCWD_I64 || dirfd == AT_FDCWD_I32 || dirfd == linux_abi::AT_FDCWD as u64
}

pub fn procd_fork(
    frame: &SyscallFrame,
    clone_flags: u64,
    stack_ptr: u64,
    ptid_ptr: u64,
    ctid_ptr: u64,
    tls: u64,
) -> u64 {
    let mut request = new_procd_request(PROCD_OP_FORK);
    request.arg0 = clone_flags;
    request.arg1 = stack_ptr;
    request.arg2 = ptid_ptr;
    request.arg3 = ctid_ptr;
    request.arg4 = tls;
    request.registers = frame_to_user_registers(frame);
    match call_procd(&request) {
        Ok(response) if response.status == 0 => response.result as u64,
        Ok(response) => linux_errno(response.status.unsigned_abs() as i64),
        Err(errno) => linux_errno(errno),
    }
}

fn frame_to_user_registers(frame: &SyscallFrame) -> RustosUserRegisters {
    RustosUserRegisters {
        rax: frame.rax,
        rbx: frame.rbx,
        rcx: frame.user_rip,
        rdx: frame.rdx,
        rsi: frame.rsi,
        rdi: frame.rdi,
        rbp: frame.rbp,
        rsp: frame.user_rsp,
        rip: frame.user_rip,
        r8: frame.r8,
        r9: frame.r9,
        r10: frame.r10,
        r11: frame.user_rflags,
        r12: frame.r12,
        r13: frame.r13,
        r14: frame.r14,
        r15: frame.r15,
        rflags: frame.user_rflags,
    }
}

pub fn new_procd_request(op: u16) -> ProcdIpcRequest {
    let mut request = ProcdIpcRequest {
        op,
        ..ProcdIpcRequest::default()
    };
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.parent_pid = multitask::parent_process_id_of(snapshot.process_id()).unwrap_or(0);
    }
    if let Some(thread_state) = multitask::current_linux_thread_state() {
        request.arg5 = thread_state.signal_mask;
    }
    request
}

pub fn call_procd(request: &ProcdIpcRequest) -> Result<ProcdIpcResponse, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_PROCD,
        as_bytes(request),
        ipc_ops::ServiceIpcClass::BootControl,
    )?;
    if response.len() != size_of::<ProcdIpcResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<ProcdIpcResponse>(response.as_slice());
    validate_procd_response_envelope(request.op, &response)?;
    log_slow_service_call(
        "procd",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.status as i64,
        None,
    );
    Ok(response)
}

fn validate_procd_response_envelope(
    request_op: u16,
    response: &ProcdIpcResponse,
) -> Result<(), i64> {
    if response.version != rustos_user_abi::syscall::PROCD_IPC_ABI_VERSION
        || response.op != request_op
        || response.reserved0 != 0
        || response.reserved1 != 0
        || response.payload_len as usize > response.payload.len()
    {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub fn ensure_empty_procd_response(response: &ProcdIpcResponse) -> Result<(), i64> {
    if response.version != rustos_user_abi::syscall::PROCD_IPC_ABI_VERSION
        || response.payload_len != 0
        || response.reserved0 != 0
        || response.reserved1 != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(())
}

pub fn new_vfs_request(op: u16) -> VfsIpcRequest {
    let mut request = VfsIpcRequest {
        op,
        ..VfsIpcRequest::default()
    };
    let mut operation = [0_u8; 16];
    nucleus_core::util::random::Random::new().fill_bytes(&mut operation);
    request.operation_hi = u64::from_le_bytes(operation[..8].try_into().unwrap());
    request.operation_lo = u64::from_le_bytes(operation[8..].try_into().unwrap());
    if request.operation_hi == 0 && request.operation_lo == 0 {
        request.operation_lo = 1;
    }
    populate_vfs_identity(&mut request);
    request
}

pub fn mint_service_object_id() -> u64 {
    let mut bytes = [0_u8; 8];
    nucleus_core::util::random::Random::new().fill_bytes(&mut bytes);
    u64::from_le_bytes(bytes).max(1)
}

pub fn populate_vfs_identity(request: &mut VfsIpcRequest) {
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.session_handle = snapshot.console_session().raw();
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }
}

pub fn populate_vfs_path(request: &mut VfsIpcRequest, path: &str) -> Result<(), i64> {
    let bytes = path.as_bytes();
    if bytes.len() > VFS_IPC_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    request.path_len = bytes.len() as u32;
    request.path[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

pub fn populate_vfs_path_base(
    request: &mut VfsIpcRequest,
    dirfd: u64,
    path: &str,
) -> Result<(), i64> {
    request.dirfd = dirfd;
    if !path.starts_with('/') && !is_linux_at_fdcwd_for_vfs(dirfd) {
        let remote = current_remote_vfs_handle(dirfd).ok_or(LINUX_EBADF)?;
        if remote.kind() != multitask::RemoteVfsHandleKind::Directory {
            return Err(LINUX_ENOTDIR);
        }
        request.remote_id = remote.remote_id();
    }
    populate_vfs_path(request, path)
}

fn is_linux_at_fdcwd_for_vfs(dirfd: u64) -> bool {
    const AT_FDCWD_I64: u64 = (-100_i64) as u64;
    const AT_FDCWD_I32: u64 = 0xffff_ff9c;
    dirfd == AT_FDCWD_I64 || dirfd == AT_FDCWD_I32 || dirfd == linux_abi::AT_FDCWD as u64
}

pub fn call_vfs_ipc_request(request: &VfsIpcRequest) -> Result<VfsIpcResponse, i64> {
    call_vfs_ipc_request_impl(request, None)
}

pub fn call_vfs_ipc_request_with_timeout(
    request: &VfsIpcRequest,
    timeout_ms: u64,
) -> Result<VfsIpcResponse, i64> {
    call_vfs_ipc_request_impl(request, Some(timeout_ms.max(1)))
}

fn vfs_request_is_replay_safe(request: &VfsIpcRequest) -> bool {
    matches!(
        request.op,
        VFS_IPC_OP_OPENAT
            | VFS_IPC_OP_CLOSE
            | VFS_IPC_OP_READ
            | VFS_IPC_OP_LSEEK
            | VFS_IPC_OP_GETDENTS64
            | VFS_IPC_OP_FCNTL
            | VFS_IPC_OP_CURSOR_SETTLE
            | VFS_IPC_OP_CHECKPOINT_ACK
    ) || request.op == VFS_IPC_OP_POLL_QUERY
        && matches!(
            request.arg0,
            VFS_POLL_QUERY_EPOLL_CREATE
                | VFS_POLL_QUERY_EPOLL_CTL
                | VFS_POLL_QUERY_EPOLL_RETIRE
                | VFS_POLL_QUERY_EPOLL_PURGE_OBJECT
                | VFS_POLL_QUERY_EPOLL_SNAPSHOT
        )
}

fn call_vfs_ipc_request_impl(
    request: &VfsIpcRequest,
    timeout_ms: Option<u64>,
) -> Result<VfsIpcResponse, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let replay_safe_request = vfs_request_is_replay_safe(request);
    let deferred_reconcile = matches!(
        request.op,
        VFS_IPC_OP_CLOSE | VFS_IPC_OP_CURSOR_SETTLE | VFS_IPC_OP_CHECKPOINT_ACK
    ) || request.op == VFS_IPC_OP_POLL_QUERY
        && matches!(
            request.arg0,
            VFS_POLL_QUERY_EPOLL_CREATE
                | VFS_POLL_QUERY_EPOLL_CTL
                | VFS_POLL_QUERY_EPOLL_RETIRE
                | VFS_POLL_QUERY_EPOLL_PURGE_OBJECT
        );
    let total_timeout_ms = timeout_ms.or(replay_safe_request.then_some(30_000));
    let attempts = if replay_safe_request {
        total_timeout_ms
            .map(|total| usize::try_from(total).unwrap_or(usize::MAX).min(3))
            .unwrap_or(3)
            .max(1)
    } else {
        1
    };
    let mut response = None;
    let mut last_errno = LINUX_ETIMEDOUT;
    for attempt in 0..attempts {
        let attempt_timeout_ms =
            total_timeout_ms.map(|total| split_retry_timeout_ms(total, attempts, attempt));
        let result = match attempt_timeout_ms {
            Some(timeout_ms) => ipc_ops::call_service_endpoint_with_class_deadline(
                IPC_SERVICE_VFSD,
                as_bytes(request),
                ipc_ops::ServiceIpcClass::BulkData,
                timeout_ms,
            ),
            None => ipc_ops::call_service_endpoint_with_class(
                IPC_SERVICE_VFSD,
                as_bytes(request),
                ipc_ops::ServiceIpcClass::BulkData,
            ),
        };
        match result {
            Ok(bytes) => {
                response = Some(bytes);
                break;
            }
            Err(errno)
                if attempts > 1
                    && matches!(errno, LINUX_ETIMEDOUT | LINUX_EPIPE | LINUX_ENOSYS) =>
            {
                last_errno = errno;
                multitask::cond_resched();
            }
            Err(errno) => return Err(errno),
        }
    }
    let response = match response {
        Some(response) => response,
        None => {
            if deferred_reconcile {
                enqueue_pending_vfs_mutation(request)?;
            }
            return Err(last_errno);
        }
    };
    let detail = vfs_request_log_detail(request);
    log_slow_service_call(
        "vfsd",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.len() as i64,
        detail.as_deref(),
    );
    if response.len() != size_of::<VfsIpcResponse>() {
        if deferred_reconcile {
            enqueue_pending_vfs_mutation(request)?;
        }
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<VfsIpcResponse>(response.as_slice());
    if let Err(errno) = validate_vfs_response_envelope(request.op, &response) {
        if deferred_reconcile {
            enqueue_pending_vfs_mutation(request)?;
        }
        return Err(errno);
    }
    if response.status == 0 && vfs_checkpoint_ack_required(request) {
        let _ = acknowledge_vfs_checkpoint_mutation(request);
    }
    Ok(response)
}

fn split_retry_timeout_ms(total: u64, attempts: usize, attempt: usize) -> u64 {
    debug_assert!(attempts > 0 && attempt < attempts);
    let attempts = attempts as u64;
    total / attempts + u64::from((attempt as u64) < total % attempts)
}

fn vfs_checkpoint_ack_required(request: &VfsIpcRequest) -> bool {
    request.op == VFS_IPC_OP_CLOSE
        || request.op == VFS_IPC_OP_POLL_QUERY
            && (request.arg0 == VFS_POLL_QUERY_EPOLL_RETIRE
                || request.arg0 == VFS_POLL_QUERY_EPOLL_PURGE_OBJECT
                || request.arg0 == VFS_POLL_QUERY_EPOLL_CTL
                    && request.arg1 == linux_abi::EPOLL_CTL_DEL)
}

fn acknowledge_vfs_checkpoint_mutation(original: &VfsIpcRequest) -> Result<(), i64> {
    let ack = vfs_checkpoint_ack_request(original);
    let mut last_errno = LINUX_ETIMEDOUT;
    for attempt in 0..3 {
        match ipc_ops::call_service_endpoint_with_class_deadline(
            IPC_SERVICE_VFSD,
            as_bytes(&ack),
            ipc_ops::ServiceIpcClass::ReadinessQuery,
            split_retry_timeout_ms(16, 3, attempt),
        ) {
            Ok(bytes) if bytes.len() == size_of::<VfsIpcResponse>() => {
                let response = read_unaligned::<VfsIpcResponse>(&bytes);
                if validate_vfs_response_envelope(ack.op, &response).is_ok() && response.status == 0
                {
                    return Ok(());
                }
                last_errno = if response.status == 0 {
                    LINUX_EINVAL
                } else {
                    i64::from(response.status.unsigned_abs())
                };
            }
            Ok(_) => last_errno = LINUX_EINVAL,
            Err(errno) => last_errno = errno,
        }
        multitask::cond_resched();
    }
    enqueue_pending_vfs_mutation(&ack)?;
    Err(last_errno)
}

fn vfs_checkpoint_ack_request(original: &VfsIpcRequest) -> VfsIpcRequest {
    let mut ack = new_vfs_request(VFS_IPC_OP_CHECKPOINT_ACK);
    ack.remote_id = original.remote_id;
    ack.arg0 = original.operation_hi;
    ack.arg1 = original.operation_lo;
    ack.arg2 = u64::from(original.op);
    ack.arg3 = if original.op == VFS_IPC_OP_POLL_QUERY {
        original.arg0
    } else {
        0
    };
    ack
}

pub fn settle_vfs_cursor_mutation(prepared: &VfsIpcRequest, commit: bool) -> Result<(), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_CURSOR_SETTLE);
    request.fd = prepared.fd;
    request.remote_id = prepared.remote_id;
    request.arg0 = prepared.operation_hi;
    request.arg1 = prepared.operation_lo;
    request.arg2 = if commit {
        VFS_CURSOR_SETTLE_COMMIT
    } else {
        VFS_CURSOR_SETTLE_CANCEL
    };
    let response = call_vfs_ipc_request_with_timeout(&request, 16)?;
    ensure_vfs_status(&response)
}

fn enqueue_pending_vfs_mutation(request: &VfsIpcRequest) -> Result<(), i64> {
    let payload_len = request.payload_len as usize;
    if payload_len > PENDING_VFS_PAYLOAD_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let mut queue = PENDING_VFS_MUTATIONS.lock();
    if queue.any(|entry| {
        if request.op == VFS_IPC_OP_CHECKPOINT_ACK {
            entry.op == VFS_IPC_OP_CHECKPOINT_ACK
                && entry.arg0 == request.arg0
                && entry.arg1 == request.arg1
        } else {
            entry.operation_hi == request.operation_hi && entry.operation_lo == request.operation_lo
        }
    }) {
        return Ok(());
    }
    if queue.len() >= PENDING_VFS_MUTATION_CAPACITY {
        return Err(LINUX_ENOSPC);
    }
    let mut payload = [0_u8; PENDING_VFS_PAYLOAD_CAPACITY];
    payload[..payload_len].copy_from_slice(&request.payload[..payload_len]);
    let admitted = queue.push(PendingVfsMutation {
        op: request.op,
        fd: request.fd,
        remote_id: request.remote_id,
        arg0: request.arg0,
        arg1: request.arg1,
        arg2: request.arg2,
        operation_hi: request.operation_hi,
        operation_lo: request.operation_lo,
        payload_len: request.payload_len,
        payload,
    });
    assert!(admitted, "vfs mutation admission capacity must be reserved");
    Ok(())
}

pub(super) fn service_deferred_vfs_mutations() -> usize {
    drain_pending_vfs_mutations()
}

fn drain_pending_vfs_mutations() -> usize {
    // This path is owned by the dedicated nucleus housekeeping task, not a
    // foreground syscall tail. Keep one transaction per yielded turn, but
    // give that transaction the normal bounded control deadline. A 1 ms
    // cancel/retry loop was shorter than an SMP service round-trip: vfsd then
    // consumed an endless stream of already-revoked replies and starved boot.
    let mut attempted = 0usize;
    for _ in 0..HOUSEKEEPING_VFS_MAINTENANCE_ATTEMPTS {
        let Some(pending) = PENDING_VFS_MUTATIONS.lock().pop() else {
            return attempted;
        };
        attempted += 1;
        let mut request = new_vfs_request(pending.op);
        request.fd = pending.fd;
        request.remote_id = pending.remote_id;
        request.arg0 = pending.arg0;
        request.arg1 = pending.arg1;
        request.arg2 = pending.arg2;
        request.operation_hi = pending.operation_hi;
        request.operation_lo = pending.operation_lo;
        request.payload_len = pending.payload_len;
        let payload_len = pending.payload_len as usize;
        request.payload[..payload_len].copy_from_slice(&pending.payload[..payload_len]);
        let response = ipc_ops::call_service_endpoint_with_class_deadline(
            IPC_SERVICE_VFSD,
            as_bytes(&request),
            ipc_ops::ServiceIpcClass::InteractiveControl,
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
        )
        .ok()
        .and_then(|bytes| {
            (bytes.len() == size_of::<VfsIpcResponse>())
                .then(|| read_unaligned::<VfsIpcResponse>(&bytes))
        });
        let completed = response.is_some_and(|response| {
            validate_vfs_response_envelope(request.op, &response).is_ok() && response.status == 0
        });
        if !completed {
            assert!(
                PENDING_VFS_MUTATIONS.lock().push_front(pending),
                "popped vfs mutation must have retry capacity"
            );
            return attempted;
        }
        if vfs_checkpoint_ack_required(&request) {
            let ack = vfs_checkpoint_ack_request(&request);
            if enqueue_pending_vfs_mutation(&ack).is_err() {
                assert!(
                    PENDING_VFS_MUTATIONS.lock().push_front(pending),
                    "popped vfs mutation must have retry capacity"
                );
                return attempted;
            }
        }
    }
    attempted
}

fn validate_vfs_response_envelope(request_op: u16, response: &VfsIpcResponse) -> Result<(), i64> {
    if response.version != VFS_IPC_ABI_VERSION
        || response.op != request_op
        || response.reserved0 != 0
        || response.payload_len as usize > response.payload.len()
    {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub fn current_input_device_access(fd: u64) -> Option<(u16, u64)> {
    current_input_device_description(fd).map(|(_, access, flags)| (access, flags))
}

pub fn current_input_device_description(fd: u64) -> Option<(u64, u16, u64)> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let entry = process_state.handles().get_entry(fd)?;
        if !entry.rights().allows_read() {
            return None;
        }
        let handle = entry.handle();
        let device = handle.device_handle()?;
        if device.device_id() != kernel_object::api::device::DeviceId::Input {
            return None;
        }
        let access = match device.access_kind() {
            kernel_object::api::device::DeviceAccessKind::Native => INPUTD_ACCESS_NATIVE,
            kernel_object::api::device::DeviceAccessKind::Evdev => INPUTD_ACCESS_EVDEV,
        };
        Some((device.token_id(), access, entry.status_flags()))
    })
    .flatten()
}

pub fn current_fd_status_flags(fd: u64) -> Option<u64> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        Some(process_state.handles().get_entry(fd)?.status_flags())
    })
    .flatten()
}

pub fn read_input_device_via_inputd(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    inputd_access: u16,
    status_flags: u64,
) -> u64 {
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    let mut request = InputdIpcRequest {
        version: INPUTD_IPC_ABI_VERSION,
        op: INPUTD_IPC_OP_READ,
        flags: ((status_flags & linux_abi::O_NONBLOCK != 0) as u32) * INPUTD_READ_FLAG_NONBLOCK,
        fd,
        access: inputd_access,
        requested_len: user_len.min(INPUTD_READ_PAYLOAD_CAPACITY) as u64,
        ..InputdIpcRequest::default()
    };
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
    }
    request.op = INPUTD_IPC_OP_AUTHORIZE_READ;
    if let Err(errno) = call_inputd_ipc_request(&request) {
        return linux_errno(errno);
    }
    request.op = INPUTD_IPC_OP_READ;
    let response = match call_inputd_read_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.payload_len as u64 > request.requested_len {
        return linux_errno(LINUX_EINVAL);
    }
    let read = response.payload_len as usize;
    if read == 0 {
        if status_flags & linux_abi::O_NONBLOCK != 0 {
            return linux_errno(LINUX_EAGAIN);
        }
        return 0;
    }
    if user_ptr.checked_add(read as u64).is_none() {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(err) = usermem::write_current_user_bytes(user_ptr, &response.payload[..read]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    read as u64
}

fn call_inputd_ipc_request(request: &InputdIpcRequest) -> Result<InputdIpcResponse, i64> {
    let response = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_INPUTD,
        as_bytes(request),
        ipc_ops::ServiceIpcClass::InteractiveControl,
    )?;
    if response.len() != size_of::<InputdIpcResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<InputdIpcResponse>(response.as_slice());
    if response.version != INPUTD_IPC_ABI_VERSION
        || response.op != request.op
        || response.flags != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.approved_len == 0 || response.approved_len > request.requested_len {
        return Err(LINUX_EINVAL);
    }
    Ok(response)
}

pub fn input_device_readiness_for_access_with_timeout(
    access: u16,
    timeout_ms: u64,
) -> Result<(bool, u64), i64> {
    if !matches!(access, INPUTD_ACCESS_NATIVE | INPUTD_ACCESS_EVDEV) {
        return Err(LINUX_EINVAL);
    }
    let mut request = InputdIpcRequest {
        version: INPUTD_IPC_ABI_VERSION,
        op: INPUTD_IPC_OP_STATS,
        access,
        requested_len: 1,
        ..InputdIpcRequest::default()
    };
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
    }
    let response = ipc_ops::call_service_endpoint_with_class_deadline(
        IPC_SERVICE_INPUTD,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::ReadinessQuery,
        timeout_ms,
    )?;
    if response.len() != size_of::<InputdIpcResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<InputdIpcResponse>(response.as_slice());
    if response.version != INPUTD_IPC_ABI_VERSION
        || response.op != INPUTD_IPC_OP_STATS
        || response.flags != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.stats.readiness_generation == 0 {
        return Err(LINUX_EINVAL);
    }
    let pending_flags =
        INPUT_STATS_FLAG_PENDING_COALESCED | INPUT_STATS_FLAG_PENDING_POINTER_POSITION;
    Ok((
        response.stats.queued != 0 || response.stats.flags & pending_flags != 0,
        response.stats.readiness_generation,
    ))
}

pub fn current_console_session_is_system() -> bool {
    multitask::current_user_snapshot()
        .map(|snapshot| snapshot.console_session().is_system())
        // A missing user task must not be elevated into the system console.
        .unwrap_or(false)
}

pub fn console_readiness_via_sessiond_with_timeout(
    session_handle: u64,
    timeout_ms: u64,
) -> Result<(bool, bool, u64), i64> {
    if session_handle == 0 {
        return Err(LINUX_EINVAL);
    }
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    if snapshot.console_session().is_system() || snapshot.console_session().raw() != session_handle
    {
        return Err(LINUX_EPERM);
    }
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_SESSIOND;
    request.header.op = COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE;
    request.header.service_id = IPC_SERVICE_SESSIOND;
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    request.arg0 = COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS;
    request.arg2 = session_handle;
    let response = ipc_ops::call_service_endpoint_with_class_deadline(
        IPC_SERVICE_SESSIOND,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::ReadinessQuery,
        timeout_ms,
    )?;
    if response.len() != size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    ipc_ops::validate_commercial_response_envelope(&request, &response)?;
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if response.descriptor_count != 1
        || response.payload_len != 0
        || response.value1 == 0
        || response.value0 & !SESSIOND_CONSOLE_READINESS_MASK != 0
    {
        return Err(LINUX_EINVAL);
    }
    Ok((
        response.value0 & SESSIOND_CONSOLE_READINESS_READY != 0,
        response.value0 & SESSIOND_CONSOLE_READINESS_LIVE != 0,
        response.value1,
    ))
}

pub fn console_read_via_sessiond(
    user_ptr: u64,
    user_len: usize,
    nonblocking: bool,
) -> Result<u64, i64> {
    if user_len == 0 {
        return Ok(0);
    }
    usermem::validate_current_user_write_buffer(user_ptr, user_len)
        .map_err(address_space_error_to_linux_errno)?;
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    let session = snapshot.console_session();
    if session.is_system() {
        return Err(LINUX_EINVAL);
    }

    let mut copied = 0usize;
    while copied < user_len {
        let chunk_len =
            (user_len - copied).min(CommercialMaxProtocolResponse::default().payload.len());
        let response = call_sessiond_console_route(
            snapshot.process_id(),
            snapshot.thread_id(),
            session.raw(),
            COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ,
            &[],
            chunk_len,
        )?;
        let read = response.payload_len as usize;
        if read > chunk_len {
            return Err(LINUX_EINVAL);
        }
        if read == 0 {
            if let Some(result) = empty_console_read_result(nonblocking, copied) {
                return result;
            }
            multitask::yield_now();
            crate::arch::rtc::sleep(1);
            continue;
        }
        let dest = user_ptr.checked_add(copied as u64).ok_or(LINUX_EINVAL)?;
        usermem::write_current_user_bytes(dest, &response.payload[..read])
            .map_err(address_space_error_to_linux_errno)?;
        copied += read;
        multitask::cond_resched();
        if read < chunk_len {
            break;
        }
    }
    Ok(copied as u64)
}

fn empty_console_read_result(nonblocking: bool, copied: usize) -> Option<Result<u64, i64>> {
    if copied != 0 {
        Some(Ok(copied as u64))
    } else if nonblocking {
        Some(Err(LINUX_EAGAIN))
    } else {
        None
    }
}

pub fn console_write_via_sessiond(user_ptr: u64, user_len: usize) -> Result<u64, i64> {
    if user_len == 0 {
        return Ok(0);
    }
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    let session = snapshot.console_session();
    if session.is_system() {
        return Err(LINUX_EINVAL);
    }

    let mut copied = 0usize;
    let mut chunk =
        alloc::vec![0_u8; user_len.min(CommercialMaxProtocolRequest::default().payload.len())];
    while copied < user_len {
        let chunk_len = (user_len - copied).min(chunk.len());
        let src = user_ptr.checked_add(copied as u64).ok_or(LINUX_EINVAL)?;
        usermem::copy_from_current_user_exact(src, &mut chunk[..chunk_len])
            .map_err(address_space_error_to_linux_errno)?;
        let response = call_sessiond_console_route(
            snapshot.process_id(),
            snapshot.thread_id(),
            session.raw(),
            COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE,
            &chunk[..chunk_len],
            0,
        )?;
        let written = usize::try_from(response.value0).map_err(|_| LINUX_EINVAL)?;
        if written > chunk_len {
            return Err(LINUX_EINVAL);
        }
        if written == 0 {
            break;
        }
        copied += written;
        multitask::cond_resched();
        if written < chunk_len {
            break;
        }
    }
    Ok(copied as u64)
}

fn call_sessiond_console_route(
    subject_pid: u64,
    subject_tid: u64,
    session_handle: u64,
    route_request: u64,
    payload: &[u8],
    read_capacity: usize,
) -> Result<CommercialMaxProtocolResponse, i64> {
    let mut request = CommercialMaxProtocolRequest::default();
    if payload.len() > request.payload.len() {
        return Err(LINUX_EINVAL);
    }
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_SESSIOND;
    request.header.op = COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE;
    request.header.service_id = IPC_SERVICE_SESSIOND;
    request.header.subject_pid = subject_pid;
    request.header.subject_tid = subject_tid;
    request.arg0 = route_request;
    request.arg2 = session_handle;
    request.arg3 = read_capacity as u64;
    request.payload[..payload.len()].copy_from_slice(payload);
    request.payload_len = payload.len() as u32;

    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_SESSIOND,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::BulkData,
    )?;
    log_slow_service_call(
        "sessiond",
        request.header.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.header.subject_pid,
        request.header.subject_tid,
        response.len() as i64,
        None,
    );
    if response.len() != size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    ipc_ops::validate_commercial_response_envelope(&request, &response)?;
    if response.descriptor_count != 1 {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    match route_request {
        COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ => {
            if response.payload_len as usize > read_capacity
                || response.value0 != u64::from(response.payload_len)
            {
                return Err(LINUX_EINVAL);
            }
        }
        COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE => {
            if response.payload_len != 0 || response.value0 as usize > payload.len() {
                return Err(LINUX_EINVAL);
            }
        }
        COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS => {
            if response.payload_len != 0
                || response.value1 == 0
                || response.value0 & !SESSIOND_CONSOLE_READINESS_MASK != 0
            {
                return Err(LINUX_EINVAL);
            }
        }
        _ => return Err(LINUX_EINVAL),
    }
    Ok(response)
}

pub fn open_device_via_devmgrd(path: &str, flags: u64) -> Result<u64, i64> {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes.len() > VFS_IPC_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let mut request = DevmgrdDeviceOpenRequest {
        version: rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION,
        op: rustos_user_abi::syscall::DEVMGRD_IPC_OP_OPEN,
        open_flags: flags,
        path_len: bytes.len() as u32,
        ..DevmgrdDeviceOpenRequest::default()
    };
    request.path[..bytes.len()].copy_from_slice(bytes);
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
    }
    let (response, entries) = ipc_ops::call_service_endpoint_with_received_entries(
        IPC_SERVICE_DEVMGRD,
        as_bytes(&request),
        1,
    )?;
    if response.len() != size_of::<DevmgrdDeviceOpenResponse>() {
        ipc_ops::drop_transfer_entries(entries);
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<DevmgrdDeviceOpenResponse>(response.as_slice());
    if response.version != rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION
        || response.op != rustos_user_abi::syscall::DEVMGRD_IPC_OP_OPEN
        || response.reserved0 != 0
    {
        ipc_ops::drop_transfer_entries(entries);
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        ipc_ops::drop_transfer_entries(entries);
        return Err(response.status.unsigned_abs() as i64);
    }
    if entries.len() != 1 {
        ipc_ops::drop_transfer_entries(entries);
        return Err(LINUX_EINVAL);
    }
    let mut entries = entries.into_iter();
    let Some(entry) = entries.next() else {
        return Err(LINUX_EINVAL);
    };
    let input_token = match entry.entry().handle() {
        multitask::KernelHandle::Device(device)
            if device.device_id() == kernel_object::api::device::DeviceId::Input =>
        {
            Some(device.token_id())
        }
        _ => None,
    };
    let Some(fd) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().install_transferred(entry)
    }) else {
        if let Some(token) = input_token {
            ipc_ops::release_input_transfer_token(token);
        }
        return Err(LINUX_EINVAL);
    };
    match fd {
        Some(fd) => Ok(fd),
        None => {
            if let Some(token) = input_token {
                ipc_ops::release_input_transfer_token(token);
            }
            Err(LINUX_EMFILE)
        }
    }
}

pub fn ioctl_tty_via_sessiond(fd: u64, request_number: u64, arg: u64) -> Result<u64, i64> {
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_SESSIOND;
    request.header.op = COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE;
    request.header.service_id = IPC_SERVICE_SESSIOND;
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    request.arg0 = request_number;
    request.arg1 = fd;
    request.arg2 = snapshot.console_session().raw();
    if matches!(
        request_number,
        linux_abi::TCSETS | linux_abi::TCSETSW | linux_abi::TCSETSF
    ) {
        let termios = usermem::read_current_user_struct::<linux_abi::LinuxTermios>(arg)
            .map_err(address_space_error_to_linux_errno)?;
        request.payload[..size_of::<linux_abi::LinuxTermios>()].copy_from_slice(as_bytes(&termios));
        request.payload_len = size_of::<linux_abi::LinuxTermios>() as u32;
    }

    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_SESSIOND,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::InteractiveControl,
    )?;
    log_slow_service_call(
        "sessiond",
        request.header.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.header.subject_pid,
        request.header.subject_tid,
        response.len() as i64,
        None,
    );
    if response.len() != size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    ipc_ops::validate_commercial_response_envelope(&request, &response)?;
    if response.descriptor_count != 1 {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }

    match request_number {
        linux_abi::TCGETS => {
            if response.payload_len as usize != size_of::<linux_abi::LinuxTermios>() {
                return Err(LINUX_EINVAL);
            }
            usermem::write_current_user_bytes(
                arg,
                &response.payload[..size_of::<linux_abi::LinuxTermios>()],
            )
            .map_err(address_space_error_to_linux_errno)?;
            Ok(0)
        }
        linux_abi::TCSETS | linux_abi::TCSETSW | linux_abi::TCSETSF => {
            if response.payload_len != 0 {
                return Err(LINUX_EINVAL);
            }
            Ok(0)
        }
        linux_abi::FIONREAD => {
            if response.payload_len != 0 {
                return Err(LINUX_EINVAL);
            }
            let pending = response.value0.min(i32::MAX as u64) as i32;
            usermem::write_current_user_bytes(arg, as_bytes(&pending))
                .map_err(address_space_error_to_linux_errno)?;
            Ok(0)
        }
        _ => Err(LINUX_ENOTTY),
    }
}

pub fn ioctl_device_via_devmgrd(fd: u64, request_number: u64, arg: u64) -> Result<u64, i64> {
    let mut request = DevmgrdDeviceIoctlRequest {
        version: rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION,
        op: rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE,
        fd,
        request: request_number,
        arg,
        ..DevmgrdDeviceIoctlRequest::default()
    };
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.session_handle = snapshot.console_session().raw();
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }
    populate_devmgrd_ioctl_payload(&mut request, arg)?;
    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_DEVMGRD,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::InteractiveControl,
    )?;
    log_slow_service_call(
        "devmgrd",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.len() as i64,
        None,
    );
    if response.len() != size_of::<DevmgrdDeviceIoctlResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<DevmgrdDeviceIoctlResponse>(response.as_slice());
    if response.version != rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION
        || response.op != rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE
        || response.payload_len as usize > response.payload.len()
        || response.reserved1 != 0
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    apply_devmgrd_ioctl_payload(request_number, arg, &response)?;
    Ok(response.value)
}

pub fn ioctl_route_via_devmgrd(fd: u64, request_number: u64) -> Result<u64, i64> {
    let mut request = DevmgrdDeviceIoctlRequest {
        version: rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION,
        op: rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_ROUTE,
        fd,
        request: request_number,
        ..DevmgrdDeviceIoctlRequest::default()
    };
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.session_handle = snapshot.console_session().raw();
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }

    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_DEVMGRD,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::InteractiveControl,
    )?;
    log_slow_service_call(
        "devmgrd",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.len() as i64,
        None,
    );
    if response.len() != size_of::<DevmgrdDeviceIoctlResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<DevmgrdDeviceIoctlResponse>(response.as_slice());
    if response.version != rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION
        || response.op != rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_ROUTE
        || response.payload_len != 0
        || response.reserved1 != 0
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    match response.value {
        rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_DIRECT
        | rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_DEVMGRD
        | rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_SESSIOND_TTY
        | rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT => Ok(response.value),
        _ => Err(LINUX_EINVAL),
    }
}

fn populate_devmgrd_ioctl_payload(
    request: &mut DevmgrdDeviceIoctlRequest,
    arg: u64,
) -> Result<(), i64> {
    match request.request {
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSIONS => {
            let snapshot = usermem::read_current_user_struct::<
                rustos_user_abi::console::ConsoleSnapshotSessionsRequest,
            >(arg)
            .map_err(address_space_error_to_linux_errno)?;
            copy_devmgrd_ioctl_request_payload(request, as_bytes(&snapshot))
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT => {
            let snapshot = usermem::read_current_user_struct::<
                rustos_user_abi::console::ConsoleSnapshotSessionOutputRequest,
            >(arg)
            .map_err(address_space_error_to_linux_errno)?;
            copy_devmgrd_ioctl_request_payload(request, as_bytes(&snapshot))
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SEND_INPUT_EVENT => {
            let input = usermem::read_current_user_struct::<
                rustos_user_abi::console::ConsoleSendInputEventRequest,
            >(arg)
            .map_err(address_space_error_to_linux_errno)?;
            copy_devmgrd_ioctl_request_payload(request, as_bytes(&input))
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SET_FOCUS => {
            let focus = usermem::read_current_user_struct::<
                rustos_user_abi::console::ConsoleSetFocusRequest,
            >(arg)
            .map_err(address_space_error_to_linux_errno)?;
            copy_devmgrd_ioctl_request_payload(request, as_bytes(&focus))
        }
        _ => Ok(()),
    }
}

fn copy_devmgrd_ioctl_request_payload(
    request: &mut DevmgrdDeviceIoctlRequest,
    bytes: &[u8],
) -> Result<(), i64> {
    if bytes.len() > request.payload.len() {
        return Err(LINUX_EINVAL);
    }
    request.payload[..bytes.len()].copy_from_slice(bytes);
    request.payload_len = bytes.len() as u32;
    Ok(())
}

fn apply_devmgrd_ioctl_payload(
    request_number: u64,
    arg: u64,
    response: &DevmgrdDeviceIoctlResponse,
) -> Result<(), i64> {
    let payload_len = response.payload_len as usize;
    if payload_len == 0 {
        return Ok(());
    }
    match request_number {
        rustos_user_abi::console::CONSOLE_IOCTL_GET_STATE => {
            if payload_len != size_of::<rustos_user_abi::console::ConsoleStateInfo>() {
                return Err(LINUX_EINVAL);
            }
            let info =
                read_unaligned::<rustos_user_abi::console::ConsoleStateInfo>(&response.payload);
            usermem::write_current_user_struct(arg, &info)
                .map_err(address_space_error_to_linux_errno)
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSIONS => {
            let header_len = size_of::<rustos_user_abi::console::ConsoleSnapshotSessionsRequest>();
            if payload_len < header_len {
                return Err(LINUX_EINVAL);
            }
            let snapshot = read_unaligned::<rustos_user_abi::console::ConsoleSnapshotSessionsRequest>(
                &response.payload,
            );
            if snapshot.count > snapshot.capacity
                || snapshot.count > rustos_user_abi::console::MAX_CONSOLE_SESSIONS as u64
            {
                return Err(LINUX_EINVAL);
            }
            let count = usize::try_from(snapshot.count).map_err(|_| LINUX_EINVAL)?;
            let sessions_len = count
                .checked_mul(size_of::<rustos_user_abi::console::ConsoleSessionInfo>())
                .ok_or(LINUX_EINVAL)?;
            if payload_len != header_len + sessions_len {
                return Err(LINUX_EINVAL);
            }
            usermem::write_current_user_struct(arg, &snapshot)
                .map_err(address_space_error_to_linux_errno)?;
            if sessions_len == 0 {
                return Ok(());
            }
            usermem::write_current_user_bytes(
                snapshot.sessions_ptr,
                &response.payload[header_len..payload_len],
            )
            .map_err(address_space_error_to_linux_errno)
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT => {
            let header_len =
                size_of::<rustos_user_abi::console::ConsoleSnapshotSessionOutputRequest>();
            if payload_len < header_len {
                return Err(LINUX_EINVAL);
            }
            let snapshot = read_unaligned::<
                rustos_user_abi::console::ConsoleSnapshotSessionOutputRequest,
            >(&response.payload);
            if snapshot.count > snapshot.capacity {
                return Err(LINUX_EINVAL);
            }
            let bytes_len = usize::try_from(snapshot.count).map_err(|_| LINUX_EINVAL)?;
            if payload_len != header_len + bytes_len {
                return Err(LINUX_EINVAL);
            }
            usermem::write_current_user_struct(arg, &snapshot)
                .map_err(address_space_error_to_linux_errno)?;
            if bytes_len == 0 {
                return Ok(());
            }
            usermem::write_current_user_bytes(
                snapshot.bytes_ptr,
                &response.payload[header_len..payload_len],
            )
            .map_err(address_space_error_to_linux_errno)
        }
        _ => Err(LINUX_EINVAL),
    }
}

pub fn call_inputd_read_request(request: &InputdIpcRequest) -> Result<InputdReadResponse, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint_with_class(
        IPC_SERVICE_INPUTD,
        as_bytes(request),
        ipc_ops::ServiceIpcClass::BulkData,
    )?;
    let detail = if request.flags & INPUTD_READ_FLAG_NONBLOCK != 0 {
        Some("nonblock")
    } else {
        Some("blocking")
    };
    log_slow_service_call(
        "inputd",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.len() as i64,
        detail,
    );
    if response.len() != size_of::<InputdReadResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<InputdReadResponse>(response.as_slice());
    if response.version != INPUTD_IPC_ABI_VERSION
        || response.op != request.op
        || response.flags != 0
        || response.reserved0 != 0
        || response.payload_len as usize > INPUTD_READ_PAYLOAD_CAPACITY
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(response)
}

pub fn call_netd_ipc_request(request: &NetdIpcRequest) -> Result<NetdIpcResponse, i64> {
    call_netd_ipc_request_impl(request, None)
}

pub fn call_netd_ipc_request_with_timeout(
    request: &NetdIpcRequest,
    timeout_ms: u64,
) -> Result<NetdIpcResponse, i64> {
    call_netd_ipc_request_impl(request, Some(timeout_ms.max(1)))
}

fn call_netd_ipc_request_impl(
    request: &NetdIpcRequest,
    timeout_ms: Option<u64>,
) -> Result<NetdIpcResponse, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let request_len = NETD_IPC_REQUEST_HEADER_SIZE
        .checked_add(request.payload_len as usize)
        .filter(|len| *len <= size_of::<NetdIpcRequest>())
        .ok_or(LINUX_EINVAL)?;
    let request_bytes = &as_bytes(request)[..request_len];
    let response = match timeout_ms {
        Some(timeout_ms) => ipc_ops::call_service_endpoint_with_class_deadline(
            IPC_SERVICE_NETD,
            request_bytes,
            ipc_ops::ServiceIpcClass::ReadinessQuery,
            timeout_ms,
        )?,
        None => ipc_ops::call_service_endpoint_with_class(
            IPC_SERVICE_NETD,
            request_bytes,
            ipc_ops::ServiceIpcClass::BulkData,
        )?,
    };
    let elapsed_ms = ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks());
    log_slow_service_call(
        "netd",
        request.op,
        elapsed_ms,
        request.pid,
        request.tid,
        response.len() as i64,
        None,
    );
    if response.len() < NETD_IPC_RESPONSE_HEADER_SIZE
        || response.len() > size_of::<NetdIpcResponse>()
    {
        return Err(LINUX_EINVAL);
    }
    let mut decoded = NetdIpcResponse::default();
    unsafe {
        core::ptr::copy_nonoverlapping(
            response.as_ptr(),
            (&mut decoded as *mut NetdIpcResponse).cast::<u8>(),
            response.len(),
        );
    }
    let expected_len = NETD_IPC_RESPONSE_HEADER_SIZE
        .checked_add(decoded.payload_len as usize)
        .ok_or(LINUX_EINVAL)?;
    if response.len() != expected_len
        || decoded.payload_len as usize > NETD_IPC_PAYLOAD_CAPACITY
        || decoded.version != NETD_IPC_ABI_VERSION
        || decoded.op != request.op
        || decoded.reserved0 != 0
        || decoded.reserved1 & !NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF != 0
    {
        return Err(LINUX_EINVAL);
    }
    if decoded.status != 0 {
        return Err(decoded.status.unsigned_abs() as i64);
    }
    // The scheduling hint is consumed at the kernel IPC boundary and is not
    // part of the Linux socket result exposed to compatibility callers.
    decoded.reserved1 = 0;
    Ok(decoded)
}

pub fn ensure_vfs_status(response: &VfsIpcResponse) -> Result<(), i64> {
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(())
}

fn ticks_elapsed_ms(start_ticks: u64, end_ticks: u64) -> u64 {
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    end_ticks
        .saturating_sub(start_ticks)
        .saturating_mul(1000)
        .saturating_div(ticks_per_second)
}

fn log_slow_service_call(
    service: &str,
    op: u16,
    elapsed_ms: u64,
    pid: u64,
    tid: u64,
    status_or_len: i64,
    detail: Option<&str>,
) {
    let sample_index = SERVICE_CALL_SAMPLE_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= EARLY_SERVICE_CALL_SAMPLES && elapsed_ms < SLOW_SERVICE_CALL_THRESHOLD_MS {
        return;
    }
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let window = crate::arch::rtc::ticks() / ticks_per_second;
    if !super::super::ipc_ops::diagnostic_rate_limit_permit(
        &SLOW_SERVICE_CALL_LOG_RATE_STATE,
        window,
        MAX_SLOW_SERVICE_CALL_LOGS_PER_SECOND as u8,
    ) {
        return;
    }
    if let Some(detail) = detail {
        debug::println!(
            "service ipc slow: service={} op={} elapsed_ms={} pid={} tid={} status_or_len={} detail={}",
            service,
            op,
            elapsed_ms,
            pid,
            tid,
            status_or_len,
            detail,
        );
    } else {
        debug::println!(
            "service ipc slow: service={} op={} elapsed_ms={} pid={} tid={} status_or_len={}",
            service,
            op,
            elapsed_ms,
            pid,
            tid,
            status_or_len,
        );
    }
}

#[allow(
    clippy::items_after_test_module,
    reason = "focused response-envelope tests remain adjacent to the parser while public path helpers close the module"
)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_nonblocking_console_read_returns_eagain_without_retry() {
        assert_eq!(empty_console_read_result(true, 0), Some(Err(LINUX_EAGAIN)));
        assert_eq!(empty_console_read_result(false, 0), None);
        assert_eq!(empty_console_read_result(true, 7), Some(Ok(7)));
    }

    #[test]
    fn vfs_response_envelope_rejects_oversized_payload_before_slice_use() {
        let mut response = VfsIpcResponse {
            version: VFS_IPC_ABI_VERSION,
            op: VFS_IPC_OP_OPENAT,
            ..VfsIpcResponse::default()
        };
        assert_eq!(
            validate_vfs_response_envelope(VFS_IPC_OP_OPENAT, &response),
            Ok(())
        );

        response.payload_len = response.payload.len() as u32 + 1;
        assert_eq!(
            validate_vfs_response_envelope(VFS_IPC_OP_OPENAT, &response),
            Err(LINUX_EINVAL)
        );
    }

    #[test]
    fn only_tombstoning_vfs_mutations_require_visibility_ack() {
        let close = VfsIpcRequest {
            op: VFS_IPC_OP_CLOSE,
            ..VfsIpcRequest::default()
        };
        assert!(vfs_checkpoint_ack_required(&close));

        let mut poll = VfsIpcRequest {
            op: VFS_IPC_OP_POLL_QUERY,
            arg0: VFS_POLL_QUERY_EPOLL_CTL,
            arg1: linux_abi::EPOLL_CTL_ADD,
            ..VfsIpcRequest::default()
        };
        assert!(!vfs_checkpoint_ack_required(&poll));
        poll.arg1 = linux_abi::EPOLL_CTL_DEL;
        assert!(vfs_checkpoint_ack_required(&poll));
        poll.arg0 = VFS_POLL_QUERY_EPOLL_RETIRE;
        assert!(vfs_checkpoint_ack_required(&poll));
        poll.arg0 = VFS_POLL_QUERY_EPOLL_PURGE_OBJECT;
        assert!(vfs_checkpoint_ack_required(&poll));

        let read = VfsIpcRequest {
            op: VFS_IPC_OP_READ,
            ..VfsIpcRequest::default()
        };
        assert!(!vfs_checkpoint_ack_required(&read));
    }

    #[test]
    fn replay_retries_share_one_total_timeout_budget() {
        let slices = (0..3)
            .map(|attempt| split_retry_timeout_ms(16, 3, attempt))
            .collect::<Vec<_>>();
        assert_eq!(slices.as_slice(), &[6, 5, 5]);
        assert_eq!(slices.iter().sum::<u64>(), 16);
        assert_eq!(split_retry_timeout_ms(1, 1, 0), 1);
    }

    #[test]
    fn epoll_snapshot_reads_are_retry_safe() {
        let mut request = VfsIpcRequest {
            op: VFS_IPC_OP_POLL_QUERY,
            ..VfsIpcRequest::default()
        };
        request.arg0 = VFS_POLL_QUERY_EPOLL_SNAPSHOT;
        assert!(vfs_request_is_replay_safe(&request));

        request.arg0 = u64::MAX;
        assert!(!vfs_request_is_replay_safe(&request));
    }

    #[test]
    fn housekeeping_vfs_maintenance_is_one_bounded_replay_turn() {
        assert_eq!(HOUSEKEEPING_VFS_MAINTENANCE_ATTEMPTS, 1);
        assert_eq!(
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
            100
        );
    }

    #[test]
    fn procd_response_envelope_rejects_cross_op_and_oversized_payload() {
        let mut response = ProcdIpcResponse {
            op: PROCD_OP_SELECT_SIGNAL,
            ..ProcdIpcResponse::default()
        };
        assert_eq!(
            validate_procd_response_envelope(PROCD_OP_SELECT_SIGNAL, &response),
            Ok(())
        );
        assert_eq!(
            validate_procd_response_envelope(PROCD_OP_EXECVE, &response),
            Err(LINUX_EINVAL)
        );

        response.payload_len = response.payload.len() as u32 + 1;
        assert_eq!(
            validate_procd_response_envelope(PROCD_OP_SELECT_SIGNAL, &response),
            Err(LINUX_EINVAL)
        );
    }
}

fn vfs_request_log_detail(request: &VfsIpcRequest) -> Option<String> {
    if request.path_len != 0 {
        let path_len = usize::try_from(request.path_len).ok()?;
        if path_len > request.path.len() {
            return None;
        }
        let path = core::str::from_utf8(&request.path[..path_len]).ok()?;
        return Some(alloc::format!("path={}", path));
    }
    if let Some(remote) = current_remote_vfs_handle(request.fd) {
        return Some(alloc::format!("fd={} path={}", request.fd, remote.path()));
    }
    None
}

pub fn current_remote_vfs_handle(fd: u64) -> Option<multitask::RemoteVfsHandle> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::RemoteVfs(remote)) => Some(remote.clone()),
            _ => None,
        }
    })
    .flatten()
}

pub fn copy_current_user_path(ptr: u64, capacity: usize) -> Result<String, i64> {
    if ptr == 0 {
        return Err(LINUX_EFAULT);
    }
    let path = usermem::read_current_user_c_string(ptr, capacity)
        .map_err(address_space_error_to_linux_errno)?;
    if path.is_empty() || path.len() > capacity {
        return Err(LINUX_EINVAL);
    }
    Ok(path)
}
