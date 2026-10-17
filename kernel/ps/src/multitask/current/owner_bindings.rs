//! Current metadata joins and exact process-PID fallback. Callers stabilize
//! Current with local IRQ exclusion; no production directory recovery.
use super::*;

// Callers exclude local IRQs through the identity/Current snapshot.
pub(super) fn owned_current_user_binding() -> Option<(
    u64,
    UserAbi,
    process_table::ProcessHandle,
    ConsoleSessionHandle,
)> {
    super::super::scheduler::stack_resources::context::current_user_binding()
}

pub(super) fn current_process_id_for_handle(handle: process_table::ProcessHandle) -> Option<u64> {
    interrupts::without_interrupts(|| {
        if let Some(pid) = published_current_identity_process_id(handle) {
            return Some(pid);
        }
        super::super::scheduler::stack_resources::context::current_process_id(handle)
    })
}

pub(super) fn owned_current_process_handle() -> Option<process_table::ProcessHandle> {
    super::super::scheduler::stack_resources::context::current_process_handle()
}
