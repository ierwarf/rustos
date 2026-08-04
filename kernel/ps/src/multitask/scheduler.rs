//! SMP task scheduler, continuation ownership, blocking, and retirement.
//!
//! - **Owner:** `kernel-ps` owns task slots, scheduling state, and exact task
//!   retirement; services own launch and workload policy.
//! - **Boundary:** Saved CPU contexts, wake tokens, deadlines, donation hints,
//!   and process generations converge here.
//! - **Lifecycle:** A task moves through suspended/runnable/running,
//!   arm/block/wake, and retired/reap-pending states with one exact epoch.
//!   A running task has consumed its published continuation; only a trapped or
//!   blocked task owns a frame that may be validated for dispatch.
//! - **Concurrency:** the lifecycle catalog currently serializes rare task
//!   mutation while per-CPU current/transition slots publish execution
//!   ownership.  A private CPU-local scratch slot carries an in-progress
//!   dispatch turn; the catalog has no shared mutable current-task field.
//! - **Failure:** Invalid published contexts quarantine and retire a user task;
//!   raced wake/cancel refuses sleep; cleanup must acknowledge every
//!   task-scoped registry before slot reuse.
//! - **Forbidden:** No split commit/yield, current-running-frame validation,
//!   affinity-escaping dispatch, unbounded external wait, or scheduler policy
//!   IPC.
//! - **Evidence:** `scheduler-lifecycle`, `scheduler-dispatch`,
//!   `process-address-space-lifecycle`, `exception-retirement`,
//!   `endpoint-lifecycle`, `task-affinity-lifecycle`, and
//!   `syscall-simd-lifecycle`.
mod affinity;
pub use affinity::{AffinityCommit, AffinityError, ProcessAffinitySnapshot};
#[cfg(test)]
mod activation_batch_tests;
mod context_validation;
mod dispatch_policy;
mod handoff_queue;
mod handoffs;
mod ipc_donation;
mod linux_thread_state;
mod locality;
mod reclaim;
mod runqueue;
mod runqueue_policy;
mod runtime_profile;
pub(in crate::multitask) use runtime_profile::SchedulerEntryCause;
pub use runtime_profile::drain_scheduler_runtime_profile;
pub(in crate::multitask) use runtime_profile::publish_scheduler_runtime_profile;

pub(in crate::multitask) fn local_dispatch_work_pending(cpu: usize) -> bool {
    runqueue::local_dispatch_work_pending(cpu)
}

mod smp;
#[cfg(test)]
mod synchronous_handoff_tests;
mod thread_slots;

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use core::{mem, ptr};

use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{DS, ES, FS, GS, Segment};
use x86_64::registers::model_specific::FsBase;
use x86_64::structures::gdt::SegmentSelector;

use crate::arch::simd::{SimdState, restore_state, save_state};
use crate::debug;
use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging::ProcessAddressSpace;
use crate::user::abi::UserAbi;
use crate::user::linux::LinuxThreadState;
use crate::user::process;
use crate::user::process_state::{UserProcessState, WindowsThreadRuntimeState};

use super::context::{SAVED_CONTEXT_BYTES, SavedContext};
use super::current_identity::{self, TaskIdentity};
use super::process_table::{self, ProcessHandle};
use super::{UserFaultDisposition, UserStackState, UserTaskBootstrap, initial_task_rflags};
use context_validation::context_validation_reason_code;
use dispatch_policy::{CpuDispatchGuard, CpuDispatchLock, CpuDispatchPolicy};
use runtime_profile::SchedulerPhase;
use ipc_donation::{IpcDonationTarget, IpcPriorityDonation};
use handoff_queue::SlotHandoffQueue;
pub(super) use linux_thread_state::CurrentLinuxThreadBinding;
use linux_thread_state::{LinuxThreadStateLock, empty_linux_thread_state_lock};
use reclaim::{RetiredSlotReclaim, RetirementSideEffect};

// The enabled product topology boots roughly twenty policy/service processes
// before the UI creates its bounded input, display, diagnostics, console, and
// Wayland workers. A 32-slot table therefore exhausted during normal shell
// launch and turned a recoverable capacity error into uiserver thread-spawn
// panic. Keep the scheduler allocation-free and explicitly bounded, but size
// the product contract for service growth and application headroom.
pub(super) const MAX_TASK: usize = 128;
/// Sentinel for an unresolved donor-slot hint. `MAX_TASK` fits in `u8`, so the
/// hint table stays one cache-friendly byte per donation entry.
const NO_SLOT_HINT: u8 = u8::MAX;
const _: () = assert!(kernel_ipc_runtime::api::MAX_ENDPOINT_WAKE_TASKS >= MAX_TASK);
const ROOT_TASK_SLOT: usize = 0;
const FIRST_DYNAMIC_TASK_SLOT: usize = 1;
const NO_IDLE_CPU: u8 = u8::MAX;

// Kernel worker threads run fairly deep Rust call chains during process/module
// bring-up, so smaller stacks can corrupt adjacent task stacks.
// Kernel-side Rust paths during syscall, process load, and service orchestration are deep in
// no-opt/debug builds. Keep a generous per-task kernel stack so corrupted return frames do not
// silently spill into adjacent scheduler storage.
const TASK_STACK_SIZE: usize = 2 * 1024 * 1024;
const TASK_STACK_GUARD_BYTES: usize = 256;
const STACK_CANARY_WORD: u64 = 0x5343_4844_554c_4552;
const TASK_ENTRY_STACK_RESERVE_QWORDS: usize = 3;
const PAGE_FAULT_VECTOR: u8 = 14;
const RFLAGS_RESERVED_BIT_1: u64 = 1 << 1;
const LONG_READY_WAIT_THRESHOLD_MS: u64 = 50;
const LONG_BLOCKED_WAIT_THRESHOLD_MS: u64 = 250;

// CFS-like fairness constants (mirrors Linux kernel/sched/fair.c).
// NICE_0_LOAD is the nominal weight; vruntime delta = elapsed * NICE_0_LOAD / weight.
// Smaller weight -> larger vruntime per real-time unit -> less CPU share.
const NICE_0_LOAD: u32 = 1024;
const MIN_LOAD_WEIGHT: u32 = 32;
const MAX_LOAD_WEIGHT: u32 = 1_000_000;
const SYSTEM_CLASS_WEIGHT_FLAG: u32 = 1 << 31;
const LOAD_WEIGHT_MASK: u32 = !SYSTEM_CLASS_WEIGHT_FLAG;
const INTERACTIVE_PIT_DIVISOR_FLAG: u16 = 1 << 15;
// Latency credit applied when a sleeper wakes: their vruntime is bounded by
// (min_vruntime - SLEEPER_LATENCY_BONUS_NS), so I/O-bound tasks get
// preferential dispatch but cannot stockpile unbounded credit while idle.
// 1.5ms (matches Linux CFS `sysctl_sched_wakeup_granularity`'s default class)
// — large enough to give a freshly-woken IPC replier preemption priority
// against a peer's tail latency, but small enough that a high-weight task
// like `uiserver` (weight_micros=2000, real weight ~19000) is not repeatedly
// starved by I/O-bound peers (~weight 952) waking from short syscall blocks.
// At 12ms, the per-wake real-time advantage to a light peer over a heavy
// task was `12ms * heavy_weight / light_weight ≈ 100ms`, which let services
// monopolize CPU on every wake and dropped uiserver render to ~5fps.
const SLEEPER_LATENCY_BONUS_NS: u64 = 1_500_000;
// Bounded L4/seL4-style IPC handoff credit. This is deliberately much smaller
// than the sleeper bonus: it nudges a just-woken server/replier ahead of the
// caller's fair position without turning every IPC service into permanent RT.
const IPC_DONATION_BONUS_NS: u64 = 2_000_000;
// Minimum preemption granularity: do not preempt the current task in favour of
// a marginally-smaller vruntime peer if it has run less than this. Set small
// (200us) so wake-up latency stays low; advantage_ns check still uses this as
// the "must be at least this much ahead" threshold for peer-driven preemption.
const SCHED_MIN_GRANULARITY_NS: u64 = 200_000;
// Fair picks may retain cache locality only while a candidate that last ran
// on this CPU remains within one minimum-granularity unit of the global
// least-vruntime peer in the same class. This is a bounded tie-break, not CPU
// affinity: exact handoffs, strict-class recovery, affinity, and remote-owner
// exclusion all run before it, and a larger lag forces the global minimum.
const SCHED_CPU_LOCALITY_LAG_NS: u64 = SCHED_MIN_GRANULARITY_NS;
// Maximum runtime budget before we *forcibly* prefer any other ready task,
// even ones that would normally lose the vruntime comparison. Acts as a
// last-resort anti-starvation rail for long-running kernel paths that finally
// call cond_resched.
const SCHED_MAX_BURST_NS: u64 = 20_000_000;
/// A runnable strict-class task must not wait behind a sequence of other
/// strict-class tasks indefinitely. This is a dispatch-latency rail, not a
/// CPU-share boost: once the bound expires the oldest ready System task gets
/// one turn, after which normal weighted vruntime resumes.
const SYSTEM_READY_LATENCY_BOUND_MS: u64 = 2;
// Initial vruntime offset for newly-spawned tasks relative to current
// min_vruntime. Keep this near min-granularity: a larger multi-ms penalty
// leaves freshly spawned services behind polling System peers during boot.
const SCHED_NEW_TASK_VRUNTIME_PENALTY_NS: u64 = SCHED_MIN_GRANULARITY_NS;

/// Strict class is an explicit admission property, not an accidental result
/// of a large CFS share. Bootstrap brokers legitimately use larger weights
/// than uiserver/inputd; deriving class from the number let those pollers
/// crowd the interactive band and caused multi-second UI starvation.
/// A critical service remains latency-favoured, but it cannot consume every
/// dispatch indefinitely while ordinary work is ready. Two System turns
/// followed by one mandatory User turn match the checked scheduler model and
/// preserve application CPU progress under a hostile input or GUI-DVM flood.
const MAX_CONSECUTIVE_SYSTEM_DISPATCHES: u8 = 2;
/// A task that slept on a real event may bypass ready System work for one
/// wakeup turn. Cap the burst so many sleepers cannot starve the critical lane.
const MAX_CONSECUTIVE_LATENCY_HANDOFFS: u8 = 8;
const MAX_CONSECUTIVE_SYNC_HANDOFFS: u8 = 8;
const READY_VALIDATION_INTERVAL_TURNS: u8 = 32;
/// An atomically published startup cohort receives one bounded first-turn
/// prefix before the reply chain that resumes its loader/supervisor. This is
/// capped by the activation ABI and never applies to ordinary single spawns.
const MAX_ATOMIC_ACTIVATION_HANDOFFS: usize = 8;
const MAX_LATENCY_HANDOFF_HINTS: usize = 16;
const UNRESTRICTED_CPU_MASK: u64 = u64::MAX;

/// Priority bands. System work wins latency-sensitive selection until its
/// bounded consecutive-dispatch reservation is exhausted; then one ready User
/// task must run before System selection resumes. Within a class the existing
/// CFS-style vruntime accounting decides fairness. A per-task User deadline
/// additionally prevents one busy User task from hiding another ready client.
///
/// This mirrors the way commercial microkernels structure mixed workloads:
///
/// - Mach (XNU) groups threads into QoS bands (USER_INTERACTIVE > DEFAULT >
///   BACKGROUND); cross-band preemption is unconditional, intra-band uses
///   the timeshare policy.
/// - Fuchsia Zircon stacks a deadline scheduler above the fair scheduler,
///   with deadline threads always preempting fair threads.
/// - QNX Neutrino uses priority levels with explicit partition/budget controls;
///   RustOS keeps the latency ordering but reserves a bounded User turn so a
///   flooded critical lane cannot consume all CPU dispatch opportunities.
/// - Linux stacks SCHED_DEADLINE > SCHED_FIFO/RR > SCHED_OTHER (CFS) so RT
///   threads always run ahead of CFS when ready.
///
/// We use just three bands because that is the smallest set that names the
/// three distinct latency contracts in this kernel: System services that
/// must stay responsive to IPC, User apps that should share the rest fairly,
/// and the Idle halt loop. More bands can be added by extending the enum
/// without touching call sites — `pick_min_vruntime` walks `SchedClass` in
/// `Ord` order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum SchedClass {
    /// Kernel-mode tasks (root during boot, housekeeping, kernel workers)
    /// and user-mode services admitted with `TASK_WEIGHT_INTERACTIVE_FLAG`.
    /// Current owners are uiserver, inputd, and rootd's immutable core-service
    /// manifest. Ordinary packages and dynamic policy metadata stay in User
    /// even when their CFS share is numerically larger.
    System = 0,
    /// Default class for user-spawned applications (`apps/*`) and services
    /// that have not opted into System latency. Runs when no System task is
    /// ready, or once the bounded System burst requires its reserved turn.
    User = 1,
    /// The root halt loop after `mark_root_idle()`. Picked only as the last
    /// resort fallback when literally nothing else is ready.
    Idle = 2,
}

impl SchedClass {
    const COUNT: usize = 3;

    const fn index(self) -> usize {
        match self {
            Self::System => 0,
            Self::User => 1,
            Self::Idle => 2,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum TaskRetireReason {
    UserFault {
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rip: u64,
    },
    Terminated {
        requested_by_pid: Option<u64>,
    },
    CorruptedContext {
        saved_rsp: usize,
        reason: &'static str,
        reason_code: u8,
    },
    Exited,
}

#[derive(Clone, Copy)]
struct TaskContext {
    saved_rsp: usize,
    ready: bool,
    ready_since_ticks: u64,
    blocked: bool,
    blocked_since_ticks: u64,
    /// Block-arm flag for race-free sleep/wake. Set by `arm_block_current_task`;
    /// cleared by `wake_task` and `commit_block_current_task`. A wake delivered
    /// while the task is still running clears the flag, so the subsequent
    /// `commit_block_current_task` observes that a wake raced and refuses to
    /// block. Mirrors Linux's `prepare_to_wait` / `set_current_state` pattern.
    wake_armed: bool,
    /// CFS-like load weight. Bigger weight -> larger CPU share. Derived from
    /// the task's `weight_micros` / pit_divisor at allocation time.
    weight: u32,
    /// Virtual runtime in nanoseconds, scaled by NICE_0_LOAD/weight. The task
    /// with the smallest vruntime among the ready set is picked next.
    vruntime_ns: u64,
    /// RTC tick when the task last started running (was switched to). Zero
    /// while the task is not running. Used to accumulate vruntime on
    /// preemption / context switch.
    exec_start_ticks: u64,
    address_space_root: u64,
    kernel_stack_base: u64,
    kernel_stack_top: u64,
    alternate_kernel_stack_base: u64,
    alternate_kernel_stack_top: u64,
    user_mode: bool,
    user_abi: Option<UserAbi>,
    console_session: ConsoleSessionHandle,
    process_handle: Option<ProcessHandle>,
    process_id: Option<u64>,
    user_stack: Option<UserStackState>,
    windows_thread_state: Option<WindowsThreadRuntimeState>,
}

#[derive(Clone, Copy)]
pub(super) struct TaskStart {
    pub(super) entry: fn(u64),
    pub(super) id: u64,
}

/// Exact result of one serialized scheduling decision.
///
/// Callers must carry this token through the architecture-return preparation
/// step.  Deriving the switch predicate from the selected slots prevents an
/// IRQ leaf from accidentally replaying CR3/TSS/FS/GS state on a same-task
/// timer turn, while a real task switch can never skip that restoration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SchedulerDispatch {
    pub(super) next_rsp: usize,
    pub(super) tick_divisor: u16,
    previous_slot: usize,
    next_slot: usize,
}

impl SchedulerDispatch {
    const fn new(
        next_rsp: usize,
        tick_divisor: u16,
        previous_slot: usize,
        next_slot: usize,
    ) -> Self {
        Self {
            next_rsp,
            tick_divisor,
            previous_slot,
            next_slot,
        }
    }

    const fn requires_architectural_restore(self) -> bool {
        self.previous_slot != self.next_slot
    }
}

pub(super) struct Scheduler {
    contexts: [Option<TaskContext>; MAX_TASK],
    /// Thread metadata is independently synchronized from scheduler state.
    /// Process-state operations may hold this bounded raw lock without
    /// escaping an unprotected pointer into `contexts`; scheduler signal and
    /// lifecycle paths take Scheduler -> LinuxThreadState in that order.
    linux_thread_states: [LinuxThreadStateLock; MAX_TASK],
    retired: [bool; MAX_TASK],
    retirement_cleanup: [Option<super::RetiredTaskCleanup>; MAX_TASK],
    retirement_side_effects: [Option<RetirementSideEffect>; MAX_TASK],
    /// IRQ-time context validation may quarantine a task immediately, but
    /// lifecycle teardown can touch IPC registries and process accounting.
    /// Keep the reason in a fixed slot until housekeeping finalizes it in
    /// process context; no allocation or cross-subsystem lock is taken in IRQ.
    deferred_retire_reasons: [Option<TaskRetireReason>; MAX_TASK],
    exec_target_quiesced: [bool; MAX_TASK], // blocks external exec target dispatch
    thread_slot_reserved: [bool; MAX_TASK],
    start_suspended: [bool; MAX_TASK],
    job_stopped: [bool; MAX_TASK],
    retire_reasons: [Option<TaskRetireReason>; MAX_TASK],
    simd_states: [SimdState; MAX_TASK],
    syscall_user_simd_states: [SimdState; MAX_TASK],
    syscall_user_simd_active: [bool; MAX_TASK],
    starts: [Option<TaskStart>; MAX_TASK],
    stacks: [Option<Vec<u8>>; MAX_TASK],
    idle_cpu: [u8; MAX_TASK],
    task_affinity_masks: [u64; MAX_TASK],
    process_affinity_masks: [u64; MAX_TASK],
    affinity_migration_pending: [bool; MAX_TASK],
    /// Last CPU that actually dispatched each exact live task. This is
    /// scheduler policy metadata, unlike the independently reset diagnostic
    /// profile. It is only a bounded fair-pick tie-break and grants no CPU
    /// ownership or affinity authority.
    task_last_cpu: [u8; MAX_TASK],
    // Unit tests construct isolated scheduler instances without CPU-local
    // boot state.  Production dispatch state lives in
    // `cpu_local::SCHEDULER_CURRENT_TASK_SCRATCH`, never in this catalog.
    #[cfg(test)]
    pub(super) current_task: usize,
    pending_reap: bool,
    /// Every hot-path policy value is owned by one logical CPU. The global
    /// scheduler object is currently only the lifecycle/catalog serializer;
    /// it must not create a second system-wide dispatch policy.
    cpu_dispatch: [CpuDispatchLock; nucleus_core::util::lockdep::MAX_TRACKED_CPUS],
    /// At most one synchronous IPC wait can be active per runnable task, so
    /// `MAX_TASK` fixed entries cover every live donation without allocating
    /// from scheduler or IPC paths.
    ipc_priority_donations: [Option<IpcPriorityDonation>; MAX_TASK],
    ipc_priority_donation_len: usize,
    /// Self-validating donor-slot hints for the reply donation table.
    ///
    /// Class derivation resolves each donation's donor identity while scanning
    /// every candidate on every pick pass, and the authoritative resolution is
    /// an identity scan over all slots. That made selection quadratic in
    /// runnable tasks and donation depth and was the dominant term of the
    /// serialized dispatch critical section. Every hint is validated against
    /// the exact live identity before use and repaired from the authoritative
    /// scan on a miss, so a stale hint can never change a derived class.
    donation_donor_slot_hints: [AtomicU8; MAX_TASK],
    /// Attribution remains fixed-size and is rendered only after the global
    /// scheduler owner is released; normal logging policy may discard it.
    runtime_profile_started_ticks: u64,
    runtime_profile_ns: [u64; MAX_TASK],
    runtime_profile_dispatches: [u64; MAX_TASK],
    runtime_profile_entry_counts: [u64; runtime_profile::SCHEDULER_ENTRY_CAUSE_COUNT],
    runtime_profile_same_task_dispatches: u64,
    runtime_profile_task_switches: u64,
    runtime_profile_address_space_switches: u64,
    runtime_profile_cross_cpu_migrations: u64,
    /// Diagnostic locality history is keyed by the exact monotonic task id so
    /// slot reuse cannot be misreported as a cross-CPU migration. It is not
    /// scheduling authority and must never influence affinity or selection.
    runtime_profile_last_cpu: [u8; MAX_TASK],
    runtime_profile_last_task_id: [u64; MAX_TASK],
    runtime_profile_lock_acquisitions: u64,
    runtime_profile_lock_wait_ns: u64,
    runtime_profile_lock_hold_ns: u64,
    runtime_profile_lock_wait_max_ns: u64,
    runtime_profile_lock_hold_max_ns: u64,
    /// Disjoint in-owner segment attribution. Total hold time alone cannot
    /// separate a genuinely long critical section from a descheduled owner.
    runtime_profile_phase_ns: [u64; runtime_profile::SCHEDULER_PHASE_COUNT],
    /// Summed size of the local runnable candidate set across the window. The
    /// pick passes are linear in this set, so attributing selection cost
    /// requires knowing how many candidates each turn actually examined.
    runtime_profile_runnable_samples: u64,
    /// True after the bootstrap root task has entered the permanent hlt loop.
    /// Before that, slot 0 still runs finalize work and remains schedulable.
    root_idle: bool,
    /// Fixed scheduler tick divisor. CFS-style scheduling accounts CPU share
    /// through vruntime weights; it must not also shorten/lengthen the hardware
    /// tick per task or low-weight services pay excessive interrupt overhead.
    scheduler_tick_divisor: u16,
}

impl Scheduler {
    pub(super) const fn new() -> Self {
        Self {
            contexts: [None; MAX_TASK],
            linux_thread_states: [const { empty_linux_thread_state_lock() }; MAX_TASK],
            retired: [false; MAX_TASK],
            retirement_cleanup: [None; MAX_TASK],
            retirement_side_effects: [None; MAX_TASK],
            deferred_retire_reasons: [None; MAX_TASK],
            exec_target_quiesced: [false; MAX_TASK],
            thread_slot_reserved: [false; MAX_TASK],
            start_suspended: [false; MAX_TASK],
            job_stopped: [false; MAX_TASK],
            retire_reasons: [None; MAX_TASK],
            simd_states: [SimdState::new(); MAX_TASK],
            syscall_user_simd_states: [SimdState::new(); MAX_TASK],
            syscall_user_simd_active: [false; MAX_TASK],
            starts: [None; MAX_TASK],
            stacks: [const { None }; MAX_TASK],
            idle_cpu: [NO_IDLE_CPU; MAX_TASK],
            task_affinity_masks: [UNRESTRICTED_CPU_MASK; MAX_TASK],
            process_affinity_masks: [UNRESTRICTED_CPU_MASK; MAX_TASK],
            affinity_migration_pending: [false; MAX_TASK],
            task_last_cpu: [NO_IDLE_CPU; MAX_TASK],
            #[cfg(test)]
            current_task: 0,
            pending_reap: false,
            cpu_dispatch: [const { CpuDispatchLock::new(CpuDispatchPolicy::new()) };
                nucleus_core::util::lockdep::MAX_TRACKED_CPUS],
            ipc_priority_donations: [None; MAX_TASK],
            ipc_priority_donation_len: 0,
            donation_donor_slot_hints: [const { AtomicU8::new(NO_SLOT_HINT) }; MAX_TASK],
            runtime_profile_started_ticks: 0,
            runtime_profile_ns: [0; MAX_TASK],
            runtime_profile_dispatches: [0; MAX_TASK],
            runtime_profile_entry_counts: [0; runtime_profile::SCHEDULER_ENTRY_CAUSE_COUNT],
            runtime_profile_same_task_dispatches: 0,
            runtime_profile_task_switches: 0,
            runtime_profile_address_space_switches: 0,
            runtime_profile_cross_cpu_migrations: 0,
            runtime_profile_last_cpu: [NO_IDLE_CPU; MAX_TASK],
            runtime_profile_last_task_id: [0; MAX_TASK],
            runtime_profile_lock_acquisitions: 0,
            runtime_profile_lock_wait_ns: 0,
            runtime_profile_lock_hold_ns: 0,
            runtime_profile_lock_wait_max_ns: 0,
            runtime_profile_lock_hold_max_ns: 0,
            runtime_profile_phase_ns: [0; runtime_profile::SCHEDULER_PHASE_COUNT],
            runtime_profile_runnable_samples: 0,
            root_idle: false,
            scheduler_tick_divisor: 0,
        }
    }

    #[inline]
    fn current_dispatch_cpu() -> usize {
        let cpu = nucleus_core::util::lockdep::current_cpu_index();
        assert!(
            cpu < nucleus_core::util::lockdep::MAX_TRACKED_CPUS,
            "scheduler dispatch CPU exceeds capacity"
        );
        cpu
    }

    /// Returns this scheduler invocation's exact CPU-local current slot.
    ///
    /// Production code keeps this transient dispatch state beside the CPU,
    /// not in the lifecycle catalog.  The public current/transition words in
    /// `cpu_local` remain the only cross-CPU execution-owner publication.
    #[inline]
    fn current_task_slot(&self) -> usize {
        #[cfg(test)]
        {
            self.current_task
        }
        #[cfg(not(test))]
        {
            super::cpu_local::scheduler_current_task_scratch()
        }
    }

    /// Updates only this CPU's private dispatch scratch slot.  The enclosing
    /// `SchedulerAccessGuard` publishes the selected slot after it installs
    /// the outgoing stack-transition owner, so an early write cannot grant
    /// remote ownership before the assembly RSP commit.
    #[inline]
    fn set_current_task_slot(&mut self, slot: usize) {
        assert!(slot < MAX_TASK, "scheduler current slot exceeds capacity");
        #[cfg(test)]
        {
            self.current_task = slot;
        }
        #[cfg(not(test))]
        {
            super::cpu_local::set_scheduler_current_task_scratch(slot);
        }
    }

    /// Charges the elapsed segment to `phase` and rebases the marker.
    ///
    /// The scheduler owner already excludes every other CPU, so this is one
    /// monotonic read plus a counter update. It exists because total hold time
    /// cannot attribute a stall to a segment, and the release gate requires
    /// that attribution rather than an assumption.
    #[inline]
    fn mark_phase(&mut self, phase: SchedulerPhase, marker: &mut u64) {
        let now = crate::arch::clock::monotonic_nanos();
        self.record_runtime_profile_phase(phase, now.saturating_sub(*marker));
        *marker = now;
    }

    #[inline]
    fn current_dispatch_policy(&self) -> CpuDispatchGuard<'_> {
        self.cpu_dispatch[Self::current_dispatch_cpu()].lock()
    }

    #[inline]
    fn current_dispatch_policy_mut(&self) -> CpuDispatchGuard<'_> {
        self.cpu_dispatch[Self::current_dispatch_cpu()].lock()
    }

    fn slot_dispatch_cpu(&self, slot: usize) -> usize {
        #[cfg(not(test))]
        if let Some(cpu) = runqueue::owner(slot).cpu {
            return cpu;
        }
        #[cfg(test)]
        let _ = slot;
        Self::current_dispatch_cpu()
    }

    fn handoff_slot_ready(&self, slot: usize) -> bool {
        slot < MAX_TASK
            && !self.retired[slot]
            && !self.start_suspended[slot]
            && !self.job_stopped[slot]
            && !self.exec_target_quiesced[slot]
            && self.deferred_retire_reasons[slot].is_none()
            && self.contexts[slot].is_some_and(|context| context.ready && !context.blocked)
    }

    fn new_task_vruntime(&self) -> u64 {
        self.current_dispatch_policy()
            .last_min_vruntime_ns
            .saturating_add(SCHED_NEW_TASK_VRUNTIME_PENALTY_NS)
    }

    fn remove_slot_from_dispatch_policies(&mut self, slot: usize) {
        for policy in &self.cpu_dispatch {
            let mut policy = policy.lock();
            if policy.next_pick_hint == Some(slot) {
                policy.next_pick_hint = None;
            }
            let mut compact = [None; MAX_LATENCY_HANDOFF_HINTS];
            let mut retained = 0_usize;
            for offset in 0..policy.latency_pick_hint_len {
                let index = (policy.latency_pick_hint_head + offset) % MAX_LATENCY_HANDOFF_HINTS;
                if let Some(candidate) = policy.latency_pick_hints[index]
                    && candidate != slot
                {
                    compact[retained] = Some(candidate);
                    retained += 1;
                }
            }
            policy.latency_pick_hints = compact;
            policy.latency_pick_hint_head = 0;
            policy.latency_pick_hint_len = retained;
            policy.spawn_pick_hints.remove(slot);
            policy.atomic_activation_pick_hints.remove(slot);
            policy.sync_pick_hints.remove(slot);
        }
    }

    /// Sets a "donate" hint that biases the next scheduler pick toward the
    /// given task id. IPC paths use this so that after `wake_task(target)` and
    /// `yield_now()`, the scheduler immediately runs `target` instead of
    /// round-robining through unrelated ready tasks. Generic IPC hints are
    /// caller-local handoffs: the most recent eligible receiver replaces any
    /// older pending IPC hint, because a stale higher-class hint must not block
    /// the service that the current caller is synchronously waiting on. Spawn
    /// handoff has its own hint slot and remains protected from IPC replies.
    pub(super) fn set_next_pick_hint(&mut self, task_id: u64) {
        let Some(slot) = self.find_task_slot(task_id) else {
            self.current_dispatch_policy_mut().next_pick_hint = None;
            return;
        };
        if !self.handoff_slot_ready(slot) {
            return;
        }
        let target_cpu = self.slot_dispatch_cpu(slot);
        if target_cpu == Self::current_dispatch_cpu() {
            self.apply_ipc_donation(slot);
        }
        self.cpu_dispatch[target_cpu].lock().next_pick_hint = Some(slot);
    }

    pub(super) fn set_next_latency_pick_hint(&mut self, task_id: u64) -> bool {
        let Some(slot) = self.find_task_slot(task_id) else {
            return false;
        };
        // This queue exists only for the explicit cross-class exception.
        // System tasks already win normal class selection and must not consume
        // the bounded User wakeup budget.
        if self.slot_class(slot) != Some(SchedClass::User) || !self.handoff_slot_ready(slot) {
            return false;
        }
        let target_cpu = self.slot_dispatch_cpu(slot);
        let policy = self.cpu_dispatch[target_cpu].lock();
        if (0..policy.latency_pick_hint_len).any(|offset| {
            let index = (policy.latency_pick_hint_head + offset) % MAX_LATENCY_HANDOFF_HINTS;
            policy.latency_pick_hints[index] == Some(slot)
        }) {
            return true;
        }
        if policy.latency_pick_hint_len >= MAX_LATENCY_HANDOFF_HINTS {
            return false;
        }
        drop(policy);
        let mut policy = self.cpu_dispatch[target_cpu].lock();
        let tail = (policy.latency_pick_hint_head + policy.latency_pick_hint_len)
            % MAX_LATENCY_HANDOFF_HINTS;
        policy.latency_pick_hints[tail] = Some(slot);
        policy.latency_pick_hint_len += 1;
        true
    }

    /// Selects a runnable worker for a process-owned endpoint when the sender
    /// enqueues between the server's reply and its next `IPC_RECV`. In that
    /// window the endpoint has no waiter task to return, but an associated
    /// process worker is ready or already running and must receive the same
    /// direct-handoff treatment. The server may belong to another runqueue;
    /// limiting this search to the caller CPU silently disables QNX-style
    /// server boost on SMP and leaves a System caller behind unrelated User
    /// work until the server's next receive.
    pub(super) fn set_next_process_pick_hint(&mut self, process_id: u64) -> Option<u64> {
        let slot = self.eligible_process_worker_slot(process_id)?;
        self.apply_ipc_donation(slot);
        let target_cpu = self.slot_dispatch_cpu(slot);
        self.cpu_dispatch[target_cpu]
            .lock()
            .sync_pick_hints
            .enqueue(slot)
            .expect("scheduler synchronous process handoff queue overflow");
        #[cfg(not(test))]
        super::irq::request_target_reschedule(target_cpu);
        self.starts[slot].map(|start| start.id)
    }

    fn pick_hint_candidate_slot(&self, hint: Option<usize>) -> Option<usize> {
        let slot = hint?;
        if slot >= MAX_TASK {
            return None;
        }
        let context = self.contexts[slot]?;
        if !context.ready || !self.context_is_schedulable(slot, context) {
            return None;
        }
        Some(slot)
    }

    fn pick_hint_ready_slot(&self, hint: Option<usize>) -> Option<usize> {
        let slot = self.pick_hint_candidate_slot(hint)?;
        // IPC donation must not violate strict class priority. If a class
        // higher than the hint has any ready work, fall through to the
        // regular pick so the hint cannot let a User-class IPC callee bypass
        // a ready System task.
        let hint_class = self.slot_class(slot)?;
        if hint_class > SchedClass::System
            && self
                .pick_min_vruntime_in_class(self.current_task_slot(), SchedClass::System)
                .is_some()
        {
            return None;
        }
        Some(slot)
    }

    fn take_next_latency_pick_hint_ready_slot(&mut self) -> Option<usize> {
        if self.current_dispatch_policy().latency_handoff_streak >= MAX_CONSECUTIVE_LATENCY_HANDOFFS
        {
            return None;
        }
        while self.current_dispatch_policy().latency_pick_hint_len != 0 {
            let hint = {
                let mut policy = self.current_dispatch_policy_mut();
                let index = policy.latency_pick_hint_head;
                let hint = policy.latency_pick_hints[index].take();
                policy.latency_pick_hint_head = (index + 1) % MAX_LATENCY_HANDOFF_HINTS;
                policy.latency_pick_hint_len -= 1;
                hint
            };
            if let Some(slot) = self.pick_hint_candidate_slot(hint) {
                return Some(slot);
            }
        }
        None
    }

    fn take_next_pick_hint_ready_slot(&mut self) -> Option<usize> {
        let hint = self.current_dispatch_policy().next_pick_hint;
        if hint.is_some() && self.pick_hint_candidate_slot(hint).is_none() {
            self.current_dispatch_policy_mut().next_pick_hint = None;
            return None;
        }
        let slot = self.pick_hint_ready_slot(hint)?;
        self.current_dispatch_policy_mut().next_pick_hint = None;
        Some(slot)
    }

    // Architectural register and entry fields stay explicit at scheduler
    // context construction boundaries.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn reset(
        &mut self,
        main_thread_pit_divisor: u16,
        entry: fn(u64),
        id: u64,
        kernel_cs: u64,
        kernel_ss: u64,
        rflags: u64,
        kernel_task_entry_rip: u64,
    ) {
        #[cfg(not(test))]
        runqueue::reset_before_publication();
        for slot in 0..MAX_TASK {
            self.clear_slot(slot);
        }

        self.simd_states = [SimdState::new(); MAX_TASK];
        self.syscall_user_simd_states = [SimdState::new(); MAX_TASK];
        self.syscall_user_simd_active = [false; MAX_TASK];
        self.retired = [false; MAX_TASK];
        self.retirement_cleanup = [None; MAX_TASK];
        self.retirement_side_effects = [None; MAX_TASK];
        self.deferred_retire_reasons = [None; MAX_TASK];
        self.thread_slot_reserved = [false; MAX_TASK];
        self.start_suspended = [false; MAX_TASK];
        self.job_stopped = [false; MAX_TASK];
        self.retire_reasons = [None; MAX_TASK];
        self.idle_cpu = [NO_IDLE_CPU; MAX_TASK];
        self.task_affinity_masks = [UNRESTRICTED_CPU_MASK; MAX_TASK];
        self.process_affinity_masks = [UNRESTRICTED_CPU_MASK; MAX_TASK];
        self.affinity_migration_pending = [false; MAX_TASK];
        self.task_last_cpu = [NO_IDLE_CPU; MAX_TASK];
        self.runtime_profile_started_ticks = 0;
        self.runtime_profile_ns = [0; MAX_TASK];
        self.runtime_profile_dispatches = [0; MAX_TASK];
        self.runtime_profile_entry_counts = [0; runtime_profile::SCHEDULER_ENTRY_CAUSE_COUNT];
        self.runtime_profile_same_task_dispatches = 0;
        self.runtime_profile_task_switches = 0;
        self.runtime_profile_address_space_switches = 0;
        self.runtime_profile_cross_cpu_migrations = 0;
        self.runtime_profile_last_cpu = [NO_IDLE_CPU; MAX_TASK];
        self.runtime_profile_last_task_id = [0; MAX_TASK];
        self.runtime_profile_lock_wait_ns = 0;
        self.runtime_profile_lock_hold_ns = 0;
        self.runtime_profile_lock_wait_max_ns = 0;
        self.runtime_profile_lock_hold_max_ns = 0;
        self.set_current_task_slot(ROOT_TASK_SLOT);
        self.pending_reap = false;
        for policy in &self.cpu_dispatch {
            *policy.lock() = CpuDispatchPolicy::new();
        }
        self.ipc_priority_donations = [None; MAX_TASK];
        self.ipc_priority_donation_len = 0;
        self.root_idle = false;
        self.scheduler_tick_divisor = main_thread_pit_divisor;
        self.reset_stack_storage(ROOT_TASK_SLOT)
            .expect("scheduler root stack allocation failed");
        let (kernel_stack_base, kernel_stack_top) = self.stack_bounds(ROOT_TASK_SLOT);
        self.contexts[ROOT_TASK_SLOT] = Some(TaskContext {
            saved_rsp: self.init_kernel_entry_context(
                ROOT_TASK_SLOT,
                kernel_cs,
                kernel_ss,
                rflags,
                kernel_task_entry_rip,
                0,
            ),
            ready: true,
            ready_since_ticks: crate::arch::rtc::ticks(),
            blocked: false,
            blocked_since_ticks: 0,
            wake_armed: false,
            weight: Self::weight_from_pit_divisor(main_thread_pit_divisor),
            vruntime_ns: 0,
            exec_start_ticks: crate::arch::rtc::ticks(),
            address_space_root: crate::memory::paging::kernel_root_phys().as_u64(),
            kernel_stack_base: kernel_stack_base as u64,
            kernel_stack_top: kernel_stack_top as u64,
            alternate_kernel_stack_base: 0,
            alternate_kernel_stack_top: 0,
            user_mode: false,
            user_abi: None,
            console_session: ConsoleSessionHandle::SYSTEM,
            process_handle: None,
            process_id: None,
            user_stack: None,
            windows_thread_state: None,
        });
        self.starts[ROOT_TASK_SLOT] = Some(TaskStart { entry, id });
        self.publish_slot_identity(ROOT_TASK_SLOT);
        #[cfg(not(test))]
        runqueue::admit_running(ROOT_TASK_SLOT, 0);
        nucleus_core::util::lockdep::set_current_task_owner(
            id.checked_add(1)
                .expect("root task id exhausted lock owner token"),
        );

        unsafe {
            save_state(&mut self.simd_states[ROOT_TASK_SLOT]);
        }
    }

    pub(super) fn initialized(&self) -> bool {
        self.contexts[ROOT_TASK_SLOT].is_some()
    }

    pub(super) fn clear_slot(&mut self, slot: usize) {
        self.take_slot_reclaim(slot).complete();
    }

    fn take_slot_reclaim(&mut self, slot: usize) -> RetiredSlotReclaim {
        assert!(
            self.retirement_side_effects[slot].is_none(),
            "scheduler slot reclaimed before external retirement side effects"
        );
        let task_id = self.starts[slot]
            .map(|start| start.id)
            .unwrap_or(slot as u64);
        if self.starts[slot].is_some() {
            self.release_ipc_priorities_for_task(task_id);
        }
        let context = self.contexts[slot];
        let process_handle = context.and_then(|context| context.process_handle);
        let user_mode = context.is_some_and(|context| context.user_mode);
        let stack_base = context
            .map(|context| context.kernel_stack_base)
            .unwrap_or(0);
        let stack_top = context.map(|context| context.kernel_stack_top).unwrap_or(0);
        let reason = self.retire_reasons[slot];

        #[cfg(not(test))]
        if self.starts[slot].is_some() {
            runqueue::release_retired(slot);
        }
        self.contexts[slot] = None;
        self.install_linux_thread_state(slot, None, None);
        self.remove_slot_from_dispatch_policies(slot);
        self.retired[slot] = false;
        self.retirement_cleanup[slot] = None;
        self.deferred_retire_reasons[slot] = None;
        self.exec_target_quiesced[slot] = false;
        self.start_suspended[slot] = false;
        self.job_stopped[slot] = false;
        self.retire_reasons[slot] = None;
        self.simd_states[slot] = SimdState::new();
        self.syscall_user_simd_states[slot] = SimdState::new();
        self.syscall_user_simd_active[slot] = false;
        self.starts[slot] = None;
        self.publish_slot_identity(slot);
        self.idle_cpu[slot] = NO_IDLE_CPU;
        self.task_affinity_masks[slot] = UNRESTRICTED_CPU_MASK;
        self.process_affinity_masks[slot] = UNRESTRICTED_CPU_MASK;
        self.affinity_migration_pending[slot] = false;
        self.task_last_cpu[slot] = NO_IDLE_CPU;
        self.runtime_profile_last_cpu[slot] = NO_IDLE_CPU;
        self.runtime_profile_last_task_id[slot] = 0;
        RetiredSlotReclaim::new(
            process_handle,
            self.stacks[slot].take(),
            task_id,
            slot,
            user_mode,
            stack_base,
            stack_top,
            reason,
        )
    }

    fn mark_slot_ready(&mut self, slot: usize, saved_rsp: usize, ready: bool) {
        let Some(context) = self.contexts[slot].as_mut() else {
            return;
        };

        context.saved_rsp = saved_rsp;
        if ready && !context.ready {
            context.ready_since_ticks = crate::arch::rtc::ticks();
        } else if !ready {
            context.ready_since_ticks = 0;
        }
        context.ready = ready;
    }

    fn ticks_elapsed_ms(start_ticks: u64, end_ticks: u64) -> u64 {
        let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
        end_ticks
            .saturating_sub(start_ticks)
            .saturating_mul(1000)
            .saturating_div(ticks_per_second)
    }

    /// Converts an RTC-tick span into nanoseconds. Used for CFS-like vruntime
    /// accounting; saturates on overflow so a runaway tick counter cannot
    /// poison the scheduler.
    fn ticks_elapsed_ns(start_ticks: u64, end_ticks: u64) -> u64 {
        let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
        end_ticks
            .saturating_sub(start_ticks)
            .saturating_mul(1_000_000_000)
            .saturating_div(ticks_per_second)
    }

    /// Linux-style weighted vruntime delta:
    /// `delta_vruntime = delta_exec * NICE_0_LOAD / weight`.
    /// Heavier-weight tasks accrue vruntime more slowly and therefore receive
    /// a proportionally larger share of CPU time.
    fn weighted_vruntime_delta(elapsed_ns: u64, weight: u32) -> u64 {
        let w = (weight & LOAD_WEIGHT_MASK).clamp(MIN_LOAD_WEIGHT, MAX_LOAD_WEIGHT) as u64;
        elapsed_ns
            .saturating_mul(NICE_0_LOAD as u64)
            .saturating_div(w)
    }

    /// Maps the per-task PIT divisor (proportional to its `weight_micros`)
    /// onto a CFS load weight. The default user task weight_micros=100 yields
    /// divisor ~119, which we scale to ~952 (close to NICE_0_LOAD=1024).
    /// Heavier services such as `uiserver` (weight_micros=2000) end up around
    /// ~19000 and naturally receive ~20x more CPU when contending.
    fn weight_from_pit_divisor(divisor: u16) -> u32 {
        // pit_divisor is BASE_FREQUENCY_HZ * weight_micros / 1_000_000, so it
        // is monotonically increasing in weight_micros. Using `divisor * 8`
        // keeps default-weight tasks near NICE_0_LOAD without arithmetic that
        // requires knowing the PIT base frequency at this layer.
        let interactive = divisor & INTERACTIVE_PIT_DIVISOR_FLAG != 0;
        let raw_divisor = divisor & !INTERACTIVE_PIT_DIVISOR_FLAG;
        let scaled = (raw_divisor.max(1) as u32).saturating_mul(8);
        let load = scaled.clamp(MIN_LOAD_WEIGHT, MAX_LOAD_WEIGHT);
        load | if interactive {
            SYSTEM_CLASS_WEIGHT_FLAG
        } else {
            0
        }
    }

    /// Returns the smallest vruntime across all ready (or current-running)
    /// tasks, plus the chosen task's vruntime. Used as the initialisation
    /// floor for newly-spawned tasks (whose class is not yet known) and
    /// nowhere else; class-aware floors go through
    /// `min_ready_vruntime_in_class`.
    fn min_ready_vruntime(&self) -> u64 {
        let mut min: Option<u64> = None;
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(ctx) = self.contexts[slot] else {
                continue;
            };
            if !ctx.ready {
                continue;
            }
            if !self.context_is_schedulable(slot, ctx) {
                continue;
            }
            let v = ctx.vruntime_ns;
            min = Some(min.map(|m| m.min(v)).unwrap_or(v));
        }
        min.unwrap_or(0)
    }

    /// Returns the smallest vruntime among ready tasks in a specific
    /// scheduling class. Used by the sleeper-bonus floor in `wake_task_slot`
    /// so the bonus is normalised against the woken task's actual peers, not
    /// against an unrelated higher-priority band whose vruntime may track a
    /// different real-time pace because of weight differences.
    fn min_ready_vruntime_in_class(&self, class: SchedClass) -> u64 {
        let mut min: Option<u64> = None;
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(ctx) = self.contexts[slot] else {
                continue;
            };
            if !ctx.ready {
                continue;
            }
            if !self.context_is_schedulable(slot, ctx) {
                continue;
            }
            if self.slot_class(slot) != Some(class) {
                continue;
            }
            let v = ctx.vruntime_ns;
            min = Some(min.map(|m| m.min(v)).unwrap_or(v));
        }
        min.unwrap_or(0)
    }

    /// Classifies a slot into one of the strict priority bands, including the
    /// bounded transitive priority inheritance carried by live reply
    /// capabilities. Result is derived (not stored) so changing
    /// `weight_micros` of a service via its manifest remains the single source
    /// of truth; no separate sched_class field has to be kept in sync.
    ///
    /// Class is determined by the explicit admission bit carried with the
    /// launch weight, not by user/kernel mode or the magnitude of the CFS
    /// share. Kernel workers and bootstrap brokers stay in User unless their
    /// trusted launcher opts them in; this prevents a high-throughput weight
    /// from silently becoming a strict-priority capability.
    fn slot_class(&self, slot: usize) -> Option<SchedClass> {
        let mut visiting = [false; MAX_TASK];
        self.effective_slot_class(slot, &mut visiting)
    }

    /// Returns the kernel-derived effective IPC/scheduling class for a live
    /// task. The public boundary exposes only the System predicate so callers
    /// cannot manufacture or persist scheduler-internal class values.
    pub(super) fn task_has_system_scheduling_class(&self, task_id: u64) -> bool {
        self.find_task_slot(task_id).is_some_and(|slot| {
            !self.retired[slot] && self.slot_class(slot) == Some(SchedClass::System)
        })
    }

    fn effective_slot_class(
        &self,
        slot: usize,
        visiting: &mut [bool; MAX_TASK],
    ) -> Option<SchedClass> {
        let base = self.base_slot_class(slot)?;
        // A System task cannot be promoted further and root-idle must remain
        // an idle fallback even if stale external state tried to reference it.
        if base != SchedClass::User || visiting[slot] {
            return Some(base);
        }
        visiting[slot] = true;
        let mut effective = base;
        for (index, donation) in self.ipc_priority_donations[..self.ipc_priority_donation_len]
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.as_ref().map(|entry| (index, entry)))
        {
            let target_is_slot = matches!(
                donation.target,
                IpcDonationTarget::BoundWorker(task_id)
                    if self.starts[slot].is_some_and(|start| start.id == task_id)
            );
            if !target_is_slot {
                continue;
            }
            let Some(donor_slot) = self.resolve_donation_slot(index, donation.donor_task_id) else {
                continue;
            };
            let Some(donor_class) = self.effective_slot_class(donor_slot, visiting) else {
                continue;
            };
            if donor_class < effective {
                effective = donor_class;
            }
        }
        visiting[slot] = false;
        Some(effective)
    }

    fn base_slot_class(&self, slot: usize) -> Option<SchedClass> {
        let ctx = self.contexts[slot]?;
        if self.idle_cpu[slot] != NO_IDLE_CPU {
            return Some(SchedClass::Idle);
        }
        if slot == ROOT_TASK_SLOT && self.root_idle {
            return Some(SchedClass::Idle);
        }
        if ctx.weight & SYSTEM_CLASS_WEIGHT_FLAG != 0 {
            Some(SchedClass::System)
        } else {
            Some(SchedClass::User)
        }
    }

    fn is_fair_candidate_slot(&self, slot: usize) -> bool {
        // Linux keeps the idle task out of CFS and only falls back to it when
        // no fair-class task is runnable. RustOS slot 0 is the same concept:
        // after finalize; before then the root task still does real boot work.
        self.idle_cpu[slot] == NO_IDLE_CPU && (slot != ROOT_TASK_SLOT || !self.root_idle)
    }

    pub(super) fn mark_root_idle(&mut self) {
        self.root_idle = true;
    }

    /// Accumulates vruntime for the currently-running slot up to `now_ticks`
    /// and clears its execution-start mark. Safe to call repeatedly.
    ///
    /// Floors the per-slice charge when explicitly requested by a voluntary
    /// yield path so a sub-tick yield can never charge 0. The RTC tick is
    /// ~976us at 1024Hz, and without a floor a task that yields rapidly
    /// (e.g. the nucleus housekeeping loop, or a user task ping-ponging IPC
    /// replies) accumulates 0 vruntime per pick and keeps winning CFS while
    /// real services sit ready for hundreds of ms. The floor mirrors
    /// `SCHED_MIN_GRANULARITY_NS` — the same threshold the keep-current guard
    /// uses — so the two halves of the heuristic agree.
    ///
    /// Timer-driven accounting (`force_min_charge = false`) is unchanged: a
    /// preempted task that genuinely ran less than a tick boundary keeps the
    /// historical zero-charge behavior, since storage and other kernel paths
    /// rely on it to make timely forward progress under TCG.
    fn account_current_runtime(&mut self, slot: usize, now_ticks: u64, force_min_charge: bool) {
        let Some(start) = self.contexts[slot].map(|context| context.exec_start_ticks) else {
            return;
        };
        if start == 0 {
            return;
        }
        let elapsed_ns = if now_ticks > start {
            Self::ticks_elapsed_ns(start, now_ticks)
        } else {
            0
        };
        self.account_runtime_profile(slot, elapsed_ns);
        let elapsed_ns = if force_min_charge {
            elapsed_ns.max(SCHED_MIN_GRANULARITY_NS)
        } else {
            elapsed_ns
        };
        let context = self.contexts[slot]
            .as_mut()
            .expect("profiled scheduler task disappeared under scheduler owner");
        if elapsed_ns == 0 {
            context.exec_start_ticks = 0;
            return;
        }
        let delta = Self::weighted_vruntime_delta(elapsed_ns, context.weight);
        context.vruntime_ns = context.vruntime_ns.saturating_add(delta);
        context.exec_start_ticks = 0;
    }

    /// Returns the lowest-vruntime User task only after the class-wide System
    /// burst is exhausted.
    ///
    /// A previous 2 ms wall-clock deadline made every runnable User task
    /// bypass weight and vruntime under overload. With more runnable tasks
    /// than admitted CPU capacity that promise is unschedulable and turns the
    /// scheduler into a high-rate round-robin loop. Ordinary fair work stays
    /// governed by charged vruntime; exact event and synchronous IPC wakeups
    /// retain their separately bounded handoff queues.
    fn reserved_user_pick(&self, current: usize) -> Option<usize> {
        self.user_reservation_due()
            .then(|| self.pick_min_vruntime_in_class(current, SchedClass::User))
            .flatten()
    }

    fn overdue_class_pick(
        &self,
        current: usize,
        now_ticks: u64,
        class: SchedClass,
        latency_bound_ms: u64,
    ) -> Option<usize> {
        let mut oldest: Option<(usize, u64, u64)> = None;
        let current_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in runqueue::local_runnable_slots(current_cpu) {
            if slot == current || !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.ready
                || context.ready_since_ticks == 0
                || !self.context_is_schedulable(slot, context)
                || self.slot_class(slot) != Some(class)
                || Self::ticks_elapsed_ms(context.ready_since_ticks, now_ticks) < latency_bound_ms
            {
                continue;
            }
            let candidate = (slot, context.ready_since_ticks, context.vruntime_ns);
            match oldest {
                None => oldest = Some(candidate),
                Some((_, oldest_since, oldest_vruntime))
                    if context.ready_since_ticks < oldest_since
                        || (context.ready_since_ticks == oldest_since
                            && context.vruntime_ns < oldest_vruntime) =>
                {
                    oldest = Some(candidate);
                }
                _ => {}
            }
        }
        oldest.map(|(slot, _, _)| slot)
    }

    fn overdue_system_pick(&self, current: usize, now_ticks: u64) -> Option<usize> {
        self.overdue_class_pick(
            current,
            now_ticks,
            SchedClass::System,
            SYSTEM_READY_LATENCY_BOUND_MS,
        )
    }

    /// Strict-class ready-age gate. Fresh handoffs cannot postpone an already
    /// admitted System continuation past its bound. User-class progress is
    /// instead guaranteed by the System-burst reservation, bounded handoff
    /// bursts, and charged vruntime; no unadmitted wall-clock deadline may
    /// override fair-share accounting.
    fn mandatory_overdue_system_pick(&self, current: usize, now_ticks: u64) -> Option<usize> {
        self.overdue_system_pick(current, now_ticks)
    }

    /// Selects an overdue strict-class continuation before an unrelated IPC
    /// hint. A blocked caller's exact causal handoff is handled earlier in
    /// `dispatch_schedule`; this helper covers only the ordinary runnable
    /// case, where a continuous stream of fresh hints must not keep a
    /// preempted System task inside a kernel transaction off-CPU forever.
    ///
    /// The hint remains pending when the overdue task wins, so the direct IPC
    /// handoff is delayed by one bounded recovery turn rather than discarded.
    fn take_overdue_system_or_pick_hint(
        &mut self,
        current: usize,
        now_ticks: u64,
    ) -> Option<usize> {
        self.overdue_system_pick(current, now_ticks)
            .or_else(|| self.take_next_pick_hint_ready_slot())
    }

    fn user_reservation_due(&self) -> bool {
        self.current_dispatch_policy().system_dispatch_streak >= MAX_CONSECUTIVE_SYSTEM_DISPATCHES
    }

    fn record_dispatch_class(&mut self, slot: usize) {
        let next = match self.slot_class(slot) {
            Some(SchedClass::System) => self
                .current_dispatch_policy()
                .system_dispatch_streak
                .saturating_add(1)
                .min(MAX_CONSECUTIVE_SYSTEM_DISPATCHES),
            Some(SchedClass::User | SchedClass::Idle) | None => 0,
        };
        self.current_dispatch_policy_mut().system_dispatch_streak = next;
    }

    fn record_latency_handoff(&mut self, latency_handoff: bool) {
        let next = if latency_handoff {
            self.current_dispatch_policy()
                .latency_handoff_streak
                .saturating_add(1)
                .min(MAX_CONSECUTIVE_LATENCY_HANDOFFS)
        } else {
            0
        };
        self.current_dispatch_policy_mut().latency_handoff_streak = next;
    }

    /// Removes the current user task's *base* System-class admission and caps
    /// its permanent fair-scheduler share at the nominal user weight.
    ///
    /// This is an irreversible self-demotion for a process helper such as a
    /// telemetry, catalog, or untrusted client-accept worker. Clone inherits
    /// both the class bit and the parent's load weight; clearing only the bit
    /// left every uiserver helper with the compositor's elevated CPU share.
    /// The cap is monotonic (a low-weight task cannot use this syscall to gain
    /// share) and intentionally does not touch a live reply-scoped IPC
    /// donation. Synchronous service work regains only its caller-derived
    /// effective class and direct handoff until the exact reply terminates.
    pub(super) fn demote_current_user_task_to_user_class(&mut self) -> bool {
        let Some(context) = self.contexts[self.current_task_slot()].as_mut() else {
            return false;
        };
        if !context.user_mode {
            return false;
        }
        context.weight = (context.weight & LOAD_WEIGHT_MASK).min(NICE_0_LOAD);
        true
    }

    fn maybe_log_ready_wait(
        &self,
        slot: usize,
        task_id: Option<u64>,
        process_id: Option<u64>,
        start_ticks: u64,
        end_ticks: u64,
    ) {
        let Some(process_id) = process_id.filter(|process_id| *process_id != 0) else {
            return;
        };
        let elapsed_ms = Self::ticks_elapsed_ms(start_ticks, end_ticks);
        if elapsed_ms < LONG_READY_WAIT_THRESHOLD_MS {
            return;
        }
        // The CPU-local current slot is still the task that held the CPU while the
        // picked slot sat ready. The scheduler raw owner must never enter the
        // synchronous debug transport, so publish an allocation-free per-CPU
        // journal record for later panic/probe inspection.
        let (from_slot, from_task, from_pid) = self.describe_current_task();
        nucleus_core::util::lockdep::record_scheduler_observation(
            nucleus_core::util::lockdep::current_cpu_index(),
            nucleus_core::util::lockdep::SchedulerObservation {
                kind: nucleus_core::util::lockdep::SchedulerObservationKind::ReadyWait,
                subject_task: task_id.unwrap_or(0),
                subject_pid: process_id,
                subject_slot: slot,
                peer_task: from_task,
                peer_pid: from_pid,
                peer_slot: from_slot,
                elapsed_ms,
                state_flags: 0,
                ready_since_ticks: start_ticks,
                blocked_since_ticks: 0,
            },
        );
    }

    fn maybe_log_blocked_wait(
        &self,
        slot: usize,
        task_id: Option<u64>,
        process_id: Option<u64>,
        start_ticks: u64,
        end_ticks: u64,
    ) {
        let Some(process_id) = process_id.filter(|process_id| *process_id != 0) else {
            return;
        };
        let elapsed_ms = Self::ticks_elapsed_ms(start_ticks, end_ticks);
        if elapsed_ms < LONG_BLOCKED_WAIT_THRESHOLD_MS {
            return;
        }
        // The waker (current task) and its identity isolate "service was idle"
        // (waker is some peer reaching it for the first time in a while) from
        // "scheduler dropped the wakeup" (waker is the IPC replier and the
        // gap should have been ms, not s). Keep this observation lock-free:
        // debug printing while holding the scheduler raw owner can deadlock or
        // force another CPU through the bounded acquisition panic.
        let (from_slot, from_task, from_pid) = self.describe_current_task();
        nucleus_core::util::lockdep::record_scheduler_observation(
            nucleus_core::util::lockdep::current_cpu_index(),
            nucleus_core::util::lockdep::SchedulerObservation {
                kind: nucleus_core::util::lockdep::SchedulerObservationKind::BlockedWait,
                subject_task: task_id.unwrap_or(0),
                subject_pid: process_id,
                subject_slot: slot,
                peer_task: from_task,
                peer_pid: from_pid,
                peer_slot: from_slot,
                elapsed_ms,
                state_flags: 0,
                ready_since_ticks: 0,
                blocked_since_ticks: start_ticks,
            },
        );
    }

    fn describe_current_task(&self) -> (usize, u64, u64) {
        let slot = self.current_task_slot();
        let task = self
            .starts
            .get(slot)
            .and_then(|start| *start)
            .map(|start| start.id)
            .unwrap_or(0);
        let pid = self
            .contexts
            .get(slot)
            .and_then(|context| *context)
            .and_then(|context| context.process_id)
            .unwrap_or(0);
        (slot, task, pid)
    }

    #[track_caller]
    fn retire_slot(&mut self, slot: usize, reason: TaskRetireReason) {
        if self.defer_remote_retirement(slot, reason) {
            return;
        }
        assert_eq!(
            nucleus_core::util::lockdep::irq_context_depth(),
            0,
            "task retirement finalization must run outside hardware interrupt context slot={} reason={:?}",
            slot,
            reason,
        );
        let rust_stack_pointer: usize;
        unsafe {
            core::arch::asm!(
                "mov {}, rsp",
                out(reg) rust_stack_pointer,
                options(nomem, nostack, preserves_flags),
            );
        }
        if rust_stack_pointer & 0xF != 0 {
            match reason {
                TaskRetireReason::UserFault { vector, rip, .. } => {
                    panic!(
                        "user-fault retirement entered Rust with a misaligned call stack: vector={} rip={:#x}",
                        vector, rip
                    )
                }
                TaskRetireReason::Terminated { .. } => {
                    panic!("termination retirement entered Rust with a misaligned call stack")
                }
                TaskRetireReason::CorruptedContext { .. } => {
                    panic!("corrupted-context retirement entered Rust with a misaligned call stack")
                }
                TaskRetireReason::Exited => {
                    panic!("task-exit retirement entered Rust with a misaligned call stack")
                }
            }
        }
        if slot == ROOT_TASK_SLOT {
            panic!("scheduler root kernel task cannot be retired");
        }

        #[cfg(not(test))]
        runqueue::retire(
            slot,
            self.contexts[slot]
                .map(|context| context.weight)
                .unwrap_or(NICE_0_LOAD),
        );

        let task_id = self.starts[slot].map(|start| start.id);
        let process =
            self.contexts[slot].and_then(|context| context.process_handle.zip(context.process_id));
        let process_terminal = process.is_some_and(|(process_handle, _)| {
            self.is_last_live_user_task_for_process(slot, process_handle)
        });
        let retiring_linux_state = self.linux_thread_state(slot);
        let retirement_cleanup =
            self.contexts[slot]
                .filter(|context| context.user_mode)
                .map(|context| super::RetiredTaskCleanup {
                    task_id: task_id.expect("live user task retirement requires a task identity"),
                    process_id: context
                        .process_id
                        .expect("live user task retirement requires a process identity"),
                    process_terminal,
                    clear_child_tid: retiring_linux_state
                        .map(|state| state.clear_child_tid)
                        .unwrap_or(0),
                    robust_list_head: retiring_linux_state
                        .map(|state| state.robust_list_head)
                        .unwrap_or(0),
                    robust_list_len: retiring_linux_state
                        .map(|state| state.robust_list_len)
                        .unwrap_or(0),
                });
        // Withdraw this slot before transferring process-directed pending
        // state. If the target's published frame is also corrupted, its
        // retirement must not transfer the same signal back here and recurse
        // indefinitely through two invalid contexts.
        self.retired[slot] = true;
        self.pending_reap = true;
        self.transfer_pending_process_sigchld(slot);
        if let Some(task_id) = task_id {
            self.release_ipc_priorities_for_task(task_id);
        }
        self.retirement_cleanup[slot] = retirement_cleanup;
        if let Some(context) = self.contexts[slot].as_mut() {
            context.ready = false;
            context.ready_since_ticks = 0;
            context.blocked = false;
            context.blocked_since_ticks = 0;
            context.wake_armed = false;
        }
        self.retire_reasons[slot] = Some(reason);
        let terminal_process_id =
            process.and_then(|(_, process_id)| process_terminal.then_some(process_id));
        if let Some(process_id) = terminal_process_id {
            self.release_ipc_priorities_for_process(process_id);
        }
        assert!(
            self.retirement_side_effects[slot]
                .replace(RetirementSideEffect::new(task_id, terminal_process_id))
                .is_none(),
            "scheduler retirement side effects published twice for one slot"
        );
    }

    fn is_last_live_user_task_for_process(
        &self,
        slot: usize,
        process_handle: ProcessHandle,
    ) -> bool {
        !self
            .contexts
            .iter()
            .enumerate()
            .any(|(candidate, context)| {
                candidate != slot
                    && !self.retired[candidate]
                    && context.is_some_and(|context| {
                        context.user_mode && context.process_handle == Some(process_handle)
                    })
            })
    }

    fn retire_exec_sibling_slot(&mut self, slot: usize) {
        let process_handle = self.contexts[slot].and_then(|context| context.process_handle);
        self.retire_slot(slot, TaskRetireReason::Exited);
        if let Some(process_handle) = process_handle {
            self.retirement_side_effects[slot]
                .as_mut()
                .expect("exec sibling retirement missing side-effect custody")
                .defer_process_detach(process_handle);
            if let Some(context) = self.contexts[slot].as_mut() {
                // Exec needs the process thread count to commit immediately,
                // but the detach itself runs only after the scheduler raw
                // owner is released. The task slot remains quarantined until
                // deferred runtime cleanup acknowledges its stamped identity.
                context.process_handle = None;
            }
        }
        self.publish_slot_identity(slot);
    }

    fn allocate_stack_storage(&mut self, slot: usize) -> Option<()> {
        if slot >= MAX_TASK {
            return None;
        }
        if self.stacks[slot].is_none() {
            let mut storage = Vec::new();
            storage.try_reserve_exact(TASK_STACK_SIZE).ok()?;
            unsafe {
                storage.set_len(TASK_STACK_SIZE);
            }
            self.stacks[slot] = Some(storage);
        }
        Some(())
    }

    fn release_stack_storage(&mut self, slot: usize) {
        if slot < MAX_TASK {
            self.stacks[slot] = None;
        }
    }

    fn reset_stack_storage(&mut self, slot: usize) -> Option<()> {
        self.allocate_stack_storage(slot)?;
        self.stack_storage_mut(slot).fill(0);
        let canary_words = TASK_STACK_GUARD_BYTES / mem::size_of::<u64>();
        let base = self.stack_storage_mut(slot).as_mut_ptr() as *mut u64;
        for index in 0..canary_words {
            unsafe {
                ptr::write(base.add(index), STACK_CANARY_WORD);
            }
        }
        Some(())
    }

    fn stack_canary_intact(&self, slot: usize) -> bool {
        let canary_words = TASK_STACK_GUARD_BYTES / mem::size_of::<u64>();
        let base = if let Some(storage) = self.stacks[slot].as_ref() {
            storage.as_ptr() as *const u64
        } else {
            let Some(context) = self.contexts[slot] else {
                return false;
            };
            let Some(raw_base) = context
                .kernel_stack_base
                .checked_sub(TASK_STACK_GUARD_BYTES as u64)
            else {
                return false;
            };
            raw_base as *const u64
        };
        for index in 0..canary_words {
            // SAFETY: every scheduler-owned or external idle stack reserves the
            // same fixed guard words below its admitted usable base.
            let value = unsafe { ptr::read(base.add(index)) };
            if value != STACK_CANARY_WORD {
                return false;
            }
        }
        true
    }

    fn scheduler_storage_bounds(&self) -> (usize, usize) {
        let base = self as *const Self as usize;
        (base, base + mem::size_of::<Self>())
    }

    fn scheduler_storage_contains(&self, addr: usize) -> bool {
        let (base, end) = self.scheduler_storage_bounds();
        if addr >= base && addr < end {
            return true;
        }

        let virt_offset = crate::memory::paging::KERNEL_VIRT_OFFSET as usize;
        if base >= virt_offset {
            let low_base = base - virt_offset;
            let low_end = end - virt_offset;
            return addr >= low_base && addr < low_end;
        }

        false
    }

    fn stack_bounds(&self, slot: usize) -> (usize, usize) {
        let base = self.stack_storage(slot).as_ptr() as usize;
        (
            base + TASK_STACK_GUARD_BYTES,
            align_kernel_stack_top(base + TASK_STACK_SIZE),
        )
    }

    fn stack_storage(&self, slot: usize) -> &[u8] {
        debug_assert!(slot < MAX_TASK);
        self.stacks[slot]
            .as_ref()
            .expect("scheduler task stack is allocated")
            .as_slice()
    }

    fn stack_storage_mut(&mut self, slot: usize) -> &mut [u8] {
        debug_assert!(slot < MAX_TASK);
        self.stacks[slot]
            .as_mut()
            .expect("scheduler task stack is allocated")
            .as_mut_slice()
    }

    fn stack_top(&self, slot: usize) -> usize {
        self.stack_bounds(slot).1
    }

    pub(super) fn current_saved_rsp(&self) -> usize {
        self.contexts[self.current_task_slot()]
            .map(|context| context.saved_rsp)
            .unwrap_or(0)
    }

    pub(super) fn set_current_alternate_kernel_stack(
        &mut self,
        base: u64,
        top: u64,
    ) -> Option<(u64, u64)> {
        if base == 0 || top <= base {
            return None;
        }

        let context = self.contexts[self.current_task_slot()].as_mut()?;
        let previous = (
            context.alternate_kernel_stack_base,
            context.alternate_kernel_stack_top,
        );
        context.alternate_kernel_stack_base = base;
        context.alternate_kernel_stack_top = top;
        Some(previous)
    }

    pub(super) fn restore_current_alternate_kernel_stack(&mut self, previous: (u64, u64)) {
        if let Some(context) = self.contexts[self.current_task_slot()].as_mut() {
            context.alternate_kernel_stack_base = previous.0;
            context.alternate_kernel_stack_top = previous.1;
        }
    }

    fn is_valid_saved_rsp(&self, slot: usize, saved_rsp: usize) -> bool {
        if saved_rsp == 0 {
            return false;
        }

        let align_mask = mem::align_of::<SavedContext>() - 1;
        if (saved_rsp & align_mask) != 0 {
            return false;
        }

        if slot >= MAX_TASK {
            return false;
        }

        let Some(frame_end) = saved_rsp.checked_add(SAVED_CONTEXT_BYTES) else {
            return false;
        };

        self.context_stack_contains(slot, saved_rsp, frame_end)
    }

    fn context_stack_contains(&self, slot: usize, start: usize, end: usize) -> bool {
        let Some(context) = self.contexts.get(slot).and_then(|context| *context) else {
            return false;
        };
        stack_range_contains(
            context.kernel_stack_base,
            context.kernel_stack_top,
            start,
            end,
        ) || stack_range_contains(
            context.alternate_kernel_stack_base,
            context.alternate_kernel_stack_top,
            start,
            end,
        )
    }

    fn context_validation_error(
        &self,
        slot: usize,
        context: TaskContext,
        saved_rsp: usize,
    ) -> Option<&'static str> {
        self.validate_saved_context(slot, context.user_mode, saved_rsp)
            .err()
    }

    fn saved_context_ref(saved_rsp: usize) -> Option<&'static SavedContext> {
        if saved_rsp == 0 || (saved_rsp & (mem::align_of::<SavedContext>() - 1)) != 0 {
            return None;
        }

        Some(unsafe { &*(saved_rsp as *const SavedContext) })
    }

    fn is_canonical_address(addr: u64) -> bool {
        let upper = addr >> 48;
        if ((addr >> 47) & 1) == 0 {
            upper == 0
        } else {
            upper == 0xFFFF
        }
    }

    fn validate_saved_context(
        &self,
        slot: usize,
        user_mode_task: bool,
        saved_rsp: usize,
    ) -> Result<(), &'static str> {
        if !self.is_valid_saved_rsp(slot, saved_rsp) {
            return Err("saved context pointer is outside the task stack");
        }
        if slot != 0 && !self.stack_canary_intact(slot) {
            return Err("kernel stack guard was corrupted");
        }

        let saved = Self::saved_context_ref(saved_rsp).ok_or("saved context pointer is invalid")?;
        if (saved.rflags & RFLAGS_RESERVED_BIT_1) == 0 {
            return Err("saved rflags lost the reserved bit");
        }

        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let kernel_cs = crate::arch::gdt::kernel_code_selector().0 as u64;
        let kernel_ss = crate::arch::gdt::kernel_data_selector().0 as u64;
        let kernel_process_task = self
            .contexts
            .get(slot)
            .and_then(|context| *context)
            .map(|context| !context.user_mode && context.process_handle.is_some())
            .unwrap_or(false);

        if saved.cs == user_cs {
            if !user_mode_task {
                return Err("kernel task cannot return directly to user mode");
            }
            if saved.ss != user_ss {
                return Err("user return frame carries an unexpected stack selector");
            }
            if !Self::is_canonical_address(saved.rip)
                || !Self::is_canonical_address(saved.rsp)
                || saved.rip >= crate::memory::paging::USER_SPACE_END_EXCLUSIVE
                || saved.rsp < crate::memory::paging::USER_SPACE_BASE
                || saved.rsp >= crate::memory::paging::USER_SPACE_END_EXCLUSIVE
            {
                return Err("user return frame points outside user space");
            }
            return Ok(());
        }

        if saved.cs != kernel_cs {
            return Err("saved code selector does not match any supported return mode");
        }
        if !Self::is_canonical_address(saved.rip) {
            return Err("kernel return RIP is not canonical");
        }
        if saved.rip >= crate::memory::paging::USER_SPACE_BASE
            && saved.rip < crate::memory::paging::USER_SPACE_END_EXCLUSIVE
            && !kernel_process_task
        {
            return Err("kernel return RIP points into user space");
        }
        if self.scheduler_storage_contains(saved.rip as usize) {
            return Err("kernel return RIP points into scheduler storage");
        }

        let kernel_interrupt_frame = saved.rsp == 1 && saved.ss == 0;
        let initial_kernel_frame =
            saved.ss == kernel_ss && Self::is_canonical_address(saved.rsp) && saved.rsp != 0;
        if !kernel_interrupt_frame && !initial_kernel_frame {
            return Err("kernel return frame has an invalid stack layout");
        }

        if initial_kernel_frame {
            let rsp = saved.rsp as usize;
            if !self.context_stack_contains(slot, rsp, rsp) {
                return Err("kernel return RSP does not belong to the task stack");
            }
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn allocate_kernel_slot(
        &mut self,
        entry: fn(u64),
        id: u64,
        pit_divisor: u16,
        cs: u64,
        ss: u64,
        rflags: u64,
        kernel_task_entry_rip: u64,
    ) -> Option<usize> {
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            if self.contexts[slot].is_none() {
                self.reset_stack_storage(slot)?;
                let (kernel_stack_base, kernel_stack_top) = self.stack_bounds(slot);
                self.contexts[slot] = Some(TaskContext {
                    saved_rsp: self.init_kernel_entry_context(
                        slot,
                        cs,
                        ss,
                        rflags,
                        kernel_task_entry_rip,
                        0,
                    ),
                    ready: true,
                    ready_since_ticks: crate::arch::rtc::ticks(),
                    blocked: false,
                    blocked_since_ticks: 0,
                    wake_armed: false,
                    weight: Self::weight_from_pit_divisor(pit_divisor),
                    vruntime_ns: self.new_task_vruntime(),
                    exec_start_ticks: 0,
                    address_space_root: crate::memory::paging::kernel_root_phys().as_u64(),
                    kernel_stack_base: kernel_stack_base as u64,
                    kernel_stack_top: kernel_stack_top as u64,
                    alternate_kernel_stack_base: 0,
                    alternate_kernel_stack_top: 0,
                    user_mode: false,
                    user_abi: None,
                    console_session: ConsoleSessionHandle::SYSTEM,
                    process_handle: None,
                    process_id: None,
                    user_stack: None,
                    windows_thread_state: None,
                });
                self.simd_states[slot] = SimdState::new();
                self.syscall_user_simd_active[slot] = false;
                self.starts[slot] = Some(TaskStart { entry, id });
                self.publish_slot_identity(slot);
                #[cfg(not(test))]
                self.admit_runqueue_slot(slot, true);
                return Some(slot);
            }
        }

        None
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn allocate_user_slot(
        &mut self,
        id: u64,
        address_space: ProcessAddressSpace,
        bootstrap: UserTaskBootstrap,
        parent_process_id: Option<u64>,
        pit_divisor: u16,
        user_cs: u64,
        user_ss: u64,
        rflags: u64,
        start_suspended: bool,
        idle_entry: fn(u64),
    ) -> Option<usize> {
        let inherited_process_mask = self.inherited_process_affinity(parent_process_id);
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            if self.contexts[slot].is_none() {
                self.reset_stack_storage(slot)?;
                let saved_rsp =
                    self.init_user_task_context(slot, &bootstrap, user_cs, user_ss, rflags);
                let exec_path = alloc::string::String::from(bootstrap.exec_path());
                let mut boxed_state = UserProcessState::new(
                    address_space,
                    bootstrap.linux_process_state,
                    bootstrap.linux_memory_map,
                    bootstrap.linux_runtime_profile,
                    bootstrap.windows_runtime,
                    bootstrap.logical_admin,
                    exec_path.as_str(),
                );
                if let Some(thread_state) = bootstrap.windows_thread_state
                    && let Err(error) = process::initialize_windows_thread_identifiers(
                        boxed_state.address_space_mut(),
                        thread_state.teb_address,
                        id,
                        id,
                    )
                {
                    panic!("failed to initialize windows thread ids: {:?}", error);
                }
                let root_phys = boxed_state.address_space().root_phys().as_u64();
                let process_handle =
                    process_table::create_process_with_parent(id, parent_process_id, boxed_state)?;
                let (kernel_stack_base, kernel_stack_top) = self.stack_bounds(slot);
                debug::debug!(
                    sched,
                    "allocate user task slot={} pid={} process={:?} root={:#x} exec={}",
                    slot,
                    id,
                    process_handle,
                    root_phys,
                    exec_path
                );

                self.contexts[slot] = Some(TaskContext {
                    saved_rsp,
                    ready: !start_suspended,
                    ready_since_ticks: if start_suspended {
                        0
                    } else {
                        crate::arch::rtc::ticks()
                    },
                    blocked: start_suspended,
                    blocked_since_ticks: if start_suspended {
                        crate::arch::rtc::ticks()
                    } else {
                        0
                    },
                    wake_armed: false,
                    weight: Self::weight_from_pit_divisor(pit_divisor),
                    vruntime_ns: self.new_task_vruntime(),
                    exec_start_ticks: 0,
                    address_space_root: root_phys,
                    kernel_stack_base: kernel_stack_base as u64,
                    kernel_stack_top: kernel_stack_top as u64,
                    alternate_kernel_stack_base: 0,
                    alternate_kernel_stack_top: 0,
                    user_mode: true,
                    user_abi: Some(bootstrap.abi),
                    console_session: bootstrap.console_session,
                    process_handle: Some(process_handle),
                    process_id: Some(id),
                    user_stack: bootstrap.user_stack,
                    windows_thread_state: bootstrap.windows_thread_state.map(|mut state| {
                        state.thread_id = id;
                        state
                    }),
                });
                self.simd_states[slot] = SimdState::new();
                self.syscall_user_simd_active[slot] = false;
                self.starts[slot] = Some(TaskStart {
                    entry: idle_entry,
                    id,
                });
                self.publish_slot_identity(slot);
                self.install_linux_thread_state(
                    slot,
                    bootstrap.linux_thread_state.map(|_| id),
                    bootstrap.linux_thread_state,
                );
                self.initialize_slot_affinity(slot, inherited_process_mask, inherited_process_mask);
                self.start_suspended[slot] = start_suspended;
                #[cfg(not(test))]
                self.admit_runqueue_slot(slot, !start_suspended);
                return Some(slot);
            }
        }

        None
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn allocate_user_process_state_slot(
        &mut self,
        id: u64,
        process_state: UserProcessState,
        bootstrap: UserTaskBootstrap,
        parent_process_id: Option<u64>,
        pit_divisor: u16,
        user_cs: u64,
        user_ss: u64,
        rflags: u64,
        idle_entry: fn(u64),
    ) -> Option<usize> {
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            if self.contexts[slot].is_none() {
                self.reset_stack_storage(slot)?;
                let saved_rsp =
                    self.init_user_task_context(slot, &bootstrap, user_cs, user_ss, rflags);
                let root_phys = process_state.address_space_root();
                let process_handle = process_table::create_process_with_parent(
                    id,
                    parent_process_id,
                    process_state,
                )?;
                let (kernel_stack_base, kernel_stack_top) = self.stack_bounds(slot);
                self.contexts[slot] = Some(TaskContext {
                    saved_rsp,
                    ready: true,
                    ready_since_ticks: crate::arch::rtc::ticks(),
                    blocked: false,
                    blocked_since_ticks: 0,
                    wake_armed: false,
                    weight: Self::weight_from_pit_divisor(pit_divisor),
                    vruntime_ns: self.new_task_vruntime(),
                    exec_start_ticks: 0,
                    address_space_root: root_phys,
                    kernel_stack_base: kernel_stack_base as u64,
                    kernel_stack_top: kernel_stack_top as u64,
                    alternate_kernel_stack_base: 0,
                    alternate_kernel_stack_top: 0,
                    user_mode: true,
                    user_abi: Some(bootstrap.abi),
                    console_session: bootstrap.console_session,
                    process_handle: Some(process_handle),
                    process_id: Some(id),
                    user_stack: bootstrap.user_stack,
                    windows_thread_state: bootstrap.windows_thread_state.map(|mut state| {
                        state.thread_id = id;
                        state
                    }),
                });
                self.simd_states[slot] = SimdState::new();
                self.syscall_user_simd_active[slot] = false;
                self.starts[slot] = Some(TaskStart {
                    entry: idle_entry,
                    id,
                });
                self.publish_slot_identity(slot);
                self.install_linux_thread_state(
                    slot,
                    bootstrap.linux_thread_state.map(|_| id),
                    bootstrap.linux_thread_state,
                );
                #[cfg(not(test))]
                self.admit_runqueue_slot(slot, true);
                return Some(slot);
            }
        }

        None
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn allocate_kernel_process_slot(
        &mut self,
        id: u64,
        process_state: UserProcessState,
        entry: VirtAddr,
        arg0: u64,
        pit_divisor: u16,
        kernel_cs: u64,
        kernel_ss: u64,
        rflags: u64,
    ) -> Option<usize> {
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            if self.contexts[slot].is_none() {
                self.reset_stack_storage(slot)?;
                let root_phys = process_state.address_space_root();
                let _exec_path = String::from(process_state.exec_path());
                let process_handle = process_table::create_process(id, process_state)?;
                let (kernel_stack_base, kernel_stack_top) = self.stack_bounds(slot);
                self.contexts[slot] = Some(TaskContext {
                    saved_rsp: self.init_kernel_entry_context(
                        slot,
                        kernel_cs,
                        kernel_ss,
                        rflags,
                        entry.as_u64(),
                        arg0,
                    ),
                    ready: true,
                    ready_since_ticks: crate::arch::rtc::ticks(),
                    blocked: false,
                    blocked_since_ticks: 0,
                    wake_armed: false,
                    weight: Self::weight_from_pit_divisor(pit_divisor),
                    vruntime_ns: self.new_task_vruntime(),
                    exec_start_ticks: 0,
                    address_space_root: root_phys,
                    kernel_stack_base: kernel_stack_base as u64,
                    kernel_stack_top: kernel_stack_top as u64,
                    alternate_kernel_stack_base: 0,
                    alternate_kernel_stack_top: 0,
                    user_mode: false,
                    user_abi: None,
                    console_session: ConsoleSessionHandle::SYSTEM,
                    process_handle: Some(process_handle),
                    process_id: Some(id),
                    user_stack: None,
                    windows_thread_state: None,
                });
                self.simd_states[slot] = SimdState::new();
                self.syscall_user_simd_active[slot] = false;
                self.starts[slot] = Some(TaskStart {
                    entry: super::noop_task_entry,
                    id,
                });
                self.publish_slot_identity(slot);
                debug::debug!(
                    sched,
                    "allocate kernel process slot={} pid={} process={:?} root={:#x} entry={:#x} exec={}",
                    slot,
                    id,
                    process_handle,
                    root_phys,
                    entry.as_u64(),
                    _exec_path
                );
                #[cfg(not(test))]
                self.admit_runqueue_slot(slot, true);
                return Some(slot);
            }
        }

        None
    }

    fn init_kernel_entry_context(
        &mut self,
        slot: usize,
        cs: u64,
        ss: u64,
        rflags: u64,
        entry_rip: u64,
        arg0: u64,
    ) -> usize {
        let stack_top = self.stack_top(slot);

        let task_rsp = stack_top - TASK_ENTRY_STACK_RESERVE_QWORDS * mem::size_of::<u64>();
        unsafe {
            let stack_slots = task_rsp as *mut u64;
            ptr::write(stack_slots, task_rsp as u64);
            ptr::write(stack_slots.add(1), ss);
            ptr::write(stack_slots.add(2), 0);
        }

        let context_ptr = task_rsp - mem::size_of::<SavedContext>();
        let context = context_ptr as *mut SavedContext;

        unsafe {
            ptr::write_bytes(context as *mut u8, 0, mem::size_of::<SavedContext>());
            (*context).rdi = arg0;
            (*context).rsp = task_rsp as u64;
            (*context).ss = ss;
            (*context).rip = entry_rip;
            (*context).cs = cs;
            (*context).rflags = rflags;
        }

        context_ptr
    }

    fn init_user_task_context(
        &mut self,
        slot: usize,
        bootstrap: &UserTaskBootstrap,
        user_cs: u64,
        user_ss: u64,
        rflags: u64,
    ) -> usize {
        let stack_top = self.stack_top(slot);
        let context_ptr = stack_top - mem::size_of::<SavedContext>();
        let context = context_ptr as *mut SavedContext;

        unsafe {
            ptr::write_bytes(context as *mut u8, 0, mem::size_of::<SavedContext>());
            (*context).rax = bootstrap.registers.rax;
            (*context).rbx = bootstrap.registers.rbx;
            (*context).rcx = bootstrap.registers.rcx;
            (*context).rdx = bootstrap.registers.rdx;
            (*context).rsi = bootstrap.registers.rsi;
            (*context).rdi = bootstrap.registers.rdi;
            (*context).rbp = bootstrap.registers.rbp;
            (*context).r8 = bootstrap.registers.r8;
            (*context).r9 = bootstrap.registers.r9;
            (*context).r10 = bootstrap.registers.r10;
            (*context).r11 = bootstrap.registers.r11;
            (*context).r12 = bootstrap.registers.r12;
            (*context).r13 = bootstrap.registers.r13;
            (*context).r14 = bootstrap.registers.r14;
            (*context).r15 = bootstrap.registers.r15;
            (*context).rsp = bootstrap.stack_pointer.as_u64();
            (*context).ss = user_ss;
            (*context).rip = bootstrap.entry.as_u64();
            (*context).cs = user_cs;
            (*context).rflags = rflags;
        }

        context_ptr
    }

    #[track_caller]
    fn retire_slot_due_to_invalid_context(
        &mut self,
        slot: usize,
        saved_rsp: usize,
        reason: &'static str,
    ) {
        if self.contexts[slot].is_none() {
            return;
        }
        self.mark_slot_ready(slot, saved_rsp, false);
        let retire_reason = TaskRetireReason::CorruptedContext {
            saved_rsp,
            reason,
            reason_code: context_validation_reason_code(reason),
        };
        if nucleus_core::util::lockdep::irq_context_depth() != 0 {
            self.quarantine_slot_for_deferred_retirement(slot, retire_reason);
        } else {
            self.retire_slot(slot, retire_reason);
        }
    }

    /// Make an invalid task undispatchable without running lifecycle teardown
    /// in hardware interrupt context. The fixed scheduler slot itself is the
    /// queue entry, so an interrupt storm cannot allocate or enqueue duplicate
    /// work. Housekeeping later calls `retire_slot` to revoke IPC/process
    /// authority and publish the normal runtime-cleanup snapshot.
    fn quarantine_slot_for_deferred_retirement(&mut self, slot: usize, reason: TaskRetireReason) {
        assert!(
            slot != ROOT_TASK_SLOT,
            "scheduler root kernel task cannot be quarantined"
        );
        if self.deferred_retire_reasons[slot].is_some() {
            return;
        }
        #[cfg(not(test))]
        runqueue::retire(
            slot,
            self.contexts[slot]
                .map(|context| context.weight)
                .unwrap_or(NICE_0_LOAD),
        );
        self.retired[slot] = true;
        self.pending_reap = true;
        self.retire_reasons[slot] = Some(reason);
        self.deferred_retire_reasons[slot] = Some(reason);
        if let Some(context) = self.contexts[slot].as_mut() {
            context.ready = false;
            context.ready_since_ticks = 0;
            context.blocked = false;
            context.blocked_since_ticks = 0;
            context.wake_armed = false;
        }
    }

    fn finalize_deferred_retirements(&mut self) {
        assert_eq!(
            nucleus_core::util::lockdep::irq_context_depth(),
            0,
            "deferred task retirement drained from interrupt context"
        );
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            let Some(reason) = self.deferred_retire_reasons[slot].take() else {
                continue;
            };
            if self.contexts[slot].is_none() {
                self.retired[slot] = false;
                self.retire_reasons[slot] = None;
                continue;
            }
            self.retire_slot(slot, reason);
        }
    }

    fn log_invalid_context(
        &self,
        slot: usize,
        saved_rsp: usize,
        reason: &'static str,
        source: &'static str,
    ) {
        let (stack_base, stack_top) = if slot < MAX_TASK {
            self.stack_bounds(slot)
        } else {
            (0, 0)
        };
        let (scheduler_base, scheduler_end) = self.scheduler_storage_bounds();
        let frame_end = saved_rsp.checked_add(SAVED_CONTEXT_BYTES).unwrap_or(0);
        let context = self.contexts.get(slot).and_then(|context| *context);
        let saved = Self::saved_context_ref(saved_rsp);
        debug::warn!(
            sched,
            "scheduler invalid context source={} slot={} current={} reason={} saved_rsp={:#x} frame_end={:#x} stack=[{:#x},{:#x}) scheduler=[{:#x},{:#x}) context_ready={} context_blocked={} context_user={} context_stack=[{:#x},{:#x}) alt_stack=[{:#x},{:#x}) saved_rip={:#x} saved_cs={:#x} saved_frame_rsp={:#x} saved_ss={:#x} saved_rflags={:#x}",
            source,
            slot,
            self.current_task_slot(),
            reason,
            saved_rsp,
            frame_end,
            stack_base,
            stack_top,
            scheduler_base,
            scheduler_end,
            context.map(|context| context.ready).unwrap_or(false),
            context.map(|context| context.blocked).unwrap_or(false),
            context.map(|context| context.user_mode).unwrap_or(false),
            context
                .map(|context| context.kernel_stack_base)
                .unwrap_or(0),
            context.map(|context| context.kernel_stack_top).unwrap_or(0),
            context
                .map(|context| context.alternate_kernel_stack_base)
                .unwrap_or(0),
            context
                .map(|context| context.alternate_kernel_stack_top)
                .unwrap_or(0),
            saved.map(|saved| saved.rip).unwrap_or(0),
            saved.map(|saved| saved.cs).unwrap_or(0),
            saved.map(|saved| saved.rsp).unwrap_or(0),
            saved.map(|saved| saved.ss).unwrap_or(0),
            saved.map(|saved| saved.rflags).unwrap_or(0),
        );
    }

    /// Enforce the scheduler's complete live-task state partition at the
    /// dispatch linearization point. After the outgoing task has published
    /// either Ready or Blocked, every other admitted task must be runnable,
    /// blocked, explicitly suspended/stopped, or retired. A live slot in none
    /// of those states has lost both CPU ownership and wake authority; letting
    /// the scheduler continue would turn a bounded wait into an invisible
    /// permanent hang.
    #[cfg(test)]
    fn assert_live_task_state_partition(&self) {
        for slot in 1..MAX_TASK {
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if super::task_slot_is_running(slot) {
                continue;
            }
            if live_task_state_is_partitioned(
                slot,
                self.current_task_slot(),
                self.retired[slot],
                self.start_suspended[slot],
                self.job_stopped[slot],
                context.ready,
                context.blocked,
            ) {
                continue;
            }
            let task_id = self.starts[slot].map(|start| start.id).unwrap_or(0);
            panic!(
                "scheduler live task lost state partition: slot={} task={} pid={:?} current_slot={} ready={} blocked={} wake_armed={}",
                slot,
                task_id,
                context.process_id,
                self.current_task_slot(),
                context.ready,
                context.blocked,
                context.wake_armed
            );
        }
    }

    pub(super) fn on_timer_interrupt(&mut self, current_rsp: usize) -> SchedulerDispatch {
        self.dispatch_schedule(current_rsp, false)
    }

    pub(super) fn on_voluntary_yield(&mut self, current_rsp: usize) -> SchedulerDispatch {
        self.dispatch_schedule(current_rsp, true)
    }

    fn dispatch_schedule(
        &mut self,
        current_rsp: usize,
        voluntary_yield: bool,
    ) -> SchedulerDispatch {
        let dispatch_cpu = nucleus_core::util::lockdep::current_cpu_index();
        let mut phase_marker = crate::arch::clock::monotonic_nanos();
        #[cfg(not(test))]
        let current_cpu = dispatch_cpu;
        let current_slot = self.current_task_slot();
        let current_task_id = self.starts[current_slot]
            .map(|start| start.id)
            .expect("running task missing scheduler identity");
        let now_ticks = crate::arch::rtc::ticks();
        let current_runtime_ns = self.contexts[current_slot]
            .map(|context| context.exec_start_ticks)
            .filter(|start| *start != 0 && now_ticks > *start)
            .map(|start| Self::ticks_elapsed_ns(start, now_ticks))
            .unwrap_or(0);

        // Account vruntime for the outgoing slot first, regardless of what we
        // do with it next. This makes the CFS-like fairness accounting see
        // every CPU cycle a task actually consumed. On voluntary yields we
        // floor the charge — see `account_current_runtime` for the rationale.
        self.account_current_runtime(current_slot, now_ticks, voluntary_yield);

        if self.retired[current_slot]
            || self.contexts[current_slot]
                .map(|ctx| ctx.blocked)
                .unwrap_or(false)
        {
            self.mark_slot_ready(current_slot, current_rsp, false);
        } else if self.contexts[current_slot]
            .and_then(|ctx| self.context_validation_error(current_slot, ctx, current_rsp))
            .is_none()
        {
            self.mark_slot_ready(current_slot, current_rsp, true);
        } else {
            let reason = self.contexts[current_slot]
                .and_then(|ctx| self.context_validation_error(current_slot, ctx, current_rsp))
                .unwrap_or("current task context is missing");
            self.log_invalid_context(current_slot, current_rsp, reason, "current");
            if current_slot == ROOT_TASK_SLOT {
                panic!("scheduler root kernel context is corrupted: {}", reason);
            }
            self.retire_slot_due_to_invalid_context(current_slot, current_rsp, reason);
        }

        self.mark_phase(SchedulerPhase::Account, &mut phase_marker);

        #[cfg(not(test))]
        {
            let owner = runqueue::owner(current_slot);
            if owner.state == runqueue::RunOwnerState::Running {
                let weight = self.contexts[current_slot]
                    .map(|context| context.weight)
                    .unwrap_or(NICE_0_LOAD);
                let current_affinity = self.task_affinity_masks[current_slot]
                    & self.process_affinity_masks[current_slot];
                let current_cpu_is_eligible = current_affinity & (1_u64 << current_cpu) != 0;
                if self.retired[current_slot]
                    || self.contexts[current_slot]
                        .is_none_or(|context| !context.ready || context.blocked)
                {
                    runqueue::publish_blocked(current_slot, current_cpu, weight);
                } else if !current_cpu_is_eligible {
                    runqueue::publish_blocked(current_slot, current_cpu, weight);
                    assert!(
                        self.publish_runqueue_wake(current_slot),
                        "scheduler could not rehome affinity-excluded current slot={current_slot}"
                    );
                } else {
                    runqueue::publish_local(current_slot, current_cpu, weight);
                }
            } else {
                assert!(
                    owner.state.is_terminal(),
                    "scheduler current task entered dispatch without Running or terminal custody slot={} owner={owner:?}",
                    current_slot
                );
            }
            let _ = runqueue::drain_remote_wakes(current_cpu);
            if self.slot_class(current_slot) == Some(SchedClass::Idle) {
                let _ = self.steal_one_for_idle_cpu(current_cpu);
            } else if !voluntary_yield {
                let _ = self.rebalance_one_from_busy_cpu(current_cpu, now_ticks);
            }
        }

        self.mark_phase(SchedulerPhase::Balance, &mut phase_marker);

        // The selected frame is validated on every dispatch below. Sweep all
        // other immutable ready frames at a bounded diagnostic cadence rather
        // than on every software handoff; the old O(local-runnable) sweep was
        // a material part of the global-lock convoy under IPC-heavy SMP load.
        if self.periodic_ready_validation_due() {
            self.retire_invalid_ready_tasks();
        }
        #[cfg(test)]
        self.assert_live_task_state_partition();
        self.mark_phase(SchedulerPhase::Validate, &mut phase_marker);

        // Refresh cached min_vruntime: this is fed to newly-spawned tasks so
        // they do not preempt the rest of the system on creation alone.
        let local_min_vruntime = self.min_ready_vruntime();
        self.current_dispatch_policy_mut().last_min_vruntime_ns = local_min_vruntime;
        #[cfg(not(test))]
        {
            self.runtime_profile_runnable_samples = self
                .runtime_profile_runnable_samples
                .saturating_add(runqueue::published_runnable_count(current_cpu) as u64);
        }
        self.mark_phase(SchedulerPhase::SelectVruntime, &mut phase_marker);

        // Pick order:
        //  1. The bounded first-turn prefix of one atomically activated
        //     startup cohort. The loader reply is already committed and may
        //     wait for at most the ABI-bounded eight sibling turns.
        //  2. One synchronous IPC handoff, bounded by an eight-turn burst.
        //     This is either the receiver required by a newly live reply
        //     capability or the caller awakened by its completion; unrelated
        //     overdue work must not turn that transaction into inversion.
        //  3. One post-admission spawn handoff. The supervisor has already
        //     consumed the exact deferred-activation capability, so delaying
        //     this first child turn behind unrelated reply wakeups creates an
        //     avoidable boot-critical latency gap without improving safety.
        //  4. Bounded wakeup handoff for a task that actually slept on IPC.
        //  5. IPC donation hints and mandatory User fairness.
        //  6. CFS-like class selection among ready tasks.
        //  7. Root task as the unconditional fallback.
        // Hints short-circuit CFS only for direct handoff; otherwise vruntime
        // decides fairness.
        // A caller that committed a synchronous IPC block has no remaining
        // work until its exact receiver runs, and a successful reply makes its
        // exact caller runnable. Honor that transaction-scoped direct handoff
        // before the unrelated User reservation; a committed child
        // activation is already an explicit, one-shot bootstrap transfer and
        // must run before either category of ordinary IPC wakeup.
        let atomic_activation_handoff = self.take_next_atomic_activation_handoff_ready_slot();
        let sync_handoff = if atomic_activation_handoff.is_none() {
            self.take_next_synchronous_pick_hint_ready_slot()
        } else {
            None
        };
        let (next_idx, ipc_handoff, reserved_user_pick, latency_handoff_pick, sync_handoff_pick) =
            match atomic_activation_handoff {
                Some(child_slot) => (child_slot, true, None, false, false),
                None => match sync_handoff {
                    Some(peer_slot) => (peer_slot, true, None, false, true),
                    None => {
                        let mandatory_overdue =
                            self.mandatory_overdue_system_pick(current_slot, now_ticks);
                        let bootstrap_handoff = if mandatory_overdue.is_none() {
                            self.take_next_bootstrap_handoff_ready_slot()
                        } else {
                            None
                        };
                        let (next_idx, ipc_handoff, reserved_user_pick, latency_handoff_pick) =
                            match mandatory_overdue {
                                Some(overdue_slot) => (overdue_slot, true, None, false),
                                None => match bootstrap_handoff {
                                    Some((woken_slot, is_latency_handoff)) => {
                                        (woken_slot, true, None, is_latency_handoff)
                                    }
                                    None => {
                                        let blocking_ipc_handoff = self.contexts[current_slot]
                                            .is_some_and(|context| context.blocked)
                                            .then(|| self.take_next_pick_hint_ready_slot())
                                            .flatten();
                                        let (next_idx, ipc_handoff, reserved_user_pick) =
                                            match blocking_ipc_handoff {
                                                Some(receiver_slot) => (receiver_slot, true, None),
                                                None => {
                                                    match self.reserved_user_pick(current_slot) {
                                                        Some(user_slot) => {
                                                            (user_slot, false, Some(user_slot))
                                                        }
                                                        None => {
                                                            let overdue_or_hint = self
                                                                .take_overdue_system_or_pick_hint(
                                                                    current_slot,
                                                                    now_ticks,
                                                                );
                                                            let cfs_pick = if voluntary_yield {
                                                                self.pick_min_vruntime_excluding(
                                                                    current_slot,
                                                                )
                                                                .or_else(|| {
                                                                    self.pick_min_vruntime(
                                                                        current_slot,
                                                                    )
                                                                })
                                                            } else {
                                                                self.pick_min_vruntime(current_slot)
                                                            };
                                                            match (overdue_or_hint, cfs_pick) {
                                                                (Some(handoff_slot), _) => {
                                                                    (handoff_slot, true, None)
                                                                }
                                                                (None, Some(slot)) => {
                                                                    (slot, false, None)
                                                                }
                                                                (None, None) => (
                                                                    self.idle_fallback_slot(),
                                                                    false,
                                                                    None,
                                                                ),
                                                            }
                                                        }
                                                    }
                                                }
                                            };
                                        (next_idx, ipc_handoff, reserved_user_pick, false)
                                    }
                                },
                            };
                        (
                            next_idx,
                            ipc_handoff,
                            reserved_user_pick,
                            latency_handoff_pick,
                            false,
                        )
                    }
                },
            };

        self.mark_phase(SchedulerPhase::SelectHandoff, &mut phase_marker);

        // Apply min-granularity guard: if the CFS pick differs from current
        // only by a few ns of vruntime advantage and current has not consumed
        // a slice of at least SCHED_MIN_GRANULARITY_NS, keep current to avoid
        // context-switch ping-pong. This is the same heuristic Linux uses to
        // damp wake-preempt thrash.
        let next_idx = if ipc_handoff || voluntary_yield || reserved_user_pick.is_some() {
            next_idx
        } else {
            self.maybe_keep_current(current_slot, next_idx, current_runtime_ns)
        };
        self.mark_phase(SchedulerPhase::SelectPick, &mut phase_marker);
        if let Some(next) = self.contexts[next_idx] {
            match self.context_validation_error(next_idx, next, next.saved_rsp) {
                None => {
                    #[cfg(not(test))]
                    assert!(
                        runqueue::claim_dispatch(next_idx, dispatch_cpu, next.weight),
                        "scheduler selected a task without local rq custody slot={} cpu={}",
                        next_idx,
                        dispatch_cpu
                    );
                    self.trace_switch(current_slot, next_idx);
                    if next_idx != current_slot && next.ready_since_ticks != 0 {
                        let task_id = self.starts[next_idx].map(|start| start.id);
                        let process_id = next.process_id;
                        self.maybe_log_ready_wait(
                            next_idx,
                            task_id,
                            process_id,
                            next.ready_since_ticks,
                            now_ticks,
                        );
                    }
                    if let Some(context) = self.contexts[next_idx].as_mut() {
                        context.ready = false;
                        context.ready_since_ticks = 0;
                        context.exec_start_ticks = now_ticks;
                    }
                    self.record_dispatch_class(next_idx);
                    self.record_runtime_profile_dispatch(next_idx);
                    self.record_runtime_profile_transition(current_slot, next_idx, dispatch_cpu);
                    self.record_task_dispatch_cpu(next_idx, dispatch_cpu);
                    self.record_latency_handoff(latency_handoff_pick);
                    self.record_synchronous_handoff(sync_handoff_pick);
                    self.set_current_task_slot(next_idx);
                    let next_task_id = self.starts[next_idx]
                        .map(|start| start.id)
                        .expect("schedulable task missing lockdep owner identity");
                    nucleus_core::util::lockdep::record_scheduler_dispatch(
                        dispatch_cpu,
                        current_task_id,
                        next_task_id,
                        current_slot,
                        next_idx,
                        next.saved_rsp,
                        self.slot_class(next_idx) == Some(SchedClass::Idle),
                        atomic_activation_handoff.is_some(),
                    );
                    nucleus_core::util::lockdep::set_current_task_owner(
                        next_task_id
                            .checked_add(1)
                            .expect("task id exhausted lock owner token"),
                    );
                    self.mark_phase(SchedulerPhase::Commit, &mut phase_marker);
                    return SchedulerDispatch::new(
                        next.saved_rsp,
                        self.scheduler_tick_divisor,
                        current_slot,
                        next_idx,
                    );
                }
                Some(reason) if next_idx == ROOT_TASK_SLOT => {
                    self.log_invalid_context(next_idx, next.saved_rsp, reason, "next");
                    panic!("scheduler root kernel context is corrupted: {}", reason);
                }
                Some(reason) => {
                    self.log_invalid_context(next_idx, next.saved_rsp, reason, "next");
                    self.retire_slot_due_to_invalid_context(next_idx, next.saved_rsp, reason);
                }
            }
        }

        let current = self.contexts[current_slot].expect("scheduler lost the current task context");
        #[cfg(not(test))]
        assert!(
            runqueue::claim_dispatch(current_slot, dispatch_cpu, current.weight),
            "scheduler fallback task lost local rq custody slot={} cpu={}",
            current_slot,
            dispatch_cpu
        );
        // Keep running current: refresh its exec_start_ticks so subsequent
        // vruntime accounting sees a non-zero baseline.
        if let Some(ctx) = self.contexts[current_slot].as_mut() {
            ctx.exec_start_ticks = now_ticks;
            ctx.ready = false;
        }
        nucleus_core::util::lockdep::record_scheduler_dispatch(
            nucleus_core::util::lockdep::current_cpu_index(),
            current_task_id,
            current_task_id,
            current_slot,
            current_slot,
            current.saved_rsp,
            self.slot_class(current_slot) == Some(SchedClass::Idle),
            false,
        );
        self.record_runtime_profile_dispatch(current_slot);
        self.record_runtime_profile_transition(current_slot, current_slot, dispatch_cpu);
        self.record_task_dispatch_cpu(current_slot, dispatch_cpu);
        self.mark_phase(SchedulerPhase::Commit, &mut phase_marker);
        SchedulerDispatch::new(
            current.saved_rsp,
            self.scheduler_tick_divisor,
            current_slot,
            current_slot,
        )
    }

    /// Min-granularity guard. Returns either `cfs_pick` (preempt) or
    /// `current_slot` (keep running) based on whether preemption is worth
    /// the context-switch cost. Mirrors `wakeup_preempt_entity` /
    /// `check_preempt_tick` from Linux CFS at a much smaller scale.
    fn maybe_keep_current(
        &self,
        current_slot: usize,
        cfs_pick: usize,
        current_runtime_ns: u64,
    ) -> usize {
        if cfs_pick == current_slot {
            if current_runtime_ns >= SCHED_MAX_BURST_NS
                && let Some(alternate) = self.pick_burst_alternate_in_current_class(current_slot)
            {
                return alternate;
            }
            return cfs_pick;
        }
        if !self.is_fair_candidate_slot(current_slot) {
            return cfs_pick;
        }
        let Some(current_ctx) = self.contexts[current_slot] else {
            return cfs_pick;
        };
        if !current_ctx.ready || !self.context_is_schedulable(current_slot, current_ctx) {
            return cfs_pick;
        }
        let Some(pick_ctx) = self.contexts[cfs_pick] else {
            return cfs_pick;
        };

        // Cross-class preemption is unconditional. The min-granularity guard
        // below is a CFS fairness damper that only makes sense between peers
        // in the same class — applying it across bands would let, say, a
        // sub-tick User-class task block a freshly-woken System task and
        // recreate exactly the latency bug this scheduler is meant to fix.
        // Mirrors Mach's QoS preemption rule and Linux's "RT > CFS, no
        // sched_min_granularity check between policies" stacking.
        if let (Some(current_class), Some(pick_class)) =
            (self.slot_class(current_slot), self.slot_class(cfs_pick))
            && pick_class < current_class
        {
            return cfs_pick;
        }

        // If the pick's vruntime advantage is large, preempt unconditionally:
        // current has burned through too much CPU relative to peers.
        let current_v = current_ctx.vruntime_ns;
        let pick_v = pick_ctx.vruntime_ns;
        let advantage_ns = current_v.saturating_sub(pick_v);

        // Linux rule (simplified): only preempt if either
        //   - current has run at least SCHED_MIN_GRANULARITY_NS, or
        //   - the peer's vruntime advantage exceeds SCHED_MIN_GRANULARITY_NS
        //     (peer was starved long enough to deserve immediate dispatch).
        if current_runtime_ns >= SCHED_MIN_GRANULARITY_NS
            || advantage_ns >= SCHED_MIN_GRANULARITY_NS
            || current_runtime_ns >= SCHED_MAX_BURST_NS
        {
            cfs_pick
        } else {
            current_slot
        }
    }

    #[allow(clippy::needless_return)]
    fn trace_switch(&self, from_slot: usize, to_slot: usize) {
        if from_slot == to_slot {
            return;
        }
        #[cfg(rustos_log_sched_debug)]
        {
            let from_context = self.contexts.get(from_slot).and_then(|context| *context);
            let to_context = self.contexts.get(to_slot).and_then(|context| *context);
            let from_user = from_context
                .map(|context| context.user_mode)
                .unwrap_or(false);
            let to_user = to_context.map(|context| context.user_mode).unwrap_or(false);
            let from_rip = from_context
                .and_then(|context| saved_context_rip(context.saved_rsp))
                .unwrap_or(0);
            let to_rip = to_context
                .and_then(|context| saved_context_rip(context.saved_rsp))
                .unwrap_or(0);
            let from_rsp = from_context.map(|context| context.saved_rsp).unwrap_or(0);
            let to_rsp = to_context.map(|context| context.saved_rsp).unwrap_or(0);
            let from_id = self
                .starts
                .get(from_slot)
                .and_then(|start| *start)
                .map(|start| start.id);
            let to_id = self
                .starts
                .get(to_slot)
                .and_then(|start| *start)
                .map(|start| start.id);

            if !debug::enabled!(sched, debug) {
                return;
            }

            debug::debug!(
                sched,
                alloc::format!(
                    "switch slot {} ({:?}, user={}, rip={:#x}, rsp={:#x}) -> slot {} ({:?}, user={}, rip={:#x}, rsp={:#x})",
                    from_slot,
                    from_id,
                    from_user,
                    from_rip,
                    from_rsp,
                    to_slot,
                    to_id,
                    to_user,
                    to_rip,
                    to_rsp
                )
                .as_str()
            );
        }
    }

    // The entry trampoline reads this metadata indirectly through its higher-half
    // address, so ordinary Rust call-site analysis cannot see the live use.
    // ASSEMBLY: `task_entry_trampoline` is installed in a synthesized context.
    #[allow(dead_code)]
    pub(super) fn current_task_start(&self) -> Option<TaskStart> {
        let slot = self.current_task_slot();
        self.starts[slot].filter(|_| {
            self.contexts[slot]
                .map(|ctx| !ctx.user_mode)
                .unwrap_or(false)
        })
    }

    pub(super) fn current_task_is_user_task(&self) -> bool {
        self.contexts[self.current_task_slot()]
            .map(|context| context.user_mode)
            .unwrap_or(false)
    }

    pub(super) fn current_task_is_idle_task(&self) -> bool {
        self.slot_class(self.current_task_slot()) == Some(SchedClass::Idle)
    }

    pub(super) fn current_task_is_retired(&self) -> bool {
        self.retired[self.current_task_slot()]
    }

    pub(super) fn current_task_is_blocked(&self) -> bool {
        self.contexts[self.current_task_slot()]
            .map(|context| context.blocked)
            .unwrap_or(false)
    }

    pub(super) fn current_process_handle(&self) -> Option<ProcessHandle> {
        self.contexts[self.current_task_slot()]?.process_handle
    }

    pub(super) fn prepare_current_task_execution(&mut self) {
        let current_slot = self.current_task_slot();
        let current =
            self.contexts[current_slot].expect("scheduler selected a missing task context");
        self.assert_current_task_affinity_allows_dispatch();
        self.affinity_migration_pending[current_slot] = false;
        let return_to_user = self.context_returns_to_user(current);
        self.validate_saved_context(current_slot, current.user_mode, current.saved_rsp)
            .expect("scheduler selected an invalid task context");
        crate::memory::paging::load_address_space_phys(PhysAddr::new(current.address_space_root));
        if current.kernel_stack_top != 0 {
            assert_eq!(
                current.kernel_stack_top & 0xF,
                0,
                "scheduler selected a kernel stack top that violates the x86_64 SysV ABI"
            );
            crate::arch::gdt::set_privilege_stack(current.kernel_stack_top);
            crate::user::syscall::set_kernel_stack_top(current.kernel_stack_top);
        }

        let fs_base = self
            .linux_thread_state(current_slot)
            .map(|state| state.fs_base)
            .unwrap_or(0);
        let user_gs_base = current
            .windows_thread_state
            .map(|state| state.teb_address)
            .unwrap_or(0);
        // Linux/x86_64 relies on FS/GS bases rather than visible segment selectors in
        // long mode. Keep user data selectors only in the iret frame (CS/SS) and clear
        // the other visible data selectors when returning to ring 3 so glibc sees the
        // usual flat-user environment.
        let data_selector = if return_to_user {
            SegmentSelector(0)
        } else {
            crate::arch::gdt::kernel_data_selector()
        };
        unsafe {
            DS::set_reg(data_selector);
            ES::set_reg(data_selector);
            FS::set_reg(data_selector);
            GS::set_reg(data_selector);
        }
        FsBase::write(VirtAddr::new(fs_base));
        crate::user::syscall::prepare_for_context_return(return_to_user, user_gs_base);
    }

    /// Restore task-specific architectural state only across a real task
    /// switch. Same-task scheduler turns retain the already-active CR3, TSS,
    /// syscall stack, segment state, and FS/GS bases. The SIMD image remains
    /// restored by every IRQ leaf because compiler-generated kernel code may
    /// use vector registers after the save boundary.
    pub(super) fn prepare_dispatched_task_execution(&mut self, dispatch: SchedulerDispatch) {
        let mut phase_marker = crate::arch::clock::monotonic_nanos();
        assert_eq!(
            self.current_task_slot(), dispatch.next_slot,
            "scheduler invariant: architecture restore token does not name current task"
        );
        if !dispatch.requires_architectural_restore() {
            assert_eq!(
                dispatch.previous_slot, dispatch.next_slot,
                "scheduler invariant: same-task restore token changed slot identity"
            );
            self.mark_phase(SchedulerPhase::ArchRestore, &mut phase_marker);
            return;
        }
        self.prepare_current_task_execution();
        self.mark_phase(SchedulerPhase::ArchRestore, &mut phase_marker);
    }

    pub(super) fn reap_inactive_retired_slots(&mut self) -> Option<RetiredSlotReclaim> {
        if !self.pending_reap {
            return None;
        }

        self.finalize_deferred_retirements();
        let active_root = self.contexts[self.current_task_slot()].map(|ctx| ctx.address_space_root);
        let mut still_pending = false;

        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            if !self.retired[slot] {
                continue;
            }
            if self.retirement_cleanup[slot].is_some() {
                still_pending = true;
                continue;
            }
            if self.retirement_side_effects[slot].is_some() {
                still_pending = true;
                continue;
            }

            let Some(context) = self.contexts[slot] else {
                self.retired[slot] = false;
                continue;
            };

            if super::task_slot_is_running(slot)
                || (context.user_mode && Some(context.address_space_root) == active_root)
            {
                still_pending = true;
                continue;
            }

            // Keep the serialized phase bounded to metadata detachment. Stack
            // destruction and process-table reclamation can release pages and
            // must run after the global scheduler raw owner is dropped.
            self.pending_reap = true;
            return Some(self.take_slot_reclaim(slot));
        }

        self.pending_reap = still_pending;
        None
    }

    pub(super) fn take_retirement_side_effect(&mut self) -> Option<RetirementSideEffect> {
        self.retirement_side_effects
            .iter_mut()
            .find_map(Option::take)
    }

    pub(super) fn next_retired_task_cleanup(&self) -> Option<super::RetiredTaskCleanup> {
        (FIRST_DYNAMIC_TASK_SLOT..MAX_TASK).find_map(|slot| {
            if !self.retired[slot] {
                return None;
            }
            self.retirement_cleanup[slot]
        })
    }

    pub(super) fn complete_retired_task_cleanup(
        &mut self,
        cleanup: super::RetiredTaskCleanup,
    ) -> bool {
        let Some(slot) = (FIRST_DYNAMIC_TASK_SLOT..MAX_TASK)
            .find(|slot| self.retired[*slot] && self.retirement_cleanup[*slot] == Some(cleanup))
        else {
            return false;
        };
        self.retirement_cleanup[slot] = None;
        true
    }

    pub(super) fn save_current_simd_state(&mut self) {
        let mut phase_marker = crate::arch::clock::monotonic_nanos();
        let slot = self.current_task_slot();
        unsafe {
            save_state(&mut self.simd_states[slot]);
        }
        self.mark_phase(SchedulerPhase::ArchRestore, &mut phase_marker);
    }

    pub(super) fn restore_current_simd_state(&mut self) {
        let mut phase_marker = crate::arch::clock::monotonic_nanos();
        let slot = self.current_task_slot();
        unsafe {
            restore_state(&self.simd_states[slot]);
        }
        self.mark_phase(SchedulerPhase::ArchRestore, &mut phase_marker);
    }

    pub(super) fn capture_current_syscall_user_simd(&mut self) -> Option<u64> {
        let slot = self.current_task_slot();
        let task_id = self.starts[slot]?.id;
        if self.syscall_user_simd_active[slot] {
            return None;
        }
        unsafe {
            save_state(&mut self.syscall_user_simd_states[slot]);
        }
        self.syscall_user_simd_active[slot] = true;
        Some(task_id)
    }

    pub(super) fn restore_current_syscall_user_simd(&mut self, task_id: u64) -> bool {
        let slot = self.current_task_slot();
        if self.starts[slot].is_none_or(|start| start.id != task_id)
            || !self.syscall_user_simd_active[slot]
        {
            return false;
        }
        unsafe {
            restore_state(&self.syscall_user_simd_states[slot]);
        }
        self.syscall_user_simd_active[slot] = false;
        true
    }

    /// Re-derives and republishes the lock-free identity record for `slot`.
    ///
    /// Every site that binds, rebinds, or retires a slot must call this. A
    /// missed call would leave a stale record rather than an absent one, so
    /// `divergent_published_identity` re-derives the whole table under the
    /// lock and names the first slot that disagrees instead of leaving the
    /// completeness of these call sites to inspection.
    pub(super) fn publish_slot_identity(&self, slot: usize) {
        match self.slot_identity(slot) {
            Some(identity) => current_identity::publish(slot, identity),
            None => current_identity::clear(slot),
        }
    }

    fn slot_identity(&self, slot: usize) -> Option<TaskIdentity> {
        let context = self.contexts.get(slot).copied().flatten()?;
        Some(TaskIdentity {
            task_id: self
                .starts
                .get(slot)
                .copied()
                .flatten()
                .map(|start| start.id),
            user_mode: context.user_mode,
            abi: context.user_abi,
            process_handle: context.process_handle,
            console_session: context.console_session,
        })
    }

    /// Returns the first slot whose published identity disagrees with the
    /// scheduler's own tables, or `None` when the publication is complete.
    pub(super) fn divergent_published_identity(&self) -> Option<usize> {
        (0..MAX_TASK)
            .find(|&slot| !current_identity::matches_authority(slot, self.slot_identity(slot)))
    }

    pub(super) fn current_user_process_binding(
        &self,
    ) -> Option<(u64, UserAbi, ProcessHandle, ConsoleSessionHandle)> {
        let slot = self.current_task_slot();
        let context = self.contexts[slot]?;
        if !context.user_mode {
            return None;
        }

        let thread_id = self.starts[slot].map(|start| start.id)?;
        let abi = context.user_abi?;
        let process_handle = context.process_handle?;
        Some((thread_id, abi, process_handle, context.console_session))
    }

    /// Snapshot the immutable identity needed by scheduler-owned wait keys.
    ///
    /// This must remain scheduler-local: futex admission runs before a task
    /// has installed any timeout recovery authority and therefore cannot spin
    /// behind unrelated same-process state mutation.
    pub(super) fn current_user_wait_binding(&self) -> Option<(u64, UserAbi, u64)> {
        let slot = self.current_task_slot();
        let context = self.contexts[slot]?;
        if !context.user_mode {
            return None;
        }
        let thread_id = self.starts[slot].map(|start| start.id)?;
        Some((thread_id, context.user_abi?, context.address_space_root))
    }

    pub(super) fn current_linux_thread_state(&self) -> Option<LinuxThreadState> {
        let slot = self.current_task_slot();
        let context = self.contexts[slot]?;
        if !context.user_mode || context.user_abi != Some(UserAbi::Linux) {
            return None;
        }
        let state = self.linux_thread_state(slot);
        // Lower the hint whenever the authority says nothing is pending. This
        // runs under the scheduler lock, so it cannot race the raise site.
        current_identity::sync_signal_pending(
            slot,
            state.is_some_and(|state| {
                state.pending_signals != 0 || state.pending_sigchld_events != 0
            }),
        );
        state
    }

    pub(super) fn current_user_stack_state(&self) -> Option<UserStackState> {
        let slot = self.current_task_slot();
        let context = self.contexts[slot]?;
        if !context.user_mode {
            return None;
        }
        context.user_stack
    }

    pub(super) fn current_user_id(&self) -> Option<u64> {
        let slot = self.current_task_slot();
        let context = self.contexts[slot]?;
        if !context.user_mode {
            return None;
        }

        self.starts[slot].map(|start| start.id)
    }

    pub(super) fn current_user_log_ids(&self) -> Option<(u64, u64)> {
        let slot = self.current_task_slot();
        let context = self.contexts[slot]?;
        if !context.user_mode {
            return None;
        }
        let process_id = context.process_id?;
        let thread_id = self.starts[slot].map(|start| start.id)?;
        Some((process_id, thread_id))
    }

    pub(super) fn user_log_ids_for_task(&self, task_id: u64) -> Option<(u64, u64)> {
        let slot = self.find_task_slot(task_id)?;
        let context = self.contexts[slot]?;
        if !context.user_mode {
            return None;
        }
        let process_id = context.process_id?;
        let thread_id = self.starts[slot].map(|start| start.id)?;
        Some((process_id, thread_id))
    }

    pub(super) fn current_task_id(&self) -> Option<u64> {
        self.starts[self.current_task_slot()].map(|start| start.id)
    }

    pub(super) fn current_console_session(&self) -> Option<ConsoleSessionHandle> {
        self.contexts[self.current_task_slot()].map(|context| context.console_session)
    }

    pub(super) fn user_process_handles_snapshot(
        &self,
    ) -> ([Option<ProcessHandle>; MAX_TASK], usize) {
        let mut seen = [None; MAX_TASK];
        let mut seen_count = 0usize;
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            if self.retired[slot] {
                continue;
            }

            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.user_mode {
                continue;
            }

            let Some(process_handle) = context.process_handle else {
                continue;
            };
            if seen[..seen_count].contains(&Some(process_handle)) {
                continue;
            }

            seen[seen_count] = Some(process_handle);
            seen_count += 1;
        }
        (seen, seen_count)
    }

    pub(super) fn exec_current_user_process(
        &mut self,
        new_root: u64,
        bootstrap: &mut UserTaskBootstrap,
    ) -> Option<ProcessHandle> {
        let slot = self.current_task_slot();
        let current_context = self.contexts[slot]?;
        if !current_context.user_mode {
            return None;
        }

        let process_handle = current_context.process_handle?;
        if !self.exec_slot_admission_valid(slot, process_handle) {
            return None;
        }
        let preserved_affinity = self.exec_affinity_snapshot(slot);
        let process_id = current_context.process_id?;
        let (sibling_slots, sibling_count) =
            self.collect_live_process_sibling_slots(slot, process_handle);
        for sibling_slot in sibling_slots.iter().take(sibling_count) {
            self.retire_exec_sibling_slot(*sibling_slot);
        }
        let preserved_signal_mask = self
            .linux_thread_state(slot)
            .map(|state| state.signal_mask)
            .unwrap_or(0);
        if let Some(thread_state) = bootstrap.linux_thread_state.as_mut() {
            thread_state.signal_mask = preserved_signal_mask;
            thread_state.pending_signals = 0;
            thread_state.pending_sigchld_events = 0;
        }
        let new_fs_base = bootstrap
            .linux_thread_state
            .map(|state| state.fs_base)
            .unwrap_or(0);

        {
            let context = self.contexts[slot].as_mut()?;
            context.address_space_root = new_root;
            context.user_abi = Some(bootstrap.abi);
            context.console_session = bootstrap.console_session;
            context.user_stack = bootstrap.user_stack;
            context.blocked = false;
            context.blocked_since_ticks = 0;
            // Self-exec keeps the only Running owner. Publishing Ready here
            // would let another CPU validate or dispatch this live stack.
            context.ready = false;
            context.ready_since_ticks = 0;
        }

        self.exec_target_quiesced[slot] = false;
        self.simd_states[slot] = SimdState::new();
        self.syscall_user_simd_active[slot] = false;
        self.starts[slot] = Some(TaskStart {
            entry: super::noop_task_entry,
            id: process_id,
        });
        self.publish_slot_identity(slot);
        self.install_linux_thread_state(
            slot,
            bootstrap.linux_thread_state.map(|_| process_id),
            bootstrap.linux_thread_state,
        );

        crate::memory::paging::load_address_space_phys(PhysAddr::new(new_root));
        FsBase::write(VirtAddr::new(new_fs_base));
        self.assert_exec_affinity_preserved(slot, preserved_affinity);
        Some(process_handle)
    }
    pub(super) fn exec_user_process_by_pid(
        &mut self,
        process_id: u64,
        thread_id: u64,
        new_root: u64,
        bootstrap: &mut UserTaskBootstrap,
    ) -> Option<ProcessHandle> {
        let slot = self.find_linux_thread_slot(process_id, thread_id)?;
        let current_context = self.contexts[slot]?;
        let process_handle = current_context.process_handle?;
        if !self.exec_slot_admission_valid(slot, process_handle) {
            return None;
        }
        let preserved_affinity = self.exec_affinity_snapshot(slot);
        self.assert_exec_target_replacement_safe(slot);
        let (sibling_slots, sibling_count) =
            self.collect_live_process_sibling_slots(slot, process_handle);
        for sibling_slot in sibling_slots.iter().take(sibling_count) {
            self.retire_exec_sibling_slot(*sibling_slot);
        }
        let preserved_signal_mask = self
            .linux_thread_state(slot)
            .map(|state| state.signal_mask)
            .unwrap_or(0);
        if let Some(thread_state) = bootstrap.linux_thread_state.as_mut() {
            thread_state.signal_mask = preserved_signal_mask;
            thread_state.pending_signals = 0;
            thread_state.pending_sigchld_events = 0;
        }
        let new_fs_base = bootstrap
            .linux_thread_state
            .map(|state| state.fs_base)
            .unwrap_or(0);
        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let rflags = initial_task_rflags().bits();
        let saved_rsp = self.init_user_task_context(slot, &bootstrap, user_cs, user_ss, rflags);

        {
            let context = self.contexts[slot].as_mut()?;
            context.saved_rsp = saved_rsp;
            context.address_space_root = new_root;
            context.user_abi = Some(bootstrap.abi);
            context.console_session = bootstrap.console_session;
            context.user_stack = bootstrap.user_stack;
            context.blocked = false;
            context.blocked_since_ticks = 0;
            context.ready = true;
            context.ready_since_ticks = crate::arch::rtc::ticks();
        }
        self.exec_target_quiesced[slot] = false;
        self.simd_states[slot] = SimdState::new();
        self.syscall_user_simd_active[slot] = false;
        self.starts[slot] = Some(TaskStart {
            entry: super::noop_task_entry,
            id: process_id,
        });
        self.publish_slot_identity(slot);
        self.install_linux_thread_state(
            slot,
            bootstrap.linux_thread_state.map(|_| process_id),
            bootstrap.linux_thread_state,
        );

        if slot == self.current_task_slot() {
            crate::memory::paging::load_address_space_phys(PhysAddr::new(new_root));
            FsBase::write(VirtAddr::new(new_fs_base));
        }
        self.assert_exec_affinity_preserved(slot, preserved_affinity);
        Some(process_handle)
    }

    fn exec_slot_admission_valid(&self, slot: usize, process_handle: ProcessHandle) -> bool {
        !self.retired[slot]
            && self.deferred_retire_reasons[slot].is_none()
            && self.retirement_cleanup[slot].is_none()
            && self.retirement_side_effects[slot].is_none()
            && self.retire_reasons[slot].is_none()
            && self.contexts[slot].is_some_and(|context| {
                context.user_mode && context.process_handle == Some(process_handle)
            })
    }
    pub(super) fn linux_thread_snapshot_by_ids(
        &self,
        process_id: u64,
        thread_id: u64,
    ) -> Option<super::LinuxThreadSnapshot> {
        let slot = self.find_linux_thread_slot(process_id, thread_id)?;
        let context = self.contexts[slot]?;
        Some(super::LinuxThreadSnapshot {
            process_id,
            thread_id,
            console_session: context.console_session,
            user_stack: context.user_stack,
            thread_state: self.linux_thread_state(slot)?,
        })
    }

    /// Collect only siblings that still have live scheduler ownership.
    ///
    /// Retired slots intentionally retain their context and process identity
    /// until out-of-lock side effects and runtime cleanup acknowledge the
    /// exact task generation. Re-collecting such a slot for process exit or
    /// exec would publish a second retirement owner and is a fatal lifecycle
    /// violation, not an idempotent operation.
    fn collect_live_process_sibling_slots(
        &self,
        current_slot: usize,
        process_handle: ProcessHandle,
    ) -> ([usize; MAX_TASK], usize) {
        let mut slots = [0usize; MAX_TASK];
        let mut count = 0usize;
        for slot in 1..MAX_TASK {
            if slot == current_slot || self.retired[slot] {
                continue;
            }

            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.user_mode || context.process_handle != Some(process_handle) {
                continue;
            }

            slots[count] = slot;
            count += 1;
        }

        (slots, count)
    }

    pub(super) fn retire_current_user_task_due_to_fault(
        &mut self,
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rip: u64,
        _rsp: u64,
    ) -> UserFaultDisposition {
        let slot = self.current_task_slot();
        let Some(context) = self.contexts[slot] else {
            return UserFaultDisposition::Unhandled;
        };
        if !context.user_mode {
            return UserFaultDisposition::Unhandled;
        }

        if self.retired[slot] {
            return UserFaultDisposition::Retired;
        }

        self.retire_slot(
            slot,
            TaskRetireReason::UserFault {
                vector,
                error_code,
                cr2,
                rip,
            },
        );
        UserFaultDisposition::Retired
    }

    pub(super) fn is_user_task_alive(&self, task_id: u64) -> bool {
        let Some(slot) = self.find_user_task_slot(task_id) else {
            return false;
        };

        self.contexts[slot].is_some() && !self.retired[slot]
    }

    pub(super) fn terminate_user_task(
        &mut self,
        task_id: u64,
        requested_by_pid: Option<u64>,
    ) -> bool {
        let Some(slot) = self.find_user_task_slot(task_id) else {
            return false;
        };
        if self.retired[slot] {
            return false;
        }
        if let Some((process_handle, process_id)) =
            self.contexts[slot].and_then(|context| context.process_handle.zip(context.process_id))
            && self.is_last_live_user_task_for_process(slot, process_handle)
            && process_table::mark_process_exiting(process_id).is_none()
        {
            return false;
        }

        self.retire_slot(slot, TaskRetireReason::Terminated { requested_by_pid });
        true
    }

    /// Retire every thread of one user process.  Privileged lifecycle brokers
    /// use this only after marking process authority unavailable and recording
    /// its exit; task-only retirement would otherwise leave sibling threads
    /// and process-owned endpoint state live.
    pub(super) fn terminate_user_process(
        &mut self,
        process_id: u64,
        requested_by_pid: Option<u64>,
    ) -> bool {
        let Some(leader_slot) = self.find_user_task_slot(process_id) else {
            return false;
        };
        let Some(process_handle) =
            self.contexts[leader_slot].and_then(|context| context.process_handle)
        else {
            return false;
        };
        if self.retired[leader_slot] {
            return false;
        }
        // Exit publication precedes task retirement. This closes thread
        // attachment and process-scoped authority admission before any
        // sibling can disappear or terminal cleanup can begin.
        if process_table::mark_process_exiting(process_id).is_none() {
            return false;
        }
        let (sibling_slots, sibling_count) =
            self.collect_live_process_sibling_slots(leader_slot, process_handle);
        for slot in sibling_slots.into_iter().take(sibling_count) {
            if !self.retired[slot] {
                self.retire_slot(slot, TaskRetireReason::Terminated { requested_by_pid });
            }
        }
        self.retire_slot(
            leader_slot,
            TaskRetireReason::Terminated { requested_by_pid },
        );
        true
    }

    /// Arms a race-free block on the current task. Pair with
    /// `commit_block_current_task`. Between the two calls the caller must
    /// re-check the wakeup condition; if a wake fires in that window the
    /// commit returns `false` and the caller stays runnable instead of
    /// sleeping with a lost wakeup.
    pub(super) fn arm_block_current_task(&mut self) -> bool {
        let slot = self.current_task_slot();
        if slot == ROOT_TASK_SLOT || self.retired[slot] || self.start_suspended[slot] {
            return false;
        }
        let Some(context) = self.contexts[slot].as_mut() else {
            return false;
        };
        if context.blocked {
            return false;
        }
        // A dispatched context is not on the ready queue. A prior raced wake
        // may have marked this still-running task ready; the caller has
        // already rechecked that condition before a new arm, so consume that
        // stale runnable mark before publishing the next wait epoch.
        context.ready = false;
        context.ready_since_ticks = 0;
        context.wake_armed = true;
        true
    }

    /// Cancels a previously armed block when the caller re-checked its wait
    /// condition and found work available. The task is still executing, so this
    /// must not mark it blocked.
    pub(super) fn cancel_block_current_task(&mut self) -> bool {
        let slot = self.current_task_slot();
        if slot == ROOT_TASK_SLOT || self.retired[slot] || self.start_suspended[slot] {
            return false;
        }
        let Some(context) = self.contexts[slot].as_mut() else {
            return false;
        };
        if context.blocked || !context.wake_armed {
            return false;
        }
        context.ready = false;
        context.ready_since_ticks = 0;
        context.wake_armed = false;
        true
    }

    /// Commits a previously armed block. Returns `Some(true)` if the task was
    /// blocked, `Some(false)` if a wake raced us (wake_armed cleared by
    /// `wake_task`) and we should re-check the condition without sleeping,
    /// `None` on invalid context.
    pub(super) fn commit_block_current_task(&mut self) -> Option<bool> {
        let slot = self.current_task_slot();
        if slot == ROOT_TASK_SLOT || self.retired[slot] || self.start_suspended[slot] {
            return None;
        }
        let context = self.contexts[slot].as_mut()?;
        if context.blocked {
            return None;
        }
        if !context.wake_armed {
            // `wake_task` makes the current context ready to record the race,
            // but it never stopped executing. Consume that transient queue
            // mark before returning to the caller's condition loop.
            context.ready = false;
            context.ready_since_ticks = 0;
            return Some(false);
        }
        context.wake_armed = false;
        context.blocked = true;
        context.ready = false;
        context.ready_since_ticks = 0;
        context.blocked_since_ticks = crate::arch::rtc::ticks();
        Some(true)
    }

    pub(super) fn wake_user_task(&mut self, task_id: u64) -> bool {
        let Some(slot) = self.find_user_task_slot(task_id) else {
            return false;
        };
        self.wake_task_slot(slot)
    }

    pub(super) fn wake_task(&mut self, task_id: u64) -> bool {
        let Some(slot) = self.find_task_slot(task_id) else {
            return false;
        };
        self.wake_task_slot(slot)
    }

    fn wake_task_slot(&mut self, slot: usize) -> bool {
        if self.retired[slot] || self.start_suspended[slot] {
            return false;
        }

        let blocked_since_ticks = self
            .contexts
            .get(slot)
            .and_then(|context| *context)
            .map(|context| context.blocked_since_ticks)
            .unwrap_or(0);
        let task_id = self.starts[slot].map(|start| start.id);
        let process_id = self.contexts[slot].and_then(|context| context.process_id);
        let (saved_rsp, user_mode, was_ready, was_blocked, wake_was_armed) =
            match self.contexts[slot] {
                Some(context) => (
                    context.saved_rsp,
                    context.user_mode,
                    context.ready,
                    context.blocked,
                    context.wake_armed,
                ),
                None => return false,
            };
        let execution_owner = if slot == self.current_task_slot() {
            // Unit schedulers do not publish CPU-local ownership, and at
            // runtime this is the scheduler's exact current task.
            Some(super::cpu_local::TaskExecutionOwner::Current(
                nucleus_core::util::lockdep::current_cpu_index(),
            ))
        } else {
            super::cpu_local::task_execution_owner(slot)
        };
        if matches!(
            execution_owner,
            Some(super::cpu_local::TaskExecutionOwner::Current(_))
        ) {
            // A CPU dispatch consumed this task's published interrupt frame.
            // `saved_rsp` therefore names ordinary, reusable stack storage
            // until the next schedule trap publishes a new frame. This also
            // covers the post-commit/pre-trap window: `blocked` is already
            // true there, but the task still owns and executes on this CPU.
            // A wake is only a token transition; clear arm/block and let the
            // inevitable trap publish a fresh frame. Validating a consumed
            // local or remote frame can quarantine a healthy task after
            // syscall stack writes have reused those bytes.
            let context = self.contexts[slot]
                .as_mut()
                .expect("current scheduler slot lost its context during wake");
            context.wake_armed = false;
            context.blocked = false;
            context.ready = false;
            context.ready_since_ticks = 0;
            context.blocked_since_ticks = 0;
            return true;
        }
        if matches!(
            execution_owner,
            Some(super::cpu_local::TaskExecutionOwner::Transition(_))
        ) {
            // The outgoing frame is already published, but the old kernel
            // stack remains owned by assembly. Preserve the wake as runnable;
            // candidate selection rejects the slot until transition release.
            // Treating this as Current would clear `ready`, and transition
            // completion would then leave the task with no owner and no queue.
            let context = self.contexts[slot]
                .as_mut()
                .expect("transitioning scheduler slot lost its context during wake");
            context.wake_armed = false;
            context.blocked = false;
            context.ready = true;
            context.ready_since_ticks = crate::arch::rtc::ticks();
            context.blocked_since_ticks = 0;
            #[cfg(not(test))]
            if let Some(cpu) = runqueue::owner(slot).cpu {
                super::irq::request_target_reschedule(cpu);
            }
            #[cfg(test)]
            super::request_deferred_reschedule();
            return true;
        }
        let already_runnable = was_ready && !was_blocked && !wake_was_armed;
        let invalid_reason = self
            .validate_saved_context(slot, user_mode, saved_rsp)
            .err();

        // Compute the sleeper bonus floor *before* mutably borrowing the
        // context. Scope the floor to the woken task's class so the bonus is
        // measured against its actual peers — pooling System and User into a
        // single min would let a long-sleeping User task come back with a
        // vruntime well below the System min and starve System peers via the
        // bonus mechanism alone. Mirrors per-cfs_rq min_vruntime tracking in
        // Linux when SCHED_DEADLINE / cgroups create logically separate
        // runqueues.
        let now_ticks = crate::arch::rtc::ticks();
        let woken_class = self.slot_class(slot);
        let class_min_vruntime = match woken_class {
            Some(class) => self.min_ready_vruntime_in_class(class),
            None => self.min_ready_vruntime(),
        };
        let wake_floor = class_min_vruntime.saturating_sub(SLEEPER_LATENCY_BONUS_NS);

        let (waker_vruntime, waker_ready) = {
            let Some(context) = self.contexts[slot].as_mut() else {
                return false;
            };
            // Always clear the arm flag so a paired commit_block_current_task
            // observes that a wake raced before the caller actually slept.
            context.wake_armed = false;
            context.blocked = false;
            context.ready = invalid_reason.is_none() || already_runnable;
            context.blocked_since_ticks = 0;
            if already_runnable {
                // Leave ready_since_ticks and vruntime unchanged for tasks
                // that were already runnable; wake is just a scheduling hint
                // in this case, not a transition out of sleep.
            } else if invalid_reason.is_none() {
                context.ready_since_ticks = now_ticks;
            } else {
                context.ready_since_ticks = 0;
            }
            if invalid_reason.is_none() && !already_runnable {
                // Bound vruntime to the runqueue floor: prevents
                // long-blocked tasks from running unimpeded after wake
                // (which would just shift starvation onto the rest of the
                // system), while still granting them latency-sensitive
                // priority via SLEEPER_LATENCY_BONUS_NS.
                context.vruntime_ns = context.vruntime_ns.max(wake_floor);
            }
            (
                context.vruntime_ns,
                context.ready && invalid_reason.is_none() && !self.job_stopped[slot],
            )
        };

        if invalid_reason.is_none() && blocked_since_ticks != 0 {
            self.maybe_log_blocked_wait(slot, task_id, process_id, blocked_since_ticks, now_ticks);
        }

        if let Some(reason) = invalid_reason {
            if slot == ROOT_TASK_SLOT {
                panic!("scheduler root kernel context is corrupted: {}", reason);
            }
            self.retire_slot_due_to_invalid_context(slot, saved_rsp, reason);
            return true;
        }

        #[cfg(not(test))]
        let wake_owner_cpu = if waker_ready && self.publish_runqueue_wake(slot) {
            runqueue::owner(slot).cpu
        } else {
            None
        };

        // Wake-preempt. Without this signal the wake just makes the task
        // ready; the next scheduling decision then waits for either the next
        // timer tick (~1ms — recoverable) or the current task to voluntarily
        // yield (could be tens to hundreds of ms in a user-task syscall that
        // doesn't cond_resched). Linux's `check_preempt_wakeup` uses a
        // wakeup_granularity threshold; Mach signals
        // `AST_PREEMPT | AST_URGENT` on cross-band wake. We use the existing
        // DEFERRED_RESCHEDULE flag for explicit in-kernel cond_resched points.
        // An ordinary syscall return clears it and relies on the next PIT,
        // avoiding a nested voluntary-yield frame while keeping wake latency
        // bounded by one scheduler tick.
        let current_slot = self.current_task_slot();
        if waker_ready && slot != current_slot {
            let current_class = self.slot_class(current_slot);
            let should_preempt = match (woken_class, current_class) {
                // Cross-class: a higher-priority band has work, preempt.
                (Some(wc), Some(cc)) if wc < cc => true,
                // Same class: only preempt if the wake brings the waker
                // distinctly ahead in vruntime — the CFS wake-preempt rule.
                (Some(wc), Some(cc)) if wc == cc => {
                    let current_v = self.contexts[current_slot]
                        .map(|ctx| ctx.vruntime_ns)
                        .unwrap_or(0);
                    current_v.saturating_sub(waker_vruntime) >= SCHED_MIN_GRANULARITY_NS
                }
                _ => false,
            };
            if should_preempt {
                #[cfg(not(test))]
                if let Some(cpu) = wake_owner_cpu {
                    super::irq::request_target_reschedule(cpu);
                }
                #[cfg(test)]
                super::request_deferred_reschedule();
            }
        }

        true
    }

    pub(super) fn exit_current_task(&mut self) {
        crate::debug::trace_loc!();
        let slot = self.current_task_slot();
        if slot == ROOT_TASK_SLOT {
            panic!("scheduler root kernel task cannot exit");
        }

        self.mark_slot_ready(
            slot,
            self.contexts[slot].map(|ctx| ctx.saved_rsp).unwrap_or(0),
            false,
        );
        if self.retire_reasons[slot].is_none() {
            self.retire_slot(slot, TaskRetireReason::Exited);
        } else {
            self.retired[slot] = true;
            self.pending_reap = true;
        }
    }

    pub(super) fn exit_current_process(&mut self) {
        let current_slot = self.current_task_slot();
        let Some(process_handle) =
            self.contexts[current_slot].and_then(|context| context.process_handle)
        else {
            self.exit_current_task();
            return;
        };
        let (sibling_slots, sibling_count) =
            self.collect_live_process_sibling_slots(current_slot, process_handle);
        let (_, current_task, current_pid) = self.describe_current_task();
        let logical_cpu = nucleus_core::util::lockdep::current_cpu_index();
        for slot in sibling_slots.iter().copied().take(sibling_count) {
            if let Some(context) = self.contexts[slot] {
                let state_flags = (u64::from(context.ready)
                    * nucleus_core::util::lockdep::SchedulerObservation::STATE_READY)
                    | (u64::from(context.blocked)
                        * nucleus_core::util::lockdep::SchedulerObservation::STATE_BLOCKED)
                    | (u64::from(context.wake_armed)
                        * nucleus_core::util::lockdep::SchedulerObservation::STATE_WAKE_ARMED)
                    | (u64::from(self.start_suspended[slot])
                        * nucleus_core::util::lockdep::SchedulerObservation::STATE_SUSPENDED)
                    | (u64::from(self.job_stopped[slot])
                        * nucleus_core::util::lockdep::SchedulerObservation::STATE_STOPPED)
                    | (u64::from(self.retired[slot])
                        * nucleus_core::util::lockdep::SchedulerObservation::STATE_RETIRED);
                nucleus_core::util::lockdep::record_scheduler_observation(
                    logical_cpu,
                    nucleus_core::util::lockdep::SchedulerObservation {
                        kind: nucleus_core::util::lockdep::SchedulerObservationKind::ExitSnapshot,
                        subject_task: self.starts[slot].map(|start| start.id).unwrap_or(0),
                        subject_pid: context.process_id.unwrap_or(0),
                        subject_slot: slot,
                        peer_task: current_task,
                        peer_pid: current_pid,
                        peer_slot: current_slot,
                        elapsed_ms: 0,
                        state_flags,
                        ready_since_ticks: context.ready_since_ticks,
                        blocked_since_ticks: context.blocked_since_ticks,
                    },
                );
            }
        }
        for slot in sibling_slots.into_iter().take(sibling_count) {
            self.retire_slot(slot, TaskRetireReason::Exited);
        }
        self.exit_current_task();
    }

    fn context_returns_to_user(&self, context: TaskContext) -> bool {
        Self::saved_context_returns_to_user(context.saved_rsp)
    }

    fn saved_context_returns_to_user(saved_rsp: usize) -> bool {
        let Some(saved) = Self::saved_context_ref(saved_rsp) else {
            return false;
        };
        saved.cs == crate::arch::gdt::user_code_selector().0 as u64
    }

    fn find_user_task_slot(&self, task_id: u64) -> Option<usize> {
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.user_mode {
                continue;
            }
            if self.starts[slot].map(|start| start.id) == Some(task_id) {
                return Some(slot);
            }
        }

        None
    }

    /// Resolves donation entry `index`'s task identity to its live slot.
    ///
    /// The hint is only an accelerator: it is accepted solely when the named
    /// slot still carries that exact live identity, and any miss falls back to
    /// the authoritative scan and repairs the hint.
    fn resolve_donation_slot(&self, index: usize, task_id: u64) -> Option<usize> {
        // ORDERING: Relaxed is exact. The hint is an accelerator validated
        // against live identity below, never ownership or publication authority,
        // and every access already holds the scheduler owner.
        let hint = usize::from(self.donation_donor_slot_hints[index].load(Ordering::Relaxed));
        if hint < MAX_TASK
            && !self.retired[hint]
            && self.contexts[hint].is_some()
            && self.starts[hint].is_some_and(|start| start.id == task_id)
        {
            return Some(hint);
        }
        let slot = self.find_task_slot(task_id)?;
        // ORDERING: see the hint load above; repair is equally non-authoritative.
        self.donation_donor_slot_hints[index].store(
            u8::try_from(slot).expect("scheduler task slot exceeds donation hint capacity"),
            Ordering::Relaxed,
        );
        Some(slot)
    }

    fn find_task_slot(&self, task_id: u64) -> Option<usize> {
        for slot in 0..MAX_TASK {
            if self.retired[slot] || self.contexts[slot].is_none() {
                continue;
            }
            if self.starts[slot].map(|start| start.id) == Some(task_id) {
                return Some(slot);
            }
        }

        None
    }
}

#[cfg(rustos_log_sched_debug)]
fn saved_context_rip(saved_rsp: usize) -> Option<u64> {
    if saved_rsp == 0 {
        return None;
    }
    let context = saved_rsp as *const SavedContext;
    Some(unsafe { (*context).rip })
}

fn stack_range_contains(base: u64, top: u64, start: usize, end: usize) -> bool {
    if base == 0 || top <= base || end < start {
        return false;
    }
    start >= base as usize && end <= top as usize
}

fn should_validate_published_ready_frame(
    slot: usize,
    current_slot: usize,
    retired: bool,
    start_suspended: bool,
    ready: bool,
    blocked: bool,
) -> bool {
    slot != current_slot && !retired && !start_suspended && ready && !blocked
}

fn published_frame_is_stable(slot: usize, current_slot: usize, running: bool) -> bool {
    slot != current_slot && !running
}

#[cfg(test)]
fn live_task_state_is_partitioned(
    slot: usize,
    current_slot: usize,
    retired: bool,
    start_suspended: bool,
    job_stopped: bool,
    ready: bool,
    blocked: bool,
) -> bool {
    slot == current_slot || retired || start_suspended || job_stopped || ready || blocked
}

/// Every hardware or `syscall` transition stack top is a SysV x86_64 call
/// boundary. Heap-backed task stacks are byte buffers and therefore do not
/// carry a stronger allocator alignment guarantee of their own.
const fn align_kernel_stack_top(raw_top: usize) -> usize {
    raw_top & !0xF
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use super::{
        ConsoleSessionHandle, IpcDonationTarget, MAX_CONSECUTIVE_SYSTEM_DISPATCHES, MAX_TASK,
        MIN_LOAD_WEIGHT, NICE_0_LOAD, SCHED_CPU_LOCALITY_LAG_NS, SYSTEM_CLASS_WEIGHT_FLAG,
        SchedClass, Scheduler, SchedulerDispatch, TaskContext, TaskStart, align_kernel_stack_top,
    };
    use crate::memory::paging::ProcessAddressSpace;
    use crate::multitask::{UserTaskBootstrap, noop_task_entry, process_table};
    use crate::user::abi::UserAbi;
    use crate::user::linux::LinuxThreadState;
    use crate::user::process_state::UserProcessState;
    use kernel_ipc_runtime::api::{EndpointResponseTake, IpcError};

    static TEST_SCHEDULER_TEMPLATE: Scheduler = Scheduler::new();

    #[test]
    fn kernel_stack_top_is_aligned_for_sysv_rust_calls() {
        for low_bits in 0..16 {
            let top = align_kernel_stack_top(0x10_000 + low_bits);
            assert_eq!(top & 0xF, 0);
            assert!(top <= 0x10_000 + low_bits);
            assert!((0x10_000 + low_bits) - top < 16);
        }
    }

    #[test]
    fn architectural_restore_is_required_exactly_for_a_task_switch() {
        let same = SchedulerDispatch::new(0x1000, 119, 7, 7);
        assert!(!same.requires_architectural_restore());

        let switched = SchedulerDispatch::new(0x2000, 119, 7, 9);
        assert!(switched.requires_architectural_restore());
    }

    #[test]
    fn syscall_user_simd_snapshot_is_disjoint_from_scheduler_continuation() {
        let continuation_start = core::mem::offset_of!(Scheduler, simd_states);
        let continuation_end =
            continuation_start + core::mem::size_of::<[super::SimdState; MAX_TASK]>();
        let snapshot_start = core::mem::offset_of!(Scheduler, syscall_user_simd_states);
        let snapshot_end = snapshot_start + core::mem::size_of::<[super::SimdState; MAX_TASK]>();
        let active_start = core::mem::offset_of!(Scheduler, syscall_user_simd_active);
        let active_end = active_start + core::mem::size_of::<[bool; MAX_TASK]>();

        assert!(continuation_end <= snapshot_start || snapshot_end <= continuation_start);
        assert!(snapshot_end <= active_start || active_end <= snapshot_start);
    }

    pub(super) fn boxed_scheduler() -> Box<Scheduler> {
        let mut scheduler = Box::<Scheduler>::new_uninit();
        unsafe {
            // The const template owns no heap allocation: every Vec-bearing
            // field is `None`. Copy it directly into the heap allocation so
            // debug test threads never materialize the large SIMD arrays on
            // their small harness stack.
            core::ptr::copy_nonoverlapping(
                core::ptr::addr_of!(TEST_SCHEDULER_TEMPLATE),
                scheduler.as_mut_ptr(),
                1,
            );
            scheduler.assume_init()
        }
    }

    pub(super) fn test_user_context(handle: process_table::ProcessHandle) -> TaskContext {
        TaskContext {
            saved_rsp: 0,
            ready: true,
            ready_since_ticks: 0,
            blocked: false,
            blocked_since_ticks: 0,
            wake_armed: false,
            weight: NICE_0_LOAD,
            vruntime_ns: 0,
            exec_start_ticks: 0,
            address_space_root: 0,
            kernel_stack_base: 0,
            kernel_stack_top: 0,
            alternate_kernel_stack_base: 0,
            alternate_kernel_stack_top: 0,
            user_mode: true,
            user_abi: Some(UserAbi::Linux),
            console_session: ConsoleSessionHandle::SYSTEM,
            process_handle: Some(handle),
            process_id: process_table::with_process_state(handle, |pid, _| pid),
            user_stack: None,
            windows_thread_state: None,
        }
    }

    pub(super) fn test_process(id: u64) -> process_table::ProcessHandle {
        process_table::create_process(
            id,
            UserProcessState::new(
                ProcessAddressSpace::empty_for_tests(),
                None,
                None,
                None,
                None,
                false,
                "/test.elf",
            ),
        )
        .expect("process handle")
    }

    #[test]
    fn ready_validation_accepts_only_immutable_published_frames() {
        use super::should_validate_published_ready_frame as should_validate;

        assert!(should_validate(2, 1, false, false, true, false));
        assert!(!should_validate(1, 1, false, false, true, false));
        assert!(!should_validate(2, 1, true, false, true, false));
        assert!(!should_validate(2, 1, false, true, true, false));
        assert!(!should_validate(2, 1, false, false, false, false));
        assert!(!should_validate(2, 1, false, false, true, true));
    }

    #[test]
    fn ready_scanner_never_reads_a_frame_owned_by_any_cpu() {
        use super::published_frame_is_stable as stable;

        assert!(stable(2, 1, false));
        assert!(!stable(1, 1, false));
        assert!(!stable(2, 1, true));
    }

    #[test]
    fn live_noncurrent_task_must_retain_one_scheduler_state_owner() {
        use super::live_task_state_is_partitioned as partitioned;

        assert!(partitioned(1, 1, false, false, false, false, false));
        assert!(partitioned(2, 1, false, false, false, true, false));
        assert!(partitioned(2, 1, false, false, false, false, true));
        assert!(partitioned(2, 1, true, false, false, false, false));
        assert!(partitioned(2, 1, false, true, false, false, false));
        assert!(partitioned(2, 1, false, false, true, false, false));
        assert!(!partitioned(2, 1, false, false, false, false, false));
    }

    #[test]
    fn collect_process_sibling_slots_returns_matching_user_slots_only() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let owner = test_process(1);
        let other = test_process(2);

        scheduler.contexts[1] = Some(test_user_context(owner));
        scheduler.contexts[2] = Some(test_user_context(owner));
        scheduler.contexts[3] = Some(test_user_context(other));
        scheduler.contexts[4] = Some(TaskContext {
            user_mode: false,
            process_handle: Some(owner),
            ..test_user_context(owner)
        });

        scheduler.retired[2] = true;
        scheduler.contexts[5] = Some(test_user_context(owner));

        let (slots, count) = scheduler.collect_live_process_sibling_slots(1, owner);
        assert_eq!(count, 1);
        assert_eq!(slots[0], 5);
        assert!(slots[1..MAX_TASK].iter().all(|slot| *slot == 0));
    }

    #[test]
    fn process_stop_is_scheduler_wide_and_sigcont_resumes_before_delivery() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let process = test_process(48);
        process_table::attach_task(process).expect("second thread");
        let leader = test_user_context(process);
        let worker = test_user_context(process);
        scheduler.contexts[1] = Some(leader);
        scheduler.contexts[2] = Some(worker);
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 48,
        });
        scheduler.starts[2] = Some(TaskStart {
            entry: noop_task_entry,
            id: 49,
        });
        scheduler.install_linux_thread_state(1, Some(48), Some(LinuxThreadState::default()));
        scheduler.install_linux_thread_state(2, Some(49), Some(LinuxThreadState::default()));
        scheduler.current_task = 1;

        assert!(scheduler.stop_current_linux_process(19));
        assert!(scheduler.job_stopped[1]);
        assert!(scheduler.job_stopped[2]);
        assert!(!scheduler.stop_current_linux_process(19));

        assert!(scheduler.queue_linux_signal(48, 48, rustos_user_abi::linux::SIGCONT));
        assert!(!scheduler.job_stopped[1]);
        assert!(!scheduler.job_stopped[2]);
        let pending = scheduler
            .linux_thread_state(1)
            .map(|state| state.pending_signals)
            .unwrap_or(0);
        assert_ne!(
            pending
                & crate::user::sysops::linux::linux_signal_bit(rustos_user_abi::linux::SIGCONT)
                    .unwrap(),
            0
        );

        process_table::note_process_exit_status(48, 0).expect("record exit");
        process_table::detach_task(process).expect("detach leader");
        process_table::detach_task(process).expect("detach worker");
        assert_eq!(process_table::reap_exited_processes(), 1);
    }

    #[test]
    fn unmasked_signal_revokes_a_pending_block_arm() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let process = test_process(52);
        let mut context = test_user_context(process);
        context.ready = false;
        scheduler.contexts[1] = Some(context);
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 52,
        });
        scheduler.install_linux_thread_state(1, Some(52), Some(LinuxThreadState::default()));
        scheduler.current_task = 1;

        assert!(scheduler.arm_block_current_task());
        assert!(scheduler.contexts[1].expect("armed context").wake_armed);
        assert!(scheduler.queue_linux_signal(52, 52, 15));
        let context = scheduler.contexts[1].expect("signalled context");
        assert!(!context.wake_armed);
        assert!(!context.blocked);
        assert!(!context.ready);
        assert_eq!(scheduler.commit_block_current_task(), Some(false));

        process_table::note_process_exit_status(52, 0).expect("record exit");
        process_table::detach_task(process).expect("detach thread");
        assert_eq!(process_table::reap_exited_processes(), 1);
    }

    #[test]
    fn process_sigchld_prefers_leader_and_retains_exact_coalesced_causes() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let process = test_process(50);
        process_table::attach_task(process).expect("second thread");
        let leader = test_user_context(process);
        let worker = test_user_context(process);
        scheduler.contexts[1] = Some(leader);
        scheduler.contexts[2] = Some(worker);
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 50,
        });
        scheduler.starts[2] = Some(TaskStart {
            entry: noop_task_entry,
            id: 51,
        });
        scheduler.install_linux_thread_state(1, Some(50), Some(LinuxThreadState::default()));
        scheduler.install_linux_thread_state(2, Some(51), Some(LinuxThreadState::default()));
        scheduler.current_task = 1;

        assert!(
            scheduler.queue_linux_process_sigchld(
                50,
                rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_STOP
            )
        );
        assert_eq!(
            scheduler
                .linux_thread_state(1)
                .map(|state| state.pending_sigchld_events),
            Some(rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_STOP)
        );

        scheduler.current_task = 2;
        scheduler.transfer_pending_process_sigchld(1);
        assert_eq!(
            scheduler
                .linux_thread_state(1)
                .map(|state| state.pending_sigchld_events),
            Some(0)
        );
        assert_eq!(
            scheduler
                .linux_thread_state(2)
                .map(|state| state.pending_sigchld_events),
            Some(rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_STOP)
        );

        scheduler.retired[1] = true;
        assert!(scheduler.queue_linux_process_sigchld(
            50,
            rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_CONTINUE
        ));
        assert_eq!(
            scheduler
                .linux_thread_state(2)
                .map(|state| state.pending_sigchld_events),
            Some(
                rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_STOP
                    | rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_CONTINUE
            )
        );

        process_table::note_process_exit_status(50, 0).expect("record exit");
        process_table::detach_task(process).expect("detach leader");
        process_table::detach_task(process).expect("detach worker");
        assert_eq!(process_table::reap_exited_processes(), 1);
    }

    #[test]
    fn terminate_user_process_retires_every_live_sibling() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let owner = test_process(41);
        let other = test_process(42);

        scheduler.contexts[1] = Some(test_user_context(owner));
        scheduler.contexts[2] = Some(test_user_context(owner));
        scheduler.contexts[3] = Some(test_user_context(other));
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 41,
        });
        scheduler.starts[2] = Some(TaskStart {
            entry: noop_task_entry,
            id: 43,
        });
        scheduler.starts[3] = Some(TaskStart {
            entry: noop_task_entry,
            id: 42,
        });

        assert!(scheduler.terminate_user_process(41, Some(7)));
        assert_eq!(process_table::is_process_exiting(41), Some(true));
        assert!(scheduler.retired[1]);
        assert!(scheduler.retired[2]);
        assert!(!scheduler.retired[3]);
    }

    #[test]
    fn terminating_the_last_task_marks_its_process_exiting() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let owner = test_process(45);
        scheduler.contexts[1] = Some(test_user_context(owner));
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 451,
        });

        assert!(scheduler.terminate_user_task(451, Some(7)));
        assert_eq!(process_table::is_process_exiting(45), Some(true));
        assert!(scheduler.retired[1]);
    }

    #[test]
    fn retirement_revokes_task_and_process_ipc_authority() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let owner = test_process(94);
        scheduler.contexts[1] = Some(test_user_context(owner));
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 941,
        });

        let task_endpoint =
            kernel_ipc_runtime::api::create_endpoint_for_task(941).expect("task-owned endpoint");
        let process_endpoint = kernel_ipc_runtime::api::create_endpoint_for_process(94)
            .expect("process-owned endpoint");
        let (task_reply, _) =
            kernel_ipc_runtime::api::enqueue_endpoint_call(task_endpoint, 951, b"task")
                .expect("task call");
        let (process_reply, _) =
            kernel_ipc_runtime::api::enqueue_endpoint_call(process_endpoint, 952, b"process")
                .expect("process call");

        scheduler.retire_slot(
            1,
            super::TaskRetireReason::Terminated {
                requested_by_pid: None,
            },
        );
        scheduler
            .take_retirement_side_effect()
            .expect("retirement side effects")
            .complete(|task_id| {
                let _ = scheduler.wake_task(task_id);
            });

        assert!(matches!(
            kernel_ipc_runtime::api::take_endpoint_response_detailed(task_reply, 0),
            Ok(EndpointResponseTake::Error {
                error: IpcError::PeerClosed,
                discarded_request_handles,
            }) if discarded_request_handles.is_empty()
        ));
        assert!(matches!(
            kernel_ipc_runtime::api::take_endpoint_response_detailed(process_reply, 0),
            Ok(EndpointResponseTake::Error {
                error: IpcError::PeerClosed,
                discarded_request_handles,
            }) if discarded_request_handles.is_empty()
        ));
        assert_eq!(
            kernel_ipc_runtime::api::enqueue_endpoint_call(task_endpoint, 953, b"late-task"),
            Err(IpcError::InvalidHandle)
        );
        assert_eq!(
            kernel_ipc_runtime::api::enqueue_endpoint_call(process_endpoint, 954, b"late-process"),
            Err(IpcError::InvalidHandle)
        );
    }

    #[test]
    fn retired_user_slot_waits_for_exact_runtime_cleanup_ack() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let owner = test_process(96);
        scheduler.contexts[1] = Some(test_user_context(owner));
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 961,
        });

        scheduler.retire_slot(1, super::TaskRetireReason::Exited);
        let cleanup = scheduler
            .next_retired_task_cleanup()
            .expect("retired user task cleanup");
        assert_eq!(cleanup.task_id(), 961);
        assert_eq!(cleanup.process_id(), 96);
        assert!(cleanup.process_terminal());
        // Retain the external side-effect token locally so this assertion
        // isolates the runtime-cleanup acknowledgement gate. If that gate is
        // removed, the retired stack becomes reclaimable before its exact
        // userspace cleanup acknowledgement.
        let side_effect = scheduler
            .take_retirement_side_effect()
            .expect("retirement side effects");
        assert!(scheduler.reap_inactive_retired_slots().is_none());
        assert!(scheduler.contexts[1].is_some());

        assert!(
            !scheduler.complete_retired_task_cleanup(crate::multitask::RetiredTaskCleanup {
                task_id: 962,
                process_id: 96,
                process_terminal: true,
                clear_child_tid: 0,
                robust_list_head: 0,
                robust_list_len: 0,
            })
        );
        assert!(scheduler.complete_retired_task_cleanup(cleanup));
        side_effect.complete(|task_id| {
            let _ = scheduler.wake_task(task_id);
        });
        let reclaim = scheduler
            .reap_inactive_retired_slots()
            .expect("retired slot reclaim");
        assert!(scheduler.contexts[1].is_none());
        assert_eq!(process_table::thread_count_by_pid(96), Some(1));
        reclaim.complete();
        assert_eq!(process_table::thread_count_by_pid(96), Some(0));
        assert_eq!(process_table::reap_exited_processes(), 1);
    }

    #[test]
    fn retirement_cleanup_stamps_process_terminal_only_on_last_live_thread() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let owner = test_process(97);
        scheduler.contexts[1] = Some(test_user_context(owner));
        scheduler.contexts[2] = Some(test_user_context(owner));
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 971,
        });
        scheduler.starts[2] = Some(TaskStart {
            entry: noop_task_entry,
            id: 972,
        });

        scheduler.retire_slot(1, super::TaskRetireReason::Exited);
        let first = scheduler
            .next_retired_task_cleanup()
            .expect("first thread cleanup");
        assert_eq!(first.task_id(), 971);
        assert!(!first.process_terminal());
        assert!(scheduler.complete_retired_task_cleanup(first));

        scheduler.retire_slot(2, super::TaskRetireReason::Exited);
        let last = scheduler
            .next_retired_task_cleanup()
            .expect("last thread cleanup");
        assert_eq!(last.task_id(), 972);
        assert!(last.process_terminal());
    }

    #[test]
    fn exec_sibling_slot_stays_quarantined_until_runtime_cleanup() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let owner = test_process(98);
        scheduler.contexts[1] = Some(test_user_context(owner));
        scheduler.contexts[2] = Some(test_user_context(owner));
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 981,
        });
        scheduler.starts[2] = Some(TaskStart {
            entry: noop_task_entry,
            id: 982,
        });

        scheduler.retire_exec_sibling_slot(2);
        assert!(scheduler.retired[2]);
        assert!(scheduler.contexts[2].is_some());
        assert_eq!(scheduler.contexts[2].unwrap().process_handle, None);
        assert_eq!(
            scheduler
                .next_retired_task_cleanup()
                .map(|cleanup| cleanup.task_id()),
            Some(982)
        );
        let _ = scheduler.reap_inactive_retired_slots();
        assert!(scheduler.contexts[2].is_some());
    }

    #[test]
    fn rejected_thread_attachment_releases_unpublished_stack() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let owner = test_process(95);
        scheduler.current_task = 1;
        scheduler.contexts[1] = Some(test_user_context(owner));
        process_table::mark_process_exiting(95).expect("mark exiting");
        assert!(scheduler.reserve_user_thread_slot(951).is_none());
        assert!(scheduler.contexts[2].is_none());
        assert!(scheduler.stacks[2].is_none());
    }

    #[test]
    fn synchronous_ipc_donation_promotes_and_revokes_a_transitive_user_chain() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let interactive = test_process(61);
        let broker = test_process(62);
        let policy = test_process(63);

        let mut interactive_context = test_user_context(interactive);
        interactive_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
        scheduler.contexts[1] = Some(interactive_context);
        scheduler.contexts[2] = Some(test_user_context(broker));
        scheduler.contexts[3] = Some(test_user_context(policy));
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 601,
        });
        scheduler.starts[2] = Some(TaskStart {
            entry: noop_task_entry,
            id: 602,
        });
        scheduler.starts[3] = Some(TaskStart {
            entry: noop_task_entry,
            id: 603,
        });

        assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));
        assert_eq!(scheduler.slot_class(3), Some(SchedClass::User));
        assert!(scheduler.inherit_ipc_priority(10, 601, 602));
        assert_eq!(scheduler.slot_class(2), Some(SchedClass::System));

        // The broker's nested synchronous call must pass the original
        // interactive class through to the final policy server.
        assert!(scheduler.inherit_ipc_priority(11, 602, 603));
        assert_eq!(scheduler.slot_class(3), Some(SchedClass::System));

        // A completed outer reply immediately restores both servers to their
        // manifest-derived class; no priority boost can leak past capability
        // lifetime.
        assert!(scheduler.release_ipc_priority(10));
        assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));
        assert_eq!(scheduler.slot_class(3), Some(SchedClass::User));
        assert!(scheduler.release_ipc_priority(11));
        assert_eq!(scheduler.slot_class(3), Some(SchedClass::User));

        // A process-owned endpoint without a sleeping receiver must select an
        // exact eligible worker before installing a System-class donation.
        assert!(scheduler.reserve_ipc_priority(601));
        let selected = scheduler
            .bind_ipc_priority_to_process_worker(12, 601, 62)
            .expect("eligible process worker");
        assert_eq!(selected, 602);
        assert_eq!(scheduler.slot_class(2), Some(SchedClass::System));
        assert_eq!(scheduler.current_dispatch_policy().sync_pick_hints.len(), 1);
        assert!(scheduler.release_ipc_priority(12));
        assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));

        // If the server is executing between receive calls, the reservation
        // follows the reply until that exact worker dequeues the request.
        scheduler.contexts[2].as_mut().unwrap().ready = false;
        assert!(scheduler.reserve_ipc_priority(601));
        assert!(
            scheduler
                .bind_ipc_priority_to_process_worker(13, 601, 62)
                .is_none()
        );
        assert!(scheduler.attach_reserved_ipc_priority(13, 601));
        assert!(
            scheduler
                .ipc_priority_donations
                .iter()
                .flatten()
                .any(|entry| {
                    entry.reply == 13 && matches!(entry.target, IpcDonationTarget::AwaitingReceiver)
                })
        );
        assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));
        assert!(scheduler.release_ipc_priority(13));
        assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));

        assert!(scheduler.reserve_ipc_priority(601));
        assert!(scheduler.attach_reserved_ipc_priority(14, 601));
        assert!(scheduler.inherit_ipc_priority(14, 601, 602));
        assert_eq!(scheduler.slot_class(2), Some(SchedClass::System));
        assert!(scheduler.release_ipc_priority(14));
        assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));
    }

    #[test]
    fn scheduler_block_arm_is_exact_race_safe_and_terminally_revoked() {
        let mut scheduler = boxed_scheduler();
        let base = crate::memory::paging::USER_SPACE_BASE;
        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let slot = scheduler
            .allocate_user_slot(
                690,
                ProcessAddressSpace::empty_for_tests(),
                UserTaskBootstrap::new(
                    UserAbi::Linux,
                    x86_64::VirtAddr::new(base + 0x2_000),
                    x86_64::VirtAddr::new(base + 0x4_000),
                ),
                None,
                crate::arch::pit::divisor_from_micros(100),
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("user slot");
        scheduler.current_task = slot;
        scheduler.contexts[slot]
            .as_mut()
            .expect("dispatched context")
            .ready = false;

        assert!(scheduler.arm_block_current_task());
        assert!(scheduler.contexts[slot].expect("context").wake_armed);
        assert!(scheduler.wake_task(690));
        assert!(!scheduler.contexts[slot].expect("context").wake_armed);
        assert_eq!(scheduler.commit_block_current_task(), Some(false));
        assert!(!scheduler.contexts[slot].expect("context").ready);

        assert!(scheduler.arm_block_current_task());
        assert_eq!(scheduler.commit_block_current_task(), Some(true));
        let blocked = scheduler.contexts[slot].expect("context");
        assert!(blocked.blocked);
        assert!(!blocked.ready);
        assert!(!scheduler.arm_block_current_task());
        assert!(!scheduler.cancel_block_current_task());

        scheduler.current_task = super::ROOT_TASK_SLOT;
        assert!(scheduler.wake_task(690));
        assert!(scheduler.contexts[slot].expect("context").ready);
        scheduler.contexts[slot]
            .as_mut()
            .expect("redispatched context")
            .ready = false;
        scheduler.current_task = slot;
        assert!(scheduler.arm_block_current_task());
        scheduler.retire_slot(slot, super::TaskRetireReason::Exited);
        let retired = scheduler.contexts[slot].expect("retired context");
        assert!(scheduler.retired[slot]);
        assert!(!retired.wake_armed);
        assert!(!scheduler.wake_task(690));
        assert_eq!(scheduler.commit_block_current_task(), None);
    }

    #[test]
    fn raced_wake_never_validates_a_consumed_current_frame() {
        let mut scheduler = boxed_scheduler();
        let slot = 1;
        scheduler.contexts[slot] = Some(TaskContext {
            // Dispatch consumed this frame. Deliberately leave an address that
            // could never be validated as a published continuation.
            saved_rsp: 0,
            ready: false,
            ready_since_ticks: 0,
            blocked: false,
            blocked_since_ticks: 0,
            wake_armed: true,
            weight: NICE_0_LOAD,
            vruntime_ns: 0,
            exec_start_ticks: 0,
            address_space_root: 0,
            kernel_stack_base: 0,
            kernel_stack_top: 0,
            alternate_kernel_stack_base: 0,
            alternate_kernel_stack_top: 0,
            user_mode: true,
            user_abi: Some(UserAbi::Linux),
            console_session: ConsoleSessionHandle::SYSTEM,
            process_handle: None,
            process_id: None,
            user_stack: None,
            windows_thread_state: None,
        });
        scheduler.starts[slot] = Some(TaskStart {
            entry: noop_task_entry,
            id: 691,
        });
        scheduler.current_task = slot;

        assert!(scheduler.wake_task(691));
        let context = scheduler.contexts[slot].expect("running task survived raced wake");
        assert!(!scheduler.retired[slot]);
        assert!(!context.ready);
        assert!(!context.blocked);
        assert!(!context.wake_armed);
        assert_eq!(scheduler.commit_block_current_task(), Some(false));

        // Commit publishes `blocked` before the caller enters its software
        // schedule trap. A remote CPU can wake in that exact interval while
        // the current stack frame is still consumed and intentionally invalid.
        assert!(scheduler.arm_block_current_task());
        assert_eq!(scheduler.commit_block_current_task(), Some(true));
        assert!(scheduler.contexts[slot].expect("committed block").blocked);
        assert!(scheduler.wake_task(691));
        let context = scheduler.contexts[slot].expect("post-commit wake survived");
        assert!(!scheduler.retired[slot]);
        assert!(!context.ready);
        assert!(!context.blocked);
        assert!(!context.wake_armed);
    }

    #[test]
    fn strict_class_requires_explicit_admission_not_a_large_cfs_weight() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let broker = test_process(69);
        let interactive = test_process(70);

        let mut broker_context = test_user_context(broker);
        broker_context.weight = 4 * NICE_0_LOAD;
        let mut interactive_context = test_user_context(interactive);
        interactive_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
        scheduler.contexts[1] = Some(broker_context);
        scheduler.contexts[2] = Some(interactive_context);

        assert_eq!(scheduler.slot_class(1), Some(SchedClass::User));
        assert_eq!(scheduler.slot_class(2), Some(SchedClass::System));
    }

    #[test]
    fn self_demotion_removes_base_system_class_and_caps_fair_weight() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let helper = test_process(73);
        let donor = test_process(74);

        let mut helper_context = test_user_context(helper);
        helper_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | (4 * NICE_0_LOAD);
        let mut donor_context = test_user_context(donor);
        donor_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
        scheduler.contexts[1] = Some(helper_context);
        scheduler.contexts[2] = Some(donor_context);
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 702,
        });
        scheduler.starts[2] = Some(TaskStart {
            entry: noop_task_entry,
            id: 701,
        });
        scheduler.current_task = 1;

        assert!(scheduler.demote_current_user_task_to_user_class());
        assert_eq!(scheduler.slot_class(1), Some(SchedClass::User));
        assert_eq!(
            scheduler.contexts[1].expect("current context").weight,
            NICE_0_LOAD
        );

        // A synchronous reply donation is a separate, capability-scoped
        // source of priority.  Demotion must not turn a pending interactive
        // request into an unbounded priority inversion.
        assert!(scheduler.inherit_ipc_priority(13, 701, 702));
        assert_eq!(scheduler.slot_class(1), Some(SchedClass::System));
        assert!(scheduler.demote_current_user_task_to_user_class());
        assert_eq!(scheduler.slot_class(1), Some(SchedClass::System));
        assert!(scheduler.release_ipc_priority(13));
        assert_eq!(scheduler.slot_class(1), Some(SchedClass::User));

        // The syscall is surrender-only. A task already below the nominal
        // share must not be able to raise its weight by invoking it.
        scheduler.contexts[1]
            .as_mut()
            .expect("current context")
            .weight = SYSTEM_CLASS_WEIGHT_FLAG | MIN_LOAD_WEIGHT;
        assert!(scheduler.demote_current_user_task_to_user_class());
        assert_eq!(
            scheduler.contexts[1].expect("current context").weight,
            MIN_LOAD_WEIGHT
        );
    }

    #[test]
    fn bounded_system_burst_reserves_a_ready_user_turn() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let system = test_process(71);
        let user = test_process(72);

        let mut system_context = test_user_context(system);
        system_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
        scheduler.contexts[1] = Some(system_context);
        scheduler.contexts[2] = Some(test_user_context(user));
        scheduler
            .current_dispatch_policy_mut()
            .system_dispatch_streak = MAX_CONSECUTIVE_SYSTEM_DISPATCHES;

        assert!(scheduler.user_reservation_due());
        scheduler.record_dispatch_class(2);
        assert_eq!(
            scheduler.current_dispatch_policy().system_dispatch_streak,
            0
        );
        assert!(!scheduler.user_reservation_due());

        scheduler.record_dispatch_class(1);
        assert_eq!(
            scheduler.current_dispatch_policy().system_dispatch_streak,
            1
        );
    }

    #[test]
    fn user_reservation_obeys_vruntime_without_a_wall_clock_bypass() {
        let mut scheduler = boxed_scheduler();
        let base = crate::memory::paging::USER_SPACE_BASE;
        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let mut allocate = |task_id, offset, weight| {
            scheduler
                .allocate_user_slot(
                    task_id,
                    ProcessAddressSpace::empty_for_tests(),
                    UserTaskBootstrap::new(
                        UserAbi::Linux,
                        x86_64::VirtAddr::new(base + offset),
                        x86_64::VirtAddr::new(base + offset + 0x1_000),
                    ),
                    None,
                    weight,
                    user_cs,
                    user_ss,
                    super::RFLAGS_RESERVED_BIT_1,
                    false,
                    noop_task_entry,
                )
                .expect("user slot")
        };
        let current = allocate(
            75,
            0x2_000,
            crate::arch::pit::divisor_from_micros(2_000) | super::INTERACTIVE_PIT_DIVISOR_FLAG,
        );
        let newer_user = allocate(76, 0x4_000, crate::arch::pit::divisor_from_micros(100));
        let older_user = allocate(77, 0x6_000, crate::arch::pit::divisor_from_micros(100));
        scheduler.contexts[newer_user]
            .as_mut()
            .expect("newer user context")
            .ready_since_ticks = 2;
        scheduler.contexts[newer_user]
            .as_mut()
            .expect("newer user context")
            .vruntime_ns = 10;
        scheduler.contexts[older_user]
            .as_mut()
            .expect("older user context")
            .ready_since_ticks = 1;
        scheduler.contexts[older_user]
            .as_mut()
            .expect("older user context")
            .vruntime_ns = 20;

        assert!(!scheduler.user_reservation_due());
        assert_eq!(scheduler.reserved_user_pick(current), None);

        scheduler
            .current_dispatch_policy_mut()
            .system_dispatch_streak = MAX_CONSECUTIVE_SYSTEM_DISPATCHES;
        assert_eq!(scheduler.reserved_user_pick(current), Some(newer_user));
    }

    #[test]
    fn fair_locality_is_bounded_by_class_and_vruntime_lag() {
        let mut scheduler = boxed_scheduler();
        let base = crate::memory::paging::USER_SPACE_BASE;
        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let mut allocate = |task_id, offset| {
            scheduler
                .allocate_user_slot(
                    task_id,
                    ProcessAddressSpace::empty_for_tests(),
                    UserTaskBootstrap::new(
                        UserAbi::Linux,
                        x86_64::VirtAddr::new(base + offset),
                        x86_64::VirtAddr::new(base + offset + 0x1_000),
                    ),
                    None,
                    crate::arch::pit::divisor_from_micros(100),
                    user_cs,
                    user_ss,
                    super::RFLAGS_RESERVED_BIT_1,
                    false,
                    noop_task_entry,
                )
                .expect("user slot")
        };
        let current = allocate(781, 0x2_000);
        let global_min = allocate(782, 0x4_000);
        let local = allocate(783, 0x6_000);
        scheduler.current_task = current;
        scheduler.task_last_cpu[current] = 0;
        scheduler.task_last_cpu[global_min] = 1;
        scheduler.task_last_cpu[local] = 0;
        scheduler.contexts[current]
            .as_mut()
            .expect("current context")
            .vruntime_ns = 10_000_000;
        scheduler.contexts[global_min]
            .as_mut()
            .expect("global context")
            .vruntime_ns = 1_000_000;
        scheduler.contexts[local]
            .as_mut()
            .expect("local context")
            .vruntime_ns = 1_000_000 + SCHED_CPU_LOCALITY_LAG_NS;

        assert_eq!(scheduler.pick_min_vruntime(current), Some(local));

        scheduler.contexts[local]
            .as_mut()
            .expect("local context")
            .vruntime_ns = 1_000_001 + SCHED_CPU_LOCALITY_LAG_NS;
        assert_eq!(scheduler.pick_min_vruntime(current), Some(global_min));

        scheduler.contexts[global_min]
            .as_mut()
            .expect("global context")
            .weight |= SYSTEM_CLASS_WEIGHT_FLAG;
        scheduler.contexts[global_min]
            .as_mut()
            .expect("global context")
            .vruntime_ns = u64::MAX / 2;
        assert_eq!(scheduler.pick_min_vruntime(current), Some(global_min));
    }

    #[test]
    fn overdue_system_task_is_forced_after_latency_bound() {
        let mut scheduler = boxed_scheduler();
        let base = crate::memory::paging::USER_SPACE_BASE;
        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let bootstrap = |offset| {
            UserTaskBootstrap::new(
                UserAbi::Linux,
                x86_64::VirtAddr::new(base + offset),
                x86_64::VirtAddr::new(base + offset + 0x1_000),
            )
        };
        let current = scheduler
            .allocate_user_slot(
                701,
                ProcessAddressSpace::empty_for_tests(),
                bootstrap(0x2_000),
                None,
                crate::arch::pit::divisor_from_micros(100),
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("current slot");
        let interactive = scheduler
            .allocate_user_slot(
                702,
                ProcessAddressSpace::empty_for_tests(),
                bootstrap(0x4_000),
                None,
                crate::arch::pit::divisor_from_micros(2_000) | super::INTERACTIVE_PIT_DIVISOR_FLAG,
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("interactive slot");
        scheduler.contexts[interactive]
            .as_mut()
            .expect("interactive context")
            .ready_since_ticks = 1;

        let now_ticks = crate::arch::rtc::ticks_per_second().saturating_mul(2);
        assert_eq!(
            scheduler.overdue_system_pick(current, now_ticks),
            Some(interactive)
        );
    }

    #[test]
    fn overdue_system_continuation_precedes_unrelated_ipc_hint_without_losing_it() {
        let mut scheduler = boxed_scheduler();
        let base = crate::memory::paging::USER_SPACE_BASE;
        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let mut allocate = |task_id, offset| {
            scheduler
                .allocate_user_slot(
                    task_id,
                    ProcessAddressSpace::empty_for_tests(),
                    UserTaskBootstrap::new(
                        UserAbi::Linux,
                        x86_64::VirtAddr::new(base + offset),
                        x86_64::VirtAddr::new(base + offset + 0x1_000),
                    ),
                    None,
                    crate::arch::pit::divisor_from_micros(2_000)
                        | super::INTERACTIVE_PIT_DIVISOR_FLAG,
                    user_cs,
                    user_ss,
                    super::RFLAGS_RESERVED_BIT_1,
                    false,
                    noop_task_entry,
                )
                .expect("System task slot")
        };
        let current = allocate(811, 0x2_000);
        let overdue = allocate(812, 0x4_000);
        let hinted = allocate(813, 0x6_000);
        scheduler.contexts[overdue]
            .as_mut()
            .expect("overdue context")
            .ready_since_ticks = 1;
        scheduler.contexts[hinted]
            .as_mut()
            .expect("hinted context")
            .ready_since_ticks = 0;
        scheduler.current_task = current;
        scheduler.set_next_pick_hint(813);

        let now_ticks = crate::arch::rtc::ticks_per_second().saturating_mul(2);
        assert_eq!(
            scheduler.take_overdue_system_or_pick_hint(current, now_ticks),
            Some(overdue)
        );
        assert_eq!(
            scheduler.current_dispatch_policy().next_pick_hint,
            Some(hinted)
        );

        scheduler.contexts[overdue]
            .as_mut()
            .expect("overdue context")
            .ready = false;
        assert_eq!(
            scheduler.take_overdue_system_or_pick_hint(current, now_ticks),
            Some(hinted)
        );
        assert_eq!(scheduler.current_dispatch_policy().next_pick_hint, None);
    }

    #[test]
    fn overdue_system_continuation_precedes_a_fresh_latency_handoff() {
        let mut scheduler = boxed_scheduler();
        let base = crate::memory::paging::USER_SPACE_BASE;
        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let mut allocate = |task_id, offset, system| {
            scheduler
                .allocate_user_slot(
                    task_id,
                    ProcessAddressSpace::empty_for_tests(),
                    UserTaskBootstrap::new(
                        UserAbi::Linux,
                        x86_64::VirtAddr::new(base + offset),
                        x86_64::VirtAddr::new(base + offset + 0x1_000),
                    ),
                    None,
                    crate::arch::pit::divisor_from_micros(if system { 2_000 } else { 100 })
                        | if system {
                            super::INTERACTIVE_PIT_DIVISOR_FLAG
                        } else {
                            0
                        },
                    user_cs,
                    user_ss,
                    super::RFLAGS_RESERVED_BIT_1,
                    false,
                    noop_task_entry,
                )
                .expect("task slot")
        };
        let current = allocate(821, 0x2_000, true);
        let overdue = allocate(822, 0x4_000, true);
        let hinted = allocate(823, 0x6_000, false);
        let now_ticks = crate::arch::rtc::ticks_per_second().saturating_mul(2);
        scheduler.contexts[overdue]
            .as_mut()
            .expect("overdue context")
            .ready_since_ticks = 1;
        scheduler.contexts[hinted]
            .as_mut()
            .expect("hinted context")
            .ready_since_ticks = now_ticks;
        scheduler.current_task = current;
        assert!(scheduler.set_next_latency_pick_hint(823));

        assert_eq!(
            scheduler.mandatory_overdue_system_pick(current, now_ticks),
            Some(overdue)
        );
        assert_eq!(scheduler.current_dispatch_policy().latency_pick_hint_len, 1);

        scheduler.contexts[overdue]
            .as_mut()
            .expect("overdue context")
            .ready = false;
        assert_eq!(
            scheduler.mandatory_overdue_system_pick(current, now_ticks),
            None
        );
        assert_eq!(
            scheduler.take_next_latency_pick_hint_ready_slot(),
            Some(hinted)
        );
    }

    #[test]
    fn event_wait_handoff_is_fifo_deduplicated_and_burst_bounded() {
        let mut scheduler = boxed_scheduler();
        let base = crate::memory::paging::USER_SPACE_BASE;
        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let bootstrap = |offset| {
            UserTaskBootstrap::new(
                UserAbi::Linux,
                x86_64::VirtAddr::new(base + offset),
                x86_64::VirtAddr::new(base + offset + 0x1_000),
            )
        };
        let user_slot = scheduler
            .allocate_user_slot(
                901,
                ProcessAddressSpace::empty_for_tests(),
                bootstrap(0x2_000),
                None,
                crate::arch::pit::divisor_from_micros(100),
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("user slot");
        let system_slot = scheduler
            .allocate_user_slot(
                902,
                ProcessAddressSpace::empty_for_tests(),
                bootstrap(0x4_000),
                None,
                crate::arch::pit::divisor_from_micros(2_000) | super::INTERACTIVE_PIT_DIVISOR_FLAG,
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("system slot");
        let second_user_slot = scheduler
            .allocate_user_slot(
                903,
                ProcessAddressSpace::empty_for_tests(),
                bootstrap(0x6_000),
                None,
                crate::arch::pit::divisor_from_micros(100),
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("second user slot");

        assert_eq!(scheduler.slot_class(user_slot), Some(SchedClass::User));
        assert_eq!(scheduler.slot_class(system_slot), Some(SchedClass::System));
        assert_eq!(
            scheduler.slot_class(second_user_slot),
            Some(SchedClass::User)
        );
        assert!(scheduler.set_next_latency_pick_hint(901));
        assert!(!scheduler.set_next_latency_pick_hint(902));
        assert!(scheduler.set_next_latency_pick_hint(903));
        assert!(scheduler.set_next_latency_pick_hint(901));
        assert_eq!(
            scheduler.take_next_latency_pick_hint_ready_slot(),
            Some(user_slot)
        );
        assert_eq!(
            scheduler.take_next_latency_pick_hint_ready_slot(),
            Some(second_user_slot)
        );
        assert_eq!(scheduler.take_next_latency_pick_hint_ready_slot(), None);

        assert!(scheduler.set_next_latency_pick_hint(901));
        scheduler
            .current_dispatch_policy_mut()
            .latency_handoff_streak = super::MAX_CONSECUTIVE_LATENCY_HANDOFFS;
        assert_eq!(scheduler.take_next_latency_pick_hint_ready_slot(), None);
        scheduler.record_latency_handoff(false);
        assert_eq!(
            scheduler.take_next_latency_pick_hint_ready_slot(),
            Some(user_slot)
        );
    }

    #[test]
    fn dispatch_fairness_and_handoff_state_is_cpu_isolated() {
        let _process_table = process_table::tests::isolate_process_table();
        let mut scheduler = boxed_scheduler();
        let user = test_process(904);
        scheduler.contexts[1] = Some(test_user_context(user));
        {
            let mut policy = scheduler.cpu_dispatch[1].lock();
            policy.system_dispatch_streak = MAX_CONSECUTIVE_SYSTEM_DISPATCHES;
            policy.next_pick_hint = Some(7);
        }

        scheduler.record_dispatch_class(1);
        scheduler.current_dispatch_policy_mut().next_pick_hint = None;

        assert_eq!(scheduler.cpu_dispatch[0].lock().system_dispatch_streak, 0);
        assert_eq!(
            scheduler.cpu_dispatch[1].lock().system_dispatch_streak,
            MAX_CONSECUTIVE_SYSTEM_DISPATCHES
        );
        assert_eq!(scheduler.cpu_dispatch[1].lock().next_pick_hint, Some(7));
    }
}
