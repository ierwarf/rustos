#![no_std]

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum LogLevel {
    Trace = 1,
    Debug = 2,
    Info = 3,
    Warn = 4,
    Error = 5,
    Fatal = 6,
}

impl LogLevel {
    pub const ALL: [Self; 6] = [
        Self::Trace,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Error,
        Self::Fatal,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Fatal => "fatal",
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LogCategory {
    Boot = 0,
    Panic = 1,
    Memory = 2,
    Sched = 3,
    Syscall = 4,
    Process = 5,
    Driver = 6,
    Storage = 7,
    Usb = 8,
    Input = 9,
    Display = 10,
    Vfs = 11,
    Console = 12,
    Service = 13,
    Compat = 14,
    Debug = 15,
    Heartbeat = 16,
}

impl LogCategory {
    pub const ALL: [Self; 17] = [
        Self::Boot,
        Self::Panic,
        Self::Memory,
        Self::Sched,
        Self::Syscall,
        Self::Process,
        Self::Driver,
        Self::Storage,
        Self::Usb,
        Self::Input,
        Self::Display,
        Self::Vfs,
        Self::Console,
        Self::Service,
        Self::Compat,
        Self::Debug,
        Self::Heartbeat,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::Panic => "panic",
            Self::Memory => "memory",
            Self::Sched => "sched",
            Self::Syscall => "syscall",
            Self::Process => "process",
            Self::Driver => "driver",
            Self::Storage => "storage",
            Self::Usb => "usb",
            Self::Input => "input",
            Self::Display => "display",
            Self::Vfs => "vfs",
            Self::Console => "console",
            Self::Service => "service",
            Self::Compat => "compat",
            Self::Debug => "debug",
            Self::Heartbeat => "heartbeat",
        }
    }
}
