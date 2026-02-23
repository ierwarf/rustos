use lazy_static::lazy_static;
use x86_64::instructions::interrupts;
use x86_64::set_general_handler;
use x86_64::structures::idt::InterruptDescriptorTable;

lazy_static! {
    pub static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();

        use crate::handlers::default_handler;
        set_general_handler!(&mut idt, default_handler);

        use crate::ticktick;
        set_general_handler!(&mut idt, ticktick, 0x20);

        return idt;
    };
}

pub fn init() {
    IDT.load();
    interrupts::enable();
}
