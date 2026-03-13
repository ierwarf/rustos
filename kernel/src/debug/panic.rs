use crate::{debug, gui};
use core::fmt::{self, Write};
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    gui_panic_line(format_args!(""));
    gui_panic_line(format_args!("[PANIC]"));
    debug::println!();
    debug::println!("[PANIC]");
    debug::println!("message: {}", info.message());
    gui_panic_line(format_args!("message: {}", info.message()));

    if let Some(location) = info.location() {
        debug::println!(
            "location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
        gui_panic_line(format_args!(
            "location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        ));
    } else {
        debug::println!("location: <unknown>");
        gui_panic_line(format_args!("location: <unknown>"));
    }

    loop {
        core::hint::spin_loop();
    }
}

fn gui_panic_line(args: fmt::Arguments<'_>) {
    let mut line = PanicLine::new();
    let _ = line.write_fmt(args);
    let _ = gui::try_write_console(line.as_bytes());
    let _ = gui::try_write_console(b"\r\n");
}

struct PanicLine {
    bytes: [u8; 256],
    len: usize,
}

impl PanicLine {
    const fn new() -> Self {
        Self {
            bytes: [0; 256],
            len: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        if self.len < self.bytes.len() {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Write for PanicLine {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for ch in s.chars() {
            match ch {
                '\n' | '\r' | '\t' => self.push(b' '),
                ' '..='~' => self.push(ch as u8),
                _ => self.push(b'?'),
            }
        }
        Ok(())
    }
}
