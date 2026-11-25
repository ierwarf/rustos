//! `fstat`/`statx` identity of kernel-owned descriptor objects.
//!
//! - **Owner:** `kernel-compat` materializes the stat of objects whose state
//!   lives in ring0 (pipes, eventfd, epoll, local and inet sockets, memfd);
//!   `vfsd` owns every path-backed file.
//! - **Identity:** `st_ino` is the identity of the kernel *object*, never of
//!   the descriptor: every `dup`, `fork` copy and `SCM_RIGHTS` copy of one open
//!   file shares it, and both ends of a pipe report the same inode. `st_dev`
//!   names the pseudo filesystem the object lives on (one constant per kind),
//!   so two kinds can never alias even if their id counters meet.
//! - **Mode:** the type and permission bits Linux reports: `S_IFIFO|0600`
//!   pipes, `S_IFSOCK|0777` sockets, `S_IFREG|0600` for the anonymous inodes
//!   (eventfd, epoll), `S_IFREG|0777` memfd. `st_nlink` is 1 and `st_blksize`
//!   4096; `st_size` is the memfd length and 0 for the rest.
//! - **Forbidden:** no identity derived from a name or a descriptor number.
use super::*;

/// Anonymous-filesystem device numbers (major 0, minor below). The values
/// only have to be constant, nonzero and mutually distinct.
pub(super) const DEV_ANON_INODE: u64 = 0x10;
pub(super) const DEV_PIPEFS: u64 = 0x11;
pub(super) const DEV_SOCKFS: u64 = 0x12;
pub(super) const DEV_MEMFD: u64 = 0x13;
pub(super) const DEV_SOCKFS_INET: u64 = 0x14;
/// Generated `/proc/<pid>` snapshots (kernel-fact nodes).
pub(super) const DEV_PROCFS: u64 = 0x15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AnonStat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u64,
    pub size: u64,
}

const S_IFSOCK: u32 = 0o140000;
const S_IFREG: u32 = 0o100000;

/// Linux `st_nlink` for a delegated `/proc/<pid>` kernel-fact node:
/// `TaskDir` holds `.`, `..`, and one `<tid>` subdirectory per live thread;
/// `FdDir` and `TaskTidDir` hold no subdirectories (`2`).
pub(super) fn proc_node_nlink(pid: Option<u64>, node: rustos_user_abi::procfs::ProcKernelNode) -> u64 {
    use rustos_user_abi::procfs::ProcKernelNode as Node;
    match node {
        Node::TaskDir => {
            let threads = pid
                .map(|p| multitask::live_user_thread_ids(p).len())
                .unwrap_or(1)
                .max(1);
            task_dir_nlink(threads)
        }
        Node::FdDir | Node::TaskTidDir => 2,
        _ => 1,
    }
}

pub(super) const fn task_dir_nlink(live_threads: usize) -> u64 {
    2_u64.saturating_add(live_threads as u64)
}

fn proc_pid_from_link(link: &str) -> Option<u64> {
    link.strip_prefix("/proc/")?
        .split('/')
        .next()?
        .parse()
        .ok()
}

/// The stat identity of `handle`, or `None` when its kind is not owned here.
pub(super) fn identity_of(handle: &multitask::KernelHandle) -> Option<AnonStat> {
    Some(match handle {
        multitask::KernelHandle::Pipe(pipe) => AnonStat {
            dev: DEV_PIPEFS,
            ino: pipe.inode(),
            mode: linux_abi::S_IFIFO | 0o600,
            nlink: 1,
            size: 0,
        },
        multitask::KernelHandle::EventFd(eventfd) => AnonStat {
            dev: DEV_ANON_INODE,
            ino: eventfd.token_id(),
            mode: S_IFREG | 0o600,
            nlink: 1,
            size: 0,
        },
        multitask::KernelHandle::Epoll(epoll) => AnonStat {
            dev: DEV_ANON_INODE,
            ino: epoll.token_id(),
            mode: S_IFREG | 0o600,
            nlink: 1,
            size: 0,
        },
        multitask::KernelHandle::UnixSocket(socket) => AnonStat {
            dev: DEV_SOCKFS,
            ino: socket.token_id(),
            mode: S_IFSOCK | 0o777,
            nlink: 1,
            size: 0,
        },
        multitask::KernelHandle::InetSocket(socket) => AnonStat {
            dev: DEV_SOCKFS_INET,
            ino: socket.token_id(),
            mode: S_IFSOCK | 0o777,
            nlink: 1,
            size: 0,
        },
        multitask::KernelHandle::Memfd(memfd) if memfd.proc_snapshot().is_some() => {
            return memfd.proc_snapshot().map(|info| proc_snapshot_stat(&info));
        }
        multitask::KernelHandle::Memfd(memfd) => AnonStat {
            dev: DEV_MEMFD,
            ino: memfd.object_id(),
            mode: S_IFREG | 0o777,
            nlink: 1,
            size: memfd.len() as u64,
        },
        _ => return None,
    })
}

/// Generated procfs node: size 0 like every `/proc` file (the content length
/// is only known by reading), ino and mode are the node's own, the same values
/// vfsd reports for `stat(path)` (0644 for the writable nodes, else 0444).
fn proc_snapshot_stat(info: &multitask::ProcSnapshotInfo) -> AnonStat {
    let parsed_node = rustos_user_abi::procfs::ProcKernelNode::from_code(info.node);
    let mode = match parsed_node {
        Some(node) => node.mode(),
        None if info.directory => rustos_user_abi::procfs::S_IFDIR | 0o555,
        None => rustos_user_abi::procfs::S_IFREG | 0o444,
    };
    let nlink = match parsed_node {
        Some(node) => {
            let pid = proc_pid_from_link(info.link.as_str())
                .or_else(multitask::current_user_process_id);
            proc_node_nlink(pid, node)
        }
        None if info.directory => 2,
        None => 1,
    };
    AnonStat {
        dev: DEV_PROCFS,
        ino: info.ino,
        mode,
        nlink,
        size: 0,
    }
}

pub(super) fn encode_stat(stat: AnonStat, uid: u32, gid: u32) -> [u8; LINUX_STAT_SIZE] {
    let mut out = [0_u8; LINUX_STAT_SIZE];
    out[0..8].copy_from_slice(&stat.dev.to_ne_bytes());
    out[8..16].copy_from_slice(&stat.ino.to_ne_bytes());
    out[16..24].copy_from_slice(&stat.nlink.to_ne_bytes());
    out[24..28].copy_from_slice(&stat.mode.to_ne_bytes());
    out[28..32].copy_from_slice(&uid.to_ne_bytes());
    out[32..36].copy_from_slice(&gid.to_ne_bytes());
    out[48..56].copy_from_slice(&(stat.size as i64).to_ne_bytes());
    out[56..64].copy_from_slice(&4096_i64.to_ne_bytes());
    out[64..72].copy_from_slice(&(stat.size.div_ceil(512) as i64).to_ne_bytes());
    out
}

pub(super) fn encode_statx(stat: AnonStat, uid: u32, gid: u32) -> [u8; LINUX_STATX_SIZE] {
    // STATX_TYPE | MODE | NLINK | UID | GID | INO | SIZE | BLOCKS: the fields
    // filled in below. No timestamps are claimed.
    const STATX_FILLED: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x100 | 0x200 | 0x400;
    let mut out = [0_u8; LINUX_STATX_SIZE];
    out[0..4].copy_from_slice(&STATX_FILLED.to_ne_bytes());
    out[4..8].copy_from_slice(&4096_u32.to_ne_bytes());
    let nlink = u32::try_from(stat.nlink).unwrap_or(u32::MAX);
    out[16..20].copy_from_slice(&nlink.to_ne_bytes());
    out[20..24].copy_from_slice(&uid.to_ne_bytes());
    out[24..28].copy_from_slice(&gid.to_ne_bytes());
    out[28..30].copy_from_slice(&(stat.mode as u16).to_ne_bytes());
    out[32..40].copy_from_slice(&stat.ino.to_ne_bytes());
    out[40..48].copy_from_slice(&stat.size.to_ne_bytes());
    out[48..56].copy_from_slice(&stat.size.div_ceil(512).to_ne_bytes());
    // stx_dev_major (0) at 136 and stx_dev_minor at 140.
    out[140..144].copy_from_slice(&(stat.dev as u32).to_ne_bytes());
    out
}

pub(super) fn owner_ids() -> (u32, u32) {
    multitask::with_current_process_credentials(|security| (security.euid(), security.egid()))
        .unwrap_or((0, 0))
}

fn current_identity(fd: u64) -> Option<AnonStat> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        process_state.handles().get(fd).and_then(identity_of)
    })
    .flatten()
}

/// `fstat` on a kernel-owned object; `None` if `fd` is not one.
pub(in crate::user::syscall::linux) fn fstat(fd: u64, stat_ptr: u64) -> Option<u64> {
    let stat = current_identity(fd)?;
    let (uid, gid) = owner_ids();
    Some(linux_unit_result((|| {
        validate_user_write(stat_ptr, LINUX_STAT_SIZE)?;
        write_user_bytes(stat_ptr, &encode_stat(stat, uid, gid))
    })()))
}

/// `statx(fd, "", AT_EMPTY_PATH, ...)` on a kernel-owned object.
pub(in crate::user::syscall::linux) fn statx(fd: u64, statx_ptr: u64) -> Option<u64> {
    let stat = current_identity(fd)?;
    let (uid, gid) = owner_ids();
    Some(linux_unit_result((|| {
        validate_user_write(statx_ptr, LINUX_STATX_SIZE)?;
        write_user_bytes(statx_ptr, &encode_statx(stat, uid, gid))
    })()))
}

/// The identity of a memfd for kernel-internal callers.
pub(super) fn memfd_stat(memfd: &multitask::MemfdHandle) -> linux_abi::LinuxStat {
    let mut st = linux_abi::LinuxStat::default();
    if let Some(info) = memfd.proc_snapshot() {
        let stat = proc_snapshot_stat(&info);
        st.st_dev = stat.dev;
        st.st_ino = stat.ino;
        st.st_mode = stat.mode;
        st.st_nlink = stat.nlink;
        st.st_blksize = 4096;
        return st;
    }
    st.st_dev = DEV_MEMFD;
    st.st_ino = memfd.object_id();
    st.st_mode = S_IFREG | 0o777;
    st.st_nlink = 1;
    st.st_size = memfd.len() as i64;
    st.st_blksize = 4096;
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_ne_bytes(bytes[at..at + 8].try_into().unwrap())
    }
    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn id(handle: multitask::KernelHandle) -> AnonStat {
        identity_of(&handle).expect("kernel-owned kind")
    }

    #[test]
    fn pipe_ends_share_one_inode_and_report_fifo() {
        let (read, write) = multitask::PipeHandle::new_pair(None, false).unwrap();
        let r = id(multitask::KernelHandle::Pipe(read.clone()));
        let w = id(multitask::KernelHandle::Pipe(write));
        assert_eq!((r.dev, r.ino), (w.dev, w.ino));
        assert_eq!(r.mode, linux_abi::S_IFIFO | 0o600);
        // A dup is a clone of the same handle: same object, same identity.
        assert_eq!(id(multitask::KernelHandle::Pipe(read)), r);
        let (other, _) = multitask::PipeHandle::new_pair(None, false).unwrap();
        assert_ne!(id(multitask::KernelHandle::Pipe(other)).ino, r.ino);
    }

    #[test]
    fn anonymous_inodes_are_regular_0600_with_stable_distinct_identity() {
        let a = multitask::EventFdHandle::new(0, false, false).unwrap();
        let b = multitask::EventFdHandle::new(0, false, false).unwrap();
        let ia = id(multitask::KernelHandle::EventFd(a.clone()));
        assert_eq!(ia.mode, S_IFREG | 0o600);
        assert_eq!(ia.dev, DEV_ANON_INODE);
        assert_eq!(id(multitask::KernelHandle::EventFd(a)), ia);
        assert_ne!(id(multitask::KernelHandle::EventFd(b)).ino, ia.ino);
        let e1 = id(multitask::KernelHandle::Epoll(multitask::EpollHandle::from_token(7)));
        assert_eq!((e1.dev, e1.ino, e1.mode), (DEV_ANON_INODE, 7, S_IFREG | 0o600));
    }

    #[test]
    fn sockets_report_sock_mode_and_kind_separated_devices() {
        let creds = multitask::SocketCredentials::new(1, 0, 0);
        let unix_handle =
            multitask::UnixSocketHandle::new(multitask::UnixSocketType::Stream, false, creds)
                .unwrap();
        let unix_token = unix_handle.token_id();
        let unix = id(multitask::KernelHandle::UnixSocket(unix_handle));
        let inet = id(multitask::KernelHandle::InetSocket(
            multitask::InetSocketHandle::from_token(unix_token, 2, 1, 0),
        ));
        assert_eq!(unix.mode, S_IFSOCK | 0o777);
        assert_eq!(inet.mode, S_IFSOCK | 0o777);
        assert_eq!(unix.ino, inet.ino);
        assert_ne!(unix.dev, inet.dev);
        assert_ne!(unix.dev, DEV_ANON_INODE);
    }

    #[test]
    fn memfd_identity_is_per_object_not_per_name_or_description() {
        let a = multitask::MemfdHandle::new("same".into(), false, None);
        let b = multitask::MemfdHandle::new("same".into(), false, None);
        let ia = id(multitask::KernelHandle::Memfd(a.clone()));
        let ib = id(multitask::KernelHandle::Memfd(b));
        assert_ne!(ia.ino, ib.ino, "equal names must not alias");
        // A reopened description is the same file.
        assert_eq!(id(multitask::KernelHandle::Memfd(a.reopen())).ino, ia.ino);
        assert_eq!(ia.mode, S_IFREG | 0o777);
        assert_eq!(ia.size, 0);
        // Size is the live memfd length (frames cannot be allocated in a host test).
        assert_eq!(memfd_stat(&a).st_size, 0);
        assert_eq!(memfd_stat(&a).st_ino, ia.ino);
    }

    #[test]
    fn encoders_place_every_field() {
        let stat = AnonStat { dev: DEV_MEMFD, ino: 99, mode: S_IFREG | 0o777, nlink: 3, size: 1000 };
        let bytes = encode_stat(stat, 1000, 100);
        assert_eq!(u64_at(&bytes, 0), DEV_MEMFD);
        assert_eq!(u64_at(&bytes, 8), 99);
        assert_eq!(u64_at(&bytes, 16), 3);
        assert_eq!(u32_at(&bytes, 24), S_IFREG | 0o777);
        assert_eq!((u32_at(&bytes, 28), u32_at(&bytes, 32)), (1000, 100));
        assert_eq!(u64_at(&bytes, 48), 1000);
        assert_eq!(u64_at(&bytes, 56), 4096);
        assert_eq!(u64_at(&bytes, 64), 2);
        let x = encode_statx(stat, 1000, 100);
        assert_eq!(u32_at(&x, 16), 3);
        assert_eq!(u16::from_ne_bytes(x[28..30].try_into().unwrap()) as u32, S_IFREG | 0o777);
        assert_eq!(u64_at(&x, 32), 99);
        assert_eq!(u64_at(&x, 40), 1000);
        assert_eq!(u32_at(&x, 4), 4096);
        assert_eq!(u32_at(&x, 140) as u64, DEV_MEMFD);
    }

    #[test]
    fn proc_task_dir_nlink_counts_dot_dotdot_and_threads() {
        use rustos_user_abi::procfs::ProcKernelNode as Node;
        assert_eq!(task_dir_nlink(1), 3);
        assert_eq!(task_dir_nlink(4), 6);
        assert_eq!(proc_node_nlink(None, Node::TaskDir), 3);
        assert_eq!(proc_node_nlink(None, Node::TaskTidDir), 2);
        assert_eq!(proc_node_nlink(None, Node::FdDir), 2);
        assert_eq!(proc_node_nlink(None, Node::Status), 1);
        assert_eq!(proc_pid_from_link("/proc/119/task"), Some(119));
        assert_eq!(proc_pid_from_link("/proc/bad/task"), None);
    }
}
