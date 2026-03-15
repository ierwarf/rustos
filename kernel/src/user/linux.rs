use crate::paging;
pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_CLOSE: u64 = 3;
pub const SYS_FSTAT: u64 = 5;
pub const SYS_LSEEK: u64 = 8;
pub const SYS_MMAP: u64 = 9;
pub const SYS_MPROTECT: u64 = 10;
pub const SYS_MUNMAP: u64 = 11;
pub const SYS_BRK: u64 = 12;
pub const SYS_RT_SIGPROCMASK: u64 = 14;
pub const SYS_IOCTL: u64 = 16;
pub const SYS_PREAD64: u64 = 17;
pub const SYS_WRITEV: u64 = 20;
pub const SYS_ACCESS: u64 = 21;
pub const SYS_NANOSLEEP: u64 = 35;
pub const SYS_GETPID: u64 = 39;
pub const SYS_READLINK: u64 = 89;
pub const SYS_ARCH_PRCTL: u64 = 158;
pub const SYS_SET_TID_ADDRESS: u64 = 218;
pub const SYS_CLOCK_GETTIME: u64 = 228;
pub const SYS_CLOCK_NANOSLEEP: u64 = 230;
pub const SYS_EXIT: u64 = 60;
pub const SYS_EXIT_GROUP: u64 = 231;
pub const SYS_OPENAT: u64 = 257;
pub const SYS_NEWFSTATAT: u64 = 262;
pub const SYS_READLINKAT: u64 = 267;
pub const SYS_FACCESSAT: u64 = 269;
pub const SYS_SET_ROBUST_LIST: u64 = 273;
pub const SYS_PRLIMIT64: u64 = 302;
pub const SYS_GETRANDOM: u64 = 318;
pub const SYS_RSEQ: u64 = 334;

pub const O_ACCMODE: u64 = 0o3;
pub const O_RDONLY: u64 = 0o0;
pub const O_WRONLY: u64 = 0o1;
pub const O_RDWR: u64 = 0o2;
pub const O_CREAT: u64 = 0o100;
pub const O_TRUNC: u64 = 0o1000;
pub const O_APPEND: u64 = 0o2000;
pub const O_CLOEXEC: u64 = 0o2000000;

pub const ARCH_SET_FS: u64 = 0x1002;
pub const ARCH_GET_FS: u64 = 0x1003;

pub const F_OK: u64 = 0;
pub const X_OK: u64 = 1;
pub const W_OK: u64 = 2;
pub const R_OK: u64 = 4;

pub const SEEK_SET: u64 = 0;
pub const SEEK_CUR: u64 = 1;
pub const SEEK_END: u64 = 2;

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
pub const AT_EMPTY_PATH: u64 = 0x1000;
pub const AT_EACCESS: u64 = 0x200;

pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
pub const PROT_EXEC: u64 = 0x4;

pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_SHARED: u64 = 0x01;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;

pub const AT_FDCWD: i32 = -100;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFREG: u32 = 0o100000;
pub const BOOT_FILE_MODE_BITS: u32 = S_IFREG | 0o444;
pub const DEVICE_FILE_MODE_BITS: u32 = S_IFCHR | 0o600;
pub const CLOCK_REALTIME: u64 = 0;
pub const CLOCK_MONOTONIC: u64 = 1;
pub const TIMER_ABSTIME: u64 = 0x1;
pub const RLIMIT_STACK: u64 = 3;

const DEFAULT_MMAP_GAP: u64 = 16 * 1024 * 1024;
const MMAP_COLLISION_GUARD: u64 = 64 * 1024 * 1024;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxTimespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxIovec {
    pub iov_base: u64,
    pub iov_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxStat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub __pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atim: LinuxTimespec,
    pub st_mtim: LinuxTimespec,
    pub st_ctim: LinuxTimespec,
    pub __glibc_reserved: [i64; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxRlimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct LinuxProcessImageInfo {
    pub entry: u64,
    pub interpreter_base: u64,
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
            robust_list_head: 0,
            robust_list_len: 0,
            rseq_area: 0,
            rseq_len: 0,
            rseq_signature: 0,
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
    pub robust_list_head: u64,
    pub robust_list_len: u64,
    pub rseq_area: u64,
    pub rseq_len: u32,
    pub rseq_signature: u32,
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
