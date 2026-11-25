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
//! - **Writable nodes:** only `comm`, `task/<tid>/comm` and `oom_score_adj`
//!   (see [`ProcKernelNode::is_writable`]); the kernel applies the write to the
//!   calling process.
//! - **Forbidden:** other pids, any other writable node, and any node whose
//!   value neither side can attest.

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
    /// `/proc/<pid>/smaps_rollup`: whole-process sums of the per-region facts.
    SmapsRollup,
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
pub const SELF_FILE_NODES: [(&str, ProcKernelNode); 12] = [
    ("auxv", ProcKernelNode::Auxv),
    ("cmdline", ProcKernelNode::Cmdline),
    ("comm", ProcKernelNode::Comm),
    ("environ", ProcKernelNode::Environ),
    ("limits", ProcKernelNode::Limits),
    ("maps", ProcKernelNode::Maps),
    ("oom_score_adj", ProcKernelNode::OomScoreAdj),
    ("smaps", ProcKernelNode::Smaps),
    ("smaps_rollup", ProcKernelNode::SmapsRollup),
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
            Self::SmapsRollup => 12,
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
            12 => Self::SmapsRollup,
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

    /// Whether a write to the node is accepted: `comm` (sets the thread
    /// name, as `prctl(PR_SET_NAME)`) and `oom_score_adj` (range-checked and
    /// stored). Every other generated node is read-only.
    pub const fn is_writable(self) -> bool {
        matches!(self, Self::Comm | Self::TaskComm | Self::OomScoreAdj)
    }

    /// `st_mode` of the node itself: writable nodes are 0644 like Linux,
    /// every other generated file is 0444.
    pub const fn mode(self) -> u32 {
        match self {
            Self::FdDir | Self::TaskDir | Self::TaskTidDir => S_IFDIR | 0o555,
            Self::FdLink => S_IFLNK | 0o777,
            node if node.is_writable() => S_IFREG | 0o644,
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

/// Bits of a per-tid task node's `number` that hold the tid; the bits above
/// hold the target pid when it is not the caller (`0` = the caller). Tids are
/// global, so `(pid, tid)` names one thread and the owner check is
/// `tid in live_user_thread_ids(pid)`.
pub const TASK_NUMBER_TID_BITS: u32 = 28;
const TASK_NUMBER_TID_MASK: u64 = (1 << TASK_NUMBER_TID_BITS) - 1;

/// Whether `node`'s `number` is a (possibly pid-qualified) tid.
pub const fn is_task_tid_node(node: ProcKernelNode) -> bool {
    matches!(
        node,
        ProcKernelNode::TaskTidDir
            | ProcKernelNode::TaskStat
            | ProcKernelNode::TaskComm
            | ProcKernelNode::TaskStatus
    )
}

/// Packs `/proc/<pid>/task/<tid>` for another process; `None` when either
/// does not fit (the node cannot exist). `pid == 0` is the caller.
pub const fn pack_task_number(pid: u64, tid: u64) -> Option<u64> {
    if tid > TASK_NUMBER_TID_MASK || (pid >> (55 - TASK_NUMBER_TID_BITS)) != 0 {
        return None;
    }
    Some((pid << TASK_NUMBER_TID_BITS) | tid)
}

/// Splits a per-tid task node `number` into `(pid, tid)`; pid `0` = caller.
pub const fn unpack_task_number(number: u64) -> (u64, u64) {
    (number >> TASK_NUMBER_TID_BITS, number & TASK_NUMBER_TID_MASK)
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

/// `(pid, number)` with a pid-qualified task number resolved to its owner.
fn resolve_task_number(pid: u64, node: ProcKernelNode, number: u64) -> (u64, u64) {
    if is_task_tid_node(node) {
        let (owner, tid) = unpack_task_number(number);
        (if owner != 0 { owner } else { pid }, tid)
    } else {
        (pid, number)
    }
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
    let (pid, number) = resolve_task_number(pid, node, number);
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
    let (pid, number) = resolve_task_number(pid, node, number);
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

// ---- bounded binary wire codecs for Ring0 -> vfsd /proc facts pull channel ---------------

pub const PROC_REGION_READ: u8 = 1 << 0;
pub const PROC_REGION_WRITE: u8 = 1 << 1;
pub const PROC_REGION_EXEC: u8 = 1 << 2;
pub const PROC_REGION_SHARED: u8 = 1 << 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcProcessFactsWire {
    pub pid: u64,
    pub ppid: u64,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub threads: u64,
    pub start_ticks: u64,
    pub rss_pages: Option<u64>,
    pub locked_bytes: u64,
    pub fd_slots: Option<u64>,
    pub oom_score_adj: i16,
    pub brk_start: u64,
    pub arg_start: u64,
    pub arg_end: u64,
    pub env_start: u64,
    pub env_end: u64,
    pub sig_ignored: u64,
    pub sig_caught: u64,
    pub no_new_privs: bool,
    pub region_count: u32,
}

impl ProcProcessFactsWire {
    pub const ENCODED_LEN: usize = 136;

    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < Self::ENCODED_LEN {
            return None;
        }
        let mut at = 0_usize;
        let mut put = |slice: &[u8]| {
            out[at..at + slice.len()].copy_from_slice(slice);
            at += slice.len();
        };
        put(&self.pid.to_le_bytes());
        put(&self.ppid.to_le_bytes());
        put(&self.uid.to_le_bytes());
        put(&self.gid.to_le_bytes());
        put(&self.euid.to_le_bytes());
        put(&self.egid.to_le_bytes());
        put(&self.threads.to_le_bytes());
        put(&self.start_ticks.to_le_bytes());
        put(&self.rss_pages.unwrap_or(0).to_le_bytes());
        put(&self.locked_bytes.to_le_bytes());
        put(&self.fd_slots.unwrap_or(0).to_le_bytes());
        put(&self.brk_start.to_le_bytes());
        put(&self.arg_start.to_le_bytes());
        put(&self.arg_end.to_le_bytes());
        put(&self.env_start.to_le_bytes());
        put(&self.env_end.to_le_bytes());
        put(&self.sig_ignored.to_le_bytes());
        put(&self.sig_caught.to_le_bytes());
        put(&self.region_count.to_le_bytes());
        put(&self.oom_score_adj.to_le_bytes());
        let flags = u8::from(self.rss_pages.is_some())
            | (u8::from(self.fd_slots.is_some()) << 1)
            | (u8::from(self.no_new_privs) << 2);
        put(&[flags, 0]);
        Some(at)
    }

    pub fn decode(bytes: &[u8]) -> Option<(Self, &[u8])> {
        if bytes.len() < Self::ENCODED_LEN {
            return None;
        }
        let mut at = 0_usize;
        let u64_at = |at: &mut usize| -> Option<u64> {
            let v = u64::from_le_bytes(bytes[*at..*at + 8].try_into().ok()?);
            *at += 8;
            Some(v)
        };
        let u32_at = |at: &mut usize| -> Option<u32> {
            let v = u32::from_le_bytes(bytes[*at..*at + 4].try_into().ok()?);
            *at += 4;
            Some(v)
        };
        let pid = u64_at(&mut at)?;
        let ppid = u64_at(&mut at)?;
        let uid = u32_at(&mut at)?;
        let gid = u32_at(&mut at)?;
        let euid = u32_at(&mut at)?;
        let egid = u32_at(&mut at)?;
        let threads = u64_at(&mut at)?;
        let start_ticks = u64_at(&mut at)?;
        let rss_val = u64_at(&mut at)?;
        let locked_bytes = u64_at(&mut at)?;
        let fd_val = u64_at(&mut at)?;
        let brk_start = u64_at(&mut at)?;
        let arg_start = u64_at(&mut at)?;
        let arg_end = u64_at(&mut at)?;
        let env_start = u64_at(&mut at)?;
        let env_end = u64_at(&mut at)?;
        let sig_ignored = u64_at(&mut at)?;
        let sig_caught = u64_at(&mut at)?;
        let region_count = u32_at(&mut at)?;
        let oom_score_adj = i16::from_le_bytes(bytes[at..at + 2].try_into().ok()?);
        at += 2;
        let flags = bytes[at];
        at += 2;
        Some((
            Self {
                pid,
                ppid,
                uid,
                gid,
                euid,
                egid,
                threads,
                start_ticks,
                rss_pages: (flags & 1 != 0).then_some(rss_val),
                locked_bytes,
                fd_slots: (flags & 2 != 0).then_some(fd_val),
                oom_score_adj,
                brk_start,
                arg_start,
                arg_end,
                env_start,
                env_end,
                sig_ignored,
                sig_caught,
                no_new_privs: flags & 4 != 0,
                region_count,
            },
            &bytes[at..],
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcMapRegionWire<'a> {
    pub start: u64,
    pub end: u64,
    pub offset: u64,
    pub flags: u8,
    pub name: Option<&'a [u8]>,
}

impl<'a> ProcMapRegionWire<'a> {
    pub const HEADER_LEN: usize = 28;

    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let name_bytes = self.name.unwrap_or(&[]);
        let name_len = u16::try_from(name_bytes.len()).ok()?;
        let total = Self::HEADER_LEN + name_bytes.len();
        if out.len() < total {
            return None;
        }
        out[0..8].copy_from_slice(&self.start.to_le_bytes());
        out[8..16].copy_from_slice(&self.end.to_le_bytes());
        out[16..24].copy_from_slice(&self.offset.to_le_bytes());
        out[24] = self.flags;
        out[25] = u8::from(self.name.is_some());
        out[26..28].copy_from_slice(&name_len.to_le_bytes());
        out[28..total].copy_from_slice(name_bytes);
        Some(total)
    }

    pub fn decode(bytes: &'a [u8]) -> Option<(Self, &'a [u8])> {
        if bytes.len() < Self::HEADER_LEN {
            return None;
        }
        let start = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let end = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        let offset = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
        let flags = bytes[24];
        let has_name = bytes[25] != 0;
        let name_len = usize::from(u16::from_le_bytes(bytes[26..28].try_into().ok()?));
        let total = Self::HEADER_LEN.checked_add(name_len)?;
        if bytes.len() < total {
            return None;
        }
        let name = if has_name {
            Some(&bytes[28..total])
        } else {
            None
        };
        Some((
            Self {
                start,
                end,
                offset,
                flags,
                name,
            },
            &bytes[total..],
        ))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcThreadFactsWire {
    pub tid: u64,
    pub state: u8,
    pub comm_len: u8,
    pub comm: [u8; 16],
    pub pending_signals: u64,
    pub blocked_signals: u64,
    pub utime_ticks: Option<u64>,
}

impl ProcThreadFactsWire {
    pub const ENCODED_LEN: usize = 52;

    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < Self::ENCODED_LEN {
            return None;
        }
        out[0..8].copy_from_slice(&self.tid.to_le_bytes());
        out[8] = self.state;
        out[9] = u8::from(self.utime_ticks.is_some());
        out[10] = self.comm_len.min(16);
        out[11] = 0;
        out[12..28].copy_from_slice(&self.comm);
        out[28..36].copy_from_slice(&self.pending_signals.to_le_bytes());
        out[36..44].copy_from_slice(&self.blocked_signals.to_le_bytes());
        out[44..52].copy_from_slice(&self.utime_ticks.unwrap_or(0).to_le_bytes());
        Some(Self::ENCODED_LEN)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::ENCODED_LEN {
            return None;
        }
        let tid = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let state = bytes[8];
        let has_utime = bytes[9] != 0;
        let comm_len = bytes[10].min(16);
        let mut comm = [0_u8; 16];
        comm.copy_from_slice(&bytes[12..28]);
        let pending_signals = u64::from_le_bytes(bytes[28..36].try_into().ok()?);
        let blocked_signals = u64::from_le_bytes(bytes[36..44].try_into().ok()?);
        let utime_val = u64::from_le_bytes(bytes[44..52].try_into().ok()?);
        Some(Self {
            tid,
            state,
            comm_len,
            comm,
            pending_signals,
            blocked_signals,
            utime_ticks: has_utime.then_some(utime_val),
        })
    }
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
        for node in [
            ProcKernelNode::Comm,
            ProcKernelNode::TaskComm,
            ProcKernelNode::OomScoreAdj,
        ] {
            assert!(node.is_writable());
            assert_eq!(node.mode(), S_IFREG | 0o644);
        }
        assert!(!ProcKernelNode::Status.is_writable());
        assert!(!ProcKernelNode::TaskStatus.is_writable());
    }

    #[test]
    fn names_resolve_in_listing_order() {
        let names: [&str; 12] = core::array::from_fn(|i| SELF_FILE_NODES[i].0);
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

    #[test]
    fn proc_facts_wire_codecs_round_trip() {
        let header = ProcProcessFactsWire {
            pid: 7,
            ppid: 1,
            uid: 1000,
            gid: 1000,
            euid: 0,
            egid: 1000,
            threads: 3,
            start_ticks: 555,
            rss_pages: Some(42),
            locked_bytes: 8192,
            fd_slots: Some(64),
            oom_score_adj: -250,
            brk_start: 0x4000_0000,
            arg_start: 0x7fff_0000,
            arg_end: 0x7fff_0010,
            env_start: 0x7fff_0010,
            env_end: 0x7fff_0020,
            sig_ignored: 4,
            sig_caught: 8,
            no_new_privs: true,
            region_count: 1,
        };
        let mut buf = [0_u8; 256];
        let n = header.encode(&mut buf).unwrap();
        assert_eq!(n, ProcProcessFactsWire::ENCODED_LEN);
        let (decoded, rest) = ProcProcessFactsWire::decode(&buf[..n]).unwrap();
        assert_eq!(decoded, header);
        assert!(rest.is_empty());

        let region = ProcMapRegionWire {
            start: 0x1000,
            end: 0x2000,
            offset: 0x100,
            flags: PROC_REGION_READ | PROC_REGION_EXEC,
            name: Some(b"/bin/app"),
        };
        let rn = region.encode(&mut buf).unwrap();
        let (rdec, rrest) = ProcMapRegionWire::decode(&buf[..rn]).unwrap();
        assert_eq!(rdec, region);
        assert!(rrest.is_empty());

        let thread = ProcThreadFactsWire {
            tid: 9,
            state: b'R',
            comm_len: 4,
            comm: *b"main\0\0\0\0\0\0\0\0\0\0\0\0",
            pending_signals: 1,
            blocked_signals: 2,
            utime_ticks: Some(12),
        };
        let tn = thread.encode(&mut buf).unwrap();
        assert_eq!(ProcThreadFactsWire::decode(&buf[..tn]), Some(thread));
    }
}
