pub use crate::multitask::SyscallUserSimdSnapshot;
pub use crate::multitask::{
    CurrentKernelStackScope, CurrentUserSnapshot, DEFAULT_USER_TASK_WEIGHT_MICROS,
    MAX_SCHEDULER_TASKS, RetainedCurrentUserAddressSpace, RetainedCurrentUserProcessState,
    SpawnTaskError, Thread, UserFaultDisposition, UserStackState, UserTaskBootstrap,
    UserTaskRegisters, WaitChildResult,
};
pub use crate::multitask::{
    activate_suspended_user_task, arm_block_current_task, block_current_task,
    block_current_user_task, cancel_block_current_task, commit_block_current_task,
    current_linux_thread_state, current_user_address_space, current_user_process_thread_count,
    current_user_stack_state, current_user_thread_id, demote_current_user_task_to_user_class,
    exec_current_user_process, exec_user_process_by_pid, exit_current_user_process,
    exit_current_user_task, inherit_ipc_priority, inherit_ipc_priority_for_process,
    is_user_process_exiting, is_user_task_alive, linux_thread_snapshot_by_ids,
    mark_user_process_exiting, mark_user_process_exiting_once, note_process_exit_status,
    queue_linux_signal, release_ipc_priorities_for_process, release_ipc_priority,
    set_next_latency_pick_hint, set_next_pick_hint, set_next_process_pick_hint,
    set_next_spawn_pick_hint, spawn_user_process_state_with_parent, spawn_user_process_with_parent,
    spawn_user_thread_suspended, terminate_user_process, terminate_user_task, wake_task,
    wake_user_task, with_current_mm, with_current_process_state, with_current_process_state_mut,
    with_current_user_linux_state_mut, with_current_user_process_and_linux_thread_state_mut,
    with_current_user_process_state, with_process_state_by_pid, with_process_state_by_pid_mut,
};
pub use crate::user::abi::UserAbi;
pub use crate::user::epoll::{EpollError, EpollHandle, EpollInterestSnapshot};
pub use crate::user::handles::{
    ConsoleHandle, ConsoleStreamKind, DisplaySurfaceHandle, FD_CLOEXEC, FIRST_DYNAMIC_FD,
    FileHandleSeekError, FileHandleSeekWhence, HandleEntry, HandleTable, InetSocketHandle,
    IpcTransferRegistryError, KernelHandle, MAX_DYNAMIC_FD, RemoteVfsHandle, RemoteVfsHandleKind,
    TransferredHandleEntry, VfsDirectoryEntry, VfsDirectoryEntryKind, VfsDirectoryHandle,
    drop_ipc_transfer_descriptors, register_ipc_transfer_entries, take_deferred_ipc_transfer_drops,
    take_ipc_transfer_entries,
};
pub use crate::user::linux::{
    LinuxMemoryMapState, LinuxProcessImageInfo, LinuxProcessLaunch, LinuxProcessState,
    LinuxRuntimeProfile, LinuxSigAction, LinuxSignalStack, LinuxTermios, LinuxThreadState,
    LinuxVma, LinuxVmaFlags, LinuxVmaName,
};
pub use crate::user::memfd::{MemfdError, MemfdHandle, MemfdMappingHold};
pub use crate::user::process_state::{ProcessSecurityContext, UserProcessState};
pub use crate::user::socket::{PassedHandle, SocketCredentials, SocketError, SocketHandle};
pub use x86_64::VirtAddr;

pub fn current_console_session() -> Option<kernel_object::api::session::ConsoleSessionHandle> {
    crate::multitask::current_console_session()
}

pub fn clear_deferred_reschedule_request() {
    crate::multitask::clear_deferred_reschedule_request();
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
        ProcessSecurityContext, SpawnTaskError, UserProcessState, UserTaskBootstrap, VirtAddr,
    };

    pub fn spawn_user_process(
        address_space: crate::memory::paging::ProcessAddressSpace,
        bootstrap: UserTaskBootstrap,
        weight_micros: u64,
    ) -> Result<u64, SpawnTaskError> {
        crate::multitask::spawn_user_process(address_space, bootstrap, weight_micros)
    }

    pub fn spawn_user_process_without_deferred_reschedule(
        address_space: crate::memory::paging::ProcessAddressSpace,
        bootstrap: UserTaskBootstrap,
        weight_micros: u64,
    ) -> Result<u64, SpawnTaskError> {
        crate::multitask::spawn_user_process_without_deferred_reschedule(
            address_space,
            bootstrap,
            weight_micros,
        )
    }

    pub fn spawn_user_process_suspended(
        address_space: crate::memory::paging::ProcessAddressSpace,
        bootstrap: UserTaskBootstrap,
        weight_micros: u64,
    ) -> Result<u64, SpawnTaskError> {
        crate::multitask::spawn_user_process_suspended(address_space, bootstrap, weight_micros)
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

    pub fn block_current_task() -> bool {
        crate::multitask::block_current_task()
    }

    pub fn arm_block_current_task() -> bool {
        crate::multitask::arm_block_current_task()
    }

    pub fn cancel_block_current_task() -> bool {
        crate::multitask::cancel_block_current_task()
    }

    pub fn commit_block_current_task() -> Option<bool> {
        crate::multitask::commit_block_current_task()
    }

    pub fn wake_task(task_id: u64) -> bool {
        crate::multitask::wake_task(task_id)
    }
}

pub mod wait {
    use super::WaitChildResult;

    pub fn wait_for_child(parent_process_id: u64, target_pid: i64) -> WaitChildResult {
        crate::multitask::wait_for_child(parent_process_id, target_pid)
    }
}

pub use boot::{is_initialized, service_deferred_work, start};
pub use fault::{halt_current_retired_task, retire_current_user_task_due_to_fault};
pub use process::{spawn_kernel_process, spawn_user_process};
pub use snapshot::{
    any_user_process_state, current_task_id, current_user_abi, current_user_id,
    current_user_log_ids, current_user_process_id, current_user_snapshot, parent_process_id_of,
    retain_current_user_process_state, user_log_ids_for_task, with_current_process_credentials,
    with_current_user_process_state_mut,
};
pub use task::{
    rtc_interrupt_handler_addr, software_schedule_interrupt_handler_addr,
    timer_interrupt_handler_addr, yield_now,
};
pub use wait::wait_for_child;
