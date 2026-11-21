//! Application-specific placement and terminal-reply custody at loader ingress.
use super::*;
use loaderd::application_jobs::{memory_probe_pages, ApplicationJobs, NativeJobs};
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
    applications: &mut ApplicationJobs, snapshot: Option<ImmutableFileSnapshot>,
) -> HandledLoaderRequest {
    let application = request.op == LOADER_OP_SPAWN_EXEC
        && rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_SESSIOND, sender_pid) >= 0;
    if !application {
        // A Job request only exists for application Jobs; a service spawn that
        // carries one is a malformed request, not a request to honor.
        if request.application_request.is_declared() {
            return spawn_response(LoaderSpawnResponse {
                version: LOADER_REQUEST_ABI_VERSION, op: request.op,
                status: EINVAL, pid: -1, ..LoaderSpawnResponse::default()
            });
        }
        return handle_placed_request(request, placement, snapshot);
    }
    // Runtime clients cannot substitute a service/foreign placement descriptor.
    if placement.is_some() {
        return spawn_response(LoaderSpawnResponse {
            version: LOADER_REQUEST_ABI_VERSION, op: request.op,
            status: EACCES, pid: -1, ..LoaderSpawnResponse::default()
        });
    }
    // The request's exact path range was validated at authenticated ingress.
    // The application's request is granted against the capacity observed now
    // (exactly, or refused naming the limit); the opt-in proof tightens only the
    // hog's page limit, not UI/service launch budgets.
    let prepared = match applications.prepare(
        &request.application_request,
        memory_probe_pages(
            option_env!("RUSTOS_LOADERD_LIMIT_PROBE") == Some("1"),
            &request.exec_path[..request.exec_path_len as usize],
        ),
        &mut NativeJobs,
    ) {
        Ok(prepared) => prepared,
        Err(failure) => {
            if let Some(refusal) = failure.refusal {
                debug_line(&format!(
                    "loaderd: application job refused {refusal} request={}/{}/{} (processes/threads/pages-per-process, 0=default)",
                    request.application_request.processes,
                    request.application_request.threads,
                    request.application_request.memory_pages_per_process,
                ));
            }
            return spawn_response(LoaderSpawnResponse {
                version: LOADER_REQUEST_ABI_VERSION, op: request.op,
                status: failure.errno, pid: -1, ..LoaderSpawnResponse::default()
            });
        }
    };
    let handle = prepared.handle;
    let mut handled = handle_placed_request(request, Some(handle), snapshot);
    if let LoaderReply::Spawn(response) = &mut handled.reply {
        if response.status == 0 && response.pid > 0 {
            match applications.published(handle, response.pid as u64, &mut NativeJobs) {
                Ok(job) => {
                    handled.application_job = Some(handle);
                    debug_line(&format!(
                        "loaderd: application job placed job/pid/limit: {job} {} {} granted processes/threads/pages-per-process: {}/{}/{}",
                        response.pid, prepared.limits.memory_pages, prepared.grant.processes,
                        prepared.grant.threads, prepared.grant.memory_pages_per_process,
                    ));
                    return handled;
                }
                Err(errno) => { response.status = errno; response.pid = -1; }
            }
        }
    }
    applications.abort(handle, &mut NativeJobs);
    handled
}
