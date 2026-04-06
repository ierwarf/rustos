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
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Api;

    #[test]
    fn rejects_removed_high_level_syscalls() {
        assert_eq!(Api::from_syscall_number(0x1001), None);
        assert_eq!(Api::from_syscall_number(0x1002), None);
        assert_eq!(Api::from_syscall_number(0x1010), None);
        assert_eq!(Api::from_syscall_number(0x1011), None);
        assert_eq!(Api::from_syscall_number(0x1012), None);
        assert_eq!(Api::from_syscall_number(0x1016), None);
        assert_eq!(Api::from_syscall_number(0x1017), None);
        assert_eq!(Api::from_syscall_number(0x1018), None);
        assert_eq!(Api::from_syscall_number(0x1019), None);
        assert_eq!(Api::from_syscall_number(0x1025), None);
        assert_eq!(Api::from_syscall_number(0x1048), None);
        assert_eq!(Api::from_syscall_number(0x1069), None);
        assert_eq!(Api::from_syscall_number(0x1033), None);
        assert_eq!(Api::from_syscall_number(0x1034), None);
        assert_eq!(Api::from_syscall_number(0x1035), None);
        assert_eq!(Api::from_syscall_number(0x1036), None);
        assert_eq!(Api::from_syscall_number(0x1038), None);
        assert_eq!(Api::from_syscall_number(0x1039), None);
        assert_eq!(Api::from_syscall_number(0x103a), None);
        assert_eq!(Api::from_syscall_number(0x103b), None);
        assert_eq!(Api::from_syscall_number(0x103d), None);
        assert_eq!(Api::from_syscall_number(0x103f), None);
        assert_eq!(Api::from_syscall_number(0x1041), None);
        assert_eq!(Api::from_syscall_number(0x1042), None);
        assert_eq!(Api::from_syscall_number(0x1043), None);
        assert_eq!(Api::from_syscall_number(0x1044), None);
        assert_eq!(Api::from_syscall_number(0x1045), None);
        assert_eq!(Api::from_syscall_number(0x1054), None);
        assert_eq!(Api::from_syscall_number(0x1056), None);
        assert_eq!(Api::from_syscall_number(0x1057), None);
        assert_eq!(Api::from_syscall_number(0x1060), None);
        assert_eq!(Api::from_syscall_number(0x1066), None);
        assert_eq!(Api::from_syscall_number(0x106a), None);
    }
}
