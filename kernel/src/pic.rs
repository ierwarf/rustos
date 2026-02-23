use lazy_static::lazy_static;
use pic8259::ChainedPics;
use spin::Mutex;
use x86_64::instructions::interrupts;

pub const PIC_1_OFFSET: u8 = 0x20; // 32
pub const PIC_2_OFFSET: u8 = 0x28; // 40

lazy_static! {
    pub static ref PICS: Mutex<ChainedPics> =
        Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });
}

pub fn init() {
    interrupts::without_interrupts(|| unsafe {
        PICS.lock().initialize();
    });
}

pub fn enable_irq(irq: u8) {
    if irq > 15 {
        panic!("IRQ must be between 0 and 15");
    }

    interrupts::without_interrupts(|| unsafe {
        let mut pics = PICS.lock();
        let [mut mask1, mut mask2] = pics.read_masks();

        if irq < 8 {
            mask1 &= !(1u8 << irq);
        } else {
            let slave_irq = irq - 8;
            mask2 &= !(1u8 << slave_irq);
            mask1 &= !(1u8 << 2); // Keep cascade line enabled for slave PIC IRQs.
        }

        pics.write_masks(mask1, mask2);
    });
}
