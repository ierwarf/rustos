use x86_64::structures::idt::InterruptStackFrame;

pub fn zero_division(stack_frame: InterruptStackFrame, _index: u8, error_code: Option<u64>) {
    panic!(
        "Unhandled exception: zero division, error code = {:?}\n\nstack frame: {:#?}",
        error_code, stack_frame
    );
}
