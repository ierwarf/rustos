#![no_std]

use core::mem::size_of;

pub const DIAG_RECORD_MAGIC: u32 = 0x4449_4147; // "DIAG"
pub const DIAG_BUFFER_MAGIC: u32 = 0x4442_5546; // "DBUF"
pub const DIAG_CRASH_MAGIC: u32 = 0x4443_5248; // "DCRH"
pub const DEBUGD_REQUEST_MAGIC: u32 = 0x4452_5154; // "DRQT"
pub const DEBUGD_RESPONSE_MAGIC: u32 = 0x4452_5350; // "DRSP"
pub const DIAG_VERSION: u16 = 1;
pub const DIAG_DEVICE_PATH: &str = "/dev/debug0";
pub const DIAG_SOCKET_PATH: &str = "/run/debugd.sock";
pub const DIAG_PAYLOAD_BYTES: usize = 160;
pub const DIAG_BOOT_BUFFER_RECORD_CAPACITY: usize = 128;
pub const DIAG_CRASH_RECORD_CAPACITY: usize = 128;
pub const DIAG_CRASH_TEXT_BYTES: usize = 512;
pub const DIAG_MODULE_NAME_BYTES: usize = 48;
pub const DIAG_MODULE_PATH_BYTES: usize = 96;

const LINUX_IOC_NRBITS: u64 = 8;
const LINUX_IOC_TYPEBITS: u64 = 8;
const LINUX_IOC_SIZEBITS: u64 = 14;
const LINUX_IOC_NRSHIFT: u64 = 0;
const LINUX_IOC_TYPESHIFT: u64 = LINUX_IOC_NRSHIFT + LINUX_IOC_NRBITS;
const LINUX_IOC_SIZESHIFT: u64 = LINUX_IOC_TYPESHIFT + LINUX_IOC_TYPEBITS;
const LINUX_IOC_DIRSHIFT: u64 = LINUX_IOC_SIZESHIFT + LINUX_IOC_SIZEBITS;
const LINUX_IOC_WRITE: u64 = 1;
const LINUX_IOC_READ: u64 = 2;

const fn linux_ioc(dir: u64, type_: u8, nr: u8, size: u64) -> u64 {
    (dir << LINUX_IOC_DIRSHIFT)
        | ((type_ as u64) << LINUX_IOC_TYPESHIFT)
        | ((nr as u64) << LINUX_IOC_NRSHIFT)
        | (size << LINUX_IOC_SIZESHIFT)
}

const fn linux_ior<T>(type_: u8, nr: u8) -> u64 {
    linux_ioc(LINUX_IOC_READ, type_, nr, size_of::<T>() as u64)
}

const fn linux_iow<T>(type_: u8, nr: u8) -> u64 {
    linux_ioc(LINUX_IOC_WRITE, type_, nr, size_of::<T>() as u64)
}

const fn linux_iowr<T>(type_: u8, nr: u8) -> u64 {
    linux_ioc(
        LINUX_IOC_READ | LINUX_IOC_WRITE,
        type_,
        nr,
        size_of::<T>() as u64,
    )
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DiagStage {
    #[default]
    Unknown = 0,
    Bootloader = 1,
    Prekernel = 2,
    KernelBoot = 3,
    Kernel = 4,
    User = 5,
    Crash = 6,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub enum DiagLevel {
    Trace = 1,
    Debug = 2,
    #[default]
    Info = 3,
    Warn = 4,
    Error = 5,
    Fatal = 6,
}

#[repr(u16)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DiagProvider {
    #[default]
    Legacy = 1,
    Breadcrumb = 2,
    Boot = 3,
    Panic = 4,
    Sched = 5,
    Syscall = 6,
    Driver = 7,
    Console = 8,
    Heartbeat = 9,
    Io = 10,
    Service = 11,
    Module = 12,
    Debug = 13,
}

impl DiagProvider {
    pub const fn bit(self) -> u64 {
        1_u64 << ((self as u16 as u64) & 63)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DiagRecordHeader {
    pub magic: u32,
    pub version: u16,
    pub stage: u8,
    pub level: u8,
    pub provider: u16,
    pub event_id: u16,
    pub flags: u32,
    pub sequence: u64,
    pub timestamp_micros: u64,
    pub span_id: u64,
    pub object_id: u64,
    pub process_id: u64,
    pub thread_id: u64,
    pub cpu_id: u32,
    pub payload_len: u16,
    pub reserved: u16,
}

impl DiagRecordHeader {
    pub const fn empty() -> Self {
        Self {
            magic: DIAG_RECORD_MAGIC,
            version: DIAG_VERSION,
            stage: DiagStage::Unknown as u8,
            level: DiagLevel::Info as u8,
            provider: DiagProvider::Legacy as u16,
            event_id: 0,
            flags: 0,
            sequence: 0,
            timestamp_micros: 0,
            span_id: 0,
            object_id: 0,
            process_id: 0,
            thread_id: 0,
            cpu_id: 0,
            payload_len: 0,
            reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DiagRecord {
    pub header: DiagRecordHeader,
    pub payload: [u8; DIAG_PAYLOAD_BYTES],
}

impl DiagRecord {
    pub const fn empty() -> Self {
        Self {
            header: DiagRecordHeader::empty(),
            payload: [0; DIAG_PAYLOAD_BYTES],
        }
    }

    pub fn set_payload_bytes(&mut self, bytes: &[u8]) {
        let len = bytes.len().min(self.payload.len());
        self.payload = [0; DIAG_PAYLOAD_BYTES];
        self.payload[..len].copy_from_slice(&bytes[..len]);
        self.header.payload_len = len as u16;
    }

    pub fn message_bytes(&self) -> &[u8] {
        &self.payload[..usize::from(self.header.payload_len).min(self.payload.len())]
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DiagSharedBufferHeader {
    pub magic: u32,
    pub version: u16,
    pub record_capacity: u16,
    pub next_sequence: u64,
    pub dropped_records: u64,
    pub reserved0: u64,
    pub reserved1: u64,
}

impl DiagSharedBufferHeader {
    pub const fn empty(record_capacity: u16) -> Self {
        Self {
            magic: DIAG_BUFFER_MAGIC,
            version: DIAG_VERSION,
            record_capacity,
            next_sequence: 0,
            dropped_records: 0,
            reserved0: 0,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BootDiagBufferInfo {
    pub addr: u64,
    pub bytes_len: u64,
    pub record_capacity: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CrashStoreHeader {
    pub magic: u32,
    pub version: u16,
    pub flags: u16,
    pub record_count: u32,
    pub panic_text_len: u32,
    pub last_sequence: u64,
    pub reserved0: u64,
    pub reserved1: u64,
}

impl CrashStoreHeader {
    pub const fn empty() -> Self {
        Self {
            magic: DIAG_CRASH_MAGIC,
            version: DIAG_VERSION,
            flags: 0,
            record_count: 0,
            panic_text_len: 0,
            last_sequence: 0,
            reserved0: 0,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CrashStoreInfo {
    pub addr: u64,
    pub bytes_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DebugDeviceState {
    pub record_size: u32,
    pub ring_capacity: u32,
    pub records_available: u64,
    pub total_sequence: u64,
    pub dropped_records: u64,
    pub filter_mask: u64,
    pub min_level: u8,
    pub enabled: u8,
    pub reserved0: u16,
    pub crash_available: u32,
    pub crash_bytes: u32,
}

impl DebugDeviceState {
    pub const fn empty() -> Self {
        Self {
            record_size: size_of::<DiagRecord>() as u32,
            ring_capacity: 0,
            records_available: 0,
            total_sequence: 0,
            dropped_records: 0,
            filter_mask: 0,
            min_level: DiagLevel::Trace as u8,
            enabled: 1,
            reserved0: 0,
            crash_available: 0,
            crash_bytes: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DebugConfigureRequest {
    pub enabled: u8,
    pub min_level: u8,
    pub reserved0: u16,
    pub provider_mask: u64,
}

impl DebugConfigureRequest {
    pub const fn default_enabled() -> Self {
        Self {
            enabled: 1,
            min_level: DiagLevel::Trace as u8,
            reserved0: 0,
            provider_mask: u64::MAX,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DebugCrashSnapshotRequest {
    pub bytes_ptr: u64,
    pub capacity: u64,
    pub count: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DebugModuleInfo {
    pub runtime_base: u64,
    pub host_base: u64,
    pub size: u64,
    pub name: [u8; DIAG_MODULE_NAME_BYTES],
    pub image_path: [u8; DIAG_MODULE_PATH_BYTES],
}

impl DebugModuleInfo {
    pub const fn empty() -> Self {
        Self {
            runtime_base: 0,
            host_base: 0,
            size: 0,
            name: [0; DIAG_MODULE_NAME_BYTES],
            image_path: [0; DIAG_MODULE_PATH_BYTES],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DebugModuleSnapshotRequest {
    pub modules_ptr: u64,
    pub capacity: u64,
    pub count: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DebugBreakRequest {
    pub reason_code: u32,
    pub reserved: u32,
}

pub const DEBUGD_COMMAND_GET_STATE: u16 = 1;
pub const DEBUGD_COMMAND_GET_RECORDS: u16 = 2;
pub const DEBUGD_COMMAND_GET_CRASH: u16 = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DebugdRequest {
    pub magic: u32,
    pub version: u16,
    pub command: u16,
    pub arg0: u64,
    pub arg1: u64,
}

impl DebugdRequest {
    pub const fn new(command: u16, arg0: u64, arg1: u64) -> Self {
        Self {
            magic: DEBUGD_REQUEST_MAGIC,
            version: DIAG_VERSION,
            command,
            arg0,
            arg1,
        }
    }

    pub const fn empty() -> Self {
        Self::new(0, 0, 0)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DebugdResponseHeader {
    pub magic: u32,
    pub version: u16,
    pub command: u16,
    pub status: i32,
    pub item_count: u64,
    pub item_size: u32,
    pub payload_len: u32,
}

impl DebugdResponseHeader {
    pub const fn empty() -> Self {
        Self {
            magic: DEBUGD_RESPONSE_MAGIC,
            version: DIAG_VERSION,
            command: 0,
            status: 0,
            item_count: 0,
            item_size: 0,
            payload_len: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DebugdState {
    pub collected_records: u64,
    pub collector_dropped: u64,
    pub crash_bytes: u64,
    pub last_sequence: u64,
}

impl DebugdState {
    pub const fn empty() -> Self {
        Self {
            collected_records: 0,
            collector_dropped: 0,
            crash_bytes: 0,
            last_sequence: 0,
        }
    }
}

const DEBUG_IOCTL_TYPE: u8 = b'G';

pub const DEBUG_IOCTL_GET_STATE: u64 = linux_ior::<DebugDeviceState>(DEBUG_IOCTL_TYPE, 1);
pub const DEBUG_IOCTL_CONFIGURE: u64 = linux_iow::<DebugConfigureRequest>(DEBUG_IOCTL_TYPE, 2);
pub const DEBUG_IOCTL_SNAPSHOT_CRASH: u64 =
    linux_iowr::<DebugCrashSnapshotRequest>(DEBUG_IOCTL_TYPE, 3);
pub const DEBUG_IOCTL_SNAPSHOT_MODULES: u64 =
    linux_iowr::<DebugModuleSnapshotRequest>(DEBUG_IOCTL_TYPE, 4);
pub const DEBUG_IOCTL_TRIGGER_BREAK: u64 = linux_iow::<DebugBreakRequest>(DEBUG_IOCTL_TYPE, 5);

pub const fn diag_buffer_bytes(record_capacity: usize) -> usize {
    size_of::<DiagSharedBufferHeader>() + size_of::<DiagRecord>() * record_capacity
}

pub const fn crash_store_bytes(record_capacity: usize) -> usize {
    size_of::<CrashStoreHeader>()
        + size_of::<DiagRecord>() * record_capacity
        + DIAG_CRASH_TEXT_BYTES
}

pub fn encode_fixed(dest: &mut [u8], text: &str) {
    let bytes = text.as_bytes();
    let len = bytes.len().min(dest.len().saturating_sub(1));
    for byte in dest.iter_mut() {
        *byte = 0;
    }
    if len != 0 {
        dest[..len].copy_from_slice(&bytes[..len]);
    }
}

pub fn decode_fixed(bytes: &[u8]) -> &[u8] {
    let mut end = 0;
    while end < bytes.len() && bytes[end] != 0 {
        end += 1;
    }
    &bytes[..end]
}
