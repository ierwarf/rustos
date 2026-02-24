use lazy_static::lazy_static;
use pic8259::ChainedPics;
use spin::Mutex;
use x86_64::instructions::interrupts;

pub const PIC_1_OFFSET: u8 = 0x20;
pub const PIC_2_OFFSET: u8 = 0x28;

const MAX_IRQ: u8 = 15;
const CASCADE_IRQ: u8 = 2;
const ALL_IRQS_MASKED: u8 = u8::MAX;

lazy_static! {
    pub static ref PICS: Mutex<ChainedPics> =
        Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });
}

pub fn init() {
    interrupts::without_interrupts(|| unsafe {
        let mut pics = PICS.lock();
        pics.initialize();
        pics.write_masks(ALL_IRQS_MASKED, ALL_IRQS_MASKED);
    });
}

fn set_irq_enabled(irq: u8, enabled: bool) {
    if irq > MAX_IRQ {
        panic!("IRQ must be between 0 and 15");
    }

    interrupts::without_interrupts(|| unsafe {
        let mut pics = PICS.lock();
        let [mut mask1, mut mask2] = pics.read_masks();

        if irq < 8 {
            if enabled {
                mask1 &= !(1u8 << irq);
            } else {
                mask1 |= 1u8 << irq;
            }
        } else {
            let slave_irq = irq - 8;
            if enabled {
                mask2 &= !(1u8 << slave_irq);
                // Keep cascade line enabled when using slave PIC IRQs.
                mask1 &= !(1u8 << CASCADE_IRQ);
            } else {
                mask2 |= 1u8 << slave_irq;
            }
        }

        pics.write_masks(mask1, mask2);
    });
}

pub fn enable_irq(irq: u8) {
    set_irq_enabled(irq, true);
}

pub fn disable_irq(irq: u8) {
    set_irq_enabled(irq, false);
}

pub fn send_eoi(interrupt_vector: u8) {
    if interrupt_vector - PIC_1_OFFSET > MAX_IRQ {
        panic!("IRQ must be between 0 and 15");
    }

    unsafe {
        PICS.lock().notify_end_of_interrupt(interrupt_vector);
    }
}
