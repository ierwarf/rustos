use core::fmt;
#[cfg(rustos_debug_print_enabled)]
use core::fmt::Write;

use diag_abi::{
    BootDiagBufferInfo, DiagLevel, DiagProvider, DiagRecord, DiagSharedBufferHeader, DiagStage,
    DIAG_BOOT_BUFFER_RECORD_CAPACITY,
};

#[cfg(rustos_debug_print_enabled)]
const DEBUGCON_PORT: u16 = 0x00e9;

static mut BOOT_DIAG_BUFFER: BootDiagBufferInfo = BootDiagBufferInfo {
    addr: 0,
    bytes_len: 0,
    record_capacity: 0,
    reserved: 0,
};

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

pub fn install_boot_diag_buffer(buffer: BootDiagBufferInfo) {
    unsafe {
        BOOT_DIAG_BUFFER = buffer;
        if buffer.addr == 0 || buffer.record_capacity == 0 {
            return;
        }
        let header = &mut *(buffer.addr as *mut DiagSharedBufferHeader);
        *header = DiagSharedBufferHeader::empty(
            buffer
                .record_capacity
                .min(DIAG_BOOT_BUFFER_RECORD_CAPACITY as u32) as u16,
        );
    }
}

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
        record.header.stage = DiagStage::Bootloader as u8;
        record.header.level = DiagLevel::Info as u8;
        record.header.provider = DiagProvider::Boot as u16;
        record.header.sequence = sequence;
        record.set_payload_bytes(line.as_bytes());
        header.next_sequence = header.next_sequence.wrapping_add(1);
    }
}

#[cfg(rustos_debug_print_enabled)]
pub fn println_fmt(args: fmt::Arguments<'_>) {
    let mut writer = DebugWriter::new();
    let _ = writer.write_fmt(args);
    record_line(writer.as_str());
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
        write_str_unlocked(s);
        Ok(())
    }
}
