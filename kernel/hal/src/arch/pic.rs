use lazy_static::lazy_static;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use pic8259::ChainedPics;
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;

pub const PIC_1_OFFSET: u8 = 0x20;
pub const PIC_2_OFFSET: u8 = 0x28;

const MAX_IRQ: u8 = 15;
const CASCADE_IRQ: u8 = 2;
const ALL_IRQS_MASKED: u8 = u8::MAX;
const PIC_1_COMMAND_PORT: u16 = 0x20;
const PIC_2_COMMAND_PORT: u16 = 0xa0;
const END_OF_INTERRUPT: u8 = 0x20;

lazy_static! {
    pub static ref PICS: TrackedSpinLock<ChainedPics, { LockClass::LegacyPic as u8 }> =
        TrackedSpinLock::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });
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
            let bit = 1u8 << irq;
            if enabled {
                mask1 &= !bit;
            } else {
                mask1 |= bit;
            }
        } else {
            let slave_irq = irq - 8;
            let bit = 1u8 << slave_irq;
            if enabled {
                mask2 &= !bit;
                // Keep cascade line enabled when using slave PIC IRQs.
                mask1 &= !(1u8 << CASCADE_IRQ);
            } else {
                mask2 |= bit;
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
    let max_vector = PIC_1_OFFSET + MAX_IRQ;
    if !(PIC_1_OFFSET..=max_vector).contains(&interrupt_vector) {
        panic!(
            "interrupt vector must be between {:#x} and {:#x}",
            PIC_1_OFFSET, max_vector
        );
    }

    assert_eq!(
        nucleus_core::util::lockdep::current_cpu_index(),
        0,
        "legacy PIC interrupt routed to non-BSP CPU"
    );
    // IRQ acknowledgement is a leaf hardware operation, not PIC policy
    // mutation. Legacy PIC delivery is BSP-only, and x86 interrupt gates
    // exclude same-CPU reentry, so taking the configuration spin lock here
    // would only create an IRQ-to-process lock inversion.
    unsafe {
        if interrupt_vector >= PIC_2_OFFSET {
            Port::<u8>::new(PIC_2_COMMAND_PORT).write(END_OF_INTERRUPT);
        }
        Port::<u8>::new(PIC_1_COMMAND_PORT).write(END_OF_INTERRUPT);
    }
}
