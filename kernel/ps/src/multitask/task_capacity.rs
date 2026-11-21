//! Task capacity authority: the one place that derives every admission ceiling
//! that depends on how many scheduler task slots the kernel owns.
//!
//! - **Owner:** `kernel-ps` derives capacity here from the scheduler's slot
//!   table size and per-task kernel stack size. Nothing else may restate a
//!   thread/process ceiling as an independent literal.
//! - **Boundary:** capacity is structural kernel evidence, not policy. Ring3
//!   (`loaderd`, `rootd`) grants Job limits from the observed capacity
//!   (`SYS_RUSTOS_TASK_CAPACITY`) instead of guessing the table size; the
//!   kernel only enforces the structural ceilings derived below and whatever
//!   Job limits Ring3 installed.
//! - **Formulas** (`T` = total scheduler task slots):
//!   - kernel/boot headroom = `T / KERNEL_RESERVE_DIVISOR` (a quarter): boot
//!     services, per-CPU idle tasks and other applications keep these slots.
//!   - application thread budget = `T - headroom`: the most threads any set of
//!     applications may hold between them.
//!   - per-process thread ceiling = `T / PROCESS_CEILING_DIVISOR` (a half): one
//!     process can never consume the table.
//!   - root-job process ceiling = `T / ROOT_PROCESS_DIVISOR` (a quarter): the
//!     boot default of the root Job's `processes.max`.
//!
//!   Changing the table size therefore changes every dependent ceiling
//!   consistently; the divisors are ratios, not capacities.
//! - **Concurrency:** the derived values are `const`. The live counts in
//!   [`TaskCapacitySnapshot`] are an instantaneous per-slot observation (each
//!   slot under its admission leaf, one at a time), never a reservation.
//! - **Failure:** a refusal names the authority limit that rejected through
//!   [`CapacityLimit`] and `SpawnTaskError::rejecting_limit`.
//! - **Forbidden:** acting on a snapshot as authority, or restating a derived
//!   ceiling as a second literal.

use super::scheduler;

/// A quarter of the table is held back for boot services, idle tasks and other
/// applications.
pub const KERNEL_RESERVE_DIVISOR: usize = 4;
/// One process may hold at most half of the table.
pub const PROCESS_CEILING_DIVISOR: usize = 2;
/// The root Job admits at most a quarter of the table's slots as processes.
pub const ROOT_PROCESS_DIVISOR: usize = 4;

/// Which authority-derived limit rejected an admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityLimit {
    /// The scheduler task table (`total_slots`).
    TaskTable,
    /// The per-process thread ceiling (`process_thread_ceiling`).
    ProcessThreads,
    /// The root Job's process ceiling (`root_process_ceiling`).
    RootProcesses,
    /// The per-task kernel stack (`kernel_stack_bytes`).
    KernelStack,
}

impl CapacityLimit {
    /// Stable name for diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::TaskTable => "task-table-slots",
            Self::ProcessThreads => "process-thread-ceiling",
            Self::RootProcesses => "root-process-ceiling",
            Self::KernelStack => "kernel-stack-bytes",
        }
    }
}

/// Capacity of the scheduler task table and every ceiling derived from it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskCapacity {
    total_slots: usize,
    kernel_stack_bytes: usize,
}

impl TaskCapacity {
    /// Derives capacity for a table of `total_slots` slots with one
    /// `kernel_stack_bytes` kernel stack per task. Fails (at compile time for
    /// the const [`CAPACITY`]) if the table is too small for the formulas to
    /// leave every ceiling usable.
    pub const fn new(total_slots: usize, kernel_stack_bytes: usize) -> Self {
        let capacity = Self {
            total_slots,
            kernel_stack_bytes,
        };
        assert!(kernel_stack_bytes != 0);
        // Every ceiling is at least one, the headroom plus the application
        // budget is exactly the table, and no single process can take more than
        // half of it.
        assert!(total_slots / KERNEL_RESERVE_DIVISOR >= 1);
        assert!(capacity.root_process_ceiling() >= 1);
        assert!(capacity.process_thread_ceiling() >= 1);
        assert!(
            capacity.kernel_reserved_slots() + capacity.application_thread_budget()
                == total_slots
        );
        assert!(capacity.process_thread_ceiling() * 2 <= total_slots);
        // The root Job can never admit more processes than the table can host.
        assert!(capacity.root_process_ceiling() <= total_slots);
        capacity
    }

    /// Total scheduler task slots (the table size, including the root slot).
    pub const fn total_slots(&self) -> usize {
        self.total_slots
    }

    /// Slots held back from applications for boot services, idle tasks and
    /// other applications' growth.
    pub const fn kernel_reserved_slots(&self) -> usize {
        self.total_slots / KERNEL_RESERVE_DIVISOR
    }

    /// Threads applications may hold between them (`total - headroom`).
    pub const fn application_thread_budget(&self) -> usize {
        self.total_slots - self.kernel_reserved_slots()
    }

    /// Structural per-process thread ceiling.
    pub const fn process_thread_ceiling(&self) -> usize {
        self.total_slots / PROCESS_CEILING_DIVISOR
    }

    /// Boot default of the root Job's `processes.max`.
    pub const fn root_process_ceiling(&self) -> usize {
        self.total_slots / ROOT_PROCESS_DIVISOR
    }

    /// Kernel stack bytes billed per thread.
    pub const fn kernel_stack_bytes(&self) -> usize {
        self.kernel_stack_bytes
    }

    /// The numeric value of `limit` for diagnostics.
    pub const fn limit_value(&self, limit: CapacityLimit) -> u64 {
        (match limit {
            CapacityLimit::TaskTable => self.total_slots,
            CapacityLimit::ProcessThreads => self.process_thread_ceiling(),
            CapacityLimit::RootProcesses => self.root_process_ceiling(),
            CapacityLimit::KernelStack => self.kernel_stack_bytes,
        }) as u64
    }
}

/// The kernel's capacity. The scheduler table size and kernel stack size are
/// owned by the scheduler; every other limit is derived from them here.
pub const CAPACITY: TaskCapacity =
    TaskCapacity::new(scheduler::MAX_TASK, scheduler::TASK_STACK_SIZE);

// The scheduler encodes slots in eight bits in several lock-free words.
const _: () = assert!(CAPACITY.total_slots().is_power_of_two());
// Process slot storage must be able to hold the root Job's default ceiling.
const _: () = assert!(CAPACITY.root_process_ceiling() <= super::slot_dir::SLOT_CEILING);

/// Read-only observation of capacity plus the current occupancy. Observation
/// only: neither field reserves anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskCapacitySnapshot {
    pub total_slots: u64,
    pub kernel_reserved_slots: u64,
    pub application_thread_budget: u64,
    /// Slots that hold a task or one still being reclaimed (not reservable).
    pub live_tasks: u64,
    /// Slots a new thread could reserve at this instant.
    pub reservable_slots: u64,
    pub kernel_stack_bytes: u64,
    pub process_thread_ceiling: u64,
    pub root_process_ceiling: u64,
}

impl TaskCapacitySnapshot {
    /// Builds the snapshot from the authority and one reservable-slot count.
    /// `reservable_slots` is clamped to the table so a racy count can never
    /// make `live_tasks` underflow.
    pub const fn from_authority(capacity: &TaskCapacity, reservable_slots: usize) -> Self {
        let total = capacity.total_slots();
        let reservable = if reservable_slots > total {
            total
        } else {
            reservable_slots
        };
        Self {
            total_slots: total as u64,
            kernel_reserved_slots: capacity.kernel_reserved_slots() as u64,
            application_thread_budget: capacity.application_thread_budget() as u64,
            live_tasks: (total - reservable) as u64,
            reservable_slots: reservable as u64,
            kernel_stack_bytes: capacity.kernel_stack_bytes() as u64,
            process_thread_ceiling: capacity.process_thread_ceiling() as u64,
            root_process_ceiling: capacity.root_process_ceiling() as u64,
        }
    }
}

/// Observes the current capacity and occupancy.
pub fn task_capacity_snapshot() -> TaskCapacitySnapshot {
    TaskCapacitySnapshot::from_authority(
        &CAPACITY,
        scheduler::thread_creation::reservable_slots(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ceiling_derives_from_the_table_size() {
        let capacity = TaskCapacity::new(256, 2 * 1024 * 1024);
        assert_eq!(capacity.total_slots(), 256);
        assert_eq!(capacity.kernel_reserved_slots(), 64);
        assert_eq!(capacity.application_thread_budget(), 192);
        assert_eq!(capacity.process_thread_ceiling(), 128);
        assert_eq!(capacity.root_process_ceiling(), 64);
        // A wider table moves every dependent ceiling together.
        let wide = TaskCapacity::new(1024, 2 * 1024 * 1024);
        assert_eq!(wide.kernel_reserved_slots(), 256);
        assert_eq!(wide.application_thread_budget(), 768);
        assert_eq!(wide.process_thread_ceiling(), 512);
        assert_eq!(wide.root_process_ceiling(), 256);
    }

    #[test]
    fn the_kernel_capacity_matches_the_scheduler_table_and_stack() {
        assert_eq!(CAPACITY.total_slots(), scheduler::MAX_TASK);
        assert_eq!(CAPACITY.kernel_stack_bytes(), scheduler::TASK_STACK_SIZE);
        assert_eq!(super::super::MAX_SCHEDULER_TASKS, CAPACITY.total_slots());
        assert_eq!(
            CAPACITY.kernel_reserved_slots() + CAPACITY.application_thread_budget(),
            CAPACITY.total_slots()
        );
    }

    #[test]
    fn limit_values_name_the_authority_limit() {
        assert_eq!(
            CAPACITY.limit_value(CapacityLimit::TaskTable),
            CAPACITY.total_slots() as u64
        );
        assert_eq!(
            CAPACITY.limit_value(CapacityLimit::ProcessThreads),
            CAPACITY.process_thread_ceiling() as u64
        );
        assert_eq!(
            CAPACITY.limit_value(CapacityLimit::RootProcesses),
            CAPACITY.root_process_ceiling() as u64
        );
        assert_eq!(
            CAPACITY.limit_value(CapacityLimit::KernelStack),
            CAPACITY.kernel_stack_bytes() as u64
        );
    }

    #[test]
    fn a_snapshot_reports_occupancy_without_underflow() {
        let snapshot = TaskCapacitySnapshot::from_authority(&CAPACITY, 10);
        assert_eq!(snapshot.total_slots, CAPACITY.total_slots() as u64);
        assert_eq!(snapshot.reservable_slots, 10);
        assert_eq!(snapshot.live_tasks, CAPACITY.total_slots() as u64 - 10);
        assert_eq!(snapshot.kernel_stack_bytes, CAPACITY.kernel_stack_bytes() as u64);
        assert_eq!(snapshot.process_thread_ceiling, CAPACITY.process_thread_ceiling() as u64);
        assert_eq!(snapshot.root_process_ceiling, CAPACITY.root_process_ceiling() as u64);
        // A racy over-count is clamped to the table.
        let clamped = TaskCapacitySnapshot::from_authority(&CAPACITY, usize::MAX);
        assert_eq!(clamped.reservable_slots, CAPACITY.total_slots() as u64);
        assert_eq!(clamped.live_tasks, 0);
    }

    #[test]
    fn every_admission_refusal_names_its_authority_limit() {
        use super::super::SpawnTaskError as E;
        assert_eq!(E::NoFreeTaskSlot.rejecting_limit(), Some(CapacityLimit::TaskTable));
        assert_eq!(E::ProcessThreadLimit.rejecting_limit(), Some(CapacityLimit::ProcessThreads));
        assert_eq!(E::ProcessLimit.rejecting_limit(), Some(CapacityLimit::RootProcesses));
        assert_eq!(E::ThreadStackAllocation.rejecting_limit(), Some(CapacityLimit::KernelStack));
        // Job limits are Ring3-granted policy, not authority-derived.
        for policy in [E::JobThreadLimit, E::JobThreadMemory] {
            assert_eq!(policy.rejecting_limit(), None, "{policy:?}");
        }
    }

    #[test]
    fn the_live_snapshot_is_consistent_with_the_scheduler_table() {
        let snapshot = task_capacity_snapshot();
        assert_eq!(snapshot.live_tasks + snapshot.reservable_slots, snapshot.total_slots);
        assert!(snapshot.reservable_slots <= snapshot.total_slots - 1, "the root slot is never reservable");
    }
}
