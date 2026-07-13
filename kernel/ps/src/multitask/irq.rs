use x86_64::instructions::interrupts;

use super::{
    DEFERRED_RESCHEDULE_REQUESTED, context::SavedContext, scheduler::Scheduler,
    scheduler_initialized, scheduler_mut,
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
    if !scheduler_initialized() {
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
    if DEFERRED_RESCHEDULE_REQUESTED.swap(0, core::sync::atomic::Ordering::AcqRel) != 0 {
        yield_now();
    }
}

pub fn cond_resched() {
    reschedule_if_requested();
}

pub fn clear_deferred_reschedule_request() {
    DEFERRED_RESCHEDULE_REQUESTED.store(0, core::sync::atomic::Ordering::Release);
}

pub(crate) fn request_deferred_reschedule() {
    DEFERRED_RESCHEDULE_REQUESTED.store(1, core::sync::atomic::Ordering::Release);
}

extern "C" fn software_schedule_interrupt_dispatch(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
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
    interrupts::without_interrupts(|| {
        crate::lowlevel::interrupts::trigger_software_schedule();
    });
}
