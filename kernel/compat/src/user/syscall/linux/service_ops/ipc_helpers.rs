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
use rustos_user_abi::syscall::COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY;
use rustos_user_abi::wire::UserWire;

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

fn normalize_proc_components(raw_path: &str) -> Option<Vec<&str>> {
    if !raw_path.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for segment in raw_path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    Some(parts)
}

fn parse_decimal_pid(text: &str) -> Option<u64> {
    if text.is_empty() || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.bytes().all(|b| b.is_ascii_digit()).then(|| text.parse().ok()).flatten()
}

pub(super) fn resolve_exec_path(
    raw_path: &str,
    caller_pid: Option<u64>,
    caller_exec_path: Option<&str>,
) -> Result<String, i64> {
    if let Some(parts) = normalize_proc_components(raw_path)
        && let ["proc", dir, "exe"] = parts.as_slice()
    {
        let matches_caller = *dir == "self"
            || *dir == "thread-self"
            || caller_pid.is_some_and(|pid| parse_decimal_pid(dir) == Some(pid));
        if !matches_caller {
            return Err(LINUX_ENOENT);
        }
        return caller_exec_path
            .filter(|path| path.starts_with('/'))
            .map(String::from)
            .ok_or(LINUX_ENOENT);
    }
    Ok(String::from(raw_path))
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
    let caller_pid = multitask::current_user_process_id();
    let caller_exec_path = multitask::with_current_user_process_state(|_, _, state| {
        String::from(state.exec_path())
    });
    let exec_path = match resolve_exec_path(
        raw_exec_path.as_str(),
        caller_pid,
        caller_exec_path.as_deref(),
    ) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut request = new_procd_request(op);
    request.dirfd = (linux_abi::AT_FDCWD as i64) as u64;
    request.flags = flags as u32;
    if exec_path.len() > request.path.len() {
        return linux_errno(LINUX_EINVAL);
    }
    request.path_len = exec_path.len() as u32;
    request.path[..exec_path.len()].copy_from_slice(exec_path.as_bytes());
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
    let child_pid = match call_procd(&request) {
        Ok(response) if response.status == 0 => response.result as u64,
        Ok(response) => return fork_refused(1, response.status.unsigned_abs() as i64),
        Err(errno) => return fork_refused(2, errno),
    };
    if clone_flags & linux_abi::CLONE_VFORK != 0 {
        wait_for_vfork_completion(child_pid);
    }
    child_pid
}

fn wait_for_vfork_completion(child_pid: u64) {
    let Some(parent_tid) = multitask::current_task_id() else {
        return;
    };
    let parent_pid = multitask::current_user_process_id();
    loop {
        if multitask::take_vfork_done(child_pid, parent_tid) {
            return;
        }
        while !multitask::arm_block_current_task() {
            if multitask::current_task_id() != Some(parent_tid) {
                multitask::cancel_vfork_wait(child_pid, parent_tid);
                return;
            }
            if multitask::take_vfork_done(child_pid, parent_tid) {
                return;
            }
            multitask::cond_resched();
        }
        if multitask::take_vfork_done(child_pid, parent_tid) {
            let _ = multitask::cancel_block_current_task();
            return;
        }
        if !multitask::is_user_task_alive(child_pid)
            || parent_pid.is_some_and(multitask::is_user_process_exiting)
        {
            let _ = multitask::cancel_block_current_task();
            multitask::cancel_vfork_wait(child_pid, parent_tid);
            return;
        }
        // The vfork parent runs on the child's shared stack, so the wait is
        // killable only (as on Linux): a pending signal must not resume it
        // early. The parent-exiting exit above is the only abandon path.
        if multitask::commit_block_current_task_and_yield().is_none() {
            multitask::yield_now();
            continue;
        }
        if multitask::take_vfork_done(child_pid, parent_tid) {
            return;
        }
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

pub fn new_procd_request(op: u16) -> alloc::boxed::Box<ProcdIpcRequest> {
    let stamp = CallerStamp::current_ids_only();
    let mut request = alloc_box_zeroed::<ProcdIpcRequest>();
    request.version = rustos_user_abi::syscall::PROCD_IPC_ABI_VERSION;
    request.op = op;
    request.pid = stamp.pid;
    request.tid = stamp.tid;
    request.parent_pid = if stamp.pid != 0 {
        multitask::parent_process_id_of(stamp.pid).unwrap_or(0)
    } else {
        0
    };
    if let Some(thread_state) = multitask::current_linux_thread_state() {
        request.arg5 = thread_state.signal_mask;
    }
    request
}

pub fn call_procd(request: &ProcdIpcRequest) -> Result<alloc::boxed::Box<ProcdIpcResponse>, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let response: alloc::boxed::Box<ProcdIpcResponse> = call_fixed_service_rpc_boxed(
        IPC_SERVICE_PROCD,
        request,
        ipc_ops::ServiceIpcClass::BootControl,
        None,
    )?;
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
    check_ipc_status(response.status)
}

pub fn new_vfs_request(op: u16) -> alloc::boxed::Box<VfsIpcRequest> {
    let mut request = alloc_box_zeroed::<VfsIpcRequest>();
    request.version = VFS_IPC_ABI_VERSION;
    request.op = op;
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
    // IPC receive stamps the sender from the task's log identity, which
    // remains available after the process is sealed for exit. The visible
    // process snapshot deliberately disappears at that point, but final
    // CLOSE and its checkpoint ACK must still identify their actual sender.
    set_vfs_sender_identity(request, multitask::current_user_log_ids());
    let stamp = CallerStamp::current();
    request.session_handle = stamp.session_handle;
    request.uid = stamp.uid;
    request.gid = stamp.gid;
    request.euid = stamp.euid;
    request.egid = stamp.egid;
}

fn set_vfs_sender_identity(request: &mut VfsIpcRequest, sender: Option<(u64, u64)>) {
    (request.pid, request.tid) = sender.unwrap_or((0, 0));
}

pub fn populate_vfs_path(request: &mut VfsIpcRequest, path: &str) -> Result<(), i64> {
    let bytes = path.as_bytes();
    if bytes.len() > VFS_IPC_PATH_CAPACITY {
        return Err(LINUX_ENAMETOOLONG);
    }
    request.path_len = bytes.len() as u32;
    request.path[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

fn check_dirfd_handle_kind(kind: multitask::RemoteVfsHandleKind, path: &str) -> Result<(), i64> {
    if !path.is_empty() && kind != multitask::RemoteVfsHandleKind::Directory {
        Err(LINUX_ENOTDIR)
    } else {
        Ok(())
    }
}

pub(super) fn resolve_vfs_dirfd_remote_id(dirfd: u64, path: &str) -> Result<u64, i64> {
    if !path.starts_with('/') && !is_linux_at_fdcwd_for_vfs(dirfd) {
        let remote = current_remote_vfs_handle(dirfd).ok_or(LINUX_EBADF)?;
        check_dirfd_handle_kind(remote.kind(), path)?;
        return Ok(remote.remote_id());
    }
    Ok(0)
}

pub fn populate_vfs_path_base(
    request: &mut VfsIpcRequest,
    dirfd: u64,
    path: &str,
) -> Result<(), i64> {
    request.dirfd = if is_linux_at_fdcwd_for_vfs(dirfd) {
        linux_abi::AT_FDCWD as u64
    } else {
        dirfd
    };
    // A generated proc directory is a typed snapshot, not a remote VFS
    // handle. Send its canonical proc path back to vfsd so that namespace
    // lookup and the caller-only PID check still happen there.
    if !path.is_empty() && !path.starts_with('/') && !is_linux_at_fdcwd_for_vfs(dirfd) {
        if let Some(memfd) = current_memfd_handle(dirfd) {
            if let Some(info) = memfd.proc_snapshot() {
                if !info.directory {
                    return Err(LINUX_ENOTDIR);
                }
                let combined = proc_directory_child_path(&info.link, path);
                request.dirfd = linux_abi::AT_FDCWD as u64;
                return populate_vfs_path(request, &combined);
            }
        }
    }
    let remote_id = resolve_vfs_dirfd_remote_id(dirfd, path)?;
    if remote_id != 0 {
        request.remote_id = remote_id;
    }
    populate_vfs_path(request, path)
}

fn proc_directory_child_path(base: &str, path: &str) -> alloc::string::String {
    // Keep components intact; vfsd owns normalization and caller checks.
    alloc::format!("{}/{}", base, path)
}

#[cfg(test)]
mod proc_directory_base_tests {
    use super::{check_dirfd_handle_kind, multitask, proc_directory_child_path, LINUX_ENOTDIR};

    #[test]
    fn relative_path_retains_components_for_vfs_namespace_resolution() {
        assert_eq!(proc_directory_child_path("/proc/7/fd", "3"), "/proc/7/fd/3");
        assert_eq!(proc_directory_child_path("/proc/7/task/7", "../7/stat"), "/proc/7/task/7/../7/stat");
    }

    #[test]
    fn empty_path_allows_non_directory_remote_vfs_handles() {
        assert_eq!(
            check_dirfd_handle_kind(multitask::RemoteVfsHandleKind::File, ""),
            Ok(())
        );
        assert_eq!(
            check_dirfd_handle_kind(multitask::RemoteVfsHandleKind::Device, ""),
            Ok(())
        );
        assert_eq!(
            check_dirfd_handle_kind(multitask::RemoteVfsHandleKind::File, "child"),
            Err(LINUX_ENOTDIR)
        );
        assert_eq!(
            check_dirfd_handle_kind(multitask::RemoteVfsHandleKind::Directory, "child"),
            Ok(())
        );
    }
}

#[cfg(test)]
mod ipc_helpers_audit_tests {
    use alloc::boxed::Box;
    use super::*;

    #[test]
    fn netd_request_bytes_rejects_oversized_payload_len() {
        let mut req = Box::new(NetdIpcRequest::default());
        req.payload_len = 0;
        assert_eq!(
            netd_request_bytes(&req).map(<[u8]>::len),
            Ok(NETD_IPC_REQUEST_HEADER_SIZE)
        );
        req.payload_len = 16;
        assert_eq!(
            netd_request_bytes(&req).map(<[u8]>::len),
            Ok(NETD_IPC_REQUEST_HEADER_SIZE + 16)
        );
        req.payload_len = u32::MAX;
        assert_eq!(netd_request_bytes(&req), Err(LINUX_EINVAL));
    }

    #[test]
    fn console_readiness_response_rejects_unknown_flags_or_zero_generation() {
        let mut resp = Box::new(CommercialMaxProtocolResponse::default());
        resp.value1 = 7;
        resp.value0 = SESSIOND_CONSOLE_READINESS_READY | SESSIOND_CONSOLE_READINESS_LIVE;
        assert_eq!(check_console_readiness_response(&resp), Ok(()));

        resp.value1 = 0;
        assert_eq!(check_console_readiness_response(&resp), Err(LINUX_EINVAL));

        resp.value1 = 7;
        resp.payload_len = 1;
        assert_eq!(check_console_readiness_response(&resp), Err(LINUX_EINVAL));

        resp.payload_len = 0;
        resp.value0 = !SESSIOND_CONSOLE_READINESS_MASK;
        assert_eq!(check_console_readiness_response(&resp), Err(LINUX_EINVAL));
    }

    #[test]
    fn devmgrd_ioctl_response_validates_envelope_and_payload_policy() {
        let mut resp = Box::new(DevmgrdDeviceIoctlResponse {
            version: rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION,
            op: rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE,
            status: -(LINUX_EPERM as i32),
            value: 0,
            payload_len: 0,
            reserved1: 0,
            reserved0: 0,
            ..DevmgrdDeviceIoctlResponse::default()
        });
        assert_eq!(
            validate_devmgrd_ioctl_response(
                rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE,
                false,
                &resp,
            ),
            Ok(-(LINUX_EPERM as i32))
        );

        resp.payload_len = 4;
        assert_eq!(
            validate_devmgrd_ioctl_response(
                rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE,
                false,
                &resp,
            ),
            Err(LINUX_EINVAL)
        );
        assert_eq!(
            validate_devmgrd_ioctl_response(
                rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE,
                true,
                &resp,
            ),
            Ok(-(LINUX_EPERM as i32))
        );

        resp.payload_len = (resp.payload.len() as u32) + 1;
        assert_eq!(
            validate_devmgrd_ioctl_response(
                rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE,
                true,
                &resp,
            ),
            Err(LINUX_EINVAL)
        );

        resp.payload_len = 0;
        resp.reserved1 = 1;
        assert_eq!(
            validate_devmgrd_ioctl_response(
                rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE,
                false,
                &resp,
            ),
            Err(LINUX_EINVAL)
        );
    }

    #[test]
    fn inputd_header_requires_matching_version_op_and_zero_flags() {
        assert_eq!(validate_inputd_header(INPUTD_IPC_ABI_VERSION, 9, 0, 9), Ok(()));
        assert_eq!(
            validate_inputd_header(INPUTD_IPC_ABI_VERSION.wrapping_add(1), 9, 0, 9),
            Err(LINUX_EINVAL)
        );
        assert_eq!(validate_inputd_header(INPUTD_IPC_ABI_VERSION, 8, 0, 9), Err(LINUX_EINVAL));
        assert_eq!(validate_inputd_header(INPUTD_IPC_ABI_VERSION, 9, 1, 9), Err(LINUX_EINVAL));
    }

    #[test]
    fn decode_and_check_vfs_reply_validates_size_envelope_and_status() {
        assert_eq!(decode_and_check_vfs_reply(VFS_IPC_OP_CLOSE, &[0_u8; 4]), Err(LINUX_EINVAL));

        let mut resp = alloc_box_zeroed::<VfsIpcResponse>();
        resp.version = VFS_IPC_ABI_VERSION;
        resp.op = VFS_IPC_OP_CLOSE;
        assert_eq!(decode_and_check_vfs_reply(VFS_IPC_OP_CLOSE, as_bytes(&*resp)), Ok(()));

        resp.status = -(LINUX_EBUSY as i32);
        assert_eq!(
            decode_and_check_vfs_reply(VFS_IPC_OP_CLOSE, as_bytes(&*resp)),
            Err(LINUX_EBUSY)
        );

        resp.status = 0;
        resp.op = VFS_IPC_OP_OPENAT;
        assert_eq!(
            decode_and_check_vfs_reply(VFS_IPC_OP_CLOSE, as_bytes(&*resp)),
            Err(LINUX_EINVAL)
        );
    }
}

pub(super) fn is_linux_at_fdcwd_for_vfs(dirfd: u64) -> bool {
    const AT_FDCWD_I64: u64 = (-100_i64) as u64;
    const AT_FDCWD_I32: u64 = 0xffff_ff9c;
    dirfd == AT_FDCWD_I64 || dirfd == AT_FDCWD_I32 || dirfd == linux_abi::AT_FDCWD as u64
}

pub fn call_vfs_ipc_request(request: &VfsIpcRequest) -> Result<alloc::boxed::Box<VfsIpcResponse>, i64> {
    call_vfs_ipc_request_impl(request, None)
}

pub fn call_vfs_ipc_request_with_timeout(
    request: &VfsIpcRequest,
    timeout_ms: u64,
) -> Result<alloc::boxed::Box<VfsIpcResponse>, i64> {
    call_vfs_ipc_request_impl(request, Some(timeout_ms.max(1)))
}

pub fn call_vfs_checked(request: &VfsIpcRequest) -> LinuxResult<alloc::boxed::Box<VfsIpcResponse>> {
    let response = call_pinned_remote_vfs_request(request)?;
    ensure_vfs_status(&response)?;
    Ok(response)
}

pub fn call_vfs_status(request: &VfsIpcRequest) -> LinuxResult<()> {
    let response = call_pinned_remote_vfs_request(request)?;
    ensure_vfs_status(&response)
}

pub fn call_vfs_path_status(
    op: u16,
    dirfd: u64,
    path_ptr: u64,
    configure: impl FnOnce(&mut VfsIpcRequest),
) -> LinuxResult<()> {
    let path = copy_current_user_path(path_ptr, VFS_IPC_PATH_CAPACITY)?;
    let mut request = new_vfs_request(op);
    configure(&mut request);
    populate_vfs_path_base(&mut request, dirfd, &path)?;
    call_vfs_status(&request)
}

pub fn call_vfs_fd_status(
    op: u16,
    fd: u64,
    remote_id: u64,
    configure: impl FnOnce(&mut VfsIpcRequest),
) -> LinuxResult<()> {
    let mut request = new_vfs_request(op);
    request.fd = fd;
    request.remote_id = remote_id;
    configure(&mut request);
    call_vfs_status(&request)
}

pub fn require_remote_vfs_with_rights(
    fd: u64,
    non_vfs_errno: i64,
) -> LinuxResult<(multitask::RemoteVfsHandle, kernel_object::api::HandleRights)> {
    current_remote_vfs_handle_with_rights(fd).ok_or_else(|| {
        if current_kernel_handle(fd).is_some() {
            non_vfs_errno
        } else {
            LINUX_EBADF
        }
    })
}

fn vfs_request_is_replay_safe(request: &VfsIpcRequest) -> bool {
    matches!(
        request.op,
        VFS_IPC_OP_OPENAT
            | VFS_IPC_OP_CLOSE
            | VFS_IPC_OP_READ
            | VFS_IPC_OP_PREAD64
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
        || request.op == rustos_user_abi::syscall::VFS_IPC_OP_FILE_LOCK
        // Moves the inode's bytes into a vfsd-owned memfd once; the reply names
        // that memfd, so a short interactive deadline would orphan the upgrade.
        || request.op == rustos_user_abi::syscall::VFS_IPC_OP_SHARED_BACKING
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

fn call_service_bytes_with_optional_deadline(
    service_id: u64,
    request_bytes: &[u8],
    class: ipc_ops::ServiceIpcClass,
    timeout_ms: Option<u64>,
) -> LinuxResult<Vec<u8>> {
    match timeout_ms {
        Some(timeout_ms) => ipc_ops::call_service_endpoint_with_class_deadline(
            service_id,
            request_bytes,
            class,
            timeout_ms,
        ),
        None => ipc_ops::call_service_endpoint_with_class(service_id, request_bytes, class),
    }
}

fn call_checked_service_rpc_boxed<Req, Resp>(
    service_name: &'static str,
    service_id: u64,
    op: u16,
    pid: u64,
    tid: u64,
    request: &Req,
    class: ipc_ops::ServiceIpcClass,
    timeout_ms: Option<u64>,
    log_slow: bool,
    detail: Option<&str>,
    validate: impl FnOnce(&Resp) -> LinuxResult<i32>,
) -> LinuxResult<alloc::boxed::Box<Resp>> {
    let start_ticks = log_slow.then(crate::arch::rtc::ticks);
    let response: alloc::boxed::Box<Resp> =
        call_fixed_service_rpc_boxed(service_id, request, class, timeout_ms)?;
    if let Some(start) = start_ticks {
        diagnostics::log_slow_service_call(
            service_name,
            op,
            ticks_elapsed_ms(start, crate::arch::rtc::ticks()),
            pid,
            tid,
            size_of::<Resp>() as i64,
            detail,
        );
    }
    let status = validate(&response)?;
    check_ipc_status(status)?;
    Ok(response)
}

fn decode_and_check_vfs_reply(op: u16, bytes: &[u8]) -> LinuxResult<()> {
    let response = decode_fixed_response_boxed::<VfsIpcResponse>(bytes)?;
    validate_vfs_response_envelope(op, &response)?;
    check_ipc_status(response.status)
}

fn call_vfs_ipc_request_impl(
    request: &VfsIpcRequest,
    timeout_ms: Option<u64>,
) -> Result<alloc::boxed::Box<VfsIpcResponse>, i64> {
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
        match call_service_bytes_with_optional_deadline(
            IPC_SERVICE_VFSD,
            as_bytes(request),
            service_class,
            attempt_timeout_ms,
        ) {
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
    let response = match decode_fixed_response_boxed::<VfsIpcResponse>(response.as_slice())
        .and_then(|decoded| {
            validate_vfs_response_envelope(request.op, &decoded)?;
            Ok(decoded)
        }) {
        Ok(decoded) => decoded,
        Err(errno) => {
            if deferred_reconcile {
                enqueue_pending_vfs_mutation(request)?;
            }
            return Err(errno);
        }
    };
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
    let err = match ipc_ops::call_service_endpoint_with_class_deadline(
        IPC_SERVICE_VFSD,
        as_bytes(ack.as_ref()),
        vfs_request_service_class(&ack),
        rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
    ) {
        Ok(bytes) => match decode_and_check_vfs_reply(ack.op, &bytes) {
            Ok(()) => return Ok(()),
            Err(errno) => errno,
        },
        Err(errno) => errno,
    };
    enqueue_captured_vfs_mutation(PendingVfsMutation::capture_ack(original, &ack)?)?;
    Err(err)
}

fn vfs_checkpoint_ack_request(original: &VfsIpcRequest) -> alloc::boxed::Box<VfsIpcRequest> {
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

/// Explicitly synchronizes a dirty kernel-owned regular-file cursor to `vfsd`
/// (`VFS_CURSOR_SETTLE_SYNC`). Returns the settled cursor confirmed by `vfsd`.
pub fn settle_vfs_cursor_sync(fd: u64, remote_id: u64, cursor: u64) -> Result<u64, i64> {
    if cursor > i64::MAX as u64 {
        return Err(LINUX_EINVAL);
    }
    let mut request = new_vfs_request(VFS_IPC_OP_CURSOR_SETTLE);
    request.fd = fd;
    request.remote_id = remote_id;
    request.arg0 = cursor;
    request.arg1 = 0;
    request.arg2 = VFS_CURSOR_SETTLE_SYNC;
    request.arg3 = 0;
    let response = call_vfs_ipc_request_with_timeout(
        &request,
        rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
    )?;
    ensure_vfs_status(&response)?;
    Ok(response.value)
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
            Some(false) | None => return attempted,
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
    let completed = ipc_ops::call_service_endpoint_with_class_deadline(
        IPC_SERVICE_VFSD,
        as_bytes(request.as_ref()),
        ipc_ops::ServiceIpcClass::StateMutation,
        rustos_user_abi::performance::IPC_STATE_MUTATION_HARD_LIMIT_MS,
    )
    .ok()
    .is_some_and(|bytes| decode_and_check_vfs_reply(request.op, &bytes).is_ok());
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
    linux_result((|| {
        let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
        if user_len == 0 {
            return Ok(0);
        }
        let mut request = new_inputd_request(INPUTD_IPC_OP_AUTHORIZE_READ, inputd_access);
        request.flags = ((status_flags & linux_abi::O_NONBLOCK != 0) as u32)
            * INPUTD_READ_FLAG_NONBLOCK;
        request.fd = fd;
        request.requested_len = user_len.min(INPUTD_READ_PAYLOAD_CAPACITY) as u64;
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
        call_inputd_ipc_request(&request)?;
        request.op = INPUTD_IPC_OP_READ;
        let response = call_inputd_read_request(&request)?;
        if response.payload_len as u64 > request.requested_len {
            return Err(LINUX_EINVAL);
        }
        let read = response.payload_len as usize;
        if read == 0 {
            return if status_flags & linux_abi::O_NONBLOCK != 0 {
                Err(LINUX_EAGAIN)
            } else {
                Ok(0)
            };
        }
        if user_ptr.checked_add(read as u64).is_none() {
            return Err(LINUX_EINVAL);
        }
        write_user_bytes_as(user_ptr, &response.payload[..read])?;
        Ok(read as u64)
    })())
}

fn validate_inputd_header(version: u16, op: u16, flags: u32, expected_op: u16) -> LinuxResult<()> {
    if version != INPUTD_IPC_ABI_VERSION || op != expected_op || flags != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

fn call_inputd_ipc_request_fixed(
    request: &InputdIpcRequest,
    class: ipc_ops::ServiceIpcClass,
    timeout_ms: Option<u64>,
) -> LinuxResult<InputdIpcResponse> {
    call_checked_service_rpc_boxed::<InputdIpcRequest, InputdIpcResponse>(
        "inputd",
        IPC_SERVICE_INPUTD,
        request.op,
        request.pid,
        request.tid,
        request,
        class,
        timeout_ms,
        false,
        None,
        |response: &InputdIpcResponse| {
            validate_inputd_header(response.version, response.op, response.flags, request.op)?;
            Ok(response.status)
        },
    )
    .map(|boxed| *boxed)
}

fn call_inputd_ipc_request(request: &InputdIpcRequest) -> Result<InputdIpcResponse, i64> {
    let response =
        call_inputd_ipc_request_fixed(request, ipc_ops::ServiceIpcClass::InteractiveControl, None)?;
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
    let mut request = new_inputd_request(INPUTD_IPC_OP_STATS, access);
    request.requested_len = 1;
    let response = call_inputd_ipc_request_fixed(
        &request,
        ipc_ops::ServiceIpcClass::ReadinessQuery,
        Some(timeout_ms),
    )?;
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

fn current_non_system_console_session() -> LinuxResult<u64> {
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    let session = snapshot.console_session();
    if session.is_system() {
        return Err(LINUX_EINVAL);
    }
    Ok(session.raw())
}

fn call_sessiond_commercial(
    op: u16,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    payload: &[u8],
    class: ipc_ops::ServiceIpcClass,
    timeout_ms: Option<u64>,
    log_slow: bool,
) -> LinuxResult<alloc::boxed::Box<CommercialMaxProtocolResponse>> {
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    let mut request = alloc_box_zeroed::<CommercialMaxProtocolRequest>();
    if payload.len() > request.payload.len() {
        return Err(LINUX_EINVAL);
    }
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_SESSIOND;
    request.header.op = op;
    request.header.service_id = IPC_SERVICE_SESSIOND;
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    request.arg0 = arg0;
    request.arg1 = arg1;
    request.arg2 = arg2;
    request.arg3 = arg3;
    request.payload[..payload.len()].copy_from_slice(payload);
    request.payload_len = payload.len() as u32;

    call_checked_service_rpc_boxed::<CommercialMaxProtocolRequest, CommercialMaxProtocolResponse>(
        "sessiond",
        IPC_SERVICE_SESSIOND,
        request.header.op,
        request.header.subject_pid,
        request.header.subject_tid,
        request.as_ref(),
        class,
        timeout_ms,
        log_slow,
        None,
        |response: &CommercialMaxProtocolResponse| {
            ipc_ops::validate_commercial_response_envelope(&request, response)?;
            if response.descriptor_count != 1 {
                return Err(LINUX_EINVAL);
            }
            Ok(response.status)
        },
    )
}

fn check_console_readiness_response(response: &CommercialMaxProtocolResponse) -> LinuxResult<()> {
    if response.payload_len != 0
        || response.value1 == 0
        || response.value0 & !SESSIOND_CONSOLE_READINESS_MASK != 0
    {
        return Err(LINUX_EINVAL);
    }
    Ok(())
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
    let response = call_sessiond_console_route(
        session_handle,
        COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS,
        0,
        0,
        &[],
        ipc_ops::ServiceIpcClass::ReadinessQuery,
        Some(timeout_ms),
        false,
    )?;
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
    let response = call_sessiond_console_route(
        known_generation,
        COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_GRAPH_READINESS,
        wait_ms.min(SESSIOND_CONSOLE_GRAPH_WAIT_MAX_MS),
        0,
        &[],
        ipc_ops::ServiceIpcClass::InteractiveControl,
        Some(rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS),
        false,
    )?;
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
    validate_user_write_as(user_ptr, user_len)?;
    let session = current_non_system_console_session()?;

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
        let chunk_len = (user_len - copied).min(COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY);
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
        let response = match call_sessiond_console_route(
            session,
            COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ,
            wait_ms,
            chunk_len,
            &[],
            ipc_ops::ServiceIpcClass::BulkData,
            None,
            true,
        ) {
            Ok(response) => response,
            Err(errno) => return if copied > 0 { Ok(copied as u64) } else { Err(errno) },
        };
        let route_elapsed_ns = crate::arch::rtc::monotonic_nanos().saturating_sub(route_started_ns);
        route_ns = route_ns.saturating_add(route_elapsed_ns);
        let read = response.payload_len as usize;
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
        let Some(dest) = user_ptr.checked_add(copied as u64) else {
            return if copied > 0 { Ok(copied as u64) } else { Err(LINUX_EINVAL) };
        };
        if let Err(errno) = write_user_bytes_as(dest, &response.payload[..read]) {
            return if copied > 0 { Ok(copied as u64) } else { Err(errno) };
        }
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
    let session = current_non_system_console_session()?;

    let mut copied = 0usize;
    let mut chunk = alloc::vec![0_u8; user_len.min(COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY)];
    while copied < user_len {
        let chunk_len = (user_len - copied).min(chunk.len());
        let Some(src) = user_ptr.checked_add(copied as u64) else {
            return if copied > 0 { Ok(copied as u64) } else { Err(LINUX_EINVAL) };
        };
        if let Err(errno) = read_user_bytes_as(src, &mut chunk[..chunk_len]) {
            return if copied > 0 { Ok(copied as u64) } else { Err(errno) };
        }
        let response = match call_sessiond_console_route(
            session,
            COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE,
            0,
            0,
            &chunk[..chunk_len],
            ipc_ops::ServiceIpcClass::BulkData,
            None,
            true,
        ) {
            Ok(response) => response,
            Err(errno) => return if copied > 0 { Ok(copied as u64) } else { Err(errno) },
        };
        let written = response.value0 as usize;
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
#[allow(clippy::too_many_arguments)]
fn call_sessiond_console_route(
    session_handle: u64,
    route_request: u64,
    wait_ms: u64,
    read_capacity: usize,
    payload: &[u8],
    class: ipc_ops::ServiceIpcClass,
    timeout_ms: Option<u64>,
    log_slow: bool,
) -> Result<alloc::boxed::Box<CommercialMaxProtocolResponse>, i64> {
    let response = call_sessiond_commercial(
        COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE,
        route_request,
        wait_ms,
        session_handle,
        read_capacity as u64,
        payload,
        class,
        timeout_ms,
        log_slow,
    )?;
    match route_request {
        COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ => {
            if response.payload_len as usize > read_capacity
                || response.value0 != u64::from(response.payload_len)
            {
                return Err(LINUX_EINVAL);
            }
        }
        COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE => {
            if response.payload_len != 0
                || usize::try_from(response.value0).map_or(true, |written| written > payload.len())
            {
                return Err(LINUX_EINVAL);
            }
        }
        COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS
        | COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_GRAPH_READINESS => {
            check_console_readiness_response(&response)?;
        }
        _ => return Err(LINUX_EINVAL),
    }
    Ok(response)
}

fn validate_devmgrd_open_reply(
    response: &[u8],
    entries: Vec<multitask::TransferredHandleEntry>,
) -> LinuxResult<multitask::TransferredHandleEntry> {
    let decoded = match decode_fixed_response::<DevmgrdDeviceOpenResponse>(response) {
        Ok(resp)
            if resp.version == rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION
                && resp.op == rustos_user_abi::syscall::DEVMGRD_IPC_OP_OPEN
                && resp.reserved0 == 0 =>
        {
            resp
        }
        _ => {
            ipc_ops::drop_transfer_entries(entries);
            return Err(LINUX_EINVAL);
        }
    };
    if let Err(errno) = check_ipc_status(decoded.status) {
        ipc_ops::drop_transfer_entries(entries);
        return Err(errno);
    }
    if entries.len() != 1 {
        ipc_ops::drop_transfer_entries(entries);
        return Err(LINUX_EINVAL);
    }
    entries.into_iter().next().ok_or(LINUX_EINVAL)
}

pub fn open_device_via_devmgrd(path: &str, flags: u64) -> Result<u64, i64> {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes.len() > VFS_IPC_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let stamp = CallerStamp::current_ids_only();
    let mut request = alloc_box_zeroed::<DevmgrdDeviceOpenRequest>();
    request.version = rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION;
    request.op = rustos_user_abi::syscall::DEVMGRD_IPC_OP_OPEN;
    request.open_flags = flags;
    request.path_len = bytes.len() as u32;
    request.pid = stamp.pid;
    request.tid = stamp.tid;
    request.path[..bytes.len()].copy_from_slice(bytes);
    let (response, entries) = ipc_ops::call_service_endpoint_with_received_entries(
        IPC_SERVICE_DEVMGRD,
        as_bytes(request.as_ref()),
        1,
    )?;
    let entry = validate_devmgrd_open_reply(response.as_slice(), entries)?;
    let input_token = match entry.entry().handle() {
        multitask::KernelHandle::Device(device)
            if device.device_id() == kernel_object::api::device::DeviceId::Input =>
        {
            Some(device.token_id())
        }
        _ => None,
    };
    let installed = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().install_transferred(entry)
    });
    match installed {
        Some(Some(fd)) => Ok(fd),
        other => {
            if let Some(token) = input_token {
                ipc_ops::release_input_transfer_token(token);
            }
            Err(if other.is_none() { LINUX_EINVAL } else { LINUX_EMFILE })
        }
    }
}

pub fn ioctl_tty_via_sessiond(fd: u64, request_number: u64, arg: u64) -> Result<u64, i64> {
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    let mut payload_buf = [0_u8; size_of::<linux_abi::LinuxTermios>()];
    let payload = if matches!(
        request_number,
        linux_abi::TCSETS | linux_abi::TCSETSW | linux_abi::TCSETSF
    ) {
        let termios = read_user_struct_as::<linux_abi::LinuxTermios>(arg)?;
        payload_buf.copy_from_slice(as_bytes(&termios));
        &payload_buf[..]
    } else {
        &[][..]
    };
    let response = call_sessiond_commercial(
        COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE,
        request_number,
        fd,
        snapshot.console_session().raw(),
        0,
        payload,
        ipc_ops::ServiceIpcClass::InteractiveControl,
        None,
        true,
    )?;

    match request_number {
        linux_abi::TCGETS => {
            copy_exact_payload_to_user_as(
                arg,
                &response.payload,
                response.payload_len,
                size_of::<linux_abi::LinuxTermios>(),
            )?;
            Ok(0)
        }
        linux_abi::TCSETS | linux_abi::TCSETSW | linux_abi::TCSETSF => {
            exact_payload(&response.payload, response.payload_len, 0)?;
            Ok(0)
        }
        linux_abi::FIONREAD => {
            exact_payload(&response.payload, response.payload_len, 0)?;
            let pending = response.value0.min(i32::MAX as u64) as i32;
            write_user_struct_as(arg, &pending)?;
            Ok(0)
        }
        _ => Err(LINUX_ENOTTY),
    }
}

fn validate_devmgrd_ioctl_response(
    op: u16,
    allow_payload: bool,
    response: &DevmgrdDeviceIoctlResponse,
) -> LinuxResult<i32> {
    let payload_ok = if allow_payload {
        response.payload_len as usize <= response.payload.len()
    } else {
        response.payload_len == 0
    };
    if response.version != rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION
        || response.op != op
        || !payload_ok
        || response.reserved1 != 0
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    Ok(response.status)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::user::syscall::linux) fn call_devmgrd_ioctl(
    fd: u64,
    op: u16,
    request_number: u64,
    arg: u64,
    require_ids: bool,
    allow_payload: bool,
    log_slow: bool,
    configure: impl FnOnce(&mut DevmgrdDeviceIoctlRequest) -> LinuxResult<()>,
) -> LinuxResult<alloc::boxed::Box<DevmgrdDeviceIoctlResponse>> {
    let mut request = new_devmgrd_ioctl_request(op, fd, request_number, arg);
    if require_ids && (request.pid == 0 || request.tid == 0) {
        return Err(LINUX_EINVAL);
    }
    configure(&mut request)?;
    call_checked_service_rpc_boxed::<DevmgrdDeviceIoctlRequest, DevmgrdDeviceIoctlResponse>(
        "devmgrd",
        IPC_SERVICE_DEVMGRD,
        request.op,
        request.pid,
        request.tid,
        request.as_ref(),
        ipc_ops::ServiceIpcClass::InteractiveControl,
        None,
        log_slow,
        None,
        |response| validate_devmgrd_ioctl_response(op, allow_payload, response),
    )
}

pub fn ioctl_device_via_devmgrd(fd: u64, request_number: u64, arg: u64) -> Result<u64, i64> {
    let response = call_devmgrd_ioctl(
        fd,
        rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE,
        request_number,
        arg,
        false,
        true,
        true,
        |request| populate_devmgrd_ioctl_payload(request, arg),
    )?;
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
    let response = call_devmgrd_ioctl(
        fd,
        rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_ROUTE,
        request_number,
        0,
        false,
        false,
        true,
        |_| Ok(()),
    )?;
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

fn copy_user_struct_to_devmgrd_payload<T: Copy + Default>(
    request: &mut DevmgrdDeviceIoctlRequest,
    arg: u64,
) -> LinuxResult<()> {
    let value = read_user_struct_as::<T>(arg)?;
    copy_struct_into_payload(&mut request.payload, &mut request.payload_len, &value)
}

fn populate_devmgrd_ioctl_payload(
    request: &mut DevmgrdDeviceIoctlRequest,
    arg: u64,
) -> Result<(), i64> {
    match request.request {
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSIONS => {
            copy_user_struct_to_devmgrd_payload::<
                rustos_user_abi::console::ConsoleSnapshotSessionsRequest,
            >(request, arg)
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT => {
            copy_user_struct_to_devmgrd_payload::<
                rustos_user_abi::console::ConsoleSnapshotSessionOutputRequest,
            >(request, arg)
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SEND_INPUT_EVENT => {
            copy_user_struct_to_devmgrd_payload::<
                rustos_user_abi::console::ConsoleSendInputEventRequest,
            >(request, arg)
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SET_FOCUS => {
            copy_user_struct_to_devmgrd_payload::<
                rustos_user_abi::console::ConsoleSetFocusRequest,
            >(request, arg)
        }
        _ => Ok(()),
    }
}

fn copy_devmgrd_snapshot_payload<H: Copy + UserWire, Item>(
    arg: u64,
    payload: &[u8],
    inspect: impl FnOnce(&H) -> LinuxResult<(u64, u64)>,
) -> LinuxResult<()> {
    let header_len = size_of::<H>();
    if payload.len() < header_len {
        return Err(LINUX_EINVAL);
    }
    let snapshot = read_unaligned::<H>(payload);
    let (out_ptr, count) = inspect(&snapshot)?;
    let count = usize::try_from(count).map_err(|_| LINUX_EINVAL)?;
    let data_len = count.checked_mul(size_of::<Item>()).ok_or(LINUX_EINVAL)?;
    if payload.len() != header_len + data_len {
        return Err(LINUX_EINVAL);
    }
    write_user_struct_as(arg, &snapshot)?;
    if data_len == 0 {
        return Ok(());
    }
    write_user_bytes_as(out_ptr, &payload[header_len..])
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
    let payload = response.payload.get(..payload_len).ok_or(LINUX_EINVAL)?;
    match request_number {
        rustos_user_abi::console::CONSOLE_IOCTL_GET_STATE => {
            let info: rustos_user_abi::console::ConsoleStateInfo =
                decode_exact_payload(&response.payload, response.payload_len)?;
            write_user_struct_as(arg, &info)
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSIONS => {
            copy_devmgrd_snapshot_payload::<
                rustos_user_abi::console::ConsoleSnapshotSessionsRequest,
                rustos_user_abi::console::ConsoleSessionInfo,
            >(arg, payload, |snapshot| {
                if snapshot.count > snapshot.capacity
                    || snapshot.count > rustos_user_abi::console::MAX_CONSOLE_SESSIONS as u64
                {
                    return Err(LINUX_EINVAL);
                }
                Ok((snapshot.sessions_ptr, snapshot.count))
            })
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT => {
            copy_devmgrd_snapshot_payload::<
                rustos_user_abi::console::ConsoleSnapshotSessionOutputRequest,
                u8,
            >(arg, payload, |snapshot| {
                if snapshot.count > snapshot.capacity {
                    return Err(LINUX_EINVAL);
                }
                Ok((snapshot.bytes_ptr, snapshot.count))
            })
        }
        _ => Err(LINUX_EINVAL),
    }
}

pub fn call_inputd_read_request(
    request: &InputdIpcRequest,
) -> Result<alloc::boxed::Box<InputdReadResponse>, i64> {
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
    let nonblocking = request.flags & INPUTD_READ_FLAG_NONBLOCK != 0;
    let (class, detail) = if nonblocking {
        (ipc_ops::ServiceIpcClass::InteractiveControl, Some("nonblock"))
    } else {
        (ipc_ops::ServiceIpcClass::BulkData, Some("blocking"))
    };
    call_checked_service_rpc_boxed::<InputdIpcRequest, InputdReadResponse>(
        "inputd",
        IPC_SERVICE_INPUTD,
        request.op,
        request.pid,
        request.tid,
        request,
        class,
        None,
        true,
        detail,
        |response: &InputdReadResponse| {
            validate_inputd_header(response.version, response.op, response.flags, request.op)?;
            if response.reserved0 != 0
                || response.payload_len as usize > INPUTD_READ_PAYLOAD_CAPACITY
            {
                return Err(LINUX_EINVAL);
            }
            Ok(response.status)
        },
    )
}

pub fn call_netd_ipc_request(
    request: &NetdIpcRequest,
) -> Result<alloc::boxed::Box<NetdIpcResponse>, i64> {
    call_netd_ipc_request_impl(request, None)
}

pub fn call_netd_ipc_request_with_timeout(
    request: &NetdIpcRequest,
    timeout_ms: u64,
) -> Result<alloc::boxed::Box<NetdIpcResponse>, i64> {
    call_netd_ipc_request_impl(request, Some(timeout_ms.max(1)))
}

pub(super) fn netd_request_bytes(request: &NetdIpcRequest) -> LinuxResult<&[u8]> {
    let request_len = NETD_IPC_REQUEST_HEADER_SIZE
        .checked_add(request.payload_len as usize)
        .filter(|len| *len <= size_of::<NetdIpcRequest>())
        .ok_or(LINUX_EINVAL)?;
    Ok(&as_bytes(request)[..request_len])
}

fn call_netd_ipc_request_impl(
    request: &NetdIpcRequest,
    timeout_ms: Option<u64>,
) -> Result<alloc::boxed::Box<NetdIpcResponse>, i64> {
    if netd_reply_uses_prepared_socket(request) {
        return call_netd_socket_ipc_request_impl(request, timeout_ms);
    }
    let start_ticks = crate::arch::rtc::ticks();
    let request_bytes = netd_request_bytes(request)?;
    let class = timeout_ms.map_or(ipc_ops::ServiceIpcClass::BulkData, |_| {
        deadline::netd_timeout_class(request.op)
    });
    // TRANSPORT BOUNDARY. Everything below this point means netd answered.
    // A failure *here* means the reply never arrived, and that is the only
    // case where the caller cannot tell whether the byte movement happened -
    // the request is already on netd's endpoint queue and netd still runs it.
    // Recording it further out would also catch netd's own status replies,
    // where `EAGAIN` on an empty non-blocking receive is the ordinary answer.
    let response = match call_service_bytes_with_optional_deadline(
        IPC_SERVICE_NETD,
        request_bytes,
        class,
        timeout_ms,
    ) {
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
    mut decoded: alloc::boxed::Box<NetdIpcResponse>,
    release: impl FnOnce(&mut NetdIpcResponse) -> Result<(), i64>,
) -> Result<alloc::boxed::Box<NetdIpcResponse>, i64> {
    // A reference mutation may carry cleanup even on an error; validate and
    // settle its custody before exposing the operation status.
    release(&mut decoded)?;
    check_ipc_status(decoded.status)?;
    // The scheduling hint is consumed at the kernel IPC boundary and is not
    // part of the Linux socket result exposed to compatibility callers.
    decoded.reserved1 &= !NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF;
    Ok(decoded)
}

fn decode_netd_ipc_response(
    request: &NetdIpcRequest,
    response: &[u8],
) -> Result<alloc::boxed::Box<NetdIpcResponse>, i64> {
    if response.len() < NETD_IPC_RESPONSE_HEADER_SIZE
        || response.len() > size_of::<NetdIpcResponse>()
    {
        return Err(LINUX_EINVAL);
    }
    let mut decoded = alloc_box_zeroed::<NetdIpcResponse>();
    // SAFETY: The byte slice was bounded to the concrete response object
    // layout above, and `NetdIpcResponse` is a C wire record copied without
    // dereferencing any pointer embedded in its bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(
            response.as_ptr(),
            (&mut *decoded as *mut NetdIpcResponse).cast::<u8>(),
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
    check_ipc_status(response.status)
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
    current_remote_vfs_handle_with_rights(fd).map(|(remote, _)| remote)
}

pub fn current_remote_vfs_handle_with_rights(
    fd: u64,
) -> Option<(multitask::RemoteVfsHandle, kernel_object::api::HandleRights)> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let entry = process_state.handles().get_entry(fd)?;
        match entry.handle() {
            multitask::KernelHandle::RemoteVfs(remote) => {
                Some((remote.clone(), entry.rights()))
            }
            _ => None,
        }
    })
    .flatten()
}

const LINUX_ENAMETOOLONG: i64 = 36;

/// Copies a path argument that names something, as nearly every path syscall
/// requires: Linux answers an empty name with `ENOENT`.
pub fn copy_current_user_path(ptr: u64, capacity: usize) -> Result<String, i64> {
    let path = copy_current_user_path_allow_empty(ptr, capacity)?;
    if path.is_empty() {
        return Err(LINUX_ENOENT);
    }
    Ok(path)
}

/// Copies a path argument whose empty string is meaningful (`AT_EMPTY_PATH`:
/// the operation applies to the descriptor itself). The caller owns rejecting
/// an empty path when that flag is absent.
pub fn copy_current_user_path_allow_empty(ptr: u64, capacity: usize) -> Result<String, i64> {
    if ptr == 0 {
        return Err(LINUX_EFAULT);
    }
    let path = usermem::read_current_user_c_string_bounded(ptr, capacity.saturating_add(1))
        .map_err(user_pointer_error_to_linux_errno)?
        .ok_or(LINUX_ENAMETOOLONG)?;
    if path.len() > capacity {
        return Err(LINUX_ENAMETOOLONG);
    }
    Ok(path)
}
