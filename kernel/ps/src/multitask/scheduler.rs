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
pub(in crate::multitask) mod activation;
mod affinity;
pub(in crate::multitask) mod return_architecture;
mod block_reason;
mod boot_execution;
pub(in crate::multitask) mod bootstrap;
mod domain_admission;
pub(in crate::multitask) mod kernel_creation;
pub(super) mod process_affinity;
pub(in crate::multitask) mod root_creation;
mod root_slots;
pub(super) mod thread_affinity;
pub use affinity::{AffinityCommit, AffinityError, ProcessAffinitySnapshot};
pub(in crate::multitask) use block_reason::BlockReason;
#[cfg(test)]
mod activation_batch_tests;
mod context_validation;
mod dispatch_policy;
pub(in crate::multitask) mod dispatcher;
pub(in crate::multitask) mod dispatch_priority;
mod donation_ledger;
pub(super) mod donation_publication;
pub(in crate::multitask) mod ipc_publication;
pub(in crate::multitask) mod ipc_lane;
pub(in crate::multitask) mod pending_reply;
mod frame_observation;
mod frame_publication;
pub(in crate::multitask) mod rsp_handoff;
mod handoff_queue;
mod handoffs;
pub(in crate::multitask) mod ipc_donation;
pub(in crate::multitask) mod linux_thread_state;
mod local_dispatch;
mod locality;
pub(in crate::multitask) mod pager_handoff;
pub(super) mod exec_transaction;
mod execution_image;
pub(in crate::multitask) mod execution_interval;
pub(in crate::multitask) mod group_exit;
pub(super) mod reaper;
mod reclaim;
pub(super) mod retirement_cleanup;
mod runqueue;
pub(in crate::multitask) mod resident;
mod terminal_quarantine;
pub(super) mod wake_publication;
pub(super) mod wake_retry;
mod terminal_publication;
pub(super) mod current_terminal;
pub(super) mod current_observation;
pub(super) mod ordering_publication;
pub(in crate::multitask) use runqueue::affinity_payload::PlacementGrant;
pub(in crate::multitask) mod runqueue_policy;
pub(in crate::multitask) mod runtime_profile;
mod scheduling_context;
mod scheduling_runtime;
pub(super) mod stack_resources;
pub(super) use stack_resources::admission::Publication;
mod sync_handoff;
mod task_directory;
mod task_wait;
mod current_wait;
pub(super) mod current_class;
pub(in crate::multitask) mod current_return;
pub(in crate::multitask) mod outgoing_image;
pub(in crate::multitask) mod dispatch_claim;
mod task_weight;
pub use runtime_profile::drain_scheduler_runtime_profile;
pub use runqueue::simd_tls::{adopt_ipc_register_simd, begin_ipc_register_simd, IpcRegisterSimdGuard};
pub(in crate::multitask) use runtime_profile::{
    SchedulerEntryCause, publish_scheduler_runtime_profile,
};
pub(super) use task_weight::{
    LOAD_WEIGHT_MASK, NICE_0_LOAD,
    SYSTEM_CLASS_WEIGHT_FLAG,
};
#[cfg(test)]
pub(super) use task_weight::MIN_LOAD_WEIGHT;

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
/// Whether the dispatcher could pick something other than the caller on `cpu`:
/// queued or mailboxed work, a synchronous handoff, or an atomic activation.
pub(in crate::multitask) fn other_dispatch_work_pending(cpu: usize) -> bool {
    local_dispatch_work_pending(cpu) || dispatch_policy::atomic_activation_pending(cpu)
}
/// Reads the effective scheduling-context owner and live donation reply from
/// the lock-free custody ledger. Native execution deliberately returns `None`.
/// Permit units a parked resident cell holds for `slot` (see `resident`).
pub(in crate::multitask) fn resident_permit_units(slot: usize) -> u32 {
    resident::permit_units(slot)
}

pub(in crate::multitask) fn borrowed_context_charge_token(slot: usize) -> Option<(usize, u64)> {
    donation_ledger::borrowed_context_charge_token(slot)
}

pub(crate) mod smp;
#[cfg(test)]
mod synchronous_handoff_tests;
#[cfg(test)]
pub(in crate::multitask) mod irq_entry_tests;
pub(in crate::multitask) mod thread_creation;
mod thread_slots;

#[cfg(test)]
use alloc::vec::Vec;
use core::{mem, ptr};

use x86_64::VirtAddr;

use kernel_object::api::identity::ObjectIdentity;

use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging::ProcessAddressSpace;
use crate::user::abi::UserAbi;
use crate::user::linux::LinuxThreadState;
use crate::user::process;
use crate::user::process_state::UserProcessState;

use super::context::{SAVED_CONTEXT_BYTES, SavedContext};
use super::current_identity::{self, PagerChargeStamp, TaskIdentity};
use super::process_table::{self, ProcessHandle};
use super::{UserStackState, UserTaskBootstrap, initial_task_rflags};
use context_validation::{
    PublishedFrameView, context_validation_reason_code,
};
use handoff_queue::SlotHandoffQueue;
pub(super) use linux_thread_state::CurrentLinuxThreadBinding;
#[cfg(test)]
use reclaim::RetirementSideEffect;
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
pub(in crate::multitask) fn release_reply_donation(
    reply: u64,
    namespace: ipc_donation::DonationNamespace,
) -> bool {
    donation_ledger::release_reply(reply, namespace)
}

pub(in crate::multitask) use current_wait::{
    CurrentWait, arm_current_wait, cancel_current_wait, commit_current_wait,
};

pub(in crate::multitask) use task_directory::{
    published_user_log_ids, reply_context_custody,
};

/// Transfers one already-reserved donor edge onto its exact reply capability.
///
/// The reservation that precedes this call is what validated the donor's live
/// slot, and the edge itself lives in the donation ledger behind its own
/// bounded lock. Nothing here reads the task catalog, so taking the exclusive
/// catalog guard added one acquisition per synchronous round trip and bought
/// no exclusion the ledger did not already provide.
pub(in crate::multitask) fn attach_reply_donation(reply: u64, donor_task_id: u64) -> bool {
    donation_ledger::attach(
        reply,
        ipc_donation::DonationNamespace::IpcReply,
        donor_task_id,
    )
}


/// Drops one donor reservation that never reached a reply capability.
///
/// Like [`attach_reply_donation`], the edge is ledger-owned and the catalog is
/// not consulted, so the cancellation does not need the catalog guard.
pub(in crate::multitask) fn cancel_reply_donation_reservation(donor_task_id: u64) -> bool {
    donation_ledger::cancel_reservation(donor_task_id)
}



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
const RFLAGS_RESERVED_BIT_1: u64 = 1 << 1;
const LONG_BLOCKED_WAIT_THRESHOLD_MS: u64 = 250;

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
/// Maximum scheduler-clock quanta that one synchronous IPC execution chain
/// may retain direct-handoff priority while unrelated fair work is ready.
/// Charging elapsed time rather than message count makes the bound independent
/// of RPC granularity: tiny call/reply pairs do not manufacture artificial
/// preemptions, while a busy chain still yields after about 7.8 ms at 1024 Hz.
const MAX_SYNC_HANDOFF_CHAIN_TICKS: u8 = 8;
const READY_VALIDATION_INTERVAL_TURNS: u8 = 32;
/// An atomically published startup cohort receives one bounded first-turn
/// prefix before the reply chain that resumes its loader/supervisor. This is
/// capped by the activation ABI and never applies to ordinary single spawns.
const MAX_ATOMIC_ACTIVATION_HANDOFFS: usize = 8;
const MAX_LATENCY_HANDOFF_HINTS: usize = 16;

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
/// and the Idle halt loop. Production ordering uses the independent published
/// LocalClass observation; this catalog enum remains for residual consumers.
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

}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// The part of a Windows thread's runtime state the scheduler needs after
/// creation: its identity and TEB (the GS base). The TLS slot values are
/// materialized into the TEB once and are never read back from here; holding
/// them made every task context 528 bytes larger, and a context is copied on
/// every wake binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowsThreadIdentity {
    thread_id: u64,
    teb_address: u64,
}


#[derive(Clone, Copy)]
struct TaskContext {
    scheduling_context: scheduling_context::SchedulingContext,
    /// Host-test fixture for the production per-slot saved-frame pointer.
    #[cfg(test)]
    saved_rsp: usize,
    /// Host-test fixture for the production per-slot address-space root.
    #[cfg(test)]
    address_space_root: u64,
    user_mode: bool,
    user_abi: Option<UserAbi>,
    console_session: ConsoleSessionHandle,
    process_handle: Option<ProcessHandle>,
    process_id: Option<u64>,
    user_stack: Option<UserStackState>,
    windows_thread_state: Option<WindowsThreadIdentity>,
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
    EligibilityUnavailable,
    DirectCustodyUnavailable,
    OrderingUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FastIpcReplyHandoffOutcome {
    LocalFallback,
    Rejected,
}
impl FastIpcReplyHandoffOutcome {
    /// Request a fast attempt after the actual typed wake/FIFO publication.
    /// This copied hint grants nothing: the dispatcher still consumes the sole
    /// FIFO, checks chain/atomic priority, and claims the original frame/funding.
    /// Remote custody, early Current wake or refusal falls back normally.
    pub(in crate::multitask) fn dispatch_hint(self, caller: u64) -> Option<u64> {
        (self != Self::Rejected).then_some(caller)
    }
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

    pub(super) const fn requires_architectural_restore(self) -> bool {
        self.previous_slot != self.next_slot
    }


    /// A task switch only requires a CR3 activation when it also crosses an
    /// address-space boundary. Threads in one process still need their TSS,
    /// syscall-stack, and TLS state restored, but replaying the same CR3 is a
    /// serializing TLB operation on every synchronous IPC handoff.
    #[cfg(test)]
    const fn requires_address_space_restore(
        self,
        previous_address_space_root: u64,
        next_address_space_root: u64,
    ) -> bool {
        self.requires_architectural_restore()
            && previous_address_space_root != next_address_space_root
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

#[cfg(test)]
pub(in crate::multitask) fn execution_test_serial_guard() -> std::sync::MutexGuard<'static, ()> {
    scheduling_runtime::test_serial_guard()
}

/// Shared native fixture order and real process object for exec-census components.
#[cfg(test)]
pub(in crate::multitask) fn with_exec_census_fixture(
    id: u64,
    f: impl FnOnce(process_table::ProcessHandle),
) {
    let _namespace = tests::boxed_scheduler();
    let handle = tests::test_process(id);
    f(handle);
}



#[cfg(rustos_log_sched_debug)]
fn saved_context_rip(saved_rsp: usize) -> Option<u64> {
    if saved_rsp == 0 {
        return None;
    }
    let context = saved_rsp as *const SavedContext;
    Some(unsafe { (*context).rip })
}

#[cfg(test)]
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

#[cfg(test)]
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
