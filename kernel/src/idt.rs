use lazy_static::lazy_static;
use x86_64::set_general_handler;
use x86_64::structures::idt::InterruptDescriptorTable;

lazy_static! {
    pub static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();

        use crate::multitask::task_switch;
        set_general_handler!(&mut idt, task_switch, 32);

        idt
    };
}

pub fn init() {
    IDT.load();
}
