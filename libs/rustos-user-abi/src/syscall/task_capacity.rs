//! Read-only observation of the kernel's task capacity.
//!
//! - Owner: `kernel-ps` derives capacity (`multitask::task_capacity`); this
//!   module only defines the wire the kernel fills in.
//! - Boundary: Ring3 policy (`loaderd`, `rootd`) grants Job limits from these
//!   numbers instead of guessing the task table size. Observation only: no
//!   field is a reservation, a grant or an authority claim.
//! - Lifecycle: `total_slots`, the ceilings and `kernel_stack_bytes` are fixed
//!   for a kernel build; `live_tasks` and `reservable_slots` are an
//!   instantaneous per-slot observation that moves between two calls.
//! - Failure: wrong version, nonzero flags/reserved/output fields -> `EINVAL`;
//!   a caller without `ProcessLoader` or `RootSupervisor` -> `EPERM`.
//! - Forbidden: treating `reservable_slots` as a promise that a thread or
//!   process can be created.
//! - Evidence: wire layout test here; kernel snapshot/formula tests in
//!   `kernel-ps` `task_capacity`, handler tests in `kernel-compat`.

/// Read-only capacity observation (any number of callers, no side effect).
pub const SYS_RUSTOS_TASK_CAPACITY: u64 = 0x5255_0067;
pub const TASK_CAPACITY_ABI_VERSION: u16 = 1;

/// Request/reply of `SYS_RUSTOS_TASK_CAPACITY`, ABI v1 (72 bytes). The caller
/// sets `abi_version` and zeroes everything else; the kernel fills the rest.
///
/// - `total_slots`: scheduler task slots, including the root slot.
/// - `kernel_reserved_slots`: held back from applications for boot services,
///   idle tasks and other applications (`total_slots / 4`).
/// - `application_thread_budget`: `total_slots - kernel_reserved_slots`, the
///   most threads applications may hold between them.
/// - `live_tasks`: slots that hold a task or one still being reclaimed.
/// - `reservable_slots`: slots a new thread could reserve at this instant.
/// - `kernel_stack_bytes`: kernel stack billed per thread (`Thread` kmem class).
/// - `process_thread_ceiling`: structural per-process thread ceiling.
/// - `root_process_ceiling`: boot default of the root Job's `processes.max`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosTaskCapacityArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub total_slots: u64,
    pub kernel_reserved_slots: u64,
    pub application_thread_budget: u64,
    pub live_tasks: u64,
    pub reservable_slots: u64,
    pub kernel_stack_bytes: u64,
    pub process_thread_ceiling: u64,
    pub root_process_ceiling: u64,
}

crate::user_wire!(RustosTaskCapacityArgs {
    abi_version: u16,
    reserved0: u16,
    flags: u32,
    total_slots: u64,
    kernel_reserved_slots: u64,
    application_thread_budget: u64,
    live_tasks: u64,
    reservable_slots: u64,
    kernel_stack_bytes: u64,
    process_thread_ceiling: u64,
    root_process_ceiling: u64,
});

impl RustosTaskCapacityArgs {
    /// A request: the version set, every other field zero.
    pub const fn request() -> Self {
        Self {
            abi_version: TASK_CAPACITY_ABI_VERSION,
            reserved0: 0,
            flags: 0,
            total_slots: 0,
            kernel_reserved_slots: 0,
            application_thread_budget: 0,
            live_tasks: 0,
            reservable_slots: 0,
            kernel_stack_bytes: 0,
            process_thread_ceiling: 0,
            root_process_ceiling: 0,
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
        assert_eq!(core::mem::size_of::<RustosTaskCapacityArgs>(), 72);
        assert_eq!(core::mem::align_of::<RustosTaskCapacityArgs>(), 8);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, total_slots), 8);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, kernel_reserved_slots), 16);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, application_thread_budget), 24);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, live_tasks), 32);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, reservable_slots), 40);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, kernel_stack_bytes), 48);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, process_thread_ceiling), 56);
        assert_eq!(core::mem::offset_of!(RustosTaskCapacityArgs, root_process_ceiling), 64);
        assert!(RustosTaskCapacityArgs::request().is_request());
        assert!(!RustosTaskCapacityArgs::default().is_request(), "version 0 is not a request");
        let mut dirty = RustosTaskCapacityArgs::request();
        dirty.live_tasks = 1;
        assert!(!dirty.is_request(), "an output field must be zero in a request");
    }
}
