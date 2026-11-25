//! Linux fork broker transaction.
//!
//! - **Owner:** `kernel-compat` composes Linux fork; `kernel-ps` owns process/VMA
//!   publication and `kernel-mm` owns COW PTE/frame transitions.
//! - **Boundary:** Only a validated policy request and exact source pid/tid may
//!   consume a reserved child lifecycle token.
//! - **Lifecycle:** Reserve invisible child, hold parent VMAs, clone COW state,
//!   publish suspended child VMAs, then activate exactly once.
//! - **Concurrency:** One process-state/VMA-writer transaction excludes mapping
//!   mutation and drains fault installers through parent downgrade.
//! - **Failure:** Every pre-activation error restores parent state, terminates
//!   the suspended child if published, and settles the exact reservation.
//! - **Forbidden:** No runnable child with a reservation hole, stale parent
//!   identity, partial COW ledger, or Windows CreateProcess inheritance.
//! - **Evidence:** `CowFrameLifecycle`, fork-hold/rollback witnesses, and the
//!   `fork_cow_private_write` KVM probe.

use super::*;

/// Process slots and scheduler task slots are returned by the housekeeping
/// reap pass, not by the parent's `wait`. A full table with exited children
/// pending is transient, so kick the pass and wait for it in short sleeps (it
/// may run on another CPU) before reporting EAGAIN.
///
/// Both resources are waited for: the process-table reservation (taken here)
/// and a scheduler task slot (observed here, taken when the child is
/// created). With only the first, a sequential fork/wait loop on a machine
/// whose task slots are nearly all in use occasionally found the table
/// free and the slot not yet retired, and the child was refused with EAGAIN
/// after the address space had already been cloned.
fn reserve_spawn_after_reclaim(parent_pid: u64) -> Option<multitask::SpawnReservation> {
    const RECLAIM_WAIT_MS: usize = 100;
    // A fork joins its parent's Job, and the Job's process limit is checked
    // authoritatively only when the child is published, after the address space
    // has been cloned. An exited child still counts until the housekeeping pass
    // retires it, so check it here as well and wait the same way.
    let parent_job = multitask::process_job(parent_pid);
    let mut waited = 0;
    loop {
        let job_admits = parent_job.is_none_or(multitask::job_admits_process);
        if job_admits && multitask::task_slot_available() {
            if let Some(reservation) = multitask::reserve_process_spawn() {
                return Some(reservation);
            }
        }
        if waited == RECLAIM_WAIT_MS {
            return None;
        }
        multitask::kick_housekeeping();
        crate::arch::rtc::sleep(1);
        waited += 1;
    }
}

/// Longest the COW clone waits for frames before the fork reports ENOMEM.
const FORK_FRAME_WAIT_MS: u32 = 250;
const FORK_FRAME_WAIT_STEP_MS: u32 = 5;

/// One bounded backoff step after a frame-exhausted clone. `false` once the
/// budget is spent (the caller then fails the fork with ENOMEM).
fn wait_for_fork_frames(waited_steps: &mut u32) -> bool {
    if *waited_steps * FORK_FRAME_WAIT_STEP_MS >= FORK_FRAME_WAIT_MS {
        return false;
    }
    *waited_steps += 1;
    multitask::kick_housekeeping();
    crate::arch::rtc::sleep(u64::from(FORK_FRAME_WAIT_STEP_MS));
    true
}

/// `linux-fork-stage` detail for a refused clone: the address-space error in
/// the low byte and the free page count at refusal above it, so a failed fork
/// states whether the machine really was out of frames.
fn fork_failure_detail(error: u64, free_bytes: u64) -> u64 {
    (error & 0xff) | ((free_bytes / rustos_user_abi::pager::PAGER_PAGE_BYTES) << 8)
}

pub(super) fn syscall_linux_rustos_proc_fork_broker(args_ptr: u64) -> u64 {
    if !current_process_can_policy() {
        return linux_errno(LINUX_EPERM);
    }
    let args: RustosProcForkBrokerArgs = match read_user_struct_as(args_ptr) {
        Ok(args) => args,
        Err(errno) => return linux_errno(errno),
    };
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || args.flags != 0
        || args.source_pid == 0
        || args.source_tid == 0
        || !valid_process_fork_plan_locally(&args)
    {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(thread_snapshot) =
        multitask::linux_thread_snapshot_by_ids(args.source_pid, args.source_tid)
    else {
        return linux_errno(LINUX_ESRCH);
    };
    if !source_thread_waiting_for_fork_reply(args.source_pid, args.source_tid) {
        return fork_stage_failed(25, LINUX_ETIMEDOUT, args.source_tid);
    }
    let spawn_reservation = match reserve_spawn_after_reclaim(args.source_pid) {
        Some(reservation) => reservation,
        None => return fork_stage_failed(20, LINUX_EAGAIN, 0),
    };
    let is_vfork = (args.clone_flags & (linux_abi::CLONE_VM | linux_abi::CLONE_VFORK))
        == (linux_abi::CLONE_VM | linux_abi::CLONE_VFORK);
    let child_pid = if is_vfork {
        spawn_vfork_child(&args, &thread_snapshot, spawn_reservation)
    } else {
        spawn_cow_fork_child(&args, &thread_snapshot, spawn_reservation)
    };
    child_pid
}

fn spawn_vfork_child(
    args: &RustosProcForkBrokerArgs,
    thread_snapshot: &multitask::LinuxThreadSnapshot,
    spawn_reservation: multitask::SpawnReservation,
) -> u64 {
    let clear_sighand = args.clone_flags & linux_abi::CLONE_CLEAR_SIGHAND != 0;
    let vforked = multitask::with_fork_parent_vfork_state(args.source_pid, |parent, regions| {
        if args.clone_flags & linux_abi::CLONE_CHILD_SETTID != 0 {
            validate_vfork_tid_write_target(parent, regions, args.ctid_ptr)?;
        }
        if args.clone_flags & linux_abi::CLONE_PARENT_SETTID != 0 {
            validate_vfork_tid_write_target(parent, regions, args.ptid_ptr)?;
        }
        let vfork_local = parent.fork_clone_vfork_local(clear_sighand, None);
        let logical_admin = parent.security().is_logical_admin();
        let linux_process_state = parent.linux_process_state().copied();
        let linux_memory_map = parent.linux_memory_map().cloned();
        let linux_runtime_profile = parent.linux_runtime_profile().cloned();
        parent.begin_fork_handle_pin();
        Ok::<_, crate::memory::paging::AddressSpaceError>((
            vfork_local,
            logical_admin,
            linux_process_state,
            linux_memory_map,
            linux_runtime_profile,
        ))
    });
    let (vfork_local, logical_admin, linux_process_state, linux_memory_map, linux_runtime_profile) =
        match unpack_fork_parent_result(vforked, spawn_reservation) {
            Ok(result) => result,
            Err(ret) => return ret,
        };
    let fork_pin = ForkHandlePinGuard::new(args.source_pid);
    let bootstrap = build_fork_bootstrap(
        args,
        thread_snapshot,
        logical_admin,
        linux_process_state,
        linux_memory_map,
        linux_runtime_profile,
        vfork_local.exec_path(),
    );
    let inherited_service_refs = match finish_acquire_service_refs(
        fork_pin,
        acquire_cloned_service_handle_refs_from_table(vfork_local.handles()),
        (args.source_pid, spawn_reservation),
    ) {
        Ok(refs) => refs,
        Err(ret) => return ret,
    };
    let child_pid = match multitask::spawn_user_process_vfork_suspended_with_parent_reservation(
        vfork_local,
        bootstrap,
        args.source_pid,
        multitask::DEFAULT_USER_TASK_WEIGHT_MICROS,
        spawn_reservation,
    ) {
        Ok(pid) => pid,
        Err(err) => {
            return fail_fork_spawn(&inherited_service_refs, spawn_reservation, err, args.source_pid);
        }
    };
    if args.clone_flags & linux_abi::CLONE_CHILD_SETTID != 0 {
        if let Err(ret) = write_fork_tid(child_pid, child_pid, args.ctid_ptr) {
            return ret;
        }
    }
    if args.clone_flags & linux_abi::CLONE_PARENT_SETTID != 0 {
        if let Err(ret) = write_fork_tid(child_pid, args.source_pid, args.ptid_ptr) {
            return ret;
        }
    }
    publish_and_activate_fork_child(args.source_pid, args.source_tid, child_pid, true)
}

fn acquire_cloned_service_handle_refs_from_table(
    handles: &multitask::HandleTable,
) -> Result<Vec<ServiceHandleRef>, i64> {
    let refs: Vec<ServiceHandleRef> = handles
        .entries_snapshot(false)
        .into_iter()
        .filter_map(|(_, entry)| service_handle_ref_for_handle(entry.handle()))
        .collect();
    let mut acquired = Vec::with_capacity(refs.len());
    for handle_ref in refs {
        if let Err(errno) = acquire_service_handle_ref(&handle_ref) {
            release_service_handle_refs(&acquired);
            return Err(errno);
        }
        acquired.push(handle_ref);
    }
    Ok(acquired)
}

fn build_fork_bootstrap(
    args: &RustosProcForkBrokerArgs,
    thread_snapshot: &multitask::LinuxThreadSnapshot,
    logical_admin: bool,
    linux_process_state: Option<crate::multitask::LinuxProcessState>,
    linux_memory_map: Option<crate::multitask::LinuxMemoryMapState>,
    linux_runtime_profile: Option<crate::multitask::LinuxRuntimeProfile>,
    exec_path: &str,
) -> multitask::UserTaskBootstrap {
    let mut child_thread_state = thread_snapshot.thread_state;
    child_thread_state.clear_child_tid = if args.clone_flags & linux_abi::CLONE_CHILD_CLEARTID != 0
    {
        args.ctid_ptr
    } else {
        0
    };
    child_thread_state.robust_list_head = 0;
    child_thread_state.robust_list_len = 0;
    child_thread_state.rseq_area = 0;
    child_thread_state.rseq_len = 0;
    child_thread_state.rseq_signature = 0;
    child_thread_state.pending_signals = 0;
    child_thread_state.pending_sigchld_events = 0;

    let mut bootstrap = multitask::UserTaskBootstrap::new(
        crate::user::abi::UserAbi::Linux,
        VirtAddr::new(args.registers.rip),
        VirtAddr::new(if args.stack_ptr != 0 {
            args.stack_ptr
        } else {
            args.registers.rsp
        }),
    );
    bootstrap.registers = user_registers_to_task_registers(args.registers);
    bootstrap.registers.rax = 0;
    bootstrap.registers.rcx = args.registers.rip;
    bootstrap.registers.r11 = args.registers.rflags;
    bootstrap.user_stack = thread_snapshot.user_stack;
    bootstrap.console_session = thread_snapshot.console_session;
    bootstrap.logical_admin = logical_admin;
    bootstrap.linux_process_state = linux_process_state;
    bootstrap.linux_memory_map = linux_memory_map;
    bootstrap.linux_runtime_profile = linux_runtime_profile;
    bootstrap.linux_thread_state = Some(child_thread_state);
    bootstrap.set_exec_path(exec_path);
    bootstrap
}

/// Whether `fork` shares this region's resident private pages copy-on-write
/// instead of copying them into the child eagerly.
///
/// Private file and image-section mappings qualify as well as anonymous ones:
/// once a process has written such a page (a relocated `.data.rel.ro`, a
/// patched GOT), its leaf is an exclusive private copy that is indistinguishable
/// from an anonymous page, and Linux shares it with the child like any other
/// `MAP_PRIVATE` page. Pages that are still unmodified file-cache aliases keep
/// their COW tag and role and are inherited as such. Copying the dirty ones
/// eagerly cost Chrome's zygote about 70 MiB per forked child. Shared, device
/// and memfd regions never qualify, and neither does an uncommitted region.
fn fork_shares_region_copy_on_write(region: &rustos_user_abi::pager::PagerVmRegionWire) -> bool {
    use rustos_user_abi::pager::{
        VM_COMMIT_COMMITTED, VM_OBJECT_ANONYMOUS, VM_OBJECT_FILE_PRIVATE, VM_OBJECT_IMAGE_SECTION,
        VM_SHARING_PRIVATE,
    };
    region.sharing == VM_SHARING_PRIVATE
        && region.commit_state == VM_COMMIT_COMMITTED
        && matches!(
            region.object.object_type,
            VM_OBJECT_ANONYMOUS | VM_OBJECT_FILE_PRIVATE | VM_OBJECT_IMAGE_SECTION
        )
}

fn spawn_cow_fork_child(
    args: &RustosProcForkBrokerArgs,
    thread_snapshot: &multitask::LinuxThreadSnapshot,
    spawn_reservation: multitask::SpawnReservation,
) -> u64 {
    // One mmap-lock-equivalent transaction now covers both authorities the
    // child inherits. The process state lock stabilizes page tables while the
    // VMA writer holds every live publication at an odd sequence; concurrent
    // faults retry, and no mmap/unmap can be linearized inside this snapshot.
    #[cfg(rustos_lifecycle_trace)]
    let fork_started = crate::arch::clock::monotonic_nanos();
    #[cfg(rustos_lifecycle_trace)]
    let mut clone_ns = 0;
    let mut frame_waits = 0_u32;
    let forked = loop {
    let attempt = multitask::with_fork_parent_state(args.source_pid, |parent, regions| {
        let mut eager_private_ranges = Vec::new();
        if args.clone_flags & linux_abi::CLONE_CHILD_SETTID != 0 {
            let byte_len = core::mem::size_of::<u32>();
            if !writable_committed_regions_cover(regions, args.ctid_ptr, byte_len) {
                return Err(crate::memory::paging::AddressSpaceError::ProtectionViolation);
            }
            parent
                .address_space()
                .validate_user_read_buffer(VirtAddr::new(args.ctid_ptr), byte_len)?;
            let page_bytes = rustos_user_abi::pager::PAGER_PAGE_BYTES;
            let first_page = args.ctid_ptr & !(page_bytes - 1);
            let last_byte = args
                .ctid_ptr
                .checked_add(byte_len as u64 - 1)
                .ok_or(crate::memory::paging::AddressSpaceError::AddressOverflow)?;
            let page_count = usize::try_from((last_byte - first_page) / page_bytes + 1)
                .map_err(|_| crate::memory::paging::AddressSpaceError::AddressOverflow)?;
            eager_private_ranges
                .try_reserve_exact(1)
                .map_err(|_| crate::memory::paging::AddressSpaceError::OutOfFrames)?;
            eager_private_ranges.push(crate::memory::paging::UserRegion {
                start: VirtAddr::new(first_page),
                page_count,
            });
        }

        let mut cow_ranges = Vec::new();
        cow_ranges
            .try_reserve_exact(regions.len())
            .map_err(|_| crate::memory::paging::AddressSpaceError::OutOfFrames)?;
        for region in regions {
            if fork_shares_region_copy_on_write(region) {
                let bytes = region
                    .end
                    .checked_sub(region.start)
                    .ok_or(crate::memory::paging::AddressSpaceError::AddressOverflow)?;
                let page_count = usize::try_from(bytes / rustos_user_abi::pager::PAGER_PAGE_BYTES)
                    .map_err(|_| crate::memory::paging::AddressSpaceError::AddressOverflow)?;
                cow_ranges.push(crate::memory::paging::UserRegion {
                    start: VirtAddr::new(region.start),
                    page_count,
                });
            }
        }
        cow_ranges.sort_unstable_by_key(|region| region.start.as_u64());
        #[cfg(rustos_lifecycle_trace)]
        let clone_started = crate::arch::clock::monotonic_nanos();
        let address_space = parent
            .address_space_mut()
            .clone_user_space_cow(&cow_ranges, &eager_private_ranges)?;
        #[cfg(rustos_lifecycle_trace)]
        {
            clone_ns = crate::arch::clock::monotonic_nanos().saturating_sub(clone_started);
        }
        let mut inherited_regions = Vec::new();
        inherited_regions
            .try_reserve_exact(regions.len())
            .map_err(|_| crate::memory::paging::AddressSpaceError::OutOfFrames)?;
        inherited_regions.extend_from_slice(regions);
        let child_state = parent.fork_clone(address_space, None);
        parent.begin_fork_handle_pin();
        Ok::<_, crate::memory::paging::AddressSpaceError>((child_state, inherited_regions))
        });
    // All fork locks are released here. Frame exhaustion is often a transient
    // overlap with an exiting process or the memory governor's reclaim, so
    // wait boundedly before reporting ENOMEM (Linux retries small allocations
    // under pressure instead of failing the clone at the first miss).
    if matches!(
        attempt,
        Ok(Err(crate::memory::paging::AddressSpaceError::OutOfFrames))
    ) && wait_for_fork_frames(&mut frame_waits)
    {
        continue;
    }
    break attempt;
    };
    let (child_state, inherited_regions) =
        match unpack_fork_parent_result(forked, spawn_reservation) {
            Ok(result) => result,
            Err(ret) => return ret,
        };
    let fork_pin = ForkHandlePinGuard::new(args.source_pid);
    #[cfg(rustos_lifecycle_trace)]
    {
        // Emit only after the parent process/VMA guards have been released.
        // Nanoseconds: entire held fork transaction vs just page-table/COW
        // cloning. Diagnostic profiles are never shipping-latency evidence.
        let fork_ns = crate::arch::clock::monotonic_nanos().saturating_sub(fork_started);
        debug::record_milestone(
            debug::LogCategory::Memory,
            "proc-fork-work",
            fork_ns,
            clone_ns,
        );
    }
    let bootstrap = build_fork_bootstrap(
        args,
        thread_snapshot,
        child_state.security().is_logical_admin(),
        child_state.linux_process_state().copied(),
        child_state.linux_memory_map().cloned(),
        child_state.linux_runtime_profile().cloned(),
        child_state.exec_path(),
    );

    let inherited_service_refs = match finish_acquire_service_refs(
        fork_pin,
        acquire_cloned_service_handle_refs(&child_state),
        (args.source_pid, spawn_reservation),
    ) {
        Ok(refs) => refs,
        Err(ret) => return ret,
    };
    let child_pid = match multitask::spawn_user_process_state_suspended_with_parent_reservation(
        child_state,
        bootstrap,
        Some(args.source_pid),
        multitask::DEFAULT_USER_TASK_WEIGHT_MICROS,
        spawn_reservation,
    ) {
        Ok(pid) => pid,
        Err(err) => {
            return fail_fork_spawn(&inherited_service_refs, spawn_reservation, err, args.source_pid);
        }
    };
    // The child now exists but is still suspended, which is the only point at
    // which its reservation can be published: the target is addressed by pid,
    // and nothing may observe a runnable child whose address space is missing
    // ranges its parent held.  A failure here revokes what it published and
    // destroys the child, exactly as a `dup_mmap` failure destroys the child mm.
    if let Err(errno) = crate::user::syscall::linux::mm_broker_ops::inherit_pager_vmas(
        child_pid,
        &inherited_regions,
    ) {
        let _ = multitask::terminate_user_task(child_pid);
        return fork_stage_failed(2, errno, 0);
    }
    if let Err(errno) = inherit_shared_memfd_mappings(child_pid) {
        let _ = multitask::terminate_user_task(child_pid);
        return fork_stage_failed(2, errno, 0);
    }
    if args.clone_flags & linux_abi::CLONE_CHILD_SETTID != 0 {
        if let Err(ret) = write_fork_tid(child_pid, child_pid, args.ctid_ptr) {
            return ret;
        }
    }
    publish_and_activate_fork_child(args.source_pid, args.source_tid, child_pid, false)
}

fn validate_vfork_tid_write_target(
    parent: &multitask::UserProcessState,
    regions: &[rustos_user_abi::pager::PagerVmRegionWire],
    ptr: u64,
) -> Result<(), crate::memory::paging::AddressSpaceError> {
    let byte_len = core::mem::size_of::<u32>();
    if !writable_committed_regions_cover(regions, ptr, byte_len) {
        return Err(crate::memory::paging::AddressSpaceError::ProtectionViolation);
    }
    parent
        .address_space()
        .validate_user_write_buffer(VirtAddr::new(ptr), byte_len)
}

fn unpack_fork_parent_result<T>(
    result: Result<
        Result<T, crate::memory::paging::AddressSpaceError>,
        multitask::PagerVmaError,
    >,
    spawn_reservation: multitask::SpawnReservation,
) -> Result<T, u64> {
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => {
            let _ = multitask::cancel_process_spawn(spawn_reservation);
            Err(fork_stage_failed(
                1,
                address_space_error_to_linux_errno(err),
                fork_failure_detail(err as u64, crate::memory::phys::free_bytes()),
            ))
        }
        Err(error) => {
            let _ = multitask::cancel_process_spawn(spawn_reservation);
            Err(fork_stage_failed(5 + error as i64 as u64, LINUX_ENOMEM, 0))
        }
    }
}

fn finish_acquire_service_refs(
    fork_pin: ForkHandlePinGuard,
    refs_result: Result<Vec<ServiceHandleRef>, i64>,
    (source_pid, spawn_reservation): (u64, multitask::SpawnReservation),
) -> Result<Vec<ServiceHandleRef>, u64> {
    drop(fork_pin);
    match refs_result {
        Ok(refs) => Ok(refs),
        Err(errno) => {
            let _ = multitask::cancel_process_spawn(spawn_reservation);
            Err(fork_stage_failed(24, errno, source_pid))
        }
    }
}

fn fail_fork_spawn(
    inherited_service_refs: &[ServiceHandleRef],
    spawn_reservation: multitask::SpawnReservation,
    err: multitask::SpawnTaskError,
    source_pid: u64,
) -> u64 {
    release_service_handle_refs(inherited_service_refs);
    let _ = multitask::cancel_process_spawn(spawn_reservation);
    fork_stage_failed(23, process_spawn_error_to_linux_errno(err), source_pid)
}

fn write_fork_tid(child_pid: u64, target_pid: u64, ptr: u64) -> Result<(), u64> {
    let child_tid = (child_pid as u32).to_le_bytes();
    let write_result = multitask::with_process_state_by_pid_mut(target_pid, |state| {
        state.address_space().copy_into_user(VirtAddr::new(ptr), &child_tid)
    });
    match write_result {
        Some(Ok(())) => Ok(()),
        Some(Err(err)) => {
            let _ = multitask::terminate_user_task(child_pid);
            Err(fork_stage_failed(
                3,
                address_space_error_to_linux_errno(err),
                err as u64,
            ))
        }
        None => {
            let _ = multitask::terminate_user_task(child_pid);
            Err(fork_stage_failed(21, LINUX_EAGAIN, child_pid))
        }
    }
}

fn publish_and_activate_fork_child(
    source_pid: u64,
    source_tid: u64,
    child_pid: u64,
    vfork_wait: bool,
) -> u64 {
    if !source_thread_waiting_for_fork_reply(source_pid, source_tid) {
        let _ = multitask::terminate_user_task(child_pid);
        return fork_stage_failed(25, LINUX_ETIMEDOUT, child_pid);
    }
    // Publish the child to vfsd before it can run: the child is still
    // suspended, so its first namespace request cannot precede this event, and
    // the parent cannot return from fork (and chdir) before it is queued.
    offload_ops::record_process_fork(child_pid, source_pid);
    if vfork_wait && !multitask::begin_vfork_wait(child_pid, source_tid) {
        offload_ops::record_process_fork_abandoned(child_pid);
        let _ = multitask::terminate_user_task(child_pid);
        return fork_stage_failed(22, LINUX_EAGAIN, child_pid);
    }
    if !multitask::activate_suspended_user_task(child_pid) {
        if vfork_wait {
            multitask::cancel_vfork_wait(child_pid, source_tid);
        }
        offload_ops::record_process_fork_abandoned(child_pid);
        let _ = multitask::terminate_user_task(child_pid);
        return fork_stage_failed(22, LINUX_EAGAIN, child_pid);
    }
    multitask::set_next_spawn_pick_hint(child_pid);
    multitask::request_deferred_reschedule();
    child_pid
}

fn inherit_shared_memfd_mappings(child_pid: u64) -> Result<(), i64> {
    let holds = multitask::with_process_state_by_pid(child_pid, |child| child.shared_memfd_holds())
        .ok_or(LINUX_ESRCH)?;
    for hold in holds {
        let res = hold.with_reconcile_and_frames(|object_id, frames| {
            multitask::with_process_state_by_pid_mut(child_pid, |child| {
                child.reconcile_shared_memfd_mappings(object_id, frames)
            })
        });
        match res {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(address_space_error_to_linux_errno(err)),
            None => return Err(LINUX_ESRCH),
        }
    }
    Ok(())
}

fn writable_committed_regions_cover(
    regions: &[rustos_user_abi::pager::PagerVmRegionWire],
    start: u64,
    byte_len: usize,
) -> bool {
    if byte_len == 0 {
        return true;
    }
    let Some(end) = start.checked_add(byte_len as u64) else {
        return false;
    };
    // `regions` arrive in publication-slot order, which has nothing to do with
    // address order, so each step finds the region that contains the cursor
    // instead of assuming a sorted walk (a sorted walk rejected a perfectly
    // covered range whenever a higher region sat in an earlier slot, which
    // failed most multi-threaded forks with EFAULT).
    let mut cursor = start;
    while cursor < end {
        let Some(region) = regions
            .iter()
            .find(|region| region.start <= cursor && cursor < region.end)
        else {
            return false;
        };
        if region.commit_state != rustos_user_abi::pager::VM_COMMIT_COMMITTED
            || region.prot & rustos_user_abi::pager::VM_PROT_WRITE == 0
        {
            return false;
        }
        cursor = core::cmp::min(end, region.end);
    }
    true
}

fn user_tid_span_in_window(ptr: u64) -> bool {
    ptr >= PROC_BROKER_USER_SPACE_BASE
        && ptr
            .checked_add(core::mem::size_of::<u32>() as u64)
            .is_some_and(|end| end <= PROC_BROKER_USER_SPACE_END_EXCLUSIVE)
}

pub(super) fn valid_process_fork_plan_locally(args: &RustosProcForkBrokerArgs) -> bool {
    let is_vfork = args.clone_flags & (linux_abi::CLONE_VM | linux_abi::CLONE_VFORK) != 0;
    if is_vfork
        && (args.clone_flags & (linux_abi::CLONE_VM | linux_abi::CLONE_VFORK))
            != (linux_abi::CLONE_VM | linux_abi::CLONE_VFORK)
    {
        return false;
    }
    let supported = if is_vfork {
        linux_abi::CSIGNAL
            | linux_abi::CLONE_VM
            | linux_abi::CLONE_VFORK
            | linux_abi::CLONE_CLEAR_SIGHAND
            | linux_abi::CLONE_PARENT_SETTID
            | linux_abi::CLONE_CHILD_SETTID
            | linux_abi::CLONE_CHILD_CLEARTID
    } else {
        linux_abi::CSIGNAL | linux_abi::CLONE_CHILD_SETTID | linux_abi::CLONE_CHILD_CLEARTID
    };
    let exit_signal = args.clone_flags & linux_abi::CSIGNAL;
    // The child's initial rip/rsp become `VirtAddr::new` operands, which panics
    // on a non-canonical address. The registers originate from the forking
    // user thread (its rsp is arbitrary), so reject them here, not there.
    let child_stack = if args.stack_ptr != 0 {
        args.stack_ptr
    } else {
        args.registers.rsp
    };
    VirtAddr::try_new(args.registers.rip).is_ok()
        && VirtAddr::try_new(child_stack).is_ok()
        && args.clone_flags & !supported == 0
        && (exit_signal == 0 || exit_signal == linux_abi::SIGCHLD)
        && ((args.clone_flags & linux_abi::CLONE_PARENT_SETTID == 0 && args.ptid_ptr == 0)
            || (args.clone_flags & linux_abi::CLONE_PARENT_SETTID != 0
                && user_tid_span_in_window(args.ptid_ptr)))
        && args.tls == 0
        && (args.clone_flags & (linux_abi::CLONE_CHILD_SETTID | linux_abi::CLONE_CHILD_CLEARTID)
            == 0
            || user_tid_span_in_window(args.ctid_ptr))
}

#[cfg(test)]
mod tests {
    use super::{
        user_tid_span_in_window, valid_process_fork_plan_locally, writable_committed_regions_cover,
        RustosProcForkBrokerArgs,
    };
    use super::super::{linux_abi, PROC_BROKER_USER_SPACE_BASE, PROC_BROKER_USER_SPACE_END_EXCLUSIVE};
    use rustos_user_abi::pager::{
        PagerVmRegionWire, VM_COMMIT_COMMITTED, VM_COMMIT_RESERVED, VM_PROT_READ, VM_PROT_WRITE,
    };

    #[test]
    fn fork_shares_private_committed_anonymous_and_file_regions_only() {
        use rustos_user_abi::pager::{
            VM_OBJECT_ANONYMOUS, VM_OBJECT_DEVICE_PINNED, VM_OBJECT_FILE_PRIVATE,
            VM_OBJECT_FILE_SHARED, VM_OBJECT_IMAGE_SECTION, VM_OBJECT_MEMFD, VM_SHARING_PRIVATE,
            VM_SHARING_SHARED,
        };
        let mut region = region(0x1000, 0x2000, VM_PROT_WRITE, VM_COMMIT_COMMITTED);
        region.sharing = VM_SHARING_PRIVATE;
        for (object_type, shared) in [
            (VM_OBJECT_ANONYMOUS, true),
            (VM_OBJECT_FILE_PRIVATE, true),
            (VM_OBJECT_IMAGE_SECTION, true),
            (VM_OBJECT_FILE_SHARED, false),
            (VM_OBJECT_MEMFD, false),
            (VM_OBJECT_DEVICE_PINNED, false),
        ] {
            region.object.object_type = object_type;
            assert_eq!(super::fork_shares_region_copy_on_write(&region), shared);
        }
        region.object.object_type = VM_OBJECT_ANONYMOUS;
        region.sharing = VM_SHARING_SHARED;
        assert!(!super::fork_shares_region_copy_on_write(&region));
        region.sharing = VM_SHARING_PRIVATE;
        region.commit_state = VM_COMMIT_RESERVED;
        assert!(!super::fork_shares_region_copy_on_write(&region));
    }

    #[test]
    fn fork_failure_detail_keeps_error_and_free_pages() {
        assert_eq!(super::fork_failure_detail(8, 0), 8);
        assert_eq!(super::fork_failure_detail(8, 4096 * 6381), (6381 << 8) | 8);
    }

    #[test]
    fn valid_process_fork_plan_locally_admits_vfork_and_rejects_half_vfork() {
        let mut args = RustosProcForkBrokerArgs::default();
        args.registers.rip = PROC_BROKER_USER_SPACE_BASE + 0x1000;
        args.registers.rsp = PROC_BROKER_USER_SPACE_BASE + 0x4000;
        args.clone_flags = linux_abi::CLONE_VM
            | linux_abi::CLONE_VFORK
            | linux_abi::CLONE_CLEAR_SIGHAND
            | linux_abi::CLONE_PARENT_SETTID
            | linux_abi::CLONE_CHILD_SETTID
            | linux_abi::CLONE_CHILD_CLEARTID
            | linux_abi::SIGCHLD;
        args.ptid_ptr = PROC_BROKER_USER_SPACE_BASE + 0x2000;
        args.ctid_ptr = PROC_BROKER_USER_SPACE_BASE + 0x2008;
        assert!(valid_process_fork_plan_locally(&args));

        args.clone_flags = linux_abi::CLONE_VM | linux_abi::SIGCHLD;
        assert!(!valid_process_fork_plan_locally(&args));
        args.clone_flags = linux_abi::CLONE_VFORK | linux_abi::SIGCHLD;
        assert!(!valid_process_fork_plan_locally(&args));
    }

    fn region(start: u64, end: u64, prot: u32, commit_state: u16) -> PagerVmRegionWire {
        PagerVmRegionWire {
            start,
            end,
            prot,
            commit_state,
            ..PagerVmRegionWire::default()
        }
    }

    #[test]
    fn child_tid_span_requires_gapless_committed_write_authority() {
        let regions = [
            region(
                0x1000,
                0x2000,
                VM_PROT_READ | VM_PROT_WRITE,
                VM_COMMIT_COMMITTED,
            ),
            region(0x2000, 0x3000, VM_PROT_WRITE, VM_COMMIT_COMMITTED),
        ];
        assert!(writable_committed_regions_cover(&regions, 0x1ffe, 4));
        assert!(!writable_committed_regions_cover(&regions, 0x2ffe, 4));

        let gap = [
            region(0x1000, 0x1800, VM_PROT_WRITE, VM_COMMIT_COMMITTED),
            region(0x1900, 0x3000, VM_PROT_WRITE, VM_COMMIT_COMMITTED),
        ];
        assert!(!writable_committed_regions_cover(&gap, 0x17ff, 0x102));
    }

    #[test]
    fn child_tid_coverage_does_not_depend_on_publication_slot_order() {
        // Slot order is arbitrary: a higher region published first must not
        // hide the lower region that actually holds the word.
        let regions = [
            region(0x9000, 0xa000, VM_PROT_WRITE, VM_COMMIT_COMMITTED),
            region(0x2000, 0x3000, VM_PROT_WRITE, VM_COMMIT_COMMITTED),
            region(0x1000, 0x2000, VM_PROT_WRITE, VM_COMMIT_COMMITTED),
        ];
        assert!(!writable_committed_regions_cover(&regions, 0x2ffe, 4));
        assert!(writable_committed_regions_cover(&regions, 0x1ffe, 4));
        assert!(writable_committed_regions_cover(&regions, 0x2ffc, 4));
        assert!(!writable_committed_regions_cover(&regions, 0x3000, 4));
    }

    #[test]
    fn child_tid_span_rejects_reserved_or_read_only_vmas() {
        assert!(!writable_committed_regions_cover(
            &[region(0x1000, 0x2000, VM_PROT_WRITE, VM_COMMIT_RESERVED)],
            0x1000,
            4,
        ));
        assert!(!writable_committed_regions_cover(
            &[region(0x1000, 0x2000, VM_PROT_READ, VM_COMMIT_COMMITTED)],
            0x1000,
            4,
        ));
        assert!(!writable_committed_regions_cover(
            &[region(0x1000, u64::MAX, VM_PROT_WRITE, VM_COMMIT_COMMITTED)],
            u64::MAX - 2,
            4,
        ));
    }

    #[test]
    fn tid_pointers_must_fit_entire_u32_inside_user_window() {
        assert!(user_tid_span_in_window(PROC_BROKER_USER_SPACE_END_EXCLUSIVE - 4));
        assert!(!user_tid_span_in_window(PROC_BROKER_USER_SPACE_END_EXCLUSIVE - 2));
        assert!(!user_tid_span_in_window(u64::MAX - 1));

        let mut args = RustosProcForkBrokerArgs {
            clone_flags: linux_abi::SIGCHLD | linux_abi::CLONE_CHILD_CLEARTID,
            ctid_ptr: PROC_BROKER_USER_SPACE_END_EXCLUSIVE - 2,
            ..RustosProcForkBrokerArgs::default()
        };
        assert!(!valid_process_fork_plan_locally(&args));
        args.ctid_ptr = PROC_BROKER_USER_SPACE_END_EXCLUSIVE - 4;
        assert!(valid_process_fork_plan_locally(&args));

        args.clone_flags = linux_abi::CLONE_VM
            | linux_abi::CLONE_VFORK
            | linux_abi::CLONE_PARENT_SETTID
            | linux_abi::SIGCHLD;
        args.ptid_ptr = PROC_BROKER_USER_SPACE_END_EXCLUSIVE - 2;
        assert!(!valid_process_fork_plan_locally(&args));
        args.ptid_ptr = PROC_BROKER_USER_SPACE_END_EXCLUSIVE - 4;
        assert!(valid_process_fork_plan_locally(&args));
    }
}

/// Names the step of a fork that failed (1 COW clone, 2 pager VMA inheritance,
/// 3 child-tid store, 5+ the parent VMA hold with its `PagerVmaError`, 20 no
/// process slot after the reclaim wait, 21 the child vanished before its tid
/// store, 22 the child could not be activated, 23 the scheduler refused the
/// child task (slot table, Job process/thread/kmem limit), 24 inherited
/// service handles could not be acquired, 25 the calling thread already timed
/// out or began exiting before child activation), so a bare errno under
/// concurrent address-space churn is attributable. Bounded.
fn source_thread_waiting_for_fork_reply(source_pid: u64, source_tid: u64) -> bool {
    !multitask::is_user_process_exiting(source_pid)
        && matches!(
            multitask::linux_thread_runnable_by_ids(source_pid, source_tid),
            Some(false)
        )
}

struct ForkHandlePinGuard {
    pid: u64,
}

impl ForkHandlePinGuard {
    fn new(pid: u64) -> Self {
        Self { pid }
    }
}

impl Drop for ForkHandlePinGuard {
    fn drop(&mut self) {
        let deferred = multitask::with_process_state_by_pid_mut(self.pid, |state| {
            state.end_fork_handle_pin()
        })
        .or_else(|| {
            multitask::with_process_state_for_teardown_mut(self.pid, |state| {
                state.end_fork_handle_pin()
            })
        })
        .unwrap_or_default();
        if !deferred.is_empty() {
            super::service_ops::release_closed_handles(deferred);
        }
    }
}

fn fork_stage_failed(stage: u64, errno: i64, detail: u64) -> u64 {
    static RECORDED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    // ORDERING: Relaxed; a counter bounding telemetry volume.
    if RECORDED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 32 {
        debug::record_milestone(
            debug::LogCategory::Compat,
            "linux-fork-stage",
            (stage << 32) | (errno.unsigned_abs() & 0xffff_ffff),
            detail,
        );
    }
    linux_errno(errno)
}
