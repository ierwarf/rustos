use x86_64::structures::idt::InterruptStackFrame;

pub fn zero_division(_stack_frame: InterruptStackFrame, index: u8, error_code: Option<u64>) {
    panic!(
        "Unhandled exception: zero division, error code = {:?}\n\nstack frame: {:#?}",
        error_code, _stack_frame
    );
}
