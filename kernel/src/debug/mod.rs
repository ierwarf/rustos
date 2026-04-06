pub mod boot_trace;
pub mod panic;

pub(crate) use trace_loc_macro::trace_loc;

use core::fmt;
#[cfg(rustos_debug_print_enabled)]
use core::fmt::Write;
#[cfg(rustos_debug_print_enabled)]
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(rustos_debug_print_enabled)]
use spin::Mutex;
#[cfg(all(rustos_debug_print_enabled, not(test)))]
use x86_64::instructions::port::Port;

#[cfg(rustos_debug_print_enabled)]
const DEBUGCON_PORT: u16 = 0x00e9;
#[cfg(rustos_debug_print_enabled)]
const RECENT_TRACE_CAPACITY: usize = 192;

#[cfg(rustos_debug_print_enabled)]
static DEBUG_LOCK: Mutex<()> = Mutex::new(());
#[cfg(rustos_debug_print_enabled)]
static RECENT_TRACE_NEXT: AtomicUsize = AtomicUsize::new(0);
#[cfg(rustos_debug_print_enabled)]
static RECENT_TRACE_SEQ: [AtomicUsize; RECENT_TRACE_CAPACITY] =
    [const { AtomicUsize::new(0) }; RECENT_TRACE_CAPACITY];
#[cfg(rustos_debug_print_enabled)]
static RECENT_TRACE_FILE_PTR: [AtomicUsize; RECENT_TRACE_CAPACITY] =
    [const { AtomicUsize::new(0) }; RECENT_TRACE_CAPACITY];
#[cfg(rustos_debug_print_enabled)]
static RECENT_TRACE_FILE_LEN: [AtomicUsize; RECENT_TRACE_CAPACITY] =
    [const { AtomicUsize::new(0) }; RECENT_TRACE_CAPACITY];
#[cfg(rustos_debug_print_enabled)]
static RECENT_TRACE_LINE: [AtomicUsize; RECENT_TRACE_CAPACITY] =
    [const { AtomicUsize::new(0) }; RECENT_TRACE_CAPACITY];
#[cfg(rustos_debug_print_enabled)]
static RECENT_TRACE_COLUMN: [AtomicUsize; RECENT_TRACE_CAPACITY] =
    [const { AtomicUsize::new(0) }; RECENT_TRACE_CAPACITY];
#[cfg(rustos_debug_print_enabled)]
static RECENT_TRACE_NOTE_PTR: [AtomicUsize; RECENT_TRACE_CAPACITY] =
    [const { AtomicUsize::new(0) }; RECENT_TRACE_CAPACITY];
#[cfg(rustos_debug_print_enabled)]
static RECENT_TRACE_NOTE_LEN: [AtomicUsize; RECENT_TRACE_CAPACITY] =
    [const { AtomicUsize::new(0) }; RECENT_TRACE_CAPACITY];

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
pub fn record_trace_location(file: &'static str, line: u32, column: u32) {
    record_trace_location_with_note(file, line, column, None);
}

#[cfg(rustos_debug_print_enabled)]
pub fn record_trace_location_with_note(
    file: &'static str,
    line: u32,
    column: u32,
    note: Option<&'static str>,
) {
    let seq = RECENT_TRACE_NEXT.fetch_add(1, Ordering::Relaxed);
    let slot = seq % RECENT_TRACE_CAPACITY;
    RECENT_TRACE_FILE_PTR[slot].store(file.as_ptr() as usize, Ordering::Relaxed);
    RECENT_TRACE_FILE_LEN[slot].store(file.len(), Ordering::Relaxed);
    RECENT_TRACE_LINE[slot].store(line as usize, Ordering::Relaxed);
    RECENT_TRACE_COLUMN[slot].store(column as usize, Ordering::Relaxed);
    if let Some(note) = note {
        RECENT_TRACE_NOTE_PTR[slot].store(note.as_ptr() as usize, Ordering::Relaxed);
        RECENT_TRACE_NOTE_LEN[slot].store(note.len(), Ordering::Relaxed);
    } else {
        RECENT_TRACE_NOTE_PTR[slot].store(0, Ordering::Relaxed);
        RECENT_TRACE_NOTE_LEN[slot].store(0, Ordering::Relaxed);
    }
    RECENT_TRACE_SEQ[slot].store(seq.wrapping_add(1), Ordering::Release);
}

#[cfg(not(rustos_debug_print_enabled))]
#[allow(dead_code)]
pub fn record_trace_location(_file: &'static str, _line: u32, _column: u32) {}

#[cfg(not(rustos_debug_print_enabled))]
#[allow(dead_code)]
pub fn record_trace_location_with_note(
    _file: &'static str,
    _line: u32,
    _column: u32,
    _note: Option<&'static str>,
) {
}

#[cfg(rustos_debug_print_enabled)]
pub fn dump_recent_trace_locations(reason: &str) {
    let end = RECENT_TRACE_NEXT.load(Ordering::Acquire);
    let start = end.saturating_sub(RECENT_TRACE_CAPACITY);
    let count = end.saturating_sub(start);

    println!("[recent-trace] {} count={}", reason, count);
    for seq in start..end {
        let slot = seq % RECENT_TRACE_CAPACITY;
        if RECENT_TRACE_SEQ[slot].load(Ordering::Acquire) != seq.wrapping_add(1) {
            continue;
        }

        let file_ptr = RECENT_TRACE_FILE_PTR[slot].load(Ordering::Relaxed) as *const u8;
        let file_len = RECENT_TRACE_FILE_LEN[slot].load(Ordering::Relaxed);
        let line = RECENT_TRACE_LINE[slot].load(Ordering::Relaxed);
        let column = RECENT_TRACE_COLUMN[slot].load(Ordering::Relaxed);
        let note_ptr = RECENT_TRACE_NOTE_PTR[slot].load(Ordering::Relaxed) as *const u8;
        let note_len = RECENT_TRACE_NOTE_LEN[slot].load(Ordering::Relaxed);
        if file_ptr.is_null() || file_len == 0 {
            continue;
        }

        let file = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(file_ptr, file_len))
        };
        if !note_ptr.is_null() && note_len != 0 {
            let note = unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(note_ptr, note_len))
            };
            println!(
                "[recent-trace] {:03} {}:{}:{} {}",
                seq.saturating_sub(start),
                file,
                line,
                column,
                note
            );
        } else {
            println!(
                "[recent-trace] {:03} {}:{}:{}",
                seq.saturating_sub(start),
                file,
                line,
                column
            );
        }
    }
}

#[cfg(not(rustos_debug_print_enabled))]
#[allow(dead_code)]
pub fn dump_recent_trace_locations(_reason: &str) {}

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
        mirror_debug_bytes_to_terminal(bytes);
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
        mirror_debug_bytes_to_terminal(bytes);
    }

    fn finish_line(&mut self) {
        print_bytes_unlocked(b"\r\n");
        crate::io::gui::write_debug_bytes(b"\r\n");
        mirror_debug_bytes_to_terminal(b"\r\n");
    }
}

#[cfg(rustos_debug_print_enabled)]
fn mirror_debug_bytes_to_terminal(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }

    let Some(session) = crate::io::session::focused_console_session() else {
        return;
    };
    let _ = crate::io::console::write_to_session(session, bytes);
}

#[cfg(rustos_debug_print_enabled)]
impl Write for DebugWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}
