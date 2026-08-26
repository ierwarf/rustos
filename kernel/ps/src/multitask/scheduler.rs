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
mod donation_ledger;
mod handoff_queue;
mod handoffs;
mod ipc_donation;
mod linux_thread_state;
mod locality;
mod reclaim;
mod runqueue;
mod runqueue_policy;
mod runtime_profile;
mod scheduling_context;
mod sync_handoff;
pub(in crate::multitask) use runtime_profile::SchedulerEntryCause;
pub use runtime_profile::drain_scheduler_runtime_profile;
pub(in crate::multitask) use runtime_profile::publish_scheduler_runtime_profile;

pub(in crate::multitask) fn local_dispatch_work_pending(cpu: usize) -> bool {
    // A direct synchronous handoff deliberately has no fair-runqueue entry.
    // Its FIFO publication is therefore an independent durable source of
    // dispatch work and must defeat the periodic continuation fast return.
    if runqueue::local_dispatch_work_pending(cpu) {
        return true;
    }
    #[cfg(not(test))]
    return sync_handoff::pending(cpu);
    #[cfg(test)]
    false
}

/// Enqueue the opaque post-reply token only after the scheduler catalog guard
/// that issued it has dropped.  The target owner validates its runqueue
/// generation without re-entering Scheduler.
pub(super) fn enqueue_reply_wake_handoff(token: ReplyWakeHandoff) -> bool {
    sync_handoff::enqueue_reply_wake(token)
}

mod smp;
#[cfg(test)]
mod synchronous_handoff_tests;
mod thread_slots;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use core::{mem, ptr};

use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{DS, ES, FS, GS, Segment};
use x86_64::registers::model_specific::FsBase;
use x86_64::structures::gdt::SegmentSelector;

use kernel_object::api::identity::ObjectIdentity;

#[cfg(test)]
use crate::arch::simd::SimdState;
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
use super::run_authority;
use super::{UserFaultDisposition, UserStackState, UserTaskBootstrap, initial_task_rflags};
use context_validation::context_validation_reason_code;
use dispatch_policy::{CpuDispatchGuard, CpuDispatchLock, CpuDispatchPolicy};
use handoff_queue::SlotHandoffQueue;
#[cfg(test)]
use ipc_donation::{IpcDonationTarget, IpcPriorityDonation};
pub(super) use linux_thread_state::CurrentLinuxThreadBinding;
use linux_thread_state::{LinuxThreadStateLock, empty_linux_thread_state_lock};
use reclaim::{RetiredSlotReclaim, RetirementSideEffect};
use runtime_profile::SchedulerPhase;
use sync_handoff::ReplyWakeHandoff;

// The enabled product topology boots roughly twenty policy/service processes
// before the UI creates its bounded input, display, diagnostics, console, and
// Wayland workers. A 32-slot table therefore exhausted during normal shell
// launch and turned a recoverable capacity error into uiserver thread-spawn
// panic. Keep the scheduler allocation-free and explicitly bounded, but size
// the product contract for service growth and application headroom.
pub(super) const MAX_TASK: usize = 128;

/// Releases reply-owned scheduling urgency without acquiring the scheduler
/// catalog. The donation ledger's exact reply identity is the terminal owner;
/// wake publication still runs through its separate owner-word transition.
pub(in crate::multitask) fn release_reply_donation(reply: u64) -> bool {
    donation_ledger::release_reply(reply)
}

/// A host-test set of task slots held in one machine word pair.
///
/// The donation walk needs a cycle-breaking set, and it needs it on the
/// dispatch path: both O(local-runnable) pick scans derive a class per
/// candidate, so the set is constructed once per candidate per dispatch. A
/// `[bool; MAX_TASK]` costs a 128-byte stack clear each time to record at most
/// `MAX_IPC_DONATION_CHAIN_DEPTH` members.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct SlotSet(u128);

/// One bit per slot, so the table may not outgrow the word.
#[cfg(test)]
const _: () = assert!(MAX_TASK <= u128::BITS as usize);

#[cfg(test)]
impl SlotSet {
    pub(super) const EMPTY: Self = Self(0);

    /// Rejects an out-of-range slot rather than wrapping it onto another
    /// member, which is what the array this replaced did by panicking on the
    /// index.
    #[inline]
    fn bit(slot: usize) -> u128 {
        assert!(slot < MAX_TASK, "slot set index exceeds capacity");
        1_u128 << slot
    }

    #[inline]
    pub(super) fn contains(&self, slot: usize) -> bool {
        self.0 & Self::bit(slot) != 0
    }

    #[inline]
    pub(super) fn insert(&mut self, slot: usize) {
        self.0 |= Self::bit(slot);
    }

    #[inline]
    pub(super) fn remove(&mut self, slot: usize) {
        self.0 &= !Self::bit(slot);
    }
}

/// How far a priority donation may propagate before the chain is truncated.
///
/// A real chain is a client calling a service that calls another service; four
/// levels covers that with room to spare, and anything deeper is either a
/// pathological topology or an attempt to drive kernel-stack depth from
/// userspace. The bound is what keeps the recursion in `effective_slot_class`
/// a constant cost inside the global scheduler lock rather than one that grows
/// with how many tasks happen to be waiting on each other.
#[cfg(test)]
const MAX_IPC_DONATION_CHAIN_DEPTH: usize = 4;
/// Sentinel for an unresolved donor-slot hint. `MAX_TASK` fits in `u8`, so the
/// hint table stays one cache-friendly byte per donation entry.
#[cfg(test)]
const NO_SLOT_HINT: u8 = u8::MAX;
const TASK_SLOT_HINT_EMPTY: u8 = u8::MAX;
const _: () = assert!(MAX_TASK.is_power_of_two() && MAX_TASK < u8::MAX as usize);
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
    scheduling_context: scheduling_context::SchedulingContext,
    /// Host-test fixture for the production per-slot saved-frame pointer.
    #[cfg(test)]
    saved_rsp: usize,
    #[cfg(test)]
    test_ready: bool,
    ready_since_ticks: u64,
    blocked: bool,
    blocked_since_ticks: u64,
    /// Block-arm flag for race-free sleep/wake. Set by `arm_block_current_task`;
    /// cleared by `wake_task` and `commit_block_current_task`. A wake delivered
    /// while the task is still running clears the flag, so the subsequent
    /// `commit_block_current_task` observes that a wake raced and refuses to
    /// block. Mirrors Linux's `prepare_to_wait` / `set_current_state` pattern.
    wake_armed: bool,
    /// Exact condition authorized by the current wait epoch. Endpoint receive
    /// identity is required before a sender may bypass ordinary wake/runqueue
    /// publication; every other sleeper remains deliberately generic.
    block_reason: BlockReason,
    /// CFS-like load weight. Bigger weight -> larger CPU share. Derived from
    /// the task's `weight_micros` / pit_divisor at allocation time.
    weight: u32,
    /// Virtual runtime in nanoseconds, scaled by NICE_0_LOAD/weight. The task
    /// with the smallest vruntime among the ready set is picked next.
    #[cfg(test)]
    vruntime_ns: u64,
    /// Test fixture for the per-slot execution baseline. Production storage
    /// is in `runqueue`, so runtime accounting does not require a task-catalog
    /// read. Zero means the task is not currently charging an interval.
    #[cfg(test)]
    exec_start_ticks: u64,
    address_space_root: u64,
    /// Host-test mirrors for production per-slot primary stack geometry.
    #[cfg(test)]
    kernel_stack_base: u64,
    #[cfg(test)]
    kernel_stack_top: u64,
    /// Host-test mirror for the production versioned alternate-stack record.
    #[cfg(test)]
    alternate_kernel_stack_base: u64,
    #[cfg(test)]
    alternate_kernel_stack_top: u64,
    user_mode: bool,
    user_abi: Option<UserAbi>,
    console_session: ConsoleSessionHandle,
    process_handle: Option<ProcessHandle>,
    process_id: Option<u64>,
    user_stack: Option<UserStackState>,
    windows_thread_state: Option<WindowsThreadRuntimeState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockReason {
    None,
    Generic,
    EndpointReceive(u64),
    EndpointReply(u64),
}

#[derive(Clone, Copy)]
pub(super) struct TaskStart {
    pub(super) entry: fn(u64),
    pub(super) id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FastIpcCallHandoffOutcome {
    CommittedSameCpu,
    CommittedCrossCpu,
    SenderMismatch,
    ReceiverMismatch,
    DonationUnavailable,
    DirectCustodyUnavailable,
    OrderingUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FastIpcReplyHandoffOutcome {
    Direct,
    LocalFallback,
    Rejected,
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

/// Result of one fused synchronous-IPC call admission.
///
/// A call needs its caller's scheduling class and, when that class is System,
/// a bounded donation reservation. Both start from `find_task_slot` for the
/// same task, and asking separately took the global scheduler lock twice --
/// masking interrupts twice -- to answer two questions about one slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpcCallAdmission {
    /// The caller is live and in the System scheduling class.
    pub system_class: bool,
    /// Reply-owned scheduling-context custody capacity was reserved. System
    /// callers additionally use the same edge for strict-priority donation.
    pub donation_reserved: bool,
    /// Exact 1:1 scheduling-context identity for this live scheduler slot.
    /// Slot reuse cannot inherit custody because the monotonically allocated
    /// task label maps to a distinct nonzero object generation.
    pub scheduling_context: Option<ObjectIdentity>,
    /// Task that owns `scheduling_context`. For a nested passive-server call
    /// this is the root client, while the immediate caller remains the reply
    /// wake target.
    pub scheduling_context_owner_task_id: Option<u64>,
}

/// Result of one fused synchronous-IPC call handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpcCallHandoffOutcome {
    /// The reply donation edge was installed, or none was required.
    pub inherited: bool,
    /// The receiver transitioned from blocked to runnable.
    pub woke: bool,
    /// The direct pick hint was accepted by the receiver's dispatch CPU.
    pub hinted: bool,
}

pub(super) struct Scheduler {
    contexts: [Option<TaskContext>; MAX_TASK],
    scheduling_domains: [Option<scheduling_context::SchedulingDomainState>;
        scheduling_context::MAX_SCHEDULING_DOMAINS],
    /// Thread metadata is independently synchronized from scheduler state.
    /// Process-state operations may hold this bounded raw lock without
    /// escaping an unprotected pointer into `contexts`; scheduler signal and
    /// lifecycle paths take Scheduler -> LinuxThreadState in that order.
    linux_thread_states: [LinuxThreadStateLock; MAX_TASK],
    retired: [bool; MAX_TASK],
    retirement_cleanup: [Option<super::RetiredTaskCleanup>; MAX_TASK],
    retirement_cleanup_claimed: [bool; MAX_TASK],
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
    /// Host-test fixture for production per-slot SIMD storage.
    #[cfg(test)]
    simd_states: [SimdState; MAX_TASK],
    starts: [Option<TaskStart>; MAX_TASK],
    /// Direct-mapped accelerator for exact task-ID lookup. Every hit is
    /// revalidated against the authoritative live slot, so collision, reuse,
    /// or stale cache state can only cause a bounded fallback scan.
    task_slot_hints: [AtomicU8; MAX_TASK],
    stacks: [Option<Vec<u8>>; MAX_TASK],
    idle_cpu: [u8; MAX_TASK],
    /// Host-test mirrors for the production versioned affinity payload.
    #[cfg(test)]
    task_affinity_masks: [u64; MAX_TASK],
    #[cfg(test)]
    process_affinity_masks: [u64; MAX_TASK],
    #[cfg(test)]
    affinity_migration_pending: [bool; MAX_TASK],
    /// Last CPU that actually dispatched each exact live task. This is
    /// scheduler policy metadata, unlike the independently reset diagnostic
    /// profile. It is only a bounded fair-pick tie-break and grants no CPU
    /// ownership or affinity authority.
    #[cfg(test)]
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
    /// Unit-test schedulers own disjoint instances of the exact production
    /// synchronous-handoff state machine. Production storage remains the
    /// per-CPU static backend in `sync_handoff`.
    #[cfg(test)]
    sync_handoff_states:
        [sync_handoff::SyncHandoffLock; nucleus_core::util::lockdep::MAX_TRACKED_CPUS],
    /// Host-test mirror of the production reply-edge ledger.
    #[cfg(test)]
    ipc_priority_donations: [Option<IpcPriorityDonation>; MAX_TASK],
    #[cfg(test)]
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
    #[cfg(test)]
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
    /// The site and attributed segment time of the worst owner turn, kept so a
    /// maximum can be charged to a caller instead of left unexplained.
    runtime_profile_lock_hold_max_caller: Option<&'static core::panic::Location<'static>>,
    runtime_profile_lock_hold_max_attributed_ns: u64,
    /// Wakes raised while a raw owner was interrupted and drained by whichever
    /// acquisition came next. They are in-owner cost with no requesting caller.
    runtime_profile_deferred_wakes: u64,
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
            scheduling_domains: [None; scheduling_context::MAX_SCHEDULING_DOMAINS],
            linux_thread_states: [const { empty_linux_thread_state_lock() }; MAX_TASK],
            retired: [false; MAX_TASK],
            retirement_cleanup: [None; MAX_TASK],
            retirement_cleanup_claimed: [false; MAX_TASK],
            retirement_side_effects: [None; MAX_TASK],
            deferred_retire_reasons: [None; MAX_TASK],
            exec_target_quiesced: [false; MAX_TASK],
            thread_slot_reserved: [false; MAX_TASK],
            start_suspended: [false; MAX_TASK],
            job_stopped: [false; MAX_TASK],
            retire_reasons: [None; MAX_TASK],
            #[cfg(test)]
            simd_states: [SimdState::new(); MAX_TASK],
            starts: [None; MAX_TASK],
            task_slot_hints: [const { AtomicU8::new(TASK_SLOT_HINT_EMPTY) }; MAX_TASK],
            stacks: [const { None }; MAX_TASK],
            idle_cpu: [NO_IDLE_CPU; MAX_TASK],
            #[cfg(test)]
            task_affinity_masks: [UNRESTRICTED_CPU_MASK; MAX_TASK],
            #[cfg(test)]
            process_affinity_masks: [UNRESTRICTED_CPU_MASK; MAX_TASK],
            #[cfg(test)]
            affinity_migration_pending: [false; MAX_TASK],
            #[cfg(test)]
            task_last_cpu: [NO_IDLE_CPU; MAX_TASK],
            #[cfg(test)]
            current_task: 0,
            pending_reap: false,
            cpu_dispatch: [const { CpuDispatchLock::new(CpuDispatchPolicy::new()) };
                nucleus_core::util::lockdep::MAX_TRACKED_CPUS],
            #[cfg(test)]
            sync_handoff_states: sync_handoff::per_scheduler_locks(),
            #[cfg(test)]
            ipc_priority_donations: [None; MAX_TASK],
            #[cfg(test)]
            ipc_priority_donation_len: 0,
            #[cfg(test)]
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
            runtime_profile_lock_hold_max_caller: None,
            runtime_profile_lock_hold_max_attributed_ns: 0,
            runtime_profile_deferred_wakes: 0,
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

    fn scheduling_domain_is_eligible(
        &self,
        domain_slot: usize,
        policy: scheduling_context::SchedulingContextPolicy,
        now_ns: u64,
    ) -> bool {
        self.scheduling_domains
            .get(domain_slot)
            .and_then(|domain| *domain)
            .is_some_and(|domain| domain.policy() == policy && domain.is_eligible(now_ns))
    }

    fn prepare_scheduling_domain_dispatch(
        &mut self,
        domain_slot: usize,
        policy: scheduling_context::SchedulingContextPolicy,
        now_ns: u64,
    ) -> bool {
        self.scheduling_domains
            .get_mut(domain_slot)
            .and_then(Option::as_mut)
            .is_some_and(|domain| domain.policy() == policy && domain.prepare_dispatch(now_ns))
    }

    fn charge_scheduling_domain_runtime(
        &mut self,
        domain_slot: usize,
        policy: scheduling_context::SchedulingContextPolicy,
        now_ns: u64,
        elapsed_ns: u64,
    ) -> Option<scheduling_context::ChargeOutcome> {
        let domain = self.scheduling_domains.get_mut(domain_slot)?.as_mut()?;
        (domain.policy() == policy).then(|| domain.charge_runtime(now_ns, elapsed_ns))?
    }

    fn charge_effective_scheduling_context_runtime(
        &mut self,
        executing_slot: usize,
        now_ns: u64,
        elapsed_ns: u64,
    ) -> Option<(u64, scheduling_context::ChargeOutcome)> {
        let (context_owner_slot, reply) =
            self.effective_scheduling_context_charge_token(executing_slot);
        let owner = self.contexts[context_owner_slot]?;
        if !owner.scheduling_context.is_budgeted() {
            return None;
        }
        let policy = owner
            .scheduling_context
            .policy()
            .expect("budgeted scheduling context lost its policy");
        let domain_slot = owner
            .scheduling_context
            .domain_slot()
            .expect("budgeted scheduling context lost its domain slot");
        let owner_task_id = self.starts[context_owner_slot]
            .map(|start| start.id)
            .expect("budgeted scheduling context lost its owner task");
        let context_outcome = self.contexts[context_owner_slot]
            .as_mut()
            .expect("accounted scheduling-context owner disappeared")
            .scheduling_context
            .charge_runtime(now_ns, elapsed_ns)
            .expect("scheduling-context accounting violated budget conservation");
        let domain_outcome = self
            .charge_scheduling_domain_runtime(domain_slot, policy, now_ns, elapsed_ns)
            .expect("scheduling-domain accounting lost admitted policy");
        if (context_outcome.exhausted || domain_outcome.exhausted)
            && context_outcome.charged_ns != 0
        {
            let recorded = self.contexts[context_owner_slot]
                .as_mut()
                .expect("exhausted scheduling-context owner disappeared")
                .scheduling_context
                .record_timeout_fault(reply);
            assert!(recorded, "budget exhaustion lost timeout-fault state");
        }
        Some((
            owner_task_id,
            scheduling_context::ChargeOutcome {
                charged_ns: context_outcome.charged_ns.min(domain_outcome.charged_ns),
                overrun_ns: context_outcome.overrun_ns.max(domain_outcome.overrun_ns),
                exhausted: context_outcome.exhausted || domain_outcome.exhausted,
            },
        ))
    }

    pub(super) fn current_scheduling_context_runtime_snapshot(
        &self,
    ) -> Option<super::SchedulingContextRuntimeSnapshot> {
        let executing_slot = self.current_task_slot();
        let executing_task_id = self.starts[executing_slot]?.id;
        let context_owner_slot = self.effective_scheduling_context_owner_slot(executing_slot);
        let owner = self.contexts[context_owner_slot]?;
        let context_owner_task_id = self.starts[context_owner_slot]?.id;
        let policy = owner.scheduling_context.policy()?;
        let context = owner.scheduling_context.runtime_snapshot()?;
        let domain_slot = owner.scheduling_context.domain_slot()?;
        let domain = self.scheduling_domains.get(domain_slot)?.as_ref()?;
        if domain.policy() != policy {
            return None;
        }
        let domain_budget = domain.runtime_snapshot()?;
        let identity = owner.scheduling_context.identity();
        Some(super::SchedulingContextRuntimeSnapshot {
            executing_task_id,
            context_owner_task_id,
            context_identity_slot: identity.slot(),
            context_identity_generation: identity.generation(),
            domain: policy.domain,
            policy_epoch: policy.policy_epoch,
            budget_ns: policy.budget_ns,
            period_ns: policy.period_ns,
            context_available_ns: context.available_ns,
            context_pending_refill_ns: context.pending_refill_ns,
            context_next_eligible_ns: context.next_eligible_ns,
            context_consumed_ns: context.consumed_ns,
            context_exhaustion_count: context.exhaustion_count,
            context_refill_count: context.refill_count,
            context_overflow_merge_count: context.overflow_merge_count,
            timeout_fault_count: context.timeout_fault_count,
            timeout_fault_consumed_ns: context.timeout_fault_consumed_ns,
            timeout_fault_budget_ns: policy.budget_ns,
            timeout_fault_period_ns: policy.period_ns,
            timeout_fault_reply: context.timeout_fault_reply,
            timeout_endpoint_cap: policy.timeout_endpoint_cap,
            timeout_fault_action: context.timeout_fault_action,
            domain_available_ns: domain_budget.available_ns,
            domain_pending_refill_ns: domain_budget.pending_refill_ns,
            domain_next_eligible_ns: domain_budget.next_eligible_ns,
            domain_consumed_ns: domain_budget.consumed_ns,
            domain_exhaustion_count: domain_budget.exhaustion_count,
            domain_refill_count: domain_budget.refill_count,
            domain_overflow_merge_count: domain_budget.overflow_merge_count,
        })
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
    /// Charges one dispatch phase, when the dispatch is instrumented.
    ///
    /// Thirteen call sites reach this per dispatch and each one reads the clock
    /// with `lfence; rdtsc`. Wrapping individually cheap phases in that is the
    /// same shape as the lock phase profiler, and ablation priced it the same
    /// way: `sched_yield` -11.7%, `ipc_rt_intra_process` -3.8%, and zero on a
    /// probe that performs no dispatch. The call sites stay unconditional so
    /// the phase split is one build switch away.
    #[inline]
    fn mark_phase(&mut self, phase: SchedulerPhase, marker: &mut u64) {
        #[cfg(rustos_scheduler_phase_profile)]
        {
            let now = crate::arch::clock::monotonic_nanos();
            self.record_runtime_profile_phase(phase, now.saturating_sub(*marker));
            *marker = now;
        }
        #[cfg(not(rustos_scheduler_phase_profile))]
        {
            let _ = (phase, marker);
        }
    }

    /// The clock read that opens a dispatch phase chain, or zero when the
    /// dispatch is not instrumented. Every consumer is a `mark_phase` delta.
    #[inline]
    fn phase_chain_start() -> u64 {
        #[cfg(rustos_scheduler_phase_profile)]
        {
            crate::arch::clock::monotonic_nanos()
        }
        #[cfg(not(rustos_scheduler_phase_profile))]
        {
            0
        }
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
        let last_cpu = self.slot_last_cpu(slot);
        if last_cpu != NO_IDLE_CPU {
            return usize::from(last_cpu);
        }
        Self::current_dispatch_cpu()
    }

    fn handoff_slot_ready(&self, slot: usize) -> bool {
        slot < MAX_TASK
            && !self.retired[slot]
            && !self.start_suspended[slot]
            && !self.job_stopped[slot]
            && !self.exec_target_quiesced[slot]
            && self.deferred_retire_reasons[slot].is_none()
            // Queue membership replaces the legacy test readiness bit, but `!blocked` stays.
            //
            // The first conversion dropped it and panicked on both 8-vCPU runs.
            // The predicates differ in *both* directions, not just the obvious
            // one: a slot can be `Local` while `blocked` is already true —
            // `commit_block_current_task` raises `blocked` before the next turn
            // moves the owner word — and the old conjunction excluded it while
            // queue membership alone admits it. Nominating that slot dispatches
            // a task that believes it is blocked, which is how a suspended
            // frame ends up failing validation.
            && self.contexts[slot].is_some_and(|context| !context.blocked)
            && self.slot_is_runnable(slot)
    }

    fn new_task_vruntime(&self) -> u64 {
        self.min_ready_vruntime()
            .saturating_add(SCHED_NEW_TASK_VRUNTIME_PENALTY_NS)
    }

    /// Returns the fair-share key from the slot payload authority.
    ///
    /// Production never stores this hot scheduling value in the global
    /// `Scheduler` payload. Host-only unit schedulers retain their private
    /// fixture field because they intentionally do not publish runqueue owner
    /// words or static kernel state.
    #[inline]
    fn slot_vruntime(&self, slot: usize) -> u64 {
        #[cfg(not(test))]
        {
            runqueue::vruntime(slot)
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .map(|context| context.vruntime_ns)
                .unwrap_or(0)
        }
    }

    #[inline]
    fn initialize_slot_vruntime(&mut self, slot: usize, value: u64) {
        #[cfg(not(test))]
        runqueue::initialize_vruntime(slot, value);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.vruntime_ns = value;
        }
    }

    #[inline]
    fn slot_exec_start_ticks(&self, slot: usize) -> u64 {
        #[cfg(not(test))]
        {
            runqueue::exec_start_ticks(slot)
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .map(|context| context.exec_start_ticks)
                .unwrap_or(0)
        }
    }

    #[inline]
    fn initialize_slot_exec_start_ticks(&mut self, slot: usize, value: u64) {
        #[cfg(not(test))]
        runqueue::initialize_exec_start_ticks(slot, value);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.exec_start_ticks = value;
        }
    }

    #[inline]
    fn set_slot_exec_start_ticks(&mut self, slot: usize, value: u64) {
        #[cfg(not(test))]
        runqueue::set_exec_start_ticks(slot, value);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.exec_start_ticks = value;
        }
    }

    #[inline]
    fn slot_saved_rsp(&self, slot: usize) -> usize {
        #[cfg(not(test))]
        {
            runqueue::saved_rsp(slot)
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .map(|context| context.saved_rsp)
                .unwrap_or(0)
        }
    }

    #[inline]
    fn initialize_slot_saved_rsp(&mut self, slot: usize, value: usize) {
        #[cfg(not(test))]
        runqueue::initialize_saved_rsp(slot, value);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.saved_rsp = value;
        }
    }

    #[inline]
    fn set_slot_saved_rsp(&mut self, slot: usize, value: usize) {
        #[cfg(not(test))]
        runqueue::set_saved_rsp(slot, value);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.saved_rsp = value;
        }
    }

    #[inline]
    fn slot_tls_fs_base(&self, slot: usize) -> u64 {
        #[cfg(not(test))]
        {
            runqueue::simd_tls::tls_fs_base(slot)
        }
        #[cfg(test)]
        {
            self.linux_thread_state(slot)
                .map(|state| state.fs_base)
                .unwrap_or(0)
        }
    }

    #[inline]
    pub(in crate::multitask) fn set_current_tls_fs_base(&mut self, value: u64) {
        let slot = self.current_task_slot();
        #[cfg(not(test))]
        runqueue::simd_tls::set_tls_fs_base(slot, value);
        #[cfg(test)]
        if let Some(state) = self.linux_thread_states[slot].lock().state.as_mut() {
            state.fs_base = value;
        }
    }

    #[inline]
    fn slot_kernel_stack_bounds(&self, slot: usize) -> (u64, u64) {
        #[cfg(not(test))]
        {
            runqueue::kernel_stack_bounds(slot)
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .map(|context| (context.kernel_stack_base, context.kernel_stack_top))
                .unwrap_or((0, 0))
        }
    }

    #[inline]
    fn initialize_slot_kernel_stack_bounds(&mut self, slot: usize, base: u64, top: u64) {
        #[cfg(not(test))]
        runqueue::initialize_kernel_stack_bounds(slot, base, top);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.kernel_stack_base = base;
            context.kernel_stack_top = top;
        }
    }

    #[inline]
    fn slot_alternate_kernel_stack_bounds(&self, slot: usize) -> (u64, u64) {
        #[cfg(not(test))]
        {
            runqueue::alternate_kernel_stack_bounds(slot)
        }
        #[cfg(test)]
        {
            self.contexts[slot]
                .map(|context| {
                    (
                        context.alternate_kernel_stack_base,
                        context.alternate_kernel_stack_top,
                    )
                })
                .unwrap_or((0, 0))
        }
    }

    #[inline]
    fn initialize_slot_alternate_kernel_stack_bounds(&mut self, slot: usize) {
        #[cfg(not(test))]
        runqueue::initialize_alternate_kernel_stack_bounds(slot);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.alternate_kernel_stack_base = 0;
            context.alternate_kernel_stack_top = 0;
        }
    }

    #[inline]
    fn replace_slot_alternate_kernel_stack_bounds(&mut self, slot: usize, base: u64, top: u64) {
        #[cfg(not(test))]
        runqueue::replace_alternate_kernel_stack_bounds(slot, base, top);
        #[cfg(test)]
        if let Some(context) = self.contexts[slot].as_mut() {
            context.alternate_kernel_stack_base = base;
            context.alternate_kernel_stack_top = top;
        }
    }

    #[inline]
    fn initialize_slot_simd_state(&mut self, slot: usize) {
        #[cfg(not(test))]
        runqueue::simd_tls::initialize_simd_state(slot);
        #[cfg(test)]
        {
            self.simd_states[slot] = SimdState::new();
        }
    }

    #[inline]
    fn reset_slot_simd_state(&mut self, slot: usize) {
        #[cfg(not(test))]
        runqueue::simd_tls::reset_simd_state(slot);
        #[cfg(test)]
        {
            self.simd_states[slot] = SimdState::new();
        }
    }

    #[inline]
    fn save_slot_simd_state(&mut self, slot: usize) {
        #[cfg(not(test))]
        runqueue::simd_tls::save_simd_state(slot);
        #[cfg(test)]
        unsafe {
            crate::arch::simd::save_state(&mut self.simd_states[slot]);
        }
    }

    #[inline]
    fn restore_slot_simd_state(&mut self, slot: usize) {
        #[cfg(not(test))]
        runqueue::simd_tls::restore_simd_state(slot);
        #[cfg(test)]
        unsafe {
            crate::arch::simd::restore_state(&self.simd_states[slot]);
        }
    }

    #[inline]
    fn slot_affinity_snapshot(&self, slot: usize) -> (u64, u64, bool) {
        #[cfg(not(test))]
        {
            runqueue::affinity_payload::affinity_snapshot(slot)
        }
        #[cfg(test)]
        {
            (
                self.task_affinity_masks[slot],
                self.process_affinity_masks[slot],
                self.affinity_migration_pending[slot],
            )
        }
    }

    #[inline]
    fn replace_slot_affinity(
        &mut self,
        slot: usize,
        task_mask: u64,
        process_mask: u64,
        migration_pending: bool,
    ) {
        #[cfg(not(test))]
        runqueue::affinity_payload::set_affinity(slot, task_mask, process_mask, migration_pending);
        #[cfg(test)]
        {
            self.task_affinity_masks[slot] = task_mask;
            self.process_affinity_masks[slot] = process_mask;
            self.affinity_migration_pending[slot] = migration_pending;
        }
    }

    #[inline]
    fn initialize_slot_affinity_payload(&mut self, slot: usize, task_mask: u64, process_mask: u64) {
        #[cfg(not(test))]
        runqueue::affinity_payload::initialize_affinity(slot, task_mask, process_mask);
        #[cfg(test)]
        self.replace_slot_affinity(slot, task_mask, process_mask, false);
    }

    #[inline]
    fn slot_last_cpu(&self, slot: usize) -> u8 {
        #[cfg(not(test))]
        {
            runqueue::affinity_payload::last_cpu(slot)
        }
        #[cfg(test)]
        {
            self.task_last_cpu[slot]
        }
    }

    #[inline]
    fn record_slot_last_cpu(&mut self, slot: usize, cpu: u8) {
        #[cfg(not(test))]
        runqueue::affinity_payload::record_last_cpu(slot, cpu);
        #[cfg(test)]
        {
            self.task_last_cpu[slot] = cpu;
        }
    }

    #[inline]
    fn add_slot_vruntime(&mut self, slot: usize, delta: u64) -> u64 {
        #[cfg(not(test))]
        {
            runqueue::add_vruntime(slot, delta)
        }
        #[cfg(test)]
        {
            let context = self.contexts[slot]
                .as_mut()
                .expect("scheduler vruntime charge missing task context");
            context.vruntime_ns = context.vruntime_ns.saturating_add(delta);
            context.vruntime_ns
        }
    }

    #[inline]
    fn raise_slot_vruntime_floor(&mut self, slot: usize, floor: u64) -> u64 {
        #[cfg(not(test))]
        {
            runqueue::raise_vruntime_floor(slot, floor)
        }
        #[cfg(test)]
        {
            let context = self.contexts[slot]
                .as_mut()
                .expect("scheduler sleeper floor missing task context");
            context.vruntime_ns = context.vruntime_ns.max(floor);
            context.vruntime_ns
        }
    }

    #[inline]
    fn lower_slot_vruntime_ceiling(&mut self, slot: usize, ceiling: u64) -> u64 {
        #[cfg(not(test))]
        {
            runqueue::lower_vruntime_ceiling(slot, ceiling)
        }
        #[cfg(test)]
        {
            let context = self.contexts[slot]
                .as_mut()
                .expect("scheduler IPC donation missing task context");
            context.vruntime_ns = context.vruntime_ns.min(ceiling);
            context.vruntime_ns
        }
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
        }
        #[cfg(test)]
        for state in &mut self.sync_handoff_states {
            state.lock().remove_slot(slot);
        }
        #[cfg(not(test))]
        sync_handoff::remove_slot_all_cpus(slot);
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

    /// Performs one synchronous-IPC call handoff as a single scheduler
    /// mutation: reply-donation bind, receiver wake, and the direct pick hint.
    ///
    /// These ran as three separate global-scheduler acquisitions on the IPC
    /// call path, in this exact order. Each acquisition pays the tracked-lock
    /// prologue, and the measured cost of the three was about 18.5k cycles per
    /// call against a ~400k-cycle round trip. Fusing them changes neither the
    /// operations nor their order; it removes two acquisitions of the most
    /// contended lock in the system from the hottest path that takes it.
    pub(super) fn commit_ipc_call_handoff(
        &mut self,
        reply: u64,
        donor_task_id: u64,
        receiver_task_id: u64,
        donation_required: bool,
    ) -> IpcCallHandoffOutcome {
        let inherited = if donation_required {
            self.bind_reserved_ipc_priority(reply, donor_task_id, receiver_task_id)
        } else {
            true
        };
        let woke = self.wake_task(receiver_task_id);
        let hinted = self.set_next_synchronous_pick_hint(receiver_task_id);
        IpcCallHandoffOutcome {
            inherited,
            woke,
            hinted,
        }
    }

    /// Atomically replaces the current sender with one exact receive-blocked
    /// peer on the same CPU. The fixed IPC frame is already reserved, but no
    /// receiver may observe it until this transaction succeeds. Every reject
    /// leaves both scheduler contexts unchanged so the caller can rollback the
    /// frame and restart through the ordinary endpoint slowpath.
    pub(super) fn commit_fast_ipc_call_handoff(
        &mut self,
        endpoint: u64,
        reply: u64,
        receiver_task_id: u64,
    ) -> FastIpcCallHandoffOutcome {
        let sender_slot = self.current_task_slot();
        let sender_matches = !self.retired[sender_slot]
            && !self.start_suspended[sender_slot]
            && self.contexts[sender_slot].is_some_and(|context| {
                !context.blocked
                    && context.wake_armed
                    && context.block_reason == BlockReason::EndpointReply(reply)
            });
        if endpoint == 0 || reply == 0 || !sender_matches {
            return FastIpcCallHandoffOutcome::SenderMismatch;
        }
        let Some(receiver_slot) = self.find_task_slot(receiver_task_id) else {
            return FastIpcCallHandoffOutcome::ReceiverMismatch;
        };
        if receiver_slot == sender_slot
            || self.retired[receiver_slot]
            || self.start_suspended[receiver_slot]
            || !self.contexts[receiver_slot].is_some_and(|context| {
                context.blocked
                    && !context.wake_armed
                    && context.block_reason == BlockReason::EndpointReceive(endpoint)
            })
        {
            return FastIpcCallHandoffOutcome::ReceiverMismatch;
        }
        let current_cpu = Self::current_dispatch_cpu();
        let target_cpu = self.slot_dispatch_cpu(receiver_slot);
        #[cfg(not(test))]
        {
            if !self.context_is_dispatch_eligible_on_cpu(
                receiver_slot,
                self.contexts[receiver_slot].expect("validated fast IPC receiver lost context"),
                target_cpu,
            ) {
                return FastIpcCallHandoffOutcome::DirectCustodyUnavailable;
            }
        }
        let sender_task_id = self.starts[sender_slot]
            .expect("validated fast IPC sender lost identity")
            .id;
        if !self.bind_reserved_ipc_priority(reply, sender_task_id, receiver_task_id) {
            return FastIpcCallHandoffOutcome::DonationUnavailable;
        }

        if target_cpu == current_cpu {
            #[cfg(not(test))]
            if !matches!(
                runqueue::publish_direct_handoff(receiver_slot, current_cpu),
                runqueue::RemoteWakeOutcome::Published { .. }
            ) {
                return FastIpcCallHandoffOutcome::DirectCustodyUnavailable;
            }
            if !self.enqueue_synchronous_handoff_slot(receiver_slot) {
                #[cfg(not(test))]
                assert!(
                    runqueue::rollback_direct_handoff(receiver_slot, current_cpu),
                    "fast IPC ordering rejection lost direct receiver custody"
                );
                return FastIpcCallHandoffOutcome::OrderingUnavailable;
            }
        } else {
            #[cfg(not(test))]
            if !matches!(
                runqueue::publish_remote_wake(
                    receiver_slot,
                    target_cpu,
                    self.contexts[receiver_slot]
                        .expect("validated fast IPC receiver lost context")
                        .weight,
                ),
                runqueue::RemoteWakeOutcome::Published { .. }
            ) {
                return FastIpcCallHandoffOutcome::DirectCustodyUnavailable;
            }
            assert!(
                self.enqueue_synchronous_handoff_slot(receiver_slot),
                "cross-CPU fast IPC RunTransfer lost bounded ordering custody"
            );
            #[cfg(not(test))]
            super::irq::request_target_reschedule(target_cpu);
        }

        let receiver = self.contexts[receiver_slot]
            .as_mut()
            .expect("validated fast IPC receiver lost context");
        receiver.blocked = false;
        receiver.blocked_since_ticks = 0;
        receiver.block_reason = BlockReason::None;
        receiver.ready_since_ticks = Self::ready_since_now_ticks();
        #[cfg(test)]
        {
            receiver.test_ready = true;
        }

        let sender = self.contexts[sender_slot]
            .as_mut()
            .expect("validated fast IPC sender lost context");
        sender.wake_armed = false;
        sender.blocked = true;
        sender.blocked_since_ticks = crate::arch::rtc::ticks();
        sender.ready_since_ticks = 0;
        #[cfg(test)]
        {
            sender.test_ready = false;
        }
        #[cfg(not(test))]
        runqueue::set_runnable(sender_slot, false);
        if target_cpu == current_cpu {
            FastIpcCallHandoffOutcome::CommittedSameCpu
        } else {
            FastIpcCallHandoffOutcome::CommittedCrossCpu
        }
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
        #[cfg(not(test))]
        let target_cpu = self.slot_dispatch_cpu(slot);
        let _ = self.enqueue_synchronous_handoff_slot(slot);
        #[cfg(not(test))]
        super::irq::request_target_reschedule(target_cpu);
        self.starts[slot].map(|start| start.id)
    }

    fn pick_hint_candidate_slot(&self, hint: Option<usize>) -> Option<usize> {
        let slot = hint?;
        if slot >= MAX_TASK {
            return None;
        }
        // A hint only suggests an ordering. The same lifecycle/queue admission
        // gate as every other handoff remains authoritative, so a stale hint
        // cannot resurrect a blocked, retired, or non-runnable slot.
        if !self.handoff_slot_ready(slot) {
            return None;
        }
        let context = self.contexts[slot]?;
        if !self.context_is_schedulable(slot, context) {
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

    fn take_next_latency_pick_hint_ready_slot(
        &self,
        policy: &mut CpuDispatchGuard<'_>,
    ) -> Option<usize> {
        if policy.latency_handoff_streak >= MAX_CONSECUTIVE_LATENCY_HANDOFFS {
            return None;
        }
        while policy.latency_pick_hint_len != 0 {
            let index = policy.latency_pick_hint_head;
            let hint = policy.latency_pick_hints[index].take();
            policy.latency_pick_hint_head = (index + 1) % MAX_LATENCY_HANDOFF_HINTS;
            policy.latency_pick_hint_len -= 1;
            if let Some(slot) = self.pick_hint_candidate_slot(hint) {
                return Some(slot);
            }
        }
        None
    }

    fn take_next_pick_hint_ready_slot(&self, policy: &mut CpuDispatchGuard<'_>) -> Option<usize> {
        let hint = policy.next_pick_hint;
        if hint.is_some() && self.pick_hint_candidate_slot(hint).is_none() {
            policy.next_pick_hint = None;
            return None;
        }
        let slot = self.pick_hint_ready_slot(hint)?;
        policy.next_pick_hint = None;
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

        #[cfg(test)]
        {
            self.simd_states = [SimdState::new(); MAX_TASK];
        }
        self.retired = [false; MAX_TASK];
        self.retirement_cleanup = [None; MAX_TASK];
        self.retirement_cleanup_claimed = [false; MAX_TASK];
        self.retirement_side_effects = [None; MAX_TASK];
        self.deferred_retire_reasons = [None; MAX_TASK];
        self.thread_slot_reserved = [false; MAX_TASK];
        self.start_suspended = [false; MAX_TASK];
        self.job_stopped = [false; MAX_TASK];
        self.retire_reasons = [None; MAX_TASK];
        self.idle_cpu = [NO_IDLE_CPU; MAX_TASK];
        #[cfg(test)]
        {
            self.task_affinity_masks = [UNRESTRICTED_CPU_MASK; MAX_TASK];
            self.process_affinity_masks = [UNRESTRICTED_CPU_MASK; MAX_TASK];
            self.affinity_migration_pending = [false; MAX_TASK];
            self.task_last_cpu = [NO_IDLE_CPU; MAX_TASK];
        }
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
        self.runtime_profile_lock_hold_max_caller = None;
        self.runtime_profile_lock_hold_max_attributed_ns = 0;
        self.runtime_profile_deferred_wakes = 0;
        self.set_current_task_slot(ROOT_TASK_SLOT);
        self.pending_reap = false;
        self.task_slot_hints = [const { AtomicU8::new(TASK_SLOT_HINT_EMPTY) }; MAX_TASK];
        for policy in &self.cpu_dispatch {
            *policy.lock() = CpuDispatchPolicy::new();
        }
        #[cfg(test)]
        {
            for state in &self.sync_handoff_states {
                *state.lock() = sync_handoff::SyncHandoffState::new();
            }
        }
        #[cfg(not(test))]
        sync_handoff::reset_all_cpus();
        #[cfg(not(test))]
        donation_ledger::reset();
        #[cfg(test)]
        {
            self.ipc_priority_donations = [None; MAX_TASK];
            self.ipc_priority_donation_len = 0;
            self.donation_donor_slot_hints = [const { AtomicU8::new(NO_SLOT_HINT) }; MAX_TASK];
        }
        self.root_idle = false;
        self.scheduler_tick_divisor = main_thread_pit_divisor;
        self.reset_stack_storage(ROOT_TASK_SLOT)
            .expect("scheduler root stack allocation failed");
        let (kernel_stack_base, kernel_stack_top) = self.stack_bounds(ROOT_TASK_SLOT);
        let root_exec_start_ticks = crate::arch::rtc::ticks();
        let root_saved_rsp = self.init_kernel_entry_context(
            ROOT_TASK_SLOT,
            kernel_cs,
            kernel_ss,
            rflags,
            kernel_task_entry_rip,
            0,
        );
        self.contexts[ROOT_TASK_SLOT] = Some(TaskContext {
            scheduling_context: scheduling_context::SchedulingContext::bind(ROOT_TASK_SLOT, id),
            #[cfg(test)]
            saved_rsp: root_saved_rsp,
            #[cfg(test)]
            test_ready: true,
            ready_since_ticks: crate::arch::rtc::ticks(),
            blocked: false,
            blocked_since_ticks: 0,
            wake_armed: false,
            block_reason: BlockReason::None,
            weight: Self::weight_from_pit_divisor(main_thread_pit_divisor),
            #[cfg(test)]
            vruntime_ns: 0,
            #[cfg(test)]
            exec_start_ticks: root_exec_start_ticks,
            address_space_root: crate::memory::paging::kernel_root_phys().as_u64(),
            #[cfg(test)]
            kernel_stack_base: kernel_stack_base as u64,
            #[cfg(test)]
            kernel_stack_top: kernel_stack_top as u64,
            #[cfg(test)]
            alternate_kernel_stack_base: 0,
            #[cfg(test)]
            alternate_kernel_stack_top: 0,
            user_mode: false,
            user_abi: None,
            console_session: ConsoleSessionHandle::SYSTEM,
            process_handle: None,
            process_id: None,
            user_stack: None,
            windows_thread_state: None,
        });
        self.initialize_slot_vruntime(ROOT_TASK_SLOT, 0);
        self.initialize_slot_exec_start_ticks(ROOT_TASK_SLOT, root_exec_start_ticks);
        self.initialize_slot_saved_rsp(ROOT_TASK_SLOT, root_saved_rsp);
        self.initialize_slot_kernel_stack_bounds(
            ROOT_TASK_SLOT,
            kernel_stack_base as u64,
            kernel_stack_top as u64,
        );
        self.initialize_slot_alternate_kernel_stack_bounds(ROOT_TASK_SLOT);
        self.initialize_slot_simd_state(ROOT_TASK_SLOT);
        self.starts[ROOT_TASK_SLOT] = Some(TaskStart { entry, id });
        self.publish_slot_identity(ROOT_TASK_SLOT);
        #[cfg(not(test))]
        runqueue::admit_running(ROOT_TASK_SLOT, 0);
        nucleus_core::util::lockdep::set_current_task_owner(
            id.checked_add(1)
                .expect("root task id exhausted lock owner token"),
        );

        self.save_slot_simd_state(ROOT_TASK_SLOT);
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
        let (stack_base, stack_top) = context
            .map(|_| self.slot_kernel_stack_bounds(slot))
            .unwrap_or((0, 0));
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
        self.retirement_cleanup_claimed[slot] = false;
        self.deferred_retire_reasons[slot] = None;
        self.exec_target_quiesced[slot] = false;
        self.start_suspended[slot] = false;
        self.job_stopped[slot] = false;
        self.retire_reasons[slot] = None;
        self.reset_slot_simd_state(slot);
        self.starts[slot] = None;
        self.publish_slot_identity(slot);
        self.idle_cpu[slot] = NO_IDLE_CPU;
        #[cfg(test)]
        self.replace_slot_affinity(slot, UNRESTRICTED_CPU_MASK, UNRESTRICTED_CPU_MASK, false);
        #[cfg(test)]
        self.record_slot_last_cpu(slot, NO_IDLE_CPU);
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

        #[cfg(test)]
        {
            context.saved_rsp = saved_rsp;
        }
        if ready && context.ready_since_ticks == 0 {
            context.ready_since_ticks = Self::ready_since_now_ticks();
        } else if !ready {
            context.ready_since_ticks = 0;
        }
        #[cfg(test)]
        {
            context.test_ready = ready;
        }
        // This funnel owns the explicit turn-boundary transition. Direct
        // lifecycle writes below intentionally do not mirror the legacy field:
        // `ready = false` historically meant both "not queued" and "blocked",
        // while a `Running` owner must retain run intent until its block
        // transaction commits.
        #[cfg(not(test))]
        runqueue::set_runnable(slot, ready);
        self.set_slot_saved_rsp(slot, saved_rsp);
    }

    /// Queue-age zero is the exact not-queued sentinel, including during the
    /// earliest boot ticks where the RTC itself can still report zero.
    #[inline]
    fn ready_since_now_ticks() -> u64 {
        crate::arch::rtc::ticks().max(1)
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
    ///
    /// The single caller is `new_task_vruntime`, which runs on the spawning
    /// CPU with the scheduler owner held, so the scan sees exactly the
    /// runnable set the new task is about to join.
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
            if !self.slot_is_runnable(slot) {
                continue;
            }
            if !self.context_is_schedulable(slot, ctx) {
                continue;
            }
            let v = self.slot_vruntime(slot);
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
            if !self.slot_is_runnable(slot) {
                continue;
            }
            if !self.context_is_schedulable(slot, ctx) {
                continue;
            }
            if self.slot_class(slot) != Some(class) {
                continue;
            }
            let v = self.slot_vruntime(slot);
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
        #[cfg(not(test))]
        {
            let base = self.base_slot_class(slot)?;
            return Some(
                if base == SchedClass::User && donation_ledger::inherited_system(slot) {
                    SchedClass::System
                } else {
                    base
                },
            );
        }
        #[cfg(test)]
        {
            // The cycle-breaking set is a bitmap, not `[bool; MAX_TASK]`. Both
            // O(local-runnable) pick scans call this once per candidate on every
            // dispatch, and a byte-per-slot set costs a 128-byte stack clear per
            // call for at most one bit that is ever set outside a donation chain.
            let mut visiting = SlotSet::EMPTY;
            self.effective_slot_class(slot, &mut visiting, 0)
        }
    }

    /// Returns the kernel-derived effective IPC/scheduling class for a live
    /// task. The public boundary exposes only the System predicate so callers
    /// cannot manufacture or persist scheduler-internal class values.
    pub(super) fn task_has_system_scheduling_class(&self, task_id: u64) -> bool {
        self.find_task_slot(task_id).is_some_and(|slot| {
            !self.retired[slot] && self.slot_class(slot) == Some(SchedClass::System)
        })
    }

    #[cfg(test)]
    fn effective_slot_class(
        &self,
        slot: usize,
        visiting: &mut SlotSet,
        depth: usize,
    ) -> Option<SchedClass> {
        let base = self.base_slot_class(slot)?;
        // A System task cannot be promoted further and root-idle must remain
        // an idle fallback even if stale external state tried to reference it.
        //
        // With no live donation there is nothing that could promote this slot,
        // so the walk below has no candidates to examine. The scan is O(slots)
        // and the walk is O(donations) inside it; naming the empty case keeps
        // an idle system from paying the product.
        if base != SchedClass::User
            || self.ipc_priority_donation_len == 0
            || visiting.contains(slot)
        {
            return Some(base);
        }
        // `visiting` breaks cycles but says nothing about depth, and this
        // recurses on the kernel stack inside the global scheduler lock. A
        // chain of `MAX_TASK` donations would be 128 nested frames there.
        // seL4 proves acyclicity and a bounded chain depth as two separate
        // properties for exactly this reason; only the first was expressed
        // here. Truncating returns the base class, which under-promotes rather
        // than over-promotes - the safe direction, since a donation only ever
        // raises urgency.
        if depth >= MAX_IPC_DONATION_CHAIN_DEPTH {
            crate::debug::record_milestone(
                crate::debug::LogCategory::Sched,
                "ipc-donation-chain-truncated",
                slot as u64,
                depth as u64,
            );
            return Some(base);
        }
        visiting.insert(slot);
        let mut effective = base;
        for (index, donation) in self.ipc_priority_donations[..self.ipc_priority_donation_len]
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.as_ref().map(|entry| (index, entry)))
        {
            if !donation.priority_donated || !donation.custody_active {
                continue;
            }
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
            let Some(donor_class) =
                self.effective_slot_class(donor_slot, visiting, depth.saturating_add(1))
            else {
                continue;
            };
            if donor_class < effective {
                effective = donor_class;
            }
        }
        visiting.remove(slot);
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
        if self.contexts[slot].is_none() {
            return;
        }
        let start = self.slot_exec_start_ticks(slot);
        if start == 0 {
            return;
        }
        let elapsed_ns = if now_ticks > start {
            Self::ticks_elapsed_ns(start, now_ticks)
        } else {
            0
        };
        self.account_runtime_profile(slot, elapsed_ns);
        if elapsed_ns != 0 {
            let now_ns = crate::arch::clock::monotonic_nanos();
            if let Some((context_owner_task_id, outcome)) =
                self.charge_effective_scheduling_context_runtime(slot, now_ns, elapsed_ns)
            {
                // Overrun volume is accumulated in the fixed runtime-counter
                // bank and rendered once per scheduler profile window.  A
                // task that remains throttled can reach this branch on every
                // scheduling attempt; publishing one debugcon record per
                // attempt turns diagnostics into the dominant runtime cost.
                // Keep the one-shot transition marker for the charge that
                // actually consumes the final budget quantum.
                if outcome.exhausted && outcome.charged_ns != 0 {
                    nucleus_core::debug::record_milestone(
                        nucleus_core::debug::LogCategory::Sched,
                        "scheduling-budget-exhausted",
                        context_owner_task_id,
                        outcome.charged_ns,
                    );
                }
            }
        }
        let elapsed_ns = if force_min_charge {
            elapsed_ns.max(SCHED_MIN_GRANULARITY_NS)
        } else {
            elapsed_ns
        };
        if elapsed_ns == 0 {
            self.set_slot_exec_start_ticks(slot, 0);
            return;
        }
        let weight = self.contexts[slot]
            .map(|context| context.weight)
            .expect("profiled scheduler task disappeared under scheduler owner");
        let delta = Self::weighted_vruntime_delta(elapsed_ns, weight);
        self.add_slot_vruntime(slot, delta);
        self.set_slot_exec_start_ticks(slot, 0);
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
    fn reserved_user_pick(&self, policy: &CpuDispatchGuard<'_>, current: usize) -> Option<usize> {
        let started_ns = crate::arch::clock::monotonic_nanos();
        let picked = self.reserved_user_pick_inner(policy, current);
        locality::charge_handoff_scan(
            crate::arch::clock::monotonic_nanos().saturating_sub(started_ns),
        );
        picked
    }

    fn reserved_user_pick_inner(
        &self,
        policy: &CpuDispatchGuard<'_>,
        current: usize,
    ) -> Option<usize> {
        Self::user_reservation_due(policy)
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
        let started_ns = Self::phase_chain_start();
        let picked = self.overdue_class_pick_inner(current, now_ticks, class, latency_bound_ms);
        locality::charge_handoff_scan(Self::phase_chain_start().saturating_sub(started_ns));
        picked
    }

    fn overdue_class_pick_inner(
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
            if !self.slot_is_runnable(slot)
                || context.ready_since_ticks == 0
                || !self.context_is_schedulable(slot, context)
                || self.slot_class(slot) != Some(class)
                || Self::ticks_elapsed_ms(context.ready_since_ticks, now_ticks) < latency_bound_ms
            {
                continue;
            }
            let vruntime = self.slot_vruntime(slot);
            let candidate = (slot, context.ready_since_ticks, vruntime);
            match oldest {
                None => oldest = Some(candidate),
                Some((_, oldest_since, oldest_vruntime))
                    if context.ready_since_ticks < oldest_since
                        || (context.ready_since_ticks == oldest_since
                            && vruntime < oldest_vruntime) =>
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

    fn user_reservation_due(policy: &CpuDispatchGuard<'_>) -> bool {
        policy.system_dispatch_streak >= MAX_CONSECUTIVE_SYSTEM_DISPATCHES
    }

    fn record_dispatch_class(&mut self, slot: usize) {
        // The class lookup does not need the policy, and taking it separately
        // for the read and the write charged the dispatch two acquisitions of
        // a lock it holds exclusively either way.
        let class = self.slot_class(slot);
        let mut policy = self.current_dispatch_policy_mut();
        let next = match class {
            Some(SchedClass::System) => policy
                .system_dispatch_streak
                .saturating_add(1)
                .min(MAX_CONSECUTIVE_SYSTEM_DISPATCHES),
            Some(SchedClass::User | SchedClass::Idle) | None => 0,
        };
        policy.system_dispatch_streak = next;
    }

    fn record_latency_handoff(&mut self, latency_handoff: bool) {
        let mut policy = self.current_dispatch_policy_mut();
        let next = if latency_handoff {
            policy
                .latency_handoff_streak
                .saturating_add(1)
                .min(MAX_CONSECUTIVE_LATENCY_HANDOFFS)
        } else {
            0
        };
        policy.latency_handoff_streak = next;
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
        self.retirement_cleanup_claimed[slot] = false;
        if let Some(context) = self.contexts[slot].as_mut() {
            context.ready_since_ticks = 0;
            context.blocked = false;
            context.blocked_since_ticks = 0;
            context.wake_armed = false;
            context.block_reason = BlockReason::None;
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

    /// The first slot an allocation may claim.
    ///
    /// A slot is free only when no context is bound **and** no thread
    /// reservation holds it. `reserve_user_thread_slot` leaves the context
    /// absent while it owns the slot, so testing the context alone hands a
    /// reserved slot to a process allocation and both write the same stack.
    #[cfg(test)]
    pub(super) fn first_allocatable_slot(&self) -> usize {
        (FIRST_DYNAMIC_TASK_SLOT..MAX_TASK)
            .find(|slot| self.contexts[*slot].is_none() && !self.thread_slot_reserved[*slot])
            .unwrap_or(MAX_TASK)
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
            if self.contexts[slot].is_none() {
                return false;
            }
            let Some(raw_base) = self
                .slot_kernel_stack_bounds(slot)
                .0
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
            .map(|_| self.slot_saved_rsp(self.current_task_slot()))
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

        let slot = self.current_task_slot();
        self.contexts[slot]?;
        let previous = self.slot_alternate_kernel_stack_bounds(slot);
        self.replace_slot_alternate_kernel_stack_bounds(slot, base, top);
        Some(previous)
    }

    pub(super) fn restore_current_alternate_kernel_stack(&mut self, previous: (u64, u64)) {
        let slot = self.current_task_slot();
        if self.contexts[slot].is_some() {
            self.replace_slot_alternate_kernel_stack_bounds(slot, previous.0, previous.1);
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
        let Some(_context) = self.contexts.get(slot).and_then(|context| *context) else {
            return false;
        };
        let (kernel_stack_base, kernel_stack_top) = self.slot_kernel_stack_bounds(slot);
        stack_range_contains(kernel_stack_base, kernel_stack_top, start, end) || {
            let (alternate_base, alternate_top) = self.slot_alternate_kernel_stack_bounds(slot);
            stack_range_contains(alternate_base, alternate_top, start, end)
        }
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

    /// Whether the frame at `saved_rsp` is entirely zero.
    ///
    /// A zeroed frame means the stack was cleared and never written, which is a
    /// different fault from a frame that was written and then corrupted. Only
    /// the rejected-activation report calls this.
    fn stack_frame_is_all_zero(&self, saved_rsp: usize) -> bool {
        let Some(saved) = Self::saved_context_ref(saved_rsp) else {
            return false;
        };
        // SAFETY: `saved_context_ref` validated the pointer and alignment, and
        // the frame is `SAVED_CONTEXT_BYTES` of initialized stack storage.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                saved as *const SavedContext as *const u8,
                SAVED_CONTEXT_BYTES,
            )
        };
        bytes.iter().all(|byte| *byte == 0)
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
            if self.contexts[slot].is_none() && !self.thread_slot_reserved[slot] {
                self.reset_stack_storage(slot)?;
                let (kernel_stack_base, kernel_stack_top) = self.stack_bounds(slot);
                let vruntime_ns = self.new_task_vruntime();
                let saved_rsp =
                    self.init_kernel_entry_context(slot, cs, ss, rflags, kernel_task_entry_rip, 0);
                self.contexts[slot] = Some(TaskContext {
                    scheduling_context: scheduling_context::SchedulingContext::bind(slot, id),
                    #[cfg(test)]
                    saved_rsp,
                    #[cfg(test)]
                    test_ready: true,
                    ready_since_ticks: crate::arch::rtc::ticks(),
                    blocked: false,
                    blocked_since_ticks: 0,
                    wake_armed: false,
                    block_reason: BlockReason::None,
                    weight: Self::weight_from_pit_divisor(pit_divisor),
                    #[cfg(test)]
                    vruntime_ns,
                    #[cfg(test)]
                    exec_start_ticks: 0,
                    address_space_root: crate::memory::paging::kernel_root_phys().as_u64(),
                    #[cfg(test)]
                    kernel_stack_base: kernel_stack_base as u64,
                    #[cfg(test)]
                    kernel_stack_top: kernel_stack_top as u64,
                    #[cfg(test)]
                    alternate_kernel_stack_base: 0,
                    #[cfg(test)]
                    alternate_kernel_stack_top: 0,
                    user_mode: false,
                    user_abi: None,
                    console_session: ConsoleSessionHandle::SYSTEM,
                    process_handle: None,
                    process_id: None,
                    user_stack: None,
                    windows_thread_state: None,
                });
                self.initialize_slot_vruntime(slot, vruntime_ns);
                self.initialize_slot_exec_start_ticks(slot, 0);
                self.initialize_slot_saved_rsp(slot, saved_rsp);
                self.initialize_slot_kernel_stack_bounds(
                    slot,
                    kernel_stack_base as u64,
                    kernel_stack_top as u64,
                );
                self.initialize_slot_alternate_kernel_stack_bounds(slot);
                self.initialize_slot_simd_state(slot);
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
    #[cfg(test)]
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
        let spawn_reservation = process_table::reserve_spawn()?;
        let allocation = self.allocate_user_slot_with_scheduling_context(
            id,
            address_space,
            bootstrap,
            parent_process_id,
            pit_divisor,
            user_cs,
            user_ss,
            rflags,
            start_suspended,
            idle_entry,
            None,
            spawn_reservation,
        );
        if allocation.is_none() {
            let _ = process_table::cancel_spawn(spawn_reservation);
        }
        allocation
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn allocate_user_slot_with_scheduling_context(
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
        admission: Option<super::SchedulingContextAdmission>,
        spawn_reservation: process_table::SpawnReservation,
    ) -> Option<usize> {
        let inherited_process_mask = self.inherited_process_affinity(parent_process_id);
        let scheduling_policy = admission
            .map(|admission| scheduling_context::SchedulingContextPolicy {
                budget_ns: admission.budget_ns,
                period_ns: admission.period_ns,
                refill_capacity: admission.refill_capacity,
                cpu_mask: admission.cpu_mask,
                criticality: admission.criticality,
                domain: admission.domain,
                policy_epoch: admission.policy_epoch,
                timeout_endpoint_cap: admission.timeout_endpoint_cap,
            })
            .or_else(|| {
                parent_process_id.and_then(|parent| {
                    self.contexts.iter().flatten().find_map(|context| {
                        (context.process_id == Some(parent))
                            .then(|| context.scheduling_context.policy())
                            .flatten()
                    })
                })
            });
        #[cfg(not(test))]
        if scheduling_policy.is_none() {
            return None;
        }
        if scheduling_policy.is_some_and(|policy| !policy.is_valid()) {
            return None;
        }
        let scheduling_domain_slot = match scheduling_policy {
            Some(policy) => Some(self.admit_scheduling_domain(policy)?),
            None => None,
        };
        let admitted_task_mask = scheduling_policy
            .map(|policy| inherited_process_mask & policy.cpu_mask)
            .unwrap_or(inherited_process_mask);
        if admitted_task_mask == 0 {
            return None;
        }
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            if self.contexts[slot].is_none() && !self.thread_slot_reserved[slot] {
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
                let process_handle = process_table::publish_spawn(
                    spawn_reservation,
                    id,
                    parent_process_id,
                    boxed_state,
                )?;
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

                let vruntime_ns = self.new_task_vruntime();
                let mut scheduling_context = scheduling_context::SchedulingContext::bind(slot, id);
                if let Some(policy) = scheduling_policy
                    && !scheduling_context.admit(
                        policy,
                        scheduling_domain_slot
                            .expect("budgeted context lost admitted scheduling domain"),
                    )
                {
                    unreachable!("validated scheduling-context policy failed admission");
                }
                self.contexts[slot] = Some(TaskContext {
                    scheduling_context,
                    #[cfg(test)]
                    saved_rsp,
                    #[cfg(test)]
                    test_ready: !start_suspended,
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
                    block_reason: BlockReason::None,
                    weight: Self::weight_from_pit_divisor(pit_divisor),
                    #[cfg(test)]
                    vruntime_ns,
                    #[cfg(test)]
                    exec_start_ticks: 0,
                    address_space_root: root_phys,
                    #[cfg(test)]
                    kernel_stack_base: kernel_stack_base as u64,
                    #[cfg(test)]
                    kernel_stack_top: kernel_stack_top as u64,
                    #[cfg(test)]
                    alternate_kernel_stack_base: 0,
                    #[cfg(test)]
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
                self.initialize_slot_vruntime(slot, vruntime_ns);
                self.initialize_slot_exec_start_ticks(slot, 0);
                self.initialize_slot_saved_rsp(slot, saved_rsp);
                self.initialize_slot_kernel_stack_bounds(
                    slot,
                    kernel_stack_base as u64,
                    kernel_stack_top as u64,
                );
                self.initialize_slot_alternate_kernel_stack_bounds(slot);
                self.initialize_slot_simd_state(slot);
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
                self.initialize_slot_affinity(slot, admitted_task_mask, inherited_process_mask);
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
        start_suspended: bool,
        idle_entry: fn(u64),
        spawn_reservation: process_table::SpawnReservation,
    ) -> Option<usize> {
        let inherited_process_mask = self.inherited_process_affinity(parent_process_id);
        let scheduling_policy = parent_process_id.and_then(|parent| {
            self.contexts.iter().flatten().find_map(|context| {
                (context.process_id == Some(parent))
                    .then(|| context.scheduling_context.policy())
                    .flatten()
            })
        });
        #[cfg(not(test))]
        if scheduling_policy.is_none() {
            return None;
        }
        let scheduling_domain_slot = match scheduling_policy {
            Some(policy) => Some(self.admit_scheduling_domain(policy)?),
            None => None,
        };
        let admitted_task_mask = scheduling_policy
            .map(|policy| inherited_process_mask & policy.cpu_mask)
            .unwrap_or(inherited_process_mask);
        if admitted_task_mask == 0 {
            return None;
        }
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            if self.contexts[slot].is_none() && !self.thread_slot_reserved[slot] {
                self.reset_stack_storage(slot)?;
                let saved_rsp =
                    self.init_user_task_context(slot, &bootstrap, user_cs, user_ss, rflags);
                let root_phys = process_state.address_space_root();
                let process_handle = process_table::publish_spawn(
                    spawn_reservation,
                    id,
                    parent_process_id,
                    process_state,
                )?;
                let (kernel_stack_base, kernel_stack_top) = self.stack_bounds(slot);
                let vruntime_ns = self.new_task_vruntime();
                let mut scheduling_context = scheduling_context::SchedulingContext::bind(slot, id);
                if let Some(policy) = scheduling_policy
                    && !scheduling_context.admit(
                        policy,
                        scheduling_domain_slot
                            .expect("forked context lost admitted scheduling domain"),
                    )
                {
                    unreachable!("inherited scheduling-context policy failed admission");
                }
                self.contexts[slot] = Some(TaskContext {
                    scheduling_context,
                    #[cfg(test)]
                    saved_rsp,
                    #[cfg(test)]
                    test_ready: !start_suspended,
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
                    block_reason: BlockReason::None,
                    weight: Self::weight_from_pit_divisor(pit_divisor),
                    #[cfg(test)]
                    vruntime_ns,
                    #[cfg(test)]
                    exec_start_ticks: 0,
                    address_space_root: root_phys,
                    #[cfg(test)]
                    kernel_stack_base: kernel_stack_base as u64,
                    #[cfg(test)]
                    kernel_stack_top: kernel_stack_top as u64,
                    #[cfg(test)]
                    alternate_kernel_stack_base: 0,
                    #[cfg(test)]
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
                self.initialize_slot_vruntime(slot, vruntime_ns);
                self.initialize_slot_exec_start_ticks(slot, 0);
                self.initialize_slot_saved_rsp(slot, saved_rsp);
                self.initialize_slot_kernel_stack_bounds(
                    slot,
                    kernel_stack_base as u64,
                    kernel_stack_top as u64,
                );
                self.initialize_slot_alternate_kernel_stack_bounds(slot);
                self.initialize_slot_simd_state(slot);
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
                self.initialize_slot_affinity(slot, admitted_task_mask, inherited_process_mask);
                self.start_suspended[slot] = start_suspended;
                #[cfg(not(test))]
                self.admit_runqueue_slot(slot, !start_suspended);
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
            context.ready_since_ticks = 0;
            context.blocked = false;
            context.blocked_since_ticks = 0;
            context.wake_armed = false;
            context.block_reason = BlockReason::None;
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
            context
                .map(|context| context.ready_since_ticks != 0)
                .unwrap_or(false),
            context.map(|context| context.blocked).unwrap_or(false),
            context.map(|context| context.user_mode).unwrap_or(false),
            context
                .map(|_| self.slot_kernel_stack_bounds(slot).0)
                .unwrap_or(0),
            context
                .map(|_| self.slot_kernel_stack_bounds(slot).1)
                .unwrap_or(0),
            context
                .map(|_| self.slot_alternate_kernel_stack_bounds(slot).0)
                .unwrap_or(0),
            context
                .map(|_| self.slot_alternate_kernel_stack_bounds(slot).1)
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
                context.ready_since_ticks != 0,
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
                context.ready_since_ticks != 0,
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
        let mut phase_marker = Self::phase_chain_start();
        #[cfg(not(test))]
        let current_cpu = dispatch_cpu;
        let current_slot = self.current_task_slot();
        let current_task_id = self.starts[current_slot]
            .map(|start| start.id)
            .expect("running task missing scheduler identity");
        let now_ticks = crate::arch::rtc::ticks();
        let current_runtime_ns = self.contexts[current_slot]
            .map(|_| self.slot_exec_start_ticks(current_slot))
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

        // A direct synchronous IPC record is already an exact, bounded next
        // owner.  Select it before generic runqueue maintenance so a hit does
        // not pay work that cannot affect that selection.  Scheduler is held
        // across this snapshot, so an atomic activation cannot appear between
        // the priority check and the later commit.
        let atomic_activation_pending =
            dispatch_policy::atomic_activation_pending(Self::current_dispatch_cpu());

        #[cfg(not(test))]
        {
            let owner = runqueue::owner(current_slot);
            if owner.state == runqueue::RunOwnerState::Running {
                let weight = self.contexts[current_slot]
                    .map(|context| context.weight)
                    .unwrap_or(NICE_0_LOAD);
                let (task_mask, process_mask, _) = self.slot_affinity_snapshot(current_slot);
                let current_affinity = task_mask & process_mask;
                let current_cpu_is_eligible = current_affinity & (1_u64 << current_cpu) != 0;
                // The owner word answers this. `owner.runnable` is Linux's
                // `p->on_rq` for an executing task, and this turn's prologue has
                // already recomputed it a few lines above: `mark_slot_ready`
                // sets `ready = !retired && !blocked && the frame validates`,
                // and mirrors that into the word in the same call. Reading the
                // word here is therefore reading this turn's decision, not the
                // previous one.
                //
                // An earlier attempt converted this site while the mirror had
                // been moved out of `mark_slot_ready`, so the bit was one
                // prologue stale; it corrupted a kernel stack guard at 8 vCPU,
                // which is what a task dispatched on two CPUs looks like. The
                // freshness of this read is the whole precondition.
                if self.retired[current_slot] || !owner.runnable {
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
        }

        let sync_handoff = (!atomic_activation_pending)
            .then(|| timed_handoff_step(1, || self.take_next_synchronous_pick_hint_ready_slot()))
            .flatten();

        #[cfg(not(test))]
        if sync_handoff.is_none() {
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
        if sync_handoff.is_none() && self.periodic_ready_validation_due() {
            self.retire_invalid_ready_tasks();
        }
        #[cfg(test)]
        self.assert_live_task_state_partition();
        self.mark_phase(SchedulerPhase::Validate, &mut phase_marker);

        // The new-task vruntime floor is read at spawn and nowhere else, so it
        // is computed at spawn. Refreshing it here charged every dispatch an
        // O(local-runnable) scan plus a policy acquisition -- 998ns of a 7.8us
        // owner turn, 14 percent of all attributed in-lock work -- to keep a
        // cache warm for an event that happens thousands of times less often.
        // Computing it on demand also makes it exact rather than as stale as
        // this CPU's previous dispatch.
        #[cfg(not(test))]
        if sync_handoff.is_none() {
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
        // Sync handoff custody is deliberately separate from the remaining
        // CPU dispatch policy.  Drop that policy guard before consuming the
        // external FIFO: reply producers take only SyncHandoff, so this keeps
        // the production order Scheduler -> SyncHandoff with no reverse or
        // same-class nested acquisition.
        // The guarded check below is what decides; this only answers "is it
        // worth taking the lock to ask", and on about 96 percent of dispatches
        // it is not. A stale `true` just takes the lock, as every dispatch did
        // before, so nothing here can admit an activation that is not queued.
        let atomic_activation_handoff = atomic_activation_pending
            .then(|| {
                let mut policy = timed_handoff_step(6, || self.current_dispatch_policy());
                timed_handoff_step(0, || {
                    self.take_next_atomic_activation_handoff_ready_slot(&mut policy)
                })
            })
            .flatten();
        debug_assert!(
            atomic_activation_handoff.is_none() || sync_handoff.is_none(),
            "atomic activation and synchronous IPC were selected in one dispatch"
        );
        #[cfg(rustos_scheduler_phase_profile)]
        if sync_handoff.is_some() {
            locality::record_sync_handoff_hit();
        }
        // A committed direct IPC handoff has already selected its exact peer
        // from the separate per-CPU custody FIFO. Taking SchedulerPolicy here
        // cannot change that choice; it only charged every fast call/reply an
        // unrelated lock acquisition. Acquire the policy lazily only for the
        // ordinary selection tree that actually reads it.
        let mut policy = None;
        let (next_idx, ipc_handoff, reserved_user_pick, latency_handoff_pick, sync_handoff_pick) =
            match atomic_activation_handoff {
                Some(child_slot) => (child_slot, true, None, false, false),
                None => match sync_handoff {
                    Some(peer_slot) => (peer_slot, true, None, false, true),
                    None => {
                        let policy =
                            policy.insert(timed_handoff_step(6, || self.current_dispatch_policy()));
                        let mandatory_overdue = timed_handoff_step(2, || {
                            self.mandatory_overdue_system_pick(current_slot, now_ticks)
                        });
                        let bootstrap_handoff = if mandatory_overdue.is_none() {
                            timed_handoff_step(3, || {
                                self.take_next_bootstrap_handoff_ready_slot(policy)
                            })
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
                                            .then(|| {
                                                timed_handoff_step(4, || {
                                                    self.take_next_pick_hint_ready_slot(policy)
                                                })
                                            })
                                            .flatten();
                                        let (next_idx, ipc_handoff, reserved_user_pick) =
                                            match blocking_ipc_handoff {
                                                Some(receiver_slot) => (receiver_slot, true, None),
                                                None => {
                                                    match self
                                                        .reserved_user_pick(policy, current_slot)
                                                    {
                                                        Some(user_slot) => {
                                                            (user_slot, false, Some(user_slot))
                                                        }
                                                        None => {
                                                            // Overdue System work was already
                                                            // offered `mandatory_overdue`, from
                                                            // the same scan with the same
                                                            // arguments, and reaching here means
                                                            // it found nothing. Nothing between
                                                            // the two touches readiness, class,
                                                            // or the local queue, so a second
                                                            // scan can only return None; it cost
                                                            // 1.0 us of every dispatch. The
                                                            // ordering it expressed is stronger
                                                            // now, not weaker: overdue is offered
                                                            // before the hint and before the
                                                            // bootstrap and reservation arms too.
                                                            let overdue_or_hint =
                                                                timed_handoff_step(5, || {
                                                                    self.take_next_pick_hint_ready_slot(
                                                                        policy,
                                                                    )
                                                                });
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

        drop(policy);

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
            let next_saved_rsp = self.slot_saved_rsp(next_idx);
            match self.context_validation_error(next_idx, next, next_saved_rsp) {
                None => {
                    let context_owner_slot = self.effective_scheduling_context_owner_slot(next_idx);
                    if self.contexts[context_owner_slot]
                        .is_some_and(|context| context.scheduling_context.is_budgeted())
                    {
                        let now_ns = crate::arch::clock::monotonic_nanos();
                        let policy = self.contexts[context_owner_slot]
                            .and_then(|context| context.scheduling_context.policy())
                            .expect("budgeted scheduling context lost its policy");
                        let domain_slot = self.contexts[context_owner_slot]
                            .and_then(|context| context.scheduling_context.domain_slot())
                            .expect("budgeted scheduling context lost its domain slot");
                        assert!(
                            self.prepare_scheduling_domain_dispatch(domain_slot, policy, now_ns),
                            "scheduler selected a task without eligible domain budget slot={next_idx}"
                        );
                        assert!(
                            self.contexts[context_owner_slot]
                                .as_mut()
                                .expect("selected scheduling context disappeared")
                                .scheduling_context
                                .prepare_dispatch(now_ns),
                            "scheduler selected a task without eligible budget slot={next_idx}"
                        );
                    }
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
                        context.ready_since_ticks = 0;
                        #[cfg(test)]
                        {
                            context.test_ready = false;
                        }
                    }
                    self.set_slot_exec_start_ticks(next_idx, now_ticks);
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
                        next_saved_rsp,
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
                        next_saved_rsp,
                        self.scheduler_tick_divisor,
                        current_slot,
                        next_idx,
                    );
                }
                Some(reason) if next_idx == ROOT_TASK_SLOT => {
                    self.log_invalid_context(next_idx, next_saved_rsp, reason, "next");
                    panic!("scheduler root kernel context is corrupted: {}", reason);
                }
                Some(reason) => {
                    self.log_invalid_context(next_idx, next_saved_rsp, reason, "next");
                    self.retire_slot_due_to_invalid_context(next_idx, next_saved_rsp, reason);
                }
            }
        }

        #[cfg(not(test))]
        let current = self.contexts[current_slot].expect("scheduler lost the current task context");
        let current_saved_rsp = self.slot_saved_rsp(current_slot);
        #[cfg(not(test))]
        assert!(
            runqueue::claim_dispatch(current_slot, dispatch_cpu, current.weight),
            "scheduler fallback task lost local rq custody slot={} cpu={}",
            current_slot,
            dispatch_cpu
        );
        // Keep running current: refresh its exec_start_ticks so subsequent
        // vruntime accounting sees a non-zero baseline.
        if self.contexts[current_slot].is_some() {
            self.set_slot_exec_start_ticks(current_slot, now_ticks);
        }
        nucleus_core::util::lockdep::record_scheduler_dispatch(
            nucleus_core::util::lockdep::current_cpu_index(),
            current_task_id,
            current_task_id,
            current_slot,
            current_slot,
            current_saved_rsp,
            self.slot_class(current_slot) == Some(SchedClass::Idle),
            false,
        );
        self.record_runtime_profile_dispatch(current_slot);
        self.record_runtime_profile_transition(current_slot, current_slot, dispatch_cpu);
        self.record_task_dispatch_cpu(current_slot, dispatch_cpu);
        self.mark_phase(SchedulerPhase::Commit, &mut phase_marker);
        SchedulerDispatch::new(
            current_saved_rsp,
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
        // Host unit schedulers do not publish global owner words. Preserve
        // their isolation by adapting the legacy bit only under `cfg(test)`;
        // the production fairness decision takes the owner word's run-intent
        // bit after this turn's prologue has published it.
        #[cfg(test)]
        let current_has_run_intent = current_ctx.test_ready;
        #[cfg(not(test))]
        let current_has_run_intent = runqueue::owner_has_run_intent(runqueue::owner(current_slot));
        if !current_has_run_intent || !self.context_is_schedulable(current_slot, current_ctx) {
            return cfs_pick;
        }
        if self.contexts[cfs_pick].is_none() {
            return cfs_pick;
        }

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
        let current_v = self.slot_vruntime(current_slot);
        let pick_v = self.slot_vruntime(cfs_pick);
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
                .and_then(|_| saved_context_rip(self.slot_saved_rsp(from_slot)))
                .unwrap_or(0);
            let to_rip = to_context
                .and_then(|_| saved_context_rip(self.slot_saved_rsp(to_slot)))
                .unwrap_or(0);
            let from_rsp = from_context
                .map(|_| self.slot_saved_rsp(from_slot))
                .unwrap_or(0);
            let to_rsp = to_context
                .map(|_| self.slot_saved_rsp(to_slot))
                .unwrap_or(0);
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
        let (task_mask, process_mask, _) = self.slot_affinity_snapshot(current_slot);
        self.replace_slot_affinity(current_slot, task_mask, process_mask, false);
        let return_to_user = self.context_returns_to_user(current_slot);
        self.validate_saved_context(
            current_slot,
            current.user_mode,
            self.slot_saved_rsp(current_slot),
        )
        .expect("scheduler selected an invalid task context");
        crate::memory::paging::load_address_space_phys(PhysAddr::new(current.address_space_root));
        let (_, kernel_stack_top) = self.slot_kernel_stack_bounds(current_slot);
        if kernel_stack_top != 0 {
            assert_eq!(
                kernel_stack_top & 0xF,
                0,
                "scheduler selected a kernel stack top that violates the x86_64 SysV ABI"
            );
            crate::arch::gdt::set_privilege_stack(kernel_stack_top);
            crate::user::syscall::set_kernel_stack_top(kernel_stack_top);
        }

        let fs_base = self.slot_tls_fs_base(current_slot);
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
        let mut phase_marker = Self::phase_chain_start();
        assert_eq!(
            self.current_task_slot(),
            dispatch.next_slot,
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

    pub(super) fn next_retired_task_cleanup(&mut self) -> Option<super::RetiredTaskCleanup> {
        let slot = (FIRST_DYNAMIC_TASK_SLOT..MAX_TASK).find(|slot| {
            self.retired[*slot]
                && self.retirement_cleanup[*slot].is_some()
                && !self.retirement_cleanup_claimed[*slot]
        })?;
        self.retirement_cleanup_claimed[slot] = true;
        self.retirement_cleanup[slot]
    }

    pub(super) fn complete_retired_task_cleanup(
        &mut self,
        cleanup: super::RetiredTaskCleanup,
    ) -> bool {
        let Some(slot) = (FIRST_DYNAMIC_TASK_SLOT..MAX_TASK).find(|slot| {
            self.retired[*slot]
                && self.retirement_cleanup_claimed[*slot]
                && self.retirement_cleanup[*slot] == Some(cleanup)
        }) else {
            return false;
        };
        self.retirement_cleanup[slot] = None;
        self.retirement_cleanup_claimed[slot] = false;
        true
    }

    pub(super) fn save_current_simd_state(&mut self) {
        let mut phase_marker = Self::phase_chain_start();
        let slot = self.current_task_slot();
        self.save_slot_simd_state(slot);
        self.mark_phase(SchedulerPhase::ArchRestore, &mut phase_marker);
    }

    pub(super) fn restore_current_simd_state(&mut self) {
        let mut phase_marker = Self::phase_chain_start();
        let slot = self.current_task_slot();
        self.restore_slot_simd_state(slot);
        self.mark_phase(SchedulerPhase::ArchRestore, &mut phase_marker);
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
            process_id: context.process_id,
            console_session: context.console_session,
        })
    }

    /// Returns the first slot whose published identity disagrees with the
    /// scheduler's own tables, or `None` when the publication is complete.
    /// The position the legacy tables imply for `slot`.
    ///
    /// This is half of the refinement map `V5-FORMAL-SCHED-019` needs written
    /// down. Execution ownership comes from the published current/transition
    /// pair, runnability from the owner word, and lifetime from the retire
    /// flag and the presence of a context. `runqueue.rs` holds all three in one
    /// word; here they are three sources, which is exactly why they can
    /// disagree.
    fn legacy_position(&self, slot: usize) -> run_authority::LegacyPosition {
        use run_authority::LegacyPosition;
        let Some(context) = self.contexts.get(slot).and_then(|context| *context) else {
            return LegacyPosition::Absent;
        };
        if let Some(owner) = super::cpu_local::task_execution_owner(slot) {
            return match owner {
                super::cpu_local::TaskExecutionOwner::Current(cpu) => {
                    LegacyPosition::Running(u8::try_from(cpu).unwrap_or(u8::MAX))
                }
                super::cpu_local::TaskExecutionOwner::Transition(cpu) => {
                    LegacyPosition::Transition(u8::try_from(cpu).unwrap_or(u8::MAX))
                }
            };
        }
        if self.retired[slot] {
            return LegacyPosition::Retiring;
        }
        // A suspended slot is not runnable, regardless of the test adapter.
        // `allocate_user_slot` initializes that adapter true and
        // then admits the slot with `admit_runqueue_slot(slot, !start_suspended)`,
        // so a service spawned suspended sits ready-but-unqueued by design until
        // activation publishes it. Reading the field alone reported that as a
        // `RunnableButUnqueued` divergence once per second for as long as such a
        // slot existed — which is also the trap stage four would have walked
        // into, since deleting the field naively would make every suspended task
        // look runnable.
        if context.ready_since_ticks != 0 && !self.start_suspended[slot] {
            return LegacyPosition::Runnable;
        }
        LegacyPosition::Blocked
    }

    /// The authoritative runqueue owner for `slot`, in the shape the comparison
    /// takes.
    fn queue_owner(&self, slot: usize) -> run_authority::QueueOwner {
        use run_authority::QueueOwner;
        let snapshot = runqueue::owner(slot);
        let cpu = snapshot.cpu.and_then(|cpu| u8::try_from(cpu).ok());
        match snapshot.state {
            runqueue::RunOwnerState::Dormant => QueueOwner::Dormant,
            runqueue::RunOwnerState::Blocked => QueueOwner::Blocked,
            runqueue::RunOwnerState::Local
            | runqueue::RunOwnerState::RemoteQueued
            | runqueue::RunOwnerState::DirectHandoff => QueueOwner::Queued(cpu),
            runqueue::RunOwnerState::Running => {
                QueueOwner::Running(cpu.unwrap_or(u8::MAX), snapshot.runnable)
            }
            runqueue::RunOwnerState::Migrating => QueueOwner::Migrating(cpu),
            runqueue::RunOwnerState::Retiring => QueueOwner::Retiring,
            runqueue::RunOwnerState::Retired => QueueOwner::Retired,
        }
    }

    /// Whether `slot` is queued and waiting to run.
    ///
    /// This is the authority the legacy readiness bit used to represent. The
    /// two predicates agree in unit fixtures: dispatch clears the adapter on
    /// the task it selects at the same
    /// moment `runqueue::claim_dispatch` moves its owner word to `Running`, so
    /// `ready == true` is exactly `Local | RemoteQueued`. Stage one of
    /// `V5-SCHED-GLOBAL-001` swept every slot once per second at 1 and 8 vCPU
    /// and found no disagreement.
    ///
    /// Reading the owner word instead of the field is the point: the word is
    /// updated by CAS and needs no lock, while the field lives in the globally
    /// locked `Scheduler` struct. Every reader moved here is one fewer reason
    /// for that struct to be shared.
    ///
    /// **This cannot answer for the task a CPU is currently executing**: it
    /// intentionally asks for queue or direct-handoff custody. Current-task
    /// decisions use `runqueue::owner_has_run_intent` instead. That shared bit
    /// preserves run intent across `Running`, `Local`, `RemoteQueued`, and
    /// `Migrating`, while this predicate remains narrow enough to reject a
    /// task that is running elsewhere or still crossing a mailbox boundary.
    pub(super) fn slot_is_runnable(&self, slot: usize) -> bool {
        // `admit_runqueue_slot` is `#[cfg(not(test))]`, so under unit test no
        // slot is ever admitted and every owner word stays `Dormant`. That is
        // worth stating plainly: the per-CPU runqueue's custody rules are
        // exercised only by the KVM gates, never by `cargo test`, and this
        // helper is not the place to change that. Reading the field here keeps
        // the selection-policy tests testing selection policy; the owner word
        // itself is covered by the once-per-drain divergence sweep on real runs.
        #[cfg(test)]
        {
            return self.contexts[slot].is_some_and(|context| context.test_ready);
        }
        #[cfg(not(test))]
        runqueue::is_handoff_dispatchable(slot)
    }

    /// Compares the per-CPU owner word against the legacy tables for every slot.
    ///
    /// Run under the lock, so both sources are one stable observation. A
    /// disagreement is a cutover blocker: once the global lock is gone the owner
    /// word is the only synchronisation between CPUs, and the execution-owner
    /// kinds become a task running on two of them.
    ///
    /// The calling CPU's in-flight dispatch pair is excluded, and that exclusion
    /// is load-bearing rather than convenient. This sweep runs from
    /// `take_runtime_profile`, which the dispatch path calls *after* the
    /// runqueue has claimed the incoming slot and *before*
    /// `SchedulerAccessGuard::drop` publishes the new current/transition pair.
    /// Both halves of that window therefore disagree by construction: the
    /// incoming slot reads `QueueRunningLegacyNot` and the outgoing slot reads
    /// `LegacyRunningQueueNot`. The first run reported exactly that, two per
    /// second, every second. Those two slots are covered by the guard's own
    /// duplicate-owner assertions, which fail the kernel rather than reporting.
    pub(super) fn sweep_run_authority(&self) {
        let dispatching = self.current_task_slot();
        let published = super::cpu_local::current_cpu_task_slot();
        for slot in 0..MAX_TASK {
            if slot == dispatching || published == Some(slot) {
                continue;
            }
            if let Some(kind) = run_authority::compare(
                self.queue_owner(slot),
                self.legacy_position(slot),
                self.contexts[slot].is_some_and(|context| context.ready_since_ticks != 0),
            ) {
                run_authority::record(slot, kind);
            }
        }
    }

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
            context.ready_since_ticks = 0;
        }

        self.exec_target_quiesced[slot] = false;
        self.reset_slot_simd_state(slot);
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
            #[cfg(test)]
            {
                context.saved_rsp = saved_rsp;
            }
            context.address_space_root = new_root;
            context.user_abi = Some(bootstrap.abi);
            context.console_session = bootstrap.console_session;
            context.user_stack = bootstrap.user_stack;
            context.blocked = false;
            context.blocked_since_ticks = 0;
            context.ready_since_ticks = Self::ready_since_now_ticks();
        }
        self.set_slot_saved_rsp(slot, saved_rsp);
        self.exec_target_quiesced[slot] = false;
        self.reset_slot_simd_state(slot);
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
        self.arm_block_current_task_with_reason(BlockReason::Generic)
    }

    pub(super) fn arm_block_current_task_on_endpoint(&mut self, endpoint: u64) -> bool {
        if endpoint == 0 {
            return false;
        }
        self.arm_block_current_task_with_reason(BlockReason::EndpointReceive(endpoint))
    }

    pub(super) fn arm_block_current_task_on_reply(&mut self, reply: u64) -> bool {
        if reply == 0 {
            return false;
        }
        self.arm_block_current_task_with_reason(BlockReason::EndpointReply(reply))
    }

    fn arm_block_current_task_with_reason(&mut self, reason: BlockReason) -> bool {
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
        context.ready_since_ticks = 0;
        #[cfg(test)]
        {
            context.test_ready = false;
        }
        context.wake_armed = true;
        context.block_reason = reason;
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
        context.ready_since_ticks = 0;
        #[cfg(test)]
        {
            context.test_ready = false;
        }
        context.wake_armed = false;
        context.block_reason = BlockReason::None;
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
            context.ready_since_ticks = 0;
            #[cfg(test)]
            {
                context.test_ready = false;
            }
            context.block_reason = BlockReason::None;
            return Some(false);
        }
        context.wake_armed = false;
        context.blocked = true;
        context.ready_since_ticks = 0;
        #[cfg(test)]
        {
            context.test_ready = false;
        }
        context.blocked_since_ticks = crate::arch::rtc::ticks();
        // A blocked task remains `Running` until the next scheduler turn
        // publishes blocked custody, but must immediately withdraw run intent
        // so that turn cannot requeue it merely because the legacy queue bit
        // is overloaded with dispatch state.
        #[cfg(not(test))]
        runqueue::set_runnable(slot, false);
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

    /// Completes the scheduler half of a terminal IPC reply under one catalog
    /// acquisition.  The returned proof carries no lifecycle authority: it is
    /// only an exact post-wake runqueue snapshot for the external per-CPU
    /// synchronous-handoff owner.
    pub(super) fn complete_ipc_reply_wake_handoff(
        &mut self,
        reply: u64,
        task_id: u64,
    ) -> Option<ReplyWakeHandoff> {
        #[cfg(test)]
        let _ = self.release_ipc_priority(reply);
        #[cfg(not(test))]
        let _ = reply;
        let slot = self.find_task_slot(task_id)?;
        if !self.wake_task_slot(slot) {
            return None;
        }
        self.reply_wake_handoff(slot, task_id)
    }

    pub(super) fn complete_fast_ipc_reply_handoff(
        &mut self,
        reply: u64,
        caller_task_id: u64,
    ) -> FastIpcReplyHandoffOutcome {
        let Some(caller_slot) = self.find_task_slot(caller_task_id) else {
            return FastIpcReplyHandoffOutcome::Rejected;
        };
        self.complete_fast_ipc_reply_handoff_slot(reply, caller_slot)
    }

    fn complete_fast_ipc_reply_handoff_slot(
        &mut self,
        reply: u64,
        caller_slot: usize,
    ) -> FastIpcReplyHandoffOutcome {
        if self.retired[caller_slot]
            || self.start_suspended[caller_slot]
            || !self.contexts[caller_slot].is_some_and(|context| {
                context.blocked
                    && !context.wake_armed
                    && context.block_reason == BlockReason::EndpointReply(reply)
            })
        {
            return FastIpcReplyHandoffOutcome::Rejected;
        }
        let current_cpu = Self::current_dispatch_cpu();
        if self.slot_dispatch_cpu(caller_slot) != current_cpu {
            return FastIpcReplyHandoffOutcome::Rejected;
        }
        #[cfg(not(test))]
        {
            if !self.context_is_dispatch_eligible(
                caller_slot,
                self.contexts[caller_slot].expect("validated fast IPC caller lost context"),
            ) {
                return FastIpcReplyHandoffOutcome::Rejected;
            }
        }
        #[cfg(not(test))]
        if !matches!(
            runqueue::publish_direct_handoff(caller_slot, current_cpu),
            runqueue::RemoteWakeOutcome::Published { .. }
        ) {
            return FastIpcReplyHandoffOutcome::Rejected;
        }
        let direct = self.enqueue_synchronous_handoff_slot(caller_slot);
        if !direct {
            #[cfg(not(test))]
            assert!(
                runqueue::materialize_direct_handoff(
                    caller_slot,
                    current_cpu,
                    self.contexts[caller_slot]
                        .expect("validated fast IPC caller lost context")
                        .weight,
                ),
                "fast IPC reply fallback lost caller custody"
            );
        }
        let caller = self.contexts[caller_slot]
            .as_mut()
            .expect("validated fast IPC caller lost context");
        caller.blocked = false;
        caller.blocked_since_ticks = 0;
        caller.block_reason = BlockReason::None;
        caller.ready_since_ticks = Self::ready_since_now_ticks();
        #[cfg(test)]
        {
            caller.test_ready = true;
        }
        if direct {
            FastIpcReplyHandoffOutcome::Direct
        } else {
            FastIpcReplyHandoffOutcome::LocalFallback
        }
    }

    /// Validates returned scheduling-context custody, releases the reply-owned
    /// donation, and publishes the reverse fast handoff under one scheduler
    /// catalog acquisition. The established Scheduler -> DonationLedger order
    /// is already used by call admission; no reverse acquisition exists.
    pub(super) fn settle_and_complete_fast_ipc_reply_handoff(
        &mut self,
        reply: u64,
        caller_task_id: u64,
        context_owner_task_id: u64,
        scheduling_context: ObjectIdentity,
    ) -> Option<FastIpcReplyHandoffOutcome> {
        let context_owner_slot =
            self.scheduling_context_slot(context_owner_task_id, scheduling_context)?;
        let caller_slot = if context_owner_task_id == caller_task_id {
            context_owner_slot
        } else {
            self.find_task_slot(caller_task_id)?
        };
        let _ = release_reply_donation(reply);
        Some(self.complete_fast_ipc_reply_handoff_slot(reply, caller_slot))
    }

    fn reply_wake_handoff(&self, slot: usize, task_id: u64) -> Option<ReplyWakeHandoff> {
        self.reply_wake_handoff_from_owner(slot, task_id, runqueue::owner(slot))
    }

    /// Pure token-mint decision shared by the production owner-word read and
    /// host witnesses. Keeping this outside `cfg(not(test))` makes the exact
    /// identity/runnability/custody seam executable without weakening the
    /// unit-test isolation of the global runqueue backend.
    fn reply_wake_handoff_from_owner(
        &self,
        slot: usize,
        task_id: u64,
        owner: runqueue::RunOwnerSnapshot,
    ) -> Option<ReplyWakeHandoff> {
        if self.starts[slot].is_none_or(|start| start.id != task_id) {
            return None;
        }
        ReplyWakeHandoff::from_owner(slot, task_id, owner)
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
        let (saved_rsp, user_mode, was_blocked, wake_was_armed) = match self.contexts[slot] {
            Some(context) => (
                self.slot_saved_rsp(slot),
                context.user_mode,
                context.blocked,
                context.wake_armed,
            ),
            None => return false,
        };
        // Unit schedulers intentionally do not publish global owner words;
        // adapt their isolated legacy bit into the same snapshot shape. The
        // production decision below takes exactly one acquire owner snapshot.
        #[cfg(test)]
        let wake_owner = self.contexts[slot].map(|context| runqueue::RunOwnerSnapshot {
            state: if context.ready_since_ticks != 0 {
                runqueue::RunOwnerState::Local
            } else {
                runqueue::RunOwnerState::Blocked
            },
            cpu: None,
            generation: 1,
            runnable: context.ready_since_ticks != 0,
        });
        #[cfg(not(test))]
        let wake_owner = Some(runqueue::owner(slot));
        let Some(wake_owner) = wake_owner else {
            return false;
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
            context.block_reason = BlockReason::None;
            context.ready_since_ticks = 0;
            #[cfg(test)]
            {
                context.test_ready = false;
            }
            context.blocked_since_ticks = 0;
            return true;
        }
        if let Some(super::cpu_local::TaskExecutionOwner::Transition(transition_cpu)) =
            execution_owner
        {
            // The outgoing frame is already published, but the old kernel
            // stack remains owned by assembly. Publish exact mailbox custody
            // before clearing the block state: `ready` is only a diagnostic
            // refinement here and can never be the wake's sole authority.
            // Candidate selection still rejects this slot while the transition
            // owner retains its stack, so the publication cannot dispatch
            // until assembly release-clears that ownership.
            //
            // A rejected result preserves the existing terminal or migration
            // owner and leaves the scheduler context untouched. `Local`,
            // `RemoteQueued`, and `Running` are already authoritative and
            // must not receive a second queue or mailbox record.
            match self.publish_runqueue_wake_to(slot, transition_cpu) {
                runqueue::RemoteWakeOutcome::Rejected => return false,
                runqueue::RemoteWakeOutcome::AlreadyOwned { .. }
                | runqueue::RemoteWakeOutcome::Published { .. } => {}
            }
            let context = self.contexts[slot]
                .as_mut()
                .expect("transitioning scheduler slot lost its context during wake");
            context.wake_armed = false;
            context.blocked = false;
            context.block_reason = BlockReason::None;
            context.ready_since_ticks = Self::ready_since_now_ticks();
            #[cfg(test)]
            {
                context.test_ready = true;
            }
            context.blocked_since_ticks = 0;
            return true;
        }
        let already_runnable =
            runqueue::wake_is_already_runnable(wake_owner, was_blocked, wake_was_armed);
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

        let waker_ready = {
            let Some(context) = self.contexts[slot].as_mut() else {
                return false;
            };
            // Always clear the arm flag so a paired commit_block_current_task
            // observes that a wake raced before the caller actually slept.
            context.wake_armed = false;
            context.blocked = false;
            context.block_reason = BlockReason::None;
            #[cfg(test)]
            {
                context.test_ready = invalid_reason.is_none() || already_runnable;
            }
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
            }
            invalid_reason.is_none() && !self.job_stopped[slot]
        };
        if invalid_reason.is_none() && !already_runnable {
            self.raise_slot_vruntime_floor(slot, wake_floor);
        }
        let waker_vruntime = self.slot_vruntime(slot);

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
                    let current_v = self.slot_vruntime(current_slot);
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
            self.contexts[slot]
                .map(|_| self.slot_saved_rsp(slot))
                .unwrap_or(0),
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
                let state_flags = (u64::from(context.ready_since_ticks != 0)
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

    fn context_returns_to_user(&self, slot: usize) -> bool {
        Self::saved_context_returns_to_user(self.slot_saved_rsp(slot))
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
    #[cfg(test)]
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

    pub(super) fn find_task_slot(&self, task_id: u64) -> Option<usize> {
        let hint_index = task_id as usize & (MAX_TASK - 1);
        let hint = usize::from(self.task_slot_hints[hint_index].load(Ordering::Relaxed));
        if hint < MAX_TASK
            && !self.retired[hint]
            && self.contexts[hint].is_some()
            && self.starts[hint].is_some_and(|start| start.id == task_id)
        {
            return Some(hint);
        }
        for slot in 0..MAX_TASK {
            if self.retired[slot] || self.contexts[slot].is_none() {
                continue;
            }
            if self.starts[slot].map(|start| start.id) == Some(task_id) {
                // ORDERING: Relaxed is exact. The scheduler owner serializes
                // writers and the value is only a hint revalidated above.
                self.task_slot_hints[hint_index].store(
                    u8::try_from(slot).expect("scheduler slot exceeds task hint capacity"),
                    Ordering::Relaxed,
                );
                return Some(slot);
            }
        }

        self.task_slot_hints[hint_index].store(TASK_SLOT_HINT_EMPTY, Ordering::Relaxed);
        None
    }

    pub(super) fn scheduling_context_matches(
        &self,
        task_id: u64,
        identity: ObjectIdentity,
    ) -> bool {
        self.scheduling_context_slot(task_id, identity).is_some()
    }

    /// Resolves the authoritative slot encoded in the typed object identity,
    /// then revalidates the exact monotonic task binding and complete identity.
    /// Scanning every task is redundant because no other slot can own it.
    fn scheduling_context_slot(&self, task_id: u64, identity: ObjectIdentity) -> Option<usize> {
        let raw_slot = identity.slot().checked_sub(1)?;
        let slot = usize::try_from(raw_slot).ok()?;
        let context = self.contexts.get(slot).copied().flatten()?;
        self.starts
            .get(slot)
            .copied()
            .flatten()
            .filter(|start| start.id == task_id)?;
        (context.scheduling_context.is_bound_to(task_id)
            && context.scheduling_context.identity() == identity)
            .then_some(slot)
    }
}

/// Self-times one member of the handoff chain.
///
/// The chain runs in order on every dispatch and the phase marks cannot see
/// inside it: one mark opens before the first step and the next closes after
/// the last, so all six steps plus the match glue arrive as a single number.
/// Wrapping the call sites is what separates them, and the steps are measured
/// before any of them is changed — the one predicate reorder attempted on a
/// guess moved nothing.
fn timed_handoff_step<T>(step: usize, body: impl FnOnce() -> T) -> T {
    #[cfg(not(rustos_scheduler_phase_profile))]
    {
        let _ = step;
        return body();
    }
    #[cfg(rustos_scheduler_phase_profile)]
    {
        let started_ns = crate::arch::clock::monotonic_nanos();
        let value = body();
        locality::charge_handoff_step(
            step,
            crate::arch::clock::monotonic_nanos().saturating_sub(started_ns),
        );
        value
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
mod tests;
