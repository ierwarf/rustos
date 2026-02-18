use lazy_static::lazy_static;
use x86_64::addr::VirtAddr;
use x86_64::set_general_handler;
use x86_64::structures::DescriptorTablePointer;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

use core::arch::asm;

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();

        set_general_handler!(&mut idt, default_handler);

        return idt;
    };
    static ref IDT_empty: InterruptDescriptorTable = InterruptDescriptorTable::new();
}

fn default_handler(_stack_frame: InterruptStackFrame, index: u8, error_code: Option<u64>) {
    panic!(
        "Unhandled interrupt: vector = {}, error code = {:?}",
        index, error_code
    );
}

pub fn init() {
    IDT.load();
}

pub fn disable() {
    IDT_empty.load();
}
