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
    DEFERRED_RESCHEDULE_REQUESTED, USER_RETURN_RESCHEDULE_ARMED, context::SavedContext,
    scheduler::Scheduler, scheduler_initialized, scheduler_mut,
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

fn deferred_reschedule_flag(logical_index: usize) -> &'static AtomicU64 {
    DEFERRED_RESCHEDULE_REQUESTED
        .get(logical_index)
        .expect("scheduler invariant: deferred-reschedule CPU index out of range")
}

fn user_return_reschedule_flag(logical_index: usize) -> &'static AtomicU64 {
    USER_RETURN_RESCHEDULE_ARMED
        .get(logical_index)
        .expect("scheduler invariant: user-return CPU index out of range")
}

fn set_local_deferred_reschedule() {
    // ORDERING: Release publishes the request to this CPU's next safe point.
    deferred_reschedule_flag(current_cpu_index()).store(1, Ordering::Release);
}

fn arm_remote_reschedule(flag: &AtomicU64) -> bool {
    // ORDERING: AcqRel is the 0->1 coalescing edge. It publishes work before
    // the ICR write and observes a request already owned by the target CPU.
    flag.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
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
    if logical_index != 0 {
        crate::arch::timer::arm_next_tick();
    }
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
    // Expire sleepers against invariant-TSC/HPET time before selecting the
    // next task. This is deliberately independent of how many PIT/RTC edges
    // the hypervisor delivered: one delayed edge catches up every absolute
    // deadline that passed while the vCPU was descheduled.
    crate::arch::rtc::service_clock_event();
    if !scheduler_initialized() {
        complete_clockevent(logical_index);
        return context_ptr;
    }
    // A task may have published its blocked/retired state immediately before
    // completing a bounded raw-lock transaction. That state alone must not
    // authorize the timer path to switch stacks while the preemption guard is
    // live. The eventual explicit scheduler handoff below the lock remains the
    // sole transition point.
    let current_rsp = context_ptr as usize;
    // SAFETY: the interrupt stub supplied a complete CPU-local frame, IRQ
    // exclusion prevents same-CPU nested scheduling, and the scheduler lock
    // serializes all cross-CPU task-state mutation.
    let (next_rsp, user_dispatch) = unsafe {
        let mut scheduler = scheduler_mut();
        if timer_interrupted_kernel_frame(context_ptr, &scheduler) {
            complete_clockevent(logical_index);
            return context_ptr;
        }
        // A real user-frame clockevent consumes any request not already
        // serviced by the IF-enabled common syscall tail.
        let logical_index = current_cpu_index();
        // ORDERING: Release completes same-CPU request consumption before the
        // selected continuation is restored.
        user_return_reschedule_flag(logical_index).store(0, Ordering::Release);
        deferred_reschedule_flag(logical_index).store(0, Ordering::Release);
        scheduler.save_current_simd_state();
        let (next_rsp, next_pit_divisor) = scheduler.on_timer_interrupt(current_rsp);
        if logical_index == 0 {
            crate::arch::pit::set_divisor(0, next_pit_divisor);
        }
        scheduler.prepare_current_task_execution();
        scheduler.restore_current_simd_state();
        (next_rsp, scheduler.current_task_is_user_task())
    };

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
        crate::arch::msi::local_apic_eoi();
    }
    record_first_cpu_event(
        &CPU_FIRST_CLOCKEVENT_RECORDED,
        "smp-cpu-first-clockevent",
        logical_index,
    );
}

extern "C" fn rtc_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
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

    let (next_rsp, user_dispatch) = unsafe {
        let mut scheduler = scheduler_mut();
        // ORDERING: Release completes same-CPU request consumption before the
        // selected continuation is restored.
        deferred_reschedule_flag(current_cpu_index()).store(0, Ordering::Release);
        scheduler.save_current_simd_state();
        let (next_rsp, _next_pit_divisor) = scheduler.on_timer_interrupt(current_rsp);
        scheduler.prepare_current_task_execution();
        scheduler.restore_current_simd_state();
        (next_rsp, scheduler.current_task_is_user_task())
    };

    record_first_user_dispatch(current_cpu_index(), user_dispatch);
    record_atomic_activation_dispatch(current_cpu_index());
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
    // ORDERING: AcqRel consumes this CPU's published request exactly once.
    if deferred_reschedule_flag(current_cpu_index()).swap(0, Ordering::AcqRel) != 0 {
        yield_now();
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
    if consume_syscall_tail_reschedule(
        deferred_reschedule_flag(logical_index),
        user_return_reschedule_flag(logical_index),
    ) {
        crate::lowlevel::interrupts::trigger_software_schedule_interruptible();
    }
}

fn consume_syscall_tail_reschedule(
    deferred: &core::sync::atomic::AtomicU64,
    user_return: &core::sync::atomic::AtomicU64,
) -> bool {
    let deferred = deferred.swap(0, Ordering::AcqRel);
    let user_return = user_return.swap(0, Ordering::AcqRel);
    deferred != 0 || user_return != 0
}

pub(crate) fn request_deferred_reschedule() {
    let source_cpu = current_cpu_index();
    // ORDERING: Release publishes the local request before any return to a
    // scheduler safe point.
    deferred_reschedule_flag(source_cpu).store(1, Ordering::Release);
    #[cfg(not(test))]
    {
        // Callers may publish a request while holding the scheduler or another
        // raw lock. The durable local bit is sufficient; the current/next safe
        // point will fan it out after the preemption gate opens.
        if nucleus_core::util::lockdep::preemption_disabled() {
            return;
        }
        if !scheduler_initialized() {
            return;
        }
        let Some(topology) = cpu::topology() else {
            return;
        };
        for descriptor in topology.cpus() {
            let target = usize::from(descriptor.logical_index);
            if target == source_cpu {
                continue;
            }
            let Some(snapshot) = cpu::lifecycle_snapshot(descriptor.logical_index) else {
                panic!(
                    "scheduler invariant: admitted CPU {} has no lifecycle slot",
                    descriptor.logical_index
                );
            };
            if snapshot.state != cpu::CpuLifecycleState::Online {
                continue;
            }
            if !arm_remote_reschedule(deferred_reschedule_flag(target)) {
                continue;
            }
            crate::arch::msi::send_reschedule_ipi(descriptor.apic_id).unwrap_or_else(|error| {
                panic!(
                    "scheduler invariant: reschedule IPI to logical CPU {} APIC {} failed: {error:?}",
                    descriptor.logical_index, descriptor.apic_id
                )
            });
        }
    }
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
    let request_present = deferred_reschedule_flag(logical_index).load(Ordering::Acquire) != 0;
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
    // ORDERING: AcqRel consumes the work publication associated with this
    // target CPU's 0->1 IPI coalescing edge.
    assert_ne!(
        deferred_reschedule_flag(logical_index).swap(0, Ordering::AcqRel),
        0,
        "scheduler invariant: reschedule request disappeared on its target CPU"
    );

    let current_rsp = context_ptr as usize;
    // SAFETY: the IPI stub supplied a complete CPU-local frame, IRQ exclusion
    // prevents same-CPU nested scheduling, and the global scheduler lock
    // serializes cross-CPU task-state mutation.
    let (next_rsp, user_dispatch) = unsafe {
        let mut scheduler = scheduler_mut();
        if timer_interrupted_kernel_frame(context_ptr, &scheduler) {
            crate::arch::msi::local_apic_eoi();
            return context_ptr;
        }
        scheduler.save_current_simd_state();
        let (next_rsp, _next_pit_divisor) = scheduler.on_voluntary_yield(current_rsp);
        scheduler.prepare_current_task_execution();
        scheduler.restore_current_simd_state();
        (next_rsp, scheduler.current_task_is_user_task())
    };
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
    let current_rsp = context_ptr as usize;
    let (next_rsp, user_dispatch) = unsafe {
        let mut scheduler = scheduler_mut();
        scheduler.save_current_simd_state();
        // Voluntary yield: floor the vruntime charge so a sub-tick yield can't
        // accumulate 0 vruntime and re-win CFS. Timer-driven preemption still
        // uses the unfloored path.
        let (next_rsp, _next_pit_divisor) = scheduler.on_voluntary_yield(current_rsp);
        scheduler.prepare_current_task_execution();
        scheduler.restore_current_simd_state();
        (next_rsp, scheduler.current_task_is_user_task())
    };

    record_first_user_dispatch(current_cpu_index(), user_dispatch);
    record_atomic_activation_dispatch(current_cpu_index());
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
        let committed = unsafe { scheduler_mut().commit_block_current_task() };
        if committed == Some(true) {
            crate::lowlevel::interrupts::trigger_software_schedule();
        }
        committed
    })
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::{
        RescheduleIpiGate, arm_remote_reschedule, consume_syscall_tail_reschedule,
        reschedule_ipi_gate,
    };

    #[test]
    fn syscall_tail_consumes_every_deferred_or_handoff_request_exactly_once() {
        let deferred = AtomicU64::new(0);
        let handoff = AtomicU64::new(0);
        assert!(!consume_syscall_tail_reschedule(&deferred, &handoff));

        deferred.store(1, Ordering::Release);
        assert!(consume_syscall_tail_reschedule(&deferred, &handoff));
        assert!(!consume_syscall_tail_reschedule(&deferred, &handoff));

        deferred.store(1, Ordering::Release);
        handoff.store(1, Ordering::Release);
        assert!(consume_syscall_tail_reschedule(&deferred, &handoff));
        assert_eq!(deferred.load(Ordering::Acquire), 0);
        assert_eq!(handoff.load(Ordering::Acquire), 0);
    }

    #[test]
    fn remote_reschedule_flags_are_cpu_isolated_and_coalesce_without_loss() {
        let flags = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
        assert!(arm_remote_reschedule(&flags[1]));
        assert!(!arm_remote_reschedule(&flags[1]));
        assert_eq!(flags[0].load(Ordering::Acquire), 0);
        assert_eq!(flags[1].swap(0, Ordering::AcqRel), 1);
        assert!(arm_remote_reschedule(&flags[1]));
        assert!(arm_remote_reschedule(&flags[2]));
        assert_eq!(flags[1].load(Ordering::Acquire), 1);
        assert_eq!(flags[2].load(Ordering::Acquire), 1);
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
