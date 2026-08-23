//! Public scheduler, process, address-space, and user-state substrate API.
//!
//! - **Owner:** `kernel-ps`; cross-crate kernel callers use this facade.
//! - **Boundary:** Exported task/process IDs never bypass generation, current
//!   context, or lifecycle admission.
//! - **Lifecycle:** APIs expose explicit create/publish, arm/commit/wake,
//!   exec/exit, cleanup acknowledgement, and reclaim operations.
//! - **Concurrency:** Each function documents interrupt, lock, blocking, and
//!   callback context and adds no hidden policy-service wait.
//! - **Failure:** Stale or missing state fails without manufacturing exit,
//!   readiness, or authority.
//! - **Forbidden:** No private scheduler reach-through, split block/yield, or
//!   service policy in ring0.
//! - **Evidence:** `scheduler-lifecycle`,
//!   `process-address-space-lifecycle`, `syscall-simd-lifecycle`, and
//!   `user-memory-access`.
pub use crate::multitask::ProcessIdentity;
pub use crate::multitask::{
    AffinityCommit, AffinityError, CurrentKernelStackScope, CurrentUserSnapshot,
    DEFAULT_USER_TASK_WEIGHT_MICROS, MAX_SCHEDULER_TASKS, ProcessAffinitySnapshot,
    RetainedCurrentUserAddressSpace, RetainedCurrentUserProcessState, RetiredTaskCleanup,
    SchedulingContextAdmission, SpawnTaskError, Thread, UserFaultDisposition, UserStackState,
    UserTaskBootstrap, UserTaskRegisters, WaitChildResult,
};
pub use crate::multitask::{
    activate_suspended_user_task, activate_suspended_user_tasks,
    activate_suspended_user_tasks_with_commit, arm_block_current_task,
    attach_reserved_ipc_priority, bind_ipc_priority_to_process_worker, bind_reserved_ipc_priority,
    cancel_block_current_task, cancel_ipc_priority_reservation,
    commit_block_current_task_and_yield, commit_ipc_call_handoff, complete_ipc_reply_wake_handoff,
    complete_ipc_reply_wake_handoff_with_custody, complete_retired_task_cleanup,
    current_linux_thread_state, current_thread_may_have_pending_signals,
    current_user_address_space, current_user_process_identity, current_user_process_thread_count,
    current_user_stack_state, current_user_thread_id, current_user_wait_binding,
    demote_current_user_task_to_user_class, drain_scheduler_runtime_profile,
    exec_current_user_process, exec_user_process_by_pid, exit_current_user_process,
    exit_current_user_task, inherit_ipc_priority, is_user_process_exiting, is_user_task_alive,
    linux_task_affinity, linux_thread_snapshot_by_ids, live_user_process_identity_by_pid,
    live_user_process_identity_with_exact_exec_path, mark_user_process_exiting,
    mark_user_process_exiting_once, next_retired_task_cleanup, note_process_exit_status,
    queue_linux_process_sigchld, queue_linux_signal, release_ipc_priorities_for_process,
    release_ipc_priority, reserve_ipc_call_donation, reserve_ipc_priority,
    set_current_linux_tls_fs_base, set_linux_task_affinity, set_next_latency_pick_hint,
    set_next_pick_hint, set_next_process_pick_hint, set_next_spawn_pick_hint,
    set_next_synchronous_pick_hint, set_windows_current_thread_affinity,
    set_windows_process_affinity, settle_ipc_reply_scheduling_context,
    spawn_user_process_state_with_parent, spawn_user_process_suspended_with_scheduling_context,
    spawn_user_process_with_scheduling_context,
    spawn_user_process_without_deferred_reschedule_with_scheduling_context,
    spawn_user_thread_suspended, stop_current_linux_process, task_has_system_scheduling_class,
    terminate_user_process, terminate_user_task, wake_task, wake_user_task,
    windows_process_affinity, with_current_mm, with_current_process_state,
    with_current_process_state_mut, with_current_user_linux_state_mut,
    with_current_user_process_and_linux_thread_state_mut, with_current_user_process_state,
    with_process_state_by_pid, with_process_state_by_pid_mut,
};
pub use crate::user::abi::UserAbi;
pub use crate::user::epoll::{EpollError, EpollHandle, EpollInterestSnapshot};
pub use crate::user::handles::{
    ConsoleHandle, ConsoleStreamKind, DisplaySurfaceHandle, FD_CLOEXEC, FIRST_DYNAMIC_FD,
    FileHandleSeekError, FileHandleSeekWhence, HandleEntry, HandleTable, InetSocketHandle,
    IpcTransferRegistryError, KernelHandle, MAX_DYNAMIC_FD, RemoteVfsHandle, RemoteVfsHandleKind,
    TransferredHandleEntry, VfsDirectoryEntry, VfsDirectoryEntryKind, VfsDirectoryHandle,
    bind_ipc_transfer_receiver_by_tickets, bind_ipc_transfer_tickets,
    claim_ipc_transfer_entries_by_tickets, commit_ipc_transfer_enqueue,
    drop_ipc_transfer_descriptors, drop_ipc_transfer_tickets, drop_ipc_transfers_for_service_epoch,
    reclaim_unbound_inet_socket_transfer, register_ipc_transfer_entries,
    register_new_inet_socket_transfer, take_deferred_ipc_transfer_drops, take_ipc_transfer_entries,
};

pub fn service_deferred_shared_region_reclaims(max_pages: usize) -> usize {
    kernel_ipc_runtime::api::service_deferred_shared_region_reclaims(max_pages)
}
pub use crate::user::linux::{
    LinuxMemoryMapState, LinuxProcessImageInfo, LinuxProcessLaunch, LinuxProcessState,
    LinuxRuntimeProfile, LinuxSigAction, LinuxSignalStack, LinuxTermios, LinuxThreadState,
    LinuxVma, LinuxVmaFlags, LinuxVmaName,
};
pub use crate::user::memfd::{MemfdError, MemfdHandle, MemfdMappingHold};
pub use crate::user::process_state::{
    ProcessSecurityContext, SharedFutexBackingKey, UserProcessState,
};
pub use crate::user::socket::{
    PassedHandle, SocketCredentials, SocketError, SocketHandle, SocketStreamGuard,
};
pub use x86_64::VirtAddr;

pub fn current_console_session() -> Option<kernel_object::api::session::ConsoleSessionHandle> {
    crate::multitask::current_console_session()
}

pub fn reschedule_deferred_from_interruptible_syscall() {
    crate::multitask::reschedule_deferred_from_interruptible_syscall();
}

pub fn request_deferred_reschedule() {
    crate::multitask::request_deferred_reschedule();
}

pub fn request_user_return_reschedule() {
    crate::multitask::request_user_return_reschedule();
}

pub fn reschedule_if_requested() {
    crate::multitask::reschedule_if_requested();
}

pub fn cond_resched() {
    crate::multitask::cond_resched();
}

pub fn mark_root_idle() {
    crate::multitask::mark_root_idle();
}

pub mod abi {
    pub use crate::user::abi::*;
}

pub mod epoll {
    pub use crate::user::epoll::*;
}

pub mod handles {
    pub use crate::user::handles::*;
}

pub mod linux {
    pub use crate::user::linux::*;
}

pub mod memfd {
    pub use crate::user::memfd::*;
}

pub mod process_state {
    pub use crate::user::process_state::*;
}

pub mod socket {
    pub use crate::user::socket::*;
}

pub mod syscall {
    pub use crate::user::syscall::*;
}

pub mod sysops {
    pub mod usermem {
        pub use crate::user::sysops::usermem::*;
    }
}

pub mod boot {
    pub fn start(entry: fn(u64)) -> ! {
        crate::multitask::start(entry)
    }

    pub fn start_secondary_cpu() -> ! {
        crate::multitask::start_secondary_cpu()
    }

    pub fn is_initialized() -> bool {
        crate::multitask::is_initialized()
    }

    pub fn service_deferred_work() -> usize {
        crate::multitask::service_deferred_work()
    }
}

pub mod fault {
    use super::UserFaultDisposition;

    pub fn retire_current_user_task_due_to_fault(
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rip: u64,
        rsp: u64,
    ) -> UserFaultDisposition {
        crate::multitask::retire_current_user_task_due_to_fault(vector, error_code, cr2, rip, rsp)
    }

    pub fn halt_current_retired_task() -> ! {
        crate::multitask::halt_current_retired_task()
    }
}

pub mod process {
    use super::{
        ProcessSecurityContext, SchedulingContextAdmission, SpawnTaskError, UserProcessState,
        UserTaskBootstrap, VirtAddr,
    };

    pub fn spawn_user_process_with_scheduling_context(
        address_space: crate::memory::paging::ProcessAddressSpace,
        bootstrap: UserTaskBootstrap,
        weight_micros: u64,
        admission: SchedulingContextAdmission,
    ) -> Result<u64, SpawnTaskError> {
        crate::multitask::spawn_user_process_with_scheduling_context(
            address_space,
            bootstrap,
            weight_micros,
            admission,
        )
    }

    pub fn spawn_user_process_without_deferred_reschedule_with_scheduling_context(
        address_space: crate::memory::paging::ProcessAddressSpace,
        bootstrap: UserTaskBootstrap,
        weight_micros: u64,
        admission: SchedulingContextAdmission,
    ) -> Result<u64, SpawnTaskError> {
        crate::multitask::spawn_user_process_without_deferred_reschedule_with_scheduling_context(
            address_space,
            bootstrap,
            weight_micros,
            admission,
        )
    }

    pub fn spawn_user_process_suspended_with_scheduling_context(
        address_space: crate::memory::paging::ProcessAddressSpace,
        bootstrap: UserTaskBootstrap,
        weight_micros: u64,
        admission: SchedulingContextAdmission,
    ) -> Result<u64, SpawnTaskError> {
        crate::multitask::spawn_user_process_suspended_with_scheduling_context(
            address_space,
            bootstrap,
            weight_micros,
            admission,
        )
    }

    pub fn spawn_kernel_process(
        process_state: UserProcessState,
        entry: VirtAddr,
        arg0: u64,
        weight_micros: u64,
    ) -> Result<u64, SpawnTaskError> {
        crate::multitask::spawn_kernel_process(process_state, entry, arg0, weight_micros)
    }

    pub type SecurityContext = ProcessSecurityContext;
}

pub mod snapshot {
    use super::{
        CurrentUserSnapshot, LinuxProcessState, LinuxThreadState, ProcessSecurityContext,
        RetainedCurrentUserAddressSpace, RetainedCurrentUserProcessState, UserAbi,
        UserProcessState,
    };
    use crate::memory::paging::ProcessAddressSpace;

    pub fn current_user_snapshot() -> Option<CurrentUserSnapshot> {
        crate::multitask::current_user_snapshot()
    }

    pub fn current_user_abi() -> Option<UserAbi> {
        crate::multitask::current_user_abi()
    }

    pub fn current_user_id() -> Option<u64> {
        crate::multitask::current_user_id()
    }

    pub fn current_task_id() -> Option<u64> {
        crate::multitask::current_task_id()
    }

    pub fn current_scheduling_context_runtime_snapshot()
    -> Option<crate::multitask::SchedulingContextRuntimeSnapshot> {
        crate::multitask::current_scheduling_context_runtime_snapshot()
    }

    pub fn current_user_log_ids() -> Option<(u64, u64)> {
        crate::multitask::current_user_log_ids()
    }

    pub fn user_log_ids_for_task(task_id: u64) -> Option<(u64, u64)> {
        crate::multitask::user_log_ids_for_task(task_id)
    }

    pub fn current_user_process_id() -> Option<u64> {
        crate::multitask::current_user_process_id()
    }

    pub fn parent_process_id_of(process_id: u64) -> Option<u64> {
        crate::multitask::parent_process_id_of(process_id)
    }

    pub fn retain_current_user_process_state() -> Option<RetainedCurrentUserProcessState> {
        crate::multitask::retain_current_user_process_state()
    }

    pub fn current_user_address_space() -> Option<RetainedCurrentUserAddressSpace> {
        crate::multitask::current_user_address_space()
    }

    pub fn with_current_user_process_state<R>(
        f: impl FnOnce(u64, UserAbi, &UserProcessState) -> R,
    ) -> Option<R> {
        crate::multitask::with_current_user_process_state(f)
    }

    pub fn with_current_user_process_state_mut<R>(
        f: impl FnOnce(u64, UserAbi, &mut UserProcessState) -> R,
    ) -> Option<R> {
        crate::multitask::with_current_user_process_state_mut(f)
    }

    pub fn with_current_user_linux_state_mut<R>(
        f: impl FnOnce(
            u64,
            u64,
            UserAbi,
            &mut ProcessAddressSpace,
            &mut Option<LinuxProcessState>,
            &mut Option<LinuxThreadState>,
        ) -> R,
    ) -> Option<R> {
        crate::multitask::with_current_user_linux_state_mut(f)
    }

    pub fn with_current_user_process_and_linux_thread_state_mut<R>(
        f: impl FnOnce(u64, u64, UserAbi, &mut UserProcessState, &mut Option<LinuxThreadState>) -> R,
    ) -> Option<R> {
        crate::multitask::with_current_user_process_and_linux_thread_state_mut(f)
    }

    pub fn with_current_mm<R>(f: impl FnOnce(&ProcessAddressSpace) -> R) -> Option<R> {
        crate::multitask::with_current_mm(f)
    }

    pub fn with_process_state_by_pid_mut<R>(
        process_id: u64,
        f: impl FnOnce(&mut UserProcessState) -> R,
    ) -> Option<R> {
        crate::multitask::with_process_state_by_pid_mut(process_id, f)
    }

    pub fn any_user_process_state(f: impl FnMut(u64, &UserProcessState) -> bool) -> bool {
        crate::multitask::any_user_process_state(f)
    }

    pub fn with_current_process_credentials<R>(
        f: impl FnOnce(ProcessSecurityContext) -> R,
    ) -> Option<R> {
        crate::multitask::with_current_process_credentials(f)
    }
}

pub mod task {
    pub fn timer_interrupt_handler_addr() -> u64 {
        crate::multitask::timer_interrupt_handler_addr()
    }

    pub fn rtc_interrupt_handler_addr() -> u64 {
        crate::multitask::rtc_interrupt_handler_addr()
    }

    pub fn software_schedule_interrupt_handler_addr() -> u64 {
        crate::multitask::software_schedule_interrupt_handler_addr()
    }

    pub fn yield_now() {
        crate::multitask::yield_now();
    }

    pub fn arm_block_current_task() -> bool {
        crate::multitask::arm_block_current_task()
    }

    pub fn cancel_block_current_task() -> bool {
        crate::multitask::cancel_block_current_task()
    }

    pub fn wake_task(task_id: u64) -> bool {
        crate::multitask::wake_task(task_id)
    }
}

pub mod wait {
    use super::WaitChildResult;

    pub fn wait_for_child(
        parent_process_id: u64,
        target_pid: i64,
        include_stopped: bool,
        include_continued: bool,
    ) -> WaitChildResult {
        crate::multitask::wait_for_child(
            parent_process_id,
            target_pid,
            include_stopped,
            include_continued,
        )
    }
}

pub use crate::user::sysops::{drain_user_copy_profile, force_drain_user_copy_profile};

pub use boot::{is_initialized, service_deferred_work, start, start_secondary_cpu};
pub use fault::{halt_current_retired_task, retire_current_user_task_due_to_fault};
pub use process::spawn_kernel_process;
pub use snapshot::{
    any_user_process_state, current_scheduling_context_runtime_snapshot, current_task_id,
    current_user_abi, current_user_id, current_user_log_ids, current_user_process_id,
    current_user_snapshot, parent_process_id_of, retain_current_user_process_state,
    user_log_ids_for_task, with_current_process_credentials, with_current_user_process_state_mut,
};
pub use task::{
    rtc_interrupt_handler_addr, software_schedule_interrupt_handler_addr,
    timer_interrupt_handler_addr, yield_now,
};
pub use wait::wait_for_child;
