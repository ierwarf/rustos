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
/// OCW3: the next read of the command port returns the in-service register.
const OCW3_READ_ISR: u8 = 0x0b;

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

/// Whether `irq` is a spurious IRQ7/IRQ15 (the line dropped before the CPU
/// acknowledged), and if so completes its acknowledgement.
///
/// A spurious interrupt sets no ISR bit, so a non-specific EOI would retire a
/// different, genuinely in-service interrupt early. The one exception is a
/// spurious IRQ15: the master still has the cascade input (IRQ2) in service and
/// needs its EOI, but the slave must not get one. Returns true when the caller
/// must not dispatch or EOI the vector itself.
pub fn consume_spurious(interrupt_vector: u8) -> bool {
    let irq = interrupt_vector.wrapping_sub(PIC_1_OFFSET);
    if irq != 7 && irq != 15 {
        return false;
    }
    assert_eq!(
        nucleus_core::util::lockdep::current_cpu_index(),
        0,
        "legacy PIC interrupt routed to non-BSP CPU"
    );
    let command_port = if irq == 15 {
        PIC_2_COMMAND_PORT
    } else {
        PIC_1_COMMAND_PORT
    };
    // SAFETY: OCW3 + command-port read on the legacy PIC is side-effect free
    // apart from selecting which register the next read returns; only the BSP
    // IRQ leaf touches these command ports (see `send_eoi`).
    let in_service = unsafe {
        let mut port = Port::<u8>::new(command_port);
        port.write(OCW3_READ_ISR);
        port.read()
    };
    if in_service & 0x80 != 0 {
        return false;
    }
    if irq == 15 {
        // SAFETY: master command port, BSP-only leaf, as above.
        unsafe { Port::<u8>::new(PIC_1_COMMAND_PORT).write(END_OF_INTERRUPT) };
    }
    true
}
