//! Application-specific placement and terminal-reply custody at loader ingress.
use super::*;
use loaderd::application_jobs::{ApplicationJobs, NativeJobs, runtime_limits};
use rustos_user_abi::syscall::{LoaderJobRealmRequest, LOADER_OP_CONFIGURE_JOB_REALM};

pub(super) fn configure(
    bytes: &[u8], sender_pid: u64, descriptors: &[i32], applications: &mut ApplicationJobs,
) -> HandledLoaderRequest {
    let mut response = LoaderSpawnResponse {
        version: LOADER_REQUEST_ABI_VERSION, op: LOADER_OP_CONFIGURE_JOB_REALM,
        pid: -1, ..LoaderSpawnResponse::default()
    };
    if bytes.len() != size_of::<LoaderJobRealmRequest>() || descriptors.len() != 1
        || descriptors[0] < 0 {
        response.status = EINVAL;
        return spawn_response(response);
    }
    // SAFETY: exact length was checked; an unaligned copy accepts arbitrary IPC
    // byte alignment and every bit pattern of this integer-only wire is valid.
    let request = unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<LoaderJobRealmRequest>()) };
    if !request.is_valid(sender_pid)
        || authorize_loader_operation(request.op, sender_pid).is_err() {
        response.status = EACCES;
        return spawn_response(response);
    }
    match applications.install(descriptors[0] as u64, &mut NativeJobs) {
        Ok(_) => debug_line("loaderd: application realm ready"),
        Err(errno) => response.status = errno,
    }
    spawn_response(response)
}

pub(super) fn spawn(
    request: &LoaderSpawnRequest, sender_pid: u64, placement: Option<u64>,
    applications: &mut ApplicationJobs,
) -> HandledLoaderRequest {
    let application = request.op == LOADER_OP_SPAWN_EXEC
        && rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_SESSIOND, sender_pid) >= 0;
    if !application { return handle_placed_request(request, placement); }
    // Runtime clients cannot substitute a service/foreign placement descriptor.
    if placement.is_some() {
        return spawn_response(LoaderSpawnResponse {
            version: LOADER_REQUEST_ABI_VERSION, op: request.op,
            status: EACCES, pid: -1, ..LoaderSpawnResponse::default()
        });
    }
    // The request's exact path range was validated at authenticated ingress.
    // The opt-in proof tightens only the hog, not UI/service launch budgets.
    let limits = runtime_limits(
        option_env!("RUSTOS_LOADERD_LIMIT_PROBE") == Some("1"),
        &request.exec_path[..request.exec_path_len as usize],
    );
    let handle = match applications.prepare(limits, &mut NativeJobs) {
        Ok(handle) => handle,
        Err(errno) => return spawn_response(LoaderSpawnResponse {
            version: LOADER_REQUEST_ABI_VERSION, op: request.op,
            status: errno, pid: -1, ..LoaderSpawnResponse::default()
        }),
    };
    let mut handled = handle_placed_request(request, Some(handle));
    if let LoaderReply::Spawn(response) = &mut handled.reply {
        if response.status == 0 && response.pid > 0 {
            match applications.published(handle, response.pid as u64, &mut NativeJobs) {
                Ok(job) => {
                    handled.application_job = Some(handle);
                    debug_line(&format!("loaderd: application job placed job/pid/limit: {job} {} {}", response.pid, limits.memory_pages));
                    return handled;
                }
                Err(errno) => { response.status = errno; response.pid = -1; }
            }
        }
    }
    applications.abort(handle, &mut NativeJobs);
    handled
}
