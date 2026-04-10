use std::ffi::CString;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;

use diag_abi::{
    CrashStoreHeader, DebugCrashSnapshotRequest, DebugDeviceState, DebugModuleInfo,
    DebugModuleSnapshotRequest, DebugdRequest, DebugdResponseHeader, DebugdState, DiagRecord,
    DEBUGD_COMMAND_GET_CRASH, DEBUGD_COMMAND_GET_RECORDS, DEBUGD_COMMAND_GET_STATE,
    DEBUG_IOCTL_GET_STATE, DEBUG_IOCTL_SNAPSHOT_CRASH, DEBUG_IOCTL_SNAPSHOT_MODULES,
    DIAG_DEVICE_PATH, DIAG_SOCKET_PATH,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| String::from("state"));
    let arg = args.next();

    let result = match command.as_str() {
        "state" => print_state(),
        "tail" => print_tail(arg.as_deref()),
        "modules" => print_modules(),
        "crash" => print_crash(),
        other => Err(format!("unknown command: {other}")),
    };

    if let Err(err) = result {
        eprintln!("debugctl: {err}");
        std::process::exit(1);
    }
}

fn print_state() -> Result<(), String> {
    let mut printed = false;

    if let Ok(state) = request_debugd_state() {
        println!("debugd.collected_records={}", state.collected_records);
        println!("debugd.collector_dropped={}", state.collector_dropped);
        println!("debugd.crash_bytes={}", state.crash_bytes);
        println!("debugd.last_sequence={}", state.last_sequence);
        printed = true;
    }

    if let Ok(fd) = open_debug_device() {
        let result = print_kernel_state(fd);
        unsafe {
            libc::close(fd);
        }
        result?;
        printed = true;
    }

    if printed {
        Ok(())
    } else {
        Err(String::from(
            "debugd socket and /dev/debug0 are both unavailable",
        ))
    }
}

fn print_kernel_state(fd: RawFd) -> Result<(), String> {
    let mut state = DebugDeviceState::empty();
    ioctl_with_mut(fd, DEBUG_IOCTL_GET_STATE as libc::c_ulong, &mut state)
        .map_err(|err| format!("get state failed: errno={err}"))?;
    println!("kernel.record_size={}", state.record_size);
    println!("kernel.ring_capacity={}", state.ring_capacity);
    println!("kernel.records_available={}", state.records_available);
    println!("kernel.total_sequence={}", state.total_sequence);
    println!("kernel.dropped_records={}", state.dropped_records);
    println!("kernel.filter_mask={:#x}", state.filter_mask);
    println!("kernel.min_level={}", state.min_level);
    println!("kernel.enabled={}", state.enabled);
    println!("kernel.crash_available={}", state.crash_available);
    println!("kernel.crash_bytes={}", state.crash_bytes);
    Ok(())
}

fn print_tail(arg: Option<&str>) -> Result<(), String> {
    let max_records = arg
        .unwrap_or("32")
        .parse::<u64>()
        .map_err(|err| format!("invalid tail count: {err}"))?;
    let records = request_debugd_records(max_records)?;
    if records.is_empty() {
        println!("no records");
        return Ok(());
    }

    for record in records {
        println!(
            "seq={} stage={} level={} provider={} event={} pid={} tid={} obj={} msg={}",
            record.header.sequence,
            stage_name(record.header.stage),
            level_name(record.header.level),
            provider_name(record.header.provider),
            record.header.event_id,
            record.header.process_id,
            record.header.thread_id,
            record.header.object_id,
            decode_message(&record),
        );
    }
    Ok(())
}

fn print_modules() -> Result<(), String> {
    let fd = open_debug_device().map_err(|err| {
        format!(
            "open {} failed for module snapshot: errno={err}",
            DIAG_DEVICE_PATH
        )
    })?;
    let result = print_modules_from_device(fd);
    unsafe {
        libc::close(fd);
    }
    result
}

fn print_modules_from_device(fd: RawFd) -> Result<(), String> {
    let mut modules = vec![DebugModuleInfo::empty(); 64];
    let mut request = DebugModuleSnapshotRequest {
        modules_ptr: modules.as_mut_ptr() as u64,
        capacity: modules.len() as u64,
        count: 0,
    };
    ioctl_with_mut(
        fd,
        DEBUG_IOCTL_SNAPSHOT_MODULES as libc::c_ulong,
        &mut request,
    )
    .map_err(|err| format!("snapshot modules failed: errno={err}"))?;
    let count = usize::try_from(request.count)
        .unwrap_or(0)
        .min(modules.len());
    for module in &modules[..count] {
        println!(
            "name={} runtime_base={:#x} host_base={:#x} size={} path={}",
            decode_fixed(&module.name),
            module.runtime_base,
            module.host_base,
            module.size,
            decode_fixed(&module.image_path),
        );
    }
    Ok(())
}

fn print_crash() -> Result<(), String> {
    if let Ok(bytes) = request_debugd_crash() {
        return print_crash_bytes(&bytes);
    }

    let fd = open_debug_device()
        .map_err(|err| format!("open {} failed: errno={err}", DIAG_DEVICE_PATH))?;
    let result = print_crash_from_device(fd);
    unsafe {
        libc::close(fd);
    }
    result
}

fn print_crash_from_device(fd: RawFd) -> Result<(), String> {
    let mut state = DebugDeviceState::empty();
    ioctl_with_mut(fd, DEBUG_IOCTL_GET_STATE as libc::c_ulong, &mut state)
        .map_err(|err| format!("get state failed: errno={err}"))?;
    if state.crash_available == 0 || state.crash_bytes == 0 {
        println!("no crash snapshot");
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
    )
    .map_err(|err| format!("snapshot crash failed: errno={err}"))?;
    bytes.truncate(usize::try_from(request.count).unwrap_or(0).min(bytes.len()));
    print_crash_bytes(&bytes)
}

fn print_crash_bytes(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() {
        println!("no crash snapshot");
        return Ok(());
    }
    if bytes.len() < size_of::<CrashStoreHeader>() {
        return Err(String::from("crash snapshot is smaller than header"));
    }

    let header = read_struct::<CrashStoreHeader>(&bytes[..size_of::<CrashStoreHeader>()]);
    println!("crash.record_count={}", header.record_count);
    println!("crash.panic_text_len={}", header.panic_text_len);
    println!("crash.last_sequence={}", header.last_sequence);

    let records_offset = size_of::<CrashStoreHeader>();
    let records_bytes = usize::try_from(header.record_count)
        .unwrap_or(0)
        .saturating_mul(size_of::<DiagRecord>());
    let text_offset = records_offset
        .saturating_add(records_bytes)
        .min(bytes.len());
    let records_end = text_offset.min(bytes.len());
    for record in decode_records(&bytes[records_offset..records_end]) {
        println!(
            "crash.seq={} stage={} level={} provider={} event={} msg={}",
            record.header.sequence,
            stage_name(record.header.stage),
            level_name(record.header.level),
            provider_name(record.header.provider),
            record.header.event_id,
            decode_message(&record),
        );
    }

    let panic_text_len = usize::try_from(header.panic_text_len).unwrap_or(0);
    let panic_end = text_offset.saturating_add(panic_text_len).min(bytes.len());
    let panic_text = String::from_utf8_lossy(&bytes[text_offset..panic_end]);
    if !panic_text.is_empty() {
        println!("crash.panic_text={panic_text}");
    }
    Ok(())
}

fn request_debugd_state() -> Result<DebugdState, String> {
    let payload = request_debugd(DEBUGD_COMMAND_GET_STATE, 0)?;
    if payload.len() != size_of::<DebugdState>() {
        return Err(format!("unexpected debugd state size: {}", payload.len()));
    }
    Ok(read_struct::<DebugdState>(&payload))
}

fn request_debugd_records(max_records: u64) -> Result<Vec<DiagRecord>, String> {
    let payload = request_debugd(DEBUGD_COMMAND_GET_RECORDS, max_records)?;
    Ok(decode_records(&payload))
}

fn request_debugd_crash() -> Result<Vec<u8>, String> {
    request_debugd(DEBUGD_COMMAND_GET_CRASH, 0)
}

fn request_debugd(command: u16, arg0: u64) -> Result<Vec<u8>, String> {
    let mut stream = UnixStream::connect(DIAG_SOCKET_PATH)
        .map_err(|err| format!("connect {} failed: {err}", DIAG_SOCKET_PATH))?;
    let request = DebugdRequest::new(command, arg0, 0);
    stream
        .write_all(struct_as_bytes(&request))
        .map_err(|err| format!("write debugd request failed: {err}"))?;

    let mut header = DebugdResponseHeader::empty();
    stream
        .read_exact(struct_as_bytes_mut(&mut header))
        .map_err(|err| format!("read debugd response header failed: {err}"))?;
    if header.magic != diag_abi::DEBUGD_RESPONSE_MAGIC {
        return Err(String::from("invalid debugd response magic"));
    }
    if header.status != 0 {
        return Err(format!("debugd request failed: errno={}", header.status));
    }

    let mut payload = vec![0_u8; header.payload_len as usize];
    if !payload.is_empty() {
        stream
            .read_exact(&mut payload)
            .map_err(|err| format!("read debugd payload failed: {err}"))?;
    }
    Ok(payload)
}

fn open_debug_device() -> Result<RawFd, i32> {
    let path = CString::new(DIAG_DEVICE_PATH).map_err(|_| libc::EINVAL)?;
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return Err(last_errno());
    }
    Ok(fd)
}

fn decode_records(bytes: &[u8]) -> Vec<DiagRecord> {
    bytes
        .chunks_exact(size_of::<DiagRecord>())
        .map(read_struct::<DiagRecord>)
        .collect()
}

fn decode_message(record: &DiagRecord) -> String {
    String::from_utf8_lossy(record.message_bytes()).into_owned()
}

fn decode_fixed(bytes: &[u8]) -> String {
    String::from_utf8_lossy(diag_abi::decode_fixed(bytes)).into_owned()
}

fn stage_name(stage: u8) -> &'static str {
    match stage {
        1 => "bootloader",
        2 => "prekernel",
        3 => "kernel-boot",
        4 => "kernel",
        5 => "user",
        6 => "crash",
        _ => "unknown",
    }
}

fn level_name(level: u8) -> &'static str {
    match level {
        1 => "trace",
        2 => "debug",
        3 => "info",
        4 => "warn",
        5 => "error",
        6 => "fatal",
        _ => "unknown",
    }
}

fn provider_name(provider: u16) -> &'static str {
    match provider {
        1 => "legacy",
        2 => "breadcrumb",
        3 => "boot",
        4 => "panic",
        5 => "sched",
        6 => "syscall",
        7 => "driver",
        8 => "console",
        9 => "heartbeat",
        10 => "io",
        11 => "service",
        12 => "module",
        13 => "debug",
        _ => "unknown",
    }
}

fn read_struct<T: Copy>(bytes: &[u8]) -> T {
    assert!(bytes.len() >= size_of::<T>());
    unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
}

fn struct_as_bytes<T>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn struct_as_bytes_mut<T>(value: &mut T) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut((value as *mut T).cast::<u8>(), size_of::<T>()) }
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
