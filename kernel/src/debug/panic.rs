#[cfg(not(test))]
use crate::debug;
use crate::io::console;
#[cfg(not(test))]
use crate::io::gui;
#[cfg(not(test))]
use core::arch::asm;
use core::fmt::{self, Write};
#[cfg(not(test))]
use core::panic::PanicInfo;
#[cfg(not(test))]
use x86_64::VirtAddr;

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    let _ = gui::try_present_panic_blackout();
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

    debug::dump_recent_trace_locations("panic");
    print_backtrace();

    gui::flush_debug_console();

    loop {
        core::hint::spin_loop();
    }
}

#[cfg(not(test))]
fn print_backtrace() {
    let mut frame = current_frame_pointer();
    debug::println!("backtrace:");
    gui_panic_line(format_args!("backtrace:"));

    for index in 0..16 {
        let Some(current) = frame else {
            break;
        };
        let Some(rbp_addr) = canonical_kernel_pointer(current) else {
            break;
        };
        let next_rbp = unsafe { *(rbp_addr.as_ptr::<u64>()) };
        let return_rip = unsafe { *(rbp_addr.as_ptr::<u64>().add(1)) };
        if return_rip == 0 {
            break;
        }
        debug::println!("  {:02}: rip={:#x} rbp={:#x}", index, return_rip, current);
        gui_panic_line(format_args!(
            "  {:02}: rip={:#x} rbp={:#x}",
            index, return_rip, current
        ));

        frame = if next_rbp <= current {
            None
        } else {
            Some(next_rbp)
        };
    }
}

#[cfg(not(test))]
fn current_frame_pointer() -> Option<u64> {
    let rbp: u64;
    unsafe {
        asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack, preserves_flags));
    }
    if rbp == 0 { None } else { Some(rbp) }
}

#[cfg(not(test))]
fn canonical_kernel_pointer(value: u64) -> Option<VirtAddr> {
    let addr = VirtAddr::new(value);
    if addr.as_u64() < 0xffff_8000_0000_0000 {
        return None;
    }
    Some(addr)
}

fn gui_panic_line(args: fmt::Arguments<'_>) {
    let mut line = PanicLine::new();
    let _ = line.write_fmt(args);
    let _ = console::write(line.as_bytes());
    let _ = console::write(b"\r\n");
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
