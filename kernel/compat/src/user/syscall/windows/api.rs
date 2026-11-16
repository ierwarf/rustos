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

/// Wine 11.22 Windows x64 NT system call table (`vendor/wine/dlls/ntdll/ntsyscalls.h`).
#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WineNtApi {
    NtWaitForSingleObject = 0x0004,
    NtReadFile = 0x0006,
    NtDeviceIoControlFile = 0x0007,
    NtWriteFile = 0x0008,
    NtReleaseSemaphore = 0x000a,
    NtSetInformationThread = 0x000d,
    NtSetEvent = 0x000e,
    NtClose = 0x000f,
    NtQueryObject = 0x0010,
    NtQueryInformationFile = 0x0011,
    NtOpenKey = 0x0012,
    NtEnumerateValueKey = 0x0013,
    NtQueryDefaultLocale = 0x0015,
    NtQueryKey = 0x0016,
    NtQueryValueKey = 0x0017,
    NtAllocateVirtualMemory = 0x0018,
    NtQueryInformationProcess = 0x0019,
    NtSetInformationProcess = 0x001c,
    NtCreateKey = 0x001d,
    NtFreeVirtualMemory = 0x001e,
    NtReleaseMutant = 0x0020,
    NtQueryInformationToken = 0x0021,
    NtQueryVirtualMemory = 0x0023,
    NtOpenThreadToken = 0x0024,
    NtQueryInformationThread = 0x0025,
    NtOpenProcess = 0x0026,
    NtSetInformationFile = 0x0027,
    NtMapViewOfSection = 0x0028,
    NtUnmapViewOfSection = 0x002a,
    NtTerminateProcess = 0x002c,
    NtOpenThreadTokenEx = 0x002f,
    NtOpenProcessTokenEx = 0x0030,
    NtQueryPerformanceCounter = 0x0031,
    NtEnumerateKey = 0x0032,
    NtOpenFile = 0x0033,
    NtDelayExecution = 0x0034,
    NtQueryDirectoryFile = 0x0035,
    NtQuerySystemInformation = 0x0036,
    NtOpenSection = 0x0037,
    NtFsControlFile = 0x0039,
    NtWriteVirtualMemory = 0x003a,
    NtDuplicateObject = 0x003c,
    NtQueryAttributesFile = 0x003d,
    NtClearEvent = 0x003e,
    NtReadVirtualMemory = 0x003f,
    NtOpenEvent = 0x0040,
    NtContinue = 0x0043,
    NtQueryDefaultUILanguage = 0x0044,
    NtYieldExecution = 0x0046,
    NtCreateEvent = 0x0048,
    NtQueryVolumeInformationFile = 0x0049,
    NtCreateSection = 0x004a,
    NtFlushBuffersFile = 0x004b,
    NtCreateThread = 0x004e,
    NtProtectVirtualMemory = 0x0050,
    NtQuerySection = 0x0051,
    NtResumeThread = 0x0052,
    NtTerminateThread = 0x0053,
    NtCreateFile = 0x0055,
    NtQueryEvent = 0x0056,
    NtOpenDirectoryObject = 0x0058,
    NtQuerySystemTime = 0x005a,
    NtWaitForMultipleObjects = 0x005b,
    NtSetInformationObject = 0x005c,
    NtSetValueKey = 0x0060,
    NtAlertThreadByThreadId = 0x0067,
    NtAllocateLocallyUniqueId = 0x0068,
    NtAllocateVirtualMemoryEx = 0x006b,
    NtAreMappedFilesTheSame = 0x0072,
    NtContinueEx = 0x007b,
    NtCreateDirectoryObject = 0x007d,
    NtCreateMutant = 0x0084,
    NtCreateNamedPipeFile = 0x0085,
    NtCreateSectionEx = 0x0088,
    NtCreateSemaphore = 0x0089,
    NtCreateThreadEx = 0x008b,
    NtDeleteKey = 0x0094,
    NtDeleteValueKey = 0x0095,
    NtFlushInstructionCache = 0x0099,
    NtFlushKey = 0x009a,
    NtFlushVirtualMemory = 0x009c,
    NtGetContextThread = 0x009d,
    NtGetCurrentProcessorNumber = 0x009e,
    NtGetNlsSectionPtr = 0x00a1,
    NtInitializeNlsFiles = 0x00a4,
    NtMapViewOfSectionEx = 0x00b0,
    NtOpenKeyEx = 0x00b6,
    NtOpenKeyedEvent = 0x00b9,
    NtOpenMutant = 0x00ba,
    NtOpenProcessToken = 0x00bc,
    NtOpenSemaphore = 0x00bd,
    NtOpenSymbolicLinkObject = 0x00be,
    NtOpenThread = 0x00bf,
    NtPulseEvent = 0x00c2,
    NtQueryDirectoryObject = 0x00c3,
    NtQueryFullAttributesFile = 0x00c5,
    NtQueryInstallUILanguage = 0x00c8,
    NtQueryMutant = 0x00cc,
    NtQuerySecurityObject = 0x00cd,
    NtQuerySemaphore = 0x00ce,
    NtQuerySymbolicLinkObject = 0x00cf,
    NtRaiseException = 0x00d6,
    NtReleaseKeyedEvent = 0x00d9,
    NtResetEvent = 0x00de,
    NtSetContextThread = 0x00e5,
    NtSetDefaultLocale = 0x00e7,
    NtSetDefaultUILanguage = 0x00e8,
    NtSuspendThread = 0x00fc,
    NtTestAlert = 0x00ff,
    NtUnmapViewOfSectionEx = 0x0105,
    NtWaitForAlertByThreadId = 0x0106,
    NtWaitForKeyedEvent = 0x0108,
}

impl WineNtApi {
    pub const fn from_syscall_number(raw: u64) -> Option<Self> {
        if raw >= SYSCALL_BASE {
            return None;
        }
        match raw as u16 {
            0x0004 => Some(Self::NtWaitForSingleObject),
            0x0006 => Some(Self::NtReadFile),
            0x0007 => Some(Self::NtDeviceIoControlFile),
            0x0008 => Some(Self::NtWriteFile),
            0x000a => Some(Self::NtReleaseSemaphore),
            0x000d => Some(Self::NtSetInformationThread),
            0x000e => Some(Self::NtSetEvent),
            0x000f => Some(Self::NtClose),
            0x0010 => Some(Self::NtQueryObject),
            0x0011 => Some(Self::NtQueryInformationFile),
            0x0012 => Some(Self::NtOpenKey),
            0x0013 => Some(Self::NtEnumerateValueKey),
            0x0015 => Some(Self::NtQueryDefaultLocale),
            0x0016 => Some(Self::NtQueryKey),
            0x0017 => Some(Self::NtQueryValueKey),
            0x0018 => Some(Self::NtAllocateVirtualMemory),
            0x0019 => Some(Self::NtQueryInformationProcess),
            0x001c => Some(Self::NtSetInformationProcess),
            0x001d => Some(Self::NtCreateKey),
            0x001e => Some(Self::NtFreeVirtualMemory),
            0x0020 => Some(Self::NtReleaseMutant),
            0x0021 => Some(Self::NtQueryInformationToken),
            0x0023 => Some(Self::NtQueryVirtualMemory),
            0x0024 => Some(Self::NtOpenThreadToken),
            0x0025 => Some(Self::NtQueryInformationThread),
            0x0026 => Some(Self::NtOpenProcess),
            0x0027 => Some(Self::NtSetInformationFile),
            0x0028 => Some(Self::NtMapViewOfSection),
            0x002a => Some(Self::NtUnmapViewOfSection),
            0x002c => Some(Self::NtTerminateProcess),
            0x002f => Some(Self::NtOpenThreadTokenEx),
            0x0030 => Some(Self::NtOpenProcessTokenEx),
            0x0031 => Some(Self::NtQueryPerformanceCounter),
            0x0032 => Some(Self::NtEnumerateKey),
            0x0033 => Some(Self::NtOpenFile),
            0x0034 => Some(Self::NtDelayExecution),
            0x0035 => Some(Self::NtQueryDirectoryFile),
            0x0036 => Some(Self::NtQuerySystemInformation),
            0x0037 => Some(Self::NtOpenSection),
            0x0039 => Some(Self::NtFsControlFile),
            0x003a => Some(Self::NtWriteVirtualMemory),
            0x003c => Some(Self::NtDuplicateObject),
            0x003d => Some(Self::NtQueryAttributesFile),
            0x003e => Some(Self::NtClearEvent),
            0x003f => Some(Self::NtReadVirtualMemory),
            0x0040 => Some(Self::NtOpenEvent),
            0x0043 => Some(Self::NtContinue),
            0x0044 => Some(Self::NtQueryDefaultUILanguage),
            0x0046 => Some(Self::NtYieldExecution),
            0x0048 => Some(Self::NtCreateEvent),
            0x0049 => Some(Self::NtQueryVolumeInformationFile),
            0x004a => Some(Self::NtCreateSection),
            0x004b => Some(Self::NtFlushBuffersFile),
            0x004e => Some(Self::NtCreateThread),
            0x0050 => Some(Self::NtProtectVirtualMemory),
            0x0051 => Some(Self::NtQuerySection),
            0x0052 => Some(Self::NtResumeThread),
            0x0053 => Some(Self::NtTerminateThread),
            0x0055 => Some(Self::NtCreateFile),
            0x0056 => Some(Self::NtQueryEvent),
            0x0058 => Some(Self::NtOpenDirectoryObject),
            0x005a => Some(Self::NtQuerySystemTime),
            0x005b => Some(Self::NtWaitForMultipleObjects),
            0x005c => Some(Self::NtSetInformationObject),
            0x0060 => Some(Self::NtSetValueKey),
            0x0067 => Some(Self::NtAlertThreadByThreadId),
            0x0068 => Some(Self::NtAllocateLocallyUniqueId),
            0x006b => Some(Self::NtAllocateVirtualMemoryEx),
            0x0072 => Some(Self::NtAreMappedFilesTheSame),
            0x007b => Some(Self::NtContinueEx),
            0x007d => Some(Self::NtCreateDirectoryObject),
            0x0084 => Some(Self::NtCreateMutant),
            0x0085 => Some(Self::NtCreateNamedPipeFile),
            0x0088 => Some(Self::NtCreateSectionEx),
            0x0089 => Some(Self::NtCreateSemaphore),
            0x008b => Some(Self::NtCreateThreadEx),
            0x0094 => Some(Self::NtDeleteKey),
            0x0095 => Some(Self::NtDeleteValueKey),
            0x0099 => Some(Self::NtFlushInstructionCache),
            0x009a => Some(Self::NtFlushKey),
            0x009c => Some(Self::NtFlushVirtualMemory),
            0x009d => Some(Self::NtGetContextThread),
            0x009e => Some(Self::NtGetCurrentProcessorNumber),
            0x00a1 => Some(Self::NtGetNlsSectionPtr),
            0x00a4 => Some(Self::NtInitializeNlsFiles),
            0x00b0 => Some(Self::NtMapViewOfSectionEx),
            0x00b6 => Some(Self::NtOpenKeyEx),
            0x00b9 => Some(Self::NtOpenKeyedEvent),
            0x00ba => Some(Self::NtOpenMutant),
            0x00bc => Some(Self::NtOpenProcessToken),
            0x00bd => Some(Self::NtOpenSemaphore),
            0x00be => Some(Self::NtOpenSymbolicLinkObject),
            0x00bf => Some(Self::NtOpenThread),
            0x00c2 => Some(Self::NtPulseEvent),
            0x00c3 => Some(Self::NtQueryDirectoryObject),
            0x00c5 => Some(Self::NtQueryFullAttributesFile),
            0x00c8 => Some(Self::NtQueryInstallUILanguage),
            0x00cc => Some(Self::NtQueryMutant),
            0x00cd => Some(Self::NtQuerySecurityObject),
            0x00ce => Some(Self::NtQuerySemaphore),
            0x00cf => Some(Self::NtQuerySymbolicLinkObject),
            0x00d6 => Some(Self::NtRaiseException),
            0x00d9 => Some(Self::NtReleaseKeyedEvent),
            0x00de => Some(Self::NtResetEvent),
            0x00e5 => Some(Self::NtSetContextThread),
            0x00e7 => Some(Self::NtSetDefaultLocale),
            0x00e8 => Some(Self::NtSetDefaultUILanguage),
            0x00fc => Some(Self::NtSuspendThread),
            0x00ff => Some(Self::NtTestAlert),
            0x0105 => Some(Self::NtUnmapViewOfSectionEx),
            0x0106 => Some(Self::NtWaitForAlertByThreadId),
            0x0108 => Some(Self::NtWaitForKeyedEvent),
            _ => None,
        }
    }
}
// RING3-MIGRATION-REFERENCE END: syscalld/loaderd-owned Win32 syscall decode exception.
