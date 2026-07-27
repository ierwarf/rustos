//! Canonical values shared by the PE64 loader, the Win32 policy service, and
//! the narrow ring0 syscall-frame adapter.
//!
//! These constants are deliberately independent from host headers. The
//! differential ABI gate compiles Microsoft-compatible headers separately and
//! rejects drift between that reference output and this module.

pub const ERROR_INVALID_FUNCTION: u32 = 1;
pub const ERROR_INVALID_HANDLE: u32 = 6;
pub const ERROR_INVALID_PARAMETER: u32 = 87;

pub const STATUS_INVALID_HANDLE: u64 = 0xc000_0008;
pub const STATUS_INVALID_PARAMETER: u64 = 0xc000_000d;
pub const STATUS_INVALID_SYSTEM_SERVICE: u64 = 0xc000_001c;

pub const HANDLE_STDIN: u64 = 0x1000_0001;
pub const HANDLE_STDOUT: u64 = 0x1000_0002;
pub const HANDLE_STDERR: u64 = 0x1000_0003;
pub const HANDLE_CURRENT_PROCESS: u64 = u64::MAX;

pub const BOOL_FALSE: u64 = 0;
pub const PAGE_SIZE: u64 = 4096;
pub const PAGE_NOACCESS: u64 = 0x0001;
pub const PAGE_READONLY: u64 = 0x0002;
pub const PAGE_READWRITE: u64 = 0x0004;
pub const PAGE_EXECUTE_READ: u64 = 0x0020;
pub const PAGE_EXECUTE_READWRITE: u64 = 0x0040;
pub const MEM_COMMIT: u64 = 0x1000;
pub const MEM_RESERVE: u64 = 0x2000;
pub const MEM_RELEASE: u64 = 0x8000;
