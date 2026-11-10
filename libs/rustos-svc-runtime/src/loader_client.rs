//! Synchronous caller view over loaderd's bounded asynchronous image staging.
//! Each wire call remains short; polling never renews the absolute work lease.
use core::mem::size_of;
use rustos_user_abi::deadline::{AbsoluteDeadline, NANOS_PER_MILLI};
use rustos_user_abi::performance::{EXECUTABLE_SNAPSHOT_HARD_LIMIT_MS, IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS};
use rustos_user_abi::syscall::*;

#[derive(Debug, Eq, PartialEq)]
pub enum ReplyState { Pending(u64), Terminal }
pub fn validate_reply(response: &LoaderSpawnResponse, wire_op: u16, original_op: u16, id: u64) -> Result<ReplyState, i32> {
    if response.version != LOADER_REQUEST_ABI_VERSION || response.op != wire_op
        || response.reserved0 != 0 || response.reserved1 != 0 || response.reserved2 != 0
        || response.status < 0 || response.completed_bytes > response.file_bytes
        || response.file_bytes > VFS_EXECUTABLE_SNAPSHOT_MAX_FILE_BYTES
    { return Err(22); }
    if response.status == LOADER_WORK_PENDING {
        if response.work_id == 0 || (id != 0 && response.work_id != id)
            || response.pid != -1 || response.image_format != 0 { return Err(22); }
        return Ok(ReplyState::Pending(response.work_id));
    }
    if response.work_id != 0 { return Err(22); }
    if response.status == 0 {
        if !matches!(response.image_format, PROC_BROKER_FORMAT_ELF64 | PROC_BROKER_FORMAT_PE64)
            || response.file_bytes == 0 || response.completed_bytes != response.file_bytes
            || (original_op == LOADER_OP_SPAWN_EXEC && response.pid <= 0)
            || (original_op == LOADER_OP_EXEC_TARGET && response.pid < 0)
            || (original_op == LOADER_OP_VALIDATE_IMAGE && response.pid != -1)
        { return Err(22); }
    } else if response.pid > 0 { return Err(22); }
    Ok(ReplyState::Terminal)
}
pub fn placement_call_args(endpoint: u64, request: &LoaderSpawnRequest, response: &mut LoaderSpawnResponse,
    fd: &i32, received: &mut u16) -> IpcCallWithHandlesArgs {
    IpcCallWithHandlesArgs { endpoint, request_ptr: request as *const _ as u64,
        request_len: size_of::<LoaderSpawnRequest>() as u64, reply_ptr: response as *mut _ as u64,
        reply_capacity: size_of::<LoaderSpawnResponse>() as u64,
        send_fds_ptr: fd as *const _ as u64, send_fd_count: 1,
        recv_fd_count_ptr: received as *mut _ as u64, ..IpcCallWithHandlesArgs::default() }
}
fn rpc(endpoint: u64, request: &LoaderSpawnRequest, placement: Option<i32>, transport: AbsoluteDeadline) -> Result<LoaderSpawnResponse, i32> {
    let timeout = transport.child_timeout_ms(crate::syscall::monotonic_nanos(), EXECUTABLE_SNAPSHOT_HARD_LIMIT_MS).map_err(|_| 110)?;
    let mut response = LoaderSpawnResponse::default();
    let status = if let Some(fd) = placement {
        let mut received = 0u16;
        let args = placement_call_args(endpoint, request, &mut response, &fd, &mut received);
        // SAFETY: initialized exact wire buffers and one borrowed descriptor
        // remain live/disjoint for the bounded transfer. Replies admit no fd.
        let status = unsafe { crate::syscall::syscall2(SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED,
            &args as *const _ as u64, timeout) };
        if received != 0 { return Err(22); }
        status
    } else {
        // SAFETY: initialized exact-sized records remain live for the call.
        unsafe { crate::syscall::syscall6(SYS_RUSTOS_IPC_CALL_BOUNDED, endpoint,
            request as *const _ as u64, size_of::<LoaderSpawnRequest>() as u64,
            &mut response as *mut _ as u64, size_of::<LoaderSpawnResponse>() as u64, timeout) }
    };
    delivered_rpc_response(status, response)
}

fn delivered_rpc_response(status: i64, response: LoaderSpawnResponse) -> Result<LoaderSpawnResponse, i32> {
    if status < 0 { return Err((-status) as i32); }
    if status as usize != size_of::<LoaderSpawnResponse>() { return Err(22); }
    // The kernel admitted and consumed the reply under the original deadline.
    // A delivered terminal reply transfers child custody. Being scheduled
    // after expiry cannot discard its PID and orphan a committed spawn.
    Ok(response)
}
fn cancellation_deadline(request_end: u64, now: u64) -> AbsoluteDeadline {
    AbsoluteDeadline::after(request_end.min(now.saturating_add(
        IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS * NANOS_PER_MILLI,
    )), 0)
}

struct WorkLease { endpoint: u64, request: LoaderSpawnRequest, active: bool }
impl Drop for WorkLease {
    fn drop(&mut self) {
        if !self.active { return; }
        self.request.op = LOADER_OP_CANCEL_WORK;
        // Cleanup cannot extend the transaction that authorized this ticket.
        // At expiry loaderd/vfsd's independent lease retirement owns cleanup;
        // do not start another full RPC or fabricate time in a destructor.
        let Ok(now) = crate::syscall::try_monotonic_nanos() else { return; };
        let cleanup = cancellation_deadline(self.request.deadline_ns, now);
        if cleanup.remaining_ns(now).is_some() {
            let _ = rpc(self.endpoint, &self.request, None, cleanup);
        }
    }
}
/// `placement` is borrowed and exported only on begin. Returned terminal op is
/// the original operation (the semantic result, not the POLL wire envelope).
/// Transport failures are not retried; cancellation uses only the original
/// lease remainder. Expired tickets are retired independently by loaderd/vfsd.
pub fn call(endpoint: u64, request: &LoaderSpawnRequest, placement: Option<i32>) -> Result<LoaderSpawnResponse, i32> {
    if !matches!(request.op, LOADER_OP_SPAWN_EXEC | LOADER_OP_EXEC_TARGET | LOADER_OP_VALIDATE_IMAGE)
        || request.deadline_ns == 0 || request.work_id != 0 || request.requester_pid == 0
    { return Err(22); }
    let deadline = AbsoluteDeadline::after(request.deadline_ns, 0);
    let response = rpc(endpoint, request, placement, deadline)?;
    let id = match validate_reply(&response, request.op, request.op, 0)? {
        ReplyState::Terminal => return Ok(response), ReplyState::Pending(id) => id,
    };
    let mut lease = WorkLease { endpoint, active: true, request: LoaderSpawnRequest {
        op: LOADER_OP_POLL_WORK, requester_pid: request.requester_pid,
        work_id: id, deadline_ns: request.deadline_ns, ..LoaderSpawnRequest::default()
    } };
    let mut completed = response.completed_bytes;
    let mut total = response.file_bytes;
    loop {
        let mut response = rpc(endpoint, &lease.request, None, deadline)?;
        let state = validate_reply(&response, lease.request.op, request.op, id)?;
        if matches!(response.status, 0 | LOADER_WORK_PENDING)
            && (response.completed_bytes < completed || (total != 0 && total != response.file_bytes)) { return Err(22); }
        completed = response.completed_bytes;
        total = response.file_bytes;
        match state {
            ReplyState::Pending(_) => {
                let remaining = deadline.remaining_ns(crate::syscall::monotonic_nanos()).ok_or(110)?;
                crate::syscall::sleep_millis((remaining / NANOS_PER_MILLI).min(5));
            }
            ReplyState::Terminal => {
                lease.active = response.status != 0;
                response.op = request.op;
                return Ok(response);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_cancellation_never_remints_the_expired_parent_budget() {
        assert_eq!(cancellation_deadline(100, 99).remaining_ns(99), Some(1));
        assert_eq!(cancellation_deadline(100, 100).remaining_ns(100), None);
        assert_eq!(cancellation_deadline(100, 101).remaining_ns(101), None);
        assert_eq!(cancellation_deadline(u64::MAX, 1).remaining_ns(1), Some(IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS * NANOS_PER_MILLI));
    }
    #[test]
    fn a_delivered_spawn_keeps_child_custody_when_the_client_resumes_after_expiry() {
        let deadline = AbsoluteDeadline::after(0, 100);
        let response = LoaderSpawnResponse {
            version: LOADER_REQUEST_ABI_VERSION, op: LOADER_OP_POLL_WORK,
            pid: 41, image_format: PROC_BROKER_FORMAT_ELF64,
            completed_bytes: 8, file_bytes: 8, ..LoaderSpawnResponse::default()
        };
        // Delivery was settled before expiry; the caller is next scheduled
        // only at 101. The transport result, not a later clock sample, owns PID.
        assert!(deadline.remaining_ns(101).is_none());
        let delivered = delivered_rpc_response(size_of::<LoaderSpawnResponse>() as i64, response).unwrap();
        assert_eq!(validate_reply(&delivered, LOADER_OP_POLL_WORK, LOADER_OP_SPAWN_EXEC, 9), Ok(ReplyState::Terminal));
        assert_eq!(delivered.pid, 41);
    }

    #[test]
    fn placement_call_sends_exactly_one_borrowed_descriptor() {
        let request = LoaderSpawnRequest::default();
        let mut response = LoaderSpawnResponse::default();
        let descriptor = 9_i32;
        let mut count = 0_u16;
        let args = placement_call_args(7, &request, &mut response, &descriptor, &mut count);
        assert_eq!((args.endpoint, args.send_fd_count, args.recv_fd_capacity, args.reserved0), (7, 1, 0, 0));
        assert_eq!(args.send_fds_ptr, &descriptor as *const _ as u64);
        assert_eq!(args.recv_fd_count_ptr, &mut count as *mut _ as u64);
    }
    #[test]
    fn pending_never_impersonates_a_process_and_requires_exact_work_identity() {
        let mut r = LoaderSpawnResponse { version: LOADER_REQUEST_ABI_VERSION, op: LOADER_OP_SPAWN_EXEC,
            status: LOADER_WORK_PENDING, pid: -1, work_id: 9, ..LoaderSpawnResponse::default() };
        assert_eq!(validate_reply(&r, r.op, r.op, 0), Ok(ReplyState::Pending(9)));
        r.pid = 3;
        assert!(validate_reply(&r, r.op, r.op, 0).is_err());
        r.pid = -1; r.op = LOADER_OP_POLL_WORK;
        assert!(validate_reply(&r, r.op, LOADER_OP_SPAWN_EXEC, 8).is_err());
    }
    #[test]
    fn validate_only_is_distinct_from_spawn_and_terminal_has_no_live_ticket() {
        let mut r = LoaderSpawnResponse { version: LOADER_REQUEST_ABI_VERSION, op: LOADER_OP_POLL_WORK,
            pid: -1, image_format: PROC_BROKER_FORMAT_ELF64, completed_bytes: 8, file_bytes: 8,
            ..LoaderSpawnResponse::default() };
        assert_eq!(validate_reply(&r, r.op, LOADER_OP_VALIDATE_IMAGE, 9), Ok(ReplyState::Terminal));
        assert!(validate_reply(&r, r.op, LOADER_OP_SPAWN_EXEC, 9).is_err());
        r.pid = 3;
        assert!(validate_reply(&r, r.op, LOADER_OP_VALIDATE_IMAGE, 9).is_err());
        assert_eq!(validate_reply(&r, r.op, LOADER_OP_SPAWN_EXEC, 9), Ok(ReplyState::Terminal));
        r.work_id = 9;
        assert!(validate_reply(&r, r.op, LOADER_OP_SPAWN_EXEC, 9).is_err());
    }
}
