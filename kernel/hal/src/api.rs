use boot_protocol::BootInfo;
use kernel_lowlevel::interrupts::SavedContext;

pub mod arch {
    pub mod acpi {
        pub use crate::arch::acpi::*;
    }

    pub mod asmtools {
        pub use crate::arch::asmtools::*;
    }

    pub mod gdt {
        pub use crate::arch::gdt::*;
    }

    pub mod pci {
        pub use crate::arch::pci::*;
    }

    pub mod pic {
        pub use crate::arch::pic::*;
    }

    pub mod pit {
        pub use crate::arch::pit::*;
    }

    pub mod rtc {
        pub use crate::arch::rtc::*;
    }

    pub mod simd {
        pub use crate::arch::simd::*;
    }
}

pub mod boot {
    use super::BootInfo;

    pub fn init_gdt() {
        crate::arch::gdt::init();
    }

    pub fn init_idt() {
        crate::arch::idt::init();
    }

    pub fn init_acpi(boot_info_ptr: *const BootInfo) {
        crate::arch::acpi::init(boot_info_ptr);
    }

    pub unsafe fn enter_higher_half(entry: u64, boot_info_ptr: u64) -> ! {
        unsafe { crate::arch::asmtools::enter_higher_half(entry, boot_info_ptr) }
    }

    pub unsafe fn call_with_stack(entry: u64, arg0: u64, stack_top: u64) -> ! {
        unsafe { crate::arch::asmtools::call_with_stack(entry, arg0, stack_top) }
    }
}

pub mod cpu {
    pub fn init_simd() {
        crate::arch::simd::init();
    }

    pub fn simd_mode_name() -> &'static str {
        crate::arch::simd::mode_name()
    }

    pub fn current_rip() -> u64 {
        crate::arch::asmtools::current_rip()
    }
}

pub mod interrupts {
    use kernel_lowlevel::interrupts::SavedContext;

    use crate::hooks::{
        HeartbeatHooks, InterruptHooks, TaskHooks,
        register_heartbeat_hooks as install_heartbeat_hooks,
        register_interrupt_hooks as install_interrupt_hooks,
        register_task_hooks as install_task_hooks,
    };

    pub fn disable_interrupts() {
        x86_64::instructions::interrupts::disable();
    }

    pub fn init_pic() {
        crate::arch::pic::init();
        kernel_lowlevel::interrupts::register_timer_interrupt_dispatch(
            super::timer_interrupt_fallback,
        );
        kernel_lowlevel::interrupts::register_software_schedule_interrupt_dispatch(
            super::software_schedule_interrupt_fallback,
        );
    }

    pub fn register_task_hooks(hooks: TaskHooks) {
        install_task_hooks(hooks);
    }

    pub fn register_interrupt_hooks(hooks: InterruptHooks) {
        install_interrupt_hooks(hooks);
    }

    pub fn register_heartbeat_hooks(hooks: HeartbeatHooks) {
        install_heartbeat_hooks(hooks);
    }

    pub unsafe fn restore_kernel_saved_context(context: *mut SavedContext) -> ! {
        unsafe extern "C" {
            #[link_name = "restore_kernel_saved_context"]
            fn restore_kernel_saved_context_raw(context: *mut SavedContext) -> !;
        }

        unsafe { restore_kernel_saved_context_raw(context) }
    }
}

pub mod time {
    pub fn init_rtc() {
        kernel_lowlevel::interrupts::register_rtc_interrupt_dispatch(super::rtc_interrupt_fallback);
        crate::arch::rtc::init();
    }
}

pub use crate::hooks::{
    CurrentUserSnapshot, HeartbeatHooks, HeartbeatSnapshot, InputEventQueueDebugSnapshot,
    InterruptHooks, TaskHooks, UserFaultDisposition,
};
pub use boot::{call_with_stack, enter_higher_half, init_acpi, init_gdt, init_idt};
pub use cpu::{current_rip, init_simd, simd_mode_name};
pub use interrupts::{
    disable_interrupts, init_pic, register_heartbeat_hooks, register_interrupt_hooks,
    register_task_hooks, restore_kernel_saved_context,
};
pub use time::init_rtc;

extern "C" fn timer_interrupt_fallback(context_ptr: *mut SavedContext) -> *mut SavedContext {
    crate::arch::pic::send_eoi(crate::arch::pic::PIC_1_OFFSET);
    context_ptr
}

extern "C" fn rtc_interrupt_fallback(context_ptr: *mut SavedContext) -> *mut SavedContext {
    crate::arch::rtc::on_interrupt();
    crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
    context_ptr
}

extern "C" fn software_schedule_interrupt_fallback(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
    context_ptr
}
