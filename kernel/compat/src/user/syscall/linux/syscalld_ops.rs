//! Linux syscall frame translation across the kernel-compat/syscalld boundary.
//!
//! - **Owner:** kernel-compat owns user-copy and narrow mechanism; syscalld
//!   owns Linux policy decisions.
//! - **Boundary:** user pointers/arguments, service replies, kernel-stamped
//!   caller/target identities, and CPU topology cross trust owners.
//! - **Lifecycle:** validate/copy input → stamp immutable authority → call
//!   syscalld → admit exact reply → revalidate target → copy or commit.
//! - **Concurrency:** user memory retains the current process generation and
//!   affinity commits serialize with scheduler dispatch.
//! - **Failure:** pointer, ABI, identity, mask, timeout, peer, and malformed
//!   reply failures return Linux errno without partial mutation.
//! - **Forbidden:** no service-fabricated topology, raw APIC mask, cross-process
//!   affinity shortcut, unchecked copy, or success fallback.
//! - **Evidence:** `cpu-affinity-observation`, `task-affinity-lifecycle`, and
//!   the Linux ABI differential/source-conformance tests.

use super::*;
use rustos_user_abi::syscall::{
    CPU_TOPOLOGY_MAX_LOGICAL_CPUS, CPU_TOPOLOGY_OBSERVATION_ABI_VERSION,
    LinuxSyscallOffloadFastMemRequest, LinuxSyscallOffloadFastRequest,
    LinuxSyscallOffloadFastResponse,
};

pub(crate) fn call_syscalld_raw(request: &[u8]) -> Result<Vec<u8>, i64> {
    ipc_ops::call_linux_syscall_endpoint(request)
}

fn write_uts_field(dst: &mut [u8; 65], value: &[u8]) {
    dst.fill(0);
    let len = value.len().min(dst.len() - 1);
    dst[..len].copy_from_slice(&value[..len]);
}

fn local_utsname() -> rustos_user_abi::syscall::LinuxUtsName {
    let mut uts = rustos_user_abi::syscall::LinuxUtsName::default();
    write_uts_field(&mut uts.sysname, b"RustOS");
    write_uts_field(&mut uts.nodename, b"rustos");
    write_uts_field(&mut uts.release, b"6.6.0-rustos");
    write_uts_field(&mut uts.version, b"RustOS 0.1");
    write_uts_field(&mut uts.machine, b"x86_64");
    write_uts_field(&mut uts.domainname, b"localdomain");
    uts
}

pub(super) fn syscall_linux_syscalld_uname(buf_ptr: u64) -> u64 {
    linux_unit_result((|| {
        validate_user_write(buf_ptr, LINUX_UTSNAME_SIZE)?;
        let uts = local_utsname();
        // SAFETY: `LinuxUtsName` is `#[repr(C)]` plain byte arrays of exact size
        // `LINUX_UTSNAME_SIZE` with no padding or uninitialized bytes.
        write_user_bytes(buf_ptr, wire_as_bytes(&uts))
    })())
}

pub(super) fn syscall_linux_syscalld_prlimit64(
    pid: u64,
    resource: u64,
    new_limit_ptr: u64,
    old_limit_ptr: u64,
) -> u64 {
    linux_unit_result((|| {
        let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64);
        request.dirfd = pid;
        request.flags = resource;
        if new_limit_ptr != 0 {
            let mut new_limit = [0_u8; LINUX_RLIMIT_SIZE];
            read_user_bytes(new_limit_ptr, &mut new_limit)?;
            request.mask |= 0x1;
            request.path_len = LINUX_RLIMIT_SIZE as u32;
            request.path[..LINUX_RLIMIT_SIZE].copy_from_slice(&new_limit);
        }
        if old_limit_ptr != 0 {
            validate_user_write(old_limit_ptr, LINUX_RLIMIT_SIZE)?;
            request.mask |= 0x2;
        }
        let response = call_syscalld(request)?;
        if old_limit_ptr != 0 {
            copy_exact_payload_to_user(
                old_limit_ptr,
                &response.payload,
                response.payload_len,
                LINUX_RLIMIT_SIZE,
            )
        } else {
            ensure_empty_syscalld_response(&response)
        }
    })())
}

/// `(soft, hard)` of `resource` for the calling process, read from the
/// authoritative store in syscalld (the same `prlimit64(0, resource, NULL, &old)`
/// a guest issues). Used to render `/proc/<pid>/limits`.
pub(super) fn current_rlimit(resource: u64) -> Result<(u64, u64), i64> {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64);
    request.dirfd = 0;
    request.flags = resource;
    request.mask |= 0x2;
    let payload = call_syscalld_payload(request, LINUX_RLIMIT_SIZE)?;
    let soft = u64::from_ne_bytes(payload[0..8].try_into().map_err(|_| LINUX_EINVAL)?);
    let hard = u64::from_ne_bytes(payload[8..16].try_into().map_err(|_| LINUX_EINVAL)?);
    Ok((soft, hard))
}

pub(super) fn syscall_linux_syscalld_sched_getaffinity(
    pid: u64,
    user_len: u64,
    mask_ptr: u64,
) -> u64 {
    linux_result((|| {
        let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
        if user_len < LINUX_CPUSET_BYTES {
            return Err(LINUX_EINVAL);
        }
        validate_user_write(mask_ptr, LINUX_CPUSET_BYTES)?;
        let online_mask = kernel_hal::api::cpu::admitted_online_mask();
        let task_mask = multitask::linux_task_affinity(pid, online_mask)
            .map_err(affinity_error_to_linux_errno)?;
        if ipc_ops::current_process_has_service_capability(ServiceCapability::VfsPolicy) {
            // vfsd serves requests on its single receive thread while syscalld can
            // be blocked in the mapping broker waiting for vfsd (PREAD64).
            // Offloading a vfsd-issued call would wait on that blocked service
            // until the IPC deadline. (/proc/cpuinfo and the /sys CPU lists no
            // longer come through here: they use PROC_FACTS_OP_ONLINE_CPUS.)
            // syscalld's policy is a pure check of
            // the kernel-stamped task mask against the Online mask, which the kernel
            // has just done, so vfsd's reply is formed here with the same check.
            if task_mask == 0 || task_mask & !online_mask != 0 {
                return Err(LINUX_EINVAL);
            }
            let mut payload = [0_u8; LINUX_CPUSET_BYTES];
            payload[..8].copy_from_slice(&task_mask.to_le_bytes());
            write_user_bytes(mask_ptr, &payload)?;
            return Ok(LINUX_CPUSET_BYTES as u64);
        }
        let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY);
        request.dirfd = pid;
        request.flags = user_len as u64;
        request.mask = u32::try_from(task_mask)
            .expect("SMP invariant: Linux task affinity exceeds fixed commercial envelope");
        let target_process_id = request.pid;
        stamp_affinity_topology(&mut request, online_mask, target_process_id);
        let response = call_syscalld(request)?;
        ensure_syscalld_status(&response)?;
        copy_exact_payload_to_user(
            mask_ptr,
            &response.payload,
            response.payload_len,
            LINUX_CPUSET_BYTES,
        )?;
        Ok(LINUX_CPUSET_BYTES as u64)
    })())
}

pub(super) fn syscall_linux_syscalld_sched_setaffinity(
    pid: u64,
    user_len: u64,
    mask_ptr: u64,
) -> u64 {
    linux_unit_result((|| {
        let user_len = usize::try_from(user_len).map_err(|_| LINUX_EINVAL)?;
        if user_len < LINUX_CPUSET_BYTES {
            return Err(LINUX_EINVAL);
        }
        let mut mask_bytes = [0_u8; LINUX_CPUSET_BYTES];
        read_user_bytes(mask_ptr, &mut mask_bytes)?;
        let online_mask = kernel_hal::api::cpu::admitted_online_mask();
        let requested_mask = u64::from_le_bytes(mask_bytes);
        // Linux silently intersects a requested mask with CPUs physically present
        // and permitted by the kernel. The fixed commercial topology has no
        // separate cpuset/cgroup restriction, so Online is the complete container.
        let effective_mask = requested_mask & online_mask;
        if effective_mask == 0 {
            return Err(LINUX_EINVAL);
        }
        multitask::linux_task_affinity(pid, online_mask)
            .map_err(affinity_error_to_linux_errno)?;

        let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_SCHED_SETAFFINITY);
        request.dirfd = pid;
        request.flags = user_len as u64;
        request.mask = u32::try_from(effective_mask)
            .expect("SMP invariant: Linux task affinity exceeds fixed commercial envelope");
        let target_process_id = request.pid;
        stamp_affinity_topology(&mut request, online_mask, target_process_id);
        call_syscalld_empty(request)?;
        multitask::set_linux_task_affinity(pid, effective_mask, online_mask)
            .map(|_| ())
            .map_err(affinity_error_to_linux_errno)
    })())
}

fn affinity_error_to_linux_errno(error: multitask::AffinityError) -> i64 {
    match error {
        multitask::AffinityError::InvalidMask => LINUX_EINVAL,
        multitask::AffinityError::MissingTask | multitask::AffinityError::WrongAbi => LINUX_ESRCH,
        multitask::AffinityError::PermissionDenied => LINUX_EPERM,
    }
}

// The offload ABI mask width must cover every CPU the kernel can admit.
const _: () =
    assert!(CPU_TOPOLOGY_MAX_LOGICAL_CPUS >= kernel_hal::api::cpu::MAX_SUPPORTED_CPUS as u64);

fn stamp_affinity_topology(
    request: &mut LinuxSyscallOffloadRequest,
    online_mask: u64,
    target_process_id: u64,
) {
    assert!(
        online_mask != 0 && online_mask & !((1_u64 << CPU_TOPOLOGY_MAX_LOGICAL_CPUS) - 1) == 0,
        "SMP invariant: invalid Online mask at Linux affinity boundary"
    );
    assert!(
        target_process_id != 0 && target_process_id == request.pid,
        "SMP invariant: Linux affinity target escaped the authenticated caller process"
    );
    request.arg0 = online_mask;
    request.arg1 = u64::from(online_mask.count_ones());
    request.arg2 = CPU_TOPOLOGY_OBSERVATION_ABI_VERSION;
    request.arg3 = target_process_id;
}

pub(super) fn syscall_linux_syscalld_id_getter(op: u16) -> u64 {
    let response = match call_syscalld_fast_id(op) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    u32::from_le_bytes([
        response.payload[0],
        response.payload[1],
        response.payload[2],
        response.payload[3],
    ]) as u64
}

fn call_syscalld_fast_rpc<Req>(
    op: u16,
    request: &Req,
) -> Result<LinuxSyscallOffloadFastResponse, i64> {
    let (received, bytes) = ipc_ops::call_linux_syscall_fast_endpoint(
        as_bytes(request),
        size_of::<LinuxSyscallOffloadFastResponse>(),
    )?;
    let response = decode_fixed_response::<LinuxSyscallOffloadFastResponse>(&bytes[..received])?;
    if response.version != SYSCALL_OFFLOAD_ABI_VERSION
        || response.op != op
        || response.reserved0 != 0
        || response.payload_len as usize > response.payload.len()
    {
        return Err(LINUX_EINVAL);
    }
    check_ipc_status(response.status)?;
    Ok(response)
}

/// Address-space operations over the compact frame (see
/// `LinuxSyscallOffloadFastMemRequest`). Returns the validated response; a
/// non-zero status from syscalld is returned as its errno, so callers keep the
/// same error handling as with the full request.
pub(super) fn call_syscalld_fast_mem(
    op: u16,
    addr: u64,
    len: u64,
    prot: u32,
    flags: u64,
    fd: u64,
    offset: u64,
) -> Result<LinuxSyscallOffloadFastResponse, i64> {
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    // Bind this request to the caller's exact process/MM incarnation. The MM
    // broker honours only operations that echo this ticket, and only until
    // this function returns - for a reply, a timeout, or an interruption alike.
    let (_, identity) = multitask::current_user_ipc_identity().ok_or(LINUX_EINVAL)?;
    let lease = super::mm_request_lease::RequestLease::open(super::mm_request_lease::MmTarget {
        pid: snapshot.process_id(),
        process_generation: identity.process_generation(),
        mm_generation: identity.mm_generation(),
    })
    .ok_or(LINUX_EAGAIN)?;
    let request = LinuxSyscallOffloadFastMemRequest {
        version: SYSCALL_OFFLOAD_ABI_VERSION,
        op,
        prot,
        pid: snapshot.process_id(),
        tid: snapshot.thread_id(),
        addr,
        len,
        flags,
        fd,
        offset,
        ticket: lease.ticket(),
    };
    call_syscalld_fast_rpc(op, &request)
}

fn call_syscalld_fast_id(op: u16) -> Result<LinuxSyscallOffloadFastResponse, i64> {
    if !matches!(
        op,
        SYSCALL_OFFLOAD_OP_LINUX_GETUID
            | SYSCALL_OFFLOAD_OP_LINUX_GETGID
            | SYSCALL_OFFLOAD_OP_LINUX_GETEUID
            | SYSCALL_OFFLOAD_OP_LINUX_GETEGID
    ) {
        return Err(LINUX_EINVAL);
    }
    let full = new_syscalld_request(op);
    let request = LinuxSyscallOffloadFastRequest {
        version: full.version,
        op,
        reserved0: 0,
        pid: full.pid,
        tid: full.tid,
        uid: full.uid,
        gid: full.gid,
        euid: full.euid,
        egid: full.egid,
    };
    let response = call_syscalld_fast_rpc(op, &request)?;
    exact_payload(&response.payload, response.payload_len, size_of::<u32>())?;
    Ok(response)
}

pub(super) fn syscall_linux_syscalld_setid(op: u16, id: u64) -> u64 {
    linux_unit_result((|| {
        let id = u32::try_from(id).map_err(|_| LINUX_EINVAL)?;
        let mut request = new_syscalld_request(op);
        request.mask = id;
        call_syscalld_empty(request)
    })())
}

/// `setre[ug]id`/`setres[ug]id`: unmodified ids go to syscalld, which owns the
/// credential triples. `ids` are the register values after 32-bit canonicalization.
pub(super) fn syscall_linux_syscalld_set_ids(op: u16, ids: [u64; 3]) -> u64 {
    let mut request = new_syscalld_request(op);
    request.arg0 = ids[0];
    request.arg1 = ids[1];
    request.arg2 = ids[2];
    linux_unit_result(call_syscalld_empty(request))
}

/// Current process umask, read through syscalld without changing it.
pub(super) fn query_umask() -> Result<u32, i64> {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_UMASK);
    request.flags = rustos_user_abi::syscall::SYSCALL_OFFLOAD_UMASK_QUERY;
    let mask = call_syscalld_value::<u32>(request)?;
    if mask & !0o777 != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(mask)
}

pub(super) fn syscall_linux_syscalld_umask(new_mask: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_UMASK);
    request.mask = (new_mask & 0o777) as u32;
    linux_result(call_syscalld_value::<u32>(request).map(u64::from))
}

pub(super) fn syscall_linux_syscalld_u64_getter(op: u16, target_pid: u64) -> u64 {
    let mut request = new_syscalld_request(op);
    request.dirfd = target_pid;
    linux_result(call_syscalld_value::<u64>(request))
}

pub(super) fn syscall_linux_syscalld_setpgid(target_pid: u64, pgid: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_SETPGID);
    request.dirfd = target_pid;
    request.arg0 = pgid;
    linux_unit_result(call_syscalld_empty(request))
}

pub(super) fn syscall_linux_syscalld_getres_id(
    op: u16,
    ruid_ptr: u64,
    euid_ptr: u64,
    suid_ptr: u64,
) -> u64 {
    linux_unit_result((|| {
        let ids = call_syscalld_value::<[u32; 3]>(new_syscalld_request(op))?;
        for (ptr, val) in [(ruid_ptr, ids[0]), (euid_ptr, ids[1]), (suid_ptr, ids[2])] {
            if ptr != 0 {
                write_user_struct(ptr, &val)?;
            }
        }
        Ok(())
    })())
}

pub(super) fn syscall_linux_syscalld_getgroups(size: i64, list_ptr: u64) -> u64 {
    linux_result((|| {
        if size < 0 {
            return Err(LINUX_EINVAL);
        }
        let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_GETGROUPS);
        request.arg0 = size as u64;
        if size > 0 {
            validate_user_write(list_ptr, (size as usize).saturating_mul(size_of::<u32>()))?;
        }
        call_syscalld_value::<u32>(request).map(u64::from)
    })())
}

pub(super) fn syscall_linux_syscalld_getpriority(which: u64, who: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_GETPRIORITY);
    request.flags = which;
    request.dirfd = who;
    linux_result(call_syscalld_value::<u64>(request))
}

pub(super) fn syscall_linux_syscalld_setpriority(which: u64, who: u64, prio: i64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_SETPRIORITY);
    request.flags = which;
    request.dirfd = who;
    request.arg0 = prio as i32 as u64;
    linux_unit_result(call_syscalld_empty(request))
}

fn validate_getrandom_flags(flags: u64) -> Result<(), i64> {
    const GRND_NONBLOCK: u64 = 0x0001;
    const GRND_RANDOM: u64 = 0x0002;
    const GRND_INSECURE: u64 = 0x0004;
    const ALLOWED: u64 = GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE;
    if flags & !ALLOWED != 0 || flags & (GRND_RANDOM | GRND_INSECURE) == GRND_RANDOM | GRND_INSECURE
    {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

pub(super) fn syscall_linux_syscalld_getrandom(user_ptr: u64, user_len: u64, flags: u64) -> u64 {
    if let Err(errno) = validate_getrandom_flags(flags) {
        return linux_errno(errno);
    }
    // Linux clamps a single read-style request to MAX_RW_COUNT.
    const MAX_RW_COUNT: usize = 0x7fff_f000;
    let Ok(len) = usize::try_from(user_len).map(|len| len.min(MAX_RW_COUNT)) else {
        return linux_errno(LINUX_EINVAL);
    };
    if len == 0 {
        return 0;
    }
    let mut chunk = [0_u8; 256];
    let mut rng = nucleus_core::util::random::Random::new();
    let mut copied = 0usize;
    while copied < len {
        let chunk_len = (len - copied).min(chunk.len());
        rng.fill_bytes(&mut chunk[..chunk_len]);
        let Some(dest_ptr) = user_ptr.checked_add(copied as u64) else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::write_current_user_bytes(dest_ptr, &chunk[..chunk_len]) {
            return linux_errno(user_pointer_error_to_linux_errno(err));
        }
        copied += chunk_len;
    }
    copied as u64
}

pub(super) fn syscall_linux_wait4(pid: i64, status_ptr: u64, options: u64, rusage_ptr: u64) -> u64 {
    const WAIT4_SUPPORTED_OPTIONS: u64 =
        (linux_abi::WNOHANG | linux_abi::WUNTRACED | linux_abi::WCONTINUED) as u64;
    if options & !WAIT4_SUPPORTED_OPTIONS != 0 || pid < -1 || pid == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if status_ptr != 0
        && let Err(err) = usermem::validate_current_user_write_buffer(status_ptr, size_of::<i32>())
    {
        return linux_errno(user_pointer_error_to_linux_errno(err));
    }
    if rusage_ptr != 0
        && let Err(err) = usermem::validate_current_user_write_buffer(
            rusage_ptr,
            size_of::<linux_abi::LinuxRusage>(),
        )
    {
        return linux_errno(user_pointer_error_to_linux_errno(err));
    }

    if !ipc_ops::current_process_has_service_capability(ServiceCapability::ProcessPolicy) {
        let mut request = new_procd_request(PROCD_OP_WAIT4);
        request.arg0 = pid as u64;
        request.arg1 = options;
        request.arg2 = status_ptr;
        request.arg3 = rusage_ptr;
        if let Err(errno) =
            call_procd(&request).and_then(|response| ensure_empty_procd_response(&response))
        {
            return linux_errno(errno);
        }
    }

    let parent_pid = match multitask::current_user_process_id() {
        Some(pid) => pid,
        None => return linux_errno(LINUX_ENOSYS),
    };
    let nohang = options & linux_abi::WNOHANG as u64 != 0;
    let include_stopped = options & linux_abi::WUNTRACED as u64 != 0;
    let include_continued = options & linux_abi::WCONTINUED as u64 != 0;
    let mut carried = None;
    loop {
        let result = carried.take().unwrap_or_else(|| {
            multitask::wait_for_child(parent_pid, pid, include_stopped, include_continued)
        });
        match result {
            multitask::WaitChildResult::Exited {
                pid: child_pid,
                status,
            }
            | multitask::WaitChildResult::StateChanged {
                pid: child_pid,
                status,
            } => {
                if status_ptr != 0
                    && let Err(err) = usermem::write_current_user_struct(status_ptr, &status)
                {
                    return linux_errno(user_pointer_error_to_linux_errno(err));
                }
                if rusage_ptr != 0
                    && let Err(err) = usermem::write_current_user_struct(
                        rusage_ptr,
                        &linux_abi::LinuxRusage::default(),
                    )
                {
                    return linux_errno(user_pointer_error_to_linux_errno(err));
                }
                // Blocking wait is the lifecycle consumer's backpressure
                // point. Advance the same bounded cleanup owner as executive
                // housekeeping, then release one acknowledged reclaim token.
                let _ = service_retired_task_runtime_cleanup(RETIRED_TASK_CLEANUP_BUDGET);
                let _ = multitask::service_deferred_work();
                return child_pid;
            }
            multitask::WaitChildResult::Pending if nohang => return 0,
            multitask::WaitChildResult::Pending => {
                let parking =
                    park_for_child_event(parent_pid, pid, include_stopped, include_continued);
                if matches!(parking, ChildParking::Unavailable) {
                    multitask::yield_now();
                }
                match child_wait_after_parking(parking, current_wait_interrupted) {
                    Ok(settled) => carried = settled,
                    Err(errno) => return linux_errno(errno),
                }
            }
            multitask::WaitChildResult::NoMatchingChild => return linux_errno(LINUX_ECHILD),
        }
    }
}

/// Outcome of one attempt to sleep until a child changes state.
enum ChildParking {
    /// Slept and was woken (child event, signal or spurious wake); re-check.
    Resumed,
    /// The child table changed between the first check and the block; the
    /// result is already consumed and must be handled by the caller.
    Settled(multitask::WaitChildResult),
    /// No wait slot or block authority; the caller polls with a yield instead.
    Unavailable,
}

fn child_wait_after_parking(
    parking: ChildParking,
    signal_pending: impl FnOnce() -> bool,
) -> Result<Option<multitask::WaitChildResult>, i64> {
    match parking {
        // The recheck already consumed this status. Deliver it before
        // considering signals, including SIGCHLD; it cannot be put back.
        ChildParking::Settled(settled) => Ok(Some(settled)),
        _ if signal_pending() => Err(LINUX_EINTR),
        _ => Ok(None),
    }
}

/// Blocks the calling thread until `ps` reports a child state change for its
/// process. Producers publish the state and then wake registered parents, so
/// the order arm -> register -> re-check -> commit cannot lose a wakeup.
fn park_for_child_event(
    parent_pid: u64,
    pid: i64,
    include_stopped: bool,
    include_continued: bool,
) -> ChildParking {
    let Some(task_id) = multitask::current_task_id() else {
        return ChildParking::Unavailable;
    };
    while !multitask::arm_block_current_task() {
        if multitask::current_task_id() != Some(task_id) {
            return ChildParking::Unavailable;
        }
        multitask::cond_resched();
    }
    if !multitask::register_child_waiter(parent_pid, task_id) {
        let _ = multitask::cancel_block_current_task();
        return ChildParking::Unavailable;
    }
    let settled = multitask::wait_for_child(parent_pid, pid, include_stopped, include_continued);
    let signal_pending = multitask::current_linux_thread_state()
        .is_some_and(|state| state.pending_signals & !state.signal_mask != 0);
    if !matches!(settled, multitask::WaitChildResult::Pending) || signal_pending {
        let _ = multitask::cancel_block_current_task();
        multitask::unregister_child_waiter(task_id);
        return if matches!(settled, multitask::WaitChildResult::Pending) {
            ChildParking::Resumed
        } else {
            ChildParking::Settled(settled)
        };
    }
    let committed = multitask::commit_block_current_task_and_yield();
    multitask::unregister_child_waiter(task_id);
    if committed.is_some() {
        ChildParking::Resumed
    } else {
        ChildParking::Unavailable
    }
}

pub(super) fn syscall_linux_memfd_create(name_ptr: u64, flags: u64) -> u64 {
    let name = match usermem::read_current_user_c_string(name_ptr, 249) {
        Ok(name) if !name.is_empty() => name,
        Ok(_) => return linux_errno(LINUX_EINVAL),
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if ipc_ops::current_process_has_service_capability(ServiceCapability::VfsPolicy) {
        // vfsd creates its shared-file backing memfds while syscalld is
        // blocked inside the mapping-broker call that is waiting for vfsd's
        // reply. Offloading this call to the single-receiver syscalld would
        // wait on that same blocked service and only end at the IPC deadline.
        // syscalld's memfd policy is stateless (flag mask; the name was
        // validated by the reader above), so the service applies it locally.
        const MFD_KNOWN: u64 = linux_abi::MFD_CLOEXEC | linux_abi::MFD_ALLOW_SEALING;
        if flags & !MFD_KNOWN != 0 {
            return linux_errno(LINUX_EINVAL);
        }
    } else {
        let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_MEMFD_CREATE);
        request.flags = flags;
        request.path_len = name.len() as u32;
        request.path[..name.len()].copy_from_slice(name.as_bytes());
        if let Err(errno) =
            call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response))
        {
            return linux_errno(errno);
        }
    }

    let fd_flags = if flags & linux_abi::MFD_CLOEXEC != 0 {
        multitask::FD_CLOEXEC
    } else {
        0
    };
    let handle = multitask::KernelHandle::Memfd(multitask::MemfdHandle::new(
        name,
        flags & linux_abi::MFD_ALLOW_SEALING != 0,
        // Backing frames bill the creating Job's resident-page group; no
        // published Job (kernel-internal caller) means unaccounted.
        multitask::current_user_job_charge_group(),
    ));
    match multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state
            .handles_mut()
            .install_entry(multitask::HandleEntry::new(
                handle,
                fd_flags,
                linux_abi::O_RDWR,
            ))
    }) {
        Some(Some(fd)) => fd,
        Some(None) => linux_errno(LINUX_EMFILE),
        None => linux_errno(LINUX_ENOSYS),
    }
}

pub(super) fn new_syscalld_request(op: u16) -> LinuxSyscallOffloadRequest {
    let stamp = CallerStamp::current();
    let mut request = LinuxSyscallOffloadRequest {
        op,
        pid: stamp.pid,
        tid: stamp.tid,
        session_handle: stamp.session_handle,
        uid: stamp.uid,
        gid: stamp.gid,
        euid: stamp.euid,
        egid: stamp.egid,
        ..LinuxSyscallOffloadRequest::default()
    };
    if stamp.pid != 0 {
        request.parent_pid = multitask::parent_process_id_of(stamp.pid).unwrap_or(0);
    }
    request
}

pub(super) fn call_syscalld(
    request: LinuxSyscallOffloadRequest,
) -> Result<LinuxSyscallOffloadResponse, i64> {
    let bytes = ipc_ops::call_linux_syscall_endpoint(as_bytes(&request))?;
    let response = decode_fixed_response::<LinuxSyscallOffloadResponse>(bytes.as_slice())?;
    validate_syscalld_response_envelope(request.op, &response)?;
    Ok(response)
}

pub(super) fn call_syscalld_empty(request: LinuxSyscallOffloadRequest) -> LinuxResult<()> {
    let response = call_syscalld(request)?;
    ensure_empty_syscalld_response(&response)
}

pub(super) fn call_syscalld_payload(
    request: LinuxSyscallOffloadRequest,
    expected_len: usize,
) -> LinuxResult<[u8; SYSCALL_OFFLOAD_PAYLOAD_CAPACITY]> {
    let response = call_syscalld(request)?;
    ensure_syscalld_payload(&response, expected_len)?;
    Ok(response.payload)
}

pub(super) fn call_syscalld_value<T: Copy>(request: LinuxSyscallOffloadRequest) -> LinuxResult<T> {
    let response = call_syscalld(request)?;
    ensure_syscalld_status(&response)?;
    decode_exact_payload::<T>(&response.payload, response.payload_len)
}

fn validate_syscalld_response_envelope(
    request_op: u16,
    response: &LinuxSyscallOffloadResponse,
) -> Result<(), i64> {
    if response.version != SYSCALL_OFFLOAD_ABI_VERSION
        || response.op != request_op
        || response.reserved0 != 0
        || response.payload_len as usize > response.payload.len()
    {
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

/// The compact-frame counterpart of `ensure_empty_syscalld_response`: status
/// was already checked when the response was accepted.
pub(super) fn ensure_empty_fast_response(
    response: LinuxSyscallOffloadFastResponse,
) -> Result<(), i64> {
    exact_payload(&response.payload, response.payload_len, 0).map(|_| ())
}

pub(super) fn ensure_empty_syscalld_response(
    response: &LinuxSyscallOffloadResponse,
) -> Result<(), i64> {
    ensure_syscalld_status(response)?;
    exact_payload(&response.payload, response.payload_len, 0).map(|_| ())
}

pub(super) fn ensure_syscalld_payload(
    response: &LinuxSyscallOffloadResponse,
    expected_len: usize,
) -> Result<(), i64> {
    ensure_syscalld_status(response)?;
    exact_payload(&response.payload, response.payload_len, expected_len).map(|_| ())
}

pub(super) fn ensure_syscalld_status(response: &LinuxSyscallOffloadResponse) -> Result<(), i64> {
    check_ipc_status(response.status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM;

    #[test]
    fn local_getrandom_flag_validation_matches_linux_rules() {
        assert_eq!(validate_getrandom_flags(0), Ok(()));
        assert_eq!(validate_getrandom_flags(0x1), Ok(())); // GRND_NONBLOCK
        assert_eq!(validate_getrandom_flags(0x2), Ok(())); // GRND_RANDOM
        assert_eq!(validate_getrandom_flags(0x4), Ok(())); // GRND_INSECURE
        assert_eq!(validate_getrandom_flags(0x1 | 0x4), Ok(()));
        assert_eq!(validate_getrandom_flags(0x2 | 0x4), Err(LINUX_EINVAL));
        assert_eq!(validate_getrandom_flags(0x8), Err(LINUX_EINVAL));
    }

    #[test]
    fn local_utsname_matches_rustos_identity() {
        let uts = local_utsname();
        assert!(uts.sysname.starts_with(b"RustOS\0"));
        assert!(uts.release.starts_with(b"6.6.0-rustos\0"));
        assert!(uts.machine.starts_with(b"x86_64\0"));
    }

    #[test]
    fn syscalld_response_envelope_rejects_oversized_payload_before_slice_use() {
        let mut response = LinuxSyscallOffloadResponse {
            op: SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM,
            ..LinuxSyscallOffloadResponse::default()
        };
        assert_eq!(
            validate_syscalld_response_envelope(SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM, &response,),
            Ok(())
        );

        response.payload_len = response.payload.len() as u32 + 1;
        assert_eq!(
            validate_syscalld_response_envelope(SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM, &response,),
            Err(LINUX_EINVAL)
        );
    }

    #[test]
    fn affinity_topology_stamp_is_versioned_exact_and_reserved_zero() {
        let mut request = LinuxSyscallOffloadRequest {
            op: SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY,
            pid: 42,
            ..LinuxSyscallOffloadRequest::default()
        };
        stamp_affinity_topology(&mut request, 0b1111, 42);
        assert_eq!(request.arg0, 0b1111);
        assert_eq!(request.arg1, 4);
        assert_eq!(request.arg2, CPU_TOPOLOGY_OBSERVATION_ABI_VERSION);
        assert_eq!(request.arg3, 42);
    }

    #[test]
    fn settled_child_status_is_delivered_before_eintr() {
        for result in [
            multitask::WaitChildResult::Exited {
                pid: 42,
                status: 0x2a00,
            },
            multitask::WaitChildResult::StateChanged {
                pid: 42,
                status: 0x137f,
            },
            multitask::WaitChildResult::NoMatchingChild,
        ] {
            assert!(matches!(
                child_wait_after_parking(ChildParking::Settled(result), || panic!(
                    "status owns completion"
                )),
                Ok(Some(_))
            ));
        }
        for parking in [ChildParking::Resumed, ChildParking::Unavailable] {
            assert!(matches!(
                child_wait_after_parking(parking, || true),
                Err(LINUX_EINTR)
            ));
        }
        assert!(matches!(
            child_wait_after_parking(ChildParking::Resumed, || false),
            Ok(None)
        ));
        let source = include_str!("syscalld_ops.rs")
            .split_once("#[cfg(test)]")
            .unwrap()
            .0;
        assert!(!source.contains("WaitChildResult::NoMatchingChild if nohang"));
    }

    #[test]
    fn wait_exit_advances_runtime_cleanup_before_reclaim_and_return() {
        let source = include_str!("syscalld_ops.rs");
        let wait = source
            .split_once("pub(super) fn syscall_linux_wait4(")
            .expect("wait4 entry")
            .1
            .split_once("pub(super) fn syscall_linux_memfd_create(")
            .expect("wait4 boundary")
            .0;
        let cleanup = wait
            .find("service_retired_task_runtime_cleanup(RETIRED_TASK_CLEANUP_BUDGET)")
            .expect("bounded runtime cleanup");
        let reclaim = wait
            .find("multitask::service_deferred_work()")
            .expect("acknowledged scheduler/process reclaim");
        let returned = wait.find("return child_pid").expect("terminal wait return");
        assert!(cleanup < reclaim && reclaim < returned);
    }

    #[test]
    fn wait4_rejects_invalid_options_or_pgid_filters_before_procd_ipc() {
        assert_eq!(
            syscall_linux_wait4(-1, 0, u64::MAX, 0),
            linux_errno(LINUX_EINVAL)
        );
        assert_eq!(
            syscall_linux_wait4(0, 0, 0, 0),
            linux_errno(LINUX_EINVAL)
        );
        assert_eq!(
            syscall_linux_wait4(-2, 0, 0, 0),
            linux_errno(LINUX_EINVAL)
        );
    }
}
