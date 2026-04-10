use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::fd::RawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use diag_abi::{
    DebugCrashSnapshotRequest, DebugDeviceState, DebugdRequest, DebugdResponseHeader, DebugdState,
    DiagProvider, DiagRecord, DEBUGD_COMMAND_GET_CRASH, DEBUGD_COMMAND_GET_RECORDS,
    DEBUGD_COMMAND_GET_STATE, DEBUGD_REQUEST_MAGIC, DEBUG_IOCTL_GET_STATE,
    DEBUG_IOCTL_SNAPSHOT_CRASH, DIAG_DEVICE_PATH, DIAG_RECORD_MAGIC, DIAG_SOCKET_PATH,
};

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_COLLECTED_RECORDS: usize = 4096;

struct CollectorState {
    records: VecDeque<DiagRecord>,
    crash_snapshot: Vec<u8>,
    dropped_records: u64,
}

impl CollectorState {
    fn new() -> Self {
        Self {
            records: VecDeque::with_capacity(MAX_COLLECTED_RECORDS),
            crash_snapshot: Vec::new(),
            dropped_records: 0,
        }
    }

    fn push_record(&mut self, record: DiagRecord) {
        let mirror_service = record.header.provider == DiagProvider::Service as u16;
        let mirror_heartbeat = record.header.provider == DiagProvider::Heartbeat as u16
            && record.header.level >= diag_abi::DiagLevel::Info as u8;
        if mirror_service || mirror_heartbeat {
            let _ = std::io::stderr().write_all(record.message_bytes());
            let _ = std::io::stderr().write_all(b"\n");
        }
        if self.records.len() == MAX_COLLECTED_RECORDS {
            self.records.pop_front();
            self.dropped_records = self.dropped_records.saturating_add(1);
        }
        self.records.push_back(record);
    }

    fn recent_records(&self, max_records: usize) -> Vec<DiagRecord> {
        let start = self.records.len().saturating_sub(max_records);
        self.records.iter().skip(start).copied().collect()
    }

    fn debugd_state(&self) -> DebugdState {
        DebugdState {
            collected_records: self.records.len() as u64,
            collector_dropped: self.dropped_records,
            crash_bytes: self.crash_snapshot.len() as u64,
            last_sequence: self
                .records
                .back()
                .map(|record| record.header.sequence)
                .unwrap_or(0),
        }
    }
}

fn main() {
    let collector = Arc::new(Mutex::new(CollectorState::new()));
    let listener = match bind_listener(DIAG_SOCKET_PATH) {
        Ok(listener) => listener,
        Err(_) => return,
    };

    let accept_collector = Arc::clone(&collector);
    thread::spawn(move || accept_connections(listener, accept_collector));

    let debug_fd = match open_debug_device() {
        Ok(fd) => fd,
        Err(_) => return,
    };
    let _ = snapshot_crash_store(debug_fd, &collector);

    loop {
        drain_kernel_records(debug_fd, &collector);
        thread::sleep(POLL_INTERVAL);
    }
}

fn bind_listener(path: &str) -> std::io::Result<UnixListener> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn accept_connections(listener: UnixListener, collector: Arc<Mutex<CollectorState>>) {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let connection_collector = Arc::clone(&collector);
                thread::spawn(move || handle_stream(&mut stream, &connection_collector));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => thread::sleep(POLL_INTERVAL),
        }
    }
}

fn handle_stream(stream: &mut UnixStream, collector: &Arc<Mutex<CollectorState>>) {
    loop {
        let mut magic = [0_u8; 4];
        if stream.read_exact(&mut magic).is_err() {
            return;
        }

        match u32::from_ne_bytes(magic) {
            DIAG_RECORD_MAGIC => {
                let mut record = DiagRecord::empty();
                let bytes = record_as_bytes_mut(&mut record);
                bytes[..magic.len()].copy_from_slice(&magic);
                if stream.read_exact(&mut bytes[magic.len()..]).is_ok() {
                    collector.lock().unwrap().push_record(record);
                    continue;
                }
                return;
            }
            DEBUGD_REQUEST_MAGIC => {
                let mut request = DebugdRequest::empty();
                let bytes = struct_as_bytes_mut(&mut request);
                bytes[..magic.len()].copy_from_slice(&magic);
                if stream.read_exact(&mut bytes[magic.len()..]).is_ok() {
                    handle_request(stream, collector, request);
                }
                return;
            }
            _ => return,
        }
    }
}

fn handle_request(
    stream: &mut UnixStream,
    collector: &Arc<Mutex<CollectorState>>,
    request: DebugdRequest,
) {
    match request.command {
        DEBUGD_COMMAND_GET_STATE => {
            let state = collector.lock().unwrap().debugd_state();
            let payload = struct_as_bytes(&state);
            let _ = write_response(
                stream,
                request.command,
                0,
                1,
                size_of::<DebugdState>() as u32,
                payload,
            );
        }
        DEBUGD_COMMAND_GET_RECORDS => {
            let max_records = usize::try_from(request.arg0).unwrap_or(0).max(1);
            let records = collector.lock().unwrap().recent_records(max_records);
            let payload = slice_as_bytes(&records);
            let _ = write_response(
                stream,
                request.command,
                0,
                records.len() as u64,
                size_of::<DiagRecord>() as u32,
                payload,
            );
        }
        DEBUGD_COMMAND_GET_CRASH => {
            let snapshot = collector.lock().unwrap().crash_snapshot.clone();
            let _ = write_response(stream, request.command, 0, 1, 1, snapshot.as_slice());
        }
        _ => {
            let _ = write_response(stream, request.command, libc::EINVAL, 0, 0, &[]);
        }
    }
}

fn write_response(
    stream: &mut UnixStream,
    command: u16,
    status: i32,
    item_count: u64,
    item_size: u32,
    payload: &[u8],
) -> std::io::Result<()> {
    let header = DebugdResponseHeader {
        magic: diag_abi::DEBUGD_RESPONSE_MAGIC,
        version: diag_abi::DIAG_VERSION,
        command,
        status,
        item_count,
        item_size,
        payload_len: payload.len() as u32,
    };
    stream.write_all(struct_as_bytes(&header))?;
    if !payload.is_empty() {
        stream.write_all(payload)?;
    }
    Ok(())
}

fn open_debug_device() -> Result<RawFd, i32> {
    let path = std::ffi::CString::new(DIAG_DEVICE_PATH).map_err(|_| libc::EINVAL)?;
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return Err(last_errno());
    }
    Ok(fd)
}

fn drain_kernel_records(fd: RawFd, collector: &Arc<Mutex<CollectorState>>) {
    let mut state = DebugDeviceState::empty();
    if ioctl_with_mut(fd, DEBUG_IOCTL_GET_STATE as libc::c_ulong, &mut state).is_err() {
        return;
    }
    if state.records_available == 0 {
        return;
    }

    let batch = usize::try_from(state.records_available.min(32)).unwrap_or(0);
    if batch == 0 {
        return;
    }
    let mut records = vec![DiagRecord::empty(); batch];
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            records.as_mut_ptr().cast::<u8>(),
            records.len() * size_of::<DiagRecord>(),
        )
    };
    let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast::<libc::c_void>(), bytes.len()) };
    if read <= 0 {
        return;
    }

    let count = usize::try_from(read).unwrap_or(0) / size_of::<DiagRecord>();
    let mut guard = collector.lock().unwrap();
    for record in records.into_iter().take(count) {
        guard.push_record(record);
    }
}

fn snapshot_crash_store(fd: RawFd, collector: &Arc<Mutex<CollectorState>>) -> Result<(), i32> {
    let mut state = DebugDeviceState::empty();
    ioctl_with_mut(fd, DEBUG_IOCTL_GET_STATE as libc::c_ulong, &mut state)?;
    if state.crash_available == 0 || state.crash_bytes == 0 {
        return Ok(());
    }

    let mut bytes = vec![0_u8; state.crash_bytes as usize];
    let mut request = DebugCrashSnapshotRequest {
        bytes_ptr: bytes.as_mut_ptr() as u64,
        capacity: bytes.len() as u64,
        count: 0,
    };
    ioctl_with_mut(
        fd,
        DEBUG_IOCTL_SNAPSHOT_CRASH as libc::c_ulong,
        &mut request,
    )?;
    bytes.truncate(usize::try_from(request.count).unwrap_or(0).min(bytes.len()));

    let mut guard = collector.lock().unwrap();
    guard.crash_snapshot = bytes;
    Ok(())
}

fn record_as_bytes_mut(record: &mut DiagRecord) -> &mut [u8] {
    unsafe {
        std::slice::from_raw_parts_mut(
            (record as *mut DiagRecord).cast::<u8>(),
            size_of::<DiagRecord>(),
        )
    }
}

fn struct_as_bytes<T>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn struct_as_bytes_mut<T>(value: &mut T) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut((value as *mut T).cast::<u8>(), size_of::<T>()) }
}

fn slice_as_bytes<T>(values: &[T]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values))
    }
}

fn ioctl_with_mut<T>(fd: RawFd, request: libc::c_ulong, arg: &mut T) -> Result<(), i32> {
    let status = unsafe { libc::ioctl(fd, request, arg as *mut T) };
    if status < 0 {
        return Err(last_errno());
    }
    Ok(())
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}
