//! Typed client of the handle-authorized kernel job tree.
//!
//! - **Owner:** rootd holds the boot root descriptor and chooses placement/kills.
//! - **Boundary:** input descriptors and output JobIds are never interchangeable.
//! - **Lifecycle:** temporary duplicates and self-check children are closed.
//! - **Failure:** positive Linux errno; the boot self-check reports its first step.
//! - **Evidence:** argument-shaping tests and `rootd: job tree ok` at boot.
use super::{syscall1, syscall2};
use rustos_user_abi::syscall::{
    JOB_ABI_VERSION, JOB_BOOTSTRAP_ROOT_FD, JOB_ID_ROOT, JOB_OP_CREATE_CHILD,
    JOB_OP_DUPLICATE_HANDLE, JOB_OP_GET_INFO, JOB_OP_KILL, JOB_OP_OPEN_SUBTREE, JOB_OP_PROCESS_JOB, JOB_OP_SET_LIMITS, JOB_LIMIT_MEMORY_PAGES, JOB_LIMIT_PROCESSES, JOB_LIMIT_THREADS, JOB_LIMIT_UNSET,
    JOB_RIGHT_CREATE_JOB, JOB_RIGHT_CREATE_PROCESS, JOB_RIGHT_DUPLICATE, JOB_RIGHT_INSPECT,
    JOB_RIGHT_TRANSFER, IpcCallWithHandlesArgs, LoaderSpawnRequest, LoaderSpawnResponse,
    RustosJobArgs, SYS_RUSTOS_IPC_CALL_WITH_HANDLES, SYS_RUSTOS_JOB,
};

fn call(mut args: RustosJobArgs) -> Result<RustosJobArgs, i64> {
    let status = syscall1(SYS_RUSTOS_JOB, (&mut args as *mut RustosJobArgs) as u64);
    if status < 0 { Err(-status) } else { Ok(args) }
}

fn request(op: u16, job_handle: u64, pid: u64) -> RustosJobArgs {
    RustosJobArgs { abi_version: JOB_ABI_VERSION, op, job_handle, pid, ..RustosJobArgs::default() }
}

pub(super) struct Info {
    pub(super) id: u64,
    pub(super) parent: u64,
    pub(super) state: u32,
    pub(super) depth: u32,
    pub(super) subtree_processes: u64,
    pub(super) usage_processes: u64,
    pub(super) children: u64,
    pub(super) rights: u32,
    pub(super) limit_processes: u64,
    pub(super) limit_threads: u64,
    pub(super) effective_processes: u64,
    pub(super) effective_threads: u64,
    pub(super) limit_memory_pages: u64,
    pub(super) effective_memory_pages: u64,
    pub(super) usage_memory_pages: u64,
    pub(super) usage_threads: u64,
}

pub(super) fn info(handle: u64) -> Result<Info, i64> {
    call(request(JOB_OP_GET_INFO, handle, 0)).map(info_from_reply)
}

pub(super) fn info_from_reply(reply: RustosJobArgs) -> Info {
    Info {
        id: reply.job, parent: reply.parent_job, state: reply.state, depth: reply.depth,
        subtree_processes: reply.subtree_processes, usage_processes: reply.usage_processes,
        children: reply.children, rights: reply.rights,
        limit_processes: reply.limit_processes, limit_threads: reply.limit_threads,
        effective_processes: reply.effective_processes, effective_threads: reply.effective_threads,
        limit_memory_pages: reply.limit_memory_pages,
        effective_memory_pages: reply.effective_memory_pages,
        usage_memory_pages: reply.usage_memory_pages,
        usage_threads: reply.usage_threads,
    }
}

/// Sets the process and/or thread limit of the job behind `handle`
/// (`JOB_LIMIT_UNSET` inherits). Returns the limits now in force.
pub(super) fn set_limits(
    handle: u64,
    mask: u32,
    processes: u64,
    threads: u64,
    memory_pages: u64,
) -> Result<(u64, u64, u64), i64> {
    let mut args = request(JOB_OP_SET_LIMITS, handle, 0);
    args.limit_mask = mask;
    args.limit_processes = processes;
    args.limit_threads = threads;
    args.limit_memory_pages = memory_pages;
    let reply = call(args)?;
    Ok((reply.effective_processes, reply.effective_threads, reply.effective_memory_pages))
}

pub(super) fn create_child(parent: u64) -> Result<u64, i64> {
    Ok(call(request(JOB_OP_CREATE_CHILD, parent, 0))?.result_handle)
}

pub(super) fn duplicate(handle: u64, rights: u32) -> Result<u64, i64> {
    let mut args = request(JOB_OP_DUPLICATE_HANDLE, handle, 0);
    args.rights = rights;
    Ok(call(args)?.result_handle)
}

/// Returns members remaining after the bounded step; repeat until zero.
pub(super) fn kill(handle: u64) -> Result<u64, i64> {
    Ok(call(request(JOB_OP_KILL, handle, 0))?.remaining)
}

pub(super) fn close(handle: u64) {
    const SYS_CLOSE: u64 = 3;
    let _ = syscall1(SYS_CLOSE, handle);
}

pub(super) fn bootstrap_args(
    path: &'static [u8],
    weight_micros: u64,
    job: u64,
) -> rustos_user_abi::syscall::RustosBootstrapSpawnArgs {
    use rustos_user_abi::syscall::{BOOTSTRAP_SPAWN_ABI_VERSION, BOOTSTRAP_SPAWN_FLAG_LOGICAL_ADMIN, RustosBootstrapSpawnArgs};
    RustosBootstrapSpawnArgs {
        abi_version: BOOTSTRAP_SPAWN_ABI_VERSION,
        flags: BOOTSTRAP_SPAWN_FLAG_LOGICAL_ADMIN,
        path_ptr: path.as_ptr() as u64,
        weight_micros,
        job_handle: job,
        ..RustosBootstrapSpawnArgs::default()
    }
}

fn loader_call_args(
    endpoint: u64,
    request: &LoaderSpawnRequest,
    response: &mut LoaderSpawnResponse,
    descriptor: &i32,
    received_count: &mut u16,
) -> IpcCallWithHandlesArgs {
    IpcCallWithHandlesArgs {
        endpoint,
        request_ptr: request as *const _ as u64,
        request_len: core::mem::size_of::<LoaderSpawnRequest>() as u64,
        reply_ptr: response as *mut _ as u64,
        reply_capacity: core::mem::size_of::<LoaderSpawnResponse>() as u64,
        send_fds_ptr: descriptor as *const _ as u64,
        send_fd_count: 1,
        recv_fd_count_ptr: received_count as *mut _ as u64,
        ..IpcCallWithHandlesArgs::default()
    }
}

/// Lend only process-creation/transfer authority to loaderd. Its receive path
/// authenticates this process independently of the transferred descriptor.
/// The supervisor's original descriptor retains inspect/kill authority.
pub(super) fn call_loader(
    endpoint: u64,
    job: u64,
    request: &LoaderSpawnRequest,
    response: &mut LoaderSpawnResponse,
) -> Result<usize, i64> {
    let borrowed = duplicate(job, JOB_RIGHT_CREATE_PROCESS | JOB_RIGHT_TRANSFER)?;
    let descriptor = match i32::try_from(borrowed) {
        Ok(fd) => fd,
        Err(_) => { close(borrowed); return Err(75); }
    };
    let mut received_count = 0_u16;
    let args = loader_call_args(endpoint, request, response, &descriptor, &mut received_count);
    // All argument/byte/fd buffers stay live and disjoint for the blocking call.
    let result = syscall1(SYS_RUSTOS_IPC_CALL_WITH_HANDLES, &args as *const _ as u64);
    close(borrowed);
    if result < 0 { return Err(-result); }
    if received_count != 0 { return Err(22); }
    Ok(result as usize)
}

/// Mints an attenuated handle to the job `job` (an id from the census) under
/// `authority`. Fails with ESRCH unless `job` lies within `authority`'s subtree.
pub(super) fn open_subtree(authority: u64, job: u64, rights: u32) -> Result<u64, i64> {
    let mut args = request(JOB_OP_OPEN_SUBTREE, authority, 0);
    args.job = job;
    args.rights = rights;
    Ok(call(args)?.result_handle)
}

/// The job of `pid` if it lies within `handle`'s subtree; ESRCH otherwise.
pub(super) fn process_job_in(handle: u64, pid: u64) -> Result<u64, i64> {
    Ok(call(request(JOB_OP_PROCESS_JOB, handle, pid))?.job)
}

pub(super) fn process_job(pid: u64) -> Result<u64, i64> {
    Ok(call(request(JOB_OP_PROCESS_JOB, JOB_BOOTSTRAP_ROOT_FD, pid))?.job)
}


/// Admission/refund is deliberately outside the IPC fast path. Only rootd's
/// boot descriptor retains ACCOUNT_SESSIONS; the loader receives attenuated
/// process creation rights and cannot underbill another Job.
pub(super) fn charge_client_session(
    pid: u64, job_id: u64, provider_pid: u64, token: u64,
) -> Result<u64, i64> {
    use rustos_user_abi::syscall::*;
    let mut args = RustosJobSessionArgs {
        abi_version: JOB_SESSION_ABI_VERSION,
        op: JOB_SESSION_OP_CHARGE,
        job_handle: JOB_BOOTSTRAP_ROOT_FD,
        client_pid: pid,
        job_id,
        provider_pid,
        token,
        ..RustosJobSessionArgs::default()
    };
    let status = syscall1(SYS_RUSTOS_JOB_SESSION, (&mut args as *mut RustosJobSessionArgs) as u64);
    if status < 0 { Err(-status) } else { Ok(args.usage) }
}

pub(super) fn release_client_session(
    job_id: u64, provider_pid: u64, token: u64,
) -> Result<u64, i64> {
    use rustos_user_abi::syscall::*;
    let mut args = RustosJobSessionArgs {
        abi_version: JOB_SESSION_ABI_VERSION,
        op: JOB_SESSION_OP_RELEASE,
        job_handle: JOB_BOOTSTRAP_ROOT_FD,
        job_id,
        provider_pid,
        token,
        ..RustosJobSessionArgs::default()
    };
    let status = syscall1(SYS_RUSTOS_JOB_SESSION, (&mut args as *mut RustosJobSessionArgs) as u64);
    if status < 0 { Err(-status) } else { Ok(args.usage) }
}

/// Rootd startup or an authenticated provider exit can reconcile any kernel
/// token left alive after losing the previous rootd process's user ledger.
pub(super) fn drain_client_sessions(provider_pid: u64) -> Result<u64, i64> {
    use rustos_user_abi::syscall::*;
    let mut args = RustosJobSessionArgs {
        abi_version: JOB_SESSION_ABI_VERSION,
        op: JOB_SESSION_OP_DRAIN,
        job_handle: JOB_BOOTSTRAP_ROOT_FD,
        provider_pid,
        ..RustosJobSessionArgs::default()
    };
    let status = syscall1(SYS_RUSTOS_JOB_SESSION, &mut args as *mut _ as u64);
    if status < 0 { Err(-status) } else { Ok(args.usage) }
}

/// Inspect the live kernel memory pressure and OOM protection state using an
/// attenuated Job inspect handle. Does not confer permission to modify it.
pub(super) fn memory_qos_info(handle: u64)
    -> Result<rustos_user_abi::syscall::RustosJobMemoryQosArgs, i64>
{
    use rustos_user_abi::syscall::*;
    let mut args = RustosJobMemoryQosArgs {
        abi_version: JOB_MEMORY_QOS_ABI_VERSION,
        op: JOB_MEMORY_QOS_OP_INSPECT,
        job_handle: handle,
        ..RustosJobMemoryQosArgs::default()
    };
    let status = syscall1(SYS_RUSTOS_JOB_MEMORY_QOS, &mut args as *mut _ as u64);
    if status < 0 { Err(-status) } else { Ok(args) }
}

/// Live evidence that page charging follows the fault and unmap paths: touching
/// fresh pages raises the root job's charged pages and unmapping returns them.
/// Other processes run concurrently, so the check is deliberately loose (half
/// the pages), and a failure is reported with the three readings rather than
/// treated as a fault. Returns `Err((before, during, after))` on failure.
pub(super) fn charge_probe() -> Result<(), (u64, u64, u64)> {
    const PAGES: usize = 32;
    const PAGE: usize = 4096;
    const SYS_MUNMAP: u64 = 11;
    let usage = || info(JOB_BOOTSTRAP_ROOT_FD).map_or(0, |root| root.usage_memory_pages);
    let before = usage();
    // SAFETY: an anonymous private mapping of fresh address space; no existing
    // memory is aliased and the result is checked before use.
    let base = unsafe { rustos_svc_runtime::syscall::mmap_anonymous(PAGES * PAGE) };
    if base < 0 {
        return Err((before, 0, 0));
    }
    for page in 0..PAGES {
        // SAFETY: `base` maps PAGES * PAGE writable bytes owned by this probe
        // until the munmap below; each write stays inside one mapped page.
        unsafe { core::ptr::write_volatile((base as usize + page * PAGE) as *mut u8, 1) };
    }
    let during = usage();
    let _ = syscall2(SYS_MUNMAP, base as u64, (PAGES * PAGE) as u64);
    let after = usage();
    let half = (PAGES / 2) as u64;
    if during >= before.saturating_add(half) && during >= after.saturating_add(half) {
        Ok(())
    } else {
        Err((before, during, after))
    }
}

/// The probe child is a second copy of this binary started in a limited job. It
/// is told apart from the real supervisor by what it lacks: the bootstrap root
/// descriptor is never inherited, so the real `rootd` can always inspect it.
pub(super) fn is_limit_probe_child() -> bool {
    info(JOB_BOOTSTRAP_ROOT_FD).is_err()
}

/// The child's whole job: touch fresh pages until the job's memory limit
/// refuses one, which ends the process with a page fault. Reaching the end of
/// the mapping means the limit did not hold; it exits cleanly so the parent
/// reads the peak instead of waiting.
pub(super) fn limit_probe_child() -> ! {
    const PAGES: usize = 32 * 1024; // 128 MiB of address space, far above the limit
    const PAGE: usize = 4096;
    // SAFETY: an anonymous private mapping of fresh address space; the result is
    // checked before use.
    let base = unsafe { rustos_svc_runtime::syscall::mmap_anonymous(PAGES * PAGE) };
    if base > 0 {
        for page in 0..PAGES {
            // SAFETY: `base` maps PAGES * PAGE writable bytes owned by this
            // process; each write stays inside one mapped page. A refused page
            // faults here and the process dies, which is the expected end.
            unsafe { core::ptr::write_volatile((base as usize + page * PAGE) as *mut u8, 1) };
        }
    }
    const SYS_EXIT: u64 = 60;
    let _ = syscall1(SYS_EXIT, 0);
    loop {}
}

/// Starts the probe child: this image, unprivileged, in `job`. The kernel admits
/// a second copy of this path only without logical admin.
fn spawn_unprivileged_probe(job: u64) -> Result<u64, i64> {
    let path: &'static [u8] = b"services/rootd/rootd.elf\0";
    let expected = info(job)?.id;
    let requester = syscall1(super::SYS_GETPID, 0);
    if requester <= 0 {
        return Err(5);
    }
    super::register_scheduling_context_authority(
        super::trim_nul(path),
        requester as u64,
        super::scheduling_context_policy_for_exec(path),
    )
    .map_err(i64::from)?;
    let mut args = bootstrap_args(path, 100, job);
    args.flags = 0;
    let pid = syscall1(rustos_user_abi::syscall::SYS_RUSTOS_SPAWN_EXEC, (&args as *const _) as u64);
    if pid < 0 {
        return Err(-pid);
    }
    if process_job(pid as u64) != Ok(expected) {
        let _ = kill(job);
        return Err(5);
    }
    Ok(pid as u64)
}

/// Live evidence that a job memory limit actually refuses pages. Spawns the
/// probe child into a fresh job limited to `LIMIT_PAGES` and watches the job:
/// the child must die short of its mapping with the job's peak charge within
/// slack of the limit, and the job must drain to zero and release afterwards.
/// Returns `Ok((peak, limit))`, or `Err((code, peak, final_usage))`.
pub(super) fn limit_probe() -> Result<(u64, u64), (u32, u64, u64)> {
    // The child image is charged at publication and is not refused: rootd's
    // 8 MiB static heap alone is 2048 pages. The limit must sit above that and
    // far below the child's 128 MiB mapping.
    const LIMIT_PAGES: u64 = 12 * 1024; // 48 MiB
    const SLACK_PAGES: u64 = 256;
    const NEAR_LIMIT_PAGES: u64 = 512;
    const SYS_SCHED_YIELD: u64 = 24;
    let Ok(job) = create_child(JOB_BOOTSTRAP_ROOT_FD) else { return Err((1, 0, 0)) };
    let result = (|| {
        if set_limits(job, JOB_LIMIT_MEMORY_PAGES, JOB_LIMIT_UNSET, JOB_LIMIT_UNSET, LIMIT_PAGES).is_err() {
            return Err((2, 0, 0));
        }
        if let Err(errno) = spawn_unprivileged_probe(job) {
            return Err((3, errno as u64, 0));
        }
        let start = rustos_svc_runtime::syscall::monotonic_nanos();
        let mut peak = 0_u64;
        let mut first = 0_u64;
        loop {
            let Ok(state) = info(job) else { return Err((4, peak, 0)) };
            if first == 0 {
                first = state.usage_memory_pages;
            }
            peak = peak.max(state.usage_memory_pages);
            if state.subtree_processes == 0 {
                break;
            }
            if rustos_svc_runtime::syscall::monotonic_nanos().saturating_sub(start) > 4_000_000_000 {
                return Err((5, peak, state.usage_memory_pages));
            }
            let _ = syscall1(SYS_SCHED_YIELD, 0);
        }
        if peak > LIMIT_PAGES + SLACK_PAGES {
            return Err((6, peak, first));
        }
        if peak + NEAR_LIMIT_PAGES < LIMIT_PAGES {
            return Err((7, peak, first));
        }
        Ok((peak, LIMIT_PAGES))
    })();
    // Retire the job; its slot is released once the child's pages have drained.
    let mut drained = false;
    for _ in 0..2000 {
        let _ = kill(job);
        match info(job) {
            Err(_) => {
                drained = true;
                break;
            }
            Ok(_) => {
                let _ = syscall1(SYS_SCHED_YIELD, 0);
            }
        }
    }
    close(job);
    match result {
        Ok(pair) if drained => Ok(pair),
        Ok((peak, _)) => Err((8, peak, 0)),
        Err(error) => Err(error),
    }
}

/// Checks boot delivery, identity/authority separation and live attenuation.
pub(super) fn selfcheck(own_pid: u64) -> u32 {
    if process_job(own_pid) != Ok(JOB_ID_ROOT) { return 1; }
    match info(JOB_BOOTSTRAP_ROOT_FD) {
        Ok(root) if root.id == JOB_ID_ROOT
            && root.rights & (JOB_RIGHT_CREATE_JOB | JOB_RIGHT_DUPLICATE)
                == (JOB_RIGHT_CREATE_JOB | JOB_RIGHT_DUPLICATE) => {}
        _ => return 2,
    }
    // Charging is live: every process is bound to a job, so the root subtree
    // holds at least rootd's own resident pages.
    match info(JOB_BOOTSTRAP_ROOT_FD) {
        Ok(root) if root.usage_memory_pages > 0 && root.effective_memory_pages == JOB_LIMIT_UNSET => {}
        _ => return 14,
    }
    let Ok(child) = create_child(JOB_BOOTSTRAP_ROOT_FD) else { return 3 };
    let result = (|| {
        let id = match info(child) {
            Ok(info) if info.parent == JOB_ID_ROOT && info.depth == 1 && info.children == 0 => info.id,
            _ => return 4,
        };
        let Ok(inspect) = duplicate(child, JOB_RIGHT_INSPECT) else { return 5 };
        let attenuated = info(inspect).is_ok_and(|info| info.id == id && info.rights == JOB_RIGHT_INSPECT)
            && kill(inspect) == Err(1)
            && create_child(inspect) == Err(1)
            && duplicate(inspect, JOB_RIGHT_INSPECT) == Err(1)
            && call(request(JOB_OP_PROCESS_JOB, inspect, own_pid)) == Err(3);
        close(inspect);
        if !attenuated { return 6; }
        // Limits: a child starts unlimited, a clamp by the parent shows in the
        // effective limit, and a handle without SET_LIMITS cannot change them.
        match info(child) {
            Ok(info) if info.limit_processes == JOB_LIMIT_UNSET && info.limit_threads == JOB_LIMIT_UNSET => {}
            _ => return 9,
        }
        let all = JOB_LIMIT_PROCESSES | JOB_LIMIT_THREADS | JOB_LIMIT_MEMORY_PAGES;
        if set_limits(child, all, 4, 8, 4096) != Ok((4, 8, 4096)) { return 10; }
        match info(child) {
            Ok(info) if info.limit_processes == 4 && info.effective_processes == 4
                && info.limit_memory_pages == 4096 && info.effective_memory_pages == 4096
                && info.usage_memory_pages == 0 => {}
            _ => return 11,
        }
        let Ok(inspect_only) = duplicate(child, JOB_RIGHT_INSPECT) else { return 12 };
        let denied = set_limits(inspect_only, JOB_LIMIT_MEMORY_PAGES, JOB_LIMIT_UNSET, JOB_LIMIT_UNSET, 1) == Err(1);
        close(inspect_only);
        if !denied { return 13; }
        if kill(child) != Ok(0) { return 7; }
        if info(child).is_ok() { return 8; }
        0
    })();
    let _ = kill(child); // rollback on a failed check; stale after success
    close(child);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_placement_names_the_caller_local_job_in_a_versioned_block() {
        static PATH: &[u8] = b"/services/loaderd/loaderd.elf\0";
        let args = bootstrap_args(PATH, 1000, 9);
        assert!(rustos_user_abi::syscall::bootstrap_spawn_shape_valid(&args));
        assert_eq!((args.job_handle, args.weight_micros, args.console_session), (9, 1000, 0));
        assert_eq!(args.path_ptr, PATH.as_ptr() as u64);
        assert_eq!(args.flags, rustos_user_abi::syscall::BOOTSTRAP_SPAWN_FLAG_LOGICAL_ADMIN);
    }

    #[test]
    fn loader_placement_sends_one_descriptor_without_encoding_a_foreign_fd() {
        let request = LoaderSpawnRequest::default();
        let mut response = LoaderSpawnResponse::default();
        let descriptor = 9_i32;
        let mut count = 0_u16;
        let args = loader_call_args(7, &request, &mut response, &descriptor, &mut count);
        assert_eq!((args.endpoint, args.send_fd_count, args.recv_fd_capacity, args.reserved0), (7, 1, 0, 0));
        assert_eq!(args.send_fds_ptr, &descriptor as *const _ as u64);
        assert_eq!(args.request_ptr, &request as *const _ as u64);
        assert_eq!(args.reply_ptr, &mut response as *mut _ as u64);
        assert_eq!(args.recv_fd_count_ptr, &mut count as *mut _ as u64);
        assert_eq!(args.recv_fds_ptr, 0);
        let source = include_str!("jobs.rs").split("#[cfg(test)]").next().unwrap();
        let lend = source.split_once("pub(super) fn call_loader(").unwrap().1;
        assert!(lend.contains("duplicate(job, JOB_RIGHT_CREATE_PROCESS | JOB_RIGHT_TRANSFER)"));
        assert!(lend.find("SYS_RUSTOS_IPC_CALL_WITH_HANDLES, &args").unwrap()
            < lend.rfind("close(borrowed)").unwrap());
    }

    #[test]
    fn info_preserves_job_identity_object_thread_and_page_accounting() {
        let reply = RustosJobArgs {
            job: 80, subtree_processes: 1, usage_processes: 2, usage_threads: 5,
            usage_memory_pages: 32_768, limit_memory_pages: 32_768,
            effective_memory_pages: 32_768, children: 2,
            ..RustosJobArgs::default()
        };
        let info = info_from_reply(reply);
        assert_eq!((info.id, info.subtree_processes, info.usage_processes), (80, 1, 2));
        assert_eq!((info.usage_threads, info.children), (5, 2));
        assert_eq!((info.usage_memory_pages, info.limit_memory_pages, info.effective_memory_pages), (32_768, 32_768, 32_768));
    }

    #[test]
    fn requests_carry_only_their_inputs() {
        let args = request(JOB_OP_PROCESS_JOB, JOB_BOOTSTRAP_ROOT_FD, 7);
        assert_eq!((args.abi_version, args.op, args.job_handle, args.pid),
            (JOB_ABI_VERSION, JOB_OP_PROCESS_JOB, JOB_BOOTSTRAP_ROOT_FD, 7));
        assert_eq!((args.job, args.parent_job, args.result_handle, args.remaining, args.flags, args.rights),
            (0, 0, 0, 0, 0, 0));
    }
}
