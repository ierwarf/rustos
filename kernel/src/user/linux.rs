use crate::paging;
pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_CLOSE: u64 = 3;
pub const SYS_FSTAT: u64 = 5;
pub const SYS_POLL: u64 = 7;
pub const SYS_LSEEK: u64 = 8;
pub const SYS_MMAP: u64 = 9;
pub const SYS_MPROTECT: u64 = 10;
pub const SYS_MUNMAP: u64 = 11;
pub const SYS_BRK: u64 = 12;
pub const SYS_RT_SIGACTION: u64 = 13;
pub const SYS_RT_SIGPROCMASK: u64 = 14;
pub const SYS_IOCTL: u64 = 16;
pub const SYS_PREAD64: u64 = 17;
pub const SYS_WRITEV: u64 = 20;
pub const SYS_ACCESS: u64 = 21;
pub const SYS_SCHED_YIELD: u64 = 24;
pub const SYS_DUP: u64 = 32;
pub const SYS_DUP2: u64 = 33;
pub const SYS_NANOSLEEP: u64 = 35;
pub const SYS_GETPID: u64 = 39;
pub const SYS_CLONE: u64 = 56;
pub const SYS_UNAME: u64 = 63;
pub const SYS_FCNTL: u64 = 72;
pub const SYS_GETCWD: u64 = 79;
pub const SYS_READLINK: u64 = 89;
pub const SYS_GETUID: u64 = 102;
pub const SYS_GETGID: u64 = 104;
pub const SYS_GETEUID: u64 = 107;
pub const SYS_GETEGID: u64 = 108;
pub const SYS_SIGALTSTACK: u64 = 131;
pub const SYS_FUTEX: u64 = 202;
pub const SYS_ARCH_PRCTL: u64 = 158;
pub const SYS_GETTID: u64 = 186;
pub const SYS_SCHED_GETAFFINITY: u64 = 204;
pub const SYS_SET_TID_ADDRESS: u64 = 218;
pub const SYS_CLOCK_GETTIME: u64 = 228;
pub const SYS_CLOCK_NANOSLEEP: u64 = 230;
pub const SYS_EXIT: u64 = 60;
pub const SYS_EXIT_GROUP: u64 = 231;
pub const SYS_TGKILL: u64 = 234;
pub const SYS_OPENAT: u64 = 257;
pub const SYS_NEWFSTATAT: u64 = 262;
pub const SYS_READLINKAT: u64 = 267;
pub const SYS_FACCESSAT: u64 = 269;
pub const SYS_SET_ROBUST_LIST: u64 = 273;
pub const SYS_DUP3: u64 = 292;
pub const SYS_PRLIMIT64: u64 = 302;
pub const SYS_GETRANDOM: u64 = 318;
pub const SYS_STATX: u64 = 332;
pub const SYS_RSEQ: u64 = 334;
pub const SYS_CLONE3: u64 = 435;

pub const TCGETS: u64 = 0x5401;
pub const TCSETS: u64 = 0x5402;
pub const TCSETSW: u64 = 0x5403;
pub const TCSETSF: u64 = 0x5404;
pub const TIOCGWINSZ: u64 = 0x5413;
pub const FIONREAD: u64 = 0x541b;
pub const TIOCINQ: u64 = FIONREAD;

pub const O_ACCMODE: u64 = 0o3;
pub const O_RDONLY: u64 = 0o0;
pub const O_WRONLY: u64 = 0o1;
pub const O_RDWR: u64 = 0o2;
pub const O_CREAT: u64 = 0o100;
pub const O_NOCTTY: u64 = 0o400;
pub const O_TRUNC: u64 = 0o1000;
pub const O_NONBLOCK: u64 = 0o4000;
pub const O_APPEND: u64 = 0o2000;
pub const O_DIRECTORY: u64 = 0o200000;
pub const O_CLOEXEC: u64 = 0o2000000;

pub const NCCS: usize = 19;
pub const VINTR: usize = 0;
pub const VQUIT: usize = 1;
pub const VERASE: usize = 2;
pub const VKILL: usize = 3;
pub const VEOF: usize = 4;
pub const VTIME: usize = 5;
pub const VMIN: usize = 6;
pub const VSTART: usize = 8;
pub const VSTOP: usize = 9;
pub const VSUSP: usize = 10;
pub const VREPRINT: usize = 12;
pub const VWERASE: usize = 14;
pub const VLNEXT: usize = 15;
pub const IGNBRK: u32 = 0x001;
pub const BRKINT: u32 = 0x002;
pub const ICRNL: u32 = 0x100;
pub const IXON: u32 = 0x400;
pub const IUTF8: u32 = 0x4000;
pub const OPOST: u32 = 0x01;
pub const ONLCR: u32 = 0x00004;
pub const CS8: u32 = 0x00000030;
pub const CREAD: u32 = 0x00000080;
pub const HUPCL: u32 = 0x00000400;
pub const CLOCAL: u32 = 0x00000800;
pub const B38400: u32 = 0x0000000f;
pub const ISIG: u32 = 0x00001;
pub const ICANON: u32 = 0x00002;
pub const ECHO: u32 = 0x00008;
pub const ECHOE: u32 = 0x00010;
pub const ECHOK: u32 = 0x00020;
pub const ECHOCTL: u32 = 0x00200;
pub const ECHOKE: u32 = 0x00800;
pub const IEXTEN: u32 = 0x08000;

pub const F_DUPFD: u64 = 0;
pub const F_GETFD: u64 = 1;
pub const F_SETFD: u64 = 2;
pub const F_GETFL: u64 = 3;
pub const F_SETFL: u64 = 4;
pub const F_DUPFD_CLOEXEC: u64 = 1030;
pub const FD_CLOEXEC: u64 = 0x1;

pub const POLLIN: i16 = 0x0001;
pub const POLLPRI: i16 = 0x0002;
pub const POLLOUT: i16 = 0x0004;
pub const POLLERR: i16 = 0x0008;
pub const POLLHUP: i16 = 0x0010;
pub const POLLNVAL: i16 = 0x0020;

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
pub const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
pub const AT_NO_AUTOMOUNT: u64 = 0x800;
pub const AT_STATX_SYNC_TYPE: u64 = 0x6000;
pub const AT_STATX_SYNC_AS_STAT: u64 = 0x0000;
pub const AT_STATX_FORCE_SYNC: u64 = 0x2000;
pub const AT_STATX_DONT_SYNC: u64 = 0x4000;

pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
pub const PROT_EXEC: u64 = 0x4;

pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_SHARED: u64 = 0x01;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;

pub const AT_FDCWD: i32 = -100;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const BOOT_DIRECTORY_MODE_BITS: u32 = S_IFDIR | 0o555;
pub const BOOT_FILE_MODE_BITS: u32 = S_IFREG | 0o444;
pub const DEVICE_FILE_MODE_BITS: u32 = S_IFCHR | 0o600;
pub const STATX_TYPE: u32 = 0x0000_0001;
pub const STATX_MODE: u32 = 0x0000_0002;
pub const STATX_NLINK: u32 = 0x0000_0004;
pub const STATX_UID: u32 = 0x0000_0008;
pub const STATX_GID: u32 = 0x0000_0010;
pub const STATX_ATIME: u32 = 0x0000_0020;
pub const STATX_MTIME: u32 = 0x0000_0040;
pub const STATX_CTIME: u32 = 0x0000_0080;
pub const STATX_INO: u32 = 0x0000_0100;
pub const STATX_SIZE: u32 = 0x0000_0200;
pub const STATX_BLOCKS: u32 = 0x0000_0400;
pub const STATX_BASIC_STATS: u32 = 0x0000_07ff;
pub const STATX_BTIME: u32 = 0x0000_0800;
pub const STATX_MNT_ID: u32 = 0x0000_1000;
pub const CLOCK_REALTIME: u64 = 0;
pub const CLOCK_MONOTONIC: u64 = 1;
pub const TIMER_ABSTIME: u64 = 0x1;
pub const RLIMIT_STACK: u64 = 3;
pub const FUTEX_WAIT: u64 = 0;
pub const FUTEX_WAKE: u64 = 1;
pub const FUTEX_WAIT_BITSET: u64 = 9;
pub const FUTEX_WAKE_BITSET: u64 = 10;
pub const FUTEX_PRIVATE_FLAG: u64 = 128;
pub const FUTEX_CLOCK_REALTIME: u64 = 256;
pub const FUTEX_CMD_MASK: u64 = 0x7f;
pub const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;
pub const MAX_SIGNAL_NUMBER: usize = 64;
pub const SIGKILL: u64 = 9;
pub const SIGSTOP: u64 = 19;
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;
pub const SS_ONSTACK: u32 = 1;
pub const SS_DISABLE: u32 = 2;
pub const CLONE_VM: u64 = 0x0000_0100;
pub const CLONE_FS: u64 = 0x0000_0200;
pub const CLONE_FILES: u64 = 0x0000_0400;
pub const CLONE_SIGHAND: u64 = 0x0000_0800;
pub const CLONE_PIDFD: u64 = 0x0000_1000;
pub const CLONE_THREAD: u64 = 0x0001_0000;
pub const CLONE_SYSVSEM: u64 = 0x0004_0000;
pub const CLONE_SETTLS: u64 = 0x0008_0000;
pub const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
pub const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
pub const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
pub const CLONE_INTO_CGROUP: u64 = 0x0200_0000_00;
pub const CSIGNAL: u64 = 0x0000_00ff;

const DEFAULT_MMAP_GAP: u64 = 16 * 1024 * 1024;
const MMAP_COLLISION_GUARD: u64 = 64 * 1024 * 1024;
const MAX_RESERVED_MAPPINGS: usize = 64;

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
pub struct LinuxPollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxTermios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; NCCS],
}

impl LinuxTermios {
    pub const fn default_console() -> Self {
        Self {
            c_iflag: BRKINT | ICRNL | IXON | IUTF8,
            c_oflag: OPOST | ONLCR,
            c_cflag: B38400 | CS8 | CREAD | HUPCL,
            c_lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            c_line: 0,
            c_cc: [3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 18, 0, 23, 22, 0, 0, 0],
        }
    }

    pub const fn is_canonical(self) -> bool {
        self.c_lflag & ICANON != 0
    }

    pub const fn echo_enabled(self) -> bool {
        self.c_lflag & ECHO != 0
    }

    pub const fn echoes_control_chars(self) -> bool {
        self.c_lflag & ECHOCTL != 0
    }

    pub const fn erase_byte(self) -> u8 {
        self.c_cc[VERASE]
    }

    pub const fn maps_output_newline_to_crlf(self) -> bool {
        self.c_oflag & OPOST != 0 && self.c_oflag & ONLCR != 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxWinsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

impl LinuxWinsize {
    pub const fn default_console() -> Self {
        Self {
            ws_row: 25,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxSigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxSignalStack {
    pub sp: u64,
    pub flags: u32,
    pub _pad: u32,
    pub size: u64,
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
pub struct LinuxStatxTimestamp {
    pub tv_sec: i64,
    pub tv_nsec: u32,
    pub __reserved: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxStatx {
    pub stx_mask: u32,
    pub stx_blksize: u32,
    pub stx_attributes: u64,
    pub stx_nlink: u32,
    pub stx_uid: u32,
    pub stx_gid: u32,
    pub stx_mode: u16,
    pub __spare0: [u16; 1],
    pub stx_ino: u64,
    pub stx_size: u64,
    pub stx_blocks: u64,
    pub stx_attributes_mask: u64,
    pub stx_atime: LinuxStatxTimestamp,
    pub stx_btime: LinuxStatxTimestamp,
    pub stx_ctime: LinuxStatxTimestamp,
    pub stx_mtime: LinuxStatxTimestamp,
    pub stx_rdev_major: u32,
    pub stx_rdev_minor: u32,
    pub stx_dev_major: u32,
    pub stx_dev_minor: u32,
    pub stx_mnt_id: u64,
    pub stx_dio_mem_align: u32,
    pub stx_dio_offset_align: u32,
    pub __spare3: [u64; 12],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxRlimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxCloneArgs {
    pub flags: u64,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub exit_signal: u64,
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
    pub set_tid: u64,
    pub set_tid_size: u64,
    pub cgroup: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LinuxUtsName {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
    pub domainname: [u8; 65],
}

impl Default for LinuxUtsName {
    fn default() -> Self {
        Self {
            sysname: [0; 65],
            nodename: [0; 65],
            release: [0; 65],
            version: [0; 65],
            machine: [0; 65],
            domainname: [0; 65],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LinuxInitialTlsInfo {
    pub template_addr: u64,
    pub template_size: u64,
    pub mem_size: u64,
    pub align: u64,
    pub mapping_base: u64,
    pub mapping_size: u64,
    pub tls_block_base: u64,
    pub thread_pointer: u64,
    pub tcb_base: u64,
    pub dtv_base: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct LinuxProcessImageInfo {
    pub entry: u64,
    pub interpreter_base: u64,
    pub program_headers: u64,
    pub program_header_entry_size: u64,
    pub program_header_count: u64,
    pub brk_start: u64,
    pub initial_tls: Option<LinuxInitialTlsInfo>,
}

impl LinuxProcessImageInfo {
    pub fn initial_process_state(self) -> LinuxProcessState {
        LinuxProcessState {
            brk_start: self.brk_start,
            brk_current: self.brk_start,
            brk_mapped_end: self.brk_start,
            mmap_next: align_up(self.brk_start.saturating_add(DEFAULT_MMAP_GAP), 4096),
            reserved_mappings: [LinuxReservedMapping::EMPTY; MAX_RESERVED_MAPPINGS],
        }
    }

    pub fn initial_thread_state(self) -> LinuxThreadState {
        LinuxThreadState {
            fs_base: self.initial_tls.map(|tls| tls.thread_pointer).unwrap_or(0),
            clear_child_tid: 0,
            robust_list_head: 0,
            robust_list_len: 0,
            rseq_area: 0,
            rseq_len: 0,
            rseq_signature: 0,
            signal_stack: LinuxSignalStack {
                sp: 0,
                flags: SS_DISABLE,
                _pad: 0,
                size: 0,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxReservedMapping {
    pub start: u64,
    pub end: u64,
}

impl LinuxReservedMapping {
    pub const EMPTY: Self = Self { start: 0, end: 0 };

    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }

    pub const fn overlaps(self, start: u64, end: u64) -> bool {
        !self.is_empty() && self.start < end && start < self.end
    }

    pub const fn contains(self, start: u64, end: u64) -> bool {
        !self.is_empty() && self.start <= start && end <= self.end
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LinuxProcessState {
    pub brk_start: u64,
    pub brk_current: u64,
    pub brk_mapped_end: u64,
    pub mmap_next: u64,
    pub reserved_mappings: [LinuxReservedMapping; MAX_RESERVED_MAPPINGS],
}

impl Default for LinuxProcessState {
    fn default() -> Self {
        Self {
            brk_start: 0,
            brk_current: 0,
            brk_mapped_end: 0,
            mmap_next: 0,
            reserved_mappings: [LinuxReservedMapping::EMPTY; MAX_RESERVED_MAPPINGS],
        }
    }
}

impl LinuxProcessState {
    pub fn brk_limit(&self) -> u64 {
        paging::USER_SPACE_END_EXCLUSIVE.saturating_sub(MMAP_COLLISION_GUARD)
    }

    pub fn can_grow_brk_to(&self, requested_end: u64) -> bool {
        requested_end <= self.brk_limit() && requested_end <= self.mmap_next
    }

    pub fn has_reserved_overlap(&self, start: u64, end: u64) -> bool {
        self.reserved_mappings
            .iter()
            .copied()
            .any(|mapping| mapping.overlaps(start, end))
    }

    pub fn is_range_reserved(&self, start: u64, end: u64) -> bool {
        if start >= end {
            return false;
        }

        let mut cursor = start;
        while cursor < end {
            let mut next_end = cursor;
            for mapping in self.reserved_mappings.iter().copied() {
                if mapping.contains(cursor, cursor.saturating_add(1)) && mapping.end > next_end {
                    next_end = mapping.end;
                }
            }
            if next_end == cursor {
                return false;
            }
            cursor = next_end.min(end);
        }
        true
    }

    pub fn reserve_range(&mut self, start: u64, end: u64) -> Result<(), ()> {
        if start >= end || self.has_reserved_overlap(start, end) {
            return Err(());
        }

        let Some(slot) = self
            .reserved_mappings
            .iter_mut()
            .find(|mapping| mapping.is_empty())
        else {
            return Err(());
        };
        *slot = LinuxReservedMapping { start, end };
        Ok(())
    }

    pub fn release_reserved_range(&mut self, start: u64, end: u64) -> Result<(), ()> {
        if start >= end || !self.is_range_reserved(start, end) {
            return Err(());
        }

        for index in 0..self.reserved_mappings.len() {
            let mapping = self.reserved_mappings[index];
            if !mapping.overlaps(start, end) {
                continue;
            }

            if start <= mapping.start && end >= mapping.end {
                self.reserved_mappings[index] = LinuxReservedMapping::EMPTY;
                continue;
            }

            if start <= mapping.start {
                self.reserved_mappings[index].start = end.min(mapping.end);
                continue;
            }

            if end >= mapping.end {
                self.reserved_mappings[index].end = start.max(mapping.start);
                continue;
            }

            let Some(empty_index) = self
                .reserved_mappings
                .iter()
                .position(|reserved| reserved.is_empty())
            else {
                return Err(());
            };

            self.reserved_mappings[index].end = start;
            self.reserved_mappings[empty_index] = LinuxReservedMapping {
                start: end,
                end: mapping.end,
            };
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxThreadState {
    pub fs_base: u64,
    pub clear_child_tid: u64,
    pub robust_list_head: u64,
    pub robust_list_len: u64,
    pub rseq_area: u64,
    pub rseq_len: u32,
    pub rseq_signature: u32,
    pub signal_stack: LinuxSignalStack,
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

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::LinuxStatx;

    #[test]
    fn linux_statx_matches_uapi_size() {
        assert_eq!(size_of::<LinuxStatx>(), 0x100);
    }
}
