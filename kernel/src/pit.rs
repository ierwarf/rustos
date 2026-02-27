use x86_64::instructions::{interrupts, port::Port};

const MAX_CHANNEL: u8 = 2;
const MAX_INTERVAL_MS: f64 = 54.0;

const COMMAND_PORT: u16 = 0x43;
const DATA_PORT_BASE: u16 = 0x40;

const CHANNEL_SHIFT: u8 = 6;
const MODE_RATE_GENERATOR: u8 = 0b0011_0100;
const BASE_FREQUENCY_HZ: u32 = 1_193_182;

fn divisor_from_millis(milliseconds: f64) -> u16 {
    let divisor = ((BASE_FREQUENCY_HZ as f64) * milliseconds / 1000.0) as u16;
    if divisor == 0 {
        panic!("PIT divisor must be non-zero");
    }
    divisor
}

pub fn start(pit_number: u8, milliseconds: f64) {
    if pit_number > MAX_CHANNEL {
        panic!("PIT number must be 0, 1, or 2");
    }

    if !(0.0..=MAX_INTERVAL_MS).contains(&milliseconds) || milliseconds == 0.0 {
        panic!("milliseconds must satisfy 0 < ms <= 54");
    }

    interrupts::without_interrupts(|| unsafe {
        let mut command_port = Port::new(COMMAND_PORT);
        let mut data_port = Port::new(DATA_PORT_BASE + pit_number as u16);
        let channel_bits = pit_number << CHANNEL_SHIFT;

        // Channel + lobyte/hibyte + mode2(rate generator) + binary counter.
        command_port.write(channel_bits | MODE_RATE_GENERATOR);

        let divisor = divisor_from_millis(milliseconds);
        data_port.write((divisor & 0xFF) as u8);
        data_port.write((divisor >> 8) as u8);
    });

    if pit_number == 0 {
        crate::pic::enable_irq(0);
    }
}
