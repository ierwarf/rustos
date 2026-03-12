use core::fmt::{self, Write};

const DEBUGCON_PORT: u16 = 0x00e9;

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

fn write_str_unlocked(s: &str) {
    for byte in s.bytes() {
        write_byte(byte);
    }
}

pub fn println_fmt(args: fmt::Arguments<'_>) {
    let mut writer = DebugWriter;
    let _ = writer.write_fmt(args);
    write_str_unlocked("\r\n");
}

macro_rules! println {
    () => {{
        $crate::debug::println_fmt(format_args!(""));
    }};
    ($($arg:tt)*) => {{
        $crate::debug::println_fmt(format_args!($($arg)*));
    }};
}

pub(crate) use println;

struct DebugWriter;

impl Write for DebugWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str_unlocked(s);
        Ok(())
    }
}
