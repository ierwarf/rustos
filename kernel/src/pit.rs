use x86_64::instructions::{interrupts, port::Port};

pub fn start(pit_number: u8, milliseconds: u8) {
    if pit_number > 2 {
        panic!("PIT number must be 0, 1, or 2");
    }

    if milliseconds > 54 {
        panic!("PIT milliseconds must be less than or equal to 54");
    }

    interrupts::without_interrupts(|| unsafe {
        let mut pit_command_port = Port::new(0x43);
        let mut pit_data_port = Port::new(0x40 + pit_number as u16);
        let pit_channel_bits = pit_number << 6;

        // Set the PIT to mode 2 (rate generator) and binary counting
        pit_command_port.write((pit_channel_bits | 0b00110100) as u8);

        // Set the PIT frequency to 100 Hz (1193182 / 100)
        let divisor: u16 = (1193182 * (milliseconds as u32 / 1000)) as u16;
        pit_data_port.write((divisor & 0xFF) as u8); // Low byte
        pit_data_port.write((divisor >> 8) as u8); // High byte
    });
}

pub fn stop(pit_number: u8) {
    start(pit_number, 0); // Setting the divisor to 0 effectively stops the PIT
}
