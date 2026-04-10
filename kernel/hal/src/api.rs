use boot_protocol::BootInfo;
use kernel_lowlevel::interrupts::SavedContext;

pub fn disable_interrupts() {
    x86_64::instructions::interrupts::disable();
}

pub fn init_gdt() {
    crate::arch::gdt::init();
}

pub fn init_idt() {
    crate::arch::idt::init();
}

pub fn init_acpi(boot_info_ptr: *const BootInfo) {
    crate::arch::acpi::init(boot_info_ptr);
}

pub fn init_pic() {
    crate::arch::pic::init();
    kernel_lowlevel::interrupts::register_timer_interrupt_dispatch(timer_interrupt_fallback);
    kernel_lowlevel::interrupts::register_software_schedule_interrupt_dispatch(
        software_schedule_interrupt_fallback,
    );
}

pub fn init_rtc() {
    kernel_lowlevel::interrupts::register_rtc_interrupt_dispatch(rtc_interrupt_fallback);
    crate::arch::rtc::init();
}

pub fn init_simd() {
    crate::arch::simd::init();
}

pub fn simd_mode_name() -> &'static str {
    crate::arch::simd::mode_name()
}

pub fn current_rip() -> u64 {
    crate::arch::asmtools::current_rip()
}

pub unsafe fn enter_higher_half(entry: u64, boot_info_ptr: u64) -> ! {
    unsafe { crate::arch::asmtools::enter_higher_half(entry, boot_info_ptr) }
}

pub unsafe fn call_with_stack(entry: u64, arg0: u64, stack_top: u64) -> ! {
    unsafe { crate::arch::asmtools::call_with_stack(entry, arg0, stack_top) }
}

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
