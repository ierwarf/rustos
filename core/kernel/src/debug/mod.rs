pub mod boot_trace;
pub mod panic;

use core::fmt;
#[cfg(rustos_debug_print_enabled)]
use core::fmt::Write;
#[cfg(rustos_debug_print_enabled)]
use spin::Mutex;
#[cfg(all(rustos_debug_print_enabled, not(test)))]
use x86_64::instructions::port::Port;

#[cfg(rustos_debug_print_enabled)]
const DEBUGCON_PORT: u16 = 0x00e9;

#[cfg(rustos_debug_print_enabled)]
static DEBUG_LOCK: Mutex<()> = Mutex::new(());

#[cfg(all(rustos_debug_print_enabled, not(test)))]
fn print_byte(byte: u8) {
    unsafe {
        let mut port = Port::new(DEBUGCON_PORT);
        port.write(byte);
    }
}

#[cfg(all(rustos_debug_print_enabled, test))]
fn print_byte(byte: u8) {
    use std::io::Write as _;

    let _ = std::io::stderr().write_all(&[byte]);
}

#[cfg(rustos_debug_print_enabled)]
#[allow(dead_code)]
fn print_bytes_unlocked(bytes: &[u8]) {
    for &byte in bytes {
        print_byte(byte);
    }
}

#[cfg(rustos_debug_print_enabled)]
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

#[cfg(rustos_debug_print_enabled)]
pub fn println_newline() {
    write_debug_bytes(b"\r\n");
}

#[cfg(not(rustos_debug_print_enabled))]
#[allow(dead_code)]
pub fn println_newline() {}

#[cfg(rustos_debug_print_enabled)]
pub fn println_fmt(args: fmt::Arguments<'_>) {
    with_debug_output_lock(|| {
        let mut writer = DebugWriter;
        let _ = writer.write_fmt(args);
        writer.finish_line();
    });
}

#[cfg(not(rustos_debug_print_enabled))]
#[allow(dead_code)]
pub fn println_fmt(_args: fmt::Arguments<'_>) {}

#[cfg(rustos_debug_print_enabled)]
#[allow(dead_code)]
pub fn write_bytes(bytes: &[u8]) {
    write_debug_bytes(bytes);
}

#[cfg(not(rustos_debug_print_enabled))]
#[allow(dead_code)]
pub fn write_bytes(_bytes: &[u8]) {}

#[cfg(rustos_debug_print_enabled)]
macro_rules! println {
    () => {{
        $crate::debug::println_newline();
    }};
    ($($arg:tt)*) => {{
        $crate::debug::println_fmt(format_args!($($arg)*));
    }};
}

#[cfg(not(rustos_debug_print_enabled))]
macro_rules! println {
    () => {{}};
    ($($arg:tt)*) => {{}};
}

pub(crate) use println;

#[cfg(rustos_debug_print_enabled)]
fn write_debug_bytes(bytes: &[u8]) {
    with_debug_output_lock(|| {
        print_bytes_unlocked(bytes);
        crate::io::gui::write_debug_bytes(bytes);
    });
}

#[cfg(not(rustos_debug_print_enabled))]
#[allow(dead_code)]
fn write_debug_bytes(_bytes: &[u8]) {}

#[cfg(rustos_debug_print_enabled)]
struct DebugWriter;

#[cfg(rustos_debug_print_enabled)]
impl DebugWriter {
    fn write_bytes(&mut self, bytes: &[u8]) {
        print_bytes_unlocked(bytes);
        crate::io::gui::write_debug_bytes(bytes);
    }

    fn finish_line(&mut self) {
        print_bytes_unlocked(b"\r\n");
        crate::io::gui::write_debug_bytes(b"\r\n");
    }
}

#[cfg(rustos_debug_print_enabled)]
impl Write for DebugWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}
