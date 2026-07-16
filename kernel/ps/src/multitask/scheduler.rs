use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::{mem, ptr, ptr::NonNull};

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
use super::process_table::{self, ProcessHandle};
use super::{UserFaultDisposition, UserStackState, UserTaskBootstrap, initial_task_rflags};

// The enabled product topology boots roughly twenty policy/service processes
// before the UI creates its bounded input, display, diagnostics, console, and
// Wayland workers. A 32-slot table therefore exhausted during normal shell
// launch and turned a recoverable capacity error into uiserver thread-spawn
// panic. Keep the scheduler allocation-free and explicitly bounded, but size
// the product contract for service growth and application headroom.
pub(super) const MAX_TASK: usize = 128;
const ROOT_TASK_SLOT: usize = 0;
const FIRST_DYNAMIC_TASK_SLOT: usize = 1;
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
// Cap bumped from 64 -> 256 so the diagnostic survives past the initial boot
// spike. Earlier runs exhausted the cap by the second of bring-up, hiding the
// later steady-state stalls completely.
const MAX_LONG_WAIT_LOGS: usize = 256;
static LONG_WAIT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

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
// Maximum runtime budget before we *forcibly* prefer any other ready task,
// even ones that would normally lose the vruntime comparison. Acts as a
// last-resort anti-starvation rail for long-running kernel paths that finally
// call cond_resched.
const SCHED_MAX_BURST_NS: u64 = 20_000_000;
/// A runnable strict-class task must not wait behind a sequence of other
/// strict-class tasks indefinitely. This is a dispatch-latency rail, not a
/// CPU-share boost: once the bound expires the oldest ready System task gets
/// one turn, after which normal weighted vruntime resumes.
const SYSTEM_READY_LATENCY_BOUND_MS: u64 = 10;
// Initial vruntime offset for newly-spawned tasks relative to current
// min_vruntime. Keep this near min-granularity: a larger multi-ms penalty
// leaves freshly spawned services behind polling System peers during boot.
const SCHED_NEW_TASK_VRUNTIME_PENALTY_NS: u64 = SCHED_MIN_GRANULARITY_NS;

/// Strict class is an explicit admission property, not an accidental result
/// of a large CFS share. Bootstrap brokers legitimately use larger weights
/// than uiserver/inputd; deriving class from the number let those pollers
/// crowd the interactive band and caused multi-second UI starvation.
/// A critical service remains latency-favoured, but it cannot consume every
/// dispatch indefinitely while ordinary work is ready. Eight System turns
/// followed by one mandatory User turn reserve at least one ninth of dispatch
/// opportunities for recovery and application work under a hostile input or
/// GUI-DVM flood.
const MAX_CONSECUTIVE_SYSTEM_DISPATCHES: u8 = 8;

/// Priority bands. System work wins latency-sensitive selection until its
/// bounded consecutive-dispatch reservation is exhausted; then one ready User
/// task must run before System selection resumes. Within a class the existing
/// CFS-style vruntime accounting decides fairness.
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

pub(super) struct CurrentLinuxThreadBinding {
    pub(super) process_handle: ProcessHandle,
    pub(super) tid: u64,
    pub(super) abi: UserAbi,
    pub(super) linux_thread_state: NonNull<Option<LinuxThreadState>>,
}

#[derive(Clone, Copy)]
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
    linux_thread_state: Option<LinuxThreadState>,
    windows_thread_state: Option<WindowsThreadRuntimeState>,
}

/// A bounded priority-inheritance edge for one synchronous IPC reply
/// capability.  The edge lasts only while that reply capability is live: the
/// caller donates its effective scheduling class to the receiver, and the
/// class therefore propagates through a nested synchronous call chain.
///
/// Keeping this in the scheduler rather than in the IPC object store avoids a
/// dependency from `kernel-ipc-runtime` back into `kernel-ps`.  Its lifetime
/// is still tied to the reply capability by the compat IPC boundary.
#[derive(Clone, Copy)]
struct IpcPriorityDonation {
    reply: u64,
    donor_task_id: u64,
    /// Task-owned endpoints donate to their exact receiver. Process-owned
    /// endpoints may not have a waiter at enqueue time, so their donation
    /// covers the receiver process until a worker is known.
    receiver_task_id: Option<u64>,
    receiver_process_id: Option<u64>,
}

#[derive(Clone, Copy)]
pub(super) struct TaskStart {
    pub(super) entry: fn(u64),
    pub(super) id: u64,
}

pub(super) struct Scheduler {
    contexts: [Option<TaskContext>; MAX_TASK],
    retired: [bool; MAX_TASK],
    start_suspended: [bool; MAX_TASK],
    retire_reasons: [Option<TaskRetireReason>; MAX_TASK],
    simd_states: [SimdState; MAX_TASK],
    starts: [Option<TaskStart>; MAX_TASK],
    stacks: [Option<Vec<u8>>; MAX_TASK],
    current_task: usize,
    pending_reap: bool,
    /// L4/seL4-style "donate" hint: on the next scheduler tick, prefer this
    /// slot if it is ready. Set by IPC paths immediately before
    /// `yield_now()` so the caller hands its remaining timeslice to the
    /// receiver/replier instead of letting round-robin pick an unrelated task
    /// and stalling for an entire PIT slice. The hint is kept while its target
    /// is ready but temporarily blocked by a higher scheduling class, and is
    /// cleared once consumed or once the target is no longer schedulable.
    next_pick_hint: Option<usize>,
    /// Spawn handoff hint, kept separate from IPC reply hints so the loader's
    /// reply to the supervisor cannot overwrite the freshly-created child.
    next_spawn_pick_hint: Option<usize>,
    /// At most one synchronous IPC wait can be active per runnable task, so
    /// `MAX_TASK` fixed entries cover every live donation without allocating
    /// from scheduler or IPC paths.
    ipc_priority_donations: [Option<IpcPriorityDonation>; MAX_TASK],
    /// Cached minimum vruntime across the ready set, refreshed each pick.
    /// New tasks initialise their vruntime from this value (plus a small
    /// penalty) so they cannot preempt long-lived ready tasks just by virtue
    /// of being freshly created.
    last_min_vruntime_ns: u64,
    /// True after the bootstrap root task has entered the permanent hlt loop.
    /// Before that, slot 0 still runs finalize work and remains schedulable.
    root_idle: bool,
    /// Number of immediately preceding System-class dispatches. Once the
    /// bounded maximum is reached a ready User task receives one mandatory
    /// dispatch, preventing a critical-lane flood from starving recovery or
    /// ordinary applications forever.
    system_dispatch_streak: u8,
    /// Fixed scheduler tick divisor. CFS-style scheduling accounts CPU share
    /// through vruntime weights; it must not also shorten/lengthen the hardware
    /// tick per task or low-weight services pay excessive interrupt overhead.
    scheduler_tick_divisor: u16,
}

impl Scheduler {
    pub(super) const fn new() -> Self {
        Self {
            contexts: [None; MAX_TASK],
            retired: [false; MAX_TASK],
            start_suspended: [false; MAX_TASK],
            retire_reasons: [None; MAX_TASK],
            simd_states: [SimdState::new(); MAX_TASK],
            starts: [None; MAX_TASK],
            stacks: [const { None }; MAX_TASK],
            current_task: 0,
            pending_reap: false,
            next_pick_hint: None,
            next_spawn_pick_hint: None,
            ipc_priority_donations: [None; MAX_TASK],
            last_min_vruntime_ns: 0,
            root_idle: false,
            system_dispatch_streak: 0,
            scheduler_tick_divisor: 0,
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
            self.next_pick_hint = None;
            return;
        };
        if !self.handoff_hint_eligible(slot) {
            return;
        }
        self.apply_ipc_donation(slot);
        self.next_pick_hint = Some(slot);
    }

    /// Selects a runnable worker for a process-owned endpoint when the sender
    /// enqueues between the server's reply and its next `IPC_RECV`. In that
    /// window the endpoint has no waiter task to return, but the process worker
    /// is ready and must receive the same direct-handoff treatment.
    pub(super) fn set_next_process_pick_hint(&mut self, process_id: u64) -> Option<u64> {
        let slot = (0..MAX_TASK)
            .filter(|slot| *slot != self.current_task)
            .filter(|slot| {
                self.contexts[*slot].is_some_and(|context| {
                    context.process_id == Some(process_id)
                        && context.ready
                        && self.context_is_schedulable(*slot, context)
                })
            })
            .min_by_key(|slot| {
                self.contexts[*slot]
                    .map(|context| (context.vruntime_ns, *slot))
                    .unwrap_or((u64::MAX, *slot))
            })?;
        self.apply_ipc_donation(slot);
        self.next_pick_hint = Some(slot);
        self.starts[slot].map(|start| start.id)
    }

    /// Makes `receiver_task_id` inherit the effective strict scheduling class
    /// of `donor_task_id` until `reply` is completed or cancelled.  Repeating
    /// this for a reply updates the receiver because a process-owned endpoint
    /// may hand a queued request to a different worker than the one initially
    /// woken by the sender.
    pub(super) fn inherit_ipc_priority(
        &mut self,
        reply: u64,
        donor_task_id: u64,
        receiver_task_id: u64,
    ) -> bool {
        if reply == 0 || donor_task_id == receiver_task_id {
            return false;
        }
        let Some(donor_slot) = self.find_task_slot(donor_task_id) else {
            return false;
        };
        let Some(receiver_slot) = self.find_task_slot(receiver_task_id) else {
            return false;
        };
        if self.retired[donor_slot] || self.retired[receiver_slot] {
            return false;
        }

        self.upsert_ipc_priority_donation(reply, donor_task_id, Some(receiver_task_id), None)
    }

    /// Starts inheritance for every live worker of a process-owned endpoint.
    /// This covers the enqueue-before-recv interval where no individual
    /// receiver is sleeping in the endpoint waiter queue yet.
    pub(super) fn inherit_ipc_priority_for_process(
        &mut self,
        reply: u64,
        donor_task_id: u64,
        receiver_process_id: u64,
    ) -> bool {
        if reply == 0 || receiver_process_id == 0 {
            return false;
        }
        let Some(donor_slot) = self.find_task_slot(donor_task_id) else {
            return false;
        };
        if self.retired[donor_slot]
            || !self.contexts.iter().enumerate().any(|(slot, context)| {
                !self.retired[slot]
                    && context
                        .is_some_and(|context| context.process_id == Some(receiver_process_id))
            })
        {
            return false;
        }

        self.upsert_ipc_priority_donation(reply, donor_task_id, None, Some(receiver_process_id))
    }

    fn upsert_ipc_priority_donation(
        &mut self,
        reply: u64,
        donor_task_id: u64,
        receiver_task_id: Option<u64>,
        receiver_process_id: Option<u64>,
    ) -> bool {
        if let Some(entry) = self
            .ipc_priority_donations
            .iter_mut()
            .flatten()
            .find(|entry| entry.reply == reply)
        {
            entry.donor_task_id = donor_task_id;
            if receiver_task_id.is_some() {
                entry.receiver_task_id = receiver_task_id;
            }
            if receiver_process_id.is_some() {
                entry.receiver_process_id = receiver_process_id;
            }
            return true;
        }
        let Some(slot) = self
            .ipc_priority_donations
            .iter_mut()
            .find(|entry| entry.is_none())
        else {
            // Do not turn a bounded accounting table into an availability
            // failure on an ABI-visible IPC call.  The fixed bound covers the
            // normal one-wait-per-task model; an exhausted table simply loses
            // the latency boost for that call.
            return false;
        };
        *slot = Some(IpcPriorityDonation {
            reply,
            donor_task_id,
            receiver_task_id,
            receiver_process_id,
        });
        true
    }

    /// Revokes the donation associated with a completed or cancelled reply
    /// capability.  This is deliberately idempotent so reply/error/timeout
    /// races cannot leave an inherited System class behind.
    pub(super) fn release_ipc_priority(&mut self, reply: u64) -> bool {
        let mut released = false;
        for entry in &mut self.ipc_priority_donations {
            if entry.is_some_and(|entry| entry.reply == reply) {
                *entry = None;
                released = true;
            }
        }
        released
    }

    fn release_ipc_priorities_for_task(&mut self, task_id: u64) {
        for entry in &mut self.ipc_priority_donations {
            if entry.is_some_and(|entry| {
                entry.donor_task_id == task_id || entry.receiver_task_id == Some(task_id)
            }) {
                *entry = None;
            }
        }
    }

    pub(super) fn release_ipc_priorities_for_process(&mut self, process_id: u64) {
        for index in 0..MAX_TASK {
            let Some(entry) = self.ipc_priority_donations[index] else {
                continue;
            };
            if entry.receiver_process_id == Some(process_id)
                || self.task_belongs_to_process(entry.donor_task_id, process_id)
                || entry
                    .receiver_task_id
                    .is_some_and(|task_id| self.task_belongs_to_process(task_id, process_id))
            {
                self.ipc_priority_donations[index] = None;
            }
        }
    }

    fn task_belongs_to_process(&self, task_id: u64, process_id: u64) -> bool {
        self.find_task_slot(task_id).is_some_and(|slot| {
            self.contexts[slot].is_some_and(|context| context.process_id == Some(process_id))
        })
    }

    pub(super) fn set_next_spawn_pick_hint(&mut self, task_id: u64) {
        let Some(slot) = self.find_task_slot(task_id) else {
            self.next_spawn_pick_hint = None;
            return;
        };
        let Some(context) = self.contexts[slot] else {
            self.next_spawn_pick_hint = None;
            return;
        };
        if !context.ready || !self.context_is_schedulable(slot, context) {
            self.next_spawn_pick_hint = None;
            return;
        }
        self.apply_ipc_donation(slot);
        self.next_spawn_pick_hint = Some(slot);
    }

    fn handoff_hint_eligible(&self, slot: usize) -> bool {
        let Some(context) = self.contexts.get(slot).and_then(|context| *context) else {
            return false;
        };
        context.ready && self.context_is_schedulable(slot, context)
    }

    fn apply_ipc_donation(&mut self, target_slot: usize) {
        if target_slot == self.current_task || target_slot >= MAX_TASK {
            return;
        }
        let Some(target) = self.contexts[target_slot] else {
            return;
        };
        if !target.ready || !self.context_is_schedulable(target_slot, target) {
            return;
        }
        let Some(current) = self.contexts[self.current_task] else {
            return;
        };
        if !self.is_fair_candidate_slot(target_slot)
            || !self.is_fair_candidate_slot(self.current_task)
        {
            return;
        }
        let caller_floor = current.vruntime_ns.saturating_sub(IPC_DONATION_BONUS_NS);
        let class_floor = self
            .slot_class(target_slot)
            .map(|class| {
                self.min_ready_vruntime_in_class(class)
                    .saturating_sub(IPC_DONATION_BONUS_NS)
            })
            .unwrap_or(caller_floor);
        let donated_floor = caller_floor.min(class_floor);
        if let Some(target) = self.contexts[target_slot].as_mut() {
            target.vruntime_ns = target.vruntime_ns.min(donated_floor);
        }
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
                .pick_min_vruntime_in_class(self.current_task, SchedClass::System)
                .is_some()
        {
            return None;
        }
        Some(slot)
    }

    fn next_pick_hint_candidate_slot(&self) -> Option<usize> {
        self.pick_hint_candidate_slot(self.next_pick_hint)
    }

    fn next_pick_hint_ready_slot(&self) -> Option<usize> {
        self.pick_hint_ready_slot(self.next_pick_hint)
    }

    fn take_next_spawn_pick_hint_ready_slot(&mut self) -> Option<usize> {
        if self.next_spawn_pick_hint.is_some()
            && self
                .pick_hint_candidate_slot(self.next_spawn_pick_hint)
                .is_none()
        {
            self.next_spawn_pick_hint = None;
            return None;
        }
        let slot = self.pick_hint_candidate_slot(self.next_spawn_pick_hint)?;
        self.next_spawn_pick_hint = None;
        Some(slot)
    }

    fn take_next_pick_hint_ready_slot(&mut self) -> Option<usize> {
        if self.next_pick_hint.is_some() && self.next_pick_hint_candidate_slot().is_none() {
            self.next_pick_hint = None;
            return None;
        }
        let slot = self.next_pick_hint_ready_slot()?;
        self.next_pick_hint = None;
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
        for slot in 0..MAX_TASK {
            self.clear_slot(slot);
        }

        self.simd_states = [SimdState::new(); MAX_TASK];
        self.retired = [false; MAX_TASK];
        self.start_suspended = [false; MAX_TASK];
        self.retire_reasons = [None; MAX_TASK];
        self.current_task = ROOT_TASK_SLOT;
        self.pending_reap = false;
        self.next_pick_hint = None;
        self.next_spawn_pick_hint = None;
        self.ipc_priority_donations = [None; MAX_TASK];
        self.root_idle = false;
        self.system_dispatch_streak = 0;
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
            linux_thread_state: None,
            windows_thread_state: None,
        });
        self.starts[ROOT_TASK_SLOT] = Some(TaskStart { entry, id });

        unsafe {
            save_state(&mut self.simd_states[ROOT_TASK_SLOT]);
        }
    }

    pub(super) fn initialized(&self) -> bool {
        self.contexts[ROOT_TASK_SLOT].is_some()
    }

    pub(super) fn clear_slot(&mut self, slot: usize) {
        if let Some(task_id) = self.starts[slot].map(|start| start.id) {
            self.release_ipc_priorities_for_task(task_id);
        }
        if let Some(context) = self.contexts[slot]
            && let Some(handle) = context.process_handle
        {
            crate::debug::trace_loc!();
            let _ = process_table::detach_task(handle);
        }

        self.contexts[slot] = None;
        if self.next_pick_hint == Some(slot) {
            self.next_pick_hint = None;
        }
        if self.next_spawn_pick_hint == Some(slot) {
            self.next_spawn_pick_hint = None;
        }
        self.retired[slot] = false;
        self.start_suspended[slot] = false;
        self.retire_reasons[slot] = None;
        self.simd_states[slot] = SimdState::new();
        self.starts[slot] = None;
        self.release_stack_storage(slot);
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
        for slot in 0..MAX_TASK {
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
        for slot in 0..MAX_TASK {
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
        for donation in self.ipc_priority_donations.iter().flatten() {
            let target_is_slot = donation
                .receiver_task_id
                .is_some_and(|task_id| self.starts[slot].is_some_and(|start| start.id == task_id))
                || donation.receiver_process_id.is_some_and(|process_id| {
                    self.contexts[slot]
                        .is_some_and(|context| context.process_id == Some(process_id))
                });
            if !target_is_slot {
                continue;
            }
            let Some(donor_slot) = self.find_task_slot(donation.donor_task_id) else {
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
        slot != ROOT_TASK_SLOT || !self.root_idle
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
        let Some(context) = self.contexts[slot].as_mut() else {
            return;
        };
        let start = context.exec_start_ticks;
        if start == 0 {
            return;
        }
        let elapsed_ns = if now_ticks > start {
            Self::ticks_elapsed_ns(start, now_ticks)
        } else {
            0
        };
        let elapsed_ns = if force_min_charge {
            elapsed_ns.max(SCHED_MIN_GRANULARITY_NS)
        } else {
            elapsed_ns
        };
        if elapsed_ns == 0 {
            context.exec_start_ticks = 0;
            return;
        }
        let delta = Self::weighted_vruntime_delta(elapsed_ns, context.weight);
        context.vruntime_ns = context.vruntime_ns.saturating_add(delta);
        context.exec_start_ticks = 0;
    }

    /// Walks scheduling classes in priority order (System > User > Idle) and
    /// within each picks the smallest-vruntime ready task. The dispatcher
    /// applies the bounded User reservation before calling this normal path.
    ///
    /// Returns `None` only if literally nothing is ready and schedulable in
    /// any class (including the current task and root) — in which case
    /// `dispatch_schedule` falls back to ROOT_TASK_SLOT.
    fn pick_min_vruntime(&self, current: usize) -> Option<usize> {
        // Keep one candidate per priority band in a single fixed-table scan.
        // The previous implementation rescanned all 128 slots once per empty
        // higher band; because class resolution also walks live IPC donations,
        // an ordinary User-only workload paid up to three full classification
        // passes on every timer tick.
        let mut best_by_class = [None::<(usize, u64)>; SchedClass::COUNT];
        for slot in 0..MAX_TASK {
            if !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.ready || !self.context_is_schedulable(slot, context) {
                continue;
            }
            let Some(class) = self.slot_class(slot) else {
                continue;
            };
            let key = if slot == current {
                context.vruntime_ns.saturating_add(1)
            } else {
                context.vruntime_ns
            };
            let candidate = &mut best_by_class[class.index()];
            if candidate
                .map(|(_, best_key)| key < best_key)
                .unwrap_or(true)
            {
                *candidate = Some((slot, key));
            }
        }
        best_by_class
            .into_iter()
            .flatten()
            .next()
            .map(|(slot, _)| slot)
    }

    fn pick_min_vruntime_excluding(&self, excluded: usize) -> Option<usize> {
        let mut best_by_class = [None::<(usize, u64, usize)>; SchedClass::COUNT];
        for slot in 0..MAX_TASK {
            if slot == excluded || !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.ready || !self.context_is_schedulable(slot, context) {
                continue;
            }
            let Some(class) = self.slot_class(slot) else {
                continue;
            };
            let distance = (slot + MAX_TASK - excluded) % MAX_TASK;
            let candidate = &mut best_by_class[class.index()];
            if candidate
                .map(|(_, best_vruntime, best_distance)| {
                    context.vruntime_ns < best_vruntime
                        || (context.vruntime_ns == best_vruntime && distance < best_distance)
                })
                .unwrap_or(true)
            {
                *candidate = Some((slot, context.vruntime_ns, distance));
            }
        }
        best_by_class
            .into_iter()
            .flatten()
            .next()
            .map(|(slot, _, _)| slot)
    }

    /// Returns one ready User task after a bounded System burst. The selected
    /// User turn is not a best-effort hint: callers skip spawn/IPC handoff and
    /// min-granularity retention for this turn, so a ready critical task cannot
    /// silently bypass the reservation.
    fn reserved_user_pick(&self, current: usize) -> Option<usize> {
        self.user_reservation_due()
            .then(|| self.pick_min_vruntime_in_class(current, SchedClass::User))
            .flatten()
    }

    fn overdue_system_pick(&self, current: usize, now_ticks: u64) -> Option<usize> {
        let mut oldest: Option<(usize, u64, u64)> = None;
        for slot in 0..MAX_TASK {
            if slot == current || !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.ready
                || context.ready_since_ticks == 0
                || !self.context_is_schedulable(slot, context)
                || self.slot_class(slot) != Some(SchedClass::System)
                || Self::ticks_elapsed_ms(context.ready_since_ticks, now_ticks)
                    < SYSTEM_READY_LATENCY_BOUND_MS
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

    fn user_reservation_due(&self) -> bool {
        self.system_dispatch_streak >= MAX_CONSECUTIVE_SYSTEM_DISPATCHES
    }

    fn record_dispatch_class(&mut self, slot: usize) {
        self.system_dispatch_streak = match self.slot_class(slot) {
            Some(SchedClass::System) => self
                .system_dispatch_streak
                .saturating_add(1)
                .min(MAX_CONSECUTIVE_SYSTEM_DISPATCHES),
            Some(SchedClass::User | SchedClass::Idle) | None => 0,
        };
    }

    /// Removes the current user task's *base* System-class admission.
    ///
    /// This is an irreversible self-demotion for a process helper such as a
    /// telemetry, catalog, or untrusted client-accept worker.  It preserves
    /// the load weight and intentionally does not touch a live reply-scoped
    /// IPC donation: a caller's bounded priority inheritance must outlive a
    /// helper's local base-class choice and is released only by that reply's
    /// terminal path.
    pub(super) fn demote_current_user_task_to_user_class(&mut self) -> bool {
        let Some(context) = self.contexts[self.current_task].as_mut() else {
            return false;
        };
        if !context.user_mode {
            return false;
        }
        context.weight &= LOAD_WEIGHT_MASK;
        true
    }

    /// Picks the schedulable task with the smallest vruntime within a single
    /// class. Ties prefer rotation away from the current task to keep
    /// RR-equivalent behaviour when all weights are equal.
    fn pick_min_vruntime_in_class(&self, current: usize, class: SchedClass) -> Option<usize> {
        let mut best: Option<(usize, u64)> = None;
        for slot in 0..MAX_TASK {
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
            // Tiny additive bias for the current slot encourages rotation on
            // exact vruntime ties (the common case under equal weights), so
            // we avoid sticking on one task when several are equally idle.
            let key = if slot == current {
                ctx.vruntime_ns.saturating_add(1)
            } else {
                ctx.vruntime_ns
            };
            match best {
                None => best = Some((slot, key)),
                Some((_, bk)) if key < bk => best = Some((slot, key)),
                _ => {}
            }
        }
        best.map(|(slot, _)| slot)
    }

    fn pick_burst_alternate_in_current_class(&self, current: usize) -> Option<usize> {
        let class = self.slot_class(current)?;
        let mut best: Option<(usize, u64)> = None;
        for slot in 0..MAX_TASK {
            if slot == current || !self.is_fair_candidate_slot(slot) {
                continue;
            }
            let Some(ctx) = self.contexts[slot] else {
                continue;
            };
            if !ctx.ready || !self.context_is_schedulable(slot, ctx) {
                continue;
            }
            if self.slot_class(slot) != Some(class) {
                continue;
            }
            let key = ctx.vruntime_ns;
            if best
                .map(|(_, current_key)| key < current_key)
                .unwrap_or(true)
            {
                best = Some((slot, key));
            }
        }
        best.map(|(slot, _)| slot)
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
        let sample = LONG_WAIT_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
        if sample >= MAX_LONG_WAIT_LOGS {
            return;
        }
        // `self.current_task` is still the slot being preempted right now: it is
        // the task that held CPU while the picked slot sat ready. Logging it
        // names the suspected cond_resched offender directly.
        let (from_slot, from_task, from_pid) = self.describe_current_task();
        crate::debug::println!(
            "scheduler long ready wait: slot={} task={} pid={} elapsed_ms={} from_slot={} from_task={} from_pid={}",
            slot,
            task_id.unwrap_or(0),
            process_id,
            elapsed_ms,
            from_slot,
            from_task,
            from_pid,
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
        let sample = LONG_WAIT_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
        if sample >= MAX_LONG_WAIT_LOGS {
            return;
        }
        // The waker (current task) and its identity isolate "service was idle"
        // (waker is some peer reaching it for the first time in a while) from
        // "scheduler dropped the wakeup" (waker is the IPC replier and the
        // gap should have been ms, not s).
        let (from_slot, from_task, from_pid) = self.describe_current_task();
        crate::debug::println!(
            "scheduler long blocked wait: slot={} task={} pid={} elapsed_ms={} woken_by_slot={} woken_by_task={} woken_by_pid={}",
            slot,
            task_id.unwrap_or(0),
            process_id,
            elapsed_ms,
            from_slot,
            from_task,
            from_pid,
        );
    }

    fn describe_current_task(&self) -> (usize, u64, u64) {
        let slot = self.current_task;
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

    fn retire_slot(&mut self, slot: usize, reason: TaskRetireReason) {
        if slot == ROOT_TASK_SLOT {
            panic!("scheduler root kernel task cannot be retired");
        }

        if let Some(task_id) = self.starts[slot].map(|start| start.id) {
            self.release_ipc_priorities_for_task(task_id);
        }
        self.retired[slot] = true;
        self.pending_reap = true;
        if let Some(context) = self.contexts[slot].as_mut() {
            context.ready = false;
        }
        self.retire_reasons[slot] = Some(reason);
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
        let base = self.stack_storage(slot).as_ptr() as *const u64;
        for index in 0..canary_words {
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
        (base + TASK_STACK_GUARD_BYTES, base + TASK_STACK_SIZE)
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
        self.stack_bounds(slot).1 & !0xF
    }

    pub(super) fn current_saved_rsp(&self) -> usize {
        self.contexts[self.current_task]
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

        let context = self.contexts[self.current_task].as_mut()?;
        let previous = (
            context.alternate_kernel_stack_base,
            context.alternate_kernel_stack_top,
        );
        context.alternate_kernel_stack_base = base;
        context.alternate_kernel_stack_top = top;
        Some(previous)
    }

    pub(super) fn restore_current_alternate_kernel_stack(&mut self, previous: (u64, u64)) {
        if let Some(context) = self.contexts[self.current_task].as_mut() {
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

    fn context_is_schedulable(&self, slot: usize, context: TaskContext) -> bool {
        self.context_validation_error(slot, context, context.saved_rsp)
            .is_none()
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
                    vruntime_ns: self
                        .last_min_vruntime_ns
                        .saturating_add(SCHED_NEW_TASK_VRUNTIME_PENALTY_NS),
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
                    linux_thread_state: None,
                    windows_thread_state: None,
                });
                self.simd_states[slot] = SimdState::new();
                self.starts[slot] = Some(TaskStart { entry, id });
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
                    vruntime_ns: self
                        .last_min_vruntime_ns
                        .saturating_add(SCHED_NEW_TASK_VRUNTIME_PENALTY_NS),
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
                    linux_thread_state: bootstrap.linux_thread_state,
                    windows_thread_state: bootstrap.windows_thread_state.map(|mut state| {
                        state.thread_id = id;
                        state
                    }),
                });
                self.simd_states[slot] = SimdState::new();
                self.starts[slot] = Some(TaskStart {
                    entry: idle_entry,
                    id,
                });
                self.start_suspended[slot] = start_suspended;
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
                    vruntime_ns: self
                        .last_min_vruntime_ns
                        .saturating_add(SCHED_NEW_TASK_VRUNTIME_PENALTY_NS),
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
                    linux_thread_state: bootstrap.linux_thread_state,
                    windows_thread_state: bootstrap.windows_thread_state.map(|mut state| {
                        state.thread_id = id;
                        state
                    }),
                });
                self.simd_states[slot] = SimdState::new();
                self.starts[slot] = Some(TaskStart {
                    entry: idle_entry,
                    id,
                });
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
                    vruntime_ns: self
                        .last_min_vruntime_ns
                        .saturating_add(SCHED_NEW_TASK_VRUNTIME_PENALTY_NS),
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
                    linux_thread_state: None,
                    windows_thread_state: None,
                });
                self.simd_states[slot] = SimdState::new();
                self.starts[slot] = Some(TaskStart {
                    entry: super::noop_task_entry,
                    id,
                });
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
                return Some(slot);
            }
        }

        None
    }

    #[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
    pub(super) fn allocate_user_thread_slot(
        &mut self,
        id: u64,
        bootstrap: UserTaskBootstrap,
        user_cs: u64,
        user_ss: u64,
        rflags: u64,
    ) -> Option<(usize, u32)> {
        let current = self.contexts[self.current_task]?;
        if !current.user_mode {
            return None;
        }

        let root_phys = current.address_space_root;
        let process_handle = current.process_handle?;
        let process_id = current.process_id?;
        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            if self.contexts[slot].is_none() {
                self.reset_stack_storage(slot)?;
                if let Some(thread_state) = bootstrap.windows_thread_state {
                    let init_result = process_table::with_process_state_mut(
                        process_handle,
                        |_, process_state| {
                            process::initialize_windows_thread_identifiers(
                                process_state.address_space_mut(),
                                thread_state.teb_address,
                                process_id,
                                id,
                            )
                        },
                    )?;
                    if let Err(error) = init_result {
                        panic!("failed to initialize windows thread ids: {:?}", error);
                    }
                }
                process_table::attach_task(process_handle)?;
                let (kernel_stack_base, kernel_stack_top) = self.stack_bounds(slot);
                self.contexts[slot] = Some(TaskContext {
                    saved_rsp: self
                        .init_user_task_context(slot, &bootstrap, user_cs, user_ss, rflags),
                    // Clone publication is transactional. The child must not
                    // execute until the caller has committed parent_tid and
                    // child_tid in the shared address space.
                    ready: false,
                    ready_since_ticks: 0,
                    blocked: true,
                    blocked_since_ticks: crate::arch::rtc::ticks(),
                    wake_armed: false,
                    // POSIX threads share one process scheduling policy.  A
                    // hard-coded default here creates a cross-class priority
                    // inversion when a System UI thread waits on a helper it
                    // just cloned.  Inherit the parent's base load weight and
                    // fair position; transient IPC donations remain scoped to
                    // their reply capability and are intentionally not copied.
                    weight: current.weight,
                    vruntime_ns: current
                        .vruntime_ns
                        .saturating_add(SCHED_NEW_TASK_VRUNTIME_PENALTY_NS),
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
                    process_id: Some(process_id),
                    user_stack: bootstrap.user_stack,
                    linux_thread_state: bootstrap.linux_thread_state,
                    windows_thread_state: bootstrap.windows_thread_state.map(|mut state| {
                        state.thread_id = id;
                        state
                    }),
                });
                self.simd_states[slot] = SimdState::new();
                self.start_suspended[slot] = true;
                self.starts[slot] = Some(TaskStart {
                    entry: super::noop_task_entry,
                    id,
                });
                return Some((slot, current.weight));
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
        self.retire_slot(
            slot,
            TaskRetireReason::CorruptedContext { saved_rsp, reason },
        );
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
            self.current_task,
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

    fn retire_invalid_ready_tasks(&mut self) {
        for slot in 1..MAX_TASK {
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if let Err(reason) =
                self.validate_saved_context(slot, context.user_mode, context.saved_rsp)
            {
                self.log_invalid_context(slot, context.saved_rsp, reason, "ready-scan");
                self.retire_slot_due_to_invalid_context(slot, context.saved_rsp, reason);
            }
        }
    }

    pub(super) fn on_timer_interrupt(&mut self, current_rsp: usize) -> (usize, u16) {
        self.dispatch_schedule(current_rsp, false)
    }

    pub(super) fn on_voluntary_yield(&mut self, current_rsp: usize) -> (usize, u16) {
        self.dispatch_schedule(current_rsp, true)
    }

    fn dispatch_schedule(&mut self, current_rsp: usize, voluntary_yield: bool) -> (usize, u16) {
        let current_slot = self.current_task;
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

        self.retire_invalid_ready_tasks();

        // Refresh cached min_vruntime: this is fed to newly-spawned tasks so
        // they do not preempt the rest of the system on creation alone.
        self.last_min_vruntime_ns = self.min_ready_vruntime();

        // Pick order:
        //  1. Spawn handoff hint (if still ready/schedulable).
        //  2. IPC donation hint (if still ready/schedulable).
        //  3. CFS-like smallest vruntime among ready tasks.
        //  4. Root task as the unconditional fallback.
        // Hints short-circuit CFS only for direct handoff; otherwise vruntime
        // decides fairness.
        // A caller that committed a synchronous IPC block has no remaining
        // work until its exact receiver runs. Honor that reply-scoped direct
        // handoff before the unrelated User reservation; selecting a polling
        // User task here can otherwise strand the entire causal chain. Normal
        // yields and spawn hints still remain behind the bounded reservation.
        let blocking_ipc_handoff = self.contexts[current_slot]
            .is_some_and(|context| context.blocked)
            .then(|| self.take_next_pick_hint_ready_slot())
            .flatten();
        let (next_idx, ipc_handoff, reserved_user_pick) = match blocking_ipc_handoff {
            Some(receiver_slot) => (receiver_slot, true, None),
            None => match self.reserved_user_pick(current_slot) {
                Some(user_slot) => (user_slot, false, Some(user_slot)),
                None => {
                    let spawn_hint = self.take_next_spawn_pick_hint_ready_slot();
                    let hint = spawn_hint.or_else(|| self.take_next_pick_hint_ready_slot());
                    let overdue = self.overdue_system_pick(current_slot, now_ticks);
                    let cfs_pick = if voluntary_yield {
                        self.pick_min_vruntime_excluding(current_slot)
                            .or_else(|| self.pick_min_vruntime(current_slot))
                    } else {
                        self.pick_min_vruntime(current_slot)
                    };
                    match (hint, overdue, cfs_pick) {
                        (Some(hint_slot), _, _) => (hint_slot, true, None),
                        (None, Some(overdue_slot), _) => (overdue_slot, true, None),
                        (None, None, Some(slot)) => (slot, false, None),
                        (None, None, None) => (ROOT_TASK_SLOT, false, None),
                    }
                }
            },
        };

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
        if let Some(next) = self.contexts[next_idx] {
            match self.context_validation_error(next_idx, next, next.saved_rsp) {
                None => {
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
                    self.current_task = next_idx;
                    return (next.saved_rsp, self.scheduler_tick_divisor);
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
        // Keep running current: refresh its exec_start_ticks so subsequent
        // vruntime accounting sees a non-zero baseline.
        if let Some(ctx) = self.contexts[current_slot].as_mut() {
            ctx.exec_start_ticks = now_ticks;
            ctx.ready = false;
        }
        (current.saved_rsp, self.scheduler_tick_divisor)
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

    // Kernel-thread start metadata is kept for upcoming worker-thread instrumentation.
    #[allow(dead_code)]
    pub(super) fn current_task_start(&self) -> Option<TaskStart> {
        self.starts[self.current_task].filter(|_| {
            self.contexts[self.current_task]
                .map(|ctx| !ctx.user_mode)
                .unwrap_or(false)
        })
    }

    pub(super) fn current_task_is_user_task(&self) -> bool {
        self.contexts[self.current_task]
            .map(|context| context.user_mode)
            .unwrap_or(false)
    }

    pub(super) fn current_task_is_retired(&self) -> bool {
        self.retired[self.current_task]
    }

    pub(super) fn current_task_is_blocked(&self) -> bool {
        self.contexts[self.current_task]
            .map(|context| context.blocked)
            .unwrap_or(false)
    }

    pub(super) fn current_process_handle(&self) -> Option<ProcessHandle> {
        self.contexts[self.current_task]?.process_handle
    }

    pub(super) fn prepare_current_task_execution(&self) {
        let current =
            self.contexts[self.current_task].expect("scheduler selected a missing task context");
        let return_to_user = self.context_returns_to_user(current);
        self.validate_saved_context(self.current_task, current.user_mode, current.saved_rsp)
            .expect("scheduler selected an invalid task context");
        crate::memory::paging::load_address_space_phys(PhysAddr::new(current.address_space_root));
        if current.kernel_stack_top != 0 {
            crate::arch::gdt::set_privilege_stack(current.kernel_stack_top);
            crate::user::syscall::set_kernel_stack_top(current.kernel_stack_top);
        }

        let fs_base = current
            .linux_thread_state
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

    pub(super) fn reap_inactive_retired_slots(&mut self) -> usize {
        if !self.pending_reap {
            return 0;
        }

        let active_root = self.contexts[self.current_task].map(|ctx| ctx.address_space_root);
        let mut still_pending = false;
        let mut reaped = 0;

        for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
            if !self.retired[slot] {
                continue;
            }

            let Some(context) = self.contexts[slot] else {
                self.retired[slot] = false;
                continue;
            };

            if context.user_mode && Some(context.address_space_root) == active_root {
                still_pending = true;
                continue;
            }

            crate::debug::trace_loc!();
            self.log_retired_slot(slot, context);
            self.clear_slot(slot);
            reaped += 1;
        }

        self.pending_reap = still_pending;
        reaped + process_table::reap_exited_processes()
    }

    pub(super) fn save_current_simd_state(&mut self) {
        unsafe {
            save_state(&mut self.simd_states[self.current_task]);
        }
    }

    pub(super) fn restore_current_simd_state(&self) {
        unsafe {
            restore_state(&self.simd_states[self.current_task]);
        }
    }

    pub(super) fn current_user_process_binding(
        &self,
    ) -> Option<(u64, UserAbi, ProcessHandle, ConsoleSessionHandle)> {
        let slot = self.current_task;
        let context = self.contexts[slot]?;
        if !context.user_mode {
            return None;
        }

        let thread_id = self.starts[slot].map(|start| start.id)?;
        let abi = context.user_abi?;
        let process_handle = context.process_handle?;
        Some((thread_id, abi, process_handle, context.console_session))
    }

    pub(super) fn current_linux_thread_state(&self) -> Option<LinuxThreadState> {
        let slot = self.current_task;
        let context = self.contexts[slot]?;
        if !context.user_mode || context.user_abi != Some(UserAbi::Linux) {
            return None;
        }
        context.linux_thread_state
    }

    pub(super) fn current_user_stack_state(&self) -> Option<UserStackState> {
        let slot = self.current_task;
        let context = self.contexts[slot]?;
        if !context.user_mode {
            return None;
        }
        context.user_stack
    }

    pub(super) fn current_user_id(&self) -> Option<u64> {
        let context = self.contexts[self.current_task]?;
        if !context.user_mode {
            return None;
        }

        self.starts[self.current_task].map(|start| start.id)
    }

    pub(super) fn current_user_log_ids(&self) -> Option<(u64, u64)> {
        let slot = self.current_task;
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
        self.starts[self.current_task].map(|start| start.id)
    }

    pub(super) fn current_console_session(&self) -> Option<ConsoleSessionHandle> {
        self.contexts[self.current_task].map(|context| context.console_session)
    }

    pub(super) fn current_linux_thread_binding(&mut self) -> Option<CurrentLinuxThreadBinding> {
        let slot = self.current_task;
        let context = self.contexts[slot].as_mut()?;
        if !context.user_mode {
            return None;
        }

        let abi = context.user_abi?;
        let tid = self.starts[slot].map(|start| start.id)?;
        let process_handle = context.process_handle?;
        let linux_thread_state = NonNull::new(ptr::addr_of_mut!(context.linux_thread_state))?;
        Some(CurrentLinuxThreadBinding {
            process_handle,
            tid,
            abi,
            linux_thread_state,
        })
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
        address_space: ProcessAddressSpace,
        mut bootstrap: UserTaskBootstrap,
    ) -> bool {
        let slot = self.current_task;
        let Some(current_context) = self.contexts[slot] else {
            return false;
        };
        if !current_context.user_mode {
            return false;
        }

        let Some(process_handle) = current_context.process_handle else {
            return false;
        };
        let Some(linux_process_state) = bootstrap.linux_process_state.take() else {
            return false;
        };
        let Some(linux_memory_map) = bootstrap.linux_memory_map.take() else {
            return false;
        };
        let Some(linux_runtime_profile) = bootstrap.linux_runtime_profile.take() else {
            return false;
        };

        let Some(process_id) = current_context.process_id else {
            return false;
        };
        let exec_path = String::from(bootstrap.exec_path());
        let (sibling_slots, sibling_count) =
            self.collect_process_sibling_slots(slot, process_handle);
        let new_root = address_space.root_phys().as_u64();
        let preserved_signal_mask = current_context
            .linux_thread_state
            .map(|state| state.signal_mask)
            .unwrap_or(0);
        if let Some(thread_state) = bootstrap.linux_thread_state.as_mut() {
            thread_state.signal_mask = preserved_signal_mask;
            thread_state.pending_signals = 0;
        }
        let new_fs_base = bootstrap
            .linux_thread_state
            .map(|state| state.fs_base)
            .unwrap_or(0);

        {
            let Some(context) = self.contexts[slot].as_mut() else {
                return false;
            };
            context.address_space_root = new_root;
            context.user_abi = Some(bootstrap.abi);
            context.console_session = bootstrap.console_session;
            context.user_stack = bootstrap.user_stack;
            context.linux_thread_state = bootstrap.linux_thread_state;
            context.blocked = false;
            context.blocked_since_ticks = 0;
            context.ready = true;
            context.ready_since_ticks = crate::arch::rtc::ticks();
        }

        self.retired[slot] = false;
        self.retire_reasons[slot] = None;
        self.simd_states[slot] = SimdState::new();
        self.starts[slot] = Some(TaskStart {
            entry: super::noop_task_entry,
            id: process_id,
        });

        crate::memory::paging::load_address_space_phys(PhysAddr::new(new_root));
        process_table::replace_for_exec(
            process_handle,
            address_space,
            linux_process_state,
            linux_memory_map,
            linux_runtime_profile,
            exec_path.as_str(),
        )
        .expect("current process handle disappeared during exec");

        for sibling_slot in sibling_slots.iter().take(sibling_count) {
            self.clear_slot(*sibling_slot);
        }

        FsBase::write(VirtAddr::new(new_fs_base));
        true
    }

    pub(super) fn exec_user_process_by_pid(
        &mut self,
        process_id: u64,
        thread_id: u64,
        address_space: ProcessAddressSpace,
        mut bootstrap: UserTaskBootstrap,
    ) -> bool {
        let Some(slot) = self.find_linux_thread_slot(process_id, thread_id) else {
            return false;
        };
        let Some(current_context) = self.contexts[slot] else {
            return false;
        };
        let Some(process_handle) = current_context.process_handle else {
            return false;
        };
        let Some(linux_process_state) = bootstrap.linux_process_state.take() else {
            return false;
        };
        let Some(linux_memory_map) = bootstrap.linux_memory_map.take() else {
            return false;
        };
        let Some(linux_runtime_profile) = bootstrap.linux_runtime_profile.take() else {
            return false;
        };
        let exec_path = String::from(bootstrap.exec_path());
        let (sibling_slots, sibling_count) =
            self.collect_process_sibling_slots(slot, process_handle);
        let new_root = address_space.root_phys().as_u64();
        let preserved_signal_mask = current_context
            .linux_thread_state
            .map(|state| state.signal_mask)
            .unwrap_or(0);
        if let Some(thread_state) = bootstrap.linux_thread_state.as_mut() {
            thread_state.signal_mask = preserved_signal_mask;
            thread_state.pending_signals = 0;
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
            let Some(context) = self.contexts[slot].as_mut() else {
                return false;
            };
            context.saved_rsp = saved_rsp;
            context.address_space_root = new_root;
            context.user_abi = Some(bootstrap.abi);
            context.console_session = bootstrap.console_session;
            context.user_stack = bootstrap.user_stack;
            context.linux_thread_state = bootstrap.linux_thread_state;
            context.blocked = false;
            context.blocked_since_ticks = 0;
            context.ready = true;
            context.ready_since_ticks = crate::arch::rtc::ticks();
        }
        self.retired[slot] = false;
        self.retire_reasons[slot] = None;
        self.simd_states[slot] = SimdState::new();
        self.starts[slot] = Some(TaskStart {
            entry: super::noop_task_entry,
            id: process_id,
        });

        if slot == self.current_task {
            crate::memory::paging::load_address_space_phys(PhysAddr::new(new_root));
            FsBase::write(VirtAddr::new(new_fs_base));
        }
        process_table::replace_for_exec(
            process_handle,
            address_space,
            linux_process_state,
            linux_memory_map,
            linux_runtime_profile,
            exec_path.as_str(),
        )
        .expect("target process handle disappeared during exec");

        for sibling_slot in sibling_slots.iter().take(sibling_count) {
            self.clear_slot(*sibling_slot);
        }
        true
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
            thread_state: context.linux_thread_state?,
        })
    }

    pub(super) fn queue_linux_signal(
        &mut self,
        process_id: u64,
        task_id: u64,
        signal: u64,
    ) -> bool {
        let signal_bit = crate::user::sysops::linux::linux_signal_bit(signal);

        for slot in 0..MAX_TASK {
            if self.retired[slot] {
                continue;
            }
            let Some(context) = self.contexts[slot].as_mut() else {
                continue;
            };
            if !context.user_mode || context.user_abi != Some(UserAbi::Linux) {
                continue;
            }
            let Some(start) = self.starts[slot] else {
                continue;
            };
            if start.id != task_id {
                continue;
            }
            if context.process_id != Some(process_id) {
                continue;
            }
            if signal == 0 {
                return true;
            }
            let Some(thread_state) = context.linux_thread_state.as_mut() else {
                continue;
            };

            let Some(signal_bit) = signal_bit else {
                return false;
            };
            thread_state.pending_signals |= signal_bit;
            if thread_state.signal_mask & signal_bit == 0 {
                context.blocked = false;
                context.blocked_since_ticks = 0;
                context.ready = true;
                context.ready_since_ticks = crate::arch::rtc::ticks();
            }
            return true;
        }

        false
    }

    fn find_linux_thread_slot(&self, process_id: u64, thread_id: u64) -> Option<usize> {
        for slot in 0..MAX_TASK {
            if self.retired[slot] {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.user_mode || context.user_abi != Some(UserAbi::Linux) {
                continue;
            }
            if self.starts[slot].map(|start| start.id) != Some(thread_id) {
                continue;
            }
            if context.process_id == Some(process_id) {
                return Some(slot);
            }
        }
        None
    }

    fn collect_process_sibling_slots(
        &self,
        current_slot: usize,
        process_handle: ProcessHandle,
    ) -> ([usize; MAX_TASK], usize) {
        let mut slots = [0usize; MAX_TASK];
        let mut count = 0usize;
        for slot in 1..MAX_TASK {
            if slot == current_slot {
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

    pub(super) fn current_process_task_ids(&self) -> ([u64; MAX_TASK], usize) {
        let mut task_ids = [0_u64; MAX_TASK];
        let Some(process_handle) =
            self.contexts[self.current_task].and_then(|context| context.process_handle)
        else {
            return (task_ids, 0);
        };
        let mut count = 0usize;
        for slot in 1..MAX_TASK {
            if self.retired[slot]
                || self.contexts[slot].and_then(|context| context.process_handle)
                    != Some(process_handle)
            {
                continue;
            }
            let Some(task_id) = self.starts[slot].map(|start| start.id) else {
                continue;
            };
            task_ids[count] = task_id;
            count += 1;
        }
        (task_ids, count)
    }

    #[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
    fn try_grow_current_user_stack_on_fault(
        &mut self,
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rsp: u64,
    ) -> bool {
        if vector != PAGE_FAULT_VECTOR || error_code.unwrap_or(0) & 0x1 != 0 {
            return false;
        }

        let slot = self.current_task;
        let Some(context) = self.contexts[slot].as_mut() else {
            return false;
        };
        if !context.user_mode {
            return false;
        }
        let _process_id = context.process_id.unwrap_or(0);

        let Some(mut stack_state) = context.user_stack else {
            return false;
        };
        if !stack_state.contains_stack_pointer(rsp) || !stack_state.contains_reserved_address(cr2) {
            return false;
        }

        let Some((growth_start, growth_end, page_count)) = stack_state.grow_to_include_fault(cr2)
        else {
            return false;
        };

        let Some(process_handle) = context.process_handle else {
            return false;
        };
        let map_result =
            process_table::with_process_state_mut(process_handle, |_, process_state| {
                let (address_space, linux_process_state) =
                    process_state.address_space_and_linux_process_state_mut();
                let map_result = address_space.map_zeroed_user_pages_at(
                    VirtAddr::new(growth_start),
                    page_count,
                    x86_64::structures::paging::PageTableFlags::WRITABLE
                        | x86_64::structures::paging::PageTableFlags::NO_EXECUTE,
                );
                if map_result.is_err() {
                    return false;
                }

                if let Some(state) = linux_process_state.as_mut() {
                    state
                        .release_reserved_range(growth_start, growth_end)
                        .expect("user stack reserved range mismatch");
                }
                true
            })
            .unwrap_or(false);
        if !map_result {
            return false;
        }

        context.user_stack = Some(stack_state);
        debug::debug!(
            sched,
            "grew user stack pid={} slot={} cr2={:#x} rsp={:#x} new_start={:#x} pages={}",
            _process_id,
            slot,
            cr2,
            rsp,
            growth_start,
            page_count,
        );
        true
    }

    pub(super) fn retire_current_user_task_due_to_fault(
        &mut self,
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rip: u64,
        rsp: u64,
    ) -> UserFaultDisposition {
        let slot = self.current_task;
        let Some(context) = self.contexts[slot] else {
            return UserFaultDisposition::Unhandled;
        };
        if !context.user_mode {
            return UserFaultDisposition::Unhandled;
        }

        if self.try_grow_current_user_stack_on_fault(vector, error_code, cr2, rsp) {
            return UserFaultDisposition::Resumed;
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

    pub(super) fn activate_suspended_user_task(&mut self, task_id: u64) -> bool {
        let Some(slot) = self.find_user_task_slot(task_id) else {
            return false;
        };
        if self.retired[slot] || !self.start_suspended[slot] {
            return false;
        }
        self.start_suspended[slot] = false;
        if !self.wake_task_slot(slot) {
            return false;
        }
        // Activation is the supervisor's commit point. A distinct one-shot
        // spawn hint gives the newly owned task its first turn after the
        // loader/supervisor reply chain without letting an earlier create
        // race supervision. Generic IPC reply hints cannot overwrite it.
        self.set_next_spawn_pick_hint(task_id);
        true
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
        let (sibling_slots, sibling_count) =
            self.collect_process_sibling_slots(leader_slot, process_handle);
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

    pub(super) fn block_current_user_task(&mut self) -> bool {
        let slot = self.current_task;
        let Some(context) = self.contexts[slot].as_mut() else {
            return false;
        };
        if !context.user_mode {
            return false;
        }

        context.blocked = true;
        context.ready = false;
        context.ready_since_ticks = 0;
        context.blocked_since_ticks = crate::arch::rtc::ticks();
        context.wake_armed = false;
        true
    }

    pub(super) fn block_current_task(&mut self) -> bool {
        let slot = self.current_task;
        let Some(context) = self.contexts[slot].as_mut() else {
            return false;
        };
        if slot == ROOT_TASK_SLOT {
            return false;
        }

        context.blocked = true;
        context.ready = false;
        context.ready_since_ticks = 0;
        context.blocked_since_ticks = crate::arch::rtc::ticks();
        context.wake_armed = false;
        true
    }

    /// Arms a race-free block on the current task. Pair with
    /// `commit_block_current_task`. Between the two calls the caller must
    /// re-check the wakeup condition; if a wake fires in that window the
    /// commit returns `false` and the caller stays runnable instead of
    /// sleeping with a lost wakeup.
    pub(super) fn arm_block_current_task(&mut self) -> bool {
        let slot = self.current_task;
        let Some(context) = self.contexts[slot].as_mut() else {
            return false;
        };
        if slot == ROOT_TASK_SLOT {
            return false;
        }
        context.wake_armed = true;
        true
    }

    /// Cancels a previously armed block when the caller re-checked its wait
    /// condition and found work available. The task is still executing, so this
    /// must not mark it blocked.
    pub(super) fn cancel_block_current_task(&mut self) -> bool {
        let slot = self.current_task;
        let Some(context) = self.contexts[slot].as_mut() else {
            return false;
        };
        if slot == ROOT_TASK_SLOT {
            return false;
        }
        context.wake_armed = false;
        true
    }

    /// Commits a previously armed block. Returns `Some(true)` if the task was
    /// blocked, `Some(false)` if a wake raced us (wake_armed cleared by
    /// `wake_task`) and we should re-check the condition without sleeping,
    /// `None` on invalid context.
    pub(super) fn commit_block_current_task(&mut self) -> Option<bool> {
        let slot = self.current_task;
        let context = self.contexts[slot].as_mut()?;
        if slot == ROOT_TASK_SLOT {
            return None;
        }
        if !context.wake_armed {
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
                context.ready && invalid_reason.is_none(),
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
        if waker_ready && slot != self.current_task {
            let current_class = self.slot_class(self.current_task);
            let should_preempt = match (woken_class, current_class) {
                // Cross-class: a higher-priority band has work, preempt.
                (Some(wc), Some(cc)) if wc < cc => true,
                // Same class: only preempt if the wake brings the waker
                // distinctly ahead in vruntime — the CFS wake-preempt rule.
                (Some(wc), Some(cc)) if wc == cc => {
                    let current_v = self.contexts[self.current_task]
                        .map(|ctx| ctx.vruntime_ns)
                        .unwrap_or(0);
                    current_v.saturating_sub(waker_vruntime) >= SCHED_MIN_GRANULARITY_NS
                }
                _ => false,
            };
            if should_preempt {
                super::request_deferred_reschedule();
            }
        }

        true
    }

    pub(super) fn exit_current_task(&mut self) {
        crate::debug::trace_loc!();
        let slot = self.current_task;
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
        let current_slot = self.current_task;
        let Some(process_handle) =
            self.contexts[current_slot].and_then(|context| context.process_handle)
        else {
            self.exit_current_task();
            return;
        };
        let (sibling_slots, sibling_count) =
            self.collect_process_sibling_slots(current_slot, process_handle);
        for slot in sibling_slots.into_iter().take(sibling_count) {
            self.retire_slot(slot, TaskRetireReason::Exited);
        }
        self.exit_current_task();
    }

    #[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
    fn log_retired_slot(&self, slot: usize, context: TaskContext) {
        let id = self.starts[slot]
            .map(|start| start.id)
            .unwrap_or(slot as u64);
        match self.retire_reasons[slot] {
            Some(TaskRetireReason::UserFault {
                vector,
                error_code,
                cr2,
                rip,
            }) => {
                debug::warn!(
                    sched,
                    "reaped user task pid={} slot={} vector={} error={:?} cr2={:#x} rip={:#x}",
                    id,
                    slot,
                    vector,
                    error_code,
                    cr2,
                    rip,
                );
            }
            Some(TaskRetireReason::CorruptedContext { saved_rsp, reason }) => {
                let (stack_base, stack_top) = self.stack_bounds(slot);
                debug::warn!(
                    sched,
                    "reaped corrupted task pid={} slot={} user_mode={} saved_rsp={:#x} stack=[{:#x}, {:#x}) reason={}",
                    id,
                    slot,
                    context.user_mode,
                    saved_rsp,
                    stack_base,
                    stack_top,
                    reason,
                );
            }
            Some(TaskRetireReason::Terminated { requested_by_pid }) => {
                let _ = requested_by_pid;
                debug::debug!(
                    sched,
                    "reaped terminated task pid={} slot={} user_mode={} requested_by={:?}",
                    id,
                    slot,
                    context.user_mode,
                    requested_by_pid,
                );
            }
            Some(TaskRetireReason::Exited) => {
                debug::debug!(
                    sched,
                    "reaped exited task pid={} slot={} user_mode={}",
                    id,
                    slot,
                    context.user_mode,
                );
            }
            None => {
                debug::debug!(
                    sched,
                    "reaped retired task pid={} slot={} user_mode={}",
                    id,
                    slot,
                    context.user_mode,
                );
            }
        }
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

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use super::{
        ConsoleSessionHandle, MAX_CONSECUTIVE_SYSTEM_DISPATCHES, MAX_TASK, NICE_0_LOAD,
        SYSTEM_CLASS_WEIGHT_FLAG, SchedClass, Scheduler, TaskContext, TaskStart,
    };
    use crate::memory::paging::ProcessAddressSpace;
    use crate::multitask::{UserTaskBootstrap, noop_task_entry, process_table};
    use crate::user::abi::UserAbi;
    use crate::user::process_state::UserProcessState;

    fn test_user_context(handle: process_table::ProcessHandle) -> TaskContext {
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
            linux_thread_state: None,
            windows_thread_state: None,
        }
    }

    fn test_process(id: u64) -> process_table::ProcessHandle {
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
    fn collect_process_sibling_slots_returns_matching_user_slots_only() {
        let mut scheduler = Box::new(Scheduler::new());
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

        let (slots, count) = scheduler.collect_process_sibling_slots(1, owner);
        assert_eq!(count, 1);
        assert_eq!(slots[0], 2);
        assert!(slots[1..MAX_TASK].iter().all(|slot| *slot == 0));
    }

    #[test]
    fn current_process_task_ids_excludes_other_and_retired_tasks() {
        let mut scheduler = Box::new(Scheduler::new());
        let owner = test_process(11);
        let other = test_process(22);

        scheduler.current_task = 1;
        scheduler.contexts[1] = Some(test_user_context(owner));
        scheduler.contexts[2] = Some(test_user_context(owner));
        scheduler.contexts[3] = Some(test_user_context(other));
        scheduler.contexts[4] = Some(test_user_context(owner));
        scheduler.starts[1] = Some(TaskStart {
            entry: noop_task_entry,
            id: 101,
        });
        scheduler.starts[2] = Some(TaskStart {
            entry: noop_task_entry,
            id: 102,
        });
        scheduler.starts[3] = Some(TaskStart {
            entry: noop_task_entry,
            id: 201,
        });
        scheduler.starts[4] = Some(TaskStart {
            entry: noop_task_entry,
            id: 103,
        });
        scheduler.retired[4] = true;

        let (task_ids, count) = scheduler.current_process_task_ids();
        assert_eq!(count, 2);
        assert_eq!(&task_ids[..count], &[101, 102]);
    }

    #[test]
    fn terminate_user_process_retires_every_live_sibling() {
        let mut scheduler = Box::new(Scheduler::new());
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
        assert!(scheduler.retired[1]);
        assert!(scheduler.retired[2]);
        assert!(!scheduler.retired[3]);
    }

    #[test]
    fn synchronous_ipc_donation_promotes_and_revokes_a_transitive_user_chain() {
        let mut scheduler = Box::new(Scheduler::new());
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

        // A process-owned endpoint can have no receiver waiter at enqueue
        // time. Its live reply still promotes every runnable worker in the
        // owner process until the reply terminally releases the donation.
        assert!(scheduler.inherit_ipc_priority_for_process(12, 601, 62));
        assert_eq!(scheduler.slot_class(2), Some(SchedClass::System));
        assert!(scheduler.release_ipc_priority(12));
        assert_eq!(scheduler.slot_class(2), Some(SchedClass::User));
    }

    #[test]
    fn strict_class_requires_explicit_admission_not_a_large_cfs_weight() {
        let mut scheduler = Box::new(Scheduler::new());
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
    fn self_demotion_removes_only_the_base_system_class() {
        let mut scheduler = Box::new(Scheduler::new());
        let helper = test_process(73);
        let donor = test_process(74);

        let mut helper_context = test_user_context(helper);
        helper_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
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
    }

    #[test]
    fn bounded_system_burst_reserves_a_ready_user_turn() {
        let mut scheduler = Box::new(Scheduler::new());
        let system = test_process(71);
        let user = test_process(72);

        let mut system_context = test_user_context(system);
        system_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
        scheduler.contexts[1] = Some(system_context);
        scheduler.contexts[2] = Some(test_user_context(user));
        scheduler.system_dispatch_streak = MAX_CONSECUTIVE_SYSTEM_DISPATCHES;

        assert!(scheduler.user_reservation_due());
        scheduler.record_dispatch_class(2);
        assert_eq!(scheduler.system_dispatch_streak, 0);
        assert!(!scheduler.user_reservation_due());

        scheduler.record_dispatch_class(1);
        assert_eq!(scheduler.system_dispatch_streak, 1);
    }

    #[test]
    fn overdue_system_task_is_forced_after_latency_bound() {
        let mut scheduler = Box::new(Scheduler::new());
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
    fn spawn_handoff_is_one_shot_and_precedes_ipc_handoff() {
        let mut scheduler = Box::new(Scheduler::new());
        let base = crate::memory::paging::USER_SPACE_BASE;
        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let parent_bootstrap = UserTaskBootstrap::new(
            UserAbi::Linux,
            x86_64::VirtAddr::new(base + 0x2_000),
            x86_64::VirtAddr::new(base + 0x4_000),
        );
        let parent_slot = scheduler
            .allocate_user_slot(
                801,
                ProcessAddressSpace::empty_for_tests(),
                parent_bootstrap,
                None,
                crate::arch::pit::divisor_from_micros(2_000) | super::INTERACTIVE_PIT_DIVISOR_FLAG,
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("parent slot");
        scheduler.current_task = parent_slot;

        for (task_id, entry_offset, stack_offset) in
            [(802, 0x3_000, 0x5_000), (803, 0x3_800, 0x5_800)]
        {
            let bootstrap = UserTaskBootstrap::new(
                UserAbi::Linux,
                x86_64::VirtAddr::new(base + entry_offset),
                x86_64::VirtAddr::new(base + stack_offset),
            );
            let (thread_slot, inherited_weight) = scheduler
                .allocate_user_thread_slot(
                    task_id,
                    bootstrap,
                    user_cs,
                    user_ss,
                    super::RFLAGS_RESERVED_BIT_1,
                )
                .expect("thread slot");
            assert_eq!(scheduler.slot_class(thread_slot), Some(SchedClass::System));
            assert!(
                !scheduler.contexts[thread_slot]
                    .expect("thread context")
                    .ready
            );
            assert!(scheduler.start_suspended[thread_slot]);
            assert_eq!(
                inherited_weight,
                scheduler.contexts[parent_slot]
                    .expect("parent context")
                    .weight
            );
        }

        assert!(scheduler.activate_suspended_user_task(802));
        assert_eq!(scheduler.take_next_spawn_pick_hint_ready_slot(), Some(2));
        assert_eq!(scheduler.take_next_spawn_pick_hint_ready_slot(), None);

        assert!(scheduler.activate_suspended_user_task(803));
        scheduler.set_next_pick_hint(802);
        assert_eq!(scheduler.take_next_spawn_pick_hint_ready_slot(), Some(3));
        assert_eq!(scheduler.take_next_pick_hint_ready_slot(), Some(2));
    }
}
