pub mod boot_trace;

extern crate alloc;

use alloc::vec::Vec;
use boot_protocol::BootInfo;
use core::fmt;
#[cfg(rustos_debug_print_enabled)]
use core::fmt::Write;
#[cfg(all(rustos_debug_print_enabled, not(test)))]
use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(rustos_debug_print_enabled)]
use diag_abi::{
    CrashStoreHeader, CrashStoreInfo, DebugConfigureRequest, DebugDeviceState, DiagLevel,
    DiagProvider, DiagRecord, DiagSharedBufferHeader, DiagStage,
};
#[cfg(not(rustos_debug_print_enabled))]
use diag_abi::{DebugConfigureRequest, DebugDeviceState, DiagRecord};
#[cfg(rustos_debug_print_enabled)]
use spin::Mutex;
#[cfg(all(rustos_debug_print_enabled, not(test)))]
use x86_64::instructions::port::Port;

#[cfg(rustos_debug_print_enabled)]
const DEBUGCON_PORT: u16 = 0x00e9;
#[cfg(rustos_debug_print_enabled)]
const DIAG_RING_CAPACITY: usize = 512;

#[cfg(rustos_debug_print_enabled)]
static DEBUG_LOCK: Mutex<()> = Mutex::new(());
#[cfg(rustos_debug_print_enabled)]
static DIAG_STATE: Mutex<DiagState> = Mutex::new(DiagState::new());
#[cfg(all(rustos_debug_print_enabled, not(test)))]
static TIMESTAMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[cfg(rustos_debug_print_enabled)]
struct DiagState {
    enabled: bool,
    min_level: u8,
    provider_mask: u64,
    next_sequence: u64,
    read_sequence: u64,
    dropped_records: u64,
    crash_store: CrashStoreInfo,
    crash_available: bool,
    crash_bytes: usize,
    ring: [DiagRecord; DIAG_RING_CAPACITY],
}

#[cfg(rustos_debug_print_enabled)]
impl DiagState {
    const fn new() -> Self {
        Self {
            enabled: true,
            min_level: DiagLevel::Trace as u8,
            provider_mask: u64::MAX,
            next_sequence: 0,
            read_sequence: 0,
            dropped_records: 0,
            crash_store: CrashStoreInfo {
                addr: 0,
                bytes_len: 0,
            },
            crash_available: false,
            crash_bytes: 0,
            ring: [const { DiagRecord::empty() }; DIAG_RING_CAPACITY],
        }
    }

    fn should_emit(&self, provider: DiagProvider, level: DiagLevel) -> bool {
        self.enabled
            && (level as u8) >= self.min_level
            && (self.provider_mask & provider.bit()) != 0
    }

    fn push_record(&mut self, mut record: DiagRecord) {
        let slot = (self.next_sequence as usize) % self.ring.len();
        record.header.sequence = self.next_sequence;
        self.ring[slot] = record;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        if self.next_sequence.saturating_sub(self.read_sequence) > self.ring.len() as u64 {
            self.read_sequence = self.next_sequence.saturating_sub(self.ring.len() as u64);
            self.dropped_records = self.dropped_records.saturating_add(1);
        }
    }

    fn import_record(&mut self, record: &DiagRecord) {
        self.push_record(*record);
    }

    fn drain_records(&mut self, max_records: usize) -> Vec<DiagRecord> {
        let available = self.next_sequence.saturating_sub(self.read_sequence);
        let count = available.min(max_records as u64) as usize;
        let mut records = Vec::with_capacity(count);
        for seq in self.read_sequence..self.read_sequence + count as u64 {
            records.push(self.ring[(seq as usize) % self.ring.len()]);
        }
        self.read_sequence = self.read_sequence.saturating_add(count as u64);
        records
    }

    fn snapshot_recent_records(&self, max_records: usize) -> Vec<DiagRecord> {
        let retained = self.next_sequence.min(self.ring.len() as u64);
        let count = retained.min(max_records as u64) as usize;
        let start = self.next_sequence.saturating_sub(count as u64);
        let mut records = Vec::with_capacity(count);
        for seq in start..self.next_sequence {
            records.push(self.ring[(seq as usize) % self.ring.len()]);
        }
        records
    }

    fn device_state(&self) -> DebugDeviceState {
        DebugDeviceState {
            record_size: core::mem::size_of::<DiagRecord>() as u32,
            ring_capacity: DIAG_RING_CAPACITY as u32,
            records_available: self.next_sequence.saturating_sub(self.read_sequence),
            total_sequence: self.next_sequence,
            dropped_records: self.dropped_records,
            filter_mask: self.provider_mask,
            min_level: self.min_level,
            enabled: u8::from(self.enabled),
            reserved0: 0,
            crash_available: u32::from(self.crash_available),
            crash_bytes: self.crash_bytes as u32,
        }
    }

    fn configure(&mut self, request: DebugConfigureRequest) {
        self.enabled = request.enabled != 0;
        self.min_level = request.min_level;
        self.provider_mask = request.provider_mask;
    }

    fn filter_allows(&self, provider: DiagProvider, level: DiagLevel) -> bool {
        self.should_emit(provider, level)
    }
}

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
fn with_diag_state_lock<R, F: FnOnce(&mut DiagState) -> R>(f: F) -> R {
    #[cfg(not(test))]
    {
        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut state = DIAG_STATE.lock();
            f(&mut state)
        })
    }

    #[cfg(test)]
    {
        let mut state = DIAG_STATE.lock();
        f(&mut state)
    }
}

#[cfg(rustos_debug_print_enabled)]
pub fn init(boot_info_ptr: *const BootInfo) {
    let Ok(boot_info) = (unsafe { BootInfo::from_ptr(boot_info_ptr) }) else {
        return;
    };
    with_diag_state_lock(|state| {
        state.crash_store = boot_info.crash_store;
        import_boot_records_locked(state, boot_info);
    });
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn init(_boot_info_ptr: *const BootInfo) {}

#[cfg(rustos_debug_print_enabled)]
fn import_boot_records_locked(state: &mut DiagState, boot_info: &BootInfo) {
    if boot_info.boot_diag.addr == 0 || boot_info.boot_diag.record_capacity == 0 {
        return;
    }

    let header = unsafe { &*(boot_info.boot_diag.addr as *const DiagSharedBufferHeader) };
    if header.magic != diag_abi::DIAG_BUFFER_MAGIC || header.record_capacity == 0 {
        return;
    }

    let capacity = usize::from(header.record_capacity);
    let next_sequence = header.next_sequence;
    let start = next_sequence.saturating_sub(capacity as u64);
    let records_base = (boot_info.boot_diag.addr as usize
        + core::mem::size_of::<DiagSharedBufferHeader>())
        as *const DiagRecord;

    for seq in start..next_sequence {
        let record = unsafe { &*records_base.add((seq as usize) % capacity) };
        if record.header.magic == diag_abi::DIAG_RECORD_MAGIC {
            state.import_record(record);
        }
    }
}

#[cfg(all(rustos_debug_print_enabled, not(test)))]
fn timestamp_micros() -> u64 {
    TIMESTAMP_COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(all(rustos_debug_print_enabled, test))]
fn timestamp_micros() -> u64 {
    0
}

#[cfg(rustos_debug_print_enabled)]
fn current_diag_subject_ids() -> (u64, u64) {
    (0, 0)
}

#[cfg(rustos_debug_print_enabled)]
pub fn emit_text(
    provider: DiagProvider,
    level: DiagLevel,
    event_id: u16,
    span_id: u64,
    object_id: u64,
    message: &str,
) {
    let mut record = DiagRecord::empty();
    record.header.stage = DiagStage::Kernel as u8;
    record.header.level = level as u8;
    record.header.provider = provider as u16;
    record.header.event_id = event_id;
    record.header.timestamp_micros = timestamp_micros();
    record.header.span_id = span_id;
    record.header.object_id = object_id;
    let (process_id, thread_id) = current_diag_subject_ids();
    record.header.process_id = process_id;
    record.header.thread_id = thread_id;
    record.set_payload_bytes(message.as_bytes());

    let should_mirror = with_diag_state_lock(|state| {
        let should_emit = state.should_emit(provider, level);
        if should_emit {
            state.push_record(record);
        }
        should_emit && should_mirror_early_boot_record(provider, level)
    });
    if should_mirror {
        mirror_diag_record_to_debugcon(provider, message);
    }
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn emit_text(
    _provider: diag_abi::DiagProvider,
    _level: diag_abi::DiagLevel,
    _event_id: u16,
    _span_id: u64,
    _object_id: u64,
    _message: &str,
) {
}

#[cfg(rustos_debug_print_enabled)]
fn should_mirror_early_boot_record(provider: DiagProvider, level: DiagLevel) -> bool {
    if matches!(provider, DiagProvider::Panic) {
        return true;
    }
    if matches!(provider, DiagProvider::Heartbeat) {
        return level >= DiagLevel::Info;
    }
    if level < DiagLevel::Info {
        return false;
    }
    matches!(
        provider,
        DiagProvider::Boot
            | DiagProvider::Driver
            | DiagProvider::Module
            | DiagProvider::Service
    )
}

#[cfg(not(rustos_debug_print_enabled))]
#[allow(dead_code)]
fn should_mirror_early_boot_record(
    _provider: diag_abi::DiagProvider,
    _level: diag_abi::DiagLevel,
) -> bool {
    false
}

#[cfg(rustos_debug_print_enabled)]
fn mirror_diag_record_to_debugcon(provider: DiagProvider, message: &str) {
    let prefix = match provider {
        DiagProvider::Boot => "[boot] ",
        DiagProvider::Driver => "[driver] ",
        DiagProvider::Module => "[module] ",
        DiagProvider::Heartbeat => "[heartbeat] ",
        DiagProvider::Service => "[service] ",
        DiagProvider::Panic => "[panic] ",
        _ => "",
    };
    with_debug_output_lock(|| {
        print_bytes_unlocked(prefix.as_bytes());
        print_bytes_unlocked(message.as_bytes());
        print_bytes_unlocked(b"\r\n");
    });
}

#[cfg(not(rustos_debug_print_enabled))]
#[allow(dead_code)]
fn mirror_diag_record_to_debugcon(_provider: diag_abi::DiagProvider, _message: &str) {}

#[cfg(rustos_debug_print_enabled)]
pub fn should_emit(provider: DiagProvider, level: DiagLevel) -> bool {
    with_diag_state_lock(|state| state.filter_allows(provider, level))
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn should_emit(_provider: diag_abi::DiagProvider, _level: diag_abi::DiagLevel) -> bool {
    false
}

#[cfg(rustos_debug_print_enabled)]
pub fn configure(request: DebugConfigureRequest) {
    with_diag_state_lock(|state| state.configure(request));
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn configure(_request: DebugConfigureRequest) {}

#[cfg(rustos_debug_print_enabled)]
pub fn device_state() -> DebugDeviceState {
    with_diag_state_lock(|state| state.device_state())
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn device_state() -> DebugDeviceState {
    DebugDeviceState::empty()
}

#[cfg(rustos_debug_print_enabled)]
pub fn drain_records(max_records: usize) -> Vec<DiagRecord> {
    with_diag_state_lock(|state| state.drain_records(max_records))
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn drain_records(_max_records: usize) -> Vec<DiagRecord> {
    Vec::new()
}

#[cfg(rustos_debug_print_enabled)]
pub fn snapshot_crash_bytes() -> Vec<u8> {
    with_diag_state_lock(|state| {
        if state.crash_store.addr == 0 || state.crash_bytes == 0 {
            return Vec::new();
        }
        let bytes = unsafe {
            core::slice::from_raw_parts(state.crash_store.addr as *const u8, state.crash_bytes)
        };
        bytes.to_vec()
    })
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn snapshot_crash_bytes() -> Vec<u8> {
    Vec::new()
}

#[cfg(rustos_debug_print_enabled)]
pub fn capture_crash_snapshot(panic_text: &str) {
    with_diag_state_lock(|state| {
        if state.crash_store.addr == 0 || state.crash_store.bytes_len == 0 {
            return;
        }
        let header_ptr = state.crash_store.addr as *mut CrashStoreHeader;
        let records_ptr = (state.crash_store.addr as usize
            + core::mem::size_of::<CrashStoreHeader>())
            as *mut DiagRecord;
        let recent = state.snapshot_recent_records(diag_abi::DIAG_CRASH_RECORD_CAPACITY);
        let text_ptr = (records_ptr as usize
            + diag_abi::DIAG_CRASH_RECORD_CAPACITY * core::mem::size_of::<DiagRecord>())
            as *mut u8;
        let text_bytes = panic_text.as_bytes();
        let text_len = text_bytes.len().min(diag_abi::DIAG_CRASH_TEXT_BYTES);

        unsafe {
            *header_ptr = CrashStoreHeader::empty();
            (*header_ptr).record_count = recent.len() as u32;
            (*header_ptr).panic_text_len = text_len as u32;
            (*header_ptr).last_sequence = state.next_sequence;
            for (index, record) in recent.iter().enumerate() {
                *records_ptr.add(index) = *record;
            }
            core::ptr::write_bytes(text_ptr, 0, diag_abi::DIAG_CRASH_TEXT_BYTES);
            if text_len != 0 {
                core::ptr::copy_nonoverlapping(text_bytes.as_ptr(), text_ptr, text_len);
            }
        }

        state.crash_available = true;
        state.crash_bytes = core::mem::size_of::<CrashStoreHeader>()
            + recent.len() * core::mem::size_of::<DiagRecord>()
            + text_len;
    });
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn capture_crash_snapshot(_panic_text: &str) {}

#[cfg(rustos_debug_print_enabled)]
pub fn println_newline() {
    write_debug_bytes(b"\r\n");
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn println_newline() {}

#[cfg(rustos_debug_print_enabled)]
pub fn println_fmt(args: fmt::Arguments<'_>) {
    with_debug_output_lock(|| {
        let mut writer = DebugWriter::new();
        let _ = writer.write_fmt(args);
        writer.finish_line();
        emit_text(
            DiagProvider::Legacy,
            DiagLevel::Info,
            0,
            0,
            0,
            writer.as_str(),
        );
    });
}

#[cfg(not(rustos_debug_print_enabled))]
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
    let mut writer = DebugWriter::new();
    let _ = match note {
        Some(note) => write!(
            &mut writer,
            "breadcrumb {}:{}:{} {}",
            file, line, column, note
        ),
        None => write!(&mut writer, "breadcrumb {}:{}:{}", file, line, column),
    };
    emit_text(
        DiagProvider::Breadcrumb,
        DiagLevel::Debug,
        0,
        0,
        0,
        writer.as_str(),
    );
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn record_trace_location(_file: &'static str, _line: u32, _column: u32) {}

#[cfg(not(rustos_debug_print_enabled))]
pub fn record_trace_location_with_note(
    _file: &'static str,
    _line: u32,
    _column: u32,
    _note: Option<&'static str>,
) {
}

#[cfg(rustos_debug_print_enabled)]
pub fn dump_recent_trace_locations(reason: &str) {
    emit_text(DiagProvider::Breadcrumb, DiagLevel::Warn, 1, 0, 0, reason);
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn dump_recent_trace_locations(_reason: &str) {}

#[cfg(rustos_debug_print_enabled)]
pub fn write_bytes(bytes: &[u8]) {
    write_debug_bytes(bytes);
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn write_bytes(_bytes: &[u8]) {}

#[cfg(rustos_debug_print_enabled)]
pub fn write_debugcon_only(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    with_debug_output_lock(|| {
        print_bytes_unlocked(bytes);
    });
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn write_debugcon_only(_bytes: &[u8]) {}

#[cfg(rustos_debug_print_enabled)]
pub fn write_debugcon_only_line(bytes: &[u8]) {
    with_debug_output_lock(|| {
        if !bytes.is_empty() {
            print_bytes_unlocked(bytes);
        }
        print_bytes_unlocked(b"\r\n");
    });
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn write_debugcon_only_line(_bytes: &[u8]) {}

#[cfg(rustos_debug_print_enabled)]
fn write_debug_bytes(bytes: &[u8]) {
    with_debug_output_lock(|| {
        print_bytes_unlocked(bytes);
    });
}

#[cfg(not(rustos_debug_print_enabled))]
#[allow(dead_code)]
fn write_debug_bytes(_bytes: &[u8]) {}

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

    fn finish_line(&mut self) {
        print_bytes_unlocked(b"\r\n");
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
        print_bytes_unlocked(bytes);
        Ok(())
    }
}
