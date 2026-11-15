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
//! Every other name under `/proc`, including any other pid (live or not), is
//! ENOENT. Unbacked nodes (`/proc/<pid>/fd`, `cmdline`, `status`, `cpuinfo`,
//! `stat`) remain unsupported: a fabricated value would be a fake success path.
//!
//! Paths passed in must already be canonical (see
//! `path::normalize_absolute_path`), so `..`, `.`, empty components and
//! trailing slashes cannot reach the parser.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::size_of;

use rustos_user_abi::syscall::{LINUX_STATX_SIZE, LINUX_STAT_SIZE};

use crate::linux_types::{LinuxStat, LinuxStatx};
use crate::path::path_inode;
use crate::{EINVAL, ENOENT};

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
    let mut parts = rest.split('/');
    let first = parts.next().unwrap_or("");
    let second = parts.next();
    if parts.next().is_some() {
        return Some(Err(ENOENT));
    }
    if second.is_none() {
        match first {
            "self" => return Some(Ok(ProcNode::SelfLink)),
            "meminfo" => return Some(Ok(ProcNode::MeminfoFile)),
            "filesystems" => return Some(Ok(ProcNode::FilesystemsFile)),
            "mounts" => return Some(Ok(ProcNode::MountsFile)),
            _ => {}
        }
    }
    if first != "self" && parse_pid(first) != Some(caller_pid) {
        return Some(Err(ENOENT));
    }
    Some(match second {
        None => Ok(ProcNode::PidDir),
        Some("exe") => Ok(ProcNode::ExeLink),
        Some("mounts") => Ok(ProcNode::MountsFile),
        Some(_) => Err(ENOENT),
    })
}

pub fn is_virtual_file(node: ProcNode) -> bool {
    matches!(
        node,
        ProcNode::MeminfoFile | ProcNode::FilesystemsFile | ProcNode::MountsFile
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

pub fn format_mounts(has_boot_volume: bool) -> Vec<u8> {
    let mut out = String::new();
    if has_boot_volume {
        out.push_str("boot / vfat ro 0 0\n");
    }
    out.push_str("devtmpfs /dev devtmpfs rw,nosuid 0 0\n");
    out.push_str("proc /proc proc ro,nosuid,nodev,noexec 0 0\n");
    out.push_str("sysfs /sys sysfs ro,nosuid,nodev,noexec 0 0\n");
    out.push_str("tmpfs /run tmpfs rw,nosuid,nodev 0 0\n");
    out.into_bytes()
}

/// Returns whether `path` is a read-only synthetic `/sys` directory and its
/// static child directory names.
pub fn sys_dir_children(path: &str) -> Option<&'static [&'static str]> {
    Some(match path {
        "/sys" => &["class", "devices", "fs", "kernel"],
        "/sys/class" | "/sys/devices" | "/sys/fs" | "/sys/kernel" => &[],
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
        ProcNode::PidDir
        | ProcNode::MeminfoFile
        | ProcNode::FilesystemsFile
        | ProcNode::MountsFile => Err(EINVAL),
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
    }
    path_inode(name.as_bytes())
}

fn node_mode(node: ProcNode) -> u32 {
    match node {
        ProcNode::PidDir => PID_DIR_MODE,
        ProcNode::SelfLink | ProcNode::ExeLink => SYMLINK_MODE,
        ProcNode::MeminfoFile | ProcNode::FilesystemsFile | ProcNode::MountsFile => {
            REG_READONLY_MODE
        }
    }
}

/// Stat of a node without following it. `size` is the link-target length for
/// symlinks, zero for directories.
pub fn node_stat(node: ProcNode, caller_pid: u64, size: u64) -> [u8; LINUX_STAT_SIZE] {
    let stat = LinuxStat {
        st_ino: node_inode(node, caller_pid),
        st_nlink: if node == ProcNode::PidDir { 2 } else { 1 },
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
        stx_nlink: if node == ProcNode::PidDir { 2 } else { 1 },
        stx_mode: node_mode(node) as u16,
        stx_ino: node_inode(node, caller_pid),
        stx_size: size,
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
        assert_eq!(lookup("/proc/self/fd", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/self/exe/x", 7), Some(Err(ENOENT)));
        assert_eq!(lookup("/proc/self/cmdline", 7), Some(Err(ENOENT)));
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

        let mounts_no_boot = String::from_utf8(format_mounts(false)).unwrap();
        assert!(!mounts_no_boot.contains("vfat"));
        assert!(mounts_no_boot.contains("proc /proc proc"));
        assert!(mounts_no_boot.contains("sysfs /sys sysfs"));
        let mounts_boot = String::from_utf8(format_mounts(true)).unwrap();
        assert!(mounts_boot.starts_with("boot / vfat ro 0 0\n"));

        assert_eq!(
            sys_dir_children("/sys"),
            Some(&["class", "devices", "fs", "kernel"][..])
        );
        assert_eq!(sys_dir_children("/sys/kernel"), Some(&[][..]));
        assert_eq!(sys_dir_children("/sys/nonexistent"), None);

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
