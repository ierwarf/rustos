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
use nucleus_core::util::{
    lockdep::{LockClass, TrackedSpinLock},
    ring::RingBuffer,
};

#[path = "ipc_helpers/bootstrap_spawn.rs"]
mod bootstrap_spawn;
#[path = "ipc_helpers_deadline.rs"]
pub(super) mod deadline;
#[path = "ipc_helpers_diagnostics.rs"]
mod diagnostics;
#[path = "ipc_helpers/netd_socket_reply.rs"]
mod netd_socket_reply;
pub use bootstrap_spawn::syscall_linux_loader_spawn_exec;
use netd_socket_reply::{
    call_netd_socket_ipc_request_impl, netd_reply_uses_prepared_socket,
};
#[cfg(test)]
use netd_socket_reply::{
    install_prepared_netd_socket_response, prepared_netd_socket_response_matches,
};

// A readiness probe cannot consume an event: it only transfers authenticated
// ingress into inputd's policy queue. Bound that safe, idempotent operation so
// a wedged inputd cannot freeze uiserver's input loop for the generic service
// IPC deadline. Stateful authorize/read calls retain their completion wait
// until the ABI carries a cancellable authorization lease.
const PENDING_VFS_MUTATION_CAPACITY: usize = 32 * 1024;
const PENDING_VFS_MUTATION_STORAGE_CAPACITY: usize = PENDING_VFS_MUTATION_CAPACITY + 1;
const PENDING_VFS_PAYLOAD_CAPACITY: usize = 64;
const HOUSEKEEPING_VFS_MAINTENANCE_ATTEMPTS: usize = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VfsMutationLane {
    Description(u64),
    Epoll(u64),
    ProviderObject(u64, u64),
}

impl VfsMutationLane {
    fn for_request(request: &VfsIpcRequest) -> Result<Self, i64> {
        if request.op == VFS_IPC_OP_POLL_QUERY && request.arg0 == VFS_POLL_QUERY_EPOLL_PURGE_OBJECT {
            return Ok(if request.arg1 == u64::from(WAITSET_PROVIDER_VFSD) {
                Self::Description(request.arg2)
            } else { Self::ProviderObject(request.arg1, request.arg2) });
        }
        if request.op == VFS_IPC_OP_CHECKPOINT_ACK && request.arg2 == u64::from(VFS_IPC_OP_POLL_QUERY) {
            // A purge ACK carries the original mutation ID, not its target.
            // Its queue owner must inherit the original typed lane explicitly.
            if request.arg3 == VFS_POLL_QUERY_EPOLL_PURGE_OBJECT { return Err(LINUX_EINVAL); }
            return Ok(Self::Epoll(request.remote_id));
        }
        Ok(if request.op == VFS_IPC_OP_POLL_QUERY { Self::Epoll(request.remote_id) }
            else { Self::Description(request.remote_id) })
    }
}

#[derive(Clone, Copy)]
struct PendingVfsMutation {
    lane: VfsMutationLane,
    lane_head: bool,
    op: u16,
    fd: u64,
    remote_id: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    flags: u32,
    dirfd: u64,
    operation_hi: u64,
    operation_lo: u64,
    payload_len: u32,
    payload: [u8; PENDING_VFS_PAYLOAD_CAPACITY],
}

const VFS_CURSOR_GATE_SLOTS: usize = 64;
const VFS_CURSOR_GATE_WAIT_MS: u64 = 5_000;

enum VfsCursorClaim {
    Claimed,
    Busy,
}

struct PendingVfsMutationQueue<const N: usize> {
    entries: RingBuffer<PendingVfsMutation, N>,
    in_flight: Option<PendingVfsMutation>,
    /// Descriptions whose foreground cursor transaction (prepare, copy, settle)
    /// is open. vfsd refuses a second cursor operation on a prepared
    /// description with EBUSY, so the compat layer serialises them here.
    cursor_owners: [Option<u64>; VFS_CURSOR_GATE_SLOTS],
}

impl<const N: usize> PendingVfsMutationQueue<N> {
    const fn new() -> Self {
        Self { entries: RingBuffer::new(), in_flight: None, cursor_owners: [None; VFS_CURSOR_GATE_SLOTS] }
    }

    /// A description is claimable only when no foreground transaction owns it
    /// and no deferred mutation (an unsettled COMMIT/CANCEL, a retained close)
    /// is queued or being replayed for it: vfsd would still see it as prepared.
    fn try_claim_cursor(&mut self, remote_id: u64) -> VfsCursorClaim {
        let lane = VfsMutationLane::Description(remote_id);
        if self.cursor_owners.contains(&Some(remote_id)) || self.any(|entry| entry.lane == lane) {
            return VfsCursorClaim::Busy;
        }
        match self.cursor_owners.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => { *slot = Some(remote_id); VfsCursorClaim::Claimed }
            None => VfsCursorClaim::Busy,
        }
    }

    fn release_cursor(&mut self, remote_id: u64) {
        let slot = self.cursor_owners.iter_mut().find(|slot| **slot == Some(remote_id));
        debug_assert!(slot.is_some(), "cursor gate released without an owner");
        if let Some(slot) = slot { *slot = None; }
    }

    fn len(&self) -> usize { self.entries.len() }

    fn any(&self, mut predicate: impl FnMut(&PendingVfsMutation) -> bool) -> bool {
        self.in_flight.as_ref().is_some_and(&mut predicate) || self.entries.any(predicate)
    }

    fn push(&mut self, mut pending: PendingVfsMutation) -> bool {
        pending.lane_head = !self.any(|entry| entry.lane == pending.lane);
        self.entries.push(pending)
    }

    /// Claims the single replay owner for the first lane head (optionally of
    /// one lane). Returns `None` while another replayer holds the owner.
    fn pop(&mut self, lane: Option<VfsMutationLane>) -> Option<PendingVfsMutation> {
        if self.in_flight.is_some() { return None; }
        let pending = self.entries.remove_first(|entry| entry.lane_head && lane.is_none_or(|lane| entry.lane == lane));
        assert!(pending.is_some() || lane.is_some() || self.entries.is_empty(), "nonempty VFS queue needs a lane head");
        self.in_flight = pending;
        pending
    }

    fn finish(&mut self, pending: PendingVfsMutation, retry: Option<PendingVfsMutation>) {
        let held = self.in_flight.take().expect("VFS replay must retain its in-flight owner");
        assert!((held.op, held.lane, held.operation_hi, held.operation_lo) ==
            (pending.op, pending.lane, pending.operation_hi, pending.operation_lo));
        let next = retry.or_else(|| self.entries.remove_first(|entry| entry.lane == pending.lane));
        if let Some(mut next) = next {
            assert_eq!(next.lane, pending.lane, "replacement must stay in its typed lane");
            next.lane_head = true;
            // Only lane heads rotate. Followers retain order and cannot
            // overtake a settlement whose response alone may have been lost.
            assert!(self.entries.push(next), "popped vfs mutation must have retry capacity");
        }
    }
}

static PENDING_VFS_MUTATIONS: TrackedSpinLock<
    PendingVfsMutationQueue<PENDING_VFS_MUTATION_STORAGE_CAPACITY>,
    { LockClass::VfsDeferredMutation as u8 },
> = TrackedSpinLock::new(PendingVfsMutationQueue::new());

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
    let mut request = new_procd_request(op);
    request.dirfd = (linux_abi::AT_FDCWD as i64) as u64;
    request.flags = flags as u32;
    if raw_exec_path.len() > request.path.len() {
        return linux_errno(LINUX_EINVAL);
    }
    request.path_len = raw_exec_path.len() as u32;
    request.path[..raw_exec_path.len()].copy_from_slice(raw_exec_path.as_bytes());
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
        Ok(response) => fork_refused(1, response.status.unsigned_abs() as i64),
        Err(errno) => fork_refused(2, errno),
    }
}

/// A fork that procd refused (`1`: its policy reply carried the errno) or that
/// never got an answer from procd (`2`: the call itself failed or timed out).
/// Without this a bare `EAGAIN` from `fork` cannot be told apart from procd
/// being unresponsive; the count is bounded so a fork storm stays cheap.
fn fork_refused(source: u64, errno: i64) -> u64 {
    static RECORDED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    // ORDERING: Relaxed; a counter bounding telemetry volume.
    if RECORDED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 32 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Compat,
            "linux-fork-procd-refused",
            source,
            errno.unsigned_abs(),
        );
    }
    linux_errno(errno)
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
    diagnostics::log_slow_service_call(
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
    // SELECT_SIGNAL carries the selected SIGCHLD event bits in `reserved0`; the
    // signal path revalidates them against the thread's pending snapshot. Every
    // other reply must leave the field zero. Rejecting it here left a reaped
    // child's SIGCHLD pending forever, and each later syscall of that process
    // re-issued a failing procd call.
    let reserved0_admitted =
        response.reserved0 == 0 || request_op == rustos_user_abi::syscall::PROCD_OP_SELECT_SIGNAL;
    if response.version != rustos_user_abi::syscall::PROCD_IPC_ABI_VERSION
        || response.op != request_op
        || !reserved0_admitted
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

fn vfs_request_service_class(request: &VfsIpcRequest) -> ipc_ops::ServiceIpcClass {
    if matches!(request.op, VFS_IPC_OP_CLOSE | VFS_IPC_OP_CURSOR_SETTLE | VFS_IPC_OP_CHECKPOINT_ACK)
        || request.op == VFS_IPC_OP_POLL_QUERY && matches!(request.arg0,
            VFS_POLL_QUERY_EPOLL_CREATE | VFS_POLL_QUERY_EPOLL_CTL |
            VFS_POLL_QUERY_EPOLL_RETIRE | VFS_POLL_QUERY_EPOLL_PURGE_OBJECT)
    {
        ipc_ops::ServiceIpcClass::StateMutation
    } else {
        ipc_ops::ServiceIpcClass::BulkData
    }
}

fn vfs_response_needs_reconciliation(request: &VfsIpcRequest, response: &VfsIpcResponse) -> bool {
    // The local final fd reference is already gone. A prepared cursor can
    // reject provider close with EBUSY; that rejection must retain the exact
    // close owner until the preceding settlement permits retirement.
    request.op == VFS_IPC_OP_CLOSE && response.status.unsigned_abs() == LINUX_EBUSY as u32
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
    let service_class = vfs_request_service_class(request);
    let control_timeout_ms = matches!(service_class, ipc_ops::ServiceIpcClass::StateMutation)
        .then_some(rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS);
    let total_timeout_ms = timeout_ms.or(control_timeout_ms).or(replay_safe_request.then_some(30_000));
    let attempts = if replay_safe_request { 3 } else { 1 };
    let mut response = None;
    let mut last_errno = LINUX_ETIMEDOUT;
    for attempt in 0..attempts {
        // A dequeued request keeps executing after its caller's reply
        // capability expires. Give it the complete remaining rail: dividing a
        // 16/100 ms contract into short attempts only manufactures late
        // replies and backlog. Retry solely when the transport breaks early;
        // a real timeout transfers mutation ownership to housekeeping below.
        let attempt_timeout_ms = match total_timeout_ms {
            Some(total) => {
                let elapsed = ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks());
                let Some(remaining) = deadline::remaining_service_timeout_ms(total, elapsed) else {
                    break;
                };
                Some(remaining)
            }
            None => None,
        };
        let result = match attempt_timeout_ms {
            Some(timeout_ms) => ipc_ops::call_service_endpoint_with_class_deadline(
                IPC_SERVICE_VFSD,
                as_bytes(request),
                service_class,
                timeout_ms,
            ),
            None => ipc_ops::call_service_endpoint_with_class(
                IPC_SERVICE_VFSD,
                as_bytes(request),
                service_class,
            ),
        };
        match result {
            Ok(bytes) => {
                response = Some(bytes);
                break;
            }
            Err(errno)
                if attempt + 1 < attempts
                    && deadline::retryable_early_service_transport_error(errno) =>
            {
                last_errno = errno;
                multitask::cond_resched();
            }
            Err(errno) => {
                last_errno = errno;
                break;
            }
        }
    }
    let detail = deadline::vfs_request_log_detail(request);
    let response = match response {
        Some(response) => response,
        None => {
            diagnostics::log_failed_service_call(
                "vfsd",
                request.op,
                ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
                request.pid,
                request.tid,
                last_errno,
                detail.as_deref(),
            );
            if deferred_reconcile {
                enqueue_pending_vfs_mutation(request)?;
            }
            return Err(last_errno);
        }
    };
    diagnostics::log_slow_service_call(
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
    if vfs_response_needs_reconciliation(request, &response) {
        enqueue_pending_vfs_mutation(request)?;
    }
    if response.status.unsigned_abs() == LINUX_EBUSY as u32 {
        diagnostics::log_failed_service_call("vfsd", request.op,
            ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()), request.pid,
            request.tid, LINUX_EBUSY, detail.as_deref());
    }
    if response.status == 0 && vfs_checkpoint_ack_required(request) {
        let _ = acknowledge_vfs_checkpoint_mutation(request);
    }
    Ok(response)
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
    // A visibility ACK mutates checkpoint custody. Give it one complete
    // state-mutation lease, never an interactive or readiness-query rail.
    let last_errno = match ipc_ops::call_service_endpoint_with_class_deadline(
        IPC_SERVICE_VFSD,
        as_bytes(&ack),
        vfs_request_service_class(&ack),
        rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
    ) {
        Ok(bytes) if bytes.len() == size_of::<VfsIpcResponse>() => {
            let response = read_unaligned::<VfsIpcResponse>(&bytes);
            if validate_vfs_response_envelope(ack.op, &response).is_ok() && response.status == 0 {
                return Ok(());
            }
            if response.status == 0 {
                LINUX_EINVAL
            } else {
                i64::from(response.status.unsigned_abs())
            }
        }
        Ok(_) => LINUX_EINVAL,
        Err(errno) => errno,
    };
    enqueue_captured_vfs_mutation(PendingVfsMutation::capture_ack(original, &ack)?)?;
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
    let response = call_vfs_ipc_request_with_timeout(&request,
        rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS)?;
    ensure_vfs_status(&response)
}

impl PendingVfsMutation {
    fn capture(request: &VfsIpcRequest) -> Result<Self, i64> {
        Self::capture_in_lane(request, VfsMutationLane::for_request(request)?)
    }

    fn capture_ack(original: &VfsIpcRequest, ack: &VfsIpcRequest) -> Result<Self, i64> {
        if !vfs_checkpoint_ack_required(original) || ack.op != VFS_IPC_OP_CHECKPOINT_ACK
            || (ack.arg0, ack.arg1) != (original.operation_hi, original.operation_lo)
            || ack.remote_id != original.remote_id || ack.arg2 != u64::from(original.op)
            || ack.arg3 != if original.op == VFS_IPC_OP_POLL_QUERY { original.arg0 } else { 0 }
        { return Err(LINUX_EINVAL); }
        Self::capture_in_lane(ack, VfsMutationLane::for_request(original)?)
    }

    fn capture_in_lane(request: &VfsIpcRequest, lane: VfsMutationLane) -> Result<Self, i64> {
        let payload_len = request.payload_len as usize;
        if payload_len > PENDING_VFS_PAYLOAD_CAPACITY || request.path_len != 0 {
            return Err(LINUX_EINVAL);
        }
        let mut payload = [0_u8; PENDING_VFS_PAYLOAD_CAPACITY];
        payload[..payload_len].copy_from_slice(&request.payload[..payload_len]);
        Ok(Self {
            lane, lane_head: true,
            op: request.op, fd: request.fd, remote_id: request.remote_id,
            arg0: request.arg0, arg1: request.arg1, arg2: request.arg2, arg3: request.arg3,
            flags: request.flags, dirfd: request.dirfd,
            operation_hi: request.operation_hi, operation_lo: request.operation_lo,
            payload_len: request.payload_len, payload,
        })
    }

    fn restore_into(&self, request: &mut VfsIpcRequest) {
        request.op = self.op;
        request.fd = self.fd;
        request.remote_id = self.remote_id;
        request.arg0 = self.arg0;
        request.arg1 = self.arg1;
        request.arg2 = self.arg2;
        request.arg3 = self.arg3;
        request.flags = self.flags;
        request.dirfd = self.dirfd;
        request.operation_hi = self.operation_hi;
        request.operation_lo = self.operation_lo;
        request.payload_len = self.payload_len;
        request.payload[..self.payload_len as usize].copy_from_slice(&self.payload[..self.payload_len as usize]);
    }
}

fn retry_vfs_mutation_later<const N: usize>(queue: &mut PendingVfsMutationQueue<N>, pending: PendingVfsMutation) {
    // Retain this lane's first owner, allowing other lanes to progress.
    queue.finish(pending, Some(pending));
}

fn enqueue_pending_vfs_mutation(request: &VfsIpcRequest) -> Result<(), i64> {
    enqueue_captured_vfs_mutation(PendingVfsMutation::capture(request)?)
}

fn enqueue_captured_vfs_mutation(pending: PendingVfsMutation) -> Result<(), i64> {
    let request = &pending;
    let mut queue = PENDING_VFS_MUTATIONS.lock();
    if queue.any(|entry| {
        if request.op == VFS_IPC_OP_CHECKPOINT_ACK {
            entry.op == VFS_IPC_OP_CHECKPOINT_ACK
                && entry.lane == request.lane
                && entry.arg0 == request.arg0
                && entry.arg1 == request.arg1
                && entry.arg2 == request.arg2
                && entry.arg3 == request.arg3
        } else {
            entry.operation_hi == request.operation_hi && entry.operation_lo == request.operation_lo
        }
    }) {
        return Ok(());
    }
    if queue.len() >= PENDING_VFS_MUTATION_CAPACITY {
        return Err(LINUX_ENOSPC);
    }
    let admitted = queue.push(pending);
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
        match replay_pending_vfs_mutation(None) {
            Some(true) => attempted += 1,
            Some(false) => return attempted + 1,
            None => return attempted,
        }
    }
    attempted
}

/// Replays one lane head. `None`: nothing to replay (empty, or another
/// replayer owns the queue); `Some(true)`: settled; `Some(false)`: the lane
/// head failed and was parked for a later attempt.
fn replay_pending_vfs_mutation(lane: Option<VfsMutationLane>) -> Option<bool> {
    let pending = PENDING_VFS_MUTATIONS.lock().pop(lane)?;
    let mut request = new_vfs_request(pending.op);
    pending.restore_into(&mut request);
    let response = ipc_ops::call_service_endpoint_with_class_deadline(
        IPC_SERVICE_VFSD,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::StateMutation,
        rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
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
        retry_vfs_mutation_later(&mut *PENDING_VFS_MUTATIONS.lock(), pending);
        return Some(false);
    }
    let next = if vfs_checkpoint_ack_required(&request) {
        // Replace the confirmed mutation by its exact ACK in the same
        // lane, using the reserved retry slot rather than fresh admission.
        let ack = vfs_checkpoint_ack_request(&request);
        Some(PendingVfsMutation::capture_ack(&request, &ack).expect("exact empty visibility ACK fits"))
    } else { None };
    PENDING_VFS_MUTATIONS.lock().finish(pending, next);
    Some(true)
}

/// Foreground ownership of one open file description's cursor, held from the
/// prepared vfsd request through the user copy and its COMMIT/CANCEL.
pub(super) struct VfsCursorGate(u64);

impl Drop for VfsCursorGate {
    fn drop(&mut self) { PENDING_VFS_MUTATIONS.lock().release_cursor(self.0); }
}

/// Orders a cursor-mutating syscall after every earlier transaction on the
/// description: other foreground users and any unsettled deferred mutation.
/// Reconciliation is driven here rather than left to housekeeping latency,
/// because vfsd would answer the next cursor operation with EBUSY.
pub(super) fn acquire_vfs_cursor_gate(remote_id: u64) -> Result<VfsCursorGate, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    loop {
        if matches!(PENDING_VFS_MUTATIONS.lock().try_claim_cursor(remote_id), VfsCursorClaim::Claimed) {
            return Ok(VfsCursorGate(remote_id));
        }
        if ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()) >= VFS_CURSOR_GATE_WAIT_MS {
            return Err(LINUX_ETIMEDOUT);
        }
        if replay_pending_vfs_mutation(Some(VfsMutationLane::Description(remote_id))).is_none() {
            multitask::cond_resched();
        }
    }
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
    // A timeout here is NOT reported as `EAGAIN`, unlike the socket halves.
    // The difference is measured, not stylistic: mapping it made the reader
    // treat "inputd did not answer" as "nothing to read", so it returned
    // immediately to an epoll that reports ready from inputd's published state
    // and issued the next pair of requests. Ten abandoned request pairs per
    // second is exactly the load a service that already missed its deadline
    // cannot recover from, and the run that carried the mapping never reached
    // WayClick's first frame at all. Without it the same build held 54
    // consecutive one-second windows at 410-428 Hz. `ETIMEDOUT` reaching the
    // reader is what keeps the retry rate tied to the service's actual pace.
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

/// Wait for the console graph to move past `known_generation`, for at most
/// `wait_ms`, and return the generation the broker reports.
///
/// # Why this does not go through devmgrd
/// Every other console ioctl is authorized by devmgrd and forwarded, which is
/// right for operations that mutate a device. This one only waits, and devmgrd
/// serves its clients from a single loop: a held reply there would stall every
/// unrelated device ioctl for the whole park. Console read, write, and
/// per-session readiness already route straight to the console broker for the
/// same reason, and this is that same rail.
///
/// # What authorizes it instead
/// The console-manager view is not session-bound, so there is no session to
/// check ownership of. The authority is the console handle itself: the caller
/// must already hold an open console description, which only a component the
/// device policy admitted to the console can obtain.
pub fn console_graph_readiness_via_sessiond(
    known_generation: u64,
    wait_ms: u64,
) -> Result<u64, i64> {
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_SESSIOND;
    request.header.op = COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE;
    request.header.service_id = IPC_SERVICE_SESSIOND;
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    request.arg0 = COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_GRAPH_READINESS;
    request.arg1 = wait_ms.min(SESSIOND_CONSOLE_GRAPH_WAIT_MAX_MS);
    request.arg2 = known_generation;
    let response = ipc_ops::call_service_endpoint_with_class_deadline(
        IPC_SERVICE_SESSIOND,
        as_bytes(&request),
        ipc_ops::ServiceIpcClass::InteractiveControl,
        rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS,
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
    Ok(response.value1)
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
    // A blocking console read is the shell's own wait for a keystroke. It used
    // to be a poll loop - ask, get "nothing yet", sleep a tick, ask again -
    // because runtimed answered `CONSOLE_ROUTE_READ` as an immediate snapshot
    // with no way to hold a reply until data arrived. Measured at 8 vCPU that
    // cost about 14.7 ms per empty pass, roughly 10.9 ms of it the round trip
    // itself, for a keystroke that had not happened yet.
    //
    // The read now carries a wait budget, so the broker holds it and answers
    // when a byte appears. The loop below still exists, but a well-behaved
    // broker drives it once per keystroke instead of once per tick.
    let mut empty_polls = 0u64;
    let mut route_ns = 0u64;
    let mut sleep_ns = 0u64;
    let waited_from_ns = crate::arch::rtc::monotonic_nanos();
    while copied < user_len {
        let chunk_len =
            (user_len - copied).min(CommercialMaxProtocolResponse::default().payload.len());
        // Only a reader that must wait, and has nothing to return yet, asks the
        // broker to hold the read. `empty_console_read_result` already answers
        // every other reader from what it has, so asking to wait would be a
        // budget nobody is waiting on.
        let wait_ms = if nonblocking || copied != 0 {
            0
        } else {
            SESSIOND_CONSOLE_READ_WAIT_MAX_MS
        };
        let route_started_ns = crate::arch::rtc::monotonic_nanos();
        let response = call_sessiond_console_route(
            snapshot.process_id(),
            snapshot.thread_id(),
            session.raw(),
            COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ,
            &[],
            chunk_len,
            wait_ms,
        )?;
        let route_elapsed_ns = crate::arch::rtc::monotonic_nanos().saturating_sub(route_started_ns);
        route_ns = route_ns.saturating_add(route_elapsed_ns);
        let read = response.payload_len as usize;
        if read > chunk_len {
            return Err(LINUX_EINVAL);
        }
        if read == 0 {
            if let Some(result) = empty_console_read_result(nonblocking, copied) {
                return result;
            }
            empty_polls = empty_polls.saturating_add(1);
            // A parked read spends its whole budget before answering empty, so
            // re-asking immediately is right. An early empty answer means the
            // broker did not park it - its park bound is full, or it predates
            // parking - and re-asking immediately would be a hot spin against a
            // service that has nothing to give. Sleep exactly as the old loop
            // did in that case. This fallback is not dead code: it is the only
            // thing standing between a full park queue and a spinning kernel.
            if route_elapsed_ns < unparked_console_read_threshold_ns(wait_ms) {
                let sleep_started_ns = crate::arch::rtc::monotonic_nanos();
                multitask::yield_now();
                crate::arch::rtc::sleep(1);
                sleep_ns = sleep_ns.saturating_add(
                    crate::arch::rtc::monotonic_nanos().saturating_sub(sleep_started_ns),
                );
            }
            continue;
        }
        if empty_polls != 0 {
            record_console_read_wait(empty_polls, waited_from_ns, route_ns, sleep_ns);
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

/// Publishes how long one blocking console read spun before data appeared.
///
/// Sampled on the same bounded schedule every high-frequency milestone in this
/// kernel uses: a debugcon line costs roughly 335 us fixed plus 11.1 us per
/// byte, and an idle shell polls about a thousand times a second, so an
/// unbounded record here would cost more than the thing it measures.
///
/// arg0=(empty poll passes, waited microseconds), arg1=running sample count.
fn record_console_read_wait(empty_polls: u64, waited_from_ns: u64, route_ns: u64, sleep_ns: u64) {
    use core::sync::atomic::{AtomicU64, Ordering};
    static CONSOLE_READ_WAITS: AtomicU64 = AtomicU64::new(0);
    const EARLY_CONSOLE_READ_WAIT_SAMPLES: u64 = 32;

    // ORDERING: Relaxed is exact; this counter owns diagnostics only and
    // orders nothing.
    let total = CONSOLE_READ_WAITS.fetch_add(1, Ordering::Relaxed) + 1;
    if total > EARLY_CONSOLE_READ_WAIT_SAMPLES && total % 256 != 0 {
        return;
    }
    let waited_us = crate::arch::rtc::monotonic_nanos().saturating_sub(waited_from_ns) / 1_000;
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "console-read-wait",
        ((empty_polls & 0xffff_ffff) << 32) | (waited_us & 0xffff_ffff),
        (((route_ns / 1_000) & 0xffff_ffff) << 32) | ((sleep_ns / 1_000) & 0xffff_ffff),
    );
    let _ = total;
}

/// Below this much elapsed time, an empty console read plainly did not wait,
/// whatever budget it asked for.
///
/// Half the requested budget is deliberately loose. It only has to separate
/// "the broker held this for a quarter second" from "the broker answered at
/// once", and those differ by orders of magnitude, so no plausible scheduling
/// jitter can move a read across this line. A zero budget gives a zero
/// threshold, which no caller can fall below - non-blocking readers never
/// reach the fallback.
fn unparked_console_read_threshold_ns(wait_ms: u64) -> u64 {
    wait_ms.saturating_mul(1_000_000) / 2
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

/// `wait_ms` is how long the console broker may hold a read that finds no
/// bytes. Zero is the immediate snapshot every non-read route uses and every
/// caller used before reads could be parked.
fn call_sessiond_console_route(
    subject_pid: u64,
    subject_tid: u64,
    session_handle: u64,
    route_request: u64,
    payload: &[u8],
    read_capacity: usize,
    wait_ms: u64,
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
    request.arg1 = wait_ms;
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
    diagnostics::log_slow_service_call(
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
    diagnostics::log_slow_service_call(
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
    diagnostics::log_slow_service_call(
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

/// Memo of devmgrd's ioctl routing answers.
///
/// # Why a routing question is worth memoizing and an authorization is not
/// devmgrd answers `DEVMGRD_IPC_OP_IOCTL_ROUTE` with `ioctl_route(request)`, a
/// pure total function of the request number: no fd, no pid, no credentials,
/// no session reach the decision. It is a static classification table that
/// happens to live in another address space. Authorization is the opposite -
/// `DEVMGRD_IPC_OP_IOCTL_AUTHORIZE` reads the caller - and is deliberately not
/// cached here; the forwarded path still pays it on every call.
///
/// # Why it mattered
/// Uncached, every console ioctl cost a devmgrd round trip to re-derive a
/// constant, on top of the round trip that did the work. A single keystroke
/// drives four of them - deliver the key, then read state, sessions, and each
/// session's output - so half of roughly ten broker round trips per keystroke
/// were spent asking a question whose inputs never change. Measured at 8 vCPU
/// that path carried a median 36 ms from key to pixel with 98% of it outside
/// every phase uiserver times.
///
/// # Why the epoch is part of the key
/// The table is compiled into devmgrd, so a different devmgrd may answer
/// differently. Registration advances the service epoch, and an entry recorded
/// under a superseded epoch is discarded rather than trusted.
struct IoctlRouteMemo {
    epoch: u64,
    len: usize,
    entries: [(u64, u64); IOCTL_ROUTE_MEMO_CAPACITY],
}

/// Distinct ioctl numbers worth remembering. The hot set is the console and
/// display surface; past this the memo simply stops growing and those callers
/// pay the query they paid before.
const IOCTL_ROUTE_MEMO_CAPACITY: usize = 32;

static IOCTL_ROUTE_MEMO: TrackedSpinLock<
    IoctlRouteMemo,
    { LockClass::DeviceIoctlRouteMemo as u8 },
> = TrackedSpinLock::new(IoctlRouteMemo {
    epoch: 0,
    len: 0,
    entries: [(0, 0); IOCTL_ROUTE_MEMO_CAPACITY],
});

fn memoized_ioctl_route(request_number: u64, epoch: u64) -> Option<u64> {
    let memo = IOCTL_ROUTE_MEMO.lock();
    if memo.epoch != epoch {
        return None;
    }
    memo.entries[..memo.len]
        .iter()
        .find(|(number, _)| *number == request_number)
        .map(|(_, route)| *route)
}

fn record_ioctl_route(request_number: u64, epoch: u64, route: u64) {
    let mut memo = IOCTL_ROUTE_MEMO.lock();
    if memo.epoch != epoch {
        memo.epoch = epoch;
        memo.len = 0;
    }
    if memo.entries[..memo.len]
        .iter()
        .any(|(number, _)| *number == request_number)
    {
        return;
    }
    if memo.len == IOCTL_ROUTE_MEMO_CAPACITY {
        return;
    }
    let index = memo.len;
    memo.entries[index] = (request_number, route);
    memo.len = index + 1;
}

pub fn ioctl_route_via_devmgrd(fd: u64, request_number: u64) -> Result<u64, i64> {
    // Zero means devmgrd has never published, and an unpublished service has
    // given no answer worth remembering.
    let epoch = ipc_ops::service_endpoint_epoch(linux_abi::IPC_SERVICE_DEVMGRD).unwrap_or(0);
    if epoch != 0 {
        if let Some(route) = memoized_ioctl_route(request_number, epoch) {
            return Ok(route);
        }
    }
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
    diagnostics::log_slow_service_call(
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
        | rustos_user_abi::syscall::DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT => {
            // Recorded only after the answer has passed every envelope and
            // range check above, so the memo can never hold a value the
            // uncached path would have rejected.
            if epoch != 0 {
                record_ioctl_route(request_number, epoch, response.value);
            }
            Ok(response.value)
        }
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
    // A nonblocking read cannot legitimately need the bulk-data rail. Its
    // contract is to answer now or report that there is nothing, so a caller
    // that waits 30 seconds for one is not waiting on data - it is waiting on a
    // wedged service, and `EAGAIN` is a complete answer.
    //
    // The rail mattered: uiserver's input reader panics its own process at
    // 3,000 ms, ten times sooner than this deadline can return anything, so the
    // read had no path to report the stall it was in. Measured at 8 vCPU it
    // took exactly that route - "read_input blocked elapsed_ms=3106
    // read_attempts=507 completed_reads=506" - and the compositor teardown that
    // followed ended the FPS proof at ten windows.
    //
    // A blocking read keeps the bulk rail. Its completion wait is genuinely
    // open-ended, and cancelling one mid-authorization is what the cancellable
    // authorization lease this file already notes has to land first.
    let class = if request.flags & INPUTD_READ_FLAG_NONBLOCK != 0 {
        ipc_ops::ServiceIpcClass::InteractiveControl
    } else {
        ipc_ops::ServiceIpcClass::BulkData
    };
    let response =
        ipc_ops::call_service_endpoint_with_class(IPC_SERVICE_INPUTD, as_bytes(request), class)?;
    let detail = if request.flags & INPUTD_READ_FLAG_NONBLOCK != 0 {
        Some("nonblock")
    } else {
        Some("blocking")
    };
    diagnostics::log_slow_service_call(
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
    if netd_reply_uses_prepared_socket(request) {
        return call_netd_socket_ipc_request_impl(request, timeout_ms);
    }
    let start_ticks = crate::arch::rtc::ticks();
    let request_len = NETD_IPC_REQUEST_HEADER_SIZE
        .checked_add(request.payload_len as usize)
        .filter(|len| *len <= size_of::<NetdIpcRequest>())
        .ok_or(LINUX_EINVAL)?;
    let request_bytes = &as_bytes(request)[..request_len];
    // TRANSPORT BOUNDARY. Everything below this point means netd answered.
    // A failure *here* means the reply never arrived, and that is the only
    // case where the caller cannot tell whether the byte movement happened -
    // the request is already on netd's endpoint queue and netd still runs it.
    // Recording it further out would also catch netd's own status replies,
    // where `EAGAIN` on an empty non-blocking receive is the ordinary answer.
    let response = match timeout_ms {
        Some(timeout_ms) => ipc_ops::call_service_endpoint_with_class_deadline(
            IPC_SERVICE_NETD,
            request_bytes,
            deadline::netd_timeout_class(request.op),
            timeout_ms,
        ),
        None => ipc_ops::call_service_endpoint_with_class(
            IPC_SERVICE_NETD,
            request_bytes,
            ipc_ops::ServiceIpcClass::BulkData,
        ),
    };
    let response = match response {
        Ok(response) => response,
        Err(errno) => {
            if super::vfs_meta::socket_op_moves_stream_bytes(request.op) {
                super::vfs_meta::record_stream_break(
                    super::vfs_meta::STREAM_BREAK_ABANDONED_IN_FLIGHT,
                    request,
                    errno.unsigned_abs() as usize,
                );
            }
            return Err(errno);
        }
    };
    let elapsed_ms = ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks());
    diagnostics::log_slow_service_call(
        "netd",
        request.op,
        elapsed_ms,
        request.pid,
        request.tid,
        response.len() as i64,
        None,
    );
    let decoded = decode_netd_ipc_response(request, response.as_slice())?;
    finish_netd_ipc_response(decoded, super::vfs_meta::consume_netd_release_payload)
}

fn finish_netd_ipc_response(
    mut decoded: NetdIpcResponse,
    release: impl FnOnce(&mut NetdIpcResponse) -> Result<(), i64>,
) -> Result<NetdIpcResponse, i64> {
    // A reference mutation may carry cleanup even on an error; validate and
    // settle its custody before exposing the operation status.
    release(&mut decoded)?;
    if decoded.status != 0 {
        return Err(decoded.status.unsigned_abs() as i64);
    }
    // The scheduling hint is consumed at the kernel IPC boundary and is not
    // part of the Linux socket result exposed to compatibility callers.
    decoded.reserved1 = 0;
    Ok(decoded)
}

fn decode_netd_ipc_response(
    request: &NetdIpcRequest,
    response: &[u8],
) -> Result<NetdIpcResponse, i64> {
    if response.len() < NETD_IPC_RESPONSE_HEADER_SIZE
        || response.len() > size_of::<NetdIpcResponse>()
    {
        return Err(LINUX_EINVAL);
    }
    let mut decoded = NetdIpcResponse::default();
    // SAFETY: The byte slice was bounded to the concrete response object
    // layout above, and `NetdIpcResponse` is a C wire record copied without
    // dereferencing any pointer embedded in its bytes.
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
        || decoded.reserved0 > decoded.payload_len
        || (decoded.reserved0 != 0
            && !matches!(
                request.op,
                SYSCALL_OFFLOAD_OP_LINUX_CLOSE | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
            ))
        || decoded.reserved1 & !NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF != 0
    {
        return Err(LINUX_EINVAL);
    }
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

#[cfg(test)]
mod tests;

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
