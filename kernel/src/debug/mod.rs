pub mod boot_trace;
pub mod panic;

use core::fmt::{self, Write};
use spin::Mutex;
use x86_64::instructions::port::Port;

const DEBUGCON_PORT: u16 = 0x00e9;

static DEBUG_LOCK: Mutex<()> = Mutex::new(());

#[cfg(not(test))]
fn print_byte(byte: u8) {
    unsafe {
        let mut port = Port::new(DEBUGCON_PORT);
        port.write(byte);
    }
}

#[cfg(test)]
fn print_byte(byte: u8) {
    use std::io::Write as _;

    let _ = std::io::stderr().write_all(&[byte]);
}

#[allow(dead_code)]
fn print_bytes_unlocked(bytes: &[u8]) {
    for &byte in bytes {
        print_byte(byte);
    }
}

fn with_debug_output_lock<F: FnOnce()>(f: F) {
    #[cfg(not(test))]
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _guard = DEBUG_LOCK.try_lock();
        f();
    });

    #[cfg(test)]
    {
        let _guard = DEBUG_LOCK.try_lock();
        f();
    }
}

pub fn println_newline() {
    write_debug_bytes(b"\r\n");
}

pub fn println_fmt(args: fmt::Arguments<'_>) {
    with_debug_output_lock(|| {
        let mut writer = DebugWriter;
        let _ = writer.write_fmt(args);
        writer.write_bytes(b"\r\n");
    });
}

#[allow(dead_code)]
pub fn write_bytes(bytes: &[u8]) {
    write_debug_bytes(bytes);
}

macro_rules! println {
    () => {{
        $crate::debug::println_newline();
    }};
    ($($arg:tt)*) => {{
        $crate::debug::println_fmt(format_args!($($arg)*));
    }};
}

pub(crate) use println;

fn write_debug_bytes(bytes: &[u8]) {
    with_debug_output_lock(|| {
        print_bytes_unlocked(bytes);
    });
}

struct DebugWriter;

impl DebugWriter {
    fn write_bytes(&mut self, bytes: &[u8]) {
        print_bytes_unlocked(bytes);
    }
}

impl Write for DebugWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}
