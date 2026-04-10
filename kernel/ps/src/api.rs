pub mod api {
    pub use crate::multitask::{
        CurrentUserSnapshot, RetainedCurrentUserAddressSpace, RetainedCurrentUserProcessState,
        SpawnTaskError, Thread, UserFaultDisposition, UserStackState, UserTaskBootstrap,
        UserTaskRegisters, WaitChildResult,
    };
    pub use crate::user::abi::UserAbi;
    pub use crate::user::process_state::{ProcessSecurityContext, UserProcessState};
    pub use x86_64::VirtAddr;

    pub fn init() {
        crate::multitask::init();
    }

    pub fn current_user_snapshot() -> Option<CurrentUserSnapshot> {
        crate::multitask::current_user_snapshot()
    }

    pub fn timer_interrupt_handler_addr() -> u64 {
        crate::multitask::timer_interrupt_handler_addr()
    }

    pub fn rtc_interrupt_handler_addr() -> u64 {
        crate::multitask::rtc_interrupt_handler_addr()
    }

    pub fn software_schedule_interrupt_handler_addr() -> u64 {
        crate::multitask::software_schedule_interrupt_handler_addr()
    }

    pub fn current_user_id() -> Option<u64> {
        crate::multitask::current_user_id()
    }

    pub fn current_user_process_id() -> Option<u64> {
        crate::multitask::current_user_process_id()
    }

    pub fn retain_current_user_process_state() -> Option<RetainedCurrentUserProcessState> {
        crate::multitask::retain_current_user_process_state()
    }

    pub fn service_deferred_work() -> usize {
        crate::multitask::service_deferred_work()
    }

    pub fn is_initialized() -> bool {
        crate::multitask::is_initialized()
    }

    pub fn yield_now() {
        crate::multitask::yield_now();
    }

    pub fn retire_current_user_task_due_to_fault(
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rip: u64,
        rsp: u64,
    ) -> UserFaultDisposition {
        crate::multitask::retire_current_user_task_due_to_fault(
            vector, error_code, cr2, rip, rsp,
        )
    }

    pub fn halt_current_retired_task() -> ! {
        crate::multitask::halt_current_retired_task()
    }

    pub fn wait_for_child(parent_process_id: u64, target_pid: i64) -> WaitChildResult {
        crate::multitask::wait_for_child(parent_process_id, target_pid)
    }

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
}
