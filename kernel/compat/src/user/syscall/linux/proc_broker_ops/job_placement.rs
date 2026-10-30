//! Job-authorized loader placement. A prepare retains identity, not a lifetime
//! pin or a descriptor: kill/recycle is rechecked by spawn reservation and birth.
//! Legacy spawn inherits the requester, never the privileged loader. Exec and
//! fork do not use placement authority and preserve existing membership.
use super::*;
use kernel_object::api::{HandleRights, JobHandleRights};
use rustos_user_abi::syscall::PROC_BROKER_PREPARE_FLAG_JOB_HANDLE;

pub(super) fn prepare_shape_valid(args: &RustosProcPrepareBrokerArgs) -> bool {
    let exec = args.flags & PROC_BROKER_PREPARE_FLAG_EXEC_TICKET != 0;
    let job = args.flags & PROC_BROKER_PREPARE_FLAG_JOB_HANDLE != 0;
    args.abi_version == PROC_BROKER_ABI_VERSION
        && args.flags & !(PROC_BROKER_PREPARE_FLAG_EXEC_TICKET | PROC_BROKER_PREPARE_FLAG_JOB_HANDLE) == 0
        && exec == (args.reserved0 != 0)
        && (!exec || !job)
        && (job || args.job_handle == 0)
        && rustos_user_abi::syscall::proc_broker_stack_top_valid(args.stack_top)
}

fn job_from_entry(entry: &multitask::HandleEntry) -> Result<multitask::JobId, i64> {
    let (KernelHandle::Job(job), HandleRights::Job(rights)) = (entry.handle(), entry.rights()) else {
        return Err(LINUX_EBADF);
    };
    if !rights.contains(JobHandleRights::CREATE_PROCESS) {
        return Err(LINUX_EPERM);
    }
    Ok(*job)
}

pub(super) fn resolve_prepare_job(
    loader: u64,
    args: &RustosProcPrepareBrokerArgs,
) -> Result<Option<multitask::JobId>, i64> {
    if args.flags & PROC_BROKER_PREPARE_FLAG_JOB_HANDLE == 0 {
        return Ok(None);
    }
    resolve_descriptor(loader, args.job_handle).map(Some)
}

pub(super) fn resolve_descriptor(owner: u64, descriptor: u64) -> Result<multitask::JobId, i64> {
    let job = multitask::with_process_state_by_pid(owner, |state| {
        state.handles().get_entry(descriptor).ok_or(LINUX_EBADF).and_then(job_from_entry)
    }).ok_or(LINUX_ESRCH)??;
    let info = multitask::job_info(job).map_err(|_| LINUX_ESRCH)?;
    if info.state != multitask::JobState::Live {
        return Err(LINUX_EAGAIN);
    }
    Ok(job)
}

pub(super) fn spawn_job(
    explicit: Option<multitask::JobId>,
    requester: u64,
) -> Result<multitask::JobId, i64> {
    explicit.or_else(|| multitask::process_job(requester)).ok_or(LINUX_ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_requires_a_typed_create_process_descriptor() {
        let table = multitask::HandleTable::new();
        assert_eq!(job_from_entry(table.get_entry(0).unwrap()), Err(LINUX_EBADF));
        for rights in [JobHandleRights::INSPECT, JobHandleRights::CREATE_JOB, JobHandleRights::KILL] {
            let entry = multitask::HandleEntry::new_with_rights(
                KernelHandle::Job(multitask::ROOT_JOB), HandleRights::Job(rights), 0, 0,
            );
            assert_eq!(job_from_entry(&entry), Err(LINUX_EPERM));
        }
        let entry = multitask::HandleEntry::new_with_rights(
            KernelHandle::Job(multitask::ROOT_JOB), HandleRights::Job(JobHandleRights::CREATE_PROCESS), 0, 0,
        );
        assert_eq!(job_from_entry(&entry), Ok(multitask::ROOT_JOB));
    }

    #[test]
    fn placement_pipeline_binds_authority_before_mapping_and_keeps_authenticated_receives() {
        // Source witness only: production user-copy/kill races need runtime
        // evidence in addition to the bounded placement/lifecycle models.
        let placement = include_str!("job_placement.rs").split("#[cfg(test)]").next().unwrap();
        assert!(placement.contains("explicit.or_else(|| multitask::process_job(requester))"));
        assert!(placement.contains("info.state != multitask::JobState::Live"));
        let broker = include_str!("../proc_broker_ops.rs");
        let commit = broker.split_once("pub(super) fn syscall_linux_rustos_proc_commit_broker(").unwrap().1;
        let select = commit.find("job_placement::spawn_job(state.job, args.requester_pid)").unwrap();
        let reserve = commit.find("reserve_process_spawn_transaction_in(Some(job))").unwrap();
        assert!(select < reserve && reserve < commit.find("address_space_from_mappings(&state.mappings)").unwrap());
        let prepare = broker.split_once("pub(super) fn syscall_linux_rustos_proc_prepare_broker(").unwrap().1;
        assert!(prepare.find("job_placement::resolve_prepare_job(loader_pid, &args)").unwrap()
            < prepare.find("let state = ProcPrepareState").unwrap());
        let loader = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../services/loaderd/src/main.rs"));
        assert!(loader.contains("ipc::recv_with_handles_and_sender(&args)"));
        assert!(loader.contains("ipc::reply_recv_with_handles_and_sender(&args)"));
        assert!(loader.contains("request.requester_is_exact_sender(sender_pid)"));
        assert!(loader.contains("job_handle: job_handle.unwrap_or(0)"));
        let serve = loader.split_once("fn serve(endpoint: u64)").unwrap().1;
        assert!(serve.find("handle_wire_request(received as usize").unwrap()
            < serve.find("close_fds(handles.as_slice())").unwrap());
    }

    #[test]
    fn prepare_job_envelope_is_explicit_and_exec_cannot_retarget() {
        let plain = RustosProcPrepareBrokerArgs {
            abi_version: PROC_BROKER_ABI_VERSION,
            stack_top: PROC_BROKER_USER_SPACE_END_EXCLUSIVE,
            ..RustosProcPrepareBrokerArgs::default()
        };
        assert!(prepare_shape_valid(&plain));
        let job = RustosProcPrepareBrokerArgs { flags: PROC_BROKER_PREPARE_FLAG_JOB_HANDLE, ..plain };
        assert!(prepare_shape_valid(&job), "fd zero is authority only with the explicit flag");
        assert!(prepare_shape_valid(&RustosProcPrepareBrokerArgs { job_handle: 7, ..job }));
        let exec = RustosProcPrepareBrokerArgs { flags: PROC_BROKER_PREPARE_FLAG_EXEC_TICKET, reserved0: 9, ..plain };
        assert!(prepare_shape_valid(&exec));
        for bad in [
            RustosProcPrepareBrokerArgs { abi_version: PROC_BROKER_ABI_VERSION - 1, ..plain },
            RustosProcPrepareBrokerArgs { job_handle: 7, ..plain },
            RustosProcPrepareBrokerArgs { flags: 1 << 31, ..plain },
            RustosProcPrepareBrokerArgs { reserved0: 1, ..plain },
            RustosProcPrepareBrokerArgs { reserved0: 0, ..exec },
            RustosProcPrepareBrokerArgs { flags: exec.flags | job.flags, ..exec },
            RustosProcPrepareBrokerArgs { stack_top: 0, ..plain },
        ] { assert!(!prepare_shape_valid(&bad), "accepted {bad:?}"); }
    }
}
