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
    pub size: u64,
}

const S_IFSOCK: u32 = 0o140000;
const S_IFREG: u32 = 0o100000;

/// The stat identity of `handle`, or `None` when its kind is not owned here.
pub(super) fn identity_of(handle: &multitask::KernelHandle) -> Option<AnonStat> {
    Some(match handle {
        multitask::KernelHandle::Pipe(pipe) => AnonStat {
            dev: DEV_PIPEFS,
            ino: pipe.inode(),
            mode: linux_abi::S_IFIFO | 0o600,
            size: 0,
        },
        multitask::KernelHandle::EventFd(eventfd) => AnonStat {
            dev: DEV_ANON_INODE,
            ino: eventfd.token_id(),
            mode: S_IFREG | 0o600,
            size: 0,
        },
        multitask::KernelHandle::Epoll(epoll) => AnonStat {
            dev: DEV_ANON_INODE,
            ino: epoll.token_id(),
            mode: S_IFREG | 0o600,
            size: 0,
        },
        multitask::KernelHandle::Socket(socket) => AnonStat {
            dev: DEV_SOCKFS,
            ino: socket.token_id(),
            mode: S_IFSOCK | 0o777,
            size: 0,
        },
        multitask::KernelHandle::InetSocket(socket) => AnonStat {
            dev: DEV_SOCKFS_INET,
            ino: socket.token_id(),
            mode: S_IFSOCK | 0o777,
            size: 0,
        },
        multitask::KernelHandle::Memfd(memfd) if memfd.proc_snapshot().is_some() => {
            return memfd.proc_snapshot().map(|info| proc_snapshot_stat(&info));
        }
        multitask::KernelHandle::Memfd(memfd) => AnonStat {
            dev: DEV_MEMFD,
            ino: memfd.object_id(),
            mode: S_IFREG | 0o777,
            size: memfd.len() as u64,
        },
        _ => return None,
    })
}

/// Generated procfs node: size 0 like every `/proc` file (the content length
/// is only known by reading), ino and mode are the node's own, the same values
/// vfsd reports for `stat(path)` (0644 for the writable nodes, else 0444).
fn proc_snapshot_stat(info: &multitask::ProcSnapshotInfo) -> AnonStat {
    let mode = match rustos_user_abi::procfs::ProcKernelNode::from_code(info.node) {
        Some(node) => node.mode(),
        None if info.directory => rustos_user_abi::procfs::S_IFDIR | 0o555,
        None => rustos_user_abi::procfs::S_IFREG | 0o444,
    };
    AnonStat {
        dev: DEV_PROCFS,
        ino: info.ino,
        mode,
        size: 0,
    }
}

pub(super) fn encode_stat(stat: AnonStat, uid: u32, gid: u32) -> [u8; LINUX_STAT_SIZE] {
    let mut out = [0_u8; LINUX_STAT_SIZE];
    out[0..8].copy_from_slice(&stat.dev.to_ne_bytes());
    out[8..16].copy_from_slice(&stat.ino.to_ne_bytes());
    out[16..24].copy_from_slice(&1_u64.to_ne_bytes());
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
    out[16..20].copy_from_slice(&1_u32.to_ne_bytes());
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
    if let Err(err) = usermem::validate_current_user_write_buffer(stat_ptr, LINUX_STAT_SIZE) {
        return Some(linux_errno(user_pointer_error_to_linux_errno(err)));
    }
    let (uid, gid) = owner_ids();
    Some(match usermem::write_current_user_bytes(stat_ptr, &encode_stat(stat, uid, gid)) {
        Ok(()) => 0,
        Err(err) => linux_errno(user_pointer_error_to_linux_errno(err)),
    })
}

/// `statx(fd, "", AT_EMPTY_PATH, ...)` on a kernel-owned object.
pub(in crate::user::syscall::linux) fn statx(fd: u64, statx_ptr: u64) -> Option<u64> {
    let stat = current_identity(fd)?;
    if let Err(err) = usermem::validate_current_user_write_buffer(statx_ptr, LINUX_STATX_SIZE) {
        return Some(linux_errno(user_pointer_error_to_linux_errno(err)));
    }
    let (uid, gid) = owner_ids();
    Some(match usermem::write_current_user_bytes(statx_ptr, &encode_statx(stat, uid, gid)) {
        Ok(()) => 0,
        Err(err) => linux_errno(user_pointer_error_to_linux_errno(err)),
    })
}

/// The identity of a memfd for kernel-internal callers.
pub(super) fn memfd_stat(memfd: &multitask::MemfdHandle) -> linux_abi::LinuxStat {
    let mut st = linux_abi::LinuxStat::default();
    if let Some(info) = memfd.proc_snapshot() {
        let stat = proc_snapshot_stat(&info);
        st.st_dev = stat.dev;
        st.st_ino = stat.ino;
        st.st_mode = stat.mode;
        st.st_nlink = 1;
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
        let unix = id(multitask::KernelHandle::Socket(multitask::SocketHandle::from_token(5, 1, 1, 0)));
        let inet = id(multitask::KernelHandle::InetSocket(multitask::InetSocketHandle::from_token(5, 2, 1, 0)));
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
        let stat = AnonStat { dev: DEV_MEMFD, ino: 99, mode: S_IFREG | 0o777, size: 1000 };
        let bytes = encode_stat(stat, 1000, 100);
        assert_eq!(u64_at(&bytes, 0), DEV_MEMFD);
        assert_eq!(u64_at(&bytes, 8), 99);
        assert_eq!(u64_at(&bytes, 16), 1);
        assert_eq!(u32_at(&bytes, 24), S_IFREG | 0o777);
        assert_eq!((u32_at(&bytes, 28), u32_at(&bytes, 32)), (1000, 100));
        assert_eq!(u64_at(&bytes, 48), 1000);
        assert_eq!(u64_at(&bytes, 56), 4096);
        assert_eq!(u64_at(&bytes, 64), 2);
        let x = encode_statx(stat, 1000, 100);
        assert_eq!(u32_at(&x, 16), 1);
        assert_eq!(u16::from_ne_bytes(x[28..30].try_into().unwrap()) as u32, S_IFREG | 0o777);
        assert_eq!(u64_at(&x, 32), 99);
        assert_eq!(u64_at(&x, 40), 1000);
        assert_eq!(u32_at(&x, 4), 4096);
        assert_eq!(u32_at(&x, 140) as u64, DEV_MEMFD);
    }
}
