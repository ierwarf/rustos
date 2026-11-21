//! Minimal `/proc` namespace owned by vfsd.
//!
//! vfsd serves exactly the nodes it can back with kernel-attested identity:
//!
//! * `/proc` - directory (`filesystems`, `meminfo`, `mounts`, `self`, `<caller pid>`).
//! * `/proc/meminfo` - kernel physical memory / swap counters from `SYS_SYSINFO`.
//! * `/proc/filesystems` - filesystems served by `vfsd`.
//! * `/proc/mounts`, `/proc/self/mounts`, `/proc/<pid>/mounts` - active VFS mounts.
//! * `/proc/self` - symlink whose target is the caller's decimal pid.
//! * `/proc/<pid>` and `/proc/self/` as a path prefix - directory, but only
//!   when `<pid>` is the caller's own pid.
//! * `/proc/<pid>/exe` - symlink to the caller's executable path. The path is
//!   the kernel's `UserProcessState::exec_path`, attached by the Ring0 Linux
//!   front-end to path-metadata requests (`VfsIpcRequest::payload`); vfsd never
//!   learns another process's path.
//!
//! * `/proc/<pid>/{maps,smaps,status,stat,statm,cmdline,environ,auxv,limits,comm,
//!   oom_score_adj}`, `fd`, `fd/<n>`, `task`, `task/<tid>` and
//!   `task/<tid>/{stat,comm,status}` - kernel-fact nodes (`ProcNode::Kernel`).
//!   vfsd owns their names and answers stat/access of the static ones, but
//!   their content (VMAs, fd table, threads, credentials) is Ring0's: vfsd
//!   answers `open` (and every operation on the fd/task subtrees) with a
//!   delegation marker (`rustos_user_abi::procfs::encode_delegate`) and the
//!   kernel completes it for the calling process.
//! * `/proc/<pid>/ns` and `ns/<kind>` - namespace links (`net:[id]`); RustOS has
//!   one namespace of each kind so the ids are constants.
//!
//! Every other name under `/proc`, including any other pid (live or not), is
//! ENOENT. Values that nothing can attest are not served: a fabricated value
//! would be a fake success path.
//!
//! Paths passed in must already be canonical (see
//! `path::normalize_absolute_path`), so `..`, `.`, empty components and
//! trailing slashes cannot reach the parser.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::size_of;

use rustos_user_abi::procfs::{
    self as abi_procfs, DT_DIR, DT_LNK, DT_REG, NS_LINKS, ProcKernelNode, SELF_FILE_NODES,
};
use rustos_user_abi::syscall::{LINUX_STATX_SIZE, LINUX_STAT_SIZE};

use crate::linux_types::{LinuxStat, LinuxStatx};
use crate::path::path_inode;
use crate::{EINVAL, ELOOP, ENOENT, ENOTDIR};

const EACCES: i32 = 13;
const O_ACCMODE: u64 = 3;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_DIRECTORY: u64 = 0o200_000;
const O_NOFOLLOW: u64 = 0o400_000;

pub const S_IFREG: u32 = 0o100_000;
pub const S_IFLNK: u32 = 0o120_000;
pub const S_IFDIR: u32 = 0o040_000;
const REG_READONLY_MODE: u32 = S_IFREG | 0o444;
const SYMLINK_MODE: u32 = S_IFLNK | 0o777;
const PID_DIR_MODE: u32 = S_IFDIR | 0o555;
const BLOCK_SIZE: u32 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcNode {
    /// `/proc/self`: symlink to the caller's pid.
    SelfLink,
    /// `/proc/<caller pid>` (also reached through `/proc/self/`).
    PidDir,
    /// `/proc/<caller pid>/exe`: symlink to the caller's executable path.
    ExeLink,
    /// `/proc/meminfo`: kernel memory usage summary.
    MeminfoFile,
    /// `/proc/filesystems`: filesystem types implemented by `vfsd`.
    FilesystemsFile,
    /// `/proc/mounts` or `/proc/<caller pid>/mounts`: active mount table.
    MountsFile,
    /// `/proc/uptime`: monotonic uptime in seconds.
    UptimeFile,
    /// A node whose content is a kernel fact (see the module comment). The
    /// number is the fd or tid for `FdLink`/`Task*` nodes, zero otherwise.
    Kernel(ProcKernelNode, u64),
    /// `/proc/<caller pid>/ns`
    NsDir,
    /// `/proc/<caller pid>/ns/<kind>`: index into `NS_LINKS`.
    NsLink(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SysFile {
    /// `/sys/devices/system/cpu/online`
    CpuOnline,
    /// `/sys/devices/system/cpu/possible`
    CpuPossible,
    /// `/sys/devices/system/cpu/present`
    CpuPresent,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemInfoSnapshot {
    pub mem_total_bytes: u64,
    pub mem_free_bytes: u64,
    pub buffers_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_free_bytes: u64,
}

/// Classifies a canonical absolute `path`.
///
/// `None`: not a procfs node handled here (outside `/proc`, or `/proc`
/// itself). `Some(Err(ENOENT))`: the name is under `/proc` but is not a
/// supported node of the caller.
pub fn lookup(path: &str, caller_pid: u64) -> Option<Result<ProcNode, i32>> {
    let rest = path.strip_prefix("/proc/")?;
    let mut parts = [""; 5];
    let mut count = 0;
    for part in rest.split('/') {
        if count == parts.len() {
            return Some(Err(ENOENT));
        }
        parts[count] = part;
        count += 1;
    }
    let first = parts[0];
    if count == 1 {
        match first {
            "self" => return Some(Ok(ProcNode::SelfLink)),
            "meminfo" => return Some(Ok(ProcNode::MeminfoFile)),
            "filesystems" => return Some(Ok(ProcNode::FilesystemsFile)),
            "mounts" => return Some(Ok(ProcNode::MountsFile)),
            "uptime" => return Some(Ok(ProcNode::UptimeFile)),
            _ => {}
        }
    }
    if first != "self" && parse_pid(first) != Some(caller_pid) {
        return Some(Err(ENOENT));
    }
    Some(lookup_in_pid_dir(&parts[1..count]))
}

/// Resolves the components below `/proc/<caller pid>`.
fn lookup_in_pid_dir(parts: &[&str]) -> Result<ProcNode, i32> {
    let kernel = |node, number| Ok(ProcNode::Kernel(node, number));
    match parts {
        [] => Ok(ProcNode::PidDir),
        ["exe"] => Ok(ProcNode::ExeLink),
        ["mounts"] => Ok(ProcNode::MountsFile),
        ["fd"] => kernel(ProcKernelNode::FdDir, 0),
        ["fd", fd] => match abi_procfs::parse_decimal(fd) {
            Some(fd) => kernel(ProcKernelNode::FdLink, fd),
            None => Err(ENOENT),
        },
        ["task"] => kernel(ProcKernelNode::TaskDir, 0),
        ["task", tid] => match abi_procfs::parse_decimal(tid) {
            Some(tid) => kernel(ProcKernelNode::TaskTidDir, tid),
            None => Err(ENOENT),
        },
        ["task", tid, name] => {
            let tid = abi_procfs::parse_decimal(tid).ok_or(ENOENT)?;
            let node = ProcKernelNode::from_task_file_name(name).ok_or(ENOENT)?;
            kernel(node, tid)
        }
        ["ns"] => Ok(ProcNode::NsDir),
        ["ns", name] => NS_LINKS
            .iter()
            .position(|(candidate, _)| candidate == name)
            .map(ProcNode::NsLink)
            .ok_or(ENOENT),
        [name] => match ProcKernelNode::from_self_file_name(name) {
            Some(node) => kernel(node, 0),
            None => Err(ENOENT),
        },
        _ => Err(ENOENT),
    }
}

/// `aux` for an operation that only the kernel can complete (the fd and task
/// subtrees: existence, stat, readlink and open all depend on live tables).
/// `None` for nodes vfsd answers itself.
pub fn kernel_only_delegate(node: ProcNode) -> Option<u64> {
    match node {
        ProcNode::Kernel(kernel, number)
            if kernel.is_directory() || kernel.is_link() || kernel.takes_number() =>
        {
            Some(abi_procfs::encode_delegate(kernel, number))
        }
        _ => None,
    }
}

/// `aux` for `open` of a kernel-fact node: the kernel builds the snapshot (or
/// directory listing) for the calling process. `None` for other nodes.
pub fn open_delegate(node: ProcNode) -> Option<u64> {
    match node {
        ProcNode::Kernel(kernel, number) => Some(abi_procfs::encode_delegate(kernel, number)),
        _ => None,
    }
}

/// Whether `kernel` is a `/proc/<pid>/*` node whose read-only text is formatted
/// in Ring3 `vfsd` from raw facts pulled via `SYS_RUSTOS_PROC_FACTS_BROKER`.
pub const fn is_ring3_formatted(kernel: ProcKernelNode) -> bool {
    matches!(
        kernel,
        ProcKernelNode::Maps
            | ProcKernelNode::Smaps
            | ProcKernelNode::Status
            | ProcKernelNode::Stat
            | ProcKernelNode::Statm
            | ProcKernelNode::Comm
            | ProcKernelNode::OomScoreAdj
            | ProcKernelNode::TaskStat
            | ProcKernelNode::TaskComm
            | ProcKernelNode::TaskStatus
    )
}

/// `aux` for `open(flags)` when Ring0 must complete the open (`FdLink`,
/// `Cmdline`/`Environ`/`Auxv`, `Limits`, directory snapshots, or writable opens
/// of `comm`/`oom_score_adj`). Read-only opens of Ring3-formatted nodes return
/// `None` so `vfsd` pulls raw facts and formats them in Ring3.
pub fn open_delegate_for_flags(node: ProcNode, flags: u64) -> Option<u64> {
    let ProcNode::Kernel(kernel, number) = node else {
        return None;
    };
    if is_ring3_formatted(kernel) && (flags & O_ACCMODE == 0) {
        return None;
    }
    Some(abi_procfs::encode_delegate(kernel, number))
}

/// Errno for an `open(flags)` of a delegated node that can never succeed, or
/// `None` when the kernel should build the snapshot. Only `comm`,
/// `task/<tid>/comm` and `oom_score_adj` may be opened for writing or with
/// `O_TRUNC`; `O_CREAT` is always refused; a regular file is not a directory;
/// a magic link is not followed under `O_NOFOLLOW`.
pub fn delegated_open_errno(node: ProcNode, flags: u64) -> Option<i32> {
    let ProcNode::Kernel(kernel, _) = node else {
        return None;
    };
    if flags & O_CREAT != 0 {
        return Some(EACCES);
    }
    if !kernel.is_writable() && (flags & O_ACCMODE != 0 || flags & O_TRUNC != 0) {
        return Some(EACCES);
    }
    if kernel.is_link() && flags & O_NOFOLLOW != 0 {
        return Some(ELOOP);
    }
    if !kernel.is_directory() && !kernel.is_link() && flags & O_DIRECTORY != 0 {
        return Some(ENOTDIR);
    }
    None
}

/// Entries of `/proc/<caller pid>` as `(name, d_type)`.
pub fn pid_dir_entries() -> Vec<(&'static str, u8)> {
    let mut entries: Vec<(&'static str, u8)> = Vec::new();
    entries.push(("exe", DT_LNK));
    entries.push(("fd", DT_DIR));
    entries.push(("mounts", DT_REG));
    entries.push(("ns", DT_DIR));
    entries.push(("task", DT_DIR));
    for (name, _) in SELF_FILE_NODES {
        entries.push((name, DT_REG));
    }
    entries.sort_unstable_by_key(|(name, _)| *name);
    entries
}

/// Entries of `/proc/<caller pid>/ns` as `(name, d_type)`.
pub fn ns_dir_entries() -> Vec<(&'static str, u8)> {
    NS_LINKS.iter().map(|(name, _)| (*name, DT_LNK)).collect()
}

pub fn is_virtual_file(node: ProcNode) -> bool {
    matches!(
        node,
        ProcNode::MeminfoFile
            | ProcNode::FilesystemsFile
            | ProcNode::MountsFile
            | ProcNode::UptimeFile
    )
}

pub fn format_meminfo(snapshot: &MemInfoSnapshot) -> Vec<u8> {
    let total_kb = snapshot.mem_total_bytes / 1024;
    let free_kb = snapshot.mem_free_bytes / 1024;
    let buffers_kb = snapshot.buffers_bytes / 1024;
    let swap_total_kb = snapshot.swap_total_bytes / 1024;
    let swap_free_kb = snapshot.swap_free_bytes / 1024;
    // Fixed 8-digit right alignment keeps file byte length invariant across
    // open/read/replay for systems under ~95 GiB.
    format!(
        "MemTotal:       {:>8} kB\n\
         MemFree:        {:>8} kB\n\
         MemAvailable:   {:>8} kB\n\
         Buffers:        {:>8} kB\n\
         Cached:         {:>8} kB\n\
         SwapTotal:      {:>8} kB\n\
         SwapFree:       {:>8} kB\n",
        total_kb, free_kb, free_kb, buffers_kb, 0_u64, swap_total_kb, swap_free_kb
    )
    .into_bytes()
}

pub fn format_filesystems() -> &'static [u8] {
    b"nodev\tsysfs\nnodev\tproc\nnodev\tdevtmpfs\nnodev\ttmpfs\n\tvfat\n"
}

/// `/proc/mounts` from the live mount table: boot-volume mounts first, then
/// the fixed virtual filesystems, then the memory-filesystem mounts.
pub fn format_mounts(mounts: &[crate::mount_table::MountInfo<'_>]) -> Vec<u8> {
    let mut out = String::new();
    let row = |out: &mut String, info: &crate::mount_table::MountInfo<'_>| {
        let source = match (info.fs_type, info.mount_point) {
            ("vfat", "/") => "boot",
            ("vfat", _) => "data",
            (fs_type, _) => fs_type,
        };
        let options = match (info.fs_type, info.read_only) {
            ("vfat", true) => "ro",
            ("vfat", false) => "rw",
            (_, true) => "ro,nosuid,nodev",
            (_, false) => "rw,nosuid,nodev",
        };
        out.push_str(&format!(
            "{source} {} {} {options} 0 0\n",
            info.mount_point, info.fs_type
        ));
    };
    for info in mounts.iter().filter(|info| info.fs_type == "vfat") {
        row(&mut out, info);
    }
    out.push_str("devtmpfs /dev devtmpfs rw,nosuid 0 0\n");
    out.push_str("proc /proc proc ro,nosuid,nodev,noexec 0 0\n");
    out.push_str("sysfs /sys sysfs ro,nosuid,nodev,noexec 0 0\n");
    for info in mounts.iter().filter(|info| info.fs_type != "vfat") {
        row(&mut out, info);
    }
    out.into_bytes()
}

/// Formats kernel monotonic uptime in `/proc/uptime` centisecond precision.
pub fn format_uptime(monotonic_nanos: u64) -> Vec<u8> {
    let cs = monotonic_nanos / 10_000_000;
    format!("{}.{:02} 0.00\n", cs / 100, cs % 100).into_bytes()
}

/// Formats an online/possible CPU bitmask into Linux sysfs cpulist syntax.
pub fn format_cpulist(cpu_mask: u64) -> Vec<u8> {
    let mut out = String::new();
    let mut bit = 0_u32;
    while bit < 64 {
        if (cpu_mask >> bit) & 1 == 0 {
            bit += 1;
            continue;
        }
        let start = bit;
        while bit + 1 < 64 && (cpu_mask >> (bit + 1)) & 1 != 0 {
            bit += 1;
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == bit {
            out.push_str(&format!("{start}"));
        } else {
            out.push_str(&format!("{start}-{bit}"));
        }
        bit += 1;
    }
    out.push('\n');
    out.into_bytes()
}

/// Classifies synthetic read-only `/sys` regular files.
pub fn sys_virtual_file(path: &str) -> Option<SysFile> {
    match path {
        "/sys/devices/system/cpu/online" => Some(SysFile::CpuOnline),
        "/sys/devices/system/cpu/possible" => Some(SysFile::CpuPossible),
        "/sys/devices/system/cpu/present" => Some(SysFile::CpuPresent),
        _ => None,
    }
}

/// Returns whether `path` is a read-only synthetic `/sys` directory and its
/// static child entry names.
pub fn sys_dir_children(path: &str) -> Option<&'static [&'static str]> {
    Some(match path {
        "/sys" => &["class", "devices", "fs", "kernel"],
        "/sys/devices" => &["system"],
        "/sys/devices/system" => &["cpu"],
        "/sys/devices/system/cpu" => &["online", "possible", "present"],
        "/sys/class" | "/sys/fs" | "/sys/kernel" => &[],
        _ => return None,
    })
}

/// Strict decimal pid: no sign, no leading zeros, no empty string.
fn parse_pid(name: &str) -> Option<u64> {
    if name.is_empty() || (name.len() > 1 && name.starts_with('0')) {
        return None;
    }
    if !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

/// Link target for a symlink node. `exec_path` is the kernel-attested path of
/// the caller; a non-absolute or empty value is reported as ENOENT instead of
/// returning a misleading target.
pub fn link_target(node: ProcNode, caller_pid: u64, exec_path: &[u8]) -> Result<Vec<u8>, i32> {
    match node {
        ProcNode::SelfLink => Ok(u64_decimal(caller_pid)),
        ProcNode::ExeLink => {
            if exec_path.first() != Some(&b'/') {
                return Err(ENOENT);
            }
            Ok(exec_path.to_vec())
        }
        ProcNode::NsLink(index) => {
            let (kind, id) = NS_LINKS[index];
            Ok(format!("{kind}:[{id}]").into_bytes())
        }
        ProcNode::PidDir
        | ProcNode::NsDir
        | ProcNode::Kernel(..)
        | ProcNode::MeminfoFile
        | ProcNode::FilesystemsFile
        | ProcNode::MountsFile
        | ProcNode::UptimeFile => Err(EINVAL),
    }
}

/// Executable path attached by the kernel in a request payload: the bytes up
/// to the first NUL, bounded by `payload_len`.
pub fn attached_exec_path(payload: &[u8], payload_len: usize) -> &[u8] {
    let payload = &payload[..payload_len.min(payload.len())];
    let end = payload
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(payload.len());
    &payload[..end]
}

fn u64_decimal(mut value: u64) -> Vec<u8> {
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
    digits[at..].to_vec()
}

fn node_inode(node: ProcNode, caller_pid: u64) -> u64 {
    if let ProcNode::Kernel(kernel, number) = node {
        return abi_procfs::node_inode(caller_pid, kernel, number);
    }
    let pid = u64_decimal(caller_pid);
    let mut name = String::from("/proc/");
    match node {
        ProcNode::SelfLink => name.push_str("self"),
        ProcNode::PidDir => name.push_str(core::str::from_utf8(&pid).unwrap_or("")),
        ProcNode::ExeLink => {
            name.push_str(core::str::from_utf8(&pid).unwrap_or(""));
            name.push_str("/exe");
        }
        ProcNode::MeminfoFile => name.push_str("meminfo"),
        ProcNode::FilesystemsFile => name.push_str("filesystems"),
        ProcNode::MountsFile => name.push_str("mounts"),
        ProcNode::UptimeFile => name.push_str("uptime"),
        ProcNode::NsDir => {
            name.push_str(core::str::from_utf8(&pid).unwrap_or(""));
            name.push_str("/ns");
        }
        ProcNode::NsLink(index) => {
            name.push_str(core::str::from_utf8(&pid).unwrap_or(""));
            name.push_str("/ns/");
            name.push_str(NS_LINKS[index].0);
        }
        ProcNode::Kernel(..) => {}
    }
    path_inode(name.as_bytes())
}

fn node_mode(node: ProcNode) -> u32 {
    match node {
        ProcNode::PidDir | ProcNode::NsDir => PID_DIR_MODE,
        ProcNode::Kernel(kernel, _) => kernel.mode(),
        ProcNode::SelfLink | ProcNode::ExeLink | ProcNode::NsLink(_) => SYMLINK_MODE,
        ProcNode::MeminfoFile
        | ProcNode::FilesystemsFile
        | ProcNode::MountsFile
        | ProcNode::UptimeFile => REG_READONLY_MODE,
    }
}

fn node_nlink(node: ProcNode) -> u32 {
    match node {
        // `/proc/<pid>` contains `.`, `..`, `fd`, `ns`, and `task`.
        ProcNode::PidDir => 5,
        ProcNode::NsDir => 2,
        ProcNode::Kernel(ProcKernelNode::TaskDir, _) => 3,
        ProcNode::Kernel(kernel, _) if kernel.is_directory() => 2,
        _ => 1,
    }
}

/// Stat of a node without following it. `size` is the link-target length for
/// symlinks, zero for directories.
pub fn node_stat(node: ProcNode, caller_pid: u64, size: u64) -> [u8; LINUX_STAT_SIZE] {
    let stat = LinuxStat {
        st_ino: node_inode(node, caller_pid),
        st_nlink: node_nlink(node).into(),
        st_mode: node_mode(node),
        st_size: size.min(i64::MAX as u64) as i64,
        st_blksize: i64::from(BLOCK_SIZE),
        ..LinuxStat::default()
    };
    let mut bytes = [0_u8; LINUX_STAT_SIZE];
    bytes.copy_from_slice(struct_bytes(&stat));
    bytes
}

pub fn node_statx(node: ProcNode, caller_pid: u64, size: u64) -> [u8; LINUX_STATX_SIZE] {
    let statx = LinuxStatx {
        stx_mask: 0x7ff,
        stx_blksize: BLOCK_SIZE,
        stx_nlink: node_nlink(node),
        stx_mode: node_mode(node) as u16,
        stx_ino: node_inode(node, caller_pid),
        stx_size: size,
        ..LinuxStatx::default()
    };
    let mut bytes = [0_u8; LINUX_STATX_SIZE];
    bytes.copy_from_slice(struct_bytes(&statx));
    bytes
}

/// Stat of the namespace object a `ns/<kind>` link resolves to (an nsfs
/// regular file whose inode number is the namespace id).
pub fn ns_target_stat(index: usize) -> [u8; LINUX_STAT_SIZE] {
    let stat = LinuxStat {
        st_ino: NS_LINKS[index].1,
        st_nlink: 1,
        st_mode: REG_READONLY_MODE,
        st_blksize: i64::from(BLOCK_SIZE),
        ..LinuxStat::default()
    };
    let mut bytes = [0_u8; LINUX_STAT_SIZE];
    bytes.copy_from_slice(struct_bytes(&stat));
    bytes
}

pub fn ns_target_statx(index: usize) -> [u8; LINUX_STATX_SIZE] {
    let statx = LinuxStatx {
        stx_mask: 0x7ff,
        stx_blksize: BLOCK_SIZE,
        stx_nlink: 1,
        stx_mode: REG_READONLY_MODE as u16,
        stx_ino: NS_LINKS[index].1,
        ..LinuxStatx::default()
    };
    let mut bytes = [0_u8; LINUX_STATX_SIZE];
    bytes.copy_from_slice(struct_bytes(&statx));
    bytes
}

fn struct_bytes<T>(value: &T) -> &[u8] {
    // SAFETY: only `#[repr(C)]` plain-data wire structs are passed in.
    unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::normalize_absolute_path;

    #[test]
    fn proc_root_and_foreign_paths_are_not_handled_here() {
        assert_eq!(lookup("/proc", 7), None);
        assert_eq!(lookup("/procfs/x", 7), None);
        assert_eq!(lookup("/etc/passwd", 7), None);
    }

    #[test]
    fn self_and_own_pid_resolve() {
        assert_eq!(lookup("/proc/self", 7), Some(Ok(ProcNode::SelfLink)));
        assert_eq!(lookup("/proc/self/exe", 7), Some(Ok(ProcNode::ExeLink)));
        assert_eq!(lookup("/proc/7", 7), Some(Ok(ProcNode::PidDir)));
        assert_eq!(lookup("/proc/7/exe", 7), Some(Ok(ProcNode::ExeLink)));
    }

    #[test]
    fn other_or_malformed_pids_are_enoent() {
        assert_eq!(lookup("/proc/8", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/8/exe", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/07/exe", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/+7", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/1/exe", 0), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/meminfo", 7), Some(Ok(ProcNode::MeminfoFile)));
        assert_eq!(
            lookup("/proc/filesystems", 7),
            Some(Ok(ProcNode::FilesystemsFile))
        );
        assert_eq!(lookup("/proc/mounts", 7), Some(Ok(ProcNode::MountsFile)));
        assert_eq!(
            lookup("/proc/self/mounts", 7),
            Some(Ok(ProcNode::MountsFile))
        );
        assert_eq!(lookup("/proc/7/mounts", 7), Some(Ok(ProcNode::MountsFile)));
        assert_eq!(lookup("/proc/cpuinfo", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/self/exe/x", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/self/mem", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/self/fd/abc", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/self/fd/03", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/self/fd/3/x", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/self/task/9/maps", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/self/ns/bogus", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/8/maps", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/8/fd/1", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/8/task/8/stat", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/8/ns/net", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/self/a/b/c/d/e", 7), Some(Err(ENOENT)));
    }

    #[test]
    fn kernel_fact_nodes_resolve_for_the_caller_only() {
        let k = |node, n| Some(Ok(ProcNode::Kernel(node, n)));
        for (name, node) in SELF_FILE_NODES {
            assert_eq!(lookup(&format!("/proc/self/{name}"), 7), k(node, 0));
            assert_eq!(lookup(&format!("/proc/7/{name}"), 7), k(node, 0));
        }
        assert_eq!(lookup("/proc/self/fd", 7), k(ProcKernelNode::FdDir, 0));
        assert_eq!(lookup("/proc/7/fd/12", 7), k(ProcKernelNode::FdLink, 12));
        assert_eq!(lookup("/proc/self/task", 7), k(ProcKernelNode::TaskDir, 0));
        assert_eq!(
            lookup("/proc/self/task/41", 7),
            k(ProcKernelNode::TaskTidDir, 41)
        );
        assert_eq!(
            lookup("/proc/self/task/41/stat", 7),
            k(ProcKernelNode::TaskStat, 41)
        );
        assert_eq!(
            lookup("/proc/self/task/41/comm", 7),
            k(ProcKernelNode::TaskComm, 41)
        );
        assert_eq!(
            lookup("/proc/self/task/41/status", 7),
            k(ProcKernelNode::TaskStatus, 41)
        );
        assert_eq!(lookup("/proc/self/ns", 7), Some(Ok(ProcNode::NsDir)));
        assert!(matches!(
            lookup("/proc/self/ns/net", 7),
            Some(Ok(ProcNode::NsLink(_)))
        ));
    }

    #[test]
    fn delegation_is_open_for_files_and_total_for_kernel_only_subtrees() {
        let maps = lookup("/proc/self/maps", 7).unwrap().unwrap();
        let fd = lookup("/proc/self/fd/4", 7).unwrap().unwrap();
        let pid_dir = lookup("/proc/self/fd", 7).unwrap().unwrap();
        assert_eq!(kernel_only_delegate(maps), None);
        assert_eq!(
            abi_procfs::decode_delegate(open_delegate(maps).unwrap()),
            Some((ProcKernelNode::Maps, 0))
        );
        assert_eq!(
            abi_procfs::decode_delegate(kernel_only_delegate(fd).unwrap()),
            Some((ProcKernelNode::FdLink, 4))
        );
        assert!(kernel_only_delegate(pid_dir).is_some());
        assert_eq!(open_delegate(ProcNode::PidDir), None);
        assert_eq!(open_delegate(ProcNode::MountsFile), None);
    }

    #[test]
    fn delegated_open_flag_rules() {
        let maps = ProcNode::Kernel(ProcKernelNode::Maps, 0);
        let fd = ProcNode::Kernel(ProcKernelNode::FdLink, 3);
        let dir = ProcNode::Kernel(ProcKernelNode::FdDir, 0);
        assert_eq!(delegated_open_errno(maps, 0), None);
        assert_eq!(delegated_open_errno(maps, 0o2_000_000), None); // O_CLOEXEC
        assert_eq!(delegated_open_errno(maps, 1), Some(EACCES));
        assert_eq!(delegated_open_errno(maps, 2), Some(EACCES));
        assert_eq!(delegated_open_errno(maps, O_TRUNC), Some(EACCES));
        assert_eq!(delegated_open_errno(maps, O_DIRECTORY), Some(ENOTDIR));
        assert_eq!(delegated_open_errno(dir, O_DIRECTORY), None);
        assert_eq!(delegated_open_errno(fd, O_NOFOLLOW), Some(ELOOP));
        assert_eq!(delegated_open_errno(fd, 0), None);
        assert_eq!(delegated_open_errno(ProcNode::PidDir, 1), None);

        for writable in [
            ProcNode::Kernel(ProcKernelNode::Comm, 0),
            ProcNode::Kernel(ProcKernelNode::TaskComm, 7),
            ProcNode::Kernel(ProcKernelNode::OomScoreAdj, 0),
        ] {
            assert_eq!(delegated_open_errno(writable, 0), None);
            assert_eq!(delegated_open_errno(writable, 1), None);
            assert_eq!(delegated_open_errno(writable, 2), None);
            assert_eq!(delegated_open_errno(writable, 1 | O_TRUNC), None);
            assert_eq!(delegated_open_errno(writable, 1 | O_CREAT), Some(EACCES));
            assert_eq!(delegated_open_errno(writable, O_DIRECTORY), Some(ENOTDIR));
        }
    }

    #[test]
    fn pid_dir_and_ns_listings_are_sorted_and_typed() {
        let entries = pid_dir_entries();
        let names: Vec<&str> = entries.iter().map(|(name, _)| *name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        let kind = |wanted: &str| entries.iter().find(|(n, _)| *n == wanted).map(|(_, t)| *t);
        assert_eq!(kind("maps"), Some(DT_REG));
        assert_eq!(kind("fd"), Some(DT_DIR));
        assert_eq!(kind("task"), Some(DT_DIR));
        assert_eq!(kind("ns"), Some(DT_DIR));
        assert_eq!(kind("exe"), Some(DT_LNK));
        assert_eq!(kind("mounts"), Some(DT_REG));
        assert_eq!(kind("oom_score_adj"), Some(DT_REG));
        assert_eq!(entries.len(), 5 + SELF_FILE_NODES.len());
        let ns = ns_dir_entries();
        assert_eq!(ns.len(), NS_LINKS.len());
        assert!(ns.iter().all(|(_, t)| *t == DT_LNK));
    }

    #[test]
    fn ns_links_read_back_as_kind_and_constant_id() {
        let index = NS_LINKS.iter().position(|(n, _)| *n == "net").unwrap();
        let target = link_target(ProcNode::NsLink(index), 7, b"").unwrap();
        assert_eq!(target, format!("net:[{}]", NS_LINKS[index].1).into_bytes());
        // The nsfs target is a regular file whose inode is the namespace id.
        let stat = ns_target_stat(index);
        let ino = u64::from_le_bytes(stat[8..16].try_into().unwrap());
        assert_eq!(ino, NS_LINKS[index].1);
        let mode = u32::from_le_bytes(stat[24..28].try_into().unwrap());
        assert_eq!(mode & 0o170_000, S_IFREG);
        // Same id for every caller pid: there is one namespace per kind.
        assert_eq!(
            link_target(ProcNode::NsLink(index), 99, b"").unwrap(),
            target
        );
    }

    #[test]
    fn kernel_nodes_stat_as_readonly_size_zero_files() {
        let maps = ProcNode::Kernel(ProcKernelNode::Maps, 0);
        let stat = node_stat(maps, 7, 0);
        let mode = u32::from_le_bytes(stat[24..28].try_into().unwrap());
        assert_eq!(mode, S_IFREG | 0o444);
        let size = i64::from_le_bytes(stat[48..56].try_into().unwrap());
        assert_eq!(size, 0);
        let fd_dir = node_stat(ProcNode::Kernel(ProcKernelNode::FdDir, 0), 7, 0);
        assert_eq!(
            u32::from_le_bytes(fd_dir[24..28].try_into().unwrap()) & 0o170_000,
            S_IFDIR
        );
        // Distinct nodes and numbers get distinct inodes.
        let ino = |n: ProcNode| u64::from_le_bytes(node_stat(n, 7, 0)[8..16].try_into().unwrap());
        assert_ne!(ino(maps), ino(ProcNode::Kernel(ProcKernelNode::Smaps, 0)));
        assert_ne!(
            ino(ProcNode::Kernel(ProcKernelNode::TaskStat, 8)),
            ino(ProcNode::Kernel(ProcKernelNode::TaskStat, 9))
        );
    }

    #[test]
    fn meminfo_filesystems_mounts_and_sys_format() {
        let mem = String::from_utf8(format_meminfo(&MemInfoSnapshot {
            mem_total_bytes: 512 * 1024 * 1024,
            mem_free_bytes: 256 * 1024 * 1024,
            buffers_bytes: 0,
            swap_total_bytes: 0,
            swap_free_bytes: 0,
        }))
        .unwrap();
        assert!(mem.contains("MemTotal:         524288 kB\n"));
        assert!(mem.contains("MemFree:          262144 kB\n"));
        assert!(mem.contains("MemAvailable:     262144 kB\n"));
        assert!(mem.contains("SwapTotal:             0 kB\n"));

        let fs = core::str::from_utf8(format_filesystems()).unwrap();
        assert!(fs.contains("nodev\tsysfs\n"));
        assert!(fs.contains("nodev\tproc\n"));
        assert!(fs.contains("\tvfat\n"));

        let mounts_no_boot = String::from_utf8(format_mounts(&[])).unwrap();
        assert!(!mounts_no_boot.contains("vfat"));
        assert!(mounts_no_boot.contains("proc /proc proc"));
        assert!(mounts_no_boot.contains("sysfs /sys sysfs"));
        use crate::mount_table::MountInfo;
        let info = |mount_point, fs_type, read_only| MountInfo {
            mount_point,
            fs_type,
            read_only,
        };
        let live = [
            info("/", "vfat", true),
            info("/data", "vfat", false),
            info("/dev/shm", "tmpfs", false),
            info("/run", "tmpfs", false),
            info("/tmp", "tmpfs", false),
        ];
        let mounts = String::from_utf8(format_mounts(&live)).unwrap();
        assert!(mounts.starts_with("boot / vfat ro 0 0\ndata /data vfat rw 0 0\n"));
        assert!(mounts.contains("tmpfs /tmp tmpfs rw,nosuid,nodev 0 0\n"));
        assert!(mounts.contains("tmpfs /dev/shm tmpfs rw,nosuid,nodev 0 0\n"));
        assert!(mounts.contains("tmpfs /run tmpfs rw,nosuid,nodev 0 0\n"));
        assert_eq!(mounts.matches("vfat").count(), 2);

        assert_eq!(
            sys_dir_children("/sys"),
            Some(&["class", "devices", "fs", "kernel"][..])
        );
        assert_eq!(sys_dir_children("/sys/devices"), Some(&["system"][..]));
        assert_eq!(
            sys_dir_children("/sys/devices/system/cpu"),
            Some(&["online", "possible", "present"][..])
        );
        assert_eq!(sys_dir_children("/sys/kernel"), Some(&[][..]));
        assert_eq!(sys_dir_children("/sys/nonexistent"), None);
        assert_eq!(
            sys_virtual_file("/sys/devices/system/cpu/online"),
            Some(SysFile::CpuOnline)
        );
        assert_eq!(sys_virtual_file("/sys/devices/system/cpu"), None);
        assert_eq!(format_cpulist(0b1), b"0\n");
        assert_eq!(format_cpulist(0b1111), b"0-3\n");
        assert_eq!(format_cpulist(0b1101), b"0,2-3\n");
        assert_eq!(lookup("/proc/uptime", 7), Some(Ok(ProcNode::UptimeFile)));
        assert_eq!(format_uptime(12_340_000_000), b"12.34 0.00\n");

        let stat = node_stat(ProcNode::MeminfoFile, 7, mem.len() as u64);
        let mode = u32::from_le_bytes(stat[24..28].try_into().unwrap());
        assert_eq!(mode & 0o170_000, S_IFREG);
    }

    #[test]
    fn canonicalization_removes_slash_and_dotdot_tricks() {
        let norm = |p: &str| normalize_absolute_path("/", p).unwrap();
        assert_eq!(
            lookup(&norm("/proc/self/exe/"), 7),
            Some(Ok(ProcNode::ExeLink))
        );
        assert_eq!(
            lookup(&norm("/proc//self//exe"), 7),
            Some(Ok(ProcNode::ExeLink))
        );
        assert_eq!(
            lookup(&norm("/proc/./7/../self/exe"), 7),
            Some(Ok(ProcNode::ExeLink))
        );
        // `..` out of the caller's directory lands on another pid or outside.
        assert_eq!(lookup(&norm("/proc/self/../8/exe"), 7), Some(Err(ENOENT)));
        assert_eq!(lookup(&norm("/proc/self/../../etc"), 7), None);
        assert_eq!(lookup(&norm("/proc/self/.."), 7), None);
    }

    #[test]
    fn link_targets() {
        assert_eq!(link_target(ProcNode::SelfLink, 4242, b"").unwrap(), b"4242");
        assert_eq!(link_target(ProcNode::SelfLink, 0, b"").unwrap(), b"0");
        assert_eq!(
            link_target(ProcNode::ExeLink, 7, b"/apps/chrome/chrome").unwrap(),
            b"/apps/chrome/chrome"
        );
        assert_eq!(link_target(ProcNode::ExeLink, 7, b"chrome"), Err(ENOENT));
        assert_eq!(link_target(ProcNode::ExeLink, 7, b""), Err(ENOENT));
        assert!(link_target(ProcNode::PidDir, 7, b"/x").is_err());
    }

    #[test]
    fn attached_path_is_bounded_and_nul_terminated() {
        let mut payload = [0_u8; 16];
        payload[..4].copy_from_slice(b"/a/b");
        assert_eq!(attached_exec_path(&payload, 16), b"/a/b");
        assert_eq!(attached_exec_path(&payload, 2), b"/a");
        assert_eq!(attached_exec_path(&payload, 0), b"");
        assert_eq!(attached_exec_path(&payload, 999), b"/a/b");
    }

    #[test]
    fn stat_reports_symlink_and_directory_modes() {
        let stat = node_stat(ProcNode::ExeLink, 7, 19);
        let mode = u32::from_le_bytes(stat[24..28].try_into().unwrap());
        assert_eq!(mode & 0o170_000, S_IFLNK);
        let size = i64::from_le_bytes(stat[48..56].try_into().unwrap());
        assert_eq!(size, 19);
        let dir = node_stat(ProcNode::PidDir, 7, 0);
        let mode = u32::from_le_bytes(dir[24..28].try_into().unwrap());
        assert_eq!(mode & 0o170_000, S_IFDIR);
        let sx = node_statx(ProcNode::SelfLink, 7, 1);
        assert_eq!(
            u32::from(u16::from_le_bytes(sx[28..30].try_into().unwrap())) & 0o170_000,
            S_IFLNK
        );
    }
}
