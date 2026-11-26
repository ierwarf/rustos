//! Application Job grants: Ring3 policy that turns an application's *request*
//! (`ApplicationRequest`) into a *grant* against the capacity and memory
//! observed at launch.
//!
//! - **Owner:** `loaderd`. The kernel owns mechanism, memory accounting, and
//!   observed counts (`SYS_RUSTOS_TASK_CAPACITY`); `loaderd` derives policy
//!   budgets from observed hardware memory and per-thread costs instead of
//!   hardcoded kernel table ratios (`1/4`, `1/2`).
//! - **Boundary:** no number here guesses the machine. Every bound comes from
//!   observed kernel sizes/RAM (`Machine`) and documented policy ratios.
//! - **Observed inputs** (`Machine`): `max_task_slots`, `max_process_slots`,
//!   `live_tasks`, `live_processes`, `task_object_bytes`, `kernel_stack_bytes`,
//!   `usable_ram_bytes`, `free_ram_bytes` (all from `SYS_RUSTOS_TASK_CAPACITY`)
//!   and physical memory (`sysinfo`).
//! - **Request semantics: exact.** A requested dimension is granted in full or
//!   refused with the binding [`Limit`]. No rounding, clamping, or partial grant.
//! - **Budgets:** total application thread kernel footprint (`task_object_bytes
//!   + kernel_stack_bytes`) is bounded at half of physical RAM (`ram_bytes / 2`)
//!   and capped below the directory ceiling; application process budget is a
//!   quarter of that thread budget (`max_process_slots` upper-bounded); default
//!   thread/process shares take `1/4` of those budgets and `1/2` of physical RAM
//!   per process. No artificial 512-thread per-process ceiling exists.
//! - **Sum-of-grants invariant:** committed threads across live application
//!   Jobs never exceed `application_thread_budget()`, nor `reservable_slots()`
//!   backed by free RAM and free directory slots.
//! - **Forbidden:** a fixed thread cap, a clamp, a fake success for an
//!   unsatisfiable request.

use core::fmt;

use rustos_user_abi::application_request::ApplicationRequest;
use rustos_user_abi::syscall::{
    RustosTaskCapacityArgs, APP_BUFFER_CLIENT_QUOTA_BYTES, JOB_KMEM_CLASS_DEVICE_BUFFER,
    JOB_KMEM_CLASS_HANDLE_TABLE, JOB_KMEM_CLASS_IPC_ENDPOINT, JOB_KMEM_CLASS_IPC_MESSAGE,
    JOB_KMEM_CLASS_PAGE_TABLE, JOB_KMEM_CLASS_THREAD, JOB_KMEM_CLASS_VMA,
    TASK_CAPACITY_ABI_VERSION,
};

const EINVAL: i32 = 22;
const EAGAIN: i32 = 11;
const EOVERFLOW: i32 = 75;
const EIO: i32 = 5;

// ---- Default ratios (policy) -------------------------------------------------

/// An application that declares no thread need gets this fraction of the
/// application thread budget (a quarter).
pub const DEFAULT_THREAD_SHARE_DIVISOR: u64 = 4;
/// An application that declares no process need gets this fraction of the
/// application process budget (a quarter).
pub const DEFAULT_PROCESS_SHARE_DIVISOR: u64 = 4;
/// An application that declares no memory need may resident-map this fraction of
/// physical memory per process (a half). A ceiling, not a reservation.
pub const DEFAULT_PROCESS_MEMORY_DIVISOR: u64 = 2;

// ---- Kernel meaning of the accounting units ------------------------------------

/// Bytes per page (x86-64 4 KiB base page); Job memory limits count these.
pub const PAGE_BYTES: u64 = 4096;
/// A page-table frame maps this many pages (512 entries).
pub const PAGES_PER_TABLE_FRAME: u64 = 512;
/// A page-table frame is billed as one 4 KiB frame (`Class::PageTable`).
pub const PAGE_TABLE_FRAME_BYTES: u64 = PAGE_BYTES;
/// Sparse mappings need more table frames than dense ones. The budget allows
/// this multiple of the dense minimum (`pages / PAGES_PER_TABLE_FRAME` frames).
pub const PAGE_TABLE_SPARSITY_FACTOR: u64 = 8;
/// Descriptor-table slot bytes (`Class::HandleTable`) one process may pin.
pub const HANDLE_TABLE_BYTES_PER_PROCESS: u64 = 256 * 1024;
/// Reserved endpoint queue bytes (`Class::IpcEndpoint`) one process may pin.
pub const IPC_ENDPOINT_BYTES_PER_PROCESS: u64 = 128 * 1024;
/// Shared-mapping bookkeeping bytes (`Class::Vma`) one process may pin.
pub const VMA_BYTES_PER_PROCESS: u64 = 128 * 1024;
/// Queued synchronous-call message bytes (`Class::IpcMessage`, billed to the
/// sending Job) one process may have outstanding.
pub const IPC_MESSAGE_BYTES_PER_PROCESS: u64 = 512 * 1024;
/// App DMA-BUF apertures (`Class::DeviceBuffer`) one Job may hold, in units of
/// the per-table client quota `APP_BUFFER_CLIENT_QUOTA_BYTES`. A device
/// aperture, not per process.
pub const DEVICE_BUFFER_QUOTAS_PER_JOB: u64 = 2;
/// Client-funded session ceiling (`SYS_RUSTOS_JOB_SESSION`) per granted process.
pub const SESSIONS_PER_PROCESS: u64 = 128;
/// `memory.high` pressure telemetry fires at this fraction of the Job's page
/// limit (numerator/denominator).
pub const MEMORY_HIGH_NUMERATOR: u64 = 4;
pub const MEMORY_HIGH_DENOMINATOR: u64 = 5;

// ---- Observation ---------------------------------------------------------------

/// What loaderd observes of the running machine at one launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Machine {
    pub max_task_slots: u64,
    pub max_process_slots: u64,
    pub live_tasks: u64,
    pub live_processes: u64,
    pub task_object_bytes: u64,
    pub kernel_stack_bytes: u64,
    pub usable_ram_bytes: u64,
    pub free_ram_bytes: u64,
    /// Physical memory in pages (from `sysinfo`).
    pub ram_pages: u64,
}

impl Machine {
    /// Validates a `SYS_RUSTOS_TASK_CAPACITY` reply plus physical memory. A
    /// reply that cannot describe a usable capacity is `EIO`.
    pub fn observe(reply: &RustosTaskCapacityArgs, ram_bytes: u64) -> Result<Self, i32> {
        let usable = reply.abi_version == TASK_CAPACITY_ABI_VERSION
            && reply.flags == 0
            && reply.max_task_slots >= 2
            && reply.max_process_slots >= 1
            && reply.live_tasks <= reply.max_task_slots
            && reply.live_processes <= reply.max_process_slots
            && reply.task_object_bytes != 0
            && reply.kernel_stack_bytes != 0
            && reply.usable_ram_bytes >= PAGE_BYTES
            && reply.free_ram_bytes <= reply.usable_ram_bytes
            && ram_bytes >= PAGE_BYTES;
        if !usable {
            return Err(EIO);
        }
        Ok(Self {
            max_task_slots: reply.max_task_slots,
            max_process_slots: reply.max_process_slots,
            live_tasks: reply.live_tasks,
            live_processes: reply.live_processes,
            task_object_bytes: reply.task_object_bytes,
            kernel_stack_bytes: reply.kernel_stack_bytes,
            usable_ram_bytes: reply.usable_ram_bytes,
            free_ram_bytes: reply.free_ram_bytes,
            ram_pages: ram_bytes / PAGE_BYTES,
        })
    }

    /// Total kernel bytes charged/needed per thread (`Task` + kernel stack).
    pub fn thread_footprint_bytes(&self) -> u64 {
        self.kernel_stack_bytes
            .saturating_add(self.task_object_bytes)
            .max(1)
    }

    /// Application-wide thread budget derived by Ring3 policy from observed
    /// physical RAM and per-thread kernel footprint, bounded below `max_task_slots`.
    pub fn application_thread_budget(&self) -> u64 {
        let ram_half = self.ram_pages.saturating_mul(PAGE_BYTES) / 2;
        let by_memory = ram_half / self.thread_footprint_bytes();
        by_memory.min(self.max_task_slots.saturating_sub(1)).max(1)
    }

    /// Thread capacity currently backed by both free directory slots and free RAM.
    pub fn reservable_slots(&self) -> u64 {
        let dir_free = self.max_task_slots.saturating_sub(self.live_tasks);
        let mem_free = self.free_ram_bytes / self.thread_footprint_bytes();
        dir_free.min(mem_free)
    }

    /// Process slots applications may hold between them.
    pub fn process_budget(&self) -> u64 {
        (self.application_thread_budget() / DEFAULT_THREAD_SHARE_DIVISOR)
            .min(self.max_process_slots)
            .max(1)
    }
}

/// What the live application Jobs already hold (summed by `ApplicationJobs`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Committed {
    /// Processes granted to Jobs that have not been fully reclaimed.
    pub processes: u64,
    /// Threads granted to Jobs that have not been fully reclaimed.
    pub threads: u64,
    /// Granted threads those Jobs have not created yet (granted minus the
    /// kernel-reported usage): slots that are free now but already promised.
    pub unused_threads: u64,
}

// ---- Grant ---------------------------------------------------------------------

/// The limits one application Job is granted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Grant {
    pub processes: u64,
    pub threads: u64,
    pub memory_pages_per_process: u64,
}

impl Grant {
    /// Job-wide page limit: the per-process budget scales with the processes
    /// (the kernel charges COW and immutable aliases per mapping root). Checked
    /// for overflow when granted.
    pub const fn job_memory_pages(&self) -> u64 {
        self.processes * self.memory_pages_per_process
    }
}

/// The limit that refused a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Limit {
    /// Processes above the process directory ceiling.
    RootProcessCeiling,
    /// Processes above the application process budget, or what
    /// other live applications leave of it.
    ApplicationProcessBudget,
    /// Threads above the application thread budget, or what other live
    /// applications leave of it.
    ApplicationThreadBudget,
    /// Slots/memory available right now, net of those promised to live Jobs.
    ReservableTaskSlots,
    /// More processes than threads: a process needs at least one thread.
    ProcessesExceedThreads,
    /// Pages per process above physical memory.
    PhysicalMemory,
    /// `processes * pages_per_process` does not fit 64 bits.
    JobMemoryOverflow,
    /// A parent Job clamps the limit below what was granted.
    AncestorJob,
}

impl Limit {
    pub const fn name(self) -> &'static str {
        match self {
            Self::RootProcessCeiling => "root-process-ceiling",
            Self::ApplicationProcessBudget => "application-process-budget",
            Self::ApplicationThreadBudget => "application-thread-budget",
            Self::ReservableTaskSlots => "reservable-task-slots",
            Self::ProcessesExceedThreads => "processes-exceed-threads",
            Self::PhysicalMemory => "physical-memory-pages",
            Self::JobMemoryOverflow => "job-memory-pages-overflow",
            Self::AncestorJob => "ancestor-job-limit",
        }
    }
}

/// A request that cannot be granted in full: the limit, what was asked for and
/// what that limit had left.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub errno: i32,
    pub limit: Limit,
    pub requested: u64,
    pub available: u64,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "limit={} requested={} available={} errno={}",
            self.limit.name(),
            self.requested,
            self.available,
            self.errno
        )
    }
}

fn structural(limit: Limit, requested: u64, available: u64) -> Refusal {
    Refusal { errno: EINVAL, limit, requested, available }
}

fn transient(limit: Limit, requested: u64, available: u64) -> Refusal {
    Refusal { errno: EAGAIN, limit, requested, available }
}

/// Resolves `request` against the observed `machine` and what other live Jobs
/// `committed`. Pure: no syscall, no state.
///
/// Check order is structural first (a request that can never fit this kernel is
/// `EINVAL` whatever the load), then transient (`EAGAIN`).
pub fn grant(
    machine: &Machine, committed: &Committed, request: &ApplicationRequest,
) -> Result<Grant, Refusal> {
    let thread_budget = machine.application_thread_budget();
    let process_budget = machine.process_budget();
    let default_threads = (thread_budget / DEFAULT_THREAD_SHARE_DIVISOR).max(1);
    let default_processes = (process_budget / DEFAULT_PROCESS_SHARE_DIVISOR).max(1);

    let (processes, threads) = match (request.processes, request.threads) {
        (0, 0) => (default_processes.min(default_threads), default_threads),
        (0, threads) => (default_processes.min(threads), threads),
        (processes, 0) => (processes, default_threads.max(processes)),
        (processes, threads) => (processes, threads),
    };
    let memory_pages_per_process = if request.memory_pages_per_process == 0 {
        (machine.ram_pages / DEFAULT_PROCESS_MEMORY_DIVISOR).max(1)
    } else {
        request.memory_pages_per_process
    };

    // Structural: the request can never be granted on this kernel.
    if processes > machine.max_process_slots {
        return Err(structural(Limit::RootProcessCeiling, processes, machine.max_process_slots));
    }
    if processes > process_budget {
        return Err(structural(Limit::ApplicationProcessBudget, processes, process_budget));
    }
    if threads > thread_budget {
        return Err(structural(
            Limit::ApplicationThreadBudget, threads, thread_budget,
        ));
    }
    if processes > threads {
        return Err(structural(Limit::ProcessesExceedThreads, processes, threads));
    }
    if memory_pages_per_process > machine.ram_pages {
        return Err(structural(Limit::PhysicalMemory, memory_pages_per_process, machine.ram_pages));
    }
    if processes.checked_mul(memory_pages_per_process).is_none() {
        return Err(Refusal {
            errno: EOVERFLOW,
            limit: Limit::JobMemoryOverflow,
            requested: memory_pages_per_process,
            available: u64::MAX / processes,
        });
    }

    // Transient: other applications hold the budget, or slots/RAM are not free.
    let processes_left = process_budget.saturating_sub(committed.processes);
    if processes > processes_left {
        return Err(transient(Limit::ApplicationProcessBudget, processes, processes_left));
    }
    let threads_left = thread_budget.saturating_sub(committed.threads);
    if threads > threads_left {
        return Err(transient(Limit::ApplicationThreadBudget, threads, threads_left));
    }
    let slots_left = machine.reservable_slots().saturating_sub(committed.unused_threads);
    if threads > slots_left {
        return Err(transient(Limit::ReservableTaskSlots, threads, slots_left));
    }
    Ok(Grant { processes, threads, memory_pages_per_process })
}

// ---- Per-Job byte budgets --------------------------------------------------------

/// What loaderd installs on a Job besides its three counted limits: the
/// session ceiling, the `memory.high` threshold and the per-class kernel byte
/// budgets. Every value is a documented unit cost times a granted count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetPlan {
    pub processes: u64,
    /// Client-funded session ceiling.
    pub sessions: u64,
    /// `memory.high` threshold in pages.
    pub memory_high_pages: u64,
    /// `(JOB_KMEM_CLASS_*, bytes)` in installation order.
    pub classes: [(u32, u64); 7],
}

/// Derives the byte budgets of a Job whose granted counts are `processes`,
/// `threads` and `job_memory_pages` (`Grant::job_memory_pages`, or the tighter
/// limit an opt-in proof applies).
///
/// - `Thread` = threads x the observed kernel stack the kernel bills per thread,
///   so the thread count stays the binding limit.
/// - `HandleTable` / `IpcEndpoint` / `Vma` / `IpcMessage` = processes x the
///   per-process unit cost.
/// - `PageTable` = the dense table frames for the pages that can really be
///   resident (`min(job pages, physical pages)`), times the sparsity factor.
/// - `DeviceBuffer` = a fixed number of client quotas for the whole Job.
pub fn budget_plan(
    processes: u64, threads: u64, job_memory_pages: u64, machine: &Machine,
) -> BudgetPlan {
    let resident_pages = job_memory_pages.min(machine.ram_pages);
    let dense_frames = resident_pages.div_ceil(PAGES_PER_TABLE_FRAME).max(1);
    let page_table_bytes = dense_frames
        .saturating_mul(PAGE_TABLE_FRAME_BYTES)
        .saturating_mul(PAGE_TABLE_SPARSITY_FACTOR);
    BudgetPlan {
        processes,
        sessions: processes.saturating_mul(SESSIONS_PER_PROCESS).max(1),
        memory_high_pages: job_memory_pages.saturating_mul(MEMORY_HIGH_NUMERATOR)
            / MEMORY_HIGH_DENOMINATOR,
        classes: [
            (JOB_KMEM_CLASS_HANDLE_TABLE, processes.saturating_mul(HANDLE_TABLE_BYTES_PER_PROCESS)),
            (JOB_KMEM_CLASS_IPC_ENDPOINT, processes.saturating_mul(IPC_ENDPOINT_BYTES_PER_PROCESS)),
            (JOB_KMEM_CLASS_PAGE_TABLE, page_table_bytes),
            (JOB_KMEM_CLASS_THREAD, threads.saturating_mul(machine.kernel_stack_bytes)),
            (JOB_KMEM_CLASS_VMA, processes.saturating_mul(VMA_BYTES_PER_PROCESS)),
            (JOB_KMEM_CLASS_IPC_MESSAGE, processes.saturating_mul(IPC_MESSAGE_BYTES_PER_PROCESS)),
            (
                JOB_KMEM_CLASS_DEVICE_BUFFER,
                DEVICE_BUFFER_QUOTAS_PER_JOB.saturating_mul(APP_BUFFER_CLIENT_QUOTA_BYTES),
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB_PAGES: u64 = 1024 * MIB / PAGE_BYTES;

    /// Builds an observed machine with 16 KiB `Task` + 240 KiB stack (256 KiB
    /// footprint per thread for crisp power-of-2 budget math) unless overridden.
    fn machine(max_slots: u64, used_tasks: u64, ram_mib: u64) -> Machine {
        let usable = ram_mib * MIB;
        let footprint = 256 * KIB;
        let used_bytes = used_tasks.saturating_mul(footprint).min(usable);
        let reply = RustosTaskCapacityArgs {
            abi_version: TASK_CAPACITY_ABI_VERSION,
            max_task_slots: max_slots,
            max_process_slots: max_slots,
            live_tasks: used_tasks,
            live_processes: 10.min(max_slots),
            task_object_bytes: 16 * KIB,
            kernel_stack_bytes: 240 * KIB,
            usable_ram_bytes: usable,
            free_ram_bytes: usable - used_bytes,
            ..RustosTaskCapacityArgs::default()
        };
        Machine::observe(&reply, ram_mib * MIB).unwrap()
    }

    const NOTHING: Committed = Committed { processes: 0, threads: 0, unused_threads: 0 };

    fn req(processes: u64, threads: u64, pages: u64) -> ApplicationRequest {
        ApplicationRequest { processes, threads, memory_pages_per_process: pages }
    }

    #[test]
    fn observation_rejects_a_reply_that_cannot_describe_a_capacity() {
        let good = RustosTaskCapacityArgs {
            abi_version: TASK_CAPACITY_ABI_VERSION,
            max_task_slots: 16384,
            max_process_slots: 16384,
            live_tasks: 56,
            live_processes: 12,
            task_object_bytes: 16 * KIB,
            kernel_stack_bytes: 256 * KIB,
            usable_ram_bytes: 4096 * MIB,
            free_ram_bytes: 3072 * MIB,
            ..RustosTaskCapacityArgs::default()
        };
        assert!(Machine::observe(&good, 4096 * MIB).is_ok());
        let bad = [
            RustosTaskCapacityArgs { abi_version: TASK_CAPACITY_ABI_VERSION + 1, ..good },
            RustosTaskCapacityArgs { flags: 1, ..good },
            RustosTaskCapacityArgs { max_task_slots: 1, ..good },
            RustosTaskCapacityArgs { max_process_slots: 0, ..good },
            RustosTaskCapacityArgs { live_tasks: 16385, ..good },
            RustosTaskCapacityArgs { live_processes: 16385, ..good },
            RustosTaskCapacityArgs { task_object_bytes: 0, ..good },
            RustosTaskCapacityArgs { kernel_stack_bytes: 0, ..good },
            RustosTaskCapacityArgs { usable_ram_bytes: 0, ..good },
            RustosTaskCapacityArgs { free_ram_bytes: 4096 * MIB + 1, ..good },
        ];
        for reply in bad {
            assert_eq!(Machine::observe(&reply, 4096 * MIB), Err(EIO), "{reply:?}");
        }
        assert_eq!(Machine::observe(&good, 0), Err(EIO), "no physical memory observed");
    }

    #[test]
    fn ring3_policy_budgets_scale_with_memory_and_footprint_not_fixed_ratios() {
        // 128 MiB RAM, 256 KiB/thread -> half RAM = 64 MiB -> 256 threads, 64 processes.
        let small = machine(16384, 0, 128);
        assert_eq!(small.application_thread_budget(), 256);
        assert_eq!(small.process_budget(), 64);
        // 512 MiB RAM -> 1024 threads, 256 processes.
        let med = machine(16384, 0, 512);
        assert_eq!(med.application_thread_budget(), 1024);
        assert_eq!(med.process_budget(), 256);
        // Bounded below max_task_slots - 1 when directory ceiling is tighter.
        let tight_dir = machine(129, 0, 4096);
        assert_eq!(tight_dir.application_thread_budget(), 128);
        assert_eq!(tight_dir.process_budget(), 32);
    }

    #[test]
    fn defaults_derive_from_ring3_policy_and_observed_ram() {
        // 128 MiB -> thread budget 256, process budget 64. Default is 1/4 each.
        let narrow = grant(&machine(16384, 0, 128), &NOTHING, &ApplicationRequest::NONE).unwrap();
        assert_eq!(
            narrow,
            Grant { processes: 16, threads: 64, memory_pages_per_process: 64 * MIB / PAGE_BYTES }
        );
        // 512 MiB -> thread budget 1024, process budget 256.
        let wide = grant(&machine(16384, 0, 512), &NOTHING, &ApplicationRequest::NONE).unwrap();
        assert_eq!(
            wide,
            Grant { processes: 64, threads: 256, memory_pages_per_process: 256 * MIB / PAGE_BYTES }
        );
        // Tiny machine still yields usable non-zero defaults.
        let tiny = grant(&machine(8, 0, 4), &NOTHING, &ApplicationRequest::NONE).unwrap();
        assert_eq!((tiny.processes, tiny.threads), (1, 1));
    }

    #[test]
    fn a_request_is_granted_exactly_per_dimension_including_above_512_per_process() {
        let m = machine(16384, 0, 512);
        let only_threads = grant(&m, &NOTHING, &req(0, 96, 0)).unwrap();
        assert_eq!((only_threads.processes, only_threads.threads), (64, 96));
        let only_processes = grant(&m, &NOTHING, &req(30, 0, 0)).unwrap();
        assert_eq!((only_processes.processes, only_processes.threads), (30, 256));
        // A single process may exceed the retired 512 per-process ceiling.
        let single_proc_many = grant(&m, &NOTHING, &req(1, 768, 4096)).unwrap();
        assert_eq!(single_proc_many, Grant { processes: 1, threads: 768, memory_pages_per_process: 4096 });
        // Default processes never exceed granted threads.
        assert_eq!(grant(&m, &NOTHING, &req(0, 3, 0)).unwrap().processes, 3);
    }

    #[test]
    fn a_request_that_can_never_fit_names_the_structural_limit_as_einval() {
        // 128 MiB -> thread budget 256, process budget 64. Directory ceiling 48 < 64.
        let m_dir = machine(48, 0, 128);
        let error = grant(&m_dir, &NOTHING, &req(49, 49, 0)).unwrap_err();
        assert_eq!((error.errno, error.limit, error.requested, error.available),
            (EINVAL, Limit::RootProcessCeiling, 49, 48));

        let m = machine(16384, 0, 128);
        let refused = |request| grant(&m, &NOTHING, &request).unwrap_err();
        // Above the application process budget (64).
        let error = refused(req(65, 65, 0));
        assert_eq!((error.errno, error.limit, error.available),
            (EINVAL, Limit::ApplicationProcessBudget, 64));
        // Above the application thread budget (256): never silently clamped.
        let error = refused(req(0, 257, 0));
        assert_eq!((error.errno, error.limit, error.requested, error.available),
            (EINVAL, Limit::ApplicationThreadBudget, 257, 256));
        // More processes than threads.
        let error = refused(req(10, 4, 0));
        assert_eq!((error.errno, error.limit), (EINVAL, Limit::ProcessesExceedThreads));
        // Pages per process above physical memory.
        let pages_128m = 128 * MIB / PAGE_BYTES;
        let error = refused(req(0, 0, pages_128m + 1));
        assert_eq!((error.errno, error.limit), (EINVAL, Limit::PhysicalMemory));
        assert!(grant(&m, &NOTHING, &req(0, 0, pages_128m)).is_ok());
    }

    #[test]
    fn a_structurally_impossible_request_is_einval_even_when_the_budget_is_taken() {
        let m = machine(16384, 0, 128);
        let busy = Committed { processes: 64, threads: 256, unused_threads: 256 };
        assert_eq!(grant(&m, &busy, &req(0, 257, 0)).unwrap_err().errno, EINVAL);
    }

    #[test]
    fn a_request_other_applications_leave_no_room_for_is_eagain_naming_the_binding_limit() {
        let m = machine(16384, 0, 128);
        let held = Committed { processes: 10, threads: 200, unused_threads: 10 };
        // 256 - 200 = 56 threads left.
        let error = grant(&m, &held, &req(0, 57, 0)).unwrap_err();
        assert_eq!((error.errno, error.limit, error.requested, error.available),
            (EAGAIN, Limit::ApplicationThreadBudget, 57, 56));
        assert!(grant(&m, &held, &req(0, 56, 0)).is_ok(), "exactly what is left is grantable");
        // 64 - 10 = 54 processes left.
        let error = grant(&m, &held, &req(55, 55, 0)).unwrap_err();
        assert_eq!((error.errno, error.limit, error.available),
            (EAGAIN, Limit::ApplicationProcessBudget, 54));
        assert!(grant(&m, &held, &req(54, 54, 0)).is_ok());
        // The default (64 threads) is refused, never shrunk.
        let nearly = Committed { processes: 0, threads: 240, unused_threads: 0 };
        let error = grant(&m, &nearly, &ApplicationRequest::NONE).unwrap_err();
        assert_eq!((error.errno, error.limit, error.requested, error.available),
            (EAGAIN, Limit::ApplicationThreadBudget, 64, 16));
    }

    #[test]
    fn slots_or_memory_not_free_now_refuse_even_inside_the_budget() {
        // 128 MiB = 512 footprints total, 472 used => only 40 free footprints now,
        // and live Jobs have already been promised 25 of them.
        let m = machine(16384, 472, 128);
        assert_eq!(m.reservable_slots(), 40);
        let promised = Committed { processes: 4, threads: 40, unused_threads: 25 };
        let error = grant(&m, &promised, &req(0, 16, 0)).unwrap_err();
        assert_eq!((error.errno, error.limit, error.requested, error.available),
            (EAGAIN, Limit::ReservableTaskSlots, 16, 15));
        assert!(grant(&m, &promised, &req(0, 15, 0)).is_ok());
    }

    #[test]
    fn the_sum_of_grants_never_exceeds_the_application_thread_budget() {
        for ram_mib in [64, 128, 512] {
            let m = machine(16384, 0, ram_mib);
            let budget = m.application_thread_budget();
            let mut committed = NOTHING;
            let mut admitted = 0;
            for step in 0..4096_u64 {
                let request = match step % 5 {
                    0 => ApplicationRequest::NONE,
                    1 => req(0, 1 + (step * 7) % budget, 0),
                    2 => req(1 + step % 8, 0, 0),
                    3 => req(2, 1 + (step * 13) % (budget / 2).max(1), 0),
                    _ => req(0, 1, 0),
                };
                if let Ok(granted) = grant(&m, &committed, &request) {
                    committed.threads += granted.threads;
                    committed.processes += granted.processes;
                    admitted += 1;
                    assert!(committed.threads <= budget, "ram_mib={ram_mib}");
                    assert!(committed.processes <= m.process_budget(), "ram_mib={ram_mib}");
                }
            }
            assert!(admitted > 1, "ram_mib={ram_mib}: the mix admits several Jobs");
            let left = budget - committed.threads;
            assert!(grant(&m, &committed, &req(0, left + 1, 0)).is_err());
        }
    }

    #[test]
    fn a_job_memory_total_that_overflows_is_refused_not_wrapped() {
        let m = Machine { ram_pages: u64::MAX, usable_ram_bytes: u64::MAX, free_ram_bytes: u64::MAX, ..machine(16384, 0, 128) };
        let error = grant(&m, &NOTHING, &req(48, 48, u64::MAX / 2)).unwrap_err();
        assert_eq!((error.errno, error.limit), (EOVERFLOW, Limit::JobMemoryOverflow));
    }

    #[test]
    fn budgets_are_unit_costs_times_the_granted_counts() {
        let m = Machine {
            task_object_bytes: 16 * KIB,
            kernel_stack_bytes: 2 * MIB,
            ..machine(16384, 0, 4096)
        };
        let granted = grant(&m, &NOTHING, &req(16, 128, 0)).unwrap();
        let plan = budget_plan(granted.processes, granted.threads, granted.job_memory_pages(), &m);
        let class = |id: u32| plan.classes.iter().find(|(c, _)| *c == id).unwrap().1;
        assert_eq!(class(JOB_KMEM_CLASS_THREAD), 128 * 2 * MIB);
        assert_eq!(class(JOB_KMEM_CLASS_HANDLE_TABLE), 16 * HANDLE_TABLE_BYTES_PER_PROCESS);
        assert_eq!(class(JOB_KMEM_CLASS_IPC_ENDPOINT), 16 * IPC_ENDPOINT_BYTES_PER_PROCESS);
        assert_eq!(class(JOB_KMEM_CLASS_VMA), 16 * VMA_BYTES_PER_PROCESS);
        assert_eq!(class(JOB_KMEM_CLASS_IPC_MESSAGE), 16 * IPC_MESSAGE_BYTES_PER_PROCESS);
        assert_eq!(class(JOB_KMEM_CLASS_DEVICE_BUFFER), 2 * APP_BUFFER_CLIENT_QUOTA_BYTES);
        assert_eq!(
            class(JOB_KMEM_CLASS_PAGE_TABLE),
            (4 * GIB_PAGES / PAGES_PER_TABLE_FRAME) * PAGE_TABLE_FRAME_BYTES * PAGE_TABLE_SPARSITY_FACTOR
        );
        assert_eq!(plan.sessions, 16 * SESSIONS_PER_PROCESS);
        assert_eq!(plan.memory_high_pages, granted.job_memory_pages() * 4 / 5);
        assert_eq!(plan.classes.len(), 7);
    }

    #[test]
    fn budgets_follow_the_grant_and_the_observed_stack_not_a_fixed_table() {
        let m = machine(16384, 0, 4096);
        let small = budget_plan(2, 4, 2048, &m);
        let large = budget_plan(8, 16, 2048, &m);
        let class = |plan: &BudgetPlan, id: u32| plan.classes.iter().find(|(c, _)| *c == id).unwrap().1;
        assert_eq!(class(&large, JOB_KMEM_CLASS_HANDLE_TABLE), 4 * class(&small, JOB_KMEM_CLASS_HANDLE_TABLE));
        assert_eq!(class(&large, JOB_KMEM_CLASS_THREAD), 4 * class(&small, JOB_KMEM_CLASS_THREAD));
        // A different kernel stack size moves only the thread class.
        let fat = Machine { kernel_stack_bytes: 512 * KIB, ..m };
        let plan = budget_plan(2, 4, 2048, &fat);
        assert_eq!(class(&plan, JOB_KMEM_CLASS_THREAD), 4 * 512 * KIB);
        assert_eq!(class(&plan, JOB_KMEM_CLASS_HANDLE_TABLE), class(&small, JOB_KMEM_CLASS_HANDLE_TABLE));
        assert!(class(&small, JOB_KMEM_CLASS_PAGE_TABLE) < class(&budget_plan(2, 4, 4 * GIB_PAGES, &m), JOB_KMEM_CLASS_PAGE_TABLE));
        for (_, bytes) in budget_plan(1, 1, 1, &m).classes {
            assert!(bytes > 0);
        }
    }

    #[test]
    fn refusals_render_the_limit_the_requested_and_the_available_amount() {
        let error = grant(&machine(16384, 0, 128), &NOTHING, &req(0, 257, 0)).unwrap_err();
        let text = std::format!("{error}");
        assert_eq!(text, "limit=application-thread-budget requested=257 available=256 errno=22");
    }
}
