use core::fmt;
#[cfg(rustos_debug_print_enabled)]
use core::fmt::Write;

#[cfg(rustos_debug_print_enabled)]
const DEBUGCON_PORT: u16 = 0x00e9;

#[cfg(rustos_debug_print_enabled)]
fn write_byte(byte: u8) {
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") DEBUGCON_PORT,
            in("al") byte,
            options(nomem, nostack, preserves_flags),
        );
    }
}

#[cfg(rustos_debug_print_enabled)]
fn write_str_unlocked(s: &str) {
    for byte in s.bytes() {
        write_byte(byte);
    }
}

#[cfg(rustos_debug_print_enabled)]
pub fn println_fmt(args: fmt::Arguments<'_>) {
    let mut writer = DebugWriter;
    let _ = writer.write_fmt(args);
    write_str_unlocked("\r\n");
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn println_fmt(_args: fmt::Arguments<'_>) {}

#[cfg(rustos_debug_print_enabled)]
macro_rules! println {
    () => {{
        $crate::debug::println_fmt(format_args!(""));
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
struct DebugWriter;

#[cfg(rustos_debug_print_enabled)]
impl Write for DebugWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str_unlocked(s);
        Ok(())
    }
}
