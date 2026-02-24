use x86_64::structures::idt::InterruptStackFrame;

const RTC_INTERRUPT_VECTOR: u8 = crate::pic::PIC_2_OFFSET;

pub fn default_handler(stack_frame: InterruptStackFrame, index: u8, error_code: Option<u64>) {
    panic!(
        "Unhandled exception: vector = {}, error code = {:?}\n\nstack frame: {:#?}",
        index, error_code, stack_frame
    );
}

pub extern "x86-interrupt" fn rtc_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::rtc::on_interrupt();
    crate::pic::send_eoi(RTC_INTERRUPT_VECTOR);
}
