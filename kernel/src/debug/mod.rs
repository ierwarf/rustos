#[cfg(not(test))]
pub mod panic;

use core::fmt::{self, Write};
use spin::Mutex;
use x86_64::instructions::port::Port;

const DEBUGCON_PORT: u16 = 0x00e9;
static DEBUG_LOCK: Mutex<()> = Mutex::new(());

fn print_byte(byte: u8) {
    unsafe {
        let mut port = Port::new(DEBUGCON_PORT);
        port.write(byte);
    }
}

fn print_unlocked(s: &str) {
    for byte in s.bytes() {
        print_byte(byte);
    }
}

fn print_fmt_unlocked(args: fmt::Arguments<'_>) {
    let mut writer = DebugWriter;
    let _ = writer.write_fmt(args);
}

fn with_debug_output_lock<F: FnOnce()>(f: F) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _guard = DEBUG_LOCK.try_lock();
        f();
    });
}

pub fn println_newline() {
    with_debug_output_lock(|| {
        print_unlocked("\r\n");
    });
}

pub fn println_fmt(args: fmt::Arguments<'_>) {
    with_debug_output_lock(|| {
        print_fmt_unlocked(args);
        print_unlocked("\r\n");
    });
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

struct DebugWriter;

impl Write for DebugWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        print_unlocked(s);
        Ok(())
    }
}
