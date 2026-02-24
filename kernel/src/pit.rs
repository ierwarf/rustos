use x86_64::instructions::{interrupts, port::Port};

const MAX_CHANNEL: u8 = 2;
const MAX_INTERVAL_MS: u8 = 54;

const COMMAND_PORT: u16 = 0x43;
const DATA_PORT_BASE: u16 = 0x40;

const CHANNEL_SHIFT: u8 = 6;
const MODE_RATE_GENERATOR: u8 = 0b0011_0100;
const BASE_FREQUENCY_HZ: u32 = 1_193_182;

pub fn start(pit_number: u8, milliseconds: u8) {
    if pit_number > MAX_CHANNEL {
        panic!("PIT number must be 0, 1, or 2");
    }

    if milliseconds > MAX_INTERVAL_MS {
        panic!("PIT milliseconds must be less than or equal to 54");
    }

    interrupts::without_interrupts(|| unsafe {
        let mut command_port = Port::new(COMMAND_PORT);
        let mut data_port = Port::new(DATA_PORT_BASE + pit_number as u16);
        let channel_bits = pit_number << CHANNEL_SHIFT;

        // Channel + lobyte/hibyte + mode2(rate generator) + binary counter.
        command_port.write(channel_bits | MODE_RATE_GENERATOR);

        let divisor_u32 = (BASE_FREQUENCY_HZ * milliseconds as u32) / 1000;
        let divisor: u16 = divisor_u32 as u16;
        data_port.write((divisor & 0xFF) as u8);
        data_port.write((divisor >> 8) as u8);
    });

    if pit_number == 0 {
        crate::pic::enable_irq(0);
    }
}

pub fn stop(pit_number: u8) {
    if pit_number > MAX_CHANNEL {
        panic!("PIT number must be 0, 1, or 2");
    }

    if pit_number == 0 {
        crate::pic::disable_irq(0);
    }
}
