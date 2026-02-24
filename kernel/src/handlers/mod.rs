use x86_64::structures::idt::InterruptStackFrame;

pub fn default_handler(stack_frame: InterruptStackFrame, index: u8, error_code: Option<u64>) {
    panic!(
        "Unhandled exception: vector = {}, error code = {:?}\n\nstack frame: {:#?}",
        index, error_code, stack_frame
    );
}

mod exceptions;
mod interrupts;
