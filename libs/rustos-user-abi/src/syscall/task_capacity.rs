//! Read-only observation of the kernel's task and memory capacity.
//!
//! - Owner: `kernel-ps` reports observed capacity (`multitask::task_capacity`);
//!   this module defines the wire ABI the kernel fills in.
//! - Boundary: Ring3 policy (`loaderd`, `rootd`) derives and enforces Job
//!   limits from these observed hardware/object values instead of relying on
//!   fixed kernel table ratios. Observation only: no field is a reservation,
//!   a grant, or an authority claim.
//! - Lifecycle: `max_task_slots`, `max_process_slots`, `task_object_bytes`,
//!   `kernel_stack_bytes`, and `usable_ram_bytes` are fixed for a boot;
//!   `live_tasks`, `live_processes`, and `free_ram_bytes` are instantaneous
//!   observations that move between calls.
//! - Failure: wrong version, nonzero flags/reserved/output fields -> `EINVAL`;
//!   a caller without `ProcessLoader` or `RootSupervisor` -> `EPERM`.
//! - Forbidden: treating observed capacity as a promise that a thread or
//!   process can be created without OOM or Job limits.
//! - Evidence: wire layout test here; kernel snapshot tests in
//!   `kernel-ps` `task_capacity`, handler tests in `kernel-compat`.

/// Read-only capacity observation (any number of callers, no side effect).
pub const SYS_RUSTOS_TASK_CAPACITY: u64 = 0x5255_0067;
pub const TASK_CAPACITY_ABI_VERSION: u16 = 2;

/// Request/reply of `SYS_RUSTOS_TASK_CAPACITY`, ABI v2 (72 bytes). The caller
/// sets `abi_version` and zeroes everything else; the kernel fills the rest.
///
/// - `max_task_slots`: structural directory ceiling for 32-bit task slots.
/// - `max_process_slots`: structural directory ceiling for process slots.
/// - `live_tasks`: active/reclaiming scheduler tasks at this instant.
/// - `live_processes`: active/un-reaped processes at this instant.
/// - `task_object_bytes`: size of one heap-allocated kernel `Task` object.
/// - `kernel_stack_bytes`: kernel stack billed per thread (`Thread` kmem class).
/// - `usable_ram_bytes`: total usable physical RAM reported by `kernel-mm`.
/// - `free_ram_bytes`: instantaneous free physical RAM reported by `kernel-mm`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosTaskCapacityArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub max_task_slots: u64,
    pub max_process_slots: u64,
    pub live_tasks: u64,
    pub live_processes: u64,
    pub task_object_bytes: u64,
    pub kernel_stack_bytes: u64,
    pub usable_ram_bytes: u64,
    pub free_ram_bytes: u64,
}

crate::user_wire!(RustosTaskCapacityArgs {
    abi_version: u16,
    reserved0: u16,
    flags: u32,
    max_task_slots: u64,
    max_process_slots: u64,
    live_tasks: u64,
    live_processes: u64,
    task_object_bytes: u64,
    kernel_stack_bytes: u64,
    usable_ram_bytes: u64,
    free_ram_bytes: u64,
});

impl RustosTaskCapacityArgs {
    /// A request: the version set, every other field zero.
    pub const fn request() -> Self {
        Self {
            abi_version: TASK_CAPACITY_ABI_VERSION,
            reserved0: 0,
            flags: 0,
            max_task_slots: 0,
            max_process_slots: 0,
            live_tasks: 0,
            live_processes: 0,
            task_object_bytes: 0,
            kernel_stack_bytes: 0,
            usable_ram_bytes: 0,
            free_ram_bytes: 0,
        }
    }

    /// Whether this is a well-formed request (exact version, all else zero).
    pub fn is_request(&self) -> bool {
        *self == Self::request()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_capacity_wire_is_fixed_width_and_request_is_all_zero_but_the_version() {
        assert_eq!(SYS_RUSTOS_TASK_CAPACITY, 0x5255_0067);
        assert_eq!(TASK_CAPACITY_ABI_VERSION, 2);
        assert_eq!(core::mem::size_of::<RustosTaskCapacityArgs>(), 72);
        assert_eq!(core::mem::align_of::<RustosTaskCapacityArgs>(), 8);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, max_task_slots), 8);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, max_process_slots), 16);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, live_tasks), 24);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, live_processes), 32);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, task_object_bytes), 40);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, kernel_stack_bytes), 48);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, usable_ram_bytes), 56);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, free_ram_bytes), 64);
        assert!(RustosTaskCapacityArgs::request().is_request());
        assert!(!RustosTaskCapacityArgs::default().is_request(), "version 0 is not a request");
        let mut dirty = RustosTaskCapacityArgs::request();
        dirty.live_tasks = 1;
        assert!(!dirty.is_request(), "an output field must be zero in a request");
    }
}
