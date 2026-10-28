// RING3-MIGRATION-REFERENCE START: capability-broker exception: procd/rootd
// own lifecycle event routing and restart policy. Ring0 keeps capability-gated
// per-consumer exit-event fan-out and collection substrate.
use super::*;

use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY, IPC_SERVICE_CAP_PROCESS_POLICY,
    IPC_SERVICE_CAP_ROOT_SUPERVISOR, LIFECYCLE_DRAIN_BROKER_ABI_VERSION, LifecycleDrainBrokerArgs,
    ROOTD_MEMORY_BROKER_ABI_VERSION, ROOTD_MEMORY_CENSUS_MAX_ENTRIES,
    ROOTD_TERMINATE_BROKER_ABI_VERSION, RootdMemoryCensusEntry, RustosRootdMemoryBrokerArgs,
    RustosRootdTerminateBrokerArgs,
};

/// A supervisor may defer a restart but must not turn the timer substrate into
/// an unbounded privileged sleep primitive. The backoff value is published by
/// rootd and deliberately fits below this cap.
const ROOTD_WAIT_MAX_MILLIS: u64 = 1_000;
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
        // The exact producer elected the last process member and sealed its
        // first fault. Do not infer this from a racy exception-time census or
        // execute potentially blocking close/policy work with exception IF=0.
        if let Some(vector) = retired_process_fault_vector(
            cleanup.process_terminal(), cleanup.user_fault_vector(),
        ) {
            super::record_linux_process_fault_termination(cleanup.process_id(), vector);
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

fn retired_process_fault_vector(process_terminal: bool, user_fault_vector: Option<u8>) -> Option<u8> {
    if process_terminal { user_fault_vector } else { None }
}

fn rootd_wait_delay_is_valid(millis: u64) -> bool {
    (1..=ROOTD_WAIT_MAX_MILLIS).contains(&millis)
}

fn rootd_terminate_args_are_valid(args: &RustosRootdTerminateBrokerArgs) -> bool {
    args.abi_version == ROOTD_TERMINATE_BROKER_ABI_VERSION
        && args.reserved0 == 0
        && args.flags == 0
        && args.target_pid != 0
}

fn lifecycle_drain_args_are_valid(args: &LifecycleDrainBrokerArgs) -> bool {
    args.abi_version == LIFECYCLE_DRAIN_BROKER_ABI_VERSION
        && args.reserved0 == 0
        && args.reserved1 == 0
        && args.reserved2 == 0
}

pub(super) fn syscall_linux_rustos_lifecycle_drain_broker(args_ptr: u64) -> u64 {
    let is_process_policy =
        ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_PROCESS_POLICY);
    let is_root_supervisor =
        ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_ROOT_SUPERVISOR);
    let is_linux_syscall_policy =
        ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY);
    if !is_process_policy && !is_root_supervisor && !is_linux_syscall_policy {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<LifecycleDrainBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !lifecycle_drain_args_are_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    let consumer = if is_root_supervisor {
        offload_ops::LifecycleConsumer::RootSupervisor
    } else if is_linux_syscall_policy {
        offload_ops::LifecycleConsumer::LinuxSyscallPolicy
    } else {
        offload_ops::LifecycleConsumer::ProcessPolicy
    };
    match offload_ops::drain_lifecycle_events(&args, consumer) {
        Ok(count) => count,
        Err(errno) => linux_errno(errno),
    }
}

/// Sleep the calling root supervisor for one bounded restart-backoff interval.
/// The kernel does not select a delay, retain retry state, or restart a
/// process: those remain rootd policy. Keeping this timer primitive here
/// avoids depending on syscalld while syscalld itself is being recovered.
pub(super) fn syscall_linux_rustos_rootd_wait_broker(millis: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_ROOT_SUPERVISOR) {
        return linux_errno(LINUX_EPERM);
    }
    if !rootd_wait_delay_is_valid(millis) {
        return linux_errno(LINUX_EINVAL);
    }
    crate::arch::rtc::sleep(millis);
    0
}

/// Finalize one rootd-selected service process.  Rootd decides whether a
/// lease is stale; this substrate only verifies root-supervisor authority and
/// performs the complete process teardown in one non-yielding syscall turn.
pub(super) fn syscall_linux_rustos_rootd_terminate_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_ROOT_SUPERVISOR) {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosRootdTerminateBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !rootd_terminate_args_are_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    if multitask::current_user_process_id() == Some(args.target_pid) {
        return linux_errno(LINUX_EPERM);
    }
    let parent_pid = multitask::parent_process_id_of(args.target_pid).unwrap_or(0);
    // The scheduler marks the process exiting before retiring any sibling, so
    // no new thread or process-scoped authority can enter during teardown.
    match multitask::terminate_user_process_once(args.target_pid) {
        multitask::TerminateOutcome::Absent => return linux_errno(LINUX_ESRCH),
        // Another terminator (the memory-pressure watchdog) published this
        // termination first and owns its once-per-process follow-up. Running it
        // again would record a second exit and signal the parent twice.
        multitask::TerminateOutcome::AlreadyQueued => {
            crate::oom_watchdog::note_rootd_termination();
            return 0;
        }
        multitask::TerminateOutcome::Started => {}
    }
    crate::oom_watchdog::note_rootd_termination();
    finalize_terminated_process(args.target_pid, parent_pid);
    0
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
    let _ = multitask::note_process_exit_status(process_id, exit_status);
    offload_ops::record_process_exit(process_id, parent_pid, exit_status);
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
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_ROOT_SUPERVISOR) {
        return linux_errno(LINUX_EPERM);
    }
    if let Err(errno) = memory_census_fault_gate(
        nucleus_core::util::fault_injection::should_fail("rootd.memory.census"),
    ) {
        return linux_errno(errno);
    }
    let args = match usermem::read_current_user_struct::<RustosRootdMemoryBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !rootd_memory_args_are_valid(&args) {
        return linux_errno(LINUX_EINVAL);
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
    match usermem::write_current_user_struct(args_ptr, &reply) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MemoryPressureStats, build_memory_reply, lifecycle_drain_args_are_valid,
        memory_census_fault_gate,
        retired_process_fault_vector, rootd_memory_args_are_valid, rootd_terminate_args_are_valid,
        rootd_wait_delay_is_valid,
    };
    use crate::multitask;
    use rustos_user_abi::syscall::{
        LIFECYCLE_DRAIN_BROKER_ABI_VERSION, LifecycleDrainBrokerArgs,
        ROOTD_MEMORY_BROKER_ABI_VERSION, ROOTD_MEMORY_CENSUS_MAX_ENTRIES,
        ROOTD_TERMINATE_BROKER_ABI_VERSION, RustosRootdMemoryBrokerArgs,
        RustosRootdTerminateBrokerArgs,
    };

    #[test]
    fn only_original_final_fault_token_commits_process_termination() {
        assert_eq!(retired_process_fault_vector(false, None), None);
        assert_eq!(retired_process_fault_vector(false, Some(14)), None);
        assert_eq!(retired_process_fault_vector(true, None), None);
        assert_eq!(retired_process_fault_vector(true, Some(14)), Some(14));
        assert_eq!(retired_process_fault_vector(true, Some(13)), Some(13));
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
    fn rootd_wait_delay_is_strictly_bounded() {
        assert!(!rootd_wait_delay_is_valid(0));
        assert!(rootd_wait_delay_is_valid(250));
        assert!(rootd_wait_delay_is_valid(1_000));
        assert!(!rootd_wait_delay_is_valid(1_001));
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
