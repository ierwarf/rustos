//! Application Job grants: Ring3 policy that turns an application's *request*
//! (`ApplicationRequest`) into a *grant* against the capacity observed at launch.
//!
//! - **Owner:** `loaderd`. The kernel owns capacity (`kernel-ps`
//!   `multitask::task_capacity`, observed through `SYS_RUSTOS_TASK_CAPACITY`) and
//!   enforces whatever Job limits Ring3 installs; it does not choose them.
//! - **Boundary:** no number here guesses the machine. Every bound is either an
//!   observed value (`Machine`) or a named ratio/unit cost of one. The ratios and
//!   unit costs are policy and are listed below with their meaning.
//! - **Observed inputs** (`Machine`): `total_slots`, `application_thread_budget`,
//!   `process_thread_ceiling`, `root_process_ceiling`, `kernel_stack_bytes`,
//!   `reservable_slots` (all from `SYS_RUSTOS_TASK_CAPACITY`) and physical
//!   memory (`sysinfo`).
//! - **Request semantics: exact.** A requested dimension is the amount the
//!   application needs and is granted exactly or the launch is refused. A grant
//!   is never rounded, clamped or partially honoured, and a request is never
//!   "a minimum" that is silently raised: granting more than asked would take
//!   budget from other applications for nothing. An absent dimension takes the
//!   derived default below.
//! - **Defaults** (undeclared dimension; `B` = `application_thread_budget`,
//!   `P` = `process_budget`, `R` = physical pages):
//!   threads `max(1, B / DEFAULT_THREAD_SHARE_DIVISOR)`, raised to one thread per
//!   requested process; processes `max(1, P / DEFAULT_PROCESS_SHARE_DIVISOR)`,
//!   never more than the granted threads; pages per process
//!   `max(1, R / DEFAULT_PROCESS_MEMORY_DIVISOR)`.
//! - **Process budget:** applications own the same fraction of the root Job's
//!   process slots as of task slots: `P = root_process_ceiling *
//!   application_thread_budget / total_slots`; boot services own the rest.
//! - **Sum-of-grants invariant:** every thread a Job may run occupies one
//!   scheduler task slot, so the threads granted to all *live* application Jobs
//!   (including ones still being retired) never exceed `B`: a new grant is
//!   admitted only if `committed_threads + threads <= B`, where `committed` is
//!   summed by `ApplicationJobs` from the grants it still holds. Processes are
//!   tracked against `P` the same way. Memory is different by design: page limits
//!   are ceilings over one shared pool that the kernel frame ledger enforces
//!   (over-commit is intended), so they are checked against physical memory
//!   only, never summed.
//! - **Refusal:** structural refusals (the request can never fit this kernel)
//!   are `EINVAL`; transient ones (other applications hold the budget, or the
//!   slots are not free right now) are `EAGAIN`. Every refusal names the limit
//!   (`Limit::name`), the requested and the available amount.
//! - **Per-Job byte budgets** (`budget_plan`) are documented unit costs times the
//!   granted counts, so a budget can never disagree with the grant it serves.
//! - **Forbidden:** a literal capacity, a clamp, a fake success for a request
//!   that cannot be granted in full.

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
pub const SESSIONS_PER_PROCESS: u64 = 4;
/// `memory.high` pressure telemetry fires at this fraction of the Job's page
/// limit (numerator/denominator).
pub const MEMORY_HIGH_NUMERATOR: u64 = 4;
pub const MEMORY_HIGH_DENOMINATOR: u64 = 5;

// ---- Observation ---------------------------------------------------------------

/// What loaderd observes of the running machine at one launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Machine {
    pub total_slots: u64,
    pub application_thread_budget: u64,
    pub process_thread_ceiling: u64,
    pub root_process_ceiling: u64,
    pub kernel_stack_bytes: u64,
    /// Slots a new thread could reserve at the instant of observation.
    pub reservable_slots: u64,
    /// Physical memory in pages (from `sysinfo`).
    pub ram_pages: u64,
}

impl Machine {
    /// Validates a `SYS_RUSTOS_TASK_CAPACITY` reply plus physical memory. A
    /// reply that cannot describe a usable capacity is `EIO` (the kernel broke
    /// its own contract), never silently "unlimited".
    pub fn observe(reply: &RustosTaskCapacityArgs, ram_bytes: u64) -> Result<Self, i32> {
        let usable = reply.abi_version == TASK_CAPACITY_ABI_VERSION
            && reply.flags == 0
            && reply.total_slots != 0
            && reply.application_thread_budget != 0
            && reply.application_thread_budget <= reply.total_slots
            && reply.process_thread_ceiling != 0
            && reply.root_process_ceiling != 0
            && reply.kernel_stack_bytes != 0
            && reply.reservable_slots <= reply.total_slots
            && ram_bytes >= PAGE_BYTES;
        if !usable {
            return Err(EIO);
        }
        Ok(Self {
            total_slots: reply.total_slots,
            application_thread_budget: reply.application_thread_budget,
            process_thread_ceiling: reply.process_thread_ceiling,
            root_process_ceiling: reply.root_process_ceiling,
            kernel_stack_bytes: reply.kernel_stack_bytes,
            reservable_slots: reply.reservable_slots,
            ram_pages: ram_bytes / PAGE_BYTES,
        })
    }

    /// Process slots applications may hold between them: the same fraction of
    /// the root Job's process slots as of task slots.
    pub fn process_budget(&self) -> u64 {
        let product = u128::from(self.root_process_ceiling)
            * u128::from(self.application_thread_budget)
            / u128::from(self.total_slots);
        // `application_thread_budget <= total_slots`, so this fits.
        (product as u64).max(1)
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
    /// Processes above the root Job's process ceiling.
    RootProcessCeiling,
    /// Processes above the application share of the root Job's slots, or what
    /// other live applications leave of it.
    ApplicationProcessBudget,
    /// Threads above the application thread budget, or what other live
    /// applications leave of it.
    ApplicationThreadBudget,
    /// Slots that are free right now, net of those promised to live Jobs.
    ReservableTaskSlots,
    /// More threads than the granted processes can hold at the per-process
    /// structural ceiling.
    ProcessThreadCeiling,
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
            Self::ProcessThreadCeiling => "process-thread-ceiling",
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
    let process_budget = machine.process_budget();
    let default_threads = (machine.application_thread_budget / DEFAULT_THREAD_SHARE_DIVISOR).max(1);
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
    if processes > machine.root_process_ceiling {
        return Err(structural(Limit::RootProcessCeiling, processes, machine.root_process_ceiling));
    }
    if processes > process_budget {
        return Err(structural(Limit::ApplicationProcessBudget, processes, process_budget));
    }
    if threads > machine.application_thread_budget {
        return Err(structural(
            Limit::ApplicationThreadBudget, threads, machine.application_thread_budget,
        ));
    }
    if processes > threads {
        return Err(structural(Limit::ProcessesExceedThreads, processes, threads));
    }
    let thread_capacity = processes.saturating_mul(machine.process_thread_ceiling);
    if threads > thread_capacity {
        return Err(structural(Limit::ProcessThreadCeiling, threads, thread_capacity));
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

    // Transient: other applications hold the budget, or the slots are not free.
    let processes_left = process_budget.saturating_sub(committed.processes);
    if processes > processes_left {
        return Err(transient(Limit::ApplicationProcessBudget, processes, processes_left));
    }
    let threads_left = machine.application_thread_budget.saturating_sub(committed.threads);
    if threads > threads_left {
        return Err(transient(Limit::ApplicationThreadBudget, threads, threads_left));
    }
    let slots_left = machine.reservable_slots.saturating_sub(committed.unused_threads);
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

    const MIB: u64 = 1024 * 1024;
    const GIB_PAGES: u64 = 1024 * 1024 * 1024 / PAGE_BYTES;

    /// Capacity the kernel derives for a table of `total` slots (the kernel's
    /// formulas: a quarter held back, half per process, a quarter of the table
    /// as root processes), with `used` slots live and `ram_gib` of memory.
    fn machine(total: u64, used: u64, ram_gib: u64) -> Machine {
        let reply = RustosTaskCapacityArgs {
            abi_version: TASK_CAPACITY_ABI_VERSION,
            total_slots: total,
            kernel_reserved_slots: total / 4,
            application_thread_budget: total - total / 4,
            live_tasks: used,
            reservable_slots: total - used,
            kernel_stack_bytes: 2 * MIB,
            process_thread_ceiling: total / 2,
            root_process_ceiling: total / 4,
            ..RustosTaskCapacityArgs::default()
        };
        Machine::observe(&reply, ram_gib * 1024 * MIB).unwrap()
    }

    const NOTHING: Committed = Committed { processes: 0, threads: 0, unused_threads: 0 };

    fn req(processes: u64, threads: u64, pages: u64) -> ApplicationRequest {
        ApplicationRequest { processes, threads, memory_pages_per_process: pages }
    }

    #[test]
    fn observation_rejects_a_reply_that_cannot_describe_a_capacity() {
        let good = RustosTaskCapacityArgs {
            abi_version: TASK_CAPACITY_ABI_VERSION,
            total_slots: 256,
            application_thread_budget: 192,
            reservable_slots: 100,
            kernel_stack_bytes: 2 * MIB,
            process_thread_ceiling: 128,
            root_process_ceiling: 64,
            ..RustosTaskCapacityArgs::default()
        };
        assert!(Machine::observe(&good, 4096 * MIB).is_ok());
        let bad = [
            RustosTaskCapacityArgs { abi_version: TASK_CAPACITY_ABI_VERSION + 1, ..good },
            RustosTaskCapacityArgs { flags: 1, ..good },
            RustosTaskCapacityArgs { total_slots: 0, ..good },
            RustosTaskCapacityArgs { application_thread_budget: 0, ..good },
            RustosTaskCapacityArgs { application_thread_budget: 257, ..good },
            RustosTaskCapacityArgs { process_thread_ceiling: 0, ..good },
            RustosTaskCapacityArgs { root_process_ceiling: 0, ..good },
            RustosTaskCapacityArgs { kernel_stack_bytes: 0, ..good },
            RustosTaskCapacityArgs { reservable_slots: 257, ..good },
        ];
        for reply in bad {
            assert_eq!(Machine::observe(&reply, 4096 * MIB), Err(EIO), "{reply:?}");
        }
        assert_eq!(Machine::observe(&good, 0), Err(EIO), "no physical memory observed");
    }

    #[test]
    fn the_process_budget_is_the_application_fraction_of_the_root_job() {
        assert_eq!(machine(256, 0, 4).process_budget(), 48);
        assert_eq!(machine(1024, 0, 4).process_budget(), 192);
        assert_eq!(machine(64, 0, 4).process_budget(), 12);
        assert_eq!(machine(4, 0, 4).process_budget(), 1, "never zero");
    }

    #[test]
    fn defaults_are_ratios_of_the_observed_capacity() {
        // T = 256: budget 192, process budget 48, 4 GiB.
        let narrow = grant(&machine(256, 0, 4), &NOTHING, &ApplicationRequest::NONE).unwrap();
        assert_eq!(
            narrow,
            Grant { processes: 12, threads: 48, memory_pages_per_process: 2 * GIB_PAGES }
        );
        // The same formulas on a wider table and more memory move together.
        let wide = grant(&machine(1024, 0, 16), &NOTHING, &ApplicationRequest::NONE).unwrap();
        assert_eq!(
            wide,
            Grant { processes: 48, threads: 192, memory_pages_per_process: 8 * GIB_PAGES }
        );
        // A tiny table still yields a usable, non-zero default.
        let tiny = grant(&machine(8, 0, 1), &NOTHING, &ApplicationRequest::NONE).unwrap();
        assert_eq!((tiny.processes, tiny.threads), (1, 1));
    }

    #[test]
    fn a_request_is_granted_exactly_per_dimension() {
        let m = machine(256, 0, 4);
        let only_threads = grant(&m, &NOTHING, &req(0, 96, 0)).unwrap();
        assert_eq!((only_threads.processes, only_threads.threads), (12, 96));
        let only_processes = grant(&m, &NOTHING, &req(30, 0, 0)).unwrap();
        // The default thread share is raised to one thread per requested process.
        assert_eq!((only_processes.processes, only_processes.threads), (30, 48));
        let more_processes = grant(&m, &NOTHING, &req(48, 0, 0)).unwrap();
        assert_eq!((more_processes.processes, more_processes.threads), (48, 48));
        let exact = grant(&m, &NOTHING, &req(16, 128, 4096)).unwrap();
        assert_eq!(exact, Grant { processes: 16, threads: 128, memory_pages_per_process: 4096 });
        // Default processes never exceed granted threads.
        assert_eq!(grant(&m, &NOTHING, &req(0, 3, 0)).unwrap().processes, 3);
    }

    #[test]
    fn a_request_that_can_never_fit_names_the_structural_limit_as_einval() {
        let m = machine(256, 0, 4);
        let refused = |request| grant(&m, &NOTHING, &request).unwrap_err();
        // Above the root Job's process ceiling (64).
        let error = refused(req(65, 0, 0));
        assert_eq!((error.errno, error.limit, error.requested, error.available),
            (EINVAL, Limit::RootProcessCeiling, 65, 64));
        // Within the root ceiling but above the application share (48).
        let error = refused(req(49, 0, 0));
        assert_eq!((error.errno, error.limit, error.available),
            (EINVAL, Limit::ApplicationProcessBudget, 48));
        // Above the application thread budget (192): never silently clamped.
        let error = refused(req(0, 193, 0));
        assert_eq!((error.errno, error.limit, error.requested, error.available),
            (EINVAL, Limit::ApplicationThreadBudget, 193, 192));
        // More processes than threads.
        let error = refused(req(10, 4, 0));
        assert_eq!((error.errno, error.limit), (EINVAL, Limit::ProcessesExceedThreads));
        // More threads than the processes can hold (one process: 128).
        let error = refused(req(1, 129, 0));
        assert_eq!((error.errno, error.limit, error.available),
            (EINVAL, Limit::ProcessThreadCeiling, 128));
        assert!(grant(&m, &NOTHING, &req(1, 128, 0)).is_ok(), "the ceiling itself is grantable");
        // Pages per process above physical memory.
        let error = refused(req(0, 0, 4 * GIB_PAGES + 1));
        assert_eq!((error.errno, error.limit), (EINVAL, Limit::PhysicalMemory));
        assert!(grant(&m, &NOTHING, &req(0, 0, 4 * GIB_PAGES)).is_ok());
    }

    #[test]
    fn a_structurally_impossible_request_is_einval_even_when_the_budget_is_taken() {
        let m = machine(256, 0, 4);
        let busy = Committed { processes: 48, threads: 192, unused_threads: 192 };
        assert_eq!(grant(&m, &busy, &req(0, 193, 0)).unwrap_err().errno, EINVAL);
    }

    #[test]
    fn a_request_other_applications_leave_no_room_for_is_eagain_naming_the_binding_limit() {
        let m = machine(256, 0, 4);
        let held = Committed { processes: 10, threads: 150, unused_threads: 10 };
        // 192 - 150 = 42 threads left.
        let error = grant(&m, &held, &req(0, 43, 0)).unwrap_err();
        assert_eq!((error.errno, error.limit, error.requested, error.available),
            (EAGAIN, Limit::ApplicationThreadBudget, 43, 42));
        assert!(grant(&m, &held, &req(0, 42, 0)).is_ok(), "exactly what is left is grantable");
        // 48 - 10 = 38 processes left.
        let error = grant(&m, &held, &req(39, 40, 0)).unwrap_err();
        assert_eq!((error.errno, error.limit, error.available),
            (EAGAIN, Limit::ApplicationProcessBudget, 38));
        assert!(grant(&m, &held, &req(38, 40, 0)).is_ok());
        // The default is judged the same way: it is refused, never shrunk.
        let nearly = Committed { processes: 0, threads: 180, unused_threads: 0 };
        let error = grant(&m, &nearly, &ApplicationRequest::NONE).unwrap_err();
        assert_eq!((error.errno, error.limit, error.requested, error.available),
            (EAGAIN, Limit::ApplicationThreadBudget, 48, 12));
    }

    #[test]
    fn slots_that_are_not_free_now_refuse_even_inside_the_budget() {
        // Boot services hold more slots than the kernel headroom: only 40 free,
        // and live Jobs have already been promised 25 of them.
        let m = machine(256, 216, 4);
        assert_eq!(m.reservable_slots, 40);
        let promised = Committed { processes: 4, threads: 40, unused_threads: 25 };
        let error = grant(&m, &promised, &req(0, 16, 0)).unwrap_err();
        assert_eq!((error.errno, error.limit, error.requested, error.available),
            (EAGAIN, Limit::ReservableTaskSlots, 16, 15));
        assert!(grant(&m, &promised, &req(0, 15, 0)).is_ok());
    }

    #[test]
    fn the_sum_of_grants_never_exceeds_the_application_thread_budget() {
        for total in [64, 256, 1024] {
            let m = machine(total, 0, 4);
            let mut committed = NOTHING;
            let mut admitted = 0;
            // A deterministic mix of defaults and explicit requests until the
            // budget is exhausted; every admitted grant is added to `committed`
            // exactly as ApplicationJobs does.
            for step in 0..4096_u64 {
                let request = match step % 5 {
                    0 => ApplicationRequest::NONE,
                    1 => req(0, 1 + (step * 7) % total, 0),
                    2 => req(1 + step % 8, 0, 0),
                    3 => req(2, 1 + (step * 13) % (total / 2), 0),
                    _ => req(0, 1, 0),
                };
                if let Ok(granted) = grant(&m, &committed, &request) {
                    committed.threads += granted.threads;
                    committed.processes += granted.processes;
                    admitted += 1;
                    assert!(committed.threads <= m.application_thread_budget, "total={total}");
                    assert!(committed.processes <= m.process_budget(), "total={total}");
                }
            }
            assert!(admitted > 1, "total={total}: the mix admits several Jobs");
            // Whatever is left is exactly what a further request may still take.
            let left = m.application_thread_budget - committed.threads;
            assert!(grant(&m, &committed, &req(0, left + 1, 0)).is_err());
        }
    }

    #[test]
    fn a_job_memory_total_that_overflows_is_refused_not_wrapped() {
        let m = Machine { ram_pages: u64::MAX, ..machine(256, 0, 4) };
        let error = grant(&m, &NOTHING, &req(48, 48, u64::MAX / 2)).unwrap_err();
        assert_eq!((error.errno, error.limit), (EOVERFLOW, Limit::JobMemoryOverflow));
    }

    #[test]
    fn budgets_are_unit_costs_times_the_granted_counts() {
        let m = machine(256, 0, 4);
        let granted = grant(&m, &NOTHING, &req(16, 128, 0)).unwrap();
        let plan = budget_plan(granted.processes, granted.threads, granted.job_memory_pages(), &m);
        let class = |id: u32| plan.classes.iter().find(|(c, _)| *c == id).unwrap().1;
        assert_eq!(class(JOB_KMEM_CLASS_THREAD), 128 * 2 * MIB);
        assert_eq!(class(JOB_KMEM_CLASS_HANDLE_TABLE), 16 * HANDLE_TABLE_BYTES_PER_PROCESS);
        assert_eq!(class(JOB_KMEM_CLASS_IPC_ENDPOINT), 16 * IPC_ENDPOINT_BYTES_PER_PROCESS);
        assert_eq!(class(JOB_KMEM_CLASS_VMA), 16 * VMA_BYTES_PER_PROCESS);
        assert_eq!(class(JOB_KMEM_CLASS_IPC_MESSAGE), 16 * IPC_MESSAGE_BYTES_PER_PROCESS);
        assert_eq!(class(JOB_KMEM_CLASS_DEVICE_BUFFER), 2 * APP_BUFFER_CLIENT_QUOTA_BYTES);
        // 4 GiB of physical pages: dense frames x 4 KiB x sparsity.
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
        let m = machine(256, 0, 4);
        let small = budget_plan(2, 4, 2048, &m);
        let large = budget_plan(8, 16, 2048, &m);
        let class = |plan: &BudgetPlan, id: u32| plan.classes.iter().find(|(c, _)| *c == id).unwrap().1;
        assert_eq!(class(&large, JOB_KMEM_CLASS_HANDLE_TABLE), 4 * class(&small, JOB_KMEM_CLASS_HANDLE_TABLE));
        assert_eq!(class(&large, JOB_KMEM_CLASS_THREAD), 4 * class(&small, JOB_KMEM_CLASS_THREAD));
        // A different kernel stack size moves only the thread class.
        let fat = Machine { kernel_stack_bytes: 4 * MIB, ..m };
        let plan = budget_plan(2, 4, 2048, &fat);
        assert_eq!(class(&plan, JOB_KMEM_CLASS_THREAD), 4 * 4 * MIB);
        assert_eq!(class(&plan, JOB_KMEM_CLASS_HANDLE_TABLE), class(&small, JOB_KMEM_CLASS_HANDLE_TABLE));
        // A Job whose limit is tighter than physical memory needs fewer table frames.
        assert!(class(&small, JOB_KMEM_CLASS_PAGE_TABLE) < class(&budget_plan(2, 4, 4 * GIB_PAGES, &m), JOB_KMEM_CLASS_PAGE_TABLE));
        // Never a zero budget, whatever the counts.
        for (_, bytes) in budget_plan(1, 1, 1, &m).classes {
            assert!(bytes > 0);
        }
    }

    #[test]
    fn refusals_render_the_limit_the_requested_and_the_available_amount() {
        let error = grant(&machine(256, 0, 4), &NOTHING, &req(0, 193, 0)).unwrap_err();
        let text = std::format!("{error}");
        assert_eq!(text, "limit=application-thread-budget requested=193 available=192 errno=22");
    }
}
