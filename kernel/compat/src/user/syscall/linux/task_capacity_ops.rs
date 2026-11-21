//! `SYS_RUSTOS_TASK_CAPACITY`: read-only observation of the task capacity.
//!
//! - **Owner:** `kernel-ps` derives capacity (`multitask::task_capacity`); this
//!   handler only validates the request, gates the caller and copies the
//!   snapshot out.
//! - **Boundary:** only the Ring3 policy services that grant Job limits
//!   (`ProcessLoader`, `RootSupervisor`) may read it; it grants nothing.
//! - **Concurrency:** one instantaneous per-slot observation, no lock held
//!   across the copyout and no state changed.
//! - **Failure:** wrong version or nonzero request fields are `EINVAL`, an
//!   unauthorized caller `EPERM`, a bad user pointer its pointer errno.
//! - **Forbidden:** treating the reply as a reservation or as authority.
use super::*;
use rustos_user_abi::syscall::RustosTaskCapacityArgs;

fn capacity_reply(snapshot: &multitask::TaskCapacitySnapshot) -> RustosTaskCapacityArgs {
    RustosTaskCapacityArgs {
        total_slots: snapshot.total_slots,
        kernel_reserved_slots: snapshot.kernel_reserved_slots,
        application_thread_budget: snapshot.application_thread_budget,
        live_tasks: snapshot.live_tasks,
        reservable_slots: snapshot.reservable_slots,
        kernel_stack_bytes: snapshot.kernel_stack_bytes,
        process_thread_ceiling: snapshot.process_thread_ceiling,
        root_process_ceiling: snapshot.root_process_ceiling,
        ..RustosTaskCapacityArgs::request()
    }
}

pub(super) fn syscall_linux_rustos_task_capacity(args_ptr: u64) -> u64 {
    let authorized =
        ipc_ops::current_process_has_service_capability(ServiceCapability::ProcessLoader)
            || ipc_ops::current_process_has_service_capability(ServiceCapability::RootSupervisor);
    if !authorized {
        return linux_errno(LINUX_EPERM);
    }
    let request = match usermem::read_current_user_struct::<RustosTaskCapacityArgs>(args_ptr) {
        Ok(request) => request,
        Err(error) => return linux_errno(user_pointer_error_to_linux_errno(error)),
    };
    if !request.is_request() {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(error) = usermem::validate_current_user_write_buffer(
        args_ptr,
        size_of::<RustosTaskCapacityArgs>(),
    ) {
        return linux_errno(user_pointer_error_to_linux_errno(error));
    }
    let reply = capacity_reply(&multitask::task_capacity_snapshot());
    match usermem::write_current_user_struct(args_ptr, &reply) {
        Ok(()) => 0,
        Err(error) => linux_errno(user_pointer_error_to_linux_errno(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reply_carries_every_authority_value_and_is_not_a_request() {
        let capacity = multitask::TaskCapacity::new(256, 2 * 1024 * 1024);
        let snapshot = multitask::TaskCapacitySnapshot::from_authority(&capacity, 200);
        let reply = capacity_reply(&snapshot);
        assert_eq!(reply.abi_version, rustos_user_abi::syscall::TASK_CAPACITY_ABI_VERSION);
        assert_eq!(reply.flags, 0);
        assert_eq!(reply.total_slots, 256);
        assert_eq!(reply.kernel_reserved_slots, 64);
        assert_eq!(reply.application_thread_budget, 192);
        assert_eq!(reply.live_tasks, 56);
        assert_eq!(reply.reservable_slots, 200);
        assert_eq!(reply.kernel_stack_bytes, 2 * 1024 * 1024);
        assert_eq!(reply.process_thread_ceiling, 128);
        assert_eq!(reply.root_process_ceiling, 64);
        assert!(!reply.is_request(), "a filled reply is not a valid request");
    }

    #[test]
    fn the_handler_gates_on_a_policy_capability_before_reading_the_request() {
        let source = include_str!("task_capacity_ops.rs");
        let handler = source
            .split_once("pub(super) fn syscall_linux_rustos_task_capacity")
            .unwrap()
            .1
            .split_once("#[cfg(test)]")
            .unwrap()
            .0;
        let gate = handler.find("ServiceCapability::ProcessLoader").unwrap();
        let read = handler.find("read_current_user_struct").unwrap();
        assert!(gate < read);
        assert!(handler.contains("ServiceCapability::RootSupervisor"));
    }
}
