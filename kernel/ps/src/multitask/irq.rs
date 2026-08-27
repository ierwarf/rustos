//! Per-CPU timer, software-reschedule, and reschedule-IPI entry ordering.
//!
//! - **Owner:** `kernel-ps` owns scheduling decisions; HAL owns interrupt
//!   acknowledgement and clockevent delivery.
//! - **Boundary:** Interrupt frames and deferred-reschedule requests enter the
//!   scheduler only at architecture-defined safe points.
//! - **Lifecycle:** Wake deadlines, publish/validate the trapped continuation,
//!   select, hand off, then acknowledge in the required order.
//! - **Concurrency:** Entry runs with interrupts excluded and tracked IRQ
//!   context; arbitrary kernel frames are not blindly preempted.
//! - **Failure:** Invalid user continuation retires the exact task; root/kernel
//!   continuation corruption is fatal.
//! - **Forbidden:** No policy in IRQ context, early acknowledgement that loses
//!   a deadline, or clearing a CPU's request without a same-CPU safe point.
//! - **Evidence:** `scheduler-lifecycle`, `scheduler-dispatch`, and
//!   `smp-reschedule-ipi-lifecycle`.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(not(test))]
use kernel_hal::api::cpu;
use x86_64::instructions::interrupts;

use super::{
    USER_RETURN_RESCHEDULE_ARMED, context::SavedContext, scheduler::Scheduler,
    scheduler_initialized, scheduler_mut,
};

#[cfg(test)]
mod test_support;
#[cfg(test)]
pub(super) use test_support::{
    flush_deferred_target_reschedules, prepare_test_deferred_target_reschedule,
    reset_test_deferred_target_reschedule_flush_epoch, test_deferred_target_reschedule_flush_epoch,
    test_deferred_target_reschedule_pending_mask, test_deferred_target_reschedule_sent_mask,
};

static AP_FIRST_WORK_DISPATCH_RECORDED: [AtomicBool;
    nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static CPU_FIRST_CLOCKEVENT_RECORDED: [AtomicBool; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static CPU_FIRST_USER_DISPATCH_RECORDED: [AtomicBool;
    nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static CPU_FIRST_RESCHEDULE_IPI_RECORDED: [AtomicBool;
    nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static TARGET_RESCHEDULE_IPI_PENDING: AtomicU64 = AtomicU64::new(0);
static PERIODIC_FAIRNESS_TICKS: [core::sync::atomic::AtomicU8;
    nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { core::sync::atomic::AtomicU8::new(0) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
// Explicit wake/block/handoff requests schedule immediately. A runnable peer
// that merely needs fair CPU time is sampled at this bounded per-CPU cadence,
// keeping the legacy global backend below its convoy knee until the per-CPU
// backend cutover. Eight 1 ms ticks remain below the 20 ms maximum fair burst.
const PERIODIC_FAIRNESS_INTERVAL_TICKS: u8 = 8;

pub fn timer_interrupt_handler_addr() -> u64 {
    crate::lowlevel::interrupts::timer_interrupt_handler_addr()
}

pub fn rtc_interrupt_handler_addr() -> u64 {
    crate::lowlevel::interrupts::rtc_interrupt_handler_addr()
}

pub fn software_schedule_interrupt_handler_addr() -> u64 {
    crate::lowlevel::interrupts::software_schedule_interrupt_handler_addr()
}

fn current_cpu_index() -> usize {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    assert!(
        logical_index < nucleus_core::util::lockdep::MAX_TRACKED_CPUS,
        "scheduler invariant: current logical CPU exceeds reschedule capacity"
    );
    logical_index
}

fn user_return_reschedule_flag(logical_index: usize) -> &'static AtomicU64 {
    USER_RETURN_RESCHEDULE_ARMED
        .get(logical_index)
        .expect("scheduler invariant: user-return CPU index out of range")
}

fn cpu_lifecycle_generation(logical_index: usize) -> u64 {
    #[cfg(test)]
    {
        assert!(logical_index < nucleus_core::util::lockdep::MAX_TRACKED_CPUS);
        return 1;
    }
    #[cfg(not(test))]
    {
        let logical_index = u8::try_from(logical_index)
            .expect("scheduler invariant: logical CPU index exceeds lifecycle identity");
        cpu::lifecycle_snapshot(logical_index)
            .filter(|snapshot| snapshot.state == cpu::CpuLifecycleState::Online)
            .map(|snapshot| snapshot.generation)
            .expect("scheduler invariant: reschedule target is not Online")
    }
}

fn set_local_deferred_reschedule() {
    let logical_index = current_cpu_index();
    let _ = super::reschedule_observation::publish_request(
        logical_index,
        cpu_lifecycle_generation(logical_index),
    );
}

const fn remote_notification_required(target: usize, current: usize) -> bool {
    target != current
}

const fn periodic_user_continuation_is_local(
    interrupted_user_frame: bool,
    runqueue_work_pending: bool,
    deferred_reschedule_pending: bool,
    user_return_reschedule_pending: bool,
) -> bool {
    interrupted_user_frame
        && !runqueue_work_pending
        && !deferred_reschedule_pending
        && !user_return_reschedule_pending
}

const fn periodic_idle_continuation_is_local(
    interrupted_kernel_frame: bool,
    current_task_is_idle: bool,
    local_work_pending: bool,
    deferred_reschedule_pending: bool,
    user_return_reschedule_pending: bool,
) -> bool {
    interrupted_kernel_frame
        && current_task_is_idle
        && !local_work_pending
        && !deferred_reschedule_pending
        && !user_return_reschedule_pending
}

fn retain_periodic_continuation(logical_index: usize, context_ptr: *const SavedContext) -> bool {
    // SAFETY: the interrupt stub supplies one complete frame for the duration
    // of this IRQ dispatch. Reading CS does not consume or mutate continuation
    // ownership.
    let interrupted_user_frame =
        unsafe { &*context_ptr }.cs == crate::arch::gdt::user_code_selector().0 as u64;
    let interrupted_kernel_frame = !interrupted_user_frame;
    let local_work_pending = super::scheduler::local_dispatch_work_pending(logical_index);
    let deferred_reschedule_pending = super::reschedule_observation::request_pending(logical_index);
    let user_return_reschedule_pending =
        user_return_reschedule_flag(logical_index).load(Ordering::Acquire) != 0;
    let fair_dispatch_due = if local_work_pending
        && !deferred_reschedule_pending
        && !user_return_reschedule_pending
        && interrupted_user_frame
    {
        let elapsed = PERIODIC_FAIRNESS_TICKS[logical_index]
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if elapsed >= PERIODIC_FAIRNESS_INTERVAL_TICKS {
            PERIODIC_FAIRNESS_TICKS[logical_index].store(0, Ordering::Relaxed);
            true
        } else {
            false
        }
    } else {
        PERIODIC_FAIRNESS_TICKS[logical_index].store(0, Ordering::Relaxed);
        false
    };
    periodic_user_continuation_is_local(
        interrupted_user_frame,
        local_work_pending && fair_dispatch_due,
        deferred_reschedule_pending,
        user_return_reschedule_pending,
    ) || periodic_idle_continuation_is_local(
        interrupted_kernel_frame,
        super::cpu_local::current_cpu_task_is_idle(logical_index),
        local_work_pending,
        deferred_reschedule_pending,
        user_return_reschedule_pending,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RescheduleIpiGate {
    RetainRequest,
    AcknowledgeOnly,
    Dispatch,
}

const fn reschedule_ipi_gate(
    scheduler_ready: bool,
    preemption_disabled: bool,
    request_present: bool,
) -> RescheduleIpiGate {
    if !scheduler_ready || preemption_disabled {
        RescheduleIpiGate::RetainRequest
    } else if !request_present {
        RescheduleIpiGate::AcknowledgeOnly
    } else {
        RescheduleIpiGate::Dispatch
    }
}

pub(crate) fn install_interrupt_dispatch_callbacks() {
    crate::lowlevel::interrupts::register_context_switch_commit(
        super::cpu_local::commit_context_switch,
    );
    crate::lowlevel::interrupts::register_timer_interrupt_dispatch(timer_interrupt_dispatch);
    crate::lowlevel::interrupts::register_rtc_interrupt_dispatch(rtc_interrupt_dispatch);
    crate::lowlevel::interrupts::register_software_schedule_interrupt_dispatch(
        software_schedule_interrupt_dispatch,
    );
    crate::lowlevel::interrupts::register_reschedule_ipi_interrupt_dispatch(
        reschedule_ipi_interrupt_dispatch,
    );
}

extern "C" fn timer_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    // The low-level timer assembly enters this function directly rather than
    // the generic PIC wrapper, so publish the IRQ context for the complete
    // clockevent, scheduler, PIT, and EOI transaction.
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    let logical_index = current_cpu_index();
    // Test the preemption gate before sleeper callbacks, debug decoration, or
    // even the lock-backed scheduler-initialized snapshot. An interrupt may
    // land in the small interval after a raw lock raised preemption depth but
    // before its lock class was published; that valid owner must simply defer
    // all scheduler-facing clockevent work to the next absolute tick.
    if nucleus_core::util::lockdep::preemption_disabled() {
        set_local_deferred_reschedule();
        complete_clockevent(logical_index);
        return context_ptr;
    }
    // The deadline registry is one global clock base, not eight per-CPU timer
    // bases. Only its BSP/PIT owner may expire it. Letting every LAPIC tick
    // walk the same registry reissued each still-unacknowledged wake once per
    // CPU and formed an SMP thundering herd on the global scheduler lock.
    // Absolute deadlines make a delayed owner edge catch up without extending
    // the wait; AP clockevents remain independent scheduler preemption edges.
    if logical_index == 0 {
        crate::arch::rtc::service_clock_event();
    }
    if !scheduler_initialized() {
        complete_clockevent(logical_index);
        return context_ptr;
    }
    // The common steady-state case on an otherwise idle CPU has exactly one
    // running user task and no queued or remotely published work. A periodic
    // clockevent cannot change that decision. Keep the CPU-local continuation
    // without bouncing the lifecycle-global scheduler cache line; a local
    // enqueue, remote mailbox edge, affinity/retirement request, or syscall-tail
    // request disables this fast path before the next selection point.
    if retain_periodic_continuation(logical_index, context_ptr) {
        complete_clockevent(logical_index);
        return context_ptr;
    }
    // Idle CPUs never poll foreign runqueues from a periodic interrupt. Wake
    // placement targets an exact CPU and publishes its mailbox edge; active
    // balancing is source-owned. Treating any foreign runnable as local work
    // made every idle CPU enter the lifecycle-global scheduler on every tick,
    // creating an O(CPUs x ticks) convoy even though none could claim that
    // foreign queue directly.
    //
    // A task may have published its blocked/retired state immediately before
    // completing a bounded raw-lock transaction. That state alone must not
    // authorize the timer path to switch stacks while the preemption guard is
    // live. The eventual explicit scheduler handoff below the lock remains the
    // sole transition point.
    let current_rsp = context_ptr as usize;
    // SAFETY: the interrupt stub supplied a complete CPU-local frame, IRQ
    // exclusion prevents same-CPU nested scheduling, and the scheduler lock
    // serializes all cross-CPU task-state mutation.
    let (next_rsp, user_dispatch, runtime_profile, reschedule_goal) = unsafe {
        let mut scheduler = scheduler_mut();
        if timer_interrupted_kernel_frame(context_ptr, &scheduler) {
            complete_clockevent(logical_index);
            return context_ptr;
        }
        // A user-frame clockevent is itself a generation-bound scheduling
        // request. Publish it before clearing coalesced deferred work so every
        // Online CPU can prove that an actual request, rather than merely an
        // interrupt arrival, reached a scheduler safe point.
        let logical_index = current_cpu_index();
        let _ = super::reschedule_observation::publish_request(
            logical_index,
            cpu_lifecycle_generation(logical_index),
        );
        let reschedule_goal = super::reschedule_observation::claim_pending(logical_index)
            .expect("timer request lost before same-CPU scheduling safe point");
        // ORDERING: Release completes same-CPU request consumption before the
        // selected continuation is restored.
        user_return_reschedule_flag(logical_index).store(0, Ordering::Release);
        scheduler.record_runtime_profile_entry(super::scheduler::SchedulerEntryCause::Timer);
        scheduler.save_current_simd_state();
        let dispatch = scheduler.on_timer_interrupt(current_rsp);
        if logical_index == 0 {
            crate::arch::pit::set_divisor(0, dispatch.tick_divisor);
        }
        scheduler.prepare_dispatched_task_execution(dispatch);
        scheduler.restore_current_simd_state();
        // The BSP may stay in its kernel idle continuation while an AP owns
        // all user work. The global scheduler owner already serializes and
        // destructively gates the one-second snapshot, so any CPU that traps
        // a user frame may claim the due profile without duplicate output.
        let runtime_profile = scheduler.take_runtime_profile(crate::arch::rtc::ticks());
        (
            dispatch.next_rsp,
            scheduler.current_task_is_user_task(),
            runtime_profile,
            reschedule_goal,
        )
    };

    super::scheduler::publish_scheduler_runtime_profile(runtime_profile);
    super::reschedule_observation::record_consumption(
        logical_index,
        reschedule_goal,
        super::reschedule_observation::RescheduleRoute::Timer,
    );
    record_first_user_dispatch(logical_index, user_dispatch);
    record_atomic_activation_dispatch(logical_index);
    record_first_ap_work_dispatch(logical_index);
    complete_clockevent(logical_index);
    next_rsp as *mut SavedContext
}

fn record_first_ap_work_dispatch(logical_index: usize) {
    if logical_index == 0 {
        return;
    }
    let Some(witness) = nucleus_core::util::lockdep::scheduler_dispatch_witness(logical_index)
    else {
        return;
    };
    if witness.to_idle
        || AP_FIRST_WORK_DISPATCH_RECORDED[logical_index]
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return;
    }
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "smp-ap-first-work-dispatch",
        logical_index as u64,
        (witness.to_task << 32) | witness.to_slot as u64,
    );
}

fn complete_clockevent(logical_index: usize) {
    if logical_index == 0 {
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_1_OFFSET);
    } else {
        // Rearm only after all scheduler/debug work is complete. At high CPU
        // counts that work can exceed one 1 ms period; arming at entry then
        // makes the deadline expire while IF=0 and causes an immediate
        // interrupt on IRET, starving a newly selected user context before
        // its first instruction. Absolute catch-up still preserves monotonic
        // time while guaranteeing the next edge is strictly in the future.
        crate::arch::timer::arm_next_tick();
        crate::arch::msi::local_apic_eoi();
    }
    record_first_cpu_event(
        &CPU_FIRST_CLOCKEVENT_RECORDED,
        "smp-cpu-first-clockevent",
        logical_index,
    );
}

extern "C" fn rtc_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    // Like the PIT/LAPIC dispatcher, the low-level RTC stub enters this
    // function directly. Keep IRQ context published through wake processing,
    // scheduler selection, acknowledgement, and the final stack decision;
    // `rtc::on_interrupt`'s inner guard covers only device accounting.
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    crate::arch::rtc::on_interrupt();
    if nucleus_core::util::lockdep::preemption_disabled() {
        set_local_deferred_reschedule();
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
        return context_ptr;
    }
    if !scheduler_initialized() {
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
        return context_ptr;
    }

    if retain_periodic_continuation(current_cpu_index(), context_ptr) {
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
        return context_ptr;
    }

    // Drain sleeper wakeups BEFORE the scheduler pick so newly-ready tasks
    // are visible to `dispatch_schedule`. The previous ordering ran the pick
    // first, then woke sleepers — so a sleeper whose target tick had just
    // arrived waited a full extra IRQ (and longer, since a heavier User-class
    // task could keep being picked over and over while the wake was stale).
    // Matches Linux's `tick_sched_timer` -> `update_process_times` -> wake
    // ordering, and the Fuchsia Zircon "wake then reschedule" pattern.
    let current_rsp = context_ptr as usize;
    let interrupted_kernel_frame = unsafe {
        let scheduler = scheduler_mut();
        timer_interrupted_kernel_frame(context_ptr, &scheduler)
    };
    if interrupted_kernel_frame {
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
        return context_ptr;
    }

    let logical_index = current_cpu_index();
    let _ = super::reschedule_observation::publish_request(
        logical_index,
        cpu_lifecycle_generation(logical_index),
    );
    let reschedule_goal = super::reschedule_observation::claim_pending(logical_index)
        .expect("RTC request lost before same-CPU scheduling safe point");
    let (next_rsp, user_dispatch, runtime_profile) = unsafe {
        let mut scheduler = scheduler_mut();
        scheduler.record_runtime_profile_entry(super::scheduler::SchedulerEntryCause::Rtc);
        scheduler.save_current_simd_state();
        let dispatch = scheduler.on_timer_interrupt(current_rsp);
        scheduler.prepare_dispatched_task_execution(dispatch);
        scheduler.restore_current_simd_state();
        let runtime_profile = scheduler.take_runtime_profile(crate::arch::rtc::ticks());
        (
            dispatch.next_rsp,
            scheduler.current_task_is_user_task(),
            runtime_profile,
        )
    };

    super::scheduler::publish_scheduler_runtime_profile(runtime_profile);
    super::reschedule_observation::record_consumption(
        logical_index,
        reschedule_goal,
        super::reschedule_observation::RescheduleRoute::Rtc,
    );
    record_first_user_dispatch(logical_index, user_dispatch);
    record_atomic_activation_dispatch(logical_index);
    crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
    next_rsp as *mut SavedContext
}

fn timer_interrupted_kernel_frame(context_ptr: *const SavedContext, scheduler: &Scheduler) -> bool {
    let context = unsafe { &*context_ptr };
    if context.cs == crate::arch::gdt::user_code_selector().0 as u64 {
        return false;
    }
    if scheduler.current_task_is_retired() || scheduler.current_task_is_blocked() {
        return false;
    }
    if scheduler.current_task_is_user_task() {
        set_local_deferred_reschedule();
        return true;
    }
    false
}

pub fn reschedule_if_requested() {
    if nucleus_core::util::lockdep::preemption_disabled() {
        return;
    }
    let logical_index = current_cpu_index();
    if let Some(goal) = super::reschedule_observation::claim_pending(logical_index) {
        yield_now();
        super::reschedule_observation::record_consumption(
            logical_index,
            goal,
            super::reschedule_observation::RescheduleRoute::LocalSafePoint,
        );
    }
}

pub fn cond_resched() {
    reschedule_if_requested();
}

/// Consume a timer or latency-handoff request from the common syscall tail.
///
/// The syscall entry deliberately executes with IF=1. A software interrupt
/// raised here therefore saves an IF-enabled kernel continuation, so the
/// scheduler may switch away and later resume it safely. Clearing the request
/// without switching starves peer services; programming a short periodic PIT
/// instead creates a VM-exit storm under long syscalls.
pub fn reschedule_deferred_from_interruptible_syscall() {
    if nucleus_core::util::lockdep::preemption_disabled() {
        return;
    }
    let logical_index = current_cpu_index();
    let reschedule_goal = super::reschedule_observation::claim_pending(logical_index);
    if consume_syscall_tail_reschedule(
        reschedule_goal.is_some(),
        user_return_reschedule_flag(logical_index),
    ) {
        crate::lowlevel::interrupts::trigger_software_schedule_interruptible();
        if let Some(reschedule_goal) = reschedule_goal {
            super::reschedule_observation::record_consumption(
                logical_index,
                reschedule_goal,
                super::reschedule_observation::RescheduleRoute::SyscallTail,
            );
        }
    }
}

fn consume_syscall_tail_reschedule(
    deferred_claimed: bool,
    user_return: &core::sync::atomic::AtomicU64,
) -> bool {
    let user_return = user_return.swap(0, Ordering::AcqRel);
    deferred_claimed || user_return != 0
}

pub(crate) fn request_deferred_reschedule() {
    let source_cpu = current_cpu_index();
    let _ = super::reschedule_observation::publish_request(
        source_cpu,
        cpu_lifecycle_generation(source_cpu),
    );
}

/// Publish one scheduling request to the CPU that owns the runnable task.
///
/// Remote wake placement has already transferred exact task custody to the
/// target mailbox before this function is called.  Notification is therefore
/// a coalesced hint, not ownership: a same-CPU target consumes the durable bit
/// at its next safe point and a remote target receives exactly one private IPI
/// on the 0->1 edge.  Ordinary wakeups never broadcast to unrelated CPUs.
#[cfg(not(test))]
pub(super) fn request_target_reschedule(target: usize) {
    assert!(
        target < nucleus_core::util::lockdep::MAX_TRACKED_CPUS,
        "scheduler targeted reschedule CPU exceeds capacity"
    );
    let snapshot = {
        let logical_index = u8::try_from(target)
            .expect("scheduler targeted reschedule CPU exceeds lifecycle identity");
        cpu::lifecycle_snapshot(logical_index)
            .filter(|snapshot| snapshot.state == cpu::CpuLifecycleState::Online)
            .expect("scheduler targeted reschedule CPU is not Online")
    };
    let publication = super::reschedule_observation::publish_request(target, snapshot.generation);
    if !publication.newly_pending {
        return;
    }
    let source = current_cpu_index();
    if !remote_notification_required(target, source) {
        return;
    }
    if nucleus_core::util::lockdep::preemption_disabled() {
        // ORDERING: Release transfers notification custody to the scheduler
        // unlock safe point. The target request bit and mailbox payload were
        // both published before this bit becomes visible.
        TARGET_RESCHEDULE_IPI_PENDING.fetch_or(1_u64 << target, Ordering::Release);
        return;
    }
    send_target_reschedule_ipi(target, snapshot.generation, publication.sequence);
}

#[cfg(not(test))]
fn send_target_reschedule_ipi(target: usize, generation: u64, sequence: u64) {
    let descriptor = cpu::topology()
        .and_then(|topology| {
            topology
                .cpus()
                .iter()
                .find(|descriptor| usize::from(descriptor.logical_index) == target)
                .copied()
        })
        .expect("scheduler targeted reschedule CPU has no topology descriptor");
    super::reschedule_observation::record_notification(target, generation, sequence);
    crate::arch::msi::send_reschedule_ipi(descriptor.apic_id).unwrap_or_else(|error| {
        panic!(
            "scheduler invariant: targeted reschedule IPI to logical CPU {} APIC {} failed: {error:?}",
            descriptor.logical_index, descriptor.apic_id
        )
    });
}

/// Emit exact target notification edges after the scheduler raw lock has
/// actually been released. Multiple flushers are safe: each target bit
/// coalesces IPIs and a racing publication remains for the next safe point.
fn flush_deferred_target_reschedules_with(
    scheduler_ready: bool,
    current_cpu: usize,
    mut online_generation: impl FnMut(usize) -> Option<u64>,
    mut pending_sequence: impl FnMut(usize) -> Option<u64>,
    mut send: impl FnMut(usize, u64, u64),
) {
    if !scheduler_ready {
        return;
    }
    // ORDERING: AcqRel claims every target whose mailbox/request became
    // durable while a raw scheduler owner disabled preemption. New target bits
    // racing this swap remain for the next safe point.
    let targeted = TARGET_RESCHEDULE_IPI_PENDING.swap(0, Ordering::AcqRel);
    for target in 0..nucleus_core::util::lockdep::MAX_TRACKED_CPUS {
        if targeted & (1_u64 << target) == 0 {
            continue;
        }
        if !remote_notification_required(target, current_cpu) {
            // The durable local request is enough. xAPIC fixed self-IPIs are
            // forbidden and the return path is already a safe point.
            continue;
        }
        let generation = online_generation(target)
            .unwrap_or_else(|| panic!("scheduler deferred target is not Online: {target}"));
        if let Some(sequence) = pending_sequence(target) {
            send(target, generation, sequence);
        }
    }
}

#[cfg(not(test))]
pub(super) fn flush_deferred_target_reschedules() {
    flush_deferred_target_reschedules_with(
        scheduler_initialized(),
        current_cpu_index(),
        |target| {
            let logical_index =
                u8::try_from(target).expect("scheduler deferred target exceeds lifecycle identity");
            cpu::lifecycle_snapshot(logical_index)
                .filter(|snapshot| snapshot.state == cpu::CpuLifecycleState::Online)
                .map(|snapshot| snapshot.generation)
        },
        |target| {
            super::reschedule_observation::request_pending(target)
                .then(|| super::reschedule_observation::request_sequence_snapshot(target))
        },
        send_target_reschedule_ipi,
    );
}

/// Requests a voluntary switch in the common interruptible syscall tail.
pub(crate) fn request_user_return_reschedule() {
    // ORDERING: Release publishes the request to this CPU's syscall tail.
    user_return_reschedule_flag(current_cpu_index()).store(1, Ordering::Release);
}

extern "C" fn reschedule_ipi_interrupt_dispatch(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    let logical_index = current_cpu_index();
    // ORDERING: Acquire observes the target CPU's durable request publication
    // before choosing whether the notification may consume it.
    let request_present = super::reschedule_observation::request_pending(logical_index);
    let preemption_disabled = nucleus_core::util::lockdep::preemption_disabled();
    // An interrupted scheduler/raw lock may be the lock this callback would
    // otherwise acquire, so the preemption gate must be evaluated first.
    let scheduler_ready = !preemption_disabled && scheduler_initialized();
    match reschedule_ipi_gate(scheduler_ready, preemption_disabled, request_present) {
        RescheduleIpiGate::RetainRequest | RescheduleIpiGate::AcknowledgeOnly => {
            // RetainRequest leaves the durable bit armed. A syscall tail,
            // cond_resched, timer edge, or later IPI consumes it after the
            // interrupted raw-lock section.
            crate::arch::msi::local_apic_eoi();
            return context_ptr;
        }
        RescheduleIpiGate::Dispatch => {}
    }
    let reschedule_goal = super::reschedule_observation::claim_pending(logical_index)
        .expect("scheduler invariant: reschedule request disappeared on its target CPU");
    // The delivered IPI covers every request coalesced before this exact claim,
    // including a publication that raced the sender-side notify marker.
    super::reschedule_observation::record_notification(
        logical_index,
        cpu_lifecycle_generation(logical_index),
        reschedule_goal,
    );

    let current_rsp = context_ptr as usize;
    // SAFETY: the IPI stub supplied a complete CPU-local frame, IRQ exclusion
    // prevents same-CPU nested scheduling, and the global scheduler lock
    // serializes cross-CPU task-state mutation.
    let (next_rsp, user_dispatch, runtime_profile) = unsafe {
        let mut scheduler = scheduler_mut();
        if timer_interrupted_kernel_frame(context_ptr, &scheduler) {
            crate::arch::msi::local_apic_eoi();
            return context_ptr;
        }
        scheduler
            .record_runtime_profile_entry(super::scheduler::SchedulerEntryCause::RescheduleIpi);
        scheduler.save_current_simd_state();
        let dispatch = scheduler.on_voluntary_yield(current_rsp);
        scheduler.prepare_dispatched_task_execution(dispatch);
        scheduler.restore_current_simd_state();
        let runtime_profile = scheduler.take_runtime_profile(crate::arch::rtc::ticks());
        (
            dispatch.next_rsp,
            scheduler.current_task_is_user_task(),
            runtime_profile,
        )
    };
    super::scheduler::publish_scheduler_runtime_profile(runtime_profile);
    super::reschedule_observation::record_consumption(
        logical_index,
        reschedule_goal,
        super::reschedule_observation::RescheduleRoute::RemoteIpi,
    );
    record_first_user_dispatch(logical_index, user_dispatch);
    record_atomic_activation_dispatch(logical_index);
    record_first_ap_work_dispatch(logical_index);
    crate::arch::msi::local_apic_eoi();
    record_first_cpu_event(
        &CPU_FIRST_RESCHEDULE_IPI_RECORDED,
        "smp-cpu-first-reschedule-ipi",
        logical_index,
    );
    next_rsp as *mut SavedContext
}

extern "C" fn software_schedule_interrupt_dispatch(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
    assert!(
        !nucleus_core::util::lockdep::preemption_disabled(),
        "software scheduler entered while raw spin lock held depth={} class={:?}",
        nucleus_core::util::lockdep::preemption_depth(),
        nucleus_core::util::lockdep::current_lock_class()
    );
    if !scheduler_initialized() {
        return context_ptr;
    }
    let logical_index = current_cpu_index();
    // The software interrupt is an explicit local scheduling request. Bind it
    // to the current CPU lifecycle before entering the serialized scheduler
    // transaction, then attest consumption after the selected frame is ready.
    let _ = super::reschedule_observation::publish_request(
        logical_index,
        cpu_lifecycle_generation(logical_index),
    );
    let reschedule_goal = super::reschedule_observation::claim_pending(logical_index)
        .expect("software scheduling request lost before dispatch");
    let current_rsp = context_ptr as usize;
    let (next_rsp, user_dispatch, runtime_profile) = unsafe {
        let mut scheduler = scheduler_mut();
        scheduler.record_runtime_profile_entry(super::scheduler::SchedulerEntryCause::Software);
        scheduler.save_current_simd_state();
        // Voluntary yield: floor the vruntime charge so a sub-tick yield can't
        // accumulate 0 vruntime and re-win CFS. Timer-driven preemption still
        // uses the unfloored path.
        let dispatch = scheduler.on_voluntary_yield(current_rsp);
        scheduler.prepare_dispatched_task_execution(dispatch);
        scheduler.restore_current_simd_state();
        let runtime_profile = scheduler.take_runtime_profile(crate::arch::rtc::ticks());
        (
            dispatch.next_rsp,
            scheduler.current_task_is_user_task(),
            runtime_profile,
        )
    };

    super::scheduler::publish_scheduler_runtime_profile(runtime_profile);
    super::reschedule_observation::record_consumption(
        logical_index,
        reschedule_goal,
        super::reschedule_observation::RescheduleRoute::LocalSafePoint,
    );
    record_first_user_dispatch(logical_index, user_dispatch);
    record_atomic_activation_dispatch(logical_index);
    next_rsp as *mut SavedContext
}

fn record_atomic_activation_dispatch(logical_index: usize) {
    let Some(witness) = nucleus_core::util::lockdep::scheduler_dispatch_witness(logical_index)
    else {
        return;
    };
    if !witness.atomic_activation_handoff {
        return;
    }
    // The scheduler journal records this while holding its raw owner. Emit
    // only after that guard is gone so reliable SMP diagnostics cannot block
    // a global scheduling transaction.
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "smp-atomic-activation-dispatch",
        logical_index as u64,
        (witness.to_task << 32) | witness.to_slot as u64,
    );
}

fn record_first_user_dispatch(logical_index: usize, user_dispatch: bool) {
    if user_dispatch {
        record_first_cpu_event(
            &CPU_FIRST_USER_DISPATCH_RECORDED,
            "smp-cpu-first-user-dispatch",
            logical_index,
        );
    }
}

fn record_first_cpu_event(
    recorded: &[AtomicBool; nucleus_core::util::lockdep::MAX_TRACKED_CPUS],
    name: &'static str,
    logical_index: usize,
) {
    if recorded[logical_index]
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        name,
        logical_index as u64,
        1,
    );
}

pub fn yield_now() {
    let preemption = nucleus_core::util::lockdep::preemption_snapshot();
    assert!(
        preemption.depth == 0,
        "task yielded while raw spin lock held cpu={} apic={:#x} depth={} held_depth={} pending_depth={} class={:?}",
        preemption.logical_cpu,
        preemption.apic_id,
        preemption.depth,
        preemption.held_depth,
        preemption.pending_depth,
        preemption.top_class,
    );
    interrupts::without_interrupts(|| {
        crate::lowlevel::interrupts::trigger_software_schedule();
    });
}

/// Linearizes a successful block commit with the software schedule trap.
///
/// A separate `commit_block_current_task(); yield_now();` sequence leaves an
/// interruptible gap in which a wake can make the still-executing task
/// runnable before it nevertheless enters the voluntary schedule path. Keep
/// interrupts excluded from the scheduler state transition through the trap,
/// matching the formal `CommitCurrentBlock` transition where a blocked task
/// ceases to own the CPU at the same linearization point.
pub fn commit_block_current_task_and_yield() -> Option<bool> {
    let preemption = nucleus_core::util::lockdep::preemption_snapshot();
    assert!(
        preemption.depth == 0,
        "task blocked while raw spin lock held cpu={} apic={:#x} depth={} held_depth={} pending_depth={} class={:?}",
        preemption.logical_cpu,
        preemption.apic_id,
        preemption.depth,
        preemption.held_depth,
        preemption.pending_depth,
        preemption.top_class,
    );
    interrupts::without_interrupts(|| {
        let committed =
            current_wait_commit_or_fallback(super::scheduler::commit_current_wait(), || unsafe {
                scheduler_mut().commit_block_current_task()
            });
        if committed == Some(true) {
            crate::lowlevel::interrupts::trigger_software_schedule();
        }
        committed
    })
}

#[inline]
fn current_wait_commit_or_fallback(
    fast: Option<bool>,
    fallback: impl FnOnce() -> Option<bool>,
) -> Option<bool> {
    fast.map(Some).unwrap_or_else(fallback)
}

/// Linearizes the Phase-5 same-CPU call transfer with the schedule trap.
/// A committed sender cannot execute with blocked owner state while interrupts
/// are enabled; every rejected result returns without trapping so the caller
/// can rollback the fixed frame and restart the slowpath from scratch.
pub fn commit_fast_ipc_call_handoff_and_yield(
    endpoint: u64,
    reply: u64,
    receiver_task_id: u64,
) -> super::scheduler::FastIpcCallHandoffOutcome {
    let preemption = nucleus_core::util::lockdep::preemption_snapshot();
    assert!(
        preemption.depth == 0,
        "fast IPC committed while raw spin lock held cpu={} apic={:#x} depth={} held_depth={} pending_depth={} class={:?}",
        preemption.logical_cpu,
        preemption.apic_id,
        preemption.depth,
        preemption.held_depth,
        preemption.pending_depth,
        preemption.top_class,
    );
    interrupts::without_interrupts(|| {
        let outcome = unsafe {
            scheduler_mut().commit_fast_ipc_call_handoff(endpoint, reply, receiver_task_id)
        };
        if matches!(
            outcome,
            super::scheduler::FastIpcCallHandoffOutcome::CommittedSameCpu
                | super::scheduler::FastIpcCallHandoffOutcome::CommittedCrossCpu
        ) {
            crate::lowlevel::interrupts::trigger_software_schedule();
        }
        outcome
    })
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::{
        RescheduleIpiGate, consume_syscall_tail_reschedule, current_wait_commit_or_fallback,
        periodic_idle_continuation_is_local, periodic_user_continuation_is_local,
        remote_notification_required, reschedule_ipi_gate,
    };

    #[test]
    fn current_wait_commit_uses_catalog_only_when_owner_publication_cannot_answer() {
        let mut fallbacks = 0;
        assert_eq!(
            current_wait_commit_or_fallback(Some(true), || {
                fallbacks += 1;
                Some(false)
            }),
            Some(true)
        );
        assert_eq!(fallbacks, 0, "ordinary commit reopened the catalog");
        assert_eq!(
            current_wait_commit_or_fallback(None, || {
                fallbacks += 1;
                Some(false)
            }),
            Some(false)
        );
        assert_eq!(fallbacks, 1, "unpublished owner lost its safe fallback");
    }

    #[test]
    fn periodic_tick_stays_local_only_without_any_dispatch_authority() {
        assert!(periodic_user_continuation_is_local(
            true, false, false, false
        ));
        assert!(!periodic_user_continuation_is_local(
            false, false, false, false
        ));
        assert!(!periodic_user_continuation_is_local(
            true, true, false, false
        ));
        assert!(!periodic_user_continuation_is_local(
            true, false, true, false
        ));
        assert!(!periodic_user_continuation_is_local(
            true, false, false, true
        ));
    }

    #[test]
    fn periodic_idle_tick_stays_local_only_without_any_queue_or_request() {
        assert!(periodic_idle_continuation_is_local(
            true, true, false, false, false
        ));
        assert!(!periodic_idle_continuation_is_local(
            true, true, true, false, false
        ));
        assert!(!periodic_idle_continuation_is_local(
            true, true, false, true, false
        ));
        assert!(!periodic_idle_continuation_is_local(
            true, false, false, false, false
        ));
        assert!(!periodic_idle_continuation_is_local(
            false, true, false, false, false
        ));
    }

    #[test]
    fn exact_target_notification_rejects_self_ipi() {
        assert!(!remote_notification_required(2, 2));
        assert!(remote_notification_required(1, 2));
        assert!(remote_notification_required(3, 2));
    }

    #[test]
    fn syscall_tail_consumes_every_deferred_or_handoff_request_exactly_once() {
        let handoff = AtomicU64::new(0);
        assert!(!consume_syscall_tail_reschedule(false, &handoff));

        assert!(consume_syscall_tail_reschedule(true, &handoff));
        assert!(!consume_syscall_tail_reschedule(false, &handoff));

        handoff.store(1, Ordering::Release);
        assert!(consume_syscall_tail_reschedule(true, &handoff));
        assert_eq!(handoff.load(Ordering::Acquire), 0);
    }

    #[test]
    fn reschedule_ipi_gate_retains_locked_work_and_dispatches_only_at_safe_point() {
        assert_eq!(
            reschedule_ipi_gate(false, false, true),
            RescheduleIpiGate::RetainRequest
        );
        assert_eq!(
            reschedule_ipi_gate(true, true, true),
            RescheduleIpiGate::RetainRequest
        );
        assert_eq!(
            reschedule_ipi_gate(true, false, false),
            RescheduleIpiGate::AcknowledgeOnly
        );
        assert_eq!(
            reschedule_ipi_gate(true, false, true),
            RescheduleIpiGate::Dispatch
        );
    }
}
