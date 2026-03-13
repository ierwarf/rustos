use crate::paging;

pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_MMAP: u64 = 9;
pub const SYS_MPROTECT: u64 = 10;
pub const SYS_BRK: u64 = 12;
pub const SYS_RT_SIGPROCMASK: u64 = 14;
pub const SYS_IOCTL: u64 = 16;
pub const SYS_NANOSLEEP: u64 = 35;
pub const SYS_GETPID: u64 = 39;
pub const SYS_ARCH_PRCTL: u64 = 158;
pub const SYS_SET_TID_ADDRESS: u64 = 218;
pub const SYS_EXIT: u64 = 60;
pub const SYS_EXIT_GROUP: u64 = 231;

pub const ARCH_SET_FS: u64 = 0x1002;
pub const ARCH_GET_FS: u64 = 0x1003;

pub const AT_NULL: u64 = 0;
pub const AT_PHDR: u64 = 3;
pub const AT_PHENT: u64 = 4;
pub const AT_PHNUM: u64 = 5;
pub const AT_PAGESZ: u64 = 6;
pub const AT_BASE: u64 = 7;
pub const AT_FLAGS: u64 = 8;
pub const AT_ENTRY: u64 = 9;
pub const AT_UID: u64 = 11;
pub const AT_EUID: u64 = 12;
pub const AT_GID: u64 = 13;
pub const AT_EGID: u64 = 14;
pub const AT_CLKTCK: u64 = 17;
pub const AT_SECURE: u64 = 23;
pub const AT_RANDOM: u64 = 25;
pub const AT_HWCAP2: u64 = 26;
pub const AT_EXECFN: u64 = 31;

pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
pub const PROT_EXEC: u64 = 0x4;

pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_ANONYMOUS: u64 = 0x20;

const DEFAULT_MMAP_GAP: u64 = 16 * 1024 * 1024;
const MMAP_COLLISION_GUARD: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct LinuxProcessImageInfo {
    pub entry: u64,
    pub program_headers: u64,
    pub program_header_entry_size: u64,
    pub program_header_count: u64,
    pub brk_start: u64,
}

impl LinuxProcessImageInfo {
    pub fn initial_task_state(self) -> LinuxTaskState {
        LinuxTaskState {
            fs_base: 0,
            brk_start: self.brk_start,
            brk_current: self.brk_start,
            brk_mapped_end: self.brk_start,
            mmap_next: align_up(self.brk_start.saturating_add(DEFAULT_MMAP_GAP), 4096),
            clear_child_tid: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxTaskState {
    pub fs_base: u64,
    pub brk_start: u64,
    pub brk_current: u64,
    pub brk_mapped_end: u64,
    pub mmap_next: u64,
    pub clear_child_tid: u64,
}

impl LinuxTaskState {
    pub fn brk_limit(&self) -> u64 {
        paging::USER_SPACE_END_EXCLUSIVE.saturating_sub(MMAP_COLLISION_GUARD)
    }

    pub fn can_grow_brk_to(&self, requested_end: u64) -> bool {
        requested_end <= self.brk_limit() && requested_end <= self.mmap_next
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LinuxProcessLaunch<'a> {
    pub exec_path: &'a str,
    pub argv: &'a [&'a str],
    pub env: &'a [&'a str],
}

impl<'a> LinuxProcessLaunch<'a> {
    pub const fn new(exec_path: &'a str) -> Self {
        Self {
            exec_path,
            argv: &[],
            env: &[],
        }
    }
}

fn align_up(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value.saturating_add(align - 1) & !(align - 1)
}
