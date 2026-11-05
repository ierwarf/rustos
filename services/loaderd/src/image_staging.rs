//! Serial loader receive-owner state, with bulk collection delegated to vfsd.
//! Only primary-image staging crosses turns; PE caches and process construction
//! remain on this one actor. No spawn reply capability is retained by a job.
use super::*;
use sha2::{Digest, Sha256};
use core::fmt::Write;
use loaderd::launch_leases::{LaunchLeases, LeaseError};
use rustos_user_abi::syscall::{LOADER_OP_POLL_WORK, LOADER_OP_CANCEL_WORK,
    LOADER_OP_VALIDATE_IMAGE, LOADER_WORK_PENDING, LOADER_APPLICATION_WORK_MAX_LEASE_MS};

const SLOTS: usize = 2;
struct PlacementFd(i32);
impl Drop for PlacementFd { fn drop(&mut self) { let _ = syscall1(SYS_CLOSE, self.0 as u64); } }
struct PendingImage { request: LoaderSpawnRequest, ticket: snapshot_client::Ticket, placement: Option<PlacementFd> }
pub(super) struct ImageStaging { leases: LaunchLeases<PendingImage, SLOTS> }
fn errno(error: LeaseError) -> i32 {
    match error { LeaseError::Full => 11, LeaseError::Exhausted => EOVERFLOW,
        LeaseError::Unknown => 2, LeaseError::Owner => EACCES, LeaseError::Lease => ETIMEDOUT }
}
fn digest_snapshot(snapshot: &ImmutableFileSnapshot, end_ns: u64) -> Result<String, i32> {
    let mut window = Vec::new();
    window.try_reserve_exact(ELF_READ_CHUNK_BYTES).map_err(|_| 12)?;
    window.resize(ELF_READ_CHUNK_BYTES, 0);
    let mut digest = Sha256::new();
    let mut offset = 0;
    while offset < snapshot.file_bytes {
        if rustos_svc_runtime::syscall::monotonic_nanos() >= end_ns { return Err(ETIMEDOUT); }
        let count = window.len().min((snapshot.file_bytes - offset) as usize);
        read_exact_at(snapshot.fd, offset, &mut window[..count])?;
        digest.update(&window[..count]);
        offset += count as u64;
    }
    if rustos_svc_runtime::syscall::monotonic_nanos() >= end_ns { return Err(ETIMEDOUT); }
    let mut hex = String::with_capacity(64);
    for byte in digest.finalize() { write!(&mut hex, "{byte:02x}").map_err(|_| EINVAL)?; }
    Ok(hex)
}
fn response(op: u16, status: i32) -> LoaderSpawnResponse {
    LoaderSpawnResponse { version: LOADER_REQUEST_ABI_VERSION, op, status, pid: -1, ..LoaderSpawnResponse::default() }
}
impl ImageStaging {
    pub(super) const fn new() -> Self { Self { leases: LaunchLeases::new() } }
    pub(super) fn reap(&mut self) {
        // Without client traffic, vfsd independently expires all large frame
        // payloads. This actor lazily reclaims its two tiny request/loan records
        // before admitting another request; it never caches a completed fd.
        while let Some(expired) = self.leases.expire_one(rustos_svc_runtime::syscall::monotonic_nanos()) { drop(expired); }
    }
    pub(super) fn cancel_owner(&mut self, id: u64, owner: (u64, u64)) {
        let retired = self.leases.cancel_owner(id, owner);
        drop(retired);
    }
    pub(super) fn begin(&mut self, request: &LoaderSpawnRequest, owner: (u64, u64), placement: Option<i32>,
        applications: &loaderd::application_jobs::ApplicationJobs) -> HandledLoaderRequest {
        // Receive custody moves here; no fcntl/job duplication or additional
        // rights are required from the attenuated CREATE_PROCESS|TRANSFER loan.
        let result = self.begin_inner(request, owner, placement.map(PlacementFd), applications);
        match result {
            Ok(id) => spawn_response(LoaderSpawnResponse { work_id: id, ..response(request.op, LOADER_WORK_PENDING) }),
            Err(status) => {
                debug_line(&format!("loaderd: image stage begin refused op={} errno={status}", request.op));
                spawn_response(response(request.op, status))
            },
        }
    }
    fn begin_inner(&mut self, request: &LoaderSpawnRequest, owner: (u64, u64), placement: Option<PlacementFd>,
        applications: &loaderd::application_jobs::ApplicationJobs) -> Result<u64, i32> {
        let now = rustos_svc_runtime::syscall::monotonic_nanos();
        if request.work_id != 0 || request.deadline_ns <= now { return Err(ETIMEDOUT); }
        if request.deadline_ns - now > LOADER_APPLICATION_WORK_MAX_LEASE_MS * NANOS_PER_MILLI { return Err(EINVAL); }
        if !self.leases.available() { return Err(11); }
        let application = request.op == LOADER_OP_SPAWN_EXEC
            && rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_SESSIOND, owner.0) >= 0;
        if application && (placement.is_some() || !applications.realm_configured()) { return Err(EACCES); }
        let path = request_text(&request.exec_path, request.exec_path_len as usize)?;
        let cap = if request.deadline_ns - now <= rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS * NANOS_PER_MILLI {
            EXECUTABLE_SNAPSHOT_HARD_LIMIT_MS
        } else { rustos_user_abi::syscall::VFS_EXECUTABLE_SNAPSHOT_MAX_LEASE_MS };
        let end = request.deadline_ns.min(now.saturating_add(cap * NANOS_PER_MILLI));
        let ticket = snapshot_client::Ticket::begin(path, request.target_pid, request.exec_dirfd, AbsoluteDeadline::after(end, 0))?;
        let pending = PendingImage { request: *request, ticket, placement };
        match self.leases.insert(owner, request.deadline_ns, pending) {
            Ok(id) => Ok(id), Err((error, pending)) => { drop(pending); Err(errno(error)) }
        }
    }
    pub(super) fn advance(&mut self, request: &LoaderSpawnRequest, owner: (u64, u64),
        applications: &mut loaderd::application_jobs::ApplicationJobs) -> HandledLoaderRequest {
        let expected = LoaderSpawnRequest { op: request.op, requester_pid: request.requester_pid,
            work_id: request.work_id, deadline_ns: request.deadline_ns, ..LoaderSpawnRequest::default() };
        if request.work_id == 0 || *request != expected { return spawn_response(response(request.op, EINVAL)); }
        if request.op == LOADER_OP_CANCEL_WORK {
            let retired = self.leases.take(request.work_id, owner, request.deadline_ns);
            let status = match retired { Ok(entry) => { drop(entry); 0 }, Err(error) => errno(error) };
            return spawn_response(response(request.op, status));
        }
        if request.op != LOADER_OP_POLL_WORK { return spawn_response(response(request.op, EINVAL)); }
        let pending = match self.leases.get_mut(request.work_id, owner, request.deadline_ns) {
            Ok(pending) => pending, Err(error) => return spawn_response(response(request.op, errno(error))),
        };
        let outcome = pending.ticket.poll();
        let (completed, total) = pending.ticket.progress();
        if let Ok(snapshot_client::Poll::Pending { completed, total }) = outcome {
            return spawn_response(LoaderSpawnResponse { work_id: request.work_id, completed_bytes: completed,
                file_bytes: total, ..response(request.op, LOADER_WORK_PENDING) });
        }
        let entry = match self.leases.take(request.work_id, owner, request.deadline_ns) {
            Ok(entry) => entry, Err(error) => return spawn_response(response(request.op, errno(error))),
        };
        let pending = entry.value;
        let snapshot = match outcome {
            Ok(snapshot_client::Poll::Ready(snapshot)) => snapshot,
            Err(status) => {
                debug_line(&format!("loaderd: staged snapshot failed work={} errno={status} completed={completed} total={total}", request.work_id));
                return spawn_response(LoaderSpawnResponse { completed_bytes: completed, file_bytes: total, ..response(request.op, status) });
            }
            Ok(snapshot_client::Poll::Pending { .. }) => unreachable!(),
        };
        if let Err(status) = authorize_loader_operation(pending.request.op, owner.0) {
            return spawn_response(LoaderSpawnResponse { completed_bytes: completed, file_bytes: total, ..response(request.op, status) });
        }
        if rustos_svc_runtime::syscall::monotonic_nanos() >= pending.request.deadline_ns {
            return spawn_response(LoaderSpawnResponse { completed_bytes: completed, file_bytes: total, ..response(request.op, ETIMEDOUT) });
        }
        if pending.request.op == LOADER_OP_VALIDATE_IMAGE {
            let mut reply = response(request.op, 0);
            reply.completed_bytes = completed; reply.file_bytes = total;
            let admission = validate_executable_fd(snapshot.fd).and_then(|image| {
                // Independent readback of the transferred sealed object, not
                // merely the input bytes acknowledged by the writer.
                let digest = digest_snapshot(&snapshot, pending.request.deadline_ns)?;
                debug_line(&format!("loaderd: staged-image digest exec={} bytes={} sha256={digest}", snapshot.resolved_path, snapshot.file_bytes));
                Ok(image.format())
            });
            match admission { Ok(format) => reply.image_format = format, Err(status) => reply.status = status }
            return spawn_response(reply); // snapshot Drop closes the only local fd.
        }
        let mut handled = application_launch::spawn(&pending.request, owner.0,
            pending.placement.as_ref().map(|fd| fd.0 as u64), applications, Some(snapshot));
        if let LoaderReply::Spawn(ref mut result) = handled.reply {
            result.op = request.op; // Echo the continuation wire envelope.
            result.completed_bytes = completed; result.file_bytes = total;
        }
        handled
    }
}
