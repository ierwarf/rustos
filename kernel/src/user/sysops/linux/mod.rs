use core::convert::TryFrom;
use core::mem::size_of;
use core::slice;

use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::registers::model_specific::FsBase;
use x86_64::structures::paging::PageTableFlags;

use crate::arch::rtc;
use crate::debug;
use crate::input::event_queue;
use crate::io::{device as device_ns, tty};
use crate::memory::paging;
use crate::multitask;
use crate::user::abi::UserAbi;
use crate::user::handles::{
    ConsoleStreamKind, FD_CLOEXEC, FIRST_DYNAMIC_FD, FileHandleSeekWhence, HandleEntry,
    KernelHandle,
};
use crate::user::linux as linux_abi;

use super::console;
use super::device;
use super::file;
use super::stat;
use super::usermem;

mod fd;
mod fs;
mod mm;
mod process;
mod signal;
mod thread;
mod time;

pub(crate) use fd::*;
pub(crate) use fs::*;
pub(crate) use mm::*;
pub(crate) use process::*;
pub(crate) use signal::*;
pub(crate) use thread::*;
pub(crate) use time::*;

const PAGE_SIZE: u64 = 4096;
const LINUX_SIGSET_SIZE: u64 = 8;
const FILE_MMAP_COPY_CHUNK_LEN: usize = 4096;
const MAX_IOV_COUNT: usize = 256;
const DEFAULT_STACK_RLIMIT_BYTES: u64 = 8 * 1024 * 1024;
const GETRANDOM_FLAG_NONBLOCK: u64 = 0x0001;
const GETRANDOM_FLAG_RANDOM: u64 = 0x0002;
const RSEQ_FLAG_UNREGISTER: u64 = 0x1;
const FUTEX_WAITERS_CAPACITY: usize = 64;
const MAX_POLL_FDS: usize = 256;

static FUTEX_WAITERS: Mutex<[Option<FutexWaiter>; FUTEX_WAITERS_CAPACITY]> =
    Mutex::new([None; FUTEX_WAITERS_CAPACITY]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FutexKey {
    address_space_root: u64,
    uaddr: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FutexWaiter {
    key: FutexKey,
    task_id: u64,
    bitset: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxCloneFrame {
    pub(crate) user_rip: u64,
    pub(crate) user_rflags: u64,
    pub(crate) registers: multitask::UserTaskRegisters,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum LinuxSysopError {
    AddressSpace(paging::AddressSpaceError),
    BadFileDescriptor,
    Busy,
    DisplayUnavailable,
    IllegalSeek,
    InvalidArgument,
    NoMemory,
    NotFound,
    NotDirectory,
    NoSuchProcess,
    NotTty,
    PermissionDenied,
    ReadOnlyFilesystem,
    Stale,
    TryAgain,
    Unsupported,
}

impl From<paging::AddressSpaceError> for LinuxSysopError {
    fn from(value: paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

impl From<device::DeviceSysopError> for LinuxSysopError {
    fn from(value: device::DeviceSysopError) -> Self {
        match value {
            device::DeviceSysopError::AddressSpace(err) => Self::AddressSpace(err),
            device::DeviceSysopError::BadFileDescriptor => Self::BadFileDescriptor,
            device::DeviceSysopError::Busy => Self::Busy,
            device::DeviceSysopError::InvalidArgument => Self::InvalidArgument,
            device::DeviceSysopError::DisplayUnavailable => Self::DisplayUnavailable,
            device::DeviceSysopError::NotFound => Self::NotFound,
            device::DeviceSysopError::StaleSurface => Self::Stale,
            device::DeviceSysopError::Unsupported => Self::Unsupported,
        }
    }
}

impl From<file::FileSysopError> for LinuxSysopError {
    fn from(value: file::FileSysopError) -> Self {
        match value {
            file::FileSysopError::AddressSpace(err) => Self::AddressSpace(err),
            file::FileSysopError::BadFileDescriptor => Self::BadFileDescriptor,
            file::FileSysopError::InvalidArgument => Self::InvalidArgument,
            file::FileSysopError::NotFound => Self::NotFound,
            file::FileSysopError::NotDirectory => Self::NotDirectory,
            file::FileSysopError::PermissionDenied => Self::PermissionDenied,
            file::FileSysopError::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            file::FileSysopError::Unsupported => Self::Unsupported,
        }
    }
}

pub(super) const fn disabled_signal_stack() -> linux_abi::LinuxSignalStack {
    linux_abi::LinuxSignalStack {
        sp: 0,
        flags: linux_abi::SS_DISABLE,
        _pad: 0,
        size: 0,
    }
}
