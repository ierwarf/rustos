pub use crate::multitask::{
    CurrentKernelStackScope, CurrentUserSnapshot, DEFAULT_USER_TASK_WEIGHT_MICROS,
    RetainedCurrentUserAddressSpace, RetainedCurrentUserProcessState, SpawnTaskError, Thread,
    UserFaultDisposition, UserStackState, UserTaskBootstrap, UserTaskRegisters, WaitChildResult,
};
pub use crate::multitask::{
    block_current_task, block_current_user_task, current_last_error, current_linux_thread_state,
    current_user_address_space, current_user_stack_state, current_user_thread_id,
    exec_current_user_process, exit_current_user_task, is_user_task_alive,
    note_process_exit_status, queue_linux_signal, restore_current_simd_state,
    save_current_simd_state, set_current_last_error, spawn_user_process_with_parent,
    spawn_user_thread, wake_task, wake_user_task, with_current_mm, with_current_process_state,
    with_current_process_state_mut, with_current_user_linux_state_mut,
    with_current_user_process_and_linux_thread_state_mut, with_current_user_process_state,
    with_process_state_by_pid_mut,
};
pub use crate::user::abi::UserAbi;
pub use crate::user::epoll::{EpollError, EpollHandle, EpollInterestSnapshot};
pub use crate::user::handles::{
    ConsoleStreamKind, DisplaySurfaceHandle, FD_CLOEXEC, FIRST_DYNAMIC_FD, FileHandleSeekError,
    FileHandleSeekWhence, HandleEntry, HandleTable, InetSocketHandle, KernelHandle,
    TransferredHandleEntry, VfsDirectoryEntry, VfsDirectoryEntryKind, VfsDirectoryHandle,
    VfsFileHandle, VfsFileObject,
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

pub fn current_console_session() -> kernel_object::api::session::ConsoleSessionHandle {
    crate::multitask::current_console_session()
}

pub fn clear_deferred_reschedule_request() {
    crate::multitask::clear_deferred_reschedule_request();
}

pub fn reschedule_if_requested() {
    crate::multitask::reschedule_if_requested();
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

    pub fn current_user_id() -> Option<u64> {
        crate::multitask::current_user_id()
    }

    pub fn current_task_id() -> Option<u64> {
        crate::multitask::current_task_id()
    }

    pub fn current_user_process_id() -> Option<u64> {
        crate::multitask::current_user_process_id()
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
    use kernel_object::api::session::ConsoleSessionHandle;

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

    pub fn wake_task(task_id: u64) -> bool {
        crate::multitask::wake_task(task_id)
    }

    pub fn register_tick_jiffies_hook(hook: fn(u64) -> u64) {
        crate::linux_runtime_hooks::register_tick_jiffies_hook(hook);
    }

    pub fn register_input_consumer_hooks(acquire: fn(), release: fn()) {
        crate::linux_runtime_hooks::register_input_consumer_hooks(acquire, release);
    }

    pub fn set_current_console_session(session: impl Into<ConsoleSessionHandle>) -> bool {
        crate::multitask::set_current_console_session(session)
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
    any_user_process_state, current_task_id, current_user_id, current_user_process_id,
    current_user_snapshot, retain_current_user_process_state, with_current_process_credentials,
    with_current_user_process_state_mut,
};
pub use task::{
    register_input_consumer_hooks, register_tick_jiffies_hook, rtc_interrupt_handler_addr,
    set_current_console_session, software_schedule_interrupt_handler_addr,
    timer_interrupt_handler_addr, yield_now,
};
pub use wait::wait_for_child;
