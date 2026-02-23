#[cfg(not(test))]
pub mod panic;

use core::fmt::{self, Write};
use x86_64::instructions::port::Port;

const DEBUGCON_PORT: u16 = 0x00e9;

pub fn print_byte(byte: u8) {
    unsafe {
        let mut port = Port::new(DEBUGCON_PORT);
        port.write(byte);
    }
}

pub fn print(s: &str) {
    for byte in s.bytes() {
        print_byte(byte);
    }
}

pub fn println(s: &str) {
    for byte in s.bytes() {
        print_byte(byte);
    }
    print("\r\n");
}

struct DebugWriter;

impl Write for DebugWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        print(s);
        Ok(())
    }
}

pub fn print_fmt(args: fmt::Arguments<'_>) {
    let mut writer = DebugWriter;
    let _ = writer.write_fmt(args);
}
