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

pub(super) fn syscall_linux_rustos_proc_fork_broker(args_ptr: u64) -> u64 {
    if !current_process_can_policy() {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosProcForkBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
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
    let spawn_reservation = match multitask::reserve_process_spawn() {
        Some(reservation) => reservation,
        None => return linux_errno(LINUX_EAGAIN),
    };
    // One mmap-lock-equivalent transaction now covers both authorities the
    // child inherits. The process state lock stabilizes page tables while the
    // VMA writer holds every live publication at an odd sequence; concurrent
    // faults retry, and no mmap/unmap can be linearized inside this snapshot.
    let forked = multitask::with_fork_parent_state(args.source_pid, |parent, regions| {
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
            if region.object.object_type == rustos_user_abi::pager::VM_OBJECT_ANONYMOUS
                && region.sharing == rustos_user_abi::pager::VM_SHARING_PRIVATE
                && region.commit_state == rustos_user_abi::pager::VM_COMMIT_COMMITTED
            {
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
        let address_space = parent
            .address_space_mut()
            .clone_user_space_cow(&cow_ranges, &eager_private_ranges)?;
        let mut inherited_regions = Vec::new();
        inherited_regions
            .try_reserve_exact(regions.len())
            .map_err(|_| crate::memory::paging::AddressSpaceError::OutOfFrames)?;
        inherited_regions.extend_from_slice(regions);
        Ok::<_, crate::memory::paging::AddressSpaceError>((
            parent.fork_clone(address_space, None),
            inherited_regions,
        ))
    });
    let (child_state, inherited_regions) = match forked {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => {
            let _ = multitask::cancel_process_spawn(spawn_reservation);
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        Err(_) => {
            let _ = multitask::cancel_process_spawn(spawn_reservation);
            return linux_errno(LINUX_ENOMEM);
        }
    };
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
    bootstrap.logical_admin = child_state.security().is_logical_admin();
    bootstrap.linux_process_state = child_state.linux_process_state().copied();
    bootstrap.linux_memory_map = child_state.linux_memory_map().cloned();
    bootstrap.linux_runtime_profile = child_state.linux_runtime_profile().cloned();
    bootstrap.linux_thread_state = Some(child_thread_state);
    bootstrap.set_exec_path(child_state.exec_path());

    let inherited_service_refs = match acquire_cloned_service_handle_refs(&child_state) {
        Ok(refs) => refs,
        Err(errno) => {
            let _ = multitask::cancel_process_spawn(spawn_reservation);
            return linux_errno(errno);
        }
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
            release_service_handle_refs(&inherited_service_refs);
            let _ = multitask::cancel_process_spawn(spawn_reservation);
            return linux_errno(process_spawn_error_to_linux_errno(err));
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
        return linux_errno(errno);
    }
    if args.clone_flags & linux_abi::CLONE_CHILD_SETTID != 0 {
        let child_tid = (child_pid as u32).to_le_bytes();
        let write_result = multitask::with_process_state_by_pid_mut(child_pid, |child| {
            child
                .address_space()
                .copy_into_user(VirtAddr::new(args.ctid_ptr), &child_tid)
        });
        match write_result {
            Some(Ok(())) => {}
            Some(Err(err)) => {
                let _ = multitask::terminate_user_task(child_pid);
                return linux_errno(address_space_error_to_linux_errno(err));
            }
            None => {
                let _ = multitask::terminate_user_task(child_pid);
                return linux_errno(LINUX_EAGAIN);
            }
        }
    }
    if !multitask::activate_suspended_user_task(child_pid) {
        let _ = multitask::terminate_user_task(child_pid);
        return linux_errno(LINUX_EAGAIN);
    }
    multitask::set_next_spawn_pick_hint(child_pid);
    multitask::request_deferred_reschedule();
    child_pid
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
    let mut cursor = start;
    for region in regions {
        if region.end <= cursor {
            continue;
        }
        if region.start > cursor
            || region.commit_state != rustos_user_abi::pager::VM_COMMIT_COMMITTED
            || region.prot & rustos_user_abi::pager::VM_PROT_WRITE == 0
        {
            return false;
        }
        cursor = core::cmp::min(end, region.end);
        if cursor == end {
            return true;
        }
    }
    false
}

pub(super) fn valid_process_fork_plan_locally(args: &RustosProcForkBrokerArgs) -> bool {
    let supported =
        linux_abi::CSIGNAL | linux_abi::CLONE_CHILD_SETTID | linux_abi::CLONE_CHILD_CLEARTID;
    let exit_signal = args.clone_flags & linux_abi::CSIGNAL;
    args.clone_flags & !supported == 0
        && (exit_signal == 0 || exit_signal == linux_abi::SIGCHLD)
        && args.ptid_ptr == 0
        && args.tls == 0
        && (args.clone_flags & (linux_abi::CLONE_CHILD_SETTID | linux_abi::CLONE_CHILD_CLEARTID)
            == 0
            || (PROC_BROKER_USER_SPACE_BASE..PROC_BROKER_USER_SPACE_END_EXCLUSIVE)
                .contains(&args.ctid_ptr))
}

#[cfg(test)]
mod tests {
    use super::writable_committed_regions_cover;
    use rustos_user_abi::pager::{
        PagerVmRegionWire, VM_COMMIT_COMMITTED, VM_COMMIT_RESERVED, VM_PROT_READ, VM_PROT_WRITE,
    };

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
    }
}
