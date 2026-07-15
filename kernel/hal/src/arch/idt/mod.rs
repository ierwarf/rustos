use lazy_static::lazy_static;
use x86_64::VirtAddr;
use x86_64::set_general_handler;
use x86_64::structures::idt::InterruptDescriptorTable;

const TIMER_INTERRUPT_VECTOR: u8 = crate::arch::pic::PIC_1_OFFSET;
const KEYBOARD_INTERRUPT_VECTOR: u8 = crate::arch::pic::PIC_1_OFFSET + 1;
const MOUSE_INTERRUPT_VECTOR: u8 = crate::arch::pic::PIC_2_OFFSET + 4;
const RTC_INTERRUPT_VECTOR: u8 = crate::arch::pic::PIC_2_OFFSET;
pub(crate) const SOFTWARE_SCHEDULE_VECTOR: u8 = 0x30;

lazy_static! {
    pub static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        use handlers::*;

        set_general_handler!(&mut idt, default_handler, 0..=31);
        set_general_handler!(
            &mut idt,
            pic_interrupt_handler,
            crate::arch::pic::PIC_1_OFFSET..=crate::arch::pic::PIC_2_OFFSET + 7
        );
        set_general_handler!(
            &mut idt,
            msi_interrupt_handler,
            crate::arch::msi::MSI_VECTOR_FIRST..=crate::arch::msi::MSI_VECTOR_LAST
        );
        unsafe {
            idt.non_maskable_interrupt
                .set_handler_fn(non_maskable_interrupt_handler);
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(crate::arch::gdt::DOUBLE_FAULT_IST_INDEX);
            idt[TIMER_INTERRUPT_VECTOR].set_handler_addr(VirtAddr::new(
                crate::lowlevel::interrupts::timer_interrupt_handler_addr(),
            ));
            idt[RTC_INTERRUPT_VECTOR].set_handler_addr(VirtAddr::new(
                crate::lowlevel::interrupts::rtc_interrupt_handler_addr(),
            ));
            idt[SOFTWARE_SCHEDULE_VECTOR].set_handler_addr(VirtAddr::new(
                crate::lowlevel::interrupts::software_schedule_interrupt_handler_addr(),
            ));
        }
        idt[KEYBOARD_INTERRUPT_VECTOR].set_handler_fn(keyboard_interrupt_eoi_handler);
        idt[MOUSE_INTERRUPT_VECTOR].set_handler_fn(mouse_interrupt_eoi_handler);

        idt
    };
}

pub fn init() {
    IDT.load();
}

mod handlers;
