// RING3-MIGRATION-REFERENCE START: capability-broker exception: procd/rootd
// own lifecycle event routing and restart policy. Ring0 keeps capability-gated
// per-consumer exit-event fan-out and collection substrate.
use super::*;

use rustos_user_abi::syscall::{ServiceCapability,
    LIFECYCLE_BROKER_MODE_DRAIN, LIFECYCLE_BROKER_MODE_PROBE_PIDS,
    LIFECYCLE_DRAIN_BROKER_ABI_VERSION, LIFECYCLE_DRAIN_MAX_EVENTS, LifecycleDrainBrokerArgs,
    LifecyclePidProbeWire,
    ROOTD_MEMORY_BROKER_ABI_VERSION, ROOTD_MEMORY_CENSUS_MAX_ENTRIES,
    ROOTD_TERMINATE_BROKER_ABI_VERSION, RootdMemoryCensusEntry, RustosRootdMemoryBrokerArgs,
    RustosRootdTerminateBrokerArgs,
};

pub(crate) const RETIRED_TASK_CLEANUP_BUDGET: usize = 4;

/// Remove every task-scoped runtime registration that lives outside the
/// scheduler before its retired slot may be recycled. The caller supplies the
/// scheduler-stamped identities; no registry is allowed to infer ownership
/// from a slot number or a possibly requeued wait key.
pub(crate) fn cleanup_retired_task_runtime_state(
    task_id: u64,
    process_id: u64,
    process_terminal: bool,
    clear_child_tid: u64,
    robust_list_head: u64,
    robust_list_len: u64,
) -> usize {
    let mut removed = 0usize;
    removed += service_ops::futex_thread::cleanup_retired_linux_thread_state(
        task_id,
        process_id,
        clear_child_tid,
        robust_list_head,
        robust_list_len,
    );
    removed += service_ops::futex_thread::cleanup_retired_task_waiter(task_id) as usize;
    removed += waitset_broker_ops::remove_waitset_waiters(task_id);
    removed += ipc_ops::remove_service_endpoint_waiter(task_id);
    removed += kernel_io_manager::api::input::transport::disarm_input_waiter(task_id) as usize;
    removed +=
        kernel_io_manager::api::input::transport::disarm_inputd_ingestion_waiter(task_id) as usize;
    removed += kernel_io_manager::api::block::disarm_dvm_waiter(task_id) as usize;
    removed += kernel_io_manager::api::tty::disarm_input_waiter(task_id) as usize;
    if process_id != 0 {
        let (tickets, transitions) =
            proc_broker_ops::cleanup_proc_broker_exec_state_for_thread(process_id, task_id);
        removed += tickets + transitions;
        if process_terminal {
            release_all_service_handle_refs(process_id);
            ipc_ops::cleanup_service_endpoints_for_process(process_id);
            let (prepares, tickets, transitions) =
                proc_broker_ops::cleanup_proc_broker_state_for_process(process_id);
            removed += prepares + tickets + transitions;
        }
    }
    removed
}

/// Complete a bounded cohort of scheduler-stamped runtime cleanup tokens.
/// The acknowledgement is the linearization point that permits later task
/// slot and process-object reclamation; a mismatched token is fatal rather
/// than silently pinning a finite process-table slot forever.
pub(crate) fn service_retired_task_runtime_cleanup(limit: usize) -> usize {
    let mut work = 0;
    for _ in 0..limit {
        let Some(cleanup) = multitask::next_retired_task_cleanup() else {
            break;
        };
        // The exact producer either won the process's first-wins fault seal
        // (a fatal fault terminates the whole thread group, whichever thread
        // took it) or elected the last process member after an unsealed fault.
        // Never infer this from a racy exception-time census or execute
        // potentially blocking close/policy work with exception IF=0.
        if let Some(vector) = retired_process_fault_vector(
            cleanup.process_terminal(), cleanup.user_fault_vector(), cleanup.fault_status_sealed(),
        ) {
            super::record_linux_process_fault_termination(
                cleanup.process_id(), vector, cleanup.fault_status_sealed(),
            );
        }
        work += cleanup_retired_task_runtime_state(
            cleanup.task_id(),
            cleanup.process_id(),
            cleanup.process_terminal(),
            cleanup.clear_child_tid(),
            cleanup.robust_list_head(),
            cleanup.robust_list_len(),
        );
        if !multitask::complete_retired_task_cleanup(cleanup) {
            panic!(
                "retired task cleanup acknowledgement lost: task_id={} process_id={}",
                cleanup.task_id(),
                cleanup.process_id()
            );
        }
        work += 1;
    }
    work
}

fn retired_process_fault_vector(
    process_terminal: bool,
    user_fault_vector: Option<u8>,
    fault_status_sealed: bool,
) -> Option<u8> {
    if process_terminal || fault_status_sealed { user_fault_vector } else { None }
}

fn rootd_terminate_args_are_valid(args: &RustosRootdTerminateBrokerArgs) -> bool {
    args.abi_version == ROOTD_TERMINATE_BROKER_ABI_VERSION
        && args.reserved0 == 0
        && args.flags == 0
        && args.target_pid != 0
}

fn lifecycle_drain_args_are_valid(args: &LifecycleDrainBrokerArgs) -> bool {
    args.abi_version == LIFECYCLE_DRAIN_BROKER_ABI_VERSION
        && matches!(args.mode, LIFECYCLE_BROKER_MODE_DRAIN | LIFECYCLE_BROKER_MODE_PROBE_PIDS)
        && args.reserved1 == 0
        && args.reserved2 == 0
}

// BEGIN lifecycle-pid-probe
/// Shape of a `LIFECYCLE_BROKER_MODE_PROBE_PIDS` request (mode and version are
/// checked by [`lifecycle_drain_args_are_valid`]).
fn lifecycle_probe_args_are_valid(args: &LifecycleDrainBrokerArgs) -> bool {
    args.mode == LIFECYCLE_BROKER_MODE_PROBE_PIDS
        && args.out_capacity != 0
        && args.out_capacity as usize <= LIFECYCLE_DRAIN_MAX_EVENTS
        && args.out_events_ptr != 0
        && args.out_count_ptr != 0
}

/// Identity of the live process `pid`: its start time (kept across `exec`),
/// forced non-zero; `0` for a pid that is absent or already exiting. The
/// process-table accessor only sees a visible (not yet sealed for exit)
/// process, which is exactly "its exit event has not been produced yet or is
/// being produced right now".
fn process_incarnation(pid: u64) -> u64 {
    if pid == 0 {
        return 0;
    }
    multitask::with_process_state_by_pid(pid, |state| state.start_ns().max(1)).unwrap_or(0)
}

/// Fills in every entry's incarnation; returns how many processes are live.
fn fill_probe_incarnations(
    entries: &mut [LifecyclePidProbeWire],
    incarnation_of: impl Fn(u64) -> u64,
) -> u32 {
    let mut live = 0_u32;
    for entry in entries.iter_mut() {
        entry.incarnation = incarnation_of(entry.pid);
        live += u32::from(entry.incarnation != 0);
    }
    live
}

fn probe_lifecycle_pids(args: &LifecycleDrainBrokerArgs) -> Result<u64, i64> {
    if !lifecycle_probe_args_are_valid(args) {
        return Err(LINUX_EINVAL);
    }
    let count = args.out_capacity as usize;
    let stride = core::mem::size_of::<LifecyclePidProbeWire>() as u64;
    let mut entries = [LifecyclePidProbeWire::default(); LIFECYCLE_DRAIN_MAX_EVENTS];
    for (index, slot) in entries[..count].iter_mut().enumerate() {
        let ptr = args
            .out_events_ptr
            .checked_add(index as u64 * stride)
            .ok_or(LINUX_EINVAL)?;
        *slot = read_user_struct(ptr)?;
    }
    let live = fill_probe_incarnations(&mut entries[..count], process_incarnation);
    usermem::write_current_user_slice(args.out_events_ptr, &entries[..count])
        .map_err(user_pointer_error_to_linux_errno)?;
    write_user_bytes(args.out_count_ptr, &live.to_le_bytes())?;
    Ok(u64::from(live))
}
// END lifecycle-pid-probe

pub(super) fn syscall_linux_rustos_lifecycle_drain_broker(args_ptr: u64) -> u64 {
    linux_result((|| {
        let is_process_policy =
            ipc_ops::current_process_has_service_capability(ServiceCapability::ProcessPolicy);
        let is_root_supervisor =
            ipc_ops::current_process_has_service_capability(ServiceCapability::RootSupervisor);
        let is_linux_syscall_policy =
            ipc_ops::current_process_has_service_capability(ServiceCapability::LinuxSyscallPolicy);
        let is_vfs_policy =
            ipc_ops::current_process_has_service_capability(ServiceCapability::VfsPolicy);
        if !is_process_policy
            && !is_root_supervisor
            && !is_linux_syscall_policy
            && !is_vfs_policy
        {
            return Err(LINUX_EPERM);
        }
        let args: LifecycleDrainBrokerArgs = read_user_struct(args_ptr)?;
        if !lifecycle_drain_args_are_valid(&args) {
            return Err(LINUX_EINVAL);
        }
        if args.mode == LIFECYCLE_BROKER_MODE_PROBE_PIDS {
            // Liveness is vfsd's lock-owner evidence; no other consumer has a use
            // for it, so a capability held only for a drain gets nothing here.
            if !is_vfs_policy {
                return Err(LINUX_EPERM);
            }
            return probe_lifecycle_pids(&args);
        }
        let consumer = if is_root_supervisor {
            offload_ops::LifecycleConsumer::RootSupervisor
        } else if is_linux_syscall_policy {
            offload_ops::LifecycleConsumer::LinuxSyscallPolicy
        } else if is_process_policy {
            offload_ops::LifecycleConsumer::ProcessPolicy
        } else {
            offload_ops::LifecycleConsumer::VfsPolicy
        };
        offload_ops::drain_lifecycle_events(&args, consumer)
    })())
}

/// Finalize one rootd-selected service process.  Rootd decides whether a
/// lease is stale; this substrate only verifies root-supervisor authority and
/// performs the complete process teardown in one non-yielding syscall turn.
pub(super) fn syscall_linux_rustos_rootd_terminate_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let args: RustosRootdTerminateBrokerArgs =
            require_capability_and_read_args(ServiceCapability::RootSupervisor, args_ptr)?;
        if !rootd_terminate_args_are_valid(&args) {
            return Err(LINUX_EINVAL);
        }
        if multitask::current_user_process_id() == Some(args.target_pid) {
            return Err(LINUX_EPERM);
        }
        let parent_pid = multitask::parent_process_id_of(args.target_pid).unwrap_or(0);
        // The scheduler marks the process exiting before retiring any sibling, so
        // no new thread or process-scoped authority can enter during teardown.
        match multitask::terminate_user_process_once(args.target_pid) {
            multitask::TerminateOutcome::Absent => return Err(LINUX_ESRCH),
            // Another terminator (the memory-pressure watchdog) published this
            // termination first and owns its once-per-process follow-up. Running it
            // again would record a second exit and signal the parent twice.
            multitask::TerminateOutcome::AlreadyQueued => {
                crate::oom_watchdog::note_rootd_termination();
                return Ok(());
            }
            multitask::TerminateOutcome::Started => {}
        }
        crate::oom_watchdog::note_rootd_termination();
        finalize_terminated_process(args.target_pid, parent_pid);
        Ok(())
    })())
}

/// Everything that follows `terminate_user_process` succeeding: the revocation
/// and the terminal status that waiters and lifecycle consumers observe. Shared
/// by the root supervisor's terminate call and the ring0 memory-pressure
/// watchdog so both end a process identically.
fn finalize_terminated_process(process_id: u64, parent_pid: u64) {
    release_all_service_handle_refs(process_id);
    ipc_ops::cleanup_service_endpoints_for_process(process_id);
    let _ = super::super::cleanup_proc_broker_state_for_process(process_id);
    // A fixed SIGKILL status prevents rootd policy from forging application
    // exit codes while still giving waiters and lifecycle consumers a terminal
    // result they can reason about.
    let exit_status = linux_abi::SIGKILL as i32;
    // Lifecycle EXIT first: policy services must see the exit no later than a
    // `wait4` caller (same ordering as `finish_linux_process_termination`).
    offload_ops::record_process_exit(process_id, parent_pid, exit_status);
    let _ = multitask::note_process_exit_status(process_id, exit_status);
    if parent_pid != 0 {
        multitask::queue_linux_process_sigchld(
            parent_pid,
            rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_EXIT,
        );
    }
}

/// Ends `process_id` on behalf of the memory-pressure watchdog. Normal task
/// context only. False means the process was already gone.
pub(crate) fn terminate_process_once_and_finalize(process_id: u64) -> bool {
    let parent_pid = multitask::parent_process_id_of(process_id).unwrap_or(0);
    // Only the terminator that published the termination finalizes it, so a
    // root supervisor kill racing this one cannot record the exit twice.
    if multitask::terminate_user_process_once(process_id) != multitask::TerminateOutcome::Started {
        return false;
    }
    finalize_terminated_process(process_id, parent_pid);
    true
}

fn rootd_memory_args_are_valid(args: &RustosRootdMemoryBrokerArgs) -> bool {
    args.abi_version == ROOTD_MEMORY_BROKER_ABI_VERSION
        && args.reserved0 == 0
        && args.flags == 0
        && args.reserved1 == 0
        && args.entry_count == 0
}

/// The injected-failure boundary of the census call: when active, `rootd`
/// receives `EIO` and is blind to memory pressure exactly as a wedged
/// supervisor would be, which is how the ring0 watchdog is exercised.
fn memory_census_fault_gate(injected_failure: bool) -> Result<(), i64> {
    if injected_failure {
        Err(LINUX_EIO)
    } else {
        Ok(())
    }
}

/// Page counts of the physical allocator, in 4 KiB pages.
struct MemoryPressureStats {
    usable_pages: u64,
    free_pages: u64,
    min_free_pages: u64,
    watermark_denials: u64,
}

fn build_memory_reply(
    stats: &MemoryPressureStats,
    census: &[multitask::MemoryCensusEntry],
) -> RustosRootdMemoryBrokerArgs {
    let mut reply = RustosRootdMemoryBrokerArgs {
        abi_version: ROOTD_MEMORY_BROKER_ABI_VERSION,
        usable_pages: stats.usable_pages,
        free_pages: stats.free_pages,
        min_free_pages: stats.min_free_pages,
        watermark_denials: stats.watermark_denials,
        ..RustosRootdMemoryBrokerArgs::default()
    };
    let count = census.len().min(ROOTD_MEMORY_CENSUS_MAX_ENTRIES);
    for (slot, entry) in reply.entries.iter_mut().zip(&census[..count]) {
        *slot = RootdMemoryCensusEntry {
            process_id: entry.process_id,
            parent_process_id: entry.parent_process_id,
            resident_pages: entry.resident_pages,
            job: entry.job,
        };
    }
    reply.entry_count = count as u32;
    reply
}

/// Report memory pressure and the largest live processes to the root
/// supervisor. Ring0 only measures: which process, if any, to terminate is
/// rootd policy, enacted through `SYS_RUSTOS_ROOTD_TERMINATE_BROKER`.
pub(super) fn syscall_linux_rustos_rootd_memory_broker(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        require_service_capability(ServiceCapability::RootSupervisor)?;
        memory_census_fault_gate(
            nucleus_core::util::fault_injection::should_fail("rootd.memory.census"),
        )?;
        let args: RustosRootdMemoryBrokerArgs = read_user_struct(args_ptr)?;
        if !rootd_memory_args_are_valid(&args) {
            return Err(LINUX_EINVAL);
        }
        let page_bytes = rustos_user_abi::pager::PAGER_PAGE_BYTES;
        let stats = MemoryPressureStats {
            usable_pages: crate::memory::phys::usable_bytes() / page_bytes,
            free_pages: crate::memory::phys::free_bytes() / page_bytes,
            min_free_pages: crate::memory::phys::min_free_bytes() / page_bytes,
            watermark_denials: crate::memory::phys::user_watermark_denials(),
        };
        let mut census = [multitask::MemoryCensusEntry::default(); ROOTD_MEMORY_CENSUS_MAX_ENTRIES];
        let count = multitask::memory_census(&mut census);
        let reply = build_memory_reply(&stats, &census[..count]);
        write_user_struct(args_ptr, &reply)
    })())
}

#[cfg(test)]
mod tests {
    use super::{
        MemoryPressureStats, build_memory_reply, fill_probe_incarnations,
        lifecycle_drain_args_are_valid, lifecycle_probe_args_are_valid,
        memory_census_fault_gate, process_incarnation, retired_process_fault_vector,
        rootd_memory_args_are_valid, rootd_terminate_args_are_valid,
    };
    use crate::multitask;
    use rustos_user_abi::syscall::{
        LIFECYCLE_BROKER_MODE_DRAIN, LIFECYCLE_BROKER_MODE_PROBE_PIDS,
        LIFECYCLE_DRAIN_BROKER_ABI_VERSION, LIFECYCLE_DRAIN_MAX_EVENTS,
        LifecycleDrainBrokerArgs, LifecyclePidProbeWire, ROOTD_MEMORY_BROKER_ABI_VERSION,
        ROOTD_MEMORY_CENSUS_MAX_ENTRIES, ROOTD_TERMINATE_BROKER_ABI_VERSION,
        RustosRootdMemoryBrokerArgs, RustosRootdTerminateBrokerArgs,
    };

    #[test]
    fn only_original_final_or_sealed_fault_token_commits_process_termination() {
        assert_eq!(retired_process_fault_vector(false, None, false), None);
        assert_eq!(retired_process_fault_vector(false, Some(14), false), None);
        assert_eq!(retired_process_fault_vector(true, None, false), None);
        assert_eq!(retired_process_fault_vector(true, Some(14), false), Some(14));
        assert_eq!(retired_process_fault_vector(true, Some(13), false), Some(13));
        // A non-final faulting thread that won the first-wins seal publishes the
        // status for the whole group; one that lost it publishes nothing.
        assert_eq!(retired_process_fault_vector(false, Some(14), true), Some(14));
        assert_eq!(retired_process_fault_vector(false, None, true), None);
    }

    #[test]
    fn lifecycle_drain_requires_exact_version_zero_reserved_envelope() {
        let valid = LifecycleDrainBrokerArgs {
            abi_version: LIFECYCLE_DRAIN_BROKER_ABI_VERSION,
            ..LifecycleDrainBrokerArgs::default()
        };
        assert!(lifecycle_drain_args_are_valid(&valid));
        assert!(!lifecycle_drain_args_are_valid(&LifecycleDrainBrokerArgs {
            abi_version: valid.abi_version.wrapping_add(1),
            ..valid
        }));
        assert!(!lifecycle_drain_args_are_valid(&LifecycleDrainBrokerArgs {
            reserved2: 1,
            ..valid
        }));
        assert!(!lifecycle_drain_args_are_valid(&LifecycleDrainBrokerArgs {
            reserved1: 1,
            ..valid
        }));
        assert!(!lifecycle_drain_args_are_valid(&LifecycleDrainBrokerArgs {
            mode: 2,
            ..valid
        }));
    }

    #[test]
    fn pid_probe_request_shape_is_bounded_and_needs_both_pointers() {
        let valid = LifecycleDrainBrokerArgs {
            abi_version: LIFECYCLE_DRAIN_BROKER_ABI_VERSION,
            mode: LIFECYCLE_BROKER_MODE_PROBE_PIDS,
            out_events_ptr: 0x1000,
            out_capacity: LIFECYCLE_DRAIN_MAX_EVENTS as u32,
            out_count_ptr: 0x2000,
            ..LifecycleDrainBrokerArgs::default()
        };
        assert!(lifecycle_drain_args_are_valid(&valid));
        assert!(lifecycle_probe_args_are_valid(&valid));
        for bad in [
            LifecycleDrainBrokerArgs { out_capacity: 0, ..valid },
            LifecycleDrainBrokerArgs { out_capacity: LIFECYCLE_DRAIN_MAX_EVENTS as u32 + 1, ..valid },
            LifecycleDrainBrokerArgs { out_events_ptr: 0, ..valid },
            LifecycleDrainBrokerArgs { out_count_ptr: 0, ..valid },
            LifecycleDrainBrokerArgs { mode: LIFECYCLE_BROKER_MODE_DRAIN, ..valid },
        ] {
            assert!(!lifecycle_probe_args_are_valid(&bad));
        }
    }

    #[test]
    fn pid_probe_reports_live_incarnations_and_zero_for_dead_or_pid_zero() {
        let mut entries = [
            LifecyclePidProbeWire { pid: 7, incarnation: 99 },
            LifecyclePidProbeWire { pid: 8, incarnation: 99 },
            LifecyclePidProbeWire { pid: 0, incarnation: 99 },
        ];
        let live = fill_probe_incarnations(&mut entries, |pid| match pid {
            7 => 1234,
            _ => 0,
        });
        assert_eq!(live, 1);
        // The input incarnation is never trusted or echoed.
        assert_eq!(entries.map(|entry| entry.incarnation), [1234, 0, 0]);
        assert_eq!(process_incarnation(0), 0);
    }

    #[test]
    fn rootd_memory_census_fault_blinds_the_supervisor_without_touching_state() {
        assert_eq!(memory_census_fault_gate(false), Ok(()));
        assert_eq!(memory_census_fault_gate(true), Err(super::LINUX_EIO));
    }

    #[test]
    fn memory_census_request_is_exact_version_with_empty_reply_fields() {
        let valid = RustosRootdMemoryBrokerArgs {
            abi_version: ROOTD_MEMORY_BROKER_ABI_VERSION,
            ..RustosRootdMemoryBrokerArgs::default()
        };
        assert!(rootd_memory_args_are_valid(&valid));
        for bad in [
            RustosRootdMemoryBrokerArgs {
                abi_version: 0,
                ..valid
            },
            RustosRootdMemoryBrokerArgs { flags: 1, ..valid },
            RustosRootdMemoryBrokerArgs {
                reserved0: 1,
                ..valid
            },
            RustosRootdMemoryBrokerArgs {
                reserved1: 1,
                ..valid
            },
            RustosRootdMemoryBrokerArgs {
                entry_count: 1,
                ..valid
            },
        ] {
            assert!(!rootd_memory_args_are_valid(&bad));
        }
    }

    #[test]
    fn memory_reply_carries_stats_and_clamps_the_census() {
        let stats = MemoryPressureStats {
            usable_pages: 1000,
            free_pages: 40,
            min_free_pages: 64,
            watermark_denials: 7,
        };
        let sample_count = ROOTD_MEMORY_CENSUS_MAX_ENTRIES + 5;
        let census: alloc::vec::Vec<multitask::MemoryCensusEntry> = (0..sample_count)
            .map(|index| multitask::MemoryCensusEntry {
                process_id: index as u64 + 1,
                parent_process_id: 1,
                resident_pages: 100 - index as u64,
                job: 1,
            })
            .collect();
        let reply = build_memory_reply(&stats, &census);
        assert_eq!(reply.abi_version, ROOTD_MEMORY_BROKER_ABI_VERSION);
        assert_eq!(
            (
                reply.usable_pages,
                reply.free_pages,
                reply.min_free_pages,
                reply.watermark_denials
            ),
            (1000, 40, 64, 7)
        );
        assert_eq!(reply.entry_count as usize, ROOTD_MEMORY_CENSUS_MAX_ENTRIES);
        assert_eq!(reply.entries[0].process_id, 1);
        assert_eq!(reply.entries[0].resident_pages, 100);
        let short = build_memory_reply(&stats, &census[..2]);
        assert_eq!(short.entry_count, 2);
        assert_eq!(short.entries[2].process_id, 0);
    }


    #[test]
    fn rootd_terminate_args_require_exact_version_nonzero_target_without_flags() {
        let valid = RustosRootdTerminateBrokerArgs {
            abi_version: ROOTD_TERMINATE_BROKER_ABI_VERSION,
            target_pid: 7,
            ..RustosRootdTerminateBrokerArgs::default()
        };
        assert!(rootd_terminate_args_are_valid(&valid));
        assert!(!rootd_terminate_args_are_valid(
            &RustosRootdTerminateBrokerArgs::default()
        ));
        assert!(!rootd_terminate_args_are_valid(
            &RustosRootdTerminateBrokerArgs { flags: 1, ..valid }
        ));
    }
}
// RING3-MIGRATION-REFERENCE END: procd/rootd-owned lifecycle broker substrate exception.
