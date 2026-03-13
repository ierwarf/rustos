use alloc::vec::Vec;
use core::fmt;
use core::fmt::Write;
use x86_64::instructions::{hlt, interrupts};

use crate::console;
use crate::debug;
use crate::fat;
use crate::user::console_host;

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

    write_status_line(format_args!("Spawning USERDEMO sessions..."));
    let program = console_host::ConsoleProgramSpec::new(
        &userdemo_image,
        userdemo_path,
        USER_DEMO_WEIGHT_MICROS,
    );
    if let Err(err) = console_host::spawn_program_on_all_sessions(program) {
        err.log_debug_details();
        fatal(format_args!(
            "failed to spawn {} for {} session: {} ({:?})",
            userdemo_path,
            err.session().name(),
            err.summary(),
            err
        ));
    }
    write_status_line(format_args!("USERDEMO sessions spawned."));

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
    console::write(line.as_bytes());
    console::write(b"\r\n");
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
