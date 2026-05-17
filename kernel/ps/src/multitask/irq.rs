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
        let _ = crate::linux_runtime_hooks::tick_jiffies(1);
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

    let current_rsp = context_ptr as usize;
    let next_rsp = unsafe {
        let scheduler = scheduler_mut();
        let _ = crate::linux_runtime_hooks::tick_jiffies(1);
        if timer_interrupted_kernel_frame(context_ptr, scheduler) {
            crate::arch::rtc::on_interrupt();
            crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
            return context_ptr;
        }
        scheduler.save_current_simd_state();
        let (next_rsp, _next_pit_divisor) = scheduler.on_timer_interrupt(current_rsp);
        scheduler.prepare_current_task_execution();
        scheduler.restore_current_simd_state();
        next_rsp
    };

    crate::arch::rtc::on_interrupt();
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
        let (next_rsp, _next_pit_divisor) = scheduler.on_timer_interrupt(current_rsp);
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
