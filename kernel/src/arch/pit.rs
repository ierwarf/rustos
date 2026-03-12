use x86_64::instructions::{interrupts, port::Port};

const MAX_CHANNEL: u8 = 2;
const MICROS_PER_SECOND: u64 = 1_000_000;

const COMMAND_PORT: u16 = 0x43;
const DATA_PORT_BASE: u16 = 0x40;

const CHANNEL_SHIFT: u8 = 6;
const MODE_RATE_GENERATOR: u8 = 0b0011_0100;
const BASE_FREQUENCY_HZ: u32 = 1_193_182;
const MAX_INTERVAL_MICROS: u64 = (u16::MAX as u64 * MICROS_PER_SECOND) / BASE_FREQUENCY_HZ as u64;

pub fn divisor_from_micros(microseconds: u64) -> u16 {
    if microseconds == 0 || microseconds > MAX_INTERVAL_MICROS {
        panic!("microseconds must satisfy 0 < us <= {MAX_INTERVAL_MICROS}");
    }

    let divisor = ((BASE_FREQUENCY_HZ as u64) * microseconds) / MICROS_PER_SECOND;
    if divisor == 0 || divisor > u16::MAX as u64 {
        panic!("PIT divisor must satisfy 1 <= divisor <= {}", u16::MAX);
    }

    divisor as u16
}

fn program(pit_number: u8, divisor: u16) {
    if pit_number > MAX_CHANNEL {
        panic!("PIT number must be 0, 1, or 2");
    }

    if divisor == 0 {
        panic!("PIT divisor must be non-zero");
    }

    interrupts::without_interrupts(|| unsafe {
        let mut command_port = Port::new(COMMAND_PORT);
        let mut data_port = Port::new(DATA_PORT_BASE + pit_number as u16);
        let channel_bits = pit_number << CHANNEL_SHIFT;

        // Channel + lobyte/hibyte + mode2(rate generator) + binary counter.
        command_port.write(channel_bits | MODE_RATE_GENERATOR);
        data_port.write((divisor & 0xFF) as u8);
        data_port.write((divisor >> 8) as u8);
    });
}

pub fn set_divisor(pit_number: u8, divisor: u16) {
    program(pit_number, divisor);
}

pub fn start_micros(pit_number: u8, microseconds: u64) {
    let divisor = divisor_from_micros(microseconds);
    program(pit_number, divisor);

    if pit_number == 0 {
        crate::pic::enable_irq(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divisor_from_micros_matches_known_values() {
        assert_eq!(divisor_from_micros(1_000), 1_193);
        assert_eq!(divisor_from_micros(MAX_INTERVAL_MICROS), u16::MAX - 1);
    }

    #[test]
    fn divisor_from_micros_accepts_smallest_valid_interval() {
        assert_eq!(divisor_from_micros(1), 1);
    }

    #[test]
    #[should_panic(expected = "microseconds must satisfy")]
    fn divisor_from_micros_rejects_zero() {
        let _ = divisor_from_micros(0);
    }

    #[test]
    #[should_panic(expected = "microseconds must satisfy")]
    fn divisor_from_micros_rejects_out_of_range() {
        let _ = divisor_from_micros(MAX_INTERVAL_MICROS + 1);
    }
}
