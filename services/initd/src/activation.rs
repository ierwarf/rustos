//! Exact bounded post-init child activation and endpoint-admission handoff.
//!
//! - **Owner:** initd owns cohort construction and supervision; loaderd and
//!   ring0 own sender binding and atomic runnable publication.
//! - **Boundary:** child PIDs, loader replies, endpoint readiness, and running
//!   supervisor records are untrusted until exact admission.
//! - **Lifecycle:** build unique zero-tailed request → bounded loader call →
//!   exact response → endpoint barrier or immediate endpoint admission.
//! - **Concurrency:** initd's single supervisor loop owns the maps and vectors;
//!   independent children overlap only before the explicit endpoint barrier.
//! - **Failure:** malformed reply or uncertain activation triggers exact child
//!   cleanup and fail-closed termination.
//! - **Forbidden:** no partial activation, PID adoption, activation-as-readiness,
//!   unbounded wait, duplicate target, or storage-dependent UI bootstrap.
//! - **Evidence:** `atomic-process-activation-batch` and
//!   `post-init-bootstrap-barrier`.

use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;
use rustos_user_abi::syscall::{
    LoaderActivateBatchRequest, LoaderActivateBatchResponse, LOADER_ACTIVATE_BATCH_ABI_VERSION,
    LOADER_ACTIVATE_BATCH_MAX_TARGETS, LOADER_OP_ACTIVATE_BATCH, SYS_RUSTOS_IPC_CALL_BOUNDED,
};

use super::bootstrap_barrier::endpoint_admission_may_overlap;
use super::{
    admit_running_service_endpoint, boot_line, fail_closed, fail_closed_after_children_cleanup,
    lookup_loader_endpoint, RunningService, LOADER_ENDPOINT_CACHE, RUNTIMED_EXEC_PATH,
    SECONDARY_SERVICE_DEFER_AFTER_RUNTIMED,
};

fn activate_spawned_services(pids: &[i32]) -> Result<(), i32> {
    let request = activation_batch_request(pids)?;
    let endpoint = lookup_loader_endpoint()?;
    let mut response = LoaderActivateBatchResponse::default();
    // SAFETY: Request and response are initialized, exact-sized ABI records
    // and remain exclusively live for the duration of the bounded call.
    let call = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_CALL_BOUNDED as libc::c_long,
            endpoint,
            (&request as *const LoaderActivateBatchRequest) as u64,
            size_of::<LoaderActivateBatchRequest>() as u64,
            (&mut response as *mut LoaderActivateBatchResponse) as u64,
            size_of::<LoaderActivateBatchResponse>() as u64,
            IPC_BOOT_CONTROL_HARD_LIMIT_MS,
        ) as i64
    };
    if call < 0 {
        LOADER_ENDPOINT_CACHE.store(0, Ordering::Relaxed);
        return Err((-call) as i32);
    }
    if call as usize != size_of::<LoaderActivateBatchResponse>()
        || response.version != LOADER_ACTIVATE_BATCH_ABI_VERSION
        || response.op != LOADER_OP_ACTIVATE_BATCH
        || response.reserved0 != 0
    {
        return Err(libc::EINVAL);
    }
    if response.status != 0 {
        return Err(response.status);
    }
    if response.activated_count != u32::try_from(pids.len()).map_err(|_| libc::EINVAL)? {
        return Err(libc::EINVAL);
    }
    boot_line(&format!(
        "initd: service activation batch committed count={}",
        pids.len()
    ));
    Ok(())
}

fn activation_batch_request(pids: &[i32]) -> Result<LoaderActivateBatchRequest, i32> {
    if pids.is_empty() || pids.len() > LOADER_ACTIVATE_BATCH_MAX_TARGETS {
        return Err(libc::EINVAL);
    }
    let mut request = LoaderActivateBatchRequest {
        version: LOADER_ACTIVATE_BATCH_ABI_VERSION,
        op: LOADER_OP_ACTIVATE_BATCH,
        requester_pid: u64::from(std::process::id()),
        target_count: u16::try_from(pids.len()).map_err(|_| libc::EINVAL)?,
        ..LoaderActivateBatchRequest::default()
    };
    for (index, pid) in pids.iter().copied().enumerate() {
        if pid <= 0 || pids[..index].contains(&pid) {
            return Err(libc::EINVAL);
        }
        request.target_pids[index] = u64::try_from(pid).map_err(|_| libc::EINVAL)?;
    }
    Ok(request)
}

pub(super) fn activate_pending_services(
    pending: &mut Vec<i32>,
    running: &mut BTreeMap<i32, RunningService>,
    ready_packages: &mut BTreeSet<String>,
    launched_once_packages: &mut BTreeSet<String>,
    defer_secondary_services_until: &mut Option<Instant>,
) {
    if pending.is_empty() {
        return;
    }
    if let Err(errno) = activate_spawned_services(pending) {
        fail_closed_after_children_cleanup(
            pending,
            &format!(
                "initd: fatal atomic service activation failed count={} errno={errno}",
                pending.len()
            ),
        );
    }

    for pid in pending.drain(..) {
        let exec = running
            .get(&pid)
            .map(|service| service.exec.clone())
            .unwrap_or_else(|| {
                fail_closed(&format!(
                    "initd: activated child missing supervisor record pid={pid}"
                ))
            });
        if endpoint_admission_may_overlap(exec.as_str()) {
            boot_line(&format!(
                "initd: endpoint barrier pending exec={exec} pid={pid}"
            ));
        } else {
            admit_running_service_endpoint(running, ready_packages, launched_once_packages, pid);
        }
        if exec == RUNTIMED_EXEC_PATH && !SECONDARY_SERVICE_DEFER_AFTER_RUNTIMED.is_zero() {
            *defer_secondary_services_until =
                Some(Instant::now() + SECONDARY_SERVICE_DEFER_AFTER_RUNTIMED);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::activation_batch_request;
    use rustos_user_abi::syscall::{LOADER_ACTIVATE_BATCH_MAX_TARGETS, LOADER_OP_ACTIVATE_BATCH};

    #[test]
    fn activation_batch_is_exact_bounded_and_zero_tailed() {
        let request = activation_batch_request(&[81, 82, 83]).expect("valid batch");
        assert_eq!(request.op, LOADER_OP_ACTIVATE_BATCH);
        assert_eq!(request.target_count, 3);
        assert_eq!(&request.target_pids[..3], &[81, 82, 83]);
        assert!(request.target_pids[3..].iter().all(|pid| *pid == 0));
        assert!(activation_batch_request(&[]).is_err());
        assert!(activation_batch_request(&[81, 81]).is_err());
        assert!(activation_batch_request(&[0]).is_err());
        assert!(activation_batch_request(&vec![1; LOADER_ACTIVATE_BATCH_MAX_TARGETS + 1]).is_err());
    }
}
