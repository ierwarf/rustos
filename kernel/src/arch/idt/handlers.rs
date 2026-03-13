use x86_64::registers::control::Cr2;
use x86_64::structures::idt::InterruptStackFrame;

const KEYBOARD_INTERRUPT_VECTOR: u8 = crate::pic::PIC_1_OFFSET + 1;
const RTC_INTERRUPT_VECTOR: u8 = crate::pic::PIC_2_OFFSET;

pub fn default_handler(stack_frame: InterruptStackFrame, index: u8, error_code: Option<u64>) {
    let cr2 = Cr2::read().map(|addr| addr.as_u64()).unwrap_or(u64::MAX);
    if is_user_mode(&stack_frame) {
        if crate::multitask::retire_current_user_task_due_to_fault(
            index,
            error_code,
            cr2,
            stack_frame.instruction_pointer.as_u64(),
        ) {
            crate::multitask::halt_current_retired_task();
        }

        panic!(
            "user-mode exception was raised without an active user task: vector={}, rip={:#x}, cs={:#x}",
            index,
            stack_frame.instruction_pointer.as_u64(),
            stack_frame.code_segment.0,
        );
    }

    panic!(
        "Unhandled exception: vector = {}, error code = {:?}, cr2 = {:#x}, rip = {:#x}, cs = {:#x}, rflags = {:#x}, rsp = {:#x}, ss = {:#x}",
        index,
        error_code,
        cr2,
        stack_frame.instruction_pointer.as_u64(),
        stack_frame.code_segment.0,
        stack_frame.cpu_flags.bits(),
        stack_frame.stack_pointer.as_u64(),
        stack_frame.stack_segment.0,
    );
}

fn is_user_mode(stack_frame: &InterruptStackFrame) -> bool {
    (stack_frame.code_segment.0 & 0x3) == 0x3
}

pub extern "x86-interrupt" fn rtc_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::rtc::on_interrupt();
    crate::pic::send_eoi(RTC_INTERRUPT_VECTOR);
}

pub extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::input::on_legacy_keyboard_interrupt();
    crate::pic::send_eoi(KEYBOARD_INTERRUPT_VECTOR);
}
