use boot_protocol::BootInfo;
use core::fmt;
#[cfg(rustos_debug_print_enabled)]
use core::fmt::Write;
use diag_abi::{
    BootDiagBufferInfo, DiagLevel, DiagProvider, DiagRecord, DiagSharedBufferHeader, DiagStage,
};
#[cfg(rustos_debug_print_enabled)]
use spin::Mutex;
#[cfg(rustos_debug_print_enabled)]
use x86_64::instructions::port::Port;

#[cfg(rustos_debug_print_enabled)]
const DEBUGCON_PORT: u16 = 0x00e9;
#[cfg(rustos_debug_print_enabled)]
static DEBUG_LOCK: Mutex<()> = Mutex::new(());
static mut BOOT_DIAG_BUFFER: BootDiagBufferInfo = BootDiagBufferInfo {
    addr: 0,
    bytes_len: 0,
    record_capacity: 0,
    reserved: 0,
};

#[cfg(rustos_debug_print_enabled)]
fn print_byte(byte: u8) {
    unsafe {
        let mut port = Port::new(DEBUGCON_PORT);
        port.write(byte);
    }
}

#[cfg(rustos_debug_print_enabled)]
fn print_unlocked(s: &str) {
    for byte in s.bytes() {
        print_byte(byte);
    }
}

#[cfg(rustos_debug_print_enabled)]
fn print_fmt_unlocked(args: fmt::Arguments<'_>) {
    let mut writer = DebugWriter::new();
    let _ = writer.write_fmt(args);
    record_line(writer.as_str());
}

#[cfg(rustos_debug_print_enabled)]
fn with_debug_output_lock<F: FnOnce()>(f: F) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _guard = DEBUG_LOCK.try_lock();
        f();
    });
}

#[cfg(rustos_debug_print_enabled)]
pub fn println_newline() {
    with_debug_output_lock(|| {
        print_unlocked("\r\n");
    });
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn println_newline() {}

#[cfg(rustos_debug_print_enabled)]
pub fn println_fmt(args: fmt::Arguments<'_>) {
    with_debug_output_lock(|| {
        print_fmt_unlocked(args);
        print_unlocked("\r\n");
    });
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn println_fmt(_args: fmt::Arguments<'_>) {}

pub fn install_boot_diag(boot_info_ptr: *const BootInfo) {
    let Ok(boot_info) = (unsafe { BootInfo::from_ptr(boot_info_ptr) }) else {
        return;
    };
    unsafe {
        BOOT_DIAG_BUFFER = boot_info.boot_diag;
        if BOOT_DIAG_BUFFER.addr == 0 || BOOT_DIAG_BUFFER.record_capacity == 0 {
            return;
        }
        let header = &mut *(BOOT_DIAG_BUFFER.addr as *mut DiagSharedBufferHeader);
        if header.magic != diag_abi::DIAG_BUFFER_MAGIC {
            *header = DiagSharedBufferHeader::empty(BOOT_DIAG_BUFFER.record_capacity as u16);
        }
    }
}

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
fn record_line(line: &str) {
    unsafe {
        if BOOT_DIAG_BUFFER.addr == 0 || BOOT_DIAG_BUFFER.record_capacity == 0 {
            return;
        }
        let header = &mut *(BOOT_DIAG_BUFFER.addr as *mut DiagSharedBufferHeader);
        let capacity = usize::from(header.record_capacity);
        if capacity == 0 {
            return;
        }
        let records_base = (BOOT_DIAG_BUFFER.addr as usize
            + core::mem::size_of::<DiagSharedBufferHeader>())
            as *mut DiagRecord;
        let sequence = header.next_sequence;
        let slot = (sequence as usize) % capacity;
        let record = &mut *records_base.add(slot);
        *record = DiagRecord::empty();
        record.header.stage = DiagStage::Prekernel as u8;
        record.header.level = DiagLevel::Info as u8;
        record.header.provider = DiagProvider::Boot as u16;
        record.header.sequence = sequence;
        record.set_payload_bytes(line.as_bytes());
        header.next_sequence = header.next_sequence.wrapping_add(1);
    }
}

#[cfg(rustos_debug_print_enabled)]
struct DebugWriter {
    bytes: [u8; diag_abi::DIAG_PAYLOAD_BYTES],
    len: usize,
}

#[cfg(rustos_debug_print_enabled)]
impl DebugWriter {
    const fn new() -> Self {
        Self {
            bytes: [0; diag_abi::DIAG_PAYLOAD_BYTES],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
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
        print_unlocked(s);
        Ok(())
    }
}
