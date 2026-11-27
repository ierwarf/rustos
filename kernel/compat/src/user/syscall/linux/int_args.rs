use super::{linux_abi, SyscallFrame};

/// Linux declares many syscall arguments as 32-bit C types (`int`,
/// `unsigned int`, `pid_t`, `mode_t`, `clockid_t`, ...). The kernel reads only
/// the low 32 bits of such a register, so a caller's syscall stub may leave
/// anything in the upper half (Chrome's does). The handlers compare whole
/// 64-bit registers, so every 32-bit argument is canonicalized here before
/// dispatch:
///
/// * [`Kind::Signed`] (`fd`, `dirfd`, `pid`, `clockid`, `sig`, timeouts, ...)
///   is sign-extended, so `AT_FDCWD` (`-100`) and `-1` sentinels arrive as the
///   `-n as i64 as u64` form the handlers compare against.
/// * [`Kind::Unsigned`] (`flags`, `prot`, `cmd`, `mode`, `uid_t`, socket
///   `level`/`optname`, ...) is zero-extended, so `(uid_t)-1` is `u32::MAX`
///   and flag masks never see garbage.
/// * [`Kind::Wide`] (`long`, `size_t`, pointers, `off_t`) is left untouched.
///
/// Arguments are listed in register order `rdi, rsi, rdx, r10, r8, r9`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Wide,
    Signed,
    Unsigned,
}

const W: Kind = Kind::Wide;
const S: Kind = Kind::Signed;
const U: Kind = Kind::Unsigned;

/// 32-bit argument kinds for a Linux syscall number. Every name below must be
/// a defined constant: an unknown identifier in a pattern is a catch-all
/// binding, which would silently apply the arm to every other syscall.
fn arg_kinds(nr: u64) -> [Kind; 6] {
    use linux_abi::*;
    match nr {
        // unsigned int fd, size_t count
        SYS_READ
        | SYS_WRITE
        | SYS_PREAD64
        | SYS_PWRITE64
        | SYS_CLOSE
        | SYS_FSTAT
        | SYS_DUP
        | SYS_FTRUNCATE
        | SYS_FSYNC
        | SYS_FDATASYNC
        | SYS_SYNCFS
        | SYS_FSTATFS => [S, W, W, W, W, W],
        // unsigned int fd, const struct iovec*, int iovcnt
        SYS_READV | SYS_WRITEV | SYS_PREADV | SYS_PWRITEV => [S, W, S, W, W, W],
        // unsigned int fd, const struct iovec*, int iovcnt, pos_l, pos_h, int flags
        SYS_PREADV2 | SYS_PWRITEV2 => [S, W, S, W, W, U],
        // int fd, loff_t offset, loff_t len, int advice
        SYS_FADVISE64 => [S, W, W, U, W, W],
        // int fd, int mode, loff_t offset, loff_t len
        SYS_FALLOCATE => [S, S, W, W, W, W],
        // unsigned int fd, umode_t mode
        SYS_FCHMOD => [S, U, W, W, W, W],
        // path*, umode_t mode
        SYS_CHMOD => [W, U, W, W, W, W],
        // int dirfd, path*, umode_t mode, int flags
        SYS_FCHMODAT | SYS_FCHMODAT2 => [S, W, U, U, W, W],
        // int dirfd, path*, timespec[2]*, int flags
        SYS_UTIMENSAT => [S, W, W, U, W, W],
        // path*, uid_t owner, gid_t group (lchown shares the shape)
        SYS_CHOWN | SYS_LCHOWN => [W, U, U, W, W, W],
        // unsigned int fd, uid_t owner, gid_t group
        SYS_FCHOWN => [S, U, U, W, W, W],
        // int dirfd, path*, uid_t owner, gid_t group, int flag
        SYS_FCHOWNAT => [S, W, U, U, U, W],
        // path*, umode_t mode, unsigned dev
        SYS_MKNOD => [W, U, U, W, W, W],
        // int dirfd, path*, umode_t mode, unsigned dev
        SYS_MKNODAT => [S, W, U, U, W, W],
        // int olddirfd, oldpath*, int newdirfd, newpath*, int flags
        SYS_LINKAT => [S, W, S, W, U, W],
        // link/utimes/utime take only pointers: the default all-wide row
        // int dirfd, path*, struct timeval[2]*
        SYS_FUTIMESAT => [S, W, W, W, W, W],
        // int out_fd, int in_fd, off_t* offset, size_t count
        SYS_SENDFILE => [S, S, W, W, W, W],
        // int fd_in, loff_t*, int fd_out, loff_t*, size_t len, unsigned int flags
        SYS_COPY_FILE_RANGE => [S, W, S, W, W, U],
        // unsigned int first, unsigned int last, unsigned int flags
        SYS_CLOSE_RANGE => [U, U, U, W, W, W],
        // unsigned int fd, unsigned int count
        SYS_GETDENTS64 => [S, W, U, W, W, W],
        // fd, off_t, whence
        SYS_LSEEK => [S, W, U, W, W, W],
        // fd, unsigned int cmd, unsigned long arg
        SYS_IOCTL | SYS_FCNTL => [S, U, W, W, W, W],
        // int fd, int operation
        SYS_FLOCK => [S, U, W, W, W, W],
        SYS_DUP2 => [S, S, W, W, W, W],
        SYS_DUP3 => [S, S, U, W, W, W],
        // addr, len, int prot, int flags, fd, off_t
        SYS_MMAP => [W, W, U, U, S, W],
        SYS_MPROTECT | SYS_MADVISE => [W, W, U, W, W, W],
        // fds*, unsigned int nfds, int timeout
        SYS_POLL => [W, U, S, W, W, W],
        SYS_PPOLL => [W, U, W, W, W, W],
        SYS_SELECT | SYS_PSELECT6 => [S, W, W, W, W, W],
        // epfd, events*, int maxevents, int timeout
        SYS_EPOLL_WAIT | SYS_EPOLL_PWAIT => [S, W, S, S, W, W],
        SYS_EPOLL_CTL => [S, U, S, W, W, W],
        SYS_EPOLL_CREATE1 => [U, W, W, W, W, W],
        // int sig, act*, oact*, size_t
        SYS_RT_SIGACTION => [S, W, W, W, W, W],
        SYS_RT_SIGPROCMASK => [U, W, W, W, W, W],
        SYS_WAIT4 => [S, W, U, W, W, W],
        SYS_KILL | SYS_TKILL => [S, S, W, W, W, W],
        SYS_TGKILL => [S, S, S, W, W, W],
        SYS_SETPGID => [S, S, W, W, W, W],
        SYS_GETPGID | SYS_GETSID => [S, W, W, W, W, W],
        SYS_GET_ROBUST_LIST => [S, W, W, W, W, W],
        SYS_SETUID | SYS_SETGID => [U, W, W, W, W, W],
        // uid_t ruid, uid_t euid (-1 = unchanged) and the gid twin
        SYS_SETREUID | SYS_SETREGID => [U, U, W, W, W, W],
        // uid_t ruid, uid_t euid, uid_t suid and the gid twin
        SYS_SETRESUID | SYS_SETRESGID => [U, U, U, W, W, W],
        SYS_UMASK => [U, W, W, W, W, W],
        SYS_GETRANDOM => [W, W, U, W, W, W],
        // int clockid, int flags, ts*, ts*
        SYS_CLOCK_GETTIME | SYS_CLOCK_GETRES => [S, W, W, W, W, W],
        SYS_CLOCK_NANOSLEEP => [S, U, W, W, W, W],
        SYS_GETRUSAGE => [S, W, W, W, W, W],
        SYS_GETGROUPS => [S, W, W, W, W, W],
        SYS_GETPRIORITY => [S, U, W, W, W, W],
        SYS_SETPRIORITY => [S, U, S, W, W, W],
        SYS_SCHED_GETSCHEDULER
        | SYS_SCHED_GETPARAM
        | SYS_SCHED_GET_PRIORITY_MAX
        | SYS_SCHED_GET_PRIORITY_MIN
        | SYS_SCHED_RR_GET_INTERVAL => [S, W, W, W, W, W],
        // uaddr, int op, u32 val, timeout*, uaddr2*, u32 val3
        SYS_FUTEX => [W, U, U, W, W, U],
        SYS_PRCTL | SYS_ARCH_PRCTL => [U, W, W, W, W, W],
        SYS_CAPGET | SYS_CAPSET => [W, W, W, W, W, W],
        // pid, unsigned int resource, new*, old*
        SYS_PRLIMIT64 => [S, U, W, W, W, W],
        SYS_SCHED_GETAFFINITY | SYS_SCHED_SETAFFINITY => [S, U, W, W, W, W],
        // rseq*, u32 len, int flags, u32 sig
        SYS_RSEQ => [W, U, U, U, W, W],
        SYS_EXECVEAT => [S, W, W, W, U, W],
        SYS_EXECVE => [W; 6],
        // sockets: int domain, int type, int protocol
        SYS_SOCKET => [U, U, U, W, W, W],
        SYS_SOCKETPAIR => [U, U, U, W, W, W],
        SYS_ACCEPT | SYS_GETSOCKNAME | SYS_GETPEERNAME => [S, W, W, W, W, W],
        SYS_ACCEPT4 => [S, W, W, U, W, W],
        // fd, addr*, int addrlen
        SYS_CONNECT | SYS_BIND => [S, W, U, W, W, W],
        SYS_LISTEN => [S, S, W, W, W, W],
        SYS_SHUTDOWN => [S, U, W, W, W, W],
        // fd, int level, int optname, optval*, optlen
        SYS_SETSOCKOPT => [S, U, U, W, U, W],
        SYS_GETSOCKOPT => [S, U, U, W, W, W],
        // fd, buf*, len, int flags, addr*, int addrlen
        SYS_SENDTO => [S, W, W, U, W, U],
        SYS_RECVFROM => [S, W, W, U, W, W],
        SYS_SENDMSG | SYS_RECVMSG => [S, W, U, W, W, W],
        // path*, int flags, umode_t mode / path*, umode_t mode
        SYS_OPEN => [W, U, U, W, W, W],
        SYS_CREAT => [W, U, W, W, W, W],
        // dirfd, path*, int flags, umode_t mode
        SYS_OPENAT => [S, W, U, U, W, W],
        SYS_UNLINKAT => [S, W, U, W, W, W],
        // dirfd, path*, buf*, int bufsiz
        SYS_READLINKAT => [S, W, W, S, W, W],
        SYS_READLINK => [W, W, S, W, W, W],
        // dirfd, path*, int mode (faccessat has no flags register)
        SYS_FACCESSAT => [S, W, U, W, W, W],
        // dirfd, path*, int mode, int flags
        SYS_FACCESSAT2 => [S, W, U, U, W, W],
        // int size
        SYS_EPOLL_CREATE => [S, W, W, W, W, W],
        SYS_ACCESS => [W, U, W, W, W, W],
        SYS_MKDIR => [W, U, W, W, W, W],
        SYS_MKDIRAT => [S, W, U, W, W, W],
        SYS_RENAMEAT => [S, W, S, W, W, W],
        SYS_RENAMEAT2 => [S, W, S, W, U, W],
        SYS_NEWFSTATAT => [S, W, W, U, W, W],
        // dirfd, path*, unsigned flags, unsigned mask, buf*
        SYS_STATX => [S, W, U, U, W, W],
        SYS_SYMLINKAT => [W, S, W, W, W, W],
        SYS_UMOUNT2 => [W, U, W, W, W, W],
        SYS_MEMFD_CREATE => [W, U, W, W, W, W],
        SYS_PIPE2 => [W, U, W, W, W, W],
        SYS_EVENTFD => [U, W, W, W, W, W],
        SYS_EVENTFD2 => [U, U, W, W, W, W],
        SYS_TIMERFD_CREATE => [S, U, W, W, W, W],
        SYS_TIMERFD_SETTIME => [S, U, W, W, W, W],
        SYS_TIMERFD_GETTIME => [S, W, W, W, W, W],
        SYS_NAME_TO_HANDLE_AT => [S, W, W, W, U, W],
        SYS_PKEY_MPROTECT => [W, W, U, S, W, W],
        SYS_PKEY_ALLOC => [U, U, W, W, W, W],
        SYS_PKEY_FREE => [S, W, W, W, W, W],
        // int flags
        SYS_INOTIFY_INIT1 => [U, W, W, W, W, W],
        // int fd, const char *path, uint32_t mask
        SYS_INOTIFY_ADD_WATCH => [S, W, U, W, W, W],
        // int fd, int wd
        SYS_INOTIFY_RM_WATCH => [S, S, W, W, W, W],
        // old*, size_t old_len, size_t new_len, int flags, new*
        SYS_MREMAP => [W, W, W, U, W, W],
        // int flags
        SYS_MLOCKALL => [U, W, W, W, W, W],
        // addr, size_t len, unsigned int flags
        SYS_MLOCK2 | SYS_MSYNC => [W, W, U, W, W, W],
        // int cmd, unsigned int flags, int cpu_id
        SYS_MEMBARRIER => [U, U, S, W, W, W],
        // int epfd, events*, int maxevents, timespec*, sigmask*, size_t
        SYS_EPOLL_PWAIT2 => [S, W, S, W, W, W],
        // unsigned int resource, rlimit*
        SYS_GETRLIMIT | SYS_SETRLIMIT => [U, W, W, W, W, W],
        // pid_t pid, int policy, sched_param*
        SYS_SCHED_SETSCHEDULER => [S, S, W, W, W, W],
        // pid_t pid, sched_param*
        SYS_SCHED_SETPARAM => [S, W, W, W, W, W],
        // int fd, mmsghdr*, unsigned int vlen, unsigned int flags, timespec*
        SYS_SENDMMSG | SYS_RECVMMSG => [S, W, U, U, W, W],
        _ => [W; 6],
    }
}

fn canonicalize(value: u64, kind: Kind) -> u64 {
    match kind {
        Kind::Wide => value,
        Kind::Signed => value as u32 as i32 as i64 as u64,
        Kind::Unsigned => value as u32 as u64,
    }
}

/// Canonicalizes 32-bit C argument slots for a Linux syscall without mutating
/// `frame`: x86-64 `syscall`/`sysretq` preserves user `rdi`, `rsi`, `rdx`, `r10`,
/// `r8`, and `r9` across the trap boundary, so truncating or sign-extending a
/// slot inside `SyscallFrame` would corrupt caller-live registers on return.
pub(super) fn canonical_linux_args(frame: &SyscallFrame) -> [u64; 6] {
    let kinds = arg_kinds(frame.rax);
    let raw = [frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9];
    if kinds == [W; 6] {
        return raw;
    }
    [
        canonicalize(raw[0], kinds[0]),
        canonicalize(raw[1], kinds[1]),
        canonicalize(raw[2], kinds[2]),
        canonicalize(raw[3], kinds[3]),
        canonicalize(raw[4], kinds[4]),
        canonicalize(raw[5], kinds[5]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_abi::*;

    const GARBAGE: u64 = 0x5_0000_0000;

    fn frame(nr: u64, args: [u64; 6]) -> SyscallFrame {
        SyscallFrame {
            user_rsp: 0,
            rax: nr,
            rbx: 0,
            user_rip: 0,
            rdx: args[2],
            rsi: args[1],
            rdi: args[0],
            rbp: 0,
            r8: args[4],
            r9: args[5],
            r10: args[3],
            user_rflags: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
        }
    }

    fn run(nr: u64, args: [u64; 6]) -> [u64; 6] {
        let f = frame(nr, args);
        canonical_linux_args(&f)
    }

    #[test]
    fn canonicalization_does_not_clobber_frame_registers() {
        let orig = [
            0x7000,
            0x8d61_0000_0089,
            0x0001_0000_0001,
            0x10,
            0x20,
            0x008d_6138_85a0,
        ];
        let f = frame(SYS_FUTEX, orig);
        let out = canonical_linux_args(&f);
        assert_eq!(out, [0x7000, 0x89, 1, 0x10, 0x20, 0x6138_85a0]);
        assert_eq!([f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9], orig);
    }

    #[test]
    fn mmap_fd_garbage_and_anonymous_sentinel() {
        let out = run(
            SYS_MMAP,
            [0x1000, 0x2000, GARBAGE | 3, GARBAGE | 0x22, GARBAGE | 4, 0],
        );
        assert_eq!(out, [0x1000, 0x2000, 3, 0x22, 4, 0]);
        let out = run(SYS_MMAP, [0, 0x1000, 3, 0x22, 0xFFFF_FFFF, 0]);
        assert_eq!(out[4], u64::MAX);
        // 64-bit length and offset are untouched.
        let out = run(SYS_MMAP, [0, 0x1_0000_0000, 3, 0x22, 0xFFFF_FFFF, 0x1_0000_1000]);
        assert_eq!(out[1], 0x1_0000_0000);
        assert_eq!(out[5], 0x1_0000_1000);
    }

    #[test]
    fn openat_dirfd_at_fdcwd_with_zero_upper_half() {
        let out = run(SYS_OPENAT, [0xFFFF_FF9C, 0xdead, GARBAGE | 0o2, GARBAGE | 0o644, 0, 0]);
        assert_eq!(out[0], AT_FDCWD as u64);
        assert_eq!(out[1], 0xdead);
        assert_eq!(out[2], 0o2);
        assert_eq!(out[3], 0o644);
        let open = run(SYS_OPEN, [0xdead, GARBAGE | 0o102, GARBAGE | 0o644, 0, 0, 0]);
        assert_eq!(open[0..3], [0xdead, 0o102, 0o644]);
        let creat = run(SYS_CREAT, [0xdead, GARBAGE | 0o666, 0, 0, 0, 0]);
        assert_eq!(creat[0..2], [0xdead, 0o666]);
    }

    #[test]
    fn read_write_fd_garbage_keeps_wide_count() {
        let out = run(SYS_READ, [GARBAGE | 7, 0x7fff_0000, 0x1_0000_0000, 0, 0, 0]);
        assert_eq!(out[0], 7);
        assert_eq!(out[2], 0x1_0000_0000);
    }

    #[test]
    fn dup_family() {
        let out = run(SYS_DUP2, [GARBAGE | 3, GARBAGE | 9, 0, 0, 0, 0]);
        assert_eq!((out[0], out[1]), (3, 9));
        let out = run(SYS_DUP3, [GARBAGE | 3, GARBAGE | 9, GARBAGE | 0o2000000, 0, 0, 0]);
        assert_eq!((out[0], out[1], out[2]), (3, 9, 0o2000000));
    }

    #[test]
    fn epoll_ctl_and_wait() {
        let out = run(SYS_EPOLL_CTL, [GARBAGE | 5, GARBAGE | 1, GARBAGE | 6, 0x1234, 0, 0]);
        assert_eq!(out, [5, 1, 6, 0x1234, 0, 0]);
        let out = run(SYS_EPOLL_WAIT, [GARBAGE | 5, 0x10, GARBAGE | 64, 0xFFFF_FFFF, 0, 0]);
        assert_eq!(out[2], 64);
        assert_eq!(out[3] as i64, -1);
    }

    #[test]
    fn socket_option_args() {
        let out = run(
            SYS_SETSOCKOPT,
            [GARBAGE | 8, GARBAGE | 1, GARBAGE | 7, 0x4000, GARBAGE | 4, 0],
        );
        assert_eq!(out, [8, 1, 7, 0x4000, 4, 0]);
        let out = run(SYS_SOCKET, [GARBAGE | 1, GARBAGE | 0o2000001, GARBAGE, 0, 0, 0]);
        assert_eq!(out, [1, 0o2000001, 0, 0, 0, 0]);
        let out = run(SYS_SENDTO, [GARBAGE | 3, 0x10, 0x20, GARBAGE | 0x4000, 0x30, GARBAGE | 16]);
        assert_eq!(out, [3, 0x10, 0x20, 0x4000, 0x30, 16]);
    }

    #[test]
    fn flock_fd_is_signed_and_operation_is_unsigned_32_bit() {
        let out = run(SYS_FLOCK, [GARBAGE | 6, GARBAGE | 6, 0x77, 0, 0, 0]);
        assert_eq!((out[0], out[1]), (6, 6), "garbage upper halves are dropped");
        // A negative fd stays negative so the handler reports EBADF.
        let out = run(SYS_FLOCK, [0xFFFF_FFFF, 2, 0, 0, 0, 0]);
        assert_eq!(out[0] as i64, -1);
        // The unused third slot is wide and passes through untouched.
        let out = run(SYS_FLOCK, [3, 1, 0x1_0000_0077, 0, 0, 0]);
        assert_eq!(out[2], 0x1_0000_0077);
    }

    #[test]
    fn fcntl_cmd_and_kill_pid_sig() {
        let out = run(SYS_FCNTL, [GARBAGE | 4, GARBAGE | 1030, 0x1_0000_0001, 0, 0, 0]);
        assert_eq!((out[0], out[1], out[2]), (4, 1030, 0x1_0000_0001));
        // kill(-1, SIGTERM) with zero upper halves.
        let out = run(SYS_KILL, [0xFFFF_FFFF, GARBAGE | 15, 0, 0, 0, 0]);
        assert_eq!(out[0] as i64, -1);
        assert_eq!(out[1], 15);
        let out = run(SYS_TGKILL, [GARBAGE | 100, GARBAGE | 101, GARBAGE | 10, 0, 0, 0]);
        assert_eq!((out[0], out[1], out[2]), (100, 101, 10));
        let out = run(SYS_WAIT4, [0xFFFF_FFFF, 0x10, GARBAGE | 1, 0, 0, 0]);
        assert_eq!((out[0] as i64, out[2]), (-1, 1));
    }

    #[test]
    fn uid_minus_one_zero_extends() {
        let out = run(SYS_SETUID, [0xFFFF_FFFF | GARBAGE, 0, 0, 0, 0, 0]);
        assert_eq!(out[0], u64::from(u32::MAX));
    }

    #[test]
    fn set_res_id_family_zero_extends_minus_one() {
        let out = run(SYS_SETRESUID, [0xFFFF_FFFF | GARBAGE, 0xFFFF_FFFF, 7 | GARBAGE, 0, 0, 0]);
        assert_eq!(out[..3], [u64::from(u32::MAX), u64::from(u32::MAX), 7]);
        let out = run(SYS_SETREGID, [0xFFFF_FFFF | GARBAGE, 5 | GARBAGE, 0x1_0000_0001, 0, 0, 0]);
        assert_eq!(out[..3], [u64::from(u32::MAX), 5, 0x1_0000_0001]);
    }

    #[test]
    fn futex_clock_and_prctl() {
        let out = run(SYS_FUTEX, [0x7000, GARBAGE | 0x89, GARBAGE | 1, 0x10, 0x20, GARBAGE | 2]);
        assert_eq!(out, [0x7000, 0x89, 1, 0x10, 0x20, 2]);
        let out = run(SYS_CLOCK_GETTIME, [GARBAGE | 1, 0x7000, 0, 0, 0, 0]);
        assert_eq!(out[0], 1);
        let out = run(SYS_PRCTL, [GARBAGE | 15, 0x7000, 0, 0, 0, 0]);
        assert_eq!(out[0], 15);
    }

    #[test]
    fn memory_signal_limit_and_message_syscall_widths() {
        let out = run(SYS_MREMAP, [0x1000, 0x1_0000_0000, 0x2_0000_0000, GARBAGE | 3, 0x9000, 0]);
        assert_eq!(out, [0x1000, 0x1_0000_0000, 0x2_0000_0000, 3, 0x9000, 0]);
        let out = run(SYS_MLOCKALL, [GARBAGE | 3, 0, 0, 0, 0, 0]);
        assert_eq!(out[0], 3);
        for nr in [SYS_MLOCK2, SYS_MSYNC] {
            let out = run(nr, [0x1000, 0x1_0000_0000, GARBAGE | 4, 0, 0, 0]);
            assert_eq!((out[0], out[1], out[2]), (0x1000, 0x1_0000_0000, 4));
        }
        let out = run(SYS_MEMBARRIER, [GARBAGE | 8, GARBAGE | 1, 0xFFFF_FFFF, 0, 0, 0]);
        assert_eq!((out[0], out[1], out[2] as i64), (8, 1, -1));
        let out = run(SYS_EPOLL_PWAIT2, [GARBAGE | 5, 0x10, GARBAGE | 64, 0x20, 0x30, 8]);
        assert_eq!(out, [5, 0x10, 64, 0x20, 0x30, 8]);
        for nr in [SYS_GETRLIMIT, SYS_SETRLIMIT] {
            let out = run(nr, [GARBAGE | 7, 0x4000, 0, 0, 0, 0]);
            assert_eq!((out[0], out[1]), (7, 0x4000));
        }
        let out = run(SYS_SCHED_SETSCHEDULER, [0xFFFF_FFFF, GARBAGE | 2, 0x5000, 0, 0, 0]);
        assert_eq!((out[0] as i64, out[1], out[2]), (-1, 2, 0x5000));
        let out = run(SYS_SCHED_SETPARAM, [GARBAGE | 9, 0x5000, 0, 0, 0, 0]);
        assert_eq!((out[0], out[1]), (9, 0x5000));
        for nr in [SYS_SENDMMSG, SYS_RECVMMSG] {
            let out = run(nr, [GARBAGE | 6, 0x6000, GARBAGE | 1024, GARBAGE | 0x10000, 0x7000, 0]);
            assert_eq!(out, [6, 0x6000, 1024, 0x10000, 0x7000, 0]);
        }
    }

    #[test]
    fn wide_only_new_syscalls_pass_every_register_through() {
        let args = [GARBAGE | 1, GARBAGE | 2, GARBAGE | 3, GARBAGE | 4, GARBAGE | 5, GARBAGE | 6];
        for nr in [SYS_MINCORE, SYS_MLOCK, SYS_MUNLOCK, SYS_MUNLOCKALL, SYS_RT_SIGTIMEDWAIT] {
            assert_eq!(run(nr, args), args);
        }
        // Existing arms are unchanged by the additions.
        let out = run(SYS_EPOLL_PWAIT, [GARBAGE | 5, 0x10, GARBAGE | 64, GARBAGE | 7, 0x30, 8]);
        assert_eq!(out, [5, 0x10, 64, 7, 0x30, 8]);
        let out = run(SYS_PRLIMIT64, [GARBAGE | 1, GARBAGE | 7, 0x10, 0x20, 0, 0]);
        assert_eq!(out, [1, 7, 0x10, 0x20, 0, 0]);
        assert_eq!(run(SYS_MUNMAP, args), args);
    }

    #[test]
    fn symlinkat_arm_is_not_a_catch_all() {
        // A syscall with no 32-bit args must pass every register through.
        let args = [GARBAGE | 1, GARBAGE | 2, GARBAGE | 3, GARBAGE | 4, GARBAGE | 5, GARBAGE | 6];
        assert_eq!(run(SYS_MUNMAP, args), args);
        assert_eq!(run(SYS_BRK, args), args);
        assert_eq!(run(SYS_EXECVE, args), args);
        let out = run(SYS_SYMLINKAT, [0x10, 0xFFFF_FF9C, 0x20, 0, 0, 0]);
        assert_eq!(out[1], AT_FDCWD as u64);
    }

    #[test]
    fn positional_vectored_close_range_and_fadvise_args() {
        let out = run(SYS_PWRITE64, [GARBAGE | 3, 0x1000, 64, 128, 0, 0]);
        assert_eq!(out, [3, 0x1000, 64, 128, 0, 0]);
        let out = run(
            SYS_PREADV2,
            [GARBAGE | 4, 0x2000, 0xFFFF_FFFE, u64::MAX, 0, GARBAGE | 0x8],
        );
        assert_eq!(out[0], 4);
        assert_eq!(out[2] as i64, -2);
        assert_eq!(out[3], u64::MAX);
        assert_eq!(out[5], 0x8);
        let out = run(
            SYS_FADVISE64,
            [GARBAGE | 5, 0, 4096, GARBAGE | 3, 0, 0],
        );
        assert_eq!((out[0], out[2], out[3]), (5, 4096, 3));
        let out = run(
            SYS_CLOSE_RANGE,
            [GARBAGE | 3, 0x5_FFFF_FFFF, GARBAGE | CLOSE_RANGE_CLOEXEC, 0, 0, 0],
        );
        assert_eq!(out[0], 3);
        assert_eq!(out[1], u64::from(u32::MAX));
        assert_eq!(out[2], CLOSE_RANGE_CLOEXEC);
    }

    #[test]
    fn writable_vfs_syscall_arg_widths() {
        let out = run(SYS_MKDIRAT, [0xFFFF_FF9C, 0x1000, GARBAGE | 0o755, 0, 0, 0]);
        assert_eq!(out[0], AT_FDCWD as u64);
        assert_eq!(out[1], 0x1000);
        assert_eq!(out[2], 0o755);

        let out = run(
            SYS_RENAMEAT2,
            [0xFFFF_FF9C, 0x1000, GARBAGE | 7, 0x2000, GARBAGE | RENAME_NOREPLACE, 0],
        );
        assert_eq!(out[0], AT_FDCWD as u64);
        assert_eq!(out[1], 0x1000);
        assert_eq!(out[2], 7);
        assert_eq!(out[3], 0x2000);
        assert_eq!(out[4], RENAME_NOREPLACE);

        let out = run(SYS_RENAMEAT, [GARBAGE | 3, 0x1000, 0xFFFF_FF9C, 0x2000, 0, 0]);
        assert_eq!(out[0], 3);
        assert_eq!(out[2], AT_FDCWD as u64);

        let out = run(SYS_FSTATFS, [GARBAGE | 9, 0x3000, 0, 0, 0, 0]);
        assert_eq!(out[0], 9);
        assert_eq!(out[1], 0x3000);
    }

    #[test]
    fn ownership_mknod_link_arg_widths() {
        // (uid_t)-1 stays u32::MAX whatever the stub left in the upper half.
        let out = run(SYS_CHOWN, [0x1000, GARBAGE | 0xFFFF_FFFF, GARBAGE | 7, 0, 0, 0]);
        assert_eq!(out, [0x1000, 0xFFFF_FFFF, 7, 0, 0, 0]);
        let out = run(SYS_LCHOWN, [0x1000, GARBAGE | 5, GARBAGE | 0xFFFF_FFFF, 0, 0, 0]);
        assert_eq!(out, [0x1000, 5, 0xFFFF_FFFF, 0, 0, 0]);
        let out = run(SYS_FCHOWN, [GARBAGE | 4, GARBAGE | 0xFFFF_FFFF, GARBAGE | 9, 0, 0, 0]);
        assert_eq!(out, [4, 0xFFFF_FFFF, 9, 0, 0, 0]);
        let out = run(
            SYS_FCHOWNAT,
            [0xFFFF_FF9C, 0x1000, GARBAGE | 1, GARBAGE | 2, GARBAGE | 0x100, 0],
        );
        assert_eq!(out, [0xFFFF_FFFF_FFFF_FF9C, 0x1000, 1, 2, 0x100, 0]);
        let out = run(SYS_MKNOD, [0x1000, GARBAGE | 0o100644, GARBAGE | 0, 0, 0, 0]);
        assert_eq!(out, [0x1000, 0o100644, 0, 0, 0, 0]);
        let out = run(SYS_MKNODAT, [0xFFFF_FF9C, 0x1000, GARBAGE | 0o140600, GARBAGE | 3, 0, 0]);
        assert_eq!(out, [0xFFFF_FFFF_FFFF_FF9C, 0x1000, 0o140600, 3, 0, 0]);
        let out = run(SYS_LINKAT, [GARBAGE | 3, 0x1000, 0xFFFF_FF9C, 0x2000, GARBAGE | 0x400, 0]);
        assert_eq!(out, [3, 0x1000, 0xFFFF_FFFF_FFFF_FF9C, 0x2000, 0x400, 0]);
        let args = [0x1000, 0x2000, 0, 0, 0, 0];
        assert_eq!(run(SYS_STAT, args), args);
        assert_eq!(run(SYS_LSTAT, args), args);
        assert_eq!(run(SYS_LINK, args), args);
        assert_eq!(run(SYS_UTIMES, args), args);
        assert_eq!(run(SYS_UTIME, args), args);
        let out = run(SYS_FUTIMESAT, [0xFFFF_FF9C, 0x1000, 0x2000, 0, 0, 0]);
        assert_eq!(out, [0xFFFF_FFFF_FFFF_FF9C, 0x1000, 0x2000, 0, 0, 0]);
    }

    #[test]
    fn file_metadata_and_bulk_copy_arg_widths() {
        let out = run(SYS_FALLOCATE, [GARBAGE | 3, GARBAGE | 1, 0x1_0000_0000, 0x2_0000_0000, 0, 0]);
        assert_eq!(out, [3, 1, 0x1_0000_0000, 0x2_0000_0000, 0, 0]);
        let out = run(SYS_FCHMODAT, [0xFFFF_FF9C, 0x1000, GARBAGE | 0o644, 0, 0, 0]);
        assert_eq!(out, [0xFFFF_FFFF_FFFF_FF9C, 0x1000, 0o644, 0, 0, 0]);
        let out = run(SYS_FCHMODAT2, [GARBAGE | 4, 0x1000, GARBAGE | 0o600, GARBAGE | 0x100, 0, 0]);
        assert_eq!(out, [4, 0x1000, 0o600, 0x100, 0, 0]);
        // faccessat has three arguments: the fourth register is not canonicalized
        // as a flag word. faccessat2 carries the flags.
        let out = run(SYS_FACCESSAT, [0xFFFF_FF9C, 0x1000, GARBAGE | 4, GARBAGE, 0, 0]);
        assert_eq!(out, [0xFFFF_FFFF_FFFF_FF9C, 0x1000, 4, GARBAGE, 0, 0]);
        let out = run(SYS_FACCESSAT2, [0xFFFF_FF9C, 0x1000, GARBAGE | 4, GARBAGE | 0x200, 0, 0]);
        assert_eq!(out, [0xFFFF_FFFF_FFFF_FF9C, 0x1000, 4, 0x200, 0, 0]);
        let out = run(SYS_EPOLL_CREATE, [GARBAGE | 0xFFFF_FFFF, 0, 0, 0, 0, 0]);
        assert_eq!(out, [0xFFFF_FFFF_FFFF_FFFF, 0, 0, 0, 0, 0]);
        let out = run(SYS_FCHMOD, [GARBAGE | 5, GARBAGE | 0o700, 0, 0, 0, 0]);
        assert_eq!(out, [5, 0o700, 0, 0, 0, 0]);
        let out = run(SYS_UTIMENSAT, [0xFFFF_FF9C, 0x1000, 0x2000, GARBAGE | 0x100, 0, 0]);
        assert_eq!(out, [0xFFFF_FFFF_FFFF_FF9C, 0x1000, 0x2000, 0x100, 0, 0]);
        let out = run(SYS_SENDFILE, [GARBAGE | 6, GARBAGE | 7, 0x3000, 0x1_0000_0000, 0, 0]);
        assert_eq!(out, [6, 7, 0x3000, 0x1_0000_0000, 0, 0]);
        let out = run(
            SYS_COPY_FILE_RANGE,
            [GARBAGE | 3, 0x4000, GARBAGE | 4, 0x5000, 0x1_0000_0000, GARBAGE],
        );
        assert_eq!(out, [3, 0x4000, 4, 0x5000, 0x1_0000_0000, 0]);
    }
}

/// `exit`/`exit_group(code)` wait status: code in bits 8..16, no signal bits.
pub(super) fn linux_exit_wait_status(code: u64) -> i32 {
    ((code as i32) & 0xff) << 8
}
