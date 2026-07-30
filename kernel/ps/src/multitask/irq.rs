//! BSP timer and software-reschedule entry ordering.
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
//!   a deadline, or SMP/IPI behavior implied by local APIC use.
//! - **Evidence:** `scheduler-lifecycle`, `scheduler-dispatch`, and
//!   `monotonic-deadline-lifecycle`.
use x86_64::instructions::interrupts;

use super::{
    DEFERRED_RESCHEDULE_REQUESTED, USER_RETURN_RESCHEDULE_ARMED, context::SavedContext,
    scheduler::Scheduler, scheduler_initialized, scheduler_mut,
};

pub fn timer_interrupt_handler_addr() -> u64 {
    crate::lowlevel::interrupts::timer_interrupt_handler_addr()
}

pub fn rtc_interrupt_handler_addr() -> u64 {
    crate::lowlevel::interrupts::rtc_interrupt_handler_addr()
}

pub fn software_schedule_interrupt_handler_addr() -> u64 {
    crate::lowlevel::interrupts::software_schedule_interrupt_handler_addr()
}

pub(crate) fn install_interrupt_dispatch_callbacks() {
    crate::lowlevel::interrupts::register_timer_interrupt_dispatch(timer_interrupt_dispatch);
    crate::lowlevel::interrupts::register_rtc_interrupt_dispatch(rtc_interrupt_dispatch);
    crate::lowlevel::interrupts::register_software_schedule_interrupt_dispatch(
        software_schedule_interrupt_dispatch,
    );
}

extern "C" fn timer_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    // The low-level timer assembly enters this function directly rather than
    // the generic PIC wrapper, so publish the IRQ context for the complete
    // clockevent, scheduler, PIT, and EOI transaction.
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    // Expire sleepers against invariant-TSC/HPET time before selecting the
    // next task. This is deliberately independent of how many PIT/RTC edges
    // the hypervisor delivered: one delayed edge catches up every absolute
    // deadline that passed while the vCPU was descheduled.
    crate::arch::rtc::service_clock_event();
    if !scheduler_initialized() {
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_1_OFFSET);
        return context_ptr;
    }
    // A task may have published its blocked/retired state immediately before
    // completing a bounded raw-lock transaction. That state alone must not
    // authorize the timer path to switch stacks while the preemption guard is
    // live. The eventual explicit scheduler handoff below the lock remains the
    // sole transition point.
    if nucleus_core::util::lockdep::preemption_disabled() {
        DEFERRED_RESCHEDULE_REQUESTED.store(1, core::sync::atomic::Ordering::Release);
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_1_OFFSET);
        return context_ptr;
    }

    let current_rsp = context_ptr as usize;
    let next_rsp = unsafe {
        let scheduler = scheduler_mut();
        if timer_interrupted_kernel_frame(context_ptr, scheduler) {
            crate::arch::pic::send_eoi(crate::arch::pic::PIC_1_OFFSET);
            return context_ptr;
        }
        // A real user-frame clockevent consumes any request not already
        // serviced by the IF-enabled common syscall tail.
        USER_RETURN_RESCHEDULE_ARMED.store(0, core::sync::atomic::Ordering::Release);
        DEFERRED_RESCHEDULE_REQUESTED.store(0, core::sync::atomic::Ordering::Release);
        scheduler.save_current_simd_state();
        let (next_rsp, next_pit_divisor) = scheduler.on_timer_interrupt(current_rsp);
        crate::arch::pit::set_divisor(0, next_pit_divisor);
        scheduler.prepare_current_task_execution();
        scheduler.restore_current_simd_state();
        next_rsp
    };

    crate::arch::pic::send_eoi(crate::arch::pic::PIC_1_OFFSET);
    next_rsp as *mut SavedContext
}

extern "C" fn rtc_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    if !scheduler_initialized() {
        crate::arch::rtc::on_interrupt();
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
    crate::arch::rtc::on_interrupt();
    let current_rsp = context_ptr as usize;
    let interrupted_kernel_frame = unsafe {
        let scheduler = scheduler_mut();
        timer_interrupted_kernel_frame(context_ptr, scheduler)
    };
    if interrupted_kernel_frame {
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
        return context_ptr;
    }

    let next_rsp = unsafe {
        let scheduler = scheduler_mut();
        DEFERRED_RESCHEDULE_REQUESTED.store(0, core::sync::atomic::Ordering::Release);
        scheduler.save_current_simd_state();
        let (next_rsp, _next_pit_divisor) = scheduler.on_timer_interrupt(current_rsp);
        scheduler.prepare_current_task_execution();
        scheduler.restore_current_simd_state();
        next_rsp
    };

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
        DEFERRED_RESCHEDULE_REQUESTED.store(1, core::sync::atomic::Ordering::Release);
        return true;
    }
    false
}

pub fn reschedule_if_requested() {
    if nucleus_core::util::lockdep::preemption_disabled() {
        return;
    }
    if DEFERRED_RESCHEDULE_REQUESTED.swap(0, core::sync::atomic::Ordering::AcqRel) != 0 {
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
    if consume_syscall_tail_reschedule(
        &DEFERRED_RESCHEDULE_REQUESTED,
        &USER_RETURN_RESCHEDULE_ARMED,
    ) {
        crate::lowlevel::interrupts::trigger_software_schedule_interruptible();
    }
}

fn consume_syscall_tail_reschedule(
    deferred: &core::sync::atomic::AtomicU64,
    user_return: &core::sync::atomic::AtomicU64,
) -> bool {
    let deferred = deferred.swap(0, core::sync::atomic::Ordering::AcqRel);
    let user_return = user_return.swap(0, core::sync::atomic::Ordering::AcqRel);
    deferred != 0 || user_return != 0
}

pub(crate) fn request_deferred_reschedule() {
    DEFERRED_RESCHEDULE_REQUESTED.store(1, core::sync::atomic::Ordering::Release);
}

/// Requests a voluntary switch in the common interruptible syscall tail.
pub(crate) fn request_user_return_reschedule() {
    USER_RETURN_RESCHEDULE_ARMED.store(1, core::sync::atomic::Ordering::Release);
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
    let next_rsp = unsafe {
        let scheduler = scheduler_mut();
        scheduler.save_current_simd_state();
        // Voluntary yield: floor the vruntime charge so a sub-tick yield can't
        // accumulate 0 vruntime and re-win CFS. Timer-driven preemption still
        // uses the unfloored path.
        let (next_rsp, _next_pit_divisor) = scheduler.on_voluntary_yield(current_rsp);
        scheduler.prepare_current_task_execution();
        scheduler.restore_current_simd_state();
        next_rsp
    };

    next_rsp as *mut SavedContext
}

pub fn yield_now() {
    assert!(
        !nucleus_core::util::lockdep::preemption_disabled(),
        "task yielded while raw spin lock held depth={} class={:?}",
        nucleus_core::util::lockdep::preemption_depth(),
        nucleus_core::util::lockdep::current_lock_class()
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
    assert!(
        !nucleus_core::util::lockdep::preemption_disabled(),
        "task blocked while raw spin lock held depth={} class={:?}",
        nucleus_core::util::lockdep::preemption_depth(),
        nucleus_core::util::lockdep::current_lock_class()
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

    use super::consume_syscall_tail_reschedule;

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
}
