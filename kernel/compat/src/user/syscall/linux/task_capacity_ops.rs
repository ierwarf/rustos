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
        max_task_slots: snapshot.max_task_slots,
        max_process_slots: snapshot.max_process_slots,
        live_tasks: snapshot.live_tasks,
        live_processes: snapshot.live_processes,
        task_object_bytes: snapshot.task_object_bytes,
        kernel_stack_bytes: snapshot.kernel_stack_bytes,
        usable_ram_bytes: snapshot.usable_ram_bytes,
        free_ram_bytes: snapshot.free_ram_bytes,
        ..RustosTaskCapacityArgs::request()
    }
}

pub(super) fn syscall_linux_rustos_task_capacity(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        require_any_service_capability(&[
            ServiceCapability::ProcessLoader,
            ServiceCapability::RootSupervisor,
        ])?;
        let request = read_user_struct::<RustosTaskCapacityArgs>(args_ptr)?;
        if !request.is_request() {
            return Err(LINUX_EINVAL);
        }
        validate_user_write_struct::<RustosTaskCapacityArgs>(args_ptr)?;
        let reply = capacity_reply(&multitask::task_capacity_snapshot());
        write_user_struct(args_ptr, &reply)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reply_carries_every_authority_value_and_is_not_a_request() {
        let capacity = multitask::TaskCapacity::new(16384, 16384, 16 * 1024, 256 * 1024);
        let snapshot = multitask::TaskCapacitySnapshot::from_observations(
            &capacity,
            56,
            12,
            4096 * 1024 * 1024,
            3072 * 1024 * 1024,
        );
        let reply = capacity_reply(&snapshot);
        assert_eq!(reply.abi_version, rustos_user_abi::syscall::TASK_CAPACITY_ABI_VERSION);
        assert_eq!(reply.flags, 0);
        assert_eq!(reply.max_task_slots, 16384);
        assert_eq!(reply.max_process_slots, 16384);
        assert_eq!(reply.live_tasks, 56);
        assert_eq!(reply.live_processes, 12);
        assert_eq!(reply.task_object_bytes, 16 * 1024);
        assert_eq!(reply.kernel_stack_bytes, 256 * 1024);
        assert_eq!(reply.usable_ram_bytes, 4096 * 1024 * 1024);
        assert_eq!(reply.free_ram_bytes, 3072 * 1024 * 1024);
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
        let read = handler.find("read_user_struct").unwrap();
        assert!(gate < read);
        assert!(handler.contains("ServiceCapability::RootSupervisor"));
    }
}
