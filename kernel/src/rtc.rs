use core::hint::spin_loop;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::{hlt, interrupts, port::Port};

const CMOS_INDEX_PORT: u16 = 0x70;
const CMOS_DATA_PORT: u16 = 0x71;
const NMI_DISABLE: u8 = 0x80;

const RTC_REG_A: u8 = 0x0A;
const RTC_REG_B: u8 = 0x0B;
const RTC_REG_C: u8 = 0x0C;
const RTC_PERIODIC_INTERRUPT_ENABLE: u8 = 1 << 6;
const RTC_RATE_1024_HZ: u8 = 6;
const RTC_TICKS_PER_SEC: u64 = 1024;

static RTC_TICKS: AtomicU64 = AtomicU64::new(0);

fn cmos_read(reg: u8) -> u8 {
    unsafe {
        let mut index_port: Port<u8> = Port::new(CMOS_INDEX_PORT);
        let mut data_port: Port<u8> = Port::new(CMOS_DATA_PORT);
        index_port.write(NMI_DISABLE | reg);
        data_port.read()
    }
}

fn cmos_write(reg: u8, value: u8) {
    unsafe {
        let mut index_port: Port<u8> = Port::new(CMOS_INDEX_PORT);
        let mut data_port: Port<u8> = Port::new(CMOS_DATA_PORT);
        index_port.write(NMI_DISABLE | reg);
        data_port.write(value);
    }
}

pub fn init() {
    interrupts::without_interrupts(|| {
        // Program RTC periodic interrupt rate to 1024 Hz.
        let prev_a = cmos_read(RTC_REG_A);
        cmos_write(RTC_REG_A, (prev_a & 0xF0) | RTC_RATE_1024_HZ);

        let prev_b = cmos_read(RTC_REG_B);
        cmos_write(RTC_REG_B, prev_b | RTC_PERIODIC_INTERRUPT_ENABLE);

        // Read C once to clear any pending interrupt latch.
        let _ = cmos_read(RTC_REG_C);
    });

    crate::pic::enable_irq(8);
}

pub fn on_interrupt() {
    RTC_TICKS.fetch_add(1, Ordering::Release);
    // Must read register C to acknowledge and re-arm RTC interrupts.
    let _ = cmos_read(RTC_REG_C);
}

pub fn sleep(milliseconds: u64) {
    if milliseconds == 0 {
        return;
    }

    let ticks_needed = (milliseconds.saturating_mul(RTC_TICKS_PER_SEC) + 999) / 1000;
    let ticks_needed = core::cmp::max(1, ticks_needed);
    let target = RTC_TICKS
        .load(Ordering::Acquire)
        .saturating_add(ticks_needed);

    let restore_disabled = !interrupts::are_enabled();
    while RTC_TICKS.load(Ordering::Acquire) < target {
        if restore_disabled {
            interrupts::enable();
            hlt();
            interrupts::disable();
        } else {
            hlt();
        }
        spin_loop();
    }
}
