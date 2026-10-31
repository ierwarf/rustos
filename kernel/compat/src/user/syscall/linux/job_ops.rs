// RING3-MIGRATION-REFERENCE START: capability-broker exception: rootd owns job
// placement and kill policy. Ring0 keeps the job tree, group admission and the
// bounded group-kill mechanism.
//! `SYS_RUSTOS_JOB`: rights-scoped process groups.
//!
//! - **Owner:** `kernel-ps` owns jobs and descriptors; rootd owns placement policy.
//! - **Boundary:** `job_handle` is local; raw JobId outputs grant no authority.
//! - **Failure:** stale jobs are `ESRCH`, missing rights `EPERM`; root/self kills
//!   remain refused. Descriptor publication follows successful result copyout.
//! - **Lifecycle:** exact fd reservations and new child jobs roll back together.
//! - **Forbidden:** minting authority from an arbitrary PID or raw job identity.
use super::*;
use kernel_object::api::handle::{HandleRights, JobHandleRights};
use rustos_user_abi::syscall::{
    JOB_ABI_VERSION, JOB_OP_CREATE_CHILD, JOB_OP_DUPLICATE_HANDLE, JOB_OP_GET_INFO,
    JOB_OP_KILL, JOB_OP_OPEN_SUBTREE, JOB_OP_PROCESS_JOB, JOB_OP_SET_LIMITS, JOB_STATE_KILLING, JOB_STATE_LIVE, RustosJobArgs,
};

const KILL_PIDS_PER_ROUND: usize = 16;
const KILL_ROUNDS: usize = 8;

pub(super) fn job_error_errno(error: multitask::JobError) -> i64 {
    match error {
        multitask::JobError::Stale => LINUX_ESRCH,
        multitask::JobError::Killing => LINUX_EBUSY,
        multitask::JobError::TooDeep | multitask::JobError::Full => LINUX_EAGAIN,
        multitask::JobError::Root => LINUX_EPERM,
    }
}

fn job_args_are_valid(args: &RustosJobArgs) -> bool {
    // OPEN_SUBTREE alone takes a target id in `job`; for every other op it is
    // an output and must arrive zero.
    let job_field_ok = if args.op == JOB_OP_OPEN_SUBTREE { args.job != 0 } else { args.job == 0 };
    let outputs_zero = args.result_handle == 0
        && job_field_ok
        && args.parent_job == 0
        && args.state == 0
        && args.depth == 0
        && args.processes == 0
        && args.subtree_processes == 0
        && args.children == 0
        && args.kills == 0
        && args.remaining == 0
        && args.effective_processes == 0
        && args.effective_threads == 0
        && args.usage_processes == 0
        && args.usage_threads == 0
        && args.effective_memory_pages == 0
        && args.usage_memory_pages == 0;
    // The limit fields are SET_LIMITS inputs and GET_INFO outputs; for every
    // other op they must arrive zero, and for SET_LIMITS the mask must name
    // only known dimensions.
    let limits_ok = if args.op == JOB_OP_SET_LIMITS {
        multitask::LimitMask::from_bits(args.limit_mask).is_some()
    } else {
        args.limit_mask == 0
            && args.limit_processes == 0
            && args.limit_threads == 0
            && args.limit_memory_pages == 0
    };
    let selector_ok = match args.op {
        JOB_OP_PROCESS_JOB => args.pid != 0,
        JOB_OP_GET_INFO | JOB_OP_CREATE_CHILD | JOB_OP_KILL | JOB_OP_DUPLICATE_HANDLE
        | JOB_OP_OPEN_SUBTREE | JOB_OP_SET_LIMITS => args.pid == 0,
        _ => false,
    };
    args.abi_version == JOB_ABI_VERSION
        && args.flags == 0
        && args.reserved0 == 0
        && args.reserved1 == 0
        && limits_ok
        && (args.rights == 0 || matches!(args.op, JOB_OP_DUPLICATE_HANDLE | JOB_OP_OPEN_SUBTREE))
        && outputs_zero
        && selector_ok
}

fn required_job_right(op: u16) -> Result<JobHandleRights, i64> {
    match op {
        JOB_OP_GET_INFO | JOB_OP_PROCESS_JOB => Ok(JobHandleRights::INSPECT),
        JOB_OP_CREATE_CHILD => Ok(JobHandleRights::CREATE_JOB),
        JOB_OP_KILL => Ok(JobHandleRights::KILL),
        JOB_OP_DUPLICATE_HANDLE | JOB_OP_OPEN_SUBTREE => Ok(JobHandleRights::DUPLICATE),
        JOB_OP_SET_LIMITS => Ok(JobHandleRights::SET_LIMITS),
        _ => Err(LINUX_EINVAL),
    }
}

fn job_authority_from_entry(
    entry: &multitask::HandleEntry,
    op: u16,
) -> Result<(multitask::JobId, JobHandleRights), i64> {
    job_authority_with_right(entry, required_job_right(op)?)
}

pub(super) fn job_authority_with_right(
    entry: &multitask::HandleEntry,
    required: JobHandleRights,
) -> Result<(multitask::JobId, JobHandleRights), i64> {
    let (multitask::KernelHandle::Job(job), HandleRights::Job(rights)) =
        (entry.handle(), entry.rights())
    else {
        return Err(LINUX_EBADF);
    };
    if !rights.contains(required) {
        return Err(LINUX_EPERM);
    }
    Ok((*job, rights))
}

fn attenuated_job_rights(parent: JobHandleRights, mask: u32) -> Result<JobHandleRights, i64> {
    let requested = JobHandleRights::from_bits(mask).ok_or(LINUX_EINVAL)?;
    if !requested.is_subset_of(parent) {
        return Err(LINUX_EPERM);
    }
    Ok(requested)
}

/// One invisible descriptor slot. Copyout failure, admission refusal and a
/// failed publication cancel only this reservation and the unpublished child.
struct PendingJobHandle {
    owner: u64,
    reservation: u64,
    fd: u64,
    target: Option<(multitask::JobId, JobHandleRights)>,
    child: Option<multitask::JobId>,
    committed: bool,
}

impl PendingJobHandle {
    fn reserve(owner: u64) -> Result<Self, i64> {
        let (reservation, slots) = multitask::with_process_state_by_pid_mut(owner, |state| {
            state.handles_mut().reserve_slots(1)
        }).ok_or(LINUX_ESRCH)?.ok_or(LINUX_EMFILE)?;
        Ok(Self { owner, reservation, fd: slots[0], target: None, child: None, committed: false })
    }

    fn commit(&mut self) -> Result<(), i64> {
        let (job, rights) = self.target.ok_or(LINUX_EINVAL)?;
        if multitask::is_user_process_exiting(self.owner) {
            return Err(LINUX_ESRCH);
        }
        let committed = multitask::with_process_state_by_pid_mut(self.owner, |state| {
            state.handles_mut().commit_reserved_job(self.reservation, self.fd, job, rights)
        }).ok_or(LINUX_ESRCH)?;
        if !committed {
            return Err(LINUX_EBUSY);
        }
        self.committed = true;
        self.child = None;
        Ok(())
    }
}

impl Drop for PendingJobHandle {
    fn drop(&mut self) {
        if self.committed { return; }
        let _ = multitask::with_process_state_by_pid_mut(self.owner, |state| {
            state.handles_mut().cancel_reservations(self.reservation, &[self.fd]);
        });
        // Never nest the process state and process table locks.
        if let Some(child) = self.child.take() {
            let _ = multitask::job_kill_begin(child);
        }
    }
}

fn job_state_code(state: multitask::JobState) -> u32 {
    match state {
        multitask::JobState::Live => JOB_STATE_LIVE,
        multitask::JobState::Killing => JOB_STATE_KILLING,
    }
}

/// At most 128 single-finalizer terminations per call; repeat while remaining.
fn kill_job(job: multitask::JobId) -> Result<u64, i64> {
    if let Some(caller) = multitask::current_user_process_id() {
        if multitask::job_subtree_contains_process(job, caller) {
            return Err(LINUX_EPERM);
        }
    }
    multitask::job_kill_begin(job).map_err(job_error_errno)?;
    let mut pids = [0_u64; KILL_PIDS_PER_ROUND];
    for _ in 0..KILL_ROUNDS {
        let count = multitask::job_collect_members(job, &mut pids);
        if count == 0 { return Ok(0); }
        for &pid in &pids[..count] {
            // A job kill is the policy owner acting, so the ring0 memory-pressure
            // watchdog must see it and defer, exactly as for a process kill.
            if terminate_process_once_and_finalize(pid) {
                crate::oom_watchdog::note_rootd_termination();
            }
        }
    }
    let mut probe = [0_u64; KILL_PIDS_PER_ROUND];
    let live = multitask::job_collect_members(job, &mut probe);
    let mut remaining = live as u64;
    if live == probe.len() {
        if let Ok(info) = multitask::job_info(job) {
            remaining = remaining.max(info.subtree_processes);
        }
    }
    Ok(remaining)
}

fn run_job_op(args: &mut RustosJobArgs, caller: u64) -> Result<Option<PendingJobHandle>, i64> {
    let (job, rights) = multitask::with_process_state_by_pid(caller, |state| {
        state.handles().get_entry(args.job_handle).ok_or(LINUX_EBADF)
            .and_then(|entry| job_authority_from_entry(entry, args.op))
    }).ok_or(LINUX_ESRCH)??;
    // The descriptor retains a complete generation, not a job lifetime pin.
    let info = multitask::job_info(job).map_err(job_error_errno)?;
    match args.op {
        JOB_OP_GET_INFO => {
            args.job = job.to_raw();
            args.parent_job = info.parent.map_or(0, multitask::JobId::to_raw);
            args.state = job_state_code(info.state);
            args.depth = u32::from(info.depth);
            args.processes = info.processes;
            args.subtree_processes = info.subtree_processes;
            args.children = info.children;
            args.kills = info.kills;
            args.rights = rights.bits();
            args.limit_processes = info.limits.processes;
            args.limit_threads = info.limits.threads;
            args.effective_processes = info.effective.processes;
            args.effective_threads = info.effective.threads;
            args.usage_processes = info.usage_processes;
            args.usage_threads = info.usage_threads;
            args.limit_memory_pages = info.limits.memory_pages;
            args.effective_memory_pages = info.effective.memory_pages;
            args.usage_memory_pages = info.usage_memory_pages;
        }
        JOB_OP_SET_LIMITS => {
            let mask = multitask::LimitMask::from_bits(args.limit_mask).ok_or(LINUX_EINVAL)?;
            multitask::job_set_limits(
                job,
                mask,
                multitask::JobLimits {
                    processes: args.limit_processes,
                    threads: args.limit_threads,
                    memory_pages: args.limit_memory_pages,
                },
            )
            .map_err(job_error_errno)?;
            // Report the limits now in force, not the request.
            let info = multitask::job_info(job).map_err(job_error_errno)?;
            args.limit_processes = info.limits.processes;
            args.limit_threads = info.limits.threads;
            args.limit_memory_pages = info.limits.memory_pages;
            args.effective_processes = info.effective.processes;
            args.effective_threads = info.effective.threads;
            args.effective_memory_pages = info.effective.memory_pages;
            args.limit_mask = 0;
        }
        JOB_OP_CREATE_CHILD => {
            let mut pending = PendingJobHandle::reserve(caller)?;
            let child = multitask::job_create_child(job).map_err(job_error_errno)?;
            pending.child = Some(child);
            pending.target = Some((child, rights));
            args.result_handle = pending.fd;
            args.job = child.to_raw();
            args.parent_job = job.to_raw();
            args.rights = rights.bits();
            return Ok(Some(pending));
        }
        JOB_OP_DUPLICATE_HANDLE => {
            let requested = attenuated_job_rights(rights, args.rights)?;
            let mut pending = PendingJobHandle::reserve(caller)?;
            pending.target = Some((job, requested));
            args.result_handle = pending.fd;
            args.job = job.to_raw();
            return Ok(Some(pending));
        }
        JOB_OP_OPEN_SUBTREE => {
            let target = multitask::JobId::from_raw(args.job);
            let requested = attenuated_job_rights(rights, args.rights)?;
            // A target outside the subtree, stale, or never existing all answer
            // the same, so a holder learns nothing about jobs it cannot reach.
            if !multitask::job_is_within(job, target) {
                return Err(LINUX_ESRCH);
            }
            let mut pending = PendingJobHandle::reserve(caller)?;
            pending.target = Some((target, requested));
            args.result_handle = pending.fd;
            return Ok(Some(pending));
        }
        JOB_OP_KILL => args.remaining = kill_job(job)?,
        JOB_OP_PROCESS_JOB => {
            if !multitask::job_subtree_contains_process(job, args.pid) {
                // Do not distinguish an absent PID from an out-of-scope one.
                return Err(LINUX_ESRCH);
            }
            // Inspection returns identity only, never a descriptor or authority.
            args.job = multitask::process_job(args.pid).ok_or(LINUX_ESRCH)?.to_raw();
        }
        _ => return Err(LINUX_EINVAL),
    }
    Ok(None)
}

pub(super) fn syscall_linux_rustos_job(args_ptr: u64) -> u64 {
    let mut args = match usermem::read_current_user_struct::<RustosJobArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !job_args_are_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(caller) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_ESRCH);
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(args_ptr, size_of::<RustosJobArgs>()) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    let mut pending = match run_job_op(&mut args, caller) {
        Ok(pending) => pending,
        Err(errno) => return linux_errno(errno),
    };
    if let Err(err) = usermem::write_current_user_struct(args_ptr, &args) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Some(pending) = pending.as_mut()
        && let Err(errno) = pending.commit()
    {
        return linux_errno(errno);
    }
    0
}


/// Session charging is a separate slow syscall. The kernel stores no
/// component names, performs no policy IPC, and retains no user session IDs:
/// the authenticated rootd supervisor keeps the exact-token ledger.
fn job_session_args_valid(args: &rustos_user_abi::syscall::RustosJobSessionArgs) -> bool {
    use rustos_user_abi::syscall::*;
    if args.abi_version != JOB_SESSION_ABI_VERSION
        || args.flags != 0 || args.reserved != 0
        || args.job_handle == 0 || args.effective != 0 || args.usage != 0
    {
        return false;
    }
    match args.op {
        JOB_SESSION_OP_INSPECT => args.client_pid == 0 && args.job_id == 0 && args.limit == 0,
        JOB_SESSION_OP_SET_LIMIT => args.client_pid == 0 && args.job_id == 0,
        JOB_SESSION_OP_CHARGE => args.client_pid != 0 && args.job_id != 0 && args.limit == 0,
        JOB_SESSION_OP_RELEASE => args.client_pid == 0 && args.job_id != 0 && args.limit == 0,
        _ => false,
    }
}

fn execute_job_session(
    args: &mut rustos_user_abi::syscall::RustosJobSessionArgs,
    caller: u64,
) -> Result<(), i64> {
    use rustos_user_abi::syscall::*;
    let required = match args.op {
        JOB_SESSION_OP_INSPECT => JobHandleRights::INSPECT,
        JOB_SESSION_OP_SET_LIMIT => JobHandleRights::SET_LIMITS,
        JOB_SESSION_OP_CHARGE | JOB_SESSION_OP_RELEASE => JobHandleRights::ACCOUNT_SESSIONS,
        _ => return Err(LINUX_EINVAL),
    };
    let (authority, _) = multitask::with_process_state_by_pid(caller, |state| {
        state.handles().get_entry(args.job_handle).ok_or(LINUX_EBADF)
            .and_then(|entry| job_authority_with_right(entry, required))
    }).ok_or(LINUX_ESRCH)??;
    // Only the boot root authority can mutate the accounting of a client
    // whose JobId was discovered through an authenticated ring3 supervisor.
    if matches!(args.op, JOB_SESSION_OP_CHARGE | JOB_SESSION_OP_RELEASE)
        && (authority != multitask::ROOT_JOB
            || args.job_handle != JOB_BOOTSTRAP_ROOT_FD)
    {
        return Err(LINUX_EPERM);
    }
    let info = match args.op {
        JOB_SESSION_OP_INSPECT => multitask::job_session_info(authority),
        JOB_SESSION_OP_SET_LIMIT => multitask::job_set_session_limit(authority, args.limit),
        JOB_SESSION_OP_CHARGE => multitask::job_charge_client_session(
            args.client_pid, multitask::JobId::from_raw(args.job_id)),
        JOB_SESSION_OP_RELEASE => multitask::job_release_client_session(
            multitask::JobId::from_raw(args.job_id)),
        _ => return Err(LINUX_EINVAL),
    }.map_err(job_error_errno)?;
    args.job_id = info.job.to_raw();
    args.limit = info.limit;
    args.effective = info.effective_limit;
    args.usage = info.usage;
    args.client_pid = 0;
    Ok(())
}

pub(super) fn syscall_linux_rustos_job_session(args_ptr: u64) -> u64 {
    use rustos_user_abi::syscall::RustosJobSessionArgs;
    let mut args = match usermem::read_current_user_struct::<RustosJobSessionArgs>(args_ptr) {
        Ok(value) => value,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !job_session_args_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(caller) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_ESRCH);
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(args_ptr, size_of::<RustosJobSessionArgs>()) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = execute_job_session(&mut args, caller) {
        return linux_errno(err);
    }
    if let Err(err) = usermem::write_current_user_struct(args_ptr, &args) {
        // A charge whose output cannot be copied back is observable by
        // rootd only as an error; compensate before returning, so retries
        // cannot double-charge. Release operations require a valid, writable
        // buffer preflight before the irreversible decrement.
        if args.op == rustos_user_abi::syscall::JOB_SESSION_OP_CHARGE {
            let _ = multitask::job_release_client_session(multitask::JobId::from_raw(args.job_id));
        }
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    0
}


/// Memory pressure policy is configured in the Job control path. Hard page
/// admission stays lock-free in kernel-mm; no kernel call to Ring3 happens
/// inside page fault, pager, futex, TLB or scheduler execution paths.
fn job_memory_qos_args_valid(args: &rustos_user_abi::syscall::RustosJobMemoryQosArgs) -> bool {
    use rustos_user_abi::syscall::*;
    if args.abi_version != JOB_MEMORY_QOS_ABI_VERSION
        || args.job_handle == 0
        || args.flags != 0
        || args.reserved != 0
        || args.effective_high_pages != 0
        || args.usage_pages != 0
        || args.high_crossings != 0
    {
        return false;
    }
    match args.op {
        JOB_MEMORY_QOS_OP_INSPECT => args.high_pages == 0 && args.min_pages == 0,
        JOB_MEMORY_QOS_OP_SET => true,
        _ => false,
    }
}

pub(super) fn syscall_linux_rustos_job_memory_qos(ptr: u64) -> u64 {
    use rustos_user_abi::syscall::*;
    let mut args = match usermem::read_current_user_struct::<RustosJobMemoryQosArgs>(ptr) {
        Ok(value) => value,
        Err(error) => return linux_errno(address_space_error_to_linux_errno(error)),
    };
    if !job_memory_qos_args_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(caller) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_ESRCH);
    };
    if let Err(error) =
        usermem::validate_current_user_write_buffer(ptr, size_of::<RustosJobMemoryQosArgs>())
    {
        return linux_errno(address_space_error_to_linux_errno(error));
    }
    let right = if args.op == JOB_MEMORY_QOS_OP_INSPECT {
        JobHandleRights::INSPECT
    } else {
        JobHandleRights::SET_LIMITS
    };
    let authority = multitask::with_process_state_by_pid(caller, |state| {
        state.handles().get_entry(args.job_handle)
            .ok_or(LINUX_EBADF)
            .and_then(|entry| job_authority_with_right(entry, right))
    });
    let (job, _) = match authority {
        Some(Ok(value)) => value,
        Some(Err(errno)) => return linux_errno(errno),
        None => return linux_errno(LINUX_ESRCH),
    };
    let reply = if args.op == JOB_MEMORY_QOS_OP_INSPECT {
        multitask::job_memory_qos(job)
    } else {
        multitask::job_set_memory_qos(job, args.high_pages, args.min_pages)
    };
    let info = match reply {
        Ok(info) => info,
        Err(error) => return linux_errno(job_error_errno(error)),
    };
    args.high_pages = info.high;
    args.min_pages = info.min;
    args.effective_high_pages = info.effective_high;
    args.usage_pages = info.usage;
    args.high_crossings = info.high_events;
    match usermem::write_current_user_struct(ptr, &args) {
        Ok(()) => 0,
        Err(error) => linux_errno(address_space_error_to_linux_errno(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(op: u16) -> RustosJobArgs {
        RustosJobArgs { abi_version: JOB_ABI_VERSION, op, ..RustosJobArgs::default() }
    }

    #[test]
    fn a_job_request_is_exact_version_with_zero_flags_and_zero_outputs() {
        assert!(job_args_are_valid(&request(JOB_OP_GET_INFO)));
        for bad in [
            RustosJobArgs { abi_version: 0, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { abi_version: 1, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { flags: 1, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { reserved0: 1, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { rights: 1, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { result_handle: 1, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { job: 1, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { parent_job: 1, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { pid: 1, ..request(JOB_OP_GET_INFO) },
            request(JOB_OP_PROCESS_JOB),
            request(u16::MAX),
            RustosJobArgs { state: 1, ..request(JOB_OP_GET_INFO) },
            RustosJobArgs { remaining: 1, ..request(JOB_OP_KILL) },
            RustosJobArgs { kills: 1, ..request(JOB_OP_KILL) },
        ] { assert!(!job_args_are_valid(&bad)); }
        assert!(job_args_are_valid(&RustosJobArgs { rights: 1, ..request(JOB_OP_DUPLICATE_HANDLE) }));
    }

    #[test]
    fn job_operations_require_typed_operation_specific_rights() {
        let table = multitask::HandleTable::new();
        assert_eq!(job_authority_from_entry(table.get_entry(0).unwrap(), JOB_OP_GET_INFO), Err(LINUX_EBADF));
        let entry = multitask::HandleEntry::new_with_rights(
            multitask::KernelHandle::Job(multitask::ROOT_JOB),
            HandleRights::Job(JobHandleRights::INSPECT), 0, 0,
        );
        assert_eq!(job_authority_from_entry(&entry, JOB_OP_GET_INFO), Ok((multitask::ROOT_JOB, JobHandleRights::INSPECT)));
        assert!(job_authority_from_entry(&entry, JOB_OP_PROCESS_JOB).is_ok());
        for op in [JOB_OP_CREATE_CHILD, JOB_OP_KILL, JOB_OP_DUPLICATE_HANDLE] {
            assert_eq!(job_authority_from_entry(&entry, op), Err(LINUX_EPERM));
        }
        assert_eq!(job_authority_from_entry(&entry, u16::MAX), Err(LINUX_EINVAL));
    }

    #[test]
    fn job_inspection_and_publication_keep_their_source_boundary_guards() {
        // Source witness, not a native user-copy fault injection test.
        let source = include_str!("job_ops.rs").split("#[cfg(test)]").next().unwrap();
        let inspect = source.split_once("JOB_OP_PROCESS_JOB => {").unwrap().1;
        let guard = inspect.find("if !multitask::job_subtree_contains_process(job, args.pid)").unwrap();
        assert!(guard < inspect.find("multitask::process_job(args.pid)").unwrap());
        let syscall = source.split_once("pub(super) fn syscall_linux_rustos_job(").unwrap().1;
        let copyout = syscall.find("usermem::write_current_user_struct(args_ptr, &args)").unwrap();
        assert!(syscall.find("run_job_op(&mut args, caller)").unwrap() < copyout);
        assert!(copyout < syscall.find("pending.commit()").unwrap());
        let commit = source.split_once("fn commit(&mut self)").unwrap().1.split_once("impl Drop").unwrap().0;
        assert!(commit.contains("self.committed = true;"));
        assert!(commit.contains("self.child = None;"));
        let rollback = source.split_once("impl Drop for PendingJobHandle").unwrap().1
            .split_once("fn job_state_code").unwrap().0;
        assert!(rollback.find("cancel_reservations(self.reservation").unwrap()
            < rollback.find("multitask::job_kill_begin(child)").unwrap());
    }

    #[test]
    fn opening_a_subtree_job_names_a_target_and_needs_the_duplicate_right() {
        let open = RustosJobArgs { job: 7, rights: 1, ..request(JOB_OP_OPEN_SUBTREE) };
        assert!(job_args_are_valid(&open));
        assert!(!job_args_are_valid(&RustosJobArgs { job: 0, ..open }), "a target id is required");
        assert!(!job_args_are_valid(&RustosJobArgs { pid: 1, ..open }));
        // The target id is an input only for this op; every other op still
        // rejects a non-zero `job` as a stale output.
        assert!(!job_args_are_valid(&RustosJobArgs { job: 7, ..request(JOB_OP_KILL) }));
        assert_eq!(required_job_right(JOB_OP_OPEN_SUBTREE), Ok(JobHandleRights::DUPLICATE));
    }

    #[test]
    fn opening_a_subtree_job_checks_scope_before_reserving_and_never_widens_rights() {
        let source = include_str!("job_ops.rs").split("#[cfg(test)]").next().unwrap();
        let open = source.split_once("JOB_OP_OPEN_SUBTREE => {").unwrap().1
            .split_once("JOB_OP_KILL =>").unwrap().0;
        let attenuate = open.find("attenuated_job_rights(rights, args.rights)?").unwrap();
        let scope = open.find("multitask::job_is_within(job, target)").unwrap();
        let reserve = open.find("PendingJobHandle::reserve(caller)?").unwrap();
        assert!(attenuate < reserve && scope < reserve);
        assert!(open.contains("Err(LINUX_ESRCH)"), "out-of-scope targets must not be distinguishable");
    }

    #[test]
    fn set_limits_needs_the_set_limits_right_and_a_known_nonempty_mask() {
        assert_eq!(required_job_right(JOB_OP_SET_LIMITS), Ok(JobHandleRights::SET_LIMITS));
        let set = RustosJobArgs { limit_mask: 1, limit_processes: 4, ..request(JOB_OP_SET_LIMITS) };
        assert!(job_args_are_valid(&set));
        assert!(job_args_are_valid(&RustosJobArgs { limit_mask: 3, ..set }));
        assert!(job_args_are_valid(&RustosJobArgs { limit_mask: 4, limit_memory_pages: 9, ..set }));
        for bad in [
            RustosJobArgs { limit_mask: 0, ..set },
            RustosJobArgs { limit_mask: 1 << 3, ..set },
            RustosJobArgs { limit_mask: 1 << 31, ..set },
            RustosJobArgs { reserved1: 1, ..set },
            RustosJobArgs { effective_processes: 1, ..set },
            RustosJobArgs { usage_threads: 1, ..set },
            RustosJobArgs { effective_memory_pages: 1, ..set },
            RustosJobArgs { usage_memory_pages: 1, ..set },
            RustosJobArgs { pid: 1, ..set },
            RustosJobArgs { rights: 1, ..set },
        ] {
            assert!(!job_args_are_valid(&bad));
        }
    }

    #[test]
    fn limit_fields_are_zero_inputs_for_every_op_except_set_limits() {
        for op in [JOB_OP_GET_INFO, JOB_OP_CREATE_CHILD, JOB_OP_KILL, JOB_OP_DUPLICATE_HANDLE] {
            assert!(job_args_are_valid(&request(op)) || op == JOB_OP_DUPLICATE_HANDLE);
            for bad in [
                RustosJobArgs { limit_mask: 1, ..request(op) },
                RustosJobArgs { limit_processes: 1, ..request(op) },
                RustosJobArgs { limit_threads: 1, ..request(op) },
                RustosJobArgs { limit_memory_pages: 1, ..request(op) },
            ] {
                assert!(!job_args_are_valid(&bad), "op {op} must not accept limit inputs");
            }
        }
    }

    #[test]
    fn set_limits_reports_the_limits_in_force_after_validating_the_mask() {
        let source = include_str!("job_ops.rs").split("#[cfg(test)]").next().unwrap();
        let set = source.split_once("JOB_OP_SET_LIMITS => {").unwrap().1
            .split_once("JOB_OP_CREATE_CHILD =>").unwrap().0;
        let mask = set.find("LimitMask::from_bits(args.limit_mask)").unwrap();
        let apply = set.find("multitask::job_set_limits(").unwrap();
        let report = set.find("let info = multitask::job_info(job)").unwrap();
        assert!(mask < apply && apply < report, "validate, apply, then report what is now in force");
    }

    #[test]
    fn job_handle_duplication_cannot_widen_or_accept_unknown_rights() {
        let rights = JobHandleRights::INSPECT.union(JobHandleRights::DUPLICATE);
        assert_eq!(attenuated_job_rights(rights, JobHandleRights::INSPECT.bits()), Ok(JobHandleRights::INSPECT));
        assert_eq!(attenuated_job_rights(rights, JobHandleRights::KILL.bits()), Err(LINUX_EPERM));
        assert_eq!(attenuated_job_rights(rights, 1 << 10), Err(LINUX_EINVAL));
    }

    #[test]
    fn job_errors_map_to_distinct_errnos() {
        assert_eq!(job_error_errno(multitask::JobError::Stale), LINUX_ESRCH);
        assert_eq!(job_error_errno(multitask::JobError::Killing), LINUX_EBUSY);
        assert_eq!(job_error_errno(multitask::JobError::Full), LINUX_EAGAIN);
        assert_eq!(job_error_errno(multitask::JobError::Root), LINUX_EPERM);
    }

    #[test]
    fn job_states_have_stable_wire_codes() {
        assert_eq!(job_state_code(multitask::JobState::Live), JOB_STATE_LIVE);
        assert_eq!(job_state_code(multitask::JobState::Killing), JOB_STATE_KILLING);
    }

    #[test]
    fn the_root_job_wire_id_matches_the_kernel_root() {
        assert_eq!(multitask::ROOT_JOB.to_raw(), rustos_user_abi::syscall::JOB_ID_ROOT);
    }
}
// RING3-MIGRATION-REFERENCE END: rootd-owned job broker.
