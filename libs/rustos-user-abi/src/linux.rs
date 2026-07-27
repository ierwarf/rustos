use alloc::string::{String, ToString};
use alloc::vec::Vec;
use linux_raw_sys::{
    auxvec as linux_auxvec, general as linux, ioctl as linux_ioctl, net as linux_net,
};

macro_rules! raw_u64 {
    ($($name:ident = $source:path;)+) => {
        $(pub const $name: u64 = $source as u64;)+
    };
}

macro_rules! raw_u32 {
    ($($name:ident = $source:path;)+) => {
        $(pub const $name: u32 = $source as u32;)+
    };
}

macro_rules! raw_i32 {
    ($($name:ident = $source:path;)+) => {
        $(pub const $name: i32 = $source as i32;)+
    };
}

macro_rules! raw_i16 {
    ($($name:ident = $source:path;)+) => {
        $(pub const $name: i16 = $source as i16;)+
    };
}

macro_rules! raw_usize {
    ($($name:ident = $source:path;)+) => {
        $(pub const $name: usize = $source as usize;)+
    };
}

raw_u64! {
    SYS_READ = linux::__NR_read;
    SYS_WRITE = linux::__NR_write;
    SYS_CLOSE = linux::__NR_close;
    SYS_UNLINK = linux::__NR_unlink;
    SYS_SOCKET = linux::__NR_socket;
    SYS_SENDTO = linux::__NR_sendto;
    SYS_ACCEPT = linux::__NR_accept;
    SYS_FTRUNCATE = linux::__NR_ftruncate;
    SYS_FSTAT = linux::__NR_fstat;
    SYS_POLL = linux::__NR_poll;
    SYS_PPOLL = linux::__NR_ppoll;
    SYS_EPOLL_WAIT = linux::__NR_epoll_wait;
    SYS_EPOLL_PWAIT = linux::__NR_epoll_pwait;
    SYS_EPOLL_CTL = linux::__NR_epoll_ctl;
    SYS_LSEEK = linux::__NR_lseek;
    SYS_MMAP = linux::__NR_mmap;
    SYS_MPROTECT = linux::__NR_mprotect;
    SYS_MUNMAP = linux::__NR_munmap;
    SYS_MADVISE = linux::__NR_madvise;
    SYS_BRK = linux::__NR_brk;
    SYS_RT_SIGACTION = linux::__NR_rt_sigaction;
    SYS_RT_SIGPROCMASK = linux::__NR_rt_sigprocmask;
    SYS_RT_SIGRETURN = linux::__NR_rt_sigreturn;
    SYS_IOCTL = linux::__NR_ioctl;
    SYS_PREAD64 = linux::__NR_pread64;
    SYS_WRITEV = linux::__NR_writev;
    SYS_ACCESS = linux::__NR_access;
    SYS_SCHED_YIELD = linux::__NR_sched_yield;
    SYS_DUP = linux::__NR_dup;
    SYS_DUP2 = linux::__NR_dup2;
    SYS_NANOSLEEP = linux::__NR_nanosleep;
    SYS_GETPID = linux::__NR_getpid;
    SYS_FORK = linux::__NR_fork;
    SYS_WAIT4 = linux::__NR_wait4;
    SYS_CLONE = linux::__NR_clone;
    SYS_UNAME = linux::__NR_uname;
    SYS_FCNTL = linux::__NR_fcntl;
    SYS_MOUNT = linux::__NR_mount;
    SYS_GETCWD = linux::__NR_getcwd;
    SYS_MKDIR = linux::__NR_mkdir;
    SYS_CHDIR = linux::__NR_chdir;
    SYS_READLINK = linux::__NR_readlink;
    SYS_GETUID = linux::__NR_getuid;
    SYS_GETGID = linux::__NR_getgid;
    SYS_GETEUID = linux::__NR_geteuid;
    SYS_GETEGID = linux::__NR_getegid;
    SYS_SETUID = linux::__NR_setuid;
    SYS_SETGID = linux::__NR_setgid;
    SYS_SIGALTSTACK = linux::__NR_sigaltstack;
    SYS_ARCH_PRCTL = linux::__NR_arch_prctl;
    SYS_GETTID = linux::__NR_gettid;
    SYS_FUTEX = linux::__NR_futex;
    SYS_EXECVE = linux::__NR_execve;
    SYS_SCHED_GETAFFINITY = linux::__NR_sched_getaffinity;
    SYS_SET_TID_ADDRESS = linux::__NR_set_tid_address;
    SYS_CLOCK_GETTIME = linux::__NR_clock_gettime;
    SYS_CLOCK_NANOSLEEP = linux::__NR_clock_nanosleep;
    SYS_EXIT = linux::__NR_exit;
    SYS_EXIT_GROUP = linux::__NR_exit_group;
    SYS_TGKILL = linux::__NR_tgkill;
    SYS_OPENAT = linux::__NR_openat;
    SYS_UNLINKAT = linux::__NR_unlinkat;
    SYS_SOCKETPAIR = linux::__NR_socketpair;
    SYS_BIND = linux::__NR_bind;
    SYS_CONNECT = linux::__NR_connect;
    SYS_LISTEN = linux::__NR_listen;
    SYS_ACCEPT4 = linux::__NR_accept4;
    SYS_GETSOCKNAME = linux::__NR_getsockname;
    SYS_GETPEERNAME = linux::__NR_getpeername;
    SYS_SETSOCKOPT = linux::__NR_setsockopt;
    SYS_GETSOCKOPT = linux::__NR_getsockopt;
    SYS_SHUTDOWN = linux::__NR_shutdown;
    SYS_SENDMSG = linux::__NR_sendmsg;
    SYS_RECVFROM = linux::__NR_recvfrom;
    SYS_RECVMSG = linux::__NR_recvmsg;
    SYS_GETDENTS64 = linux::__NR_getdents64;
    SYS_EXECVEAT = linux::__NR_execveat;
    SYS_NEWFSTATAT = linux::__NR_newfstatat;
    SYS_UMOUNT2 = linux::__NR_umount2;
    SYS_READLINKAT = linux::__NR_readlinkat;
    SYS_FACCESSAT = linux::__NR_faccessat;
    SYS_SET_ROBUST_LIST = linux::__NR_set_robust_list;
    SYS_GET_ROBUST_LIST = linux::__NR_get_robust_list;
    SYS_DUP3 = linux::__NR_dup3;
    SYS_EPOLL_CREATE1 = linux::__NR_epoll_create1;
    SYS_PRLIMIT64 = linux::__NR_prlimit64;
    SYS_GETRANDOM = linux::__NR_getrandom;
    SYS_STATX = linux::__NR_statx;
    SYS_RSEQ = linux::__NR_rseq;
    SYS_CLONE3 = linux::__NR_clone3;
    SYS_MEMFD_CREATE = linux::__NR_memfd_create;
    SYS_UMASK = linux::__NR_umask;
    SYS_KILL = linux::__NR_kill;
    SYS_TKILL = linux::__NR_tkill;
    SYS_GETPPID = linux::__NR_getppid;
    SYS_GETPGID = linux::__NR_getpgid;
    SYS_SETPGID = linux::__NR_setpgid;
    SYS_GETSID = linux::__NR_getsid;
    SYS_SETSID = linux::__NR_setsid;
    TCGETS = linux_ioctl::TCGETS;
    TCSETS = linux_ioctl::TCSETS;
    TCSETSW = linux_ioctl::TCSETSW;
    TCSETSF = linux_ioctl::TCSETSF;
    TIOCGWINSZ = linux_ioctl::TIOCGWINSZ;
    FIONREAD = linux_ioctl::FIONREAD;
    FIONBIO = linux_ioctl::FIONBIO;
    O_ACCMODE = linux::O_ACCMODE;
    O_RDONLY = linux::O_RDONLY;
    O_WRONLY = linux::O_WRONLY;
    O_RDWR = linux::O_RDWR;
    MS_RDONLY = linux::MS_RDONLY;
    O_CREAT = linux::O_CREAT;
    O_NOCTTY = linux::O_NOCTTY;
    O_TRUNC = linux::O_TRUNC;
    O_NONBLOCK = linux::O_NONBLOCK;
    O_APPEND = linux::O_APPEND;
    O_DIRECTORY = linux::O_DIRECTORY;
    O_CLOEXEC = linux::O_CLOEXEC;
    AF_UNIX = linux_net::AF_UNIX;
    AF_INET = linux_net::AF_INET;
    SOCK_STREAM = linux_net::SOCK_STREAM;
    SOCK_DGRAM = linux_net::SOCK_DGRAM;
    SOCK_NONBLOCK = linux::O_NONBLOCK;
    SOCK_CLOEXEC = linux::O_CLOEXEC;
    MSG_DONTWAIT = linux_net::MSG_DONTWAIT;
    MSG_NOSIGNAL = linux_net::MSG_NOSIGNAL;
    MSG_CMSG_CLOEXEC = linux_net::MSG_CMSG_CLOEXEC;
    MSG_CTRUNC = linux_net::MSG_CTRUNC;
    SHUT_RD = linux_net::SHUT_RD;
    SHUT_WR = linux_net::SHUT_WR;
    SHUT_RDWR = linux_net::SHUT_RDWR;
    SOL_SOCKET = linux_net::SOL_SOCKET;
    SO_REUSEADDR = linux_net::SO_REUSEADDR;
    SO_KEEPALIVE = linux_net::SO_KEEPALIVE;
    SO_ERROR = linux_net::SO_ERROR;
    SO_TYPE = linux_net::SO_TYPE;
    SO_SNDBUF = linux_net::SO_SNDBUF;
    SO_RCVBUF = linux_net::SO_RCVBUF;
    SO_PASSCRED = linux_net::SO_PASSCRED;
    SO_PEERCRED = linux_net::SO_PEERCRED;
    SO_ACCEPTCONN = linux_net::SO_ACCEPTCONN;
    SO_REUSEPORT = linux_net::SO_REUSEPORT;
    SO_DOMAIN = linux_net::SO_DOMAIN;
    SO_PROTOCOL = linux_net::SO_PROTOCOL;
    SCM_RIGHTS = linux_net::SCM_RIGHTS;
    F_DUPFD = linux::F_DUPFD;
    F_GETFD = linux::F_GETFD;
    F_SETFD = linux::F_SETFD;
    F_GETFL = linux::F_GETFL;
    F_SETFL = linux::F_SETFL;
    F_DUPFD_CLOEXEC = linux::F_DUPFD_CLOEXEC;
    F_ADD_SEALS = linux::F_ADD_SEALS;
    F_GET_SEALS = linux::F_GET_SEALS;
    F_SEAL_SEAL = linux::F_SEAL_SEAL;
    F_SEAL_SHRINK = linux::F_SEAL_SHRINK;
    F_SEAL_GROW = linux::F_SEAL_GROW;
    F_SEAL_WRITE = linux::F_SEAL_WRITE;
    ARCH_SET_FS = linux::ARCH_SET_FS;
    F_OK = linux::F_OK;
    X_OK = linux::X_OK;
    W_OK = linux::W_OK;
    R_OK = linux::R_OK;
    SEEK_SET = linux::SEEK_SET;
    SEEK_CUR = linux::SEEK_CUR;
    SEEK_END = linux::SEEK_END;
    AT_NULL = linux_auxvec::AT_NULL;
    AT_PHDR = linux_auxvec::AT_PHDR;
    AT_PHENT = linux_auxvec::AT_PHENT;
    AT_PHNUM = linux_auxvec::AT_PHNUM;
    AT_PAGESZ = linux_auxvec::AT_PAGESZ;
    AT_BASE = linux_auxvec::AT_BASE;
    AT_FLAGS = linux_auxvec::AT_FLAGS;
    AT_ENTRY = linux_auxvec::AT_ENTRY;
    AT_UID = linux_auxvec::AT_UID;
    AT_EUID = linux_auxvec::AT_EUID;
    AT_GID = linux_auxvec::AT_GID;
    AT_EGID = linux_auxvec::AT_EGID;
    AT_CLKTCK = linux_auxvec::AT_CLKTCK;
    AT_PLATFORM = linux_auxvec::AT_PLATFORM;
    AT_HWCAP = linux_auxvec::AT_HWCAP;
    AT_SECURE = linux_auxvec::AT_SECURE;
    AT_RANDOM = linux_auxvec::AT_RANDOM;
    AT_HWCAP2 = linux_auxvec::AT_HWCAP2;
    AT_EXECFN = linux_auxvec::AT_EXECFN;
    AT_EMPTY_PATH = linux::AT_EMPTY_PATH;
    AT_EACCESS = linux::AT_EACCESS;
    AT_SYMLINK_NOFOLLOW = linux::AT_SYMLINK_NOFOLLOW;
    AT_NO_AUTOMOUNT = linux::AT_NO_AUTOMOUNT;
    AT_STATX_SYNC_TYPE = linux::AT_STATX_SYNC_TYPE;
    AT_STATX_SYNC_AS_STAT = linux::AT_STATX_SYNC_AS_STAT;
    AT_STATX_FORCE_SYNC = linux::AT_STATX_FORCE_SYNC;
    AT_STATX_DONT_SYNC = linux::AT_STATX_DONT_SYNC;
    PROT_READ = linux::PROT_READ;
    PROT_WRITE = linux::PROT_WRITE;
    PROT_EXEC = linux::PROT_EXEC;
    MAP_TYPE = linux::MAP_TYPE;
    MAP_PRIVATE = linux::MAP_PRIVATE;
    MAP_SHARED = linux::MAP_SHARED;
    MAP_SHARED_VALIDATE = linux::MAP_SHARED_VALIDATE;
    MAP_FIXED = linux::MAP_FIXED;
    MAP_ANONYMOUS = linux::MAP_ANONYMOUS;
    MAP_DENYWRITE = linux::MAP_DENYWRITE;
    MAP_EXECUTABLE = linux::MAP_EXECUTABLE;
    CLOCK_REALTIME = linux::CLOCK_REALTIME;
    CLOCK_MONOTONIC = linux::CLOCK_MONOTONIC;
    TIMER_ABSTIME = linux::TIMER_ABSTIME;
    RLIMIT_STACK = linux::RLIMIT_STACK;
    FUTEX_WAIT = linux::FUTEX_WAIT;
    FUTEX_WAKE = linux::FUTEX_WAKE;
    FUTEX_WAIT_BITSET = linux::FUTEX_WAIT_BITSET;
    FUTEX_WAKE_BITSET = linux::FUTEX_WAKE_BITSET;
    FUTEX_REQUEUE = linux::FUTEX_REQUEUE;
    FUTEX_CMP_REQUEUE = linux::FUTEX_CMP_REQUEUE;
    FUTEX_PRIVATE_FLAG = linux::FUTEX_PRIVATE_FLAG;
    FUTEX_CLOCK_REALTIME = linux::FUTEX_CLOCK_REALTIME;
    FUTEX_CMD_MASK = linux::FUTEX_CMD_MASK;
    SIGKILL = linux::SIGKILL;
    SIGCHLD = linux::SIGCHLD;
    SIGSTOP = linux::SIGSTOP;
    SIG_BLOCK = linux::SIG_BLOCK;
    SIG_UNBLOCK = linux::SIG_UNBLOCK;
    SIG_SETMASK = linux::SIG_SETMASK;
    CLONE_VM = linux::CLONE_VM;
    CLONE_FS = linux::CLONE_FS;
    CLONE_FILES = linux::CLONE_FILES;
    CLONE_SIGHAND = linux::CLONE_SIGHAND;
    CLONE_PIDFD = linux::CLONE_PIDFD;
    CLONE_THREAD = linux::CLONE_THREAD;
    CLONE_SYSVSEM = linux::CLONE_SYSVSEM;
    CLONE_SETTLS = linux::CLONE_SETTLS;
    CLONE_PARENT_SETTID = linux::CLONE_PARENT_SETTID;
    CLONE_CHILD_CLEARTID = linux::CLONE_CHILD_CLEARTID;
    CLONE_CHILD_SETTID = linux::CLONE_CHILD_SETTID;
    CLONE_INTO_CGROUP = linux::CLONE_INTO_CGROUP;
    CSIGNAL = linux::CSIGNAL;
    DT_UNKNOWN = linux::DT_UNKNOWN;
    DT_CHR = linux::DT_CHR;
    DT_DIR = linux::DT_DIR;
    DT_REG = linux::DT_REG;
    MFD_CLOEXEC = linux::MFD_CLOEXEC;
    MFD_ALLOW_SEALING = linux::MFD_ALLOW_SEALING;
}

pub const SCM_CREDENTIALS: u64 = 2;
pub const MADV_NORMAL: u64 = 0;
pub const MADV_RANDOM: u64 = 1;
pub const MADV_SEQUENTIAL: u64 = 2;
pub const MADV_WILLNEED: u64 = 3;
pub const MADV_DONTNEED: u64 = 4;
pub const MADV_FREE: u64 = 8;
pub const MADV_REMOVE: u64 = 9;
pub const MADV_DONTFORK: u64 = 10;
pub const MADV_DOFORK: u64 = 11;
pub const MADV_MERGEABLE: u64 = 12;
pub const MADV_UNMERGEABLE: u64 = 13;
pub const MADV_HUGEPAGE: u64 = 14;
pub const MADV_NOHUGEPAGE: u64 = 15;
pub const MADV_DONTDUMP: u64 = 16;
pub const MADV_DODUMP: u64 = 17;
pub const MADV_WIPEONFORK: u64 = 18;
pub const MADV_KEEPONFORK: u64 = 19;
pub const MADV_COLD: u64 = 20;
pub const MADV_PAGEOUT: u64 = 21;
pub const MADV_POPULATE_READ: u64 = 22;
pub const MADV_POPULATE_WRITE: u64 = 23;
pub const MADV_DONTNEED_LOCKED: u64 = 24;

raw_u32! {
    IGNBRK = linux::IGNBRK;
    BRKINT = linux::BRKINT;
    ICRNL = linux::ICRNL;
    IXON = linux::IXON;
    IUTF8 = linux::IUTF8;
    OPOST = linux::OPOST;
    ONLCR = linux::ONLCR;
    CS8 = linux::CS8;
    CREAD = linux::CREAD;
    HUPCL = linux::HUPCL;
    CLOCAL = linux::CLOCAL;
    B38400 = linux::B38400;
    ISIG = linux::ISIG;
    ICANON = linux::ICANON;
    ECHO = linux::ECHO;
    ECHOE = linux::ECHOE;
    ECHOK = linux::ECHOK;
    ECHOCTL = linux::ECHOCTL;
    ECHOKE = linux::ECHOKE;
    IEXTEN = linux::IEXTEN;
    FD_CLOEXEC = linux::FD_CLOEXEC;
    FUTEX_BITSET_MATCH_ANY = linux::FUTEX_BITSET_MATCH_ANY;
    SS_ONSTACK = linux::SS_ONSTACK;
    SS_DISABLE = linux::SS_DISABLE;
    S_IFCHR = linux::S_IFCHR;
    S_IFDIR = linux::S_IFDIR;
    S_IFREG = linux::S_IFREG;
    STATX_TYPE = linux::STATX_TYPE;
    STATX_MODE = linux::STATX_MODE;
    STATX_NLINK = linux::STATX_NLINK;
    STATX_UID = linux::STATX_UID;
    STATX_GID = linux::STATX_GID;
    STATX_ATIME = linux::STATX_ATIME;
    STATX_MTIME = linux::STATX_MTIME;
    STATX_CTIME = linux::STATX_CTIME;
    STATX_INO = linux::STATX_INO;
    STATX_SIZE = linux::STATX_SIZE;
    STATX_BLOCKS = linux::STATX_BLOCKS;
    STATX_BASIC_STATS = linux::STATX_BASIC_STATS;
    STATX_BTIME = linux::STATX_BTIME;
    STATX_MNT_ID = linux::STATX_MNT_ID;
}

raw_i32! {
    AT_FDCWD = linux::AT_FDCWD;
    WNOHANG = linux::WNOHANG;
}

raw_i16! {
    POLLIN = linux::POLLIN;
    POLLPRI = linux::POLLPRI;
    POLLOUT = linux::POLLOUT;
    POLLERR = linux::POLLERR;
    POLLHUP = linux::POLLHUP;
    POLLNVAL = linux::POLLNVAL;
}

pub const EPOLL_CLOEXEC: u64 = O_CLOEXEC;
pub const EPOLL_CTL_ADD: u64 = 1;
pub const EPOLL_CTL_DEL: u64 = 2;
pub const EPOLL_CTL_MOD: u64 = 3;
pub const EPOLLIN: u32 = POLLIN as u32;
pub const EPOLLPRI: u32 = POLLPRI as u32;
pub const EPOLLOUT: u32 = POLLOUT as u32;
pub const EPOLLERR: u32 = POLLERR as u32;
pub const EPOLLHUP: u32 = POLLHUP as u32;
pub const EPOLLNVAL: u32 = POLLNVAL as u32;

raw_usize! {
    NCCS = linux::NCCS;
    VINTR = linux::VINTR;
    VQUIT = linux::VQUIT;
    VERASE = linux::VERASE;
    VKILL = linux::VKILL;
    VEOF = linux::VEOF;
    VTIME = linux::VTIME;
    VMIN = linux::VMIN;
    VSTART = linux::VSTART;
    VSTOP = linux::VSTOP;
    VSUSP = linux::VSUSP;
    VREPRINT = linux::VREPRINT;
    VWERASE = linux::VWERASE;
    VLNEXT = linux::VLNEXT;
}

pub const TIOCINQ: u64 = FIONREAD;
pub const ARCH_GET_FS: u64 = 0x1003;
pub const BOOT_DIRECTORY_MODE_BITS: u32 = S_IFDIR | 0o555;
pub const BOOT_FILE_MODE_BITS: u32 = S_IFREG | 0o444;
pub const DEVICE_FILE_MODE_BITS: u32 = S_IFCHR | 0o600;
pub const MAX_SIGNAL_NUMBER: usize = 64;
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;
pub const SOCK_TYPE_MASK: u64 = 0xf;
pub const UNIX_PATH_MAX: usize = 108;

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
pub struct LinuxTimeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxIovec {
    pub iov_base: u64,
    pub iov_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LinuxSockaddrUn {
    pub sun_family: u16,
    pub sun_path: [u8; UNIX_PATH_MAX],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxSockaddrIn {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: u32,
    pub sin_zero: [u8; 8],
}

impl Default for LinuxSockaddrUn {
    fn default() -> Self {
        Self {
            sun_family: 0,
            sun_path: [0; UNIX_PATH_MAX],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxUCred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxMsghdr {
    pub msg_name: u64,
    pub msg_namelen: u32,
    pub __pad0: u32,
    pub msg_iov: u64,
    pub msg_iovlen: u64,
    pub msg_control: u64,
    pub msg_controllen: u64,
    pub msg_flags: u32,
    pub __pad1: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxCmsghdr {
    pub cmsg_len: u64,
    pub cmsg_level: u32,
    pub cmsg_type: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxPollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxEpollEvent {
    pub events: u32,
    pub data: u64,
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
            c_cc: [
                3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 18, 0, 23, 22, 0, 0, 0,
            ],
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
pub struct LinuxRusage {
    pub ru_utime: LinuxTimeval,
    pub ru_stime: LinuxTimeval,
    pub ru_maxrss: i64,
    pub ru_ixrss: i64,
    pub ru_idrss: i64,
    pub ru_isrss: i64,
    pub ru_minflt: i64,
    pub ru_majflt: i64,
    pub ru_nswap: i64,
    pub ru_inblock: i64,
    pub ru_oublock: i64,
    pub ru_msgsnd: i64,
    pub ru_msgrcv: i64,
    pub ru_nsignals: i64,
    pub ru_nvcsw: i64,
    pub ru_nivcsw: i64,
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct LinuxVmaFlags {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
    pub private: bool,
}

impl LinuxVmaFlags {
    pub const fn new(read: bool, write: bool, execute: bool, private: bool) -> Self {
        Self {
            read,
            write,
            execute,
            private,
        }
    }

    pub const fn private_anon(read: bool, write: bool, execute: bool) -> Self {
        Self::new(read, write, execute, true)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LinuxImageMappingPathKind {
    None,
    Executable,
    Interpreter,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct LinuxImageMapping {
    pub start: u64,
    pub end: u64,
    pub offset: u64,
    pub flags: LinuxVmaFlags,
    pub path_kind: LinuxImageMappingPathKind,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum LinuxVmaName {
    None,
    Path(String),
    Label(&'static str),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LinuxVma {
    pub start: u64,
    pub end: u64,
    pub offset: u64,
    pub flags: LinuxVmaFlags,
    pub name: LinuxVmaName,
}

impl LinuxVma {
    pub fn new(
        start: u64,
        end: u64,
        offset: u64,
        flags: LinuxVmaFlags,
        name: LinuxVmaName,
    ) -> Option<Self> {
        if start >= end {
            return None;
        }

        Some(Self {
            start,
            end,
            offset,
            flags,
            name,
        })
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start < end && start < self.end
    }

    fn contains_range(&self, start: u64, end: u64) -> bool {
        self.start <= start && end <= self.end
    }

    fn subrange(&self, start: u64, end: u64) -> Option<Self> {
        if !self.contains_range(start, end) || start >= end {
            return None;
        }

        Some(Self {
            start,
            end,
            offset: self.offset.checked_add(start.saturating_sub(self.start))?,
            flags: self.flags,
            name: self.name.clone(),
        })
    }

    fn can_merge_with(&self, next: &Self) -> bool {
        self.end == next.start
            && self.flags == next.flags
            && self.name == next.name
            && self.offset.checked_add(self.end.saturating_sub(self.start)) == Some(next.offset)
    }
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct LinuxMemoryMapState {
    areas: Vec<LinuxVma>,
}

/// Failure category for in-memory Linux VMA bookkeeping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxMemoryMapError {
    InvalidRange,
    Overlap,
    Unmapped,
    Capacity,
}

impl LinuxMemoryMapState {
    pub fn new() -> Self {
        Self { areas: Vec::new() }
    }

    pub fn areas(&self) -> &[LinuxVma] {
        &self.areas
    }

    pub fn area_covering_range(&self, start: u64, end: u64) -> Option<&LinuxVma> {
        self.areas
            .iter()
            .find(|area| area.contains_range(start, end))
    }

    pub fn insert_area(&mut self, area: LinuxVma) -> Result<(), LinuxMemoryMapError> {
        if self
            .areas
            .iter()
            .any(|existing| existing.overlaps(area.start, area.end))
        {
            return Err(LinuxMemoryMapError::Overlap);
        }

        self.areas.push(area);
        self.normalize();
        Ok(())
    }

    pub fn replace_area(&mut self, area: LinuxVma) {
        self.unmap_range(area.start, area.end);
        self.areas.push(area);
        self.normalize();
    }

    pub fn unmap_range(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }

        let mut updated = Vec::with_capacity(self.areas.len() + 1);
        for area in self.areas.drain(..) {
            if !area.overlaps(start, end) {
                updated.push(area);
                continue;
            }

            if start > area.start
                && let Some(left) = area.subrange(area.start, start.min(area.end))
            {
                updated.push(left);
            }
            if end < area.end
                && let Some(right) = area.subrange(end.max(area.start), area.end)
            {
                updated.push(right);
            }
        }

        self.areas = updated;
        self.normalize();
    }

    pub fn protect_range(
        &mut self,
        start: u64,
        end: u64,
        flags: LinuxVmaFlags,
    ) -> Result<(), LinuxMemoryMapError> {
        if start >= end {
            return Ok(());
        }

        let original = self.areas.clone();
        let mut cursor = start;
        let mut updated = Vec::with_capacity(original.len() + 2);
        let mut covered = false;

        for area in original.iter().cloned() {
            if !area.overlaps(start, end) {
                updated.push(area);
                continue;
            }

            let overlap_start = area.start.max(start);
            let overlap_end = area.end.min(end);
            if cursor < overlap_start {
                self.areas = original;
                return Err(LinuxMemoryMapError::Unmapped);
            }

            if area.start < overlap_start
                && let Some(left) = area.subrange(area.start, overlap_start)
            {
                updated.push(left);
            }

            let mut middle = area
                .subrange(overlap_start, overlap_end)
                .ok_or(LinuxMemoryMapError::Unmapped)?;
            middle.flags =
                LinuxVmaFlags::new(flags.read, flags.write, flags.execute, area.flags.private);
            updated.push(middle);
            covered = true;
            cursor = overlap_end;

            if overlap_end < area.end
                && let Some(right) = area.subrange(overlap_end, area.end)
            {
                updated.push(right);
            }
        }

        if !covered || cursor < end {
            self.areas = original;
            return Err(LinuxMemoryMapError::Unmapped);
        }

        self.areas = updated;
        self.normalize();
        Ok(())
    }

    pub fn set_heap_range(&mut self, start: u64, end: u64) {
        self.areas
            .retain(|area| area.name != LinuxVmaName::Label("[heap]"));
        if let Some(area) = LinuxVma::new(
            start,
            end,
            0,
            LinuxVmaFlags::private_anon(true, true, false),
            LinuxVmaName::Label("[heap]"),
        ) {
            self.areas.push(area);
            self.normalize();
        }
    }

    fn normalize(&mut self) {
        self.areas.sort_by_key(|area| area.start);
        let mut merged: Vec<LinuxVma> = Vec::with_capacity(self.areas.len());
        for area in self.areas.drain(..) {
            if let Some(previous) = merged.last_mut()
                && previous.can_merge_with(&area)
            {
                previous.end = area.end;
                continue;
            }
            merged.push(area);
        }
        self.areas = merged;
    }
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct LinuxRuntimeProfile {
    loader_search_dirs: Vec<String>,
    kernel_runtime_access_dirs: Vec<String>,
}

impl LinuxRuntimeProfile {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn loader_search_dirs(&self) -> &[String] {
        &self.loader_search_dirs
    }

    pub fn kernel_runtime_access_dirs(&self) -> &[String] {
        &self.kernel_runtime_access_dirs
    }

    pub fn allow_loader_search_dir(&mut self, path: &str) {
        let Some(path) = normalize_runtime_search_dir(path) else {
            return;
        };
        push_unique_runtime_dir(&mut self.loader_search_dirs, path.as_str());
    }

    pub fn allow_kernel_runtime_access_dir(&mut self, path: &str) {
        let Some(path) = normalize_runtime_search_dir(path) else {
            return;
        };
        push_unique_runtime_dir(&mut self.kernel_runtime_access_dirs, path.as_str());
    }
}

#[derive(Debug, Clone)]
pub struct LinuxProcessImageInfo {
    pub entry: u64,
    pub interpreter_base: u64,
    pub interpreter_path: Option<String>,
    pub program_headers: u64,
    pub program_header_entry_size: u64,
    pub program_header_count: u64,
    pub brk_start: u64,
    /// Base of the seL4-style bootstrap heap pre-mapped by the kernel for
    /// static-PIE policy services. Zero when no such heap was reserved
    /// (i.e. the process is a normal dynamic Linux ELF).
    pub bootstrap_heap_base: u64,
    /// Length of the bootstrap heap in bytes. Zero when unused.
    pub bootstrap_heap_len: u64,
    pub initial_tls: Option<LinuxInitialTlsInfo>,
    pub image_mappings: Vec<LinuxImageMapping>,
    pub runtime_search_paths: Vec<String>,
}

impl LinuxProcessImageInfo {
    pub fn initial_process_state(&self) -> LinuxProcessState {
        let bootstrap_end = self
            .bootstrap_heap_base
            .saturating_add(self.bootstrap_heap_len);
        let default_mmap_next = align_up(self.brk_start.saturating_add(DEFAULT_MMAP_GAP), 4096);
        let mmap_next = if bootstrap_end > default_mmap_next {
            align_up(bootstrap_end, 4096)
        } else {
            default_mmap_next
        };
        LinuxProcessState {
            brk_start: self.brk_start,
            brk_current: self.brk_start,
            brk_mapped_end: self.brk_start,
            mmap_next,
            reserved_mappings: [LinuxReservedMapping::EMPTY; MAX_RESERVED_MAPPINGS],
        }
    }

    pub fn initial_thread_state(&self) -> LinuxThreadState {
        LinuxThreadState {
            fs_base: self.initial_tls.map(|tls| tls.thread_pointer).unwrap_or(0),
            clear_child_tid: 0,
            robust_list_head: 0,
            robust_list_len: 0,
            rseq_area: 0,
            rseq_len: 0,
            rseq_signature: 0,
            signal_mask: 0,
            pending_signals: 0,
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
        crate::syscall::PROC_BROKER_USER_SPACE_END_EXCLUSIVE.saturating_sub(MMAP_COLLISION_GUARD)
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

    pub fn reserve_range(&mut self, start: u64, end: u64) -> Result<(), LinuxMemoryMapError> {
        if start >= end {
            return Err(LinuxMemoryMapError::InvalidRange);
        }
        if self.has_reserved_overlap(start, end) {
            return Err(LinuxMemoryMapError::Overlap);
        }

        let Some(slot) = self
            .reserved_mappings
            .iter_mut()
            .find(|mapping| mapping.is_empty())
        else {
            return Err(LinuxMemoryMapError::Capacity);
        };
        *slot = LinuxReservedMapping { start, end };
        Ok(())
    }

    pub fn release_reserved_range(
        &mut self,
        start: u64,
        end: u64,
    ) -> Result<(), LinuxMemoryMapError> {
        if start >= end {
            return Err(LinuxMemoryMapError::InvalidRange);
        }
        if !self.is_range_reserved(start, end) {
            return Err(LinuxMemoryMapError::Unmapped);
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
                return Err(LinuxMemoryMapError::Capacity);
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

#[cfg(test)]
mod statx_tests {
    use alloc::string::String;

    use super::{LinuxMemoryMapState, LinuxVma, LinuxVmaFlags, LinuxVmaName};

    #[test]
    fn memory_map_unmap_splits_and_adjusts_offsets() {
        let mut maps = LinuxMemoryMapState::new();
        maps.insert_area(
            LinuxVma::new(
                0x1000,
                0x4000,
                0,
                LinuxVmaFlags::private_anon(true, false, true),
                LinuxVmaName::Path(String::from("/bin/test")),
            )
            .unwrap(),
        )
        .unwrap();

        maps.unmap_range(0x2000, 0x3000);

        assert_eq!(maps.areas().len(), 2);
        assert_eq!(maps.areas()[0].start, 0x1000);
        assert_eq!(maps.areas()[0].end, 0x2000);
        assert_eq!(maps.areas()[0].offset, 0);
        assert_eq!(maps.areas()[1].start, 0x3000);
        assert_eq!(maps.areas()[1].end, 0x4000);
        assert_eq!(maps.areas()[1].offset, 0x2000);
    }

    #[test]
    fn memory_map_protect_splits_middle_range() {
        let mut maps = LinuxMemoryMapState::new();
        maps.insert_area(
            LinuxVma::new(
                0x1000,
                0x4000,
                0,
                LinuxVmaFlags::private_anon(true, true, false),
                LinuxVmaName::None,
            )
            .unwrap(),
        )
        .unwrap();

        maps.protect_range(
            0x2000,
            0x3000,
            LinuxVmaFlags::private_anon(true, false, false),
        )
        .unwrap();

        assert_eq!(maps.areas().len(), 3);
        assert_eq!(maps.areas()[1].start, 0x2000);
        assert_eq!(maps.areas()[1].end, 0x3000);
        assert!(!maps.areas()[1].flags.write);
        assert_eq!(maps.areas()[1].offset, 0x1000);
    }

    #[test]
    fn memory_map_merges_adjacent_identical_ranges() {
        let mut maps = LinuxMemoryMapState::new();
        let flags = LinuxVmaFlags::private_anon(true, false, true);
        let name = LinuxVmaName::Path(String::from("/bin/test"));

        maps.insert_area(LinuxVma::new(0x1000, 0x2000, 0, flags, name.clone()).unwrap())
            .unwrap();
        maps.insert_area(LinuxVma::new(0x2000, 0x3000, 0x1000, flags, name).unwrap())
            .unwrap();

        assert_eq!(maps.areas().len(), 1);
        assert_eq!(maps.areas()[0].start, 0x1000);
        assert_eq!(maps.areas()[0].end, 0x3000);
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
    pub signal_mask: u64,
    pub pending_signals: u64,
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

fn normalize_runtime_search_dir(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return None;
    }

    let mut components = Vec::new();
    for component in trimmed.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            components.pop();
            continue;
        }
        components.push(component);
    }

    let mut normalized = String::from("/");
    for (index, component) in components.iter().enumerate() {
        if index != 0 {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    Some(normalized)
}

fn push_unique_runtime_dir(dest: &mut Vec<String>, value: &str) {
    if dest.iter().any(|current| current == value) {
        return;
    }
    dest.push(value.to_string());
}

// Debug-only private syscall for RustOS userspace tracing.
pub const SYS_RUSTOS_DEBUG_PRINT: u64 = crate::syscall::SYS_RUSTOS_DEBUG_PRINT;
pub const SYS_RUSTOS_PRODUCT_MILESTONE: u64 = crate::syscall::SYS_RUSTOS_PRODUCT_MILESTONE;
pub const PRODUCT_MILESTONE_ROOT_CORE_READY: u64 =
    crate::syscall::PRODUCT_MILESTONE_ROOT_CORE_READY;
pub const PRODUCT_MILESTONE_DISPLAY_READY: u64 = crate::syscall::PRODUCT_MILESTONE_DISPLAY_READY;
pub const PRODUCT_MILESTONE_STORAGE_READY: u64 = crate::syscall::PRODUCT_MILESTONE_STORAGE_READY;
pub const PRODUCT_MILESTONE_EXECUTABLE_SNAPSHOT_SEALED: u64 =
    crate::syscall::PRODUCT_MILESTONE_EXECUTABLE_SNAPSHOT_SEALED;
pub const PRODUCT_MILESTONE_FIRST_FRAME: u64 = crate::syscall::PRODUCT_MILESTONE_FIRST_FRAME;
pub const SYS_RUSTOS_SPAWN_EXEC: u64 = crate::syscall::SYS_RUSTOS_SPAWN_EXEC;
pub const SYS_RUSTOS_IPC_ENDPOINT_CREATE: u64 = crate::syscall::SYS_RUSTOS_IPC_ENDPOINT_CREATE;
pub const SYS_RUSTOS_IPC_CALL: u64 = crate::syscall::SYS_RUSTOS_IPC_CALL;
pub const SYS_RUSTOS_IPC_CALL_BOUNDED: u64 = crate::syscall::SYS_RUSTOS_IPC_CALL_BOUNDED;
pub const SYS_RUSTOS_IPC_RECV: u64 = crate::syscall::SYS_RUSTOS_IPC_RECV;
pub const SYS_RUSTOS_IPC_REPLY: u64 = crate::syscall::SYS_RUSTOS_IPC_REPLY;
pub const SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT: u64 =
    crate::syscall::SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT;
pub const SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT: u64 =
    crate::syscall::SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT;
pub const SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT: u64 =
    crate::syscall::SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT;
pub const SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT: u64 =
    crate::syscall::SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT;
pub const SYS_RUSTOS_IPC_CALL_WITH_HANDLES: u64 = crate::syscall::SYS_RUSTOS_IPC_CALL_WITH_HANDLES;
pub const SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED: u64 =
    crate::syscall::SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED;
pub const SYS_RUSTOS_IPC_RECV_WITH_HANDLES: u64 = crate::syscall::SYS_RUSTOS_IPC_RECV_WITH_HANDLES;
pub const SYS_RUSTOS_IPC_REPLY_WITH_HANDLES: u64 =
    crate::syscall::SYS_RUSTOS_IPC_REPLY_WITH_HANDLES;
pub const SYS_RUSTOS_FD_CLOSE_BROKER: u64 = crate::syscall::SYS_RUSTOS_FD_CLOSE_BROKER;
pub const SYS_RUSTOS_FD_DUP_BROKER: u64 = crate::syscall::SYS_RUSTOS_FD_DUP_BROKER;
pub const SYS_RUSTOS_FD_GETDENTS64_BROKER: u64 = crate::syscall::SYS_RUSTOS_FD_GETDENTS64_BROKER;
pub const SYS_RUSTOS_FD_FCNTL_BROKER: u64 = crate::syscall::SYS_RUSTOS_FD_FCNTL_BROKER;
pub const SYS_RUSTOS_VFS_MOUNT_BROKER: u64 = crate::syscall::SYS_RUSTOS_VFS_MOUNT_BROKER;
pub const SYS_RUSTOS_VFS_UMOUNT_BROKER: u64 = crate::syscall::SYS_RUSTOS_VFS_UMOUNT_BROKER;
pub const SYS_RUSTOS_PROC_PREPARE_BROKER: u64 = crate::syscall::SYS_RUSTOS_PROC_PREPARE_BROKER;
pub const SYS_RUSTOS_PROC_MAP_FILE_BROKER: u64 = crate::syscall::SYS_RUSTOS_PROC_MAP_FILE_BROKER;
pub const SYS_RUSTOS_PROC_MAP_ZEROED_BROKER: u64 =
    crate::syscall::SYS_RUSTOS_PROC_MAP_ZEROED_BROKER;
pub const SYS_RUSTOS_PROC_MAP_DATA_BROKER: u64 = crate::syscall::SYS_RUSTOS_PROC_MAP_DATA_BROKER;
pub const SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER: u64 =
    crate::syscall::SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER;
pub const SYS_RUSTOS_PROC_AUTHORIZE_EXEC_BROKER: u64 =
    crate::syscall::SYS_RUSTOS_PROC_AUTHORIZE_EXEC_BROKER;
pub const SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER: u64 =
    crate::syscall::SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER;
pub const SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER: u64 =
    crate::syscall::SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER;
pub const SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER: u64 =
    crate::syscall::SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER;
pub const SYS_RUSTOS_PROC_EXEC_TARGET_BROKER: u64 =
    crate::syscall::SYS_RUSTOS_PROC_EXEC_TARGET_BROKER;
pub const SYS_RUSTOS_PROC_FORK_BROKER: u64 = crate::syscall::SYS_RUSTOS_PROC_FORK_BROKER;
pub const SYS_RUSTOS_PROC_SIGNAL_QUEUE_BROKER: u64 =
    crate::syscall::SYS_RUSTOS_PROC_SIGNAL_QUEUE_BROKER;
pub const SYS_RUSTOS_PROC_COMMIT_BROKER: u64 = crate::syscall::SYS_RUSTOS_PROC_COMMIT_BROKER;
pub const SYS_RUSTOS_PROC_ACTIVATE_BROKER: u64 = crate::syscall::SYS_RUSTOS_PROC_ACTIVATE_BROKER;
pub const SYS_RUSTOS_PROC_VALIDATE_DEFERRED_SPAWN_BROKER: u64 =
    crate::syscall::SYS_RUSTOS_PROC_VALIDATE_DEFERRED_SPAWN_BROKER;
pub const SYS_RUSTOS_PROC_ABORT_BROKER: u64 = crate::syscall::SYS_RUSTOS_PROC_ABORT_BROKER;
pub const SYS_RUSTOS_MM_BROKER: u64 = crate::syscall::SYS_RUSTOS_MM_BROKER;
pub const SYS_RUSTOS_DEVICE_IOCTL_BROKER: u64 = crate::syscall::SYS_RUSTOS_DEVICE_IOCTL_BROKER;
pub const SYS_RUSTOS_NET_BROKER: u64 = crate::syscall::SYS_RUSTOS_NET_BROKER;
pub const SYS_RUSTOS_BLOCK_BROKER: u64 = crate::syscall::SYS_RUSTOS_BLOCK_BROKER;
pub const SYS_RUSTOS_INPUT_STATS_BROKER: u64 = crate::syscall::SYS_RUSTOS_INPUT_STATS_BROKER;
pub const SYS_RUSTOS_EARLY_SYSTEM_BROKER: u64 = crate::syscall::SYS_RUSTOS_EARLY_SYSTEM_BROKER;
pub const SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER: u64 =
    crate::syscall::SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER;
pub const SYS_RUSTOS_DEVICE_OPEN_BROKER: u64 = crate::syscall::SYS_RUSTOS_DEVICE_OPEN_BROKER;
pub const SYS_RUSTOS_INPUT_INGEST_BROKER: u64 = crate::syscall::SYS_RUSTOS_INPUT_INGEST_BROKER;
pub const SYS_RUSTOS_INPUT_WAIT_BROKER: u64 = crate::syscall::SYS_RUSTOS_INPUT_WAIT_BROKER;
pub const SYS_RUSTOS_WAITSET_SIGNAL_BROKER: u64 = crate::syscall::SYS_RUSTOS_WAITSET_SIGNAL_BROKER;
pub const SYS_RUSTOS_ENTROPY_BROKER: u64 = crate::syscall::SYS_RUSTOS_ENTROPY_BROKER;
pub const SYS_RUSTOS_IPC_TRY_RECV: u64 = crate::syscall::SYS_RUSTOS_IPC_TRY_RECV;
pub const SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER: u64 =
    crate::syscall::SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER;
pub const SYS_RUSTOS_IPC_RECV_WITH_SENDER: u64 = crate::syscall::SYS_RUSTOS_IPC_RECV_WITH_SENDER;
pub const SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER: u64 =
    crate::syscall::SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER;
pub const SYS_RUSTOS_ROOTD_WAIT_BROKER: u64 = crate::syscall::SYS_RUSTOS_ROOTD_WAIT_BROKER;
pub const SYS_RUSTOS_ROOTD_TERMINATE_BROKER: u64 =
    crate::syscall::SYS_RUSTOS_ROOTD_TERMINATE_BROKER;
pub const SYS_RUSTOS_STATX_METADATA: u64 = crate::syscall::SYS_RUSTOS_STATX_METADATA;
pub const SYS_RUSTOS_STAT_METADATA: u64 = crate::syscall::SYS_RUSTOS_STAT_METADATA;
pub const SYS_RUSTOS_READLINK_METADATA: u64 = crate::syscall::SYS_RUSTOS_READLINK_METADATA;
pub const SYS_RUSTOS_ACCESS_METADATA: u64 = crate::syscall::SYS_RUSTOS_ACCESS_METADATA;
pub const SYS_RUSTOS_GETCWD_METADATA: u64 = crate::syscall::SYS_RUSTOS_GETCWD_METADATA;
pub const SYS_RUSTOS_CHDIR_METADATA: u64 = crate::syscall::SYS_RUSTOS_CHDIR_METADATA;
pub const IPC_SERVICE_LINUX_SYSCALLD: u64 = crate::syscall::IPC_SERVICE_LINUX_SYSCALLD;
pub const IPC_SERVICE_VFSD: u64 = crate::syscall::IPC_SERVICE_VFSD;
pub const IPC_SERVICE_NETD: u64 = crate::syscall::IPC_SERVICE_NETD;
pub const IPC_SERVICE_DEVMGRD: u64 = crate::syscall::IPC_SERVICE_DEVMGRD;
pub const IPC_SERVICE_LOADERD: u64 = crate::syscall::IPC_SERVICE_LOADERD;
pub const IPC_SERVICE_STORAGED: u64 = crate::syscall::IPC_SERVICE_STORAGED;
pub const IPC_SERVICE_INPUTD: u64 = crate::syscall::IPC_SERVICE_INPUTD;
pub const IPC_SERVICE_PROCD: u64 = crate::syscall::IPC_SERVICE_PROCD;
pub const IPC_SERVICE_ROOTD: u64 = crate::syscall::IPC_SERVICE_ROOTD;
pub const IPC_SERVICE_SESSIOND: u64 = crate::syscall::IPC_SERVICE_SESSIOND;
pub const IPC_SERVICE_PAGERD: u64 = crate::syscall::IPC_SERVICE_PAGERD;
pub const IPC_SERVICE_UISERVER: u64 = crate::syscall::IPC_SERVICE_UISERVER;
pub const IPC_SERVICE_INITD: u64 = crate::syscall::IPC_SERVICE_INITD;

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use core::mem::size_of;

    use super::{LinuxRuntimeProfile, LinuxStatx};

    #[test]
    fn linux_statx_matches_uapi_size() {
        assert_eq!(size_of::<LinuxStatx>(), 0x100);
    }

    #[test]
    fn runtime_profile_normalizes_and_deduplicates_search_dirs() {
        let mut profile = LinuxRuntimeProfile::new();
        profile.allow_loader_search_dir("/lib64");
        profile.allow_loader_search_dir("/usr/lib/../lib64/");
        profile.allow_loader_search_dir("relative/path");
        profile.allow_kernel_runtime_access_dir("/usr/lib64");
        profile.allow_kernel_runtime_access_dir("/usr/lib64/");

        assert_eq!(
            profile.loader_search_dirs(),
            &[String::from("/lib64"), String::from("/usr/lib64")]
        );
        assert_eq!(
            profile.kernel_runtime_access_dirs(),
            &[String::from("/usr/lib64")]
        );
    }
}
