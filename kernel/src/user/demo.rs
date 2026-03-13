use alloc::vec::Vec;
use core::fmt;
use core::fmt::Write;
use x86_64::instructions::{hlt, interrupts};

use crate::debug;
use crate::{fat, process};

const USER_DEMO_EXE_PATH: &str = "USERDEMO.EXE";
const USER_DEMO_ELF_PATH: &str = "USERDEMO.ELF";
const USER_DEMO_WEIGHT_MICROS: u64 = 50;

pub fn run() -> ! {
    write_status_line(format_args!("Loading USERDEMO..."));
    let (userdemo_path, userdemo_image) = match read_preferred_user_demo() {
        Ok(value) => value,
        Err(err) => fatal(format_args!(
            "failed to read {} or {} from boot volume: {:?}",
            USER_DEMO_EXE_PATH, USER_DEMO_ELF_PATH, err
        )),
    };

    write_status_line(format_args!("Spawning USERDEMO..."));
    let spawned = match process::spawn_process(&userdemo_image, USER_DEMO_WEIGHT_MICROS, 0, 0) {
        Ok(spawned) => spawned,
        Err(err) => {
            err.log_debug_details();
            fatal(format_args!(
                "failed to spawn {}: {} ({:?})",
                userdemo_path,
                err.summary(),
                err
            ))
        }
    };

    debug::println!(
        "Ring3 process spawned: pid={} entry={:#x} weight={}us path={}",
        spawned.pid,
        spawned.entry.as_u64(),
        USER_DEMO_WEIGHT_MICROS,
        userdemo_path
    );
    write_status_line(format_args!("USERDEMO spawned."));

    interrupts::enable();
    loop {
        hlt();
    }
}

fn read_boot_file(path: &str) -> core::result::Result<Vec<u8>, fatfs::Error<fat::DiskIoError>> {
    fat::read_file_to_vec(path)
}

fn read_preferred_user_demo()
-> core::result::Result<(&'static str, Vec<u8>), fatfs::Error<fat::DiskIoError>> {
    match read_boot_file(USER_DEMO_ELF_PATH) {
        Ok(image) => Ok((USER_DEMO_ELF_PATH, image)),
        Err(_) => read_boot_file(USER_DEMO_EXE_PATH).map(|image| (USER_DEMO_EXE_PATH, image)),
    }
}

fn fatal(args: fmt::Arguments<'_>) -> ! {
    write_status_line(format_args!(""));
    write_status_line(format_args!("[KERNEL FATAL]"));
    write_status_line(args);
    debug::println!();
    debug::println!("[KERNEL FATAL]");
    debug::println!("{}", args);
    interrupts::disable();
    loop {
        hlt();
    }
}

fn write_status_line(args: fmt::Arguments<'_>) {
    let mut line = StatusLine::new();
    let _ = line.write_fmt(args);
    crate::gui::write_console(line.as_bytes());
    crate::gui::write_console(b"\r\n");
}

struct StatusLine {
    bytes: [u8; 256],
    len: usize,
}

impl StatusLine {
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

impl Write for StatusLine {
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
