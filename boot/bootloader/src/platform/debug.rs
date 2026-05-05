use core::fmt;
#[cfg(rustos_debug_print_enabled)]
use core::fmt::Write;

#[cfg(rustos_debug_print_enabled)]
const DEBUGCON_PORT: u16 = 0x00e9;
#[cfg(rustos_debug_print_enabled)]
const LINE_CAPACITY: usize = 240;

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
    let mut writer = DebugWriter::new();
    let _ = writer.write_fmt(args);
    write_str_unlocked("\r\n");
}

#[cfg(not(rustos_debug_print_enabled))]
#[allow(dead_code)]
pub fn println_fmt(_args: fmt::Arguments<'_>) {}

#[cfg(all(rustos_debug_print_enabled, rustos_log_boot_info))]
macro_rules! println {
    () => {{
        $crate::debug::println_fmt(format_args!(""));
    }};
    ($($arg:tt)*) => {{
        $crate::debug::println_fmt(format_args!($($arg)*));
    }};
}

#[cfg(not(all(rustos_debug_print_enabled, rustos_log_boot_info)))]
macro_rules! println {
    () => {{}};
    ($($arg:tt)*) => {{}};
}

pub(crate) use println;

#[cfg(rustos_debug_print_enabled)]
struct DebugWriter {
    bytes: [u8; LINE_CAPACITY],
    len: usize,
}

#[cfg(rustos_debug_print_enabled)]
impl DebugWriter {
    const fn new() -> Self {
        Self {
            bytes: [0; LINE_CAPACITY],
            len: 0,
        }
    }
}

#[cfg(rustos_debug_print_enabled)]
impl Write for DebugWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let copy_len = bytes.len().min(self.bytes.len().saturating_sub(self.len));
        if copy_len != 0 {
            self.bytes[self.len..self.len + copy_len].copy_from_slice(&bytes[..copy_len]);
            self.len += copy_len;
        }
        write_str_unlocked(s);
        Ok(())
    }
}
