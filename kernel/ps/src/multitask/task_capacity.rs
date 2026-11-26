//! Observed kernel task and memory capacity.
//!
//! - **Owner:** `kernel-ps` reports the structural directory ceilings
//!   (`TaskTable`, `slot_dir`) and per-thread kernel memory costs alongside
//!   observed physical RAM from `kernel-mm`. Ratio-derived slot quotas (`1/4`,
//!   `1/2`) and `boot_slot_count` are deleted: thread and process limits belong
//!   to physical/virtual memory accounting and Ring3 Job policy.
//! - **Boundary:** capacity is read-only factual kernel observation, not policy.
//!   Ring3 (`loaderd`, `rootd`) derives and installs Job limits from observed
//!   `SYS_RUSTOS_TASK_CAPACITY` values; the kernel enforces memory/OOM and
//!   whatever Job limits Ring3 installed.
//! - **Concurrency:** structural bounds and object sizes are constants for a
//!   kernel build; live counts (`live_tasks`, `live_processes`,
//!   `free_ram_bytes`) are instantaneous atomic observations, never reservations.
//! - **Failure:** a refusal names the limit that rejected through
//!   [`CapacityLimit`] and `SpawnTaskError::rejecting_limit`.
//! - **Forbidden:** reintroducing hardcoded ratio-derived task/process ceilings
//!   or acting on an instantaneous snapshot as a reservation.

use super::scheduler;

/// Which structural kernel limit rejected an admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityLimit {
    /// The scheduler task directory ceiling (`max_task_slots`).
    TaskDirectory,
    /// The process directory ceiling (`max_process_slots`).
    ProcessDirectory,
    /// The per-task kernel stack (`kernel_stack_bytes`).
    KernelStack,
}

impl CapacityLimit {
    /// Stable name for diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::TaskDirectory => "task-directory-slots",
            Self::ProcessDirectory => "process-directory-slots",
            Self::KernelStack => "kernel-stack-bytes",
        }
    }
}

/// Structural capacities and per-thread kernel footprint observed by the kernel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskCapacity {
    max_task_slots: usize,
    max_process_slots: usize,
    task_object_bytes: usize,
    kernel_stack_bytes: usize,
}

impl TaskCapacity {
    /// Builds capacity metadata from the task directory ceiling, process
    /// directory ceiling, heap `Task` size, and per-task usable kernel stack.
    pub const fn new(
        max_task_slots: usize,
        max_process_slots: usize,
        task_object_bytes: usize,
        kernel_stack_bytes: usize,
    ) -> Self {
        assert!(max_task_slots >= 2);
        assert!(max_process_slots >= 1);
        assert!(task_object_bytes != 0);
        assert!(kernel_stack_bytes != 0);
        Self {
            max_task_slots,
            max_process_slots,
            task_object_bytes,
            kernel_stack_bytes,
        }
    }

    /// Structural 32-bit task directory slot ceiling (including the root slot).
    pub const fn max_task_slots(&self) -> usize {
        self.max_task_slots
    }

    /// Structural process directory slot ceiling.
    pub const fn max_process_slots(&self) -> usize {
        self.max_process_slots
    }

    /// Size of one heap-allocated `Task` control block.
    pub const fn task_object_bytes(&self) -> usize {
        self.task_object_bytes
    }

    /// Kernel stack bytes billed per thread (`Class::Thread`).
    pub const fn kernel_stack_bytes(&self) -> usize {
        self.kernel_stack_bytes
    }

    /// The numeric value of `limit` for diagnostics.
    pub const fn limit_value(&self, limit: CapacityLimit) -> u64 {
        (match limit {
            CapacityLimit::TaskDirectory => self.max_task_slots,
            CapacityLimit::ProcessDirectory => self.max_process_slots,
            CapacityLimit::KernelStack => self.kernel_stack_bytes,
        }) as u64
    }
}

/// The kernel's structural capacities and per-thread sizes.
pub const fn capacity() -> TaskCapacity {
    TaskCapacity::new(
        super::task_table::MAX_TASK_SLOTS,
        super::slot_dir::SLOT_CEILING,
        core::mem::size_of::<super::task_table::Task>(),
        scheduler::TASK_STACK_SIZE,
    )
}

/// Read-only observation of kernel structural capacity, live occupancy, and
/// physical memory. Observation only: no field reserves anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskCapacitySnapshot {
    pub max_task_slots: u64,
    pub max_process_slots: u64,
    pub live_tasks: u64,
    pub live_processes: u64,
    pub task_object_bytes: u64,
    pub kernel_stack_bytes: u64,
    pub usable_ram_bytes: u64,
    pub free_ram_bytes: u64,
}

impl TaskCapacitySnapshot {
    /// Builds a snapshot from structural capacity and instantaneous observed
    /// live counts/memory.
    pub const fn from_observations(
        capacity: &TaskCapacity,
        live_tasks: usize,
        live_processes: usize,
        usable_ram_bytes: u64,
        free_ram_bytes: u64,
    ) -> Self {
        let clamped_tasks = if live_tasks > capacity.max_task_slots() {
            capacity.max_task_slots()
        } else {
            live_tasks
        };
        let clamped_processes = if live_processes > capacity.max_process_slots() {
            capacity.max_process_slots()
        } else {
            live_processes
        };
        Self {
            max_task_slots: capacity.max_task_slots() as u64,
            max_process_slots: capacity.max_process_slots() as u64,
            live_tasks: clamped_tasks as u64,
            live_processes: clamped_processes as u64,
            task_object_bytes: capacity.task_object_bytes() as u64,
            kernel_stack_bytes: capacity.kernel_stack_bytes() as u64,
            usable_ram_bytes,
            free_ram_bytes,
        }
    }
}

/// Observes the current structural capacity, occupancy, and physical memory.
pub fn task_capacity_snapshot() -> TaskCapacitySnapshot {
    TaskCapacitySnapshot::from_observations(
        &capacity(),
        super::task_table::active_task_count(),
        super::process_table::live_process_count(),
        kernel_mm::api::usable_bytes(),
        kernel_mm::api::free_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn cap() -> TaskCapacity {
        capacity()
    }

    #[test]
    fn capacity_reports_real_structural_directory_and_object_sizes() {
        let c = cap();
        assert_eq!(c.max_task_slots(), super::super::task_table::MAX_TASK_SLOTS);
        assert_eq!(c.max_process_slots(), super::super::slot_dir::SLOT_CEILING);
        assert_eq!(
            c.task_object_bytes(),
            core::mem::size_of::<super::super::task_table::Task>()
        );
        assert_eq!(c.kernel_stack_bytes(), scheduler::TASK_STACK_SIZE);
    }

    #[test]
    fn limit_values_name_the_structural_limit() {
        assert_eq!(
            cap().limit_value(CapacityLimit::TaskDirectory),
            cap().max_task_slots() as u64
        );
        assert_eq!(
            cap().limit_value(CapacityLimit::ProcessDirectory),
            cap().max_process_slots() as u64
        );
        assert_eq!(
            cap().limit_value(CapacityLimit::KernelStack),
            cap().kernel_stack_bytes() as u64
        );
    }

    #[test]
    fn snapshot_clamps_live_counts_without_overflow() {
        let snapshot = TaskCapacitySnapshot::from_observations(
            &cap(),
            usize::MAX,
            usize::MAX,
            4096 * 1024 * 1024,
            1024 * 1024 * 1024,
        );
        assert_eq!(snapshot.live_tasks, cap().max_task_slots() as u64);
        assert_eq!(snapshot.live_processes, cap().max_process_slots() as u64);
    }

    #[test]
    fn every_admission_refusal_names_its_structural_limit() {
        use super::super::SpawnTaskError as E;
        assert_eq!(E::NoFreeTaskSlot.rejecting_limit(), Some(CapacityLimit::TaskDirectory));
        assert_eq!(E::ProcessLimit.rejecting_limit(), Some(CapacityLimit::ProcessDirectory));
        assert_eq!(E::ThreadStackAllocation.rejecting_limit(), Some(CapacityLimit::KernelStack));
        for policy in [E::JobThreadLimit, E::JobThreadMemory] {
            assert_eq!(policy.rejecting_limit(), None, "{policy:?}");
        }
    }

    #[test]
    fn the_live_snapshot_is_consistent_with_the_scheduler_table() {
        let snapshot = task_capacity_snapshot();
        assert_eq!(snapshot.max_task_slots, super::super::task_table::MAX_TASK_SLOTS as u64);
        assert_eq!(snapshot.max_process_slots, super::super::slot_dir::SLOT_CEILING as u64);
        assert!(snapshot.live_tasks <= snapshot.max_task_slots);
        assert!(
            scheduler::thread_creation::reservable_slots() as u64 <= snapshot.max_task_slots - 1,
            "the root slot is never reservable"
        );
    }
}
