//! Windows syscall-number decode owned by the narrow ring0 frame adapter.
//!
//! - **Owner:** kernel-compat owns number decoding; syscalld owns Win32 policy.
//! - **Boundary:** untrusted PE64 syscall numbers become one closed `Api` enum.
//! - **Lifecycle:** decode an exact fixed number, dispatch once, then discard
//!   the enum with the syscall frame.
//! - **Concurrency:** immutable constants and pure decoding require no lock.
//! - **Failure:** unknown, tombstoned, or out-of-range numbers are rejected.
//! - **Forbidden:** no dynamic registration, alias, host syscall inference, or
//!   policy implementation in this module.
//! - **Evidence:** `cpu-affinity-observation`, `task-affinity-lifecycle`, and
//!   the Windows ABI differential probe.

// RING3-MIGRATION-REFERENCE START: decode exception: syscalld/loaderd own Win32
// syscall policy. Ring0 keeps syscall number decode substrate.
const SYSCALL_BASE: u64 = 0x1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Api {
    NtWriteFile = 3,
    NtReadFile = 4,
    NtDelayExecution = 5,
    NtClose = 6,
    NtGetConsoleMode = 12,
    NtSetConsoleMode = 13,
    RtlExitUserProcess = 14,
    NtAllocateVirtualMemory = 19,
    NtFreeVirtualMemory = 20,
    NtProtectVirtualMemory = 21,
    NtQueryVirtualMemory = 70,
    NtQuerySystemInformation = 71,
    RustosQueryProcessAffinity = 72,
    RustosSetProcessAffinity = 73,
    RustosSetThreadAffinity = 74,
    RustosGetCurrentProcessorNumber = 75,
}

impl Api {
    pub const fn syscall_number(self) -> u64 {
        SYSCALL_BASE + self as u64
    }

    pub fn from_syscall_number(syscall_number: u64) -> Option<Self> {
        match syscall_number {
            value if value == Self::NtWriteFile.syscall_number() => Some(Self::NtWriteFile),
            value if value == Self::NtReadFile.syscall_number() => Some(Self::NtReadFile),
            value if value == Self::NtDelayExecution.syscall_number() => {
                Some(Self::NtDelayExecution)
            }
            value if value == Self::NtClose.syscall_number() => Some(Self::NtClose),
            value if value == Self::NtGetConsoleMode.syscall_number() => {
                Some(Self::NtGetConsoleMode)
            }
            value if value == Self::NtSetConsoleMode.syscall_number() => {
                Some(Self::NtSetConsoleMode)
            }
            value if value == Self::RtlExitUserProcess.syscall_number() => {
                Some(Self::RtlExitUserProcess)
            }
            value if value == Self::NtAllocateVirtualMemory.syscall_number() => {
                Some(Self::NtAllocateVirtualMemory)
            }
            value if value == Self::NtFreeVirtualMemory.syscall_number() => {
                Some(Self::NtFreeVirtualMemory)
            }
            value if value == Self::NtProtectVirtualMemory.syscall_number() => {
                Some(Self::NtProtectVirtualMemory)
            }
            value if value == Self::NtQueryVirtualMemory.syscall_number() => {
                Some(Self::NtQueryVirtualMemory)
            }
            value if value == Self::NtQuerySystemInformation.syscall_number() => {
                Some(Self::NtQuerySystemInformation)
            }
            value if value == Self::RustosQueryProcessAffinity.syscall_number() => {
                Some(Self::RustosQueryProcessAffinity)
            }
            value if value == Self::RustosSetProcessAffinity.syscall_number() => {
                Some(Self::RustosSetProcessAffinity)
            }
            value if value == Self::RustosSetThreadAffinity.syscall_number() => {
                Some(Self::RustosSetThreadAffinity)
            }
            value if value == Self::RustosGetCurrentProcessorNumber.syscall_number() => {
                Some(Self::RustosGetCurrentProcessorNumber)
            }
            _ => None,
        }
    }
}
// RING3-MIGRATION-REFERENCE END: syscalld/loaderd-owned Win32 syscall decode exception.
