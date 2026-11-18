//! Node model of the caller-only `/proc/<pid>` namespace shared by `vfsd`
//! (namespace policy) and the Ring0 Linux front-end (kernel facts).
//!
//! - **Owner:** `vfsd` decides which names exist and answers every question
//!   that needs no kernel state (stat of static nodes, `/proc/self`
//!   listing, `/proc/self/ns/*`). Ring0 supplies the content of nodes whose
//!   truth only the kernel holds (VMAs, fd table, threads, credentials).
//! - **Delegation:** for such a node `vfsd` answers `status == 0` with
//!   `VfsIpcResponse::aux = encode_delegate(node, number)`. The flag bit can
//!   never collide with a device-access route (those are 16-bit values), and a
//!   response without the bit is an ordinary vfsd result. The kernel then
//!   completes the operation locally for the *calling* process only; `vfsd`
//!   never receives another process's data and never stores any.
//! - **Forbidden:** other pids, writable nodes, and any node whose value
//!   neither side can attest.

/// Set in `VfsIpcResponse::aux` when the kernel must complete the operation.
pub const PROC_DELEGATE_FLAG: u64 = 1 << 63;
const NODE_MASK: u64 = 0xff;
const NUMBER_SHIFT: u32 = 8;
const NUMBER_MASK: u64 = (1 << 55) - 1;

/// Upper bound of one generated snapshot (bytes). Generated files are cut at
/// the last whole record at or below this size (see the kernel formatter).
pub const PROC_SNAPSHOT_MAX_BYTES: usize = 256 * 1024;

pub const S_IFREG: u32 = 0o100_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFLNK: u32 = 0o120_000;

pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;

/// Nodes whose content or existence is decided by kernel facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcKernelNode {
    Maps,
    Smaps,
    Status,
    Stat,
    Statm,
    Cmdline,
    Environ,
    Auxv,
    Limits,
    Comm,
    OomScoreAdj,
    /// `/proc/<pid>/fd`
    FdDir,
    /// `/proc/<pid>/fd/<number>` (magic link; `number` is the fd)
    FdLink,
    /// `/proc/<pid>/task`
    TaskDir,
    /// `/proc/<pid>/task/<number>` (`number` is the tid)
    TaskTidDir,
    /// `/proc/<pid>/task/<number>/stat`
    TaskStat,
    /// `/proc/<pid>/task/<number>/comm`
    TaskComm,
    /// `/proc/<pid>/task/<number>/status`
    TaskStatus,
}

/// The per-process regular files directly under `/proc/<pid>`, in listing order.
pub const SELF_FILE_NODES: [(&str, ProcKernelNode); 11] = [
    ("auxv", ProcKernelNode::Auxv),
    ("cmdline", ProcKernelNode::Cmdline),
    ("comm", ProcKernelNode::Comm),
    ("environ", ProcKernelNode::Environ),
    ("limits", ProcKernelNode::Limits),
    ("maps", ProcKernelNode::Maps),
    ("oom_score_adj", ProcKernelNode::OomScoreAdj),
    ("smaps", ProcKernelNode::Smaps),
    ("stat", ProcKernelNode::Stat),
    ("statm", ProcKernelNode::Statm),
    ("status", ProcKernelNode::Status),
];

/// Names inside `/proc/<pid>/task/<tid>`.
pub const TASK_FILE_NODES: [(&str, ProcKernelNode); 3] = [
    ("comm", ProcKernelNode::TaskComm),
    ("stat", ProcKernelNode::TaskStat),
    ("status", ProcKernelNode::TaskStatus),
];

/// One namespace link of `/proc/<pid>/ns`. RustOS has exactly one namespace
/// of every kind, so every process reports the same identity.
pub const NS_LINKS: [(&str, u64); 10] = [
    ("cgroup", 4_026_531_835),
    ("ipc", 4_026_531_839),
    ("mnt", 4_026_531_840),
    ("net", 4_026_531_992),
    ("pid", 4_026_531_836),
    ("pid_for_children", 4_026_531_836),
    ("time", 4_026_531_834),
    ("time_for_children", 4_026_531_834),
    ("user", 4_026_531_837),
    ("uts", 4_026_531_838),
];

impl ProcKernelNode {
    pub const fn code(self) -> u8 {
        match self {
            Self::Maps => 1,
            Self::Smaps => 2,
            Self::Status => 3,
            Self::Stat => 4,
            Self::Statm => 5,
            Self::Cmdline => 6,
            Self::Environ => 7,
            Self::Auxv => 8,
            Self::Limits => 9,
            Self::Comm => 10,
            Self::OomScoreAdj => 11,
            Self::FdDir => 32,
            Self::FdLink => 33,
            Self::TaskDir => 34,
            Self::TaskTidDir => 35,
            Self::TaskStat => 36,
            Self::TaskComm => 37,
            Self::TaskStatus => 38,
        }
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Maps,
            2 => Self::Smaps,
            3 => Self::Status,
            4 => Self::Stat,
            5 => Self::Statm,
            6 => Self::Cmdline,
            7 => Self::Environ,
            8 => Self::Auxv,
            9 => Self::Limits,
            10 => Self::Comm,
            11 => Self::OomScoreAdj,
            32 => Self::FdDir,
            33 => Self::FdLink,
            34 => Self::TaskDir,
            35 => Self::TaskTidDir,
            36 => Self::TaskStat,
            37 => Self::TaskComm,
            38 => Self::TaskStatus,
            _ => return None,
        })
    }

    pub const fn is_directory(self) -> bool {
        matches!(self, Self::FdDir | Self::TaskDir | Self::TaskTidDir)
    }

    pub const fn is_link(self) -> bool {
        matches!(self, Self::FdLink)
    }

    /// Whether the node is addressed by a number (fd or tid).
    pub const fn takes_number(self) -> bool {
        matches!(
            self,
            Self::FdLink | Self::TaskTidDir | Self::TaskStat | Self::TaskComm | Self::TaskStatus
        )
    }

    /// `st_mode` of the node itself. Generated files are read-only: nothing
    /// in them can be written back (`oom_score_adj` and `comm` are 0644 on
    /// Linux, but a write here would be refused, so the mode says so).
    pub const fn mode(self) -> u32 {
        match self {
            Self::FdDir | Self::TaskDir | Self::TaskTidDir => S_IFDIR | 0o555,
            Self::FdLink => S_IFLNK | 0o777,
            _ => S_IFREG | 0o444,
        }
    }

    pub fn from_self_file_name(name: &str) -> Option<Self> {
        SELF_FILE_NODES
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, node)| *node)
    }

    pub fn from_task_file_name(name: &str) -> Option<Self> {
        TASK_FILE_NODES
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, node)| *node)
    }
}

/// Packs a delegation into `VfsIpcResponse::aux`.
pub const fn encode_delegate(node: ProcKernelNode, number: u64) -> u64 {
    PROC_DELEGATE_FLAG | ((number & NUMBER_MASK) << NUMBER_SHIFT) | node.code() as u64
}

/// Unpacks `VfsIpcResponse::aux`; `None` when it is not a delegation.
pub fn decode_delegate(aux: u64) -> Option<(ProcKernelNode, u64)> {
    if aux & PROC_DELEGATE_FLAG == 0 {
        return None;
    }
    let node = ProcKernelNode::from_code((aux & NODE_MASK) as u8)?;
    let number = (aux >> NUMBER_SHIFT) & NUMBER_MASK;
    Some((node, number))
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x1000_0000_01b3;

fn fnv_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte.to_ascii_lowercase());
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn fnv_decimal(hash: u64, mut value: u64) -> u64 {
    let mut digits = [0_u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    fnv_bytes(hash, &digits[at..])
}

fn node_name(node: ProcKernelNode) -> &'static str {
    match node {
        ProcKernelNode::FdDir | ProcKernelNode::FdLink => "fd",
        ProcKernelNode::TaskDir | ProcKernelNode::TaskTidDir => "task",
        ProcKernelNode::TaskStat => "stat",
        ProcKernelNode::TaskComm => "comm",
        ProcKernelNode::TaskStatus => "status",
        other => SELF_FILE_NODES
            .iter()
            .find(|(_, candidate)| *candidate == other)
            .map(|(name, _)| *name)
            .unwrap_or(""),
    }
}

/// Stable inode of a kernel-fact node of process `pid`: FNV-1a over the
/// canonical `/proc/<pid>/...` path, the same function and spelling `vfsd`'s
/// `path_inode` applies, so `statx(path)` (vfsd) and `fstat(fd)` (kernel)
/// report one inode for one node.
pub fn node_inode(pid: u64, node: ProcKernelNode, number: u64) -> u64 {
    let mut hash = fnv_bytes(FNV_OFFSET, b"/proc/");
    hash = fnv_decimal(hash, pid);
    hash = fnv_bytes(hash, b"/");
    let first = match node {
        ProcKernelNode::TaskStat | ProcKernelNode::TaskComm | ProcKernelNode::TaskStatus => "task",
        other => node_name(other),
    };
    hash = fnv_bytes(hash, first.as_bytes());
    match node {
        ProcKernelNode::FdLink | ProcKernelNode::TaskTidDir => {
            hash = fnv_bytes(hash, b"/");
            hash = fnv_decimal(hash, number);
        }
        ProcKernelNode::TaskStat | ProcKernelNode::TaskComm | ProcKernelNode::TaskStatus => {
            hash = fnv_bytes(hash, b"/");
            hash = fnv_decimal(hash, number);
            hash = fnv_bytes(hash, b"/");
            hash = fnv_bytes(hash, node_name(node).as_bytes());
        }
        _ => {}
    }
    hash.max(1)
}

/// Writes the canonical absolute path of a kernel-fact node of process `pid`,
/// for example `/proc/7/fd/12` or `/proc/7/task/9/stat`.
pub fn write_node_path<W: core::fmt::Write>(
    out: &mut W,
    pid: u64,
    node: ProcKernelNode,
    number: u64,
) -> core::fmt::Result {
    write!(out, "/proc/{pid}/")?;
    match node {
        ProcKernelNode::FdLink => write!(out, "fd/{number}"),
        ProcKernelNode::TaskTidDir => write!(out, "task/{number}"),
        ProcKernelNode::TaskStat | ProcKernelNode::TaskComm | ProcKernelNode::TaskStatus => {
            write!(out, "task/{number}/{}", node_name(node))
        }
        other => write!(out, "{}", node_name(other)),
    }
}

/// Inode of the `/proc/<pid>` directory itself (what `vfsd` reports for it).
pub fn pid_dir_inode(pid: u64) -> u64 {
    fnv_decimal(fnv_bytes(FNV_OFFSET, b"/proc/"), pid).max(1)
}

/// Strict decimal: no sign, no leading zeros, no empty string.
pub fn parse_decimal(name: &str) -> Option<u64> {
    if name.is_empty() || (name.len() > 1 && name.starts_with('0')) {
        return None;
    }
    if !name.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

/// Bytes of one `linux_dirent64` record for a name of `name_len` bytes.
pub const fn dirent64_record_len(name_len: usize) -> usize {
    (19 + name_len + 1).next_multiple_of(8)
}

/// Encodes one `linux_dirent64` at the start of `out`; `None` if it does not fit.
pub fn encode_dirent64(
    out: &mut [u8],
    ino: u64,
    next_offset: i64,
    d_type: u8,
    name: &[u8],
) -> Option<usize> {
    let len = dirent64_record_len(name.len());
    if len > out.len() || len > u16::MAX as usize {
        return None;
    }
    let record = &mut out[..len];
    record.fill(0);
    record[0..8].copy_from_slice(&ino.to_ne_bytes());
    record[8..16].copy_from_slice(&next_offset.to_ne_bytes());
    record[16..18].copy_from_slice(&(len as u16).to_ne_bytes());
    record[18] = d_type;
    record[19..19 + name.len()].copy_from_slice(name);
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegate_round_trips_and_ignores_plain_aux() {
        for node in [
            ProcKernelNode::Maps,
            ProcKernelNode::FdLink,
            ProcKernelNode::TaskTidDir,
            ProcKernelNode::TaskStatus,
        ] {
            let aux = encode_delegate(node, 12_345);
            assert_eq!(decode_delegate(aux), Some((node, 12_345)));
        }
        assert_eq!(decode_delegate(0), None);
        assert_eq!(decode_delegate(0x0100), None);
        assert_eq!(decode_delegate(PROC_DELEGATE_FLAG | 0xff), None);
    }

    #[test]
    fn every_node_code_round_trips() {
        for code in 0..=255_u8 {
            if let Some(node) = ProcKernelNode::from_code(code) {
                assert_eq!(node.code(), code);
            }
        }
        assert!(ProcKernelNode::FdDir.is_directory());
        assert!(ProcKernelNode::FdLink.is_link());
        assert!(ProcKernelNode::TaskStat.takes_number());
        assert!(!ProcKernelNode::Maps.takes_number());
    }

    #[test]
    fn modes_follow_node_kind() {
        assert_eq!(ProcKernelNode::Maps.mode(), S_IFREG | 0o444);
        assert_eq!(ProcKernelNode::FdDir.mode(), S_IFDIR | 0o555);
        assert_eq!(ProcKernelNode::FdLink.mode(), S_IFLNK | 0o777);
    }

    #[test]
    fn names_resolve_in_listing_order() {
        let names: [&str; 11] = core::array::from_fn(|i| SELF_FILE_NODES[i].0);
        let mut sorted = names;
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert_eq!(
            ProcKernelNode::from_self_file_name("maps"),
            Some(ProcKernelNode::Maps)
        );
        assert_eq!(ProcKernelNode::from_self_file_name("mem"), None);
        assert_eq!(
            ProcKernelNode::from_task_file_name("comm"),
            Some(ProcKernelNode::TaskComm)
        );
        assert_eq!(ProcKernelNode::from_task_file_name("maps"), None);
    }

    #[test]
    fn node_inode_hashes_the_canonical_proc_path() {
        let fnv = |text: &str| {
            let mut hash = FNV_OFFSET;
            for byte in text.bytes() {
                hash ^= u64::from(byte.to_ascii_lowercase());
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            hash.max(1)
        };
        assert_eq!(node_inode(7, ProcKernelNode::Maps, 0), fnv("/proc/7/maps"));
        assert_eq!(node_inode(7, ProcKernelNode::FdDir, 0), fnv("/proc/7/fd"));
        assert_eq!(node_inode(7, ProcKernelNode::FdLink, 12), fnv("/proc/7/fd/12"));
        assert_eq!(node_inode(7, ProcKernelNode::TaskDir, 0), fnv("/proc/7/task"));
        assert_eq!(node_inode(7, ProcKernelNode::TaskTidDir, 9), fnv("/proc/7/task/9"));
        assert_eq!(
            node_inode(7, ProcKernelNode::TaskStat, 9),
            fnv("/proc/7/task/9/stat")
        );
        assert_eq!(
            node_inode(1234, ProcKernelNode::TaskComm, 5),
            fnv("/proc/1234/task/5/comm")
        );
        assert_ne!(
            node_inode(7, ProcKernelNode::Maps, 0),
            node_inode(8, ProcKernelNode::Maps, 0)
        );
    }

    #[test]
    fn node_paths_are_canonical() {
        extern crate alloc;
        use alloc::string::String;
        let path = |node, number| {
            let mut out = String::new();
            write_node_path(&mut out, 7, node, number).unwrap();
            out
        };
        assert_eq!(path(ProcKernelNode::Maps, 0), "/proc/7/maps");
        assert_eq!(path(ProcKernelNode::FdDir, 0), "/proc/7/fd");
        assert_eq!(path(ProcKernelNode::FdLink, 3), "/proc/7/fd/3");
        assert_eq!(path(ProcKernelNode::TaskDir, 0), "/proc/7/task");
        assert_eq!(path(ProcKernelNode::TaskTidDir, 9), "/proc/7/task/9");
        assert_eq!(path(ProcKernelNode::TaskComm, 9), "/proc/7/task/9/comm");
        assert_eq!(path(ProcKernelNode::OomScoreAdj, 0), "/proc/7/oom_score_adj");
    }

    #[test]
    fn pid_dir_inode_matches_the_path_hash() {
        let mut hash = FNV_OFFSET;
        for byte in b"/proc/42" {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        assert_eq!(pid_dir_inode(42), hash);
    }

    #[test]
    fn decimal_is_strict() {
        assert_eq!(parse_decimal("0"), Some(0));
        assert_eq!(parse_decimal("17"), Some(17));
        assert_eq!(parse_decimal("017"), None);
        assert_eq!(parse_decimal("+1"), None);
        assert_eq!(parse_decimal(""), None);
        assert_eq!(parse_decimal("1a"), None);
    }

    #[test]
    fn dirent64_layout_matches_linux() {
        let mut buf = [0xaa_u8; 64];
        let len = encode_dirent64(&mut buf, 7, 24, DT_REG, b"maps").unwrap();
        assert_eq!(len, 24);
        assert_eq!(dirent64_record_len(4), 24);
        assert_eq!(u64::from_ne_bytes(buf[0..8].try_into().unwrap()), 7);
        assert_eq!(i64::from_ne_bytes(buf[8..16].try_into().unwrap()), 24);
        assert_eq!(u16::from_ne_bytes(buf[16..18].try_into().unwrap()), 24);
        assert_eq!(buf[18], DT_REG);
        assert_eq!(&buf[19..23], b"maps");
        assert!(buf[23..24].iter().all(|&b| b == 0));
        assert_eq!(buf[24], 0xaa);
        assert!(encode_dirent64(&mut buf[..23], 7, 24, DT_REG, b"maps").is_none());
    }
}
