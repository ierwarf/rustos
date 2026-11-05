//! Short snapshot control RPCs and an explicitly thread-bound job ticket.
//! The receive owner may advance one poll per turn; convenience callers can
//! await the same ticket without resetting its absolute lease.
use super::*;
use loaderd::snapshot_protocol::{validate_response, SnapshotReply};
use rustos_user_abi::syscall::{VFS_EXECUTABLE_SNAPSHOT_OP_POLL, VFS_EXECUTABLE_SNAPSHOT_OP_CANCEL};

struct Reply { response: VfsExecutableSnapshotResponse, fds: [u64; 1], count: u16 }
impl Drop for Reply {
    fn drop(&mut self) {
        for fd in self.fds.iter().take(usize::from(self.count).min(self.fds.len())) { let _ = syscall1(SYS_CLOSE, *fd); }
    }
}
impl Reply {
    fn take_fd(&mut self) -> Result<i32, i32> {
        if self.count != 1 { return Err(EINVAL); }
        let fd = i32::try_from(self.fds[0]).map_err(|_| EOVERFLOW)?;
        self.count = 0; Ok(fd)
    }
}
fn rpc(endpoint: u64, request: &VfsExecutableSnapshotRequest, deadline: AbsoluteDeadline) -> Result<Reply, i32> {
    let timeout = deadline.child_timeout_ms(rustos_svc_runtime::syscall::monotonic_nanos(),
        EXECUTABLE_SNAPSHOT_HARD_LIMIT_MS).map_err(|_| ETIMEDOUT)?;
    let mut reply = Reply { response: VfsExecutableSnapshotResponse::default(), fds: [0; 1], count: 0 };
    let args = IpcCallWithHandlesArgs {
        endpoint, request_ptr: request as *const _ as u64,
        request_len: size_of::<VfsExecutableSnapshotRequest>() as u64,
        reply_ptr: &mut reply.response as *mut _ as u64,
        reply_capacity: size_of::<VfsExecutableSnapshotResponse>() as u64,
        recv_fd_capacity: 1, recv_fds_ptr: reply.fds.as_mut_ptr() as u64,
        recv_fd_count_ptr: &mut reply.count as *mut _ as u64,
        ..IpcCallWithHandlesArgs::default()
    };
    // SAFETY: initialized request/output records and descriptor storage remain
    // disjoint and live until the bounded synchronous copy finishes.
    let status = unsafe { rustos_svc_runtime::syscall::syscall2(
        SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED, &args as *const _ as u64, timeout) };
    if status < 0 { return Err((-status) as i32); }
    if status as usize != size_of::<VfsExecutableSnapshotResponse>() { return Err(EINVAL); }
    Ok(reply)
}
struct JobLease { endpoint: u64, request: VfsExecutableSnapshotRequest, active: bool }
impl Drop for JobLease {
    fn drop(&mut self) {
        if !self.active { return; }
        self.request.op = VFS_EXECUTABLE_SNAPSHOT_OP_CANCEL;
        // Cleanup transport grace does not renew the provider's job lease.
        let grace = AbsoluteDeadline::after(rustos_svc_runtime::syscall::monotonic_nanos(),
            rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS * NANOS_PER_MILLI);
        let _ = rpc(self.endpoint, &self.request, grace);
    }
}
pub(super) enum Poll { Pending { completed: u64, total: u64 }, Ready(ImmutableFileSnapshot) }
pub(super) struct Ticket {
    lease: JobLease,
    deadline: AbsoluteDeadline,
    completed: u64,
    total: u64,
    next_report: u64,
    path: String,
    // PID/TID binding is part of provider authority, not merely bookkeeping.
    _thread_bound: core::marker::PhantomData<alloc::rc::Rc<()>>,
}
impl Ticket {
    pub(super) fn begin(path: &str, target_pid: u64, dirfd: u64, deadline: AbsoluteDeadline) -> Result<Self, i32> {
        if path.is_empty() || path.len() > VFS_IPC_PATH_CAPACITY || path.as_bytes().contains(&0) { return Err(EINVAL); }
        let endpoint = rustos_svc_runtime::ipc::lookup_service_endpoint(IPC_SERVICE_VFSD);
        if endpoint < 0 { return Err((-endpoint) as i32); }
        let pid = syscall0(SYS_GETPID); let tid = syscall0(SYS_GETTID);
        if pid <= 0 || tid <= 0 { return Err(EACCES); }
        let mut request = VfsExecutableSnapshotRequest { requester_pid: pid as u64, requester_tid: tid as u64,
            target_pid, dirfd, max_bytes: VFS_EXECUTABLE_SNAPSHOT_MAX_FILE_BYTES,
            path_len: path.len() as u32, deadline_ns: deadline.end_ns(), ..VfsExecutableSnapshotRequest::default() };
        request.path[..path.len()].copy_from_slice(path.as_bytes());
        let reply = rpc(endpoint as u64, &request, deadline)?;
        let id = match validate_response(&reply.response, request.op, 0, reply.count)? {
            SnapshotReply::Pending { id, .. } => id,
            SnapshotReply::Failed(errno) => return Err(errno), SnapshotReply::Ready => return Err(EINVAL),
        };
        Ok(Self {
            lease: JobLease { endpoint: endpoint as u64, active: true,
                request: VfsExecutableSnapshotRequest { op: VFS_EXECUTABLE_SNAPSHOT_OP_POLL,
                    requester_pid: pid as u64, requester_tid: tid as u64, job_id: id,
                    deadline_ns: deadline.end_ns(), ..VfsExecutableSnapshotRequest::default() } },
            deadline, completed: 0, total: 0, next_report: 64 * 1024 * 1024, path: String::from(path),
            _thread_bound: core::marker::PhantomData,
        })
    }
    pub(super) fn poll(&mut self) -> Result<Poll, i32> {
        let mut reply = rpc(self.lease.endpoint, &self.lease.request, self.deadline)?;
        match validate_response(&reply.response, self.lease.request.op, self.lease.request.job_id, reply.count)? {
            SnapshotReply::Failed(errno) => { self.lease.active = false; Err(errno) }
            SnapshotReply::Ready => {
                self.lease.active = false;
                if reply.response.file_bytes < self.completed || (self.total != 0 && self.total != reply.response.file_bytes) { return Err(EINVAL); }
                let path = core::str::from_utf8(&reply.response.resolved_path[..reply.response.resolved_path_len as usize]).map_err(|_| EINVAL)?;
                let resolved_path = String::from(path);
                let fd = reply.take_fd()?;
                self.completed = reply.response.file_bytes; self.total = reply.response.file_bytes;
                Ok(Poll::Ready(ImmutableFileSnapshot { fd, file_bytes: self.total, resolved_path }))
            }
            SnapshotReply::Pending { completed, total, .. } => {
                if completed < self.completed || (self.total != 0 && self.total != total) { return Err(EINVAL); }
                self.completed = completed; self.total = total;
                if completed >= self.next_report {
                    debug_line(&format!("loaderd: snapshot progress exec={} completed={completed} total={total}", self.path));
                    self.next_report += 64 * 1024 * 1024;
                }
                Ok(Poll::Pending { completed, total })
            }
        }
    }
    pub(super) fn progress(&self) -> (u64, u64) { (self.completed, self.total) }
}
pub(super) fn open(path: &str, target_pid: u64, dirfd: u64, deadline: AbsoluteDeadline) -> Result<ImmutableFileSnapshot, i32> {
    let mut ticket = Ticket::begin(path, target_pid, dirfd, deadline)?;
    loop {
        match ticket.poll()? { Poll::Ready(snapshot) => return Ok(snapshot), Poll::Pending { .. } => rustos_svc_runtime::syscall::sleep_millis(5) }
    }
}
