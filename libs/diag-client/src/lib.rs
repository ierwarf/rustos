use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use diag_abi::{
    DiagLevel, DiagProvider, DiagRecord, DiagStage, DIAG_PAYLOAD_BYTES, DIAG_SOCKET_PATH,
};

pub use diag_abi;

fn cached_stream() -> &'static Mutex<Option<UnixStream>> {
    static STREAM: OnceLock<Mutex<Option<UnixStream>>> = OnceLock::new();
    STREAM.get_or_init(|| Mutex::new(None))
}

fn timestamp_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u128::from(u64::MAX)) as u64
}

fn current_tid() -> u64 {
    unsafe { libc::syscall(libc::SYS_gettid as libc::c_long) as u64 }
}

fn encode_record(
    service: &str,
    level: DiagLevel,
    event_id: u16,
    span_id: u64,
    object_id: u64,
    message: &str,
) -> DiagRecord {
    let mut record = DiagRecord::empty();
    record.header.stage = DiagStage::User as u8;
    record.header.level = level as u8;
    record.header.provider = DiagProvider::Service as u16;
    record.header.event_id = event_id;
    record.header.timestamp_micros = timestamp_micros();
    record.header.span_id = span_id;
    record.header.object_id = object_id;
    record.header.process_id = u64::from(std::process::id());
    record.header.thread_id = current_tid();
    compose_payload(&mut record, service, message);
    record
}

fn compose_payload(record: &mut DiagRecord, service: &str, message: &str) {
    let mut payload = [0_u8; DIAG_PAYLOAD_BYTES];
    let mut len = 0;
    append_payload(&mut payload, &mut len, service.as_bytes());
    append_payload(&mut payload, &mut len, b": ");
    append_payload(&mut payload, &mut len, message.as_bytes());
    record.payload = payload;
    record.header.payload_len = len as u16;
}

fn append_payload(dest: &mut [u8], len: &mut usize, src: &[u8]) {
    if *len >= dest.len() || src.is_empty() {
        return;
    }
    let available = dest.len() - *len;
    let copied = src.len().min(available);
    dest[*len..*len + copied].copy_from_slice(&src[..copied]);
    *len += copied;
}

fn record_as_bytes(record: &DiagRecord) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            (record as *const DiagRecord).cast::<u8>(),
            std::mem::size_of::<DiagRecord>(),
        )
    }
}

fn write_record(record: &DiagRecord) -> std::io::Result<()> {
    let mut guard = cached_stream().lock().unwrap();
    if guard.is_none() {
        *guard = UnixStream::connect(DIAG_SOCKET_PATH).ok().map(|stream| {
            let _ = stream.set_nonblocking(true);
            stream
        });
    }

    if let Some(stream) = guard.as_mut() {
        if stream.write_all(record_as_bytes(record)).is_ok() {
            return Ok(());
        }
        *guard = None;
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "debugd socket unavailable",
    ))
}

pub fn emit_text(
    service: &str,
    level: DiagLevel,
    event_id: u16,
    span_id: u64,
    object_id: u64,
    message: &str,
) {
    let record = encode_record(service, level, event_id, span_id, object_id, message);
    if write_record(&record).is_err() {
        let _ = std::io::stderr().write_all(record.message_bytes());
        let _ = std::io::stderr().write_all(b"\n");
    }
}

#[macro_export]
macro_rules! diag_trace {
    ($service:expr, $message:expr $(,)?) => {{
        $crate::emit_text(
            $service,
            $crate::diag_abi::DiagLevel::Trace,
            0,
            0,
            0,
            $message,
        );
    }};
    ($service:expr, $($arg:tt)+) => {{
        $crate::emit_text(
            $service,
            $crate::diag_abi::DiagLevel::Trace,
            0,
            0,
            0,
            &format!($($arg)+),
        );
    }};
}

#[macro_export]
macro_rules! diag_debug {
    ($service:expr, $message:expr $(,)?) => {{
        $crate::emit_text(
            $service,
            $crate::diag_abi::DiagLevel::Debug,
            0,
            0,
            0,
            $message,
        );
    }};
    ($service:expr, $($arg:tt)+) => {{
        $crate::emit_text(
            $service,
            $crate::diag_abi::DiagLevel::Debug,
            0,
            0,
            0,
            &format!($($arg)+),
        );
    }};
}

#[macro_export]
macro_rules! diag_info {
    ($service:expr, $message:expr $(,)?) => {{
        $crate::emit_text(
            $service,
            $crate::diag_abi::DiagLevel::Info,
            0,
            0,
            0,
            $message,
        );
    }};
    ($service:expr, $($arg:tt)+) => {{
        $crate::emit_text(
            $service,
            $crate::diag_abi::DiagLevel::Info,
            0,
            0,
            0,
            &format!($($arg)+),
        );
    }};
}

#[macro_export]
macro_rules! diag_warn {
    ($service:expr, $message:expr $(,)?) => {{
        $crate::emit_text(
            $service,
            $crate::diag_abi::DiagLevel::Warn,
            0,
            0,
            0,
            $message,
        );
    }};
    ($service:expr, $($arg:tt)+) => {{
        $crate::emit_text(
            $service,
            $crate::diag_abi::DiagLevel::Warn,
            0,
            0,
            0,
            &format!($($arg)+),
        );
    }};
}

#[macro_export]
macro_rules! diag_error {
    ($service:expr, $message:expr $(,)?) => {{
        $crate::emit_text(
            $service,
            $crate::diag_abi::DiagLevel::Error,
            0,
            0,
            0,
            $message,
        );
    }};
    ($service:expr, $($arg:tt)+) => {{
        $crate::emit_text(
            $service,
            $crate::diag_abi::DiagLevel::Error,
            0,
            0,
            0,
            &format!($($arg)+),
        );
    }};
}
