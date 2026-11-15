//! Canonical values shared by the PE64 loader, the Win32 policy service, and
//! the narrow ring0 syscall-frame adapter.
//!
//! - **Owner:** rustos-user-abi owns fixed Windows-compatible wire values and
//!   layouts; policy and mechanism remain in their named owners.
//! - **Boundary:** PE64 guests, winsys C shims, syscalld, loaderd, and ring0
//!   must agree on every value and byte layout.
//! - **Lifecycle:** constants are immutable; ABI versions change only with
//!   coordinated producers, consumers, probes, and retirement rules.
//! - **Concurrency:** this module contains immutable values and has no mutable
//!   runtime state.
//! - **Failure:** differential and layout tests reject host/reference drift
//!   before artifacts are admitted.
//! - **Forbidden:** no host-dependent typedef inference, reserved-field reuse,
//!   undocumented alias, or silent ABI widening.
//! - **Evidence:** `cpu-affinity-observation`, `task-affinity-lifecycle`, and
//!   `formal/run-abi-differential.sh`.

pub const ERROR_INVALID_FUNCTION: u32 = 1;
pub const ERROR_INVALID_HANDLE: u32 = 6;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
pub const ERROR_INVALID_LEVEL: u32 = 124;

pub const STATUS_SUCCESS: u64 = 0;
pub const STATUS_WAIT_0: u64 = 0;
pub const STATUS_TIMEOUT: u64 = 0x0000_0102;
pub const STATUS_IMAGE_NOT_AT_BASE: u64 = 0x4000_0003;
pub const STATUS_NOT_IMPLEMENTED: u64 = 0xc000_0002;
pub const STATUS_INVALID_INFO_CLASS: u64 = 0xc000_0003;
pub const STATUS_INFO_LENGTH_MISMATCH: u64 = 0xc000_0004;
pub const STATUS_INVALID_HANDLE: u64 = 0xc000_0008;
pub const STATUS_INVALID_PARAMETER: u64 = 0xc000_000d;
pub const STATUS_END_OF_FILE: u64 = 0xc000_0011;
pub const STATUS_NO_MEMORY: u64 = 0xc000_0017;
pub const STATUS_INVALID_SYSTEM_SERVICE: u64 = 0xc000_001c;
pub const STATUS_ACCESS_DENIED: u64 = 0xc000_0022;
pub const STATUS_BUFFER_TOO_SMALL: u64 = 0xc000_0023;
pub const STATUS_OBJECT_NAME_NOT_FOUND: u64 = 0xc000_0034;
pub const STATUS_OBJECT_PATH_NOT_FOUND: u64 = 0xc000_003a;

pub const HANDLE_STDIN: u64 = 0x1000_0001;
pub const HANDLE_STDOUT: u64 = 0x1000_0002;
pub const HANDLE_STDERR: u64 = 0x1000_0003;
pub const WIN32_FD_HANDLE_BASE: u64 = 0x1001_0000;
pub const WIN32_SECTION_HANDLE_BASE: u64 = 0x2001_0000;
pub const WIN32_EVENT_HANDLE_BASE: u64 = 0x3001_0000;
pub const HANDLE_CURRENT_PROCESS: u64 = u64::MAX;
pub const HANDLE_CURRENT_THREAD: u64 = u64::MAX - 1;

pub const BOOL_FALSE: u64 = 0;
pub const PAGE_SIZE: u64 = 4096;
pub const PAGE_NOACCESS: u64 = 0x0001;
pub const PAGE_READONLY: u64 = 0x0002;
pub const PAGE_READWRITE: u64 = 0x0004;
pub const PAGE_WRITECOPY: u64 = 0x0008;
pub const PAGE_EXECUTE: u64 = 0x0010;
pub const PAGE_EXECUTE_READ: u64 = 0x0020;
pub const PAGE_EXECUTE_READWRITE: u64 = 0x0040;
pub const PAGE_EXECUTE_WRITECOPY: u64 = 0x0080;
pub const MEM_COMMIT: u64 = 0x1000;
pub const MEM_RESERVE: u64 = 0x2000;
pub const MEM_DECOMMIT: u64 = 0x4000;
pub const MEM_RELEASE: u64 = 0x8000;
pub const MEM_FREE: u64 = 0x1_0000;
pub const MEM_PRIVATE: u64 = 0x2_0000;
pub const MEM_MAPPED: u64 = 0x4_0000;
pub const MEM_TOP_DOWN: u64 = 0x10_0000;
pub const MEM_IMAGE: u64 = 0x100_0000;

/// Microsoft documents the SystemBasicInformation buffer as 24 reserved
/// bytes, four pointer-sized reserved fields, and one signed processor count.
/// The final padding is part of the x86_64 C layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowsSystemBasicInformation {
    pub reserved1: [u8; 24],
    pub reserved2: [u64; 4],
    pub number_of_processors: i8,
    pub reserved3: [u8; 7],
}

impl WindowsSystemBasicInformation {
    pub const BYTES: usize = 64;

    pub const fn from_online_count(online_count: u8) -> Self {
        Self {
            reserved1: [0; 24],
            // Microsoft defines every field other than NumberOfProcessors as
            // reserved. Do not smuggle RustOS-private topology through a
            // buffer that is directly observable by PE applications.
            reserved2: [0; 4],
            number_of_processors: online_count as i8,
            reserved3: [0; 7],
        }
    }
}

const _: () = {
    assert!(core::mem::size_of::<WindowsSystemBasicInformation>() == 64);
    assert!(core::mem::offset_of!(WindowsSystemBasicInformation, reserved2) == 24);
    assert!(core::mem::offset_of!(WindowsSystemBasicInformation, number_of_processors) == 56);
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowsSystemInfo {
    pub processor_architecture: u16,
    pub reserved: u16,
    pub page_size: u32,
    pub minimum_application_address: u64,
    pub maximum_application_address: u64,
    pub active_processor_mask: u64,
    pub number_of_processors: u32,
    pub processor_type: u32,
    pub allocation_granularity: u32,
    pub processor_level: u16,
    pub processor_revision: u16,
}

const _: () = {
    assert!(core::mem::size_of::<WindowsSystemInfo>() == 48);
    assert!(core::mem::offset_of!(WindowsSystemInfo, active_processor_mask) == 24);
    assert!(core::mem::offset_of!(WindowsSystemInfo, number_of_processors) == 32);
};
