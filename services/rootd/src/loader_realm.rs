//! Rootd retains a separate application realm for each loader incarnation.
//! Lost provisioning replies retry the same realm. Blocking IPC always lends
//! the endpoint receiver to the existing cooperative supervisor, so an earlier
//! loader spawn can obtain its first VFSD authorization without a wait cycle.
use super::*;
use rustos_user_abi::syscall::{
    IpcCallWithHandlesArgs, LoaderJobRealmRequest, LoaderSpawnResponse,
    JOB_BOOTSTRAP_ROOT_FD, LOADER_APPLICATION_REALM_RIGHTS, LOADER_JOB_REALM_ABI_VERSION,
    LOADER_OP_CONFIGURE_JOB_REALM, LOADER_REQUEST_ABI_VERSION, SYS_RUSTOS_IPC_CALL_WITH_HANDLES,
};

pub(super) fn configure_ready(
    rootd_endpoint: u64, leases: &mut [Lease], post_init_leases: &mut [PostInitLease],
    service_checkpoints: &mut ServiceCheckpointStore,
) -> Result<(), i64> {
    let lease = leases.iter_mut().find(|lease| lease.service_id == IPC_SERVICE_LOADERD).ok_or(3)?;
    if lease.application_realm_configured { return Ok(()); }
    if lease.pid == 0 || lease.state != ROOTD_LEASE_STATE_RUNNING
        || rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_LOADERD, lease.pid) < 0 {
        return Err(11);
    }
    let endpoint = syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, IPC_SERVICE_LOADERD);
    if endpoint <= 0 { return Err(11); }
    let realm = match lease.application_realm {
        Some(handle) => handle,
        None => {
            let handle = jobs::create_child(JOB_BOOTSTRAP_ROOT_FD)?;
            lease.application_realm = Some(handle);
            handle
        }
    };
    let loader_pid = lease.pid;
    // Retain an independent descriptor BEFORE the worker can retire a lease.
    // A worker-side close/reuse of the lease fd cannot retarget this transfer.
    let borrowed = jobs::duplicate(realm, LOADER_APPLICATION_REALM_RIGHTS)?;
    let descriptor = match i32::try_from(borrowed) {
        Ok(fd) => fd,
        Err(_) => { jobs::close(borrowed); return Err(75); }
    };
    let result = with_loader_supervision(
        rootd_endpoint, leases, post_init_leases, service_checkpoints,
        || configure_blocking(endpoint as u64, descriptor).map(|()| 1),
    );
    jobs::close(borrowed);
    result?;
    // The worker may have consumed the loader's exit while the reply arrived.
    // Do not mark a retired/replaced incarnation as provisioned.
    let lease = leases.iter_mut().find(|lease| lease.service_id == IPC_SERVICE_LOADERD).ok_or(3)?;
    if lease.pid != loader_pid || lease.state != ROOTD_LEASE_STATE_RUNNING
        || lease.application_realm != Some(realm)
        || rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_LOADERD, loader_pid) < 0 {
        return Err(3);
    }
    lease.application_realm_configured = true;
    debug_line(b"rootd: loader application realm ok\n");
    Ok(())
}

fn configure_blocking(endpoint: u64, descriptor: i32) -> Result<(), i64> {
    let request = LoaderJobRealmRequest {
        version: LOADER_JOB_REALM_ABI_VERSION, op: LOADER_OP_CONFIGURE_JOB_REALM,
        requester_pid: syscall0(SYS_GETPID) as u64, reserved: 0,
    };
    let mut response = LoaderSpawnResponse::default();
    let mut received_count = 0_u16;
    let args = IpcCallWithHandlesArgs {
        endpoint,
        request_ptr: &request as *const _ as u64,
        request_len: core::mem::size_of::<LoaderJobRealmRequest>() as u64,
        reply_ptr: &mut response as *mut _ as u64,
        reply_capacity: core::mem::size_of::<LoaderSpawnResponse>() as u64,
        send_fds_ptr: &descriptor as *const _ as u64, send_fd_count: 1,
        recv_fd_count_ptr: &mut received_count as *mut _ as u64,
        ..IpcCallWithHandlesArgs::default()
    };
    let result = syscall1(SYS_RUSTOS_IPC_CALL_WITH_HANDLES, &args as *const _ as u64);
    if result < 0 { return Err(-result); }
    if result as usize != core::mem::size_of::<LoaderSpawnResponse>() || received_count != 0
        || response.version != LOADER_REQUEST_ABI_VERSION || response.op != LOADER_OP_CONFIGURE_JOB_REALM
        || response.pid != -1 || response.reserved0 != 0 {
        return Err(22);
    }
    if response.status != 0 { return Err(i64::from(response.status)); }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn provisioning_records_custody_before_transfer_and_retries_the_same_realm() {
        let source = include_str!("loader_realm.rs").split("#[cfg(test)]").next().unwrap();
        let existing = source.find("Some(handle) => handle").unwrap();
        let create = source.find("jobs::create_child(JOB_BOOTSTRAP_ROOT_FD)?").unwrap();
        let retain = source.find("lease.application_realm = Some(handle)").unwrap();
        let lend = source.find("jobs::duplicate(realm, LOADER_APPLICATION_REALM_RIGHTS)?").unwrap();
        let supervise = source.find("let result = with_loader_supervision(").unwrap();
        let confirm = source.find("lease.application_realm_configured = true").unwrap();
        assert!(existing < create && create < retain && retain < lend && lend < supervise && supervise < confirm);
        assert!(source.contains("validate_service_owner(IPC_SERVICE_LOADERD, lease.pid)"));
        assert!(source.contains("lease.pid != loader_pid"));
        assert!(source.contains("lease.application_realm != Some(realm)"));
        assert!(source.contains("response.op != LOADER_OP_CONFIGURE_JOB_REALM"));
        assert!(source.contains("jobs::close(borrowed)"));
        let main = include_str!("main.rs");
        let startup = main.split_once("fn spawn_initd_via_loaderd(").unwrap().1;
        assert!(startup.find("loader_realm::configure_ready(").unwrap()
            < startup.find("spawn_exec_via_loaderd_cooperative(").unwrap());
        let supervision = main.split_once("fn with_loader_supervision(").unwrap().1;
        assert!(supervision.contains("loader_transaction::run("));
        assert!(supervision.contains("|| start_loader_supervisor_worker("));
        assert!(supervision.contains("complete_loader_supervision,"));
    }
}
