use lazy_static::lazy_static;
use x86_64::VirtAddr;
use x86_64::set_general_handler;
use x86_64::structures::idt::InterruptDescriptorTable;

const TIMER_INTERRUPT_VECTOR: u8 = crate::pic::PIC_1_OFFSET;
const RTC_INTERRUPT_VECTOR: u8 = crate::pic::PIC_2_OFFSET;

lazy_static! {
    pub static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        use handlers::*;

        set_general_handler!(&mut idt, default_handler, 0..=31);
        unsafe {
            idt[TIMER_INTERRUPT_VECTOR].set_handler_addr(VirtAddr::new(
                crate::multitask::timer_interrupt_handler_addr(),
            ));
        }
        idt[RTC_INTERRUPT_VECTOR].set_handler_fn(rtc_interrupt_handler);

        idt
    };
}

pub fn init() {
    IDT.load();
}

mod handlers;
