//! Kernel-attested caller identity for vfsd's `/proc` namespace.
//!
//! vfsd owns the `/proc` policy (which nodes exist, ENOENT for foreign pids)
//! but only the kernel knows the caller's executable path
//! (`UserProcessState::exec_path`). The Ring0 front-end therefore attaches
//! that path, as a mechanism-level fact, to the path-metadata requests that
//! can name `/proc/<pid>/exe` (`readlinkat`, `newfstatat`, `statx`). vfsd uses
//! it only for the caller's own pid, so no other process's path is exposed.

use super::*;

/// Copies `exec_path` into `payload`, NUL-free and bounded; returns the number
/// of bytes copied. A path longer than the payload is dropped entirely rather
/// than truncated, so vfsd can never return a wrong-but-plausible link target.
pub(crate) fn copy_exec_path(payload: &mut [u8], exec_path: &str) -> usize {
    let bytes = exec_path.as_bytes();
    if bytes.len() > payload.len() || bytes.contains(&0) {
        return 0;
    }
    payload[..bytes.len()].copy_from_slice(bytes);
    bytes.len()
}

/// Attaches the calling process's executable path to `request.payload`.
pub(crate) fn attach_caller_exec_path(request: &mut VfsIpcRequest) {
    let len = multitask::with_current_user_process_state(|_, _, process_state| {
        copy_exec_path(&mut request.payload, process_state.exec_path())
    })
    .unwrap_or(0);
    request.payload_len = len as u32;
}

// ---- kernel-fact `/proc/<own pid>` nodes ----------------------------------------------------
//
// vfsd owns the namespace and answers `status 0` with a delegation in
// `VfsIpcResponse::aux` (see `rustos_user_abi::procfs`) for every node whose
// content is a kernel fact. This section is the Ring0 completion of that
// delegation for the CALLING process only (vfsd already refused every other
// pid). Nothing here decides policy: it formats facts the kernel attests and
// holds them in a frozen, unmappable memfd snapshot taken at `open`.

use alloc::string::String;
use alloc::vec::Vec;
use kernel_object::api::handle::{FileHandleRights, HandleRights};
use rustos_user_abi::procfs::{self as abi_procfs, ProcKernelNode as Node};

use super::anon_stat::{self, AnonStat};
use super::proc_snapshot as snap;

const LINUX_ENXIO_ERRNO: i64 = 6;
const W_OK: u64 = 2;
const X_OK: u64 = 1;
const AT_SYMLINK_NOFOLLOW_FLAG: u64 = 0x100;
/// One clock tick is 10 ms (`USER_HZ` 100).
const NS_PER_TICK: u64 = 1_000_000_000 / snap::CLOCK_TICKS_PER_SEC;

/// Where a delegated `stat` result goes.
#[derive(Clone, Copy)]
pub(crate) enum StatOut {
    Stat { ptr: u64 },
    Statx { ptr: u64, mask: u64 },
}

fn caller_pid() -> Result<u64, i64> {
    multitask::current_user_process_id().ok_or(LINUX_ESRCH)
}

fn tid_is_live(pid: u64, tid: u64) -> bool {
    multitask::live_user_thread_ids(pid).contains(&tid)
}

fn node_link(pid: u64, node: Node, number: u64) -> String {
    let mut link = String::new();
    let _ = abi_procfs::write_node_path(&mut link, pid, node, number);
    link
}

// ---- fact gathering -----------------------------------------------------------------------

fn trimmed_comm(name: [u8; rustos_user_abi::linux::LINUX_TASK_COMM_LEN]) -> Vec<u8> {
    name.iter().copied().take_while(|&byte| byte != 0).collect()
}

fn exec_path() -> String {
    multitask::with_current_user_process_state(|_, _, state| String::from(state.exec_path()))
        .unwrap_or_default()
}

fn caller_thread(tid: u64) -> snap::ThreadFacts {
    let state = multitask::current_linux_thread_state();
    let stored = state
        .as_ref()
        .map_or([0; rustos_user_abi::linux::LINUX_TASK_COMM_LEN], |s| s.comm);
    let name = crate::user::syscall::linux::prctl_ops::effective_name(&stored, &exec_path());
    snap::ThreadFacts {
        tid,
        comm: trimmed_comm(name),
        state: b'R',
        pending_signals: state.as_ref().map_or(0, |s| s.pending_signals),
        blocked_signals: state.as_ref().map_or(0, |s| s.signal_mask),
        utime_ticks: multitask::current_scheduling_context_runtime_snapshot()
            .map(|runtime| runtime.context_consumed_ns / NS_PER_TICK),
    }
}

fn other_thread(pid: u64, tid: u64) -> Option<snap::ThreadFacts> {
    let snapshot = multitask::linux_thread_snapshot_by_ids(pid, tid)?;
    let name = crate::user::syscall::linux::prctl_ops::effective_name(
        &snapshot.thread_state.comm,
        &exec_path(),
    );
    Some(snap::ThreadFacts {
        tid,
        comm: trimmed_comm(name),
        // The kernel does not expose another thread's run state or CPU time
        // here, so it is reported as sleeping with none.
        state: b'S',
        pending_signals: snapshot.thread_state.pending_signals,
        blocked_signals: snapshot.thread_state.signal_mask,
        utime_ticks: None,
    })
}

fn thread_facts(pid: u64, caller_tid: u64, tid: u64) -> Result<snap::ThreadFacts, i64> {
    if tid == caller_tid {
        return Ok(caller_thread(tid));
    }
    if !tid_is_live(pid, tid) {
        return Err(LINUX_ENOENT);
    }
    other_thread(pid, tid).ok_or(LINUX_ENOENT)
}

fn gather_process(pid: u64) -> Result<snap::ProcessFacts, i64> {
    use rustos_user_abi::linux::LinuxVmaName;
    use rustos_user_abi::pager::{VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE, VM_SHARING_SHARED};
    // Read before taking the process-state lock: the pager snapshot must not
    // nest inside it.
    let pager = multitask::pager_vma_regions_for_process(pid).unwrap_or_default();
    let ppid = multitask::parent_process_id_of(pid).unwrap_or(0);
    let threads = multitask::current_user_process_thread_count().unwrap_or(1) as u64;
    let mut facts = multitask::with_current_user_process_state(|_, _, state| {
        let mut regions = Vec::new();
        if let Some(map) = state.linux_memory_map() {
            for area in map.areas() {
                regions.push(snap::MapRegion {
                    start: area.start,
                    end: area.end,
                    read: area.flags.read,
                    write: area.flags.write,
                    exec: area.flags.execute,
                    shared: !area.flags.private,
                    offset: area.offset,
                    name: match &area.name {
                        LinuxVmaName::None => None,
                        LinuxVmaName::Path(path) => Some(path.clone()),
                        LinuxVmaName::Label(label) => Some(String::from(*label)),
                    },
                });
            }
        }
        let security = state.security();
        let (brk_start, startup) = state
            .linux_process_state()
            .map(|linux| (linux.brk_start, linux.startup))
            .unwrap_or_default();
        let (mut ignored, mut caught) = (0_u64, 0_u64);
        for signal in 1..=64_u64 {
            if let Some(action) = state.linux_signal_action(signal) {
                match action.handler {
                    0 => {}
                    1 => ignored |= 1 << (signal - 1),
                    _ => caught |= 1 << (signal - 1),
                }
            }
        }
        let rss = crate::memory::phys::data_leaf_count(state.address_space().root_phys().as_u64());
        snap::ProcessFacts {
            pid,
            ppid,
            uid: security.uid(),
            gid: security.gid(),
            euid: security.euid(),
            egid: security.egid(),
            threads,
            start_ticks: state.start_ns() / NS_PER_TICK,
            rss_pages: rss,
            locked_bytes: state.memlock().locked_bytes(),
            regions,
            brk_start,
            arg_start: startup.arg_start,
            arg_end: startup.arg_end,
            env_start: startup.env_start,
            env_end: startup.env_end,
            sig_ignored: ignored,
            sig_caught: caught,
            no_new_privs: security.no_new_privs(),
        }
    })
    .ok_or(LINUX_ESRCH)?;
    // Pager-backed mappings are anonymous or file-backed without a recorded
    // path; skip any that the Linux map already describes.
    for region in pager {
        let overlaps = facts
            .regions
            .iter()
            .any(|known| region.start < known.end && known.start < region.end);
        if overlaps {
            continue;
        }
        facts.regions.push(snap::MapRegion {
            start: region.start,
            end: region.end,
            read: region.prot & VM_PROT_READ != 0,
            write: region.prot & VM_PROT_WRITE != 0,
            exec: region.prot & VM_PROT_EXECUTE != 0,
            shared: region.sharing == VM_SHARING_SHARED,
            offset: region.object_offset,
            name: None,
        });
    }
    facts.regions = snap::merge_regions(core::mem::take(&mut facts.regions));
    Ok(facts)
}

fn read_user_range(start: u64, end: u64) -> Result<Vec<u8>, i64> {
    let Some(len) = snap::user_range_len(start, end) else {
        return Ok(Vec::new());
    };
    if len > abi_procfs::PROC_SNAPSHOT_MAX_BYTES {
        return Err(LINUX_EOVERFLOW);
    }
    let mut buffer = alloc::vec![0_u8; len];
    usermem::copy_from_current_user_exact(start, &mut buffer)
        .map_err(user_pointer_error_to_linux_errno)?;
    Ok(buffer)
}

fn complete_snapshot(bytes: Vec<u8>) -> Result<Vec<u8>, i64> {
    if bytes.len() > abi_procfs::PROC_SNAPSHOT_MAX_BYTES {
        Err(LINUX_EOVERFLOW)
    } else {
        Ok(bytes)
    }
}

fn gather_limits() -> [Option<(u64, u64)>; 10] {
    let mut limits = [None; 10];
    for (resource, slot) in limits.iter_mut().enumerate() {
        *slot = crate::user::syscall::linux::syscalld_ops::current_rlimit(resource as u64).ok();
    }
    limits
}

/// Names of a generated directory. `numbers` are the fd or tid members.
fn dir_names(pid: u64, node: Node, numbers: &[u64]) -> Vec<snap::DirName> {
    let own = abi_procfs::node_inode(pid, node, 0);
    let parent = abi_procfs::pid_dir_inode(pid);
    let mut names = alloc::vec![
        snap::DirName { ino: own, d_type: abi_procfs::DT_DIR, name: b".".to_vec() },
        snap::DirName { ino: parent, d_type: abi_procfs::DT_DIR, name: b"..".to_vec() },
    ];
    for &number in numbers {
        let mut decimal = String::new();
        let _ = core::fmt::Write::write_fmt(&mut decimal, format_args!("{number}"));
        let (ino, d_type) = match node {
            Node::FdDir => (abi_procfs::node_inode(pid, Node::FdLink, number), abi_procfs::DT_LNK),
            _ => (abi_procfs::node_inode(pid, Node::TaskTidDir, number), abi_procfs::DT_DIR),
        };
        names.push(snap::DirName { ino, d_type, name: decimal.into_bytes() });
    }
    names
}

fn task_tid_names(pid: u64, tid: u64) -> Vec<snap::DirName> {
    let mut names = alloc::vec![
        snap::DirName {
            ino: abi_procfs::node_inode(pid, Node::TaskTidDir, tid),
            d_type: abi_procfs::DT_DIR,
            name: b".".to_vec(),
        },
        snap::DirName {
            ino: abi_procfs::node_inode(pid, Node::TaskDir, 0),
            d_type: abi_procfs::DT_DIR,
            name: b"..".to_vec(),
        },
    ];
    for (name, member) in abi_procfs::TASK_FILE_NODES {
        names.push(snap::DirName {
            ino: abi_procfs::node_inode(pid, member, tid),
            d_type: abi_procfs::DT_REG,
            name: name.as_bytes().to_vec(),
        });
    }
    names
}

/// Content of the node as of now; oversized snapshots fail rather than truncate.
fn render(node: Node, number: u64, pid: u64) -> Result<Vec<u8>, i64> {
    let caller_tid = multitask::current_user_thread_id().ok_or(LINUX_ESRCH)?;
    let bytes = match node {
        Node::Maps => snap::format_maps(&gather_process(pid)?.regions),
        Node::Smaps => snap::format_smaps(&gather_process(pid)?.regions),
        Node::Status => snap::format_status(&gather_process(pid)?, &caller_thread(caller_tid)),
        Node::Stat => snap::format_stat(&gather_process(pid)?, &caller_thread(caller_tid)),
        Node::Statm => snap::format_statm(&gather_process(pid)?),
        Node::Comm => snap::format_comm(&caller_thread(caller_tid)),
        Node::OomScoreAdj => snap::format_oom_score_adj(),
        Node::Limits => snap::format_limits(&gather_limits()),
        Node::Cmdline => {
            let process = gather_process(pid)?;
            read_user_range(process.arg_start, process.arg_end)?
        }
        Node::Environ => {
            let process = gather_process(pid)?;
            read_user_range(process.env_start, process.env_end)?
        }
        Node::Auxv => {
            let startup = multitask::with_current_user_process_state(|_, _, state| {
                state.linux_process_state().map(|linux| linux.startup)
            })
            .flatten()
            .unwrap_or_default();
            let bytes = read_user_range(startup.auxv_start, startup.auxv_end)?;
            if bytes.len() % 16 != 0 {
                return Err(LINUX_EINVAL);
            }
            bytes
        }
        Node::TaskStat | Node::TaskComm | Node::TaskStatus => {
            let thread = thread_facts(pid, caller_tid, number)?;
            match node {
                Node::TaskStat => snap::format_stat(&gather_process(pid)?, &thread),
                Node::TaskComm => snap::format_comm(&thread),
                _ => snap::format_status(&gather_process(pid)?, &thread),
            }
        }
        Node::FdDir => {
            let fds = multitask::with_current_user_process_state(|_, _, state| {
                state.handles().fd_numbers()
            })
            .ok_or(LINUX_ESRCH)?;
            snap::encode_dir_snapshot(&dir_names(pid, node, &fds))
        }
        Node::TaskDir => {
            snap::encode_dir_snapshot(&dir_names(pid, node, &multitask::live_user_thread_ids(pid)))
        }
        Node::TaskTidDir => {
            if !tid_is_live(pid, number) {
                return Err(LINUX_ENOENT);
            }
            snap::encode_dir_snapshot(&task_tid_names(pid, number))
        }
        Node::FdLink => return Err(LINUX_EINVAL),
    };
    complete_snapshot(bytes)
}

fn install_snapshot(
    link: String,
    directory: bool,
    ino: u64,
    bytes: &[u8],
    flags: u64,
) -> Result<u64, i64> {
    let group = multitask::current_user_job_charge_group();
    let memfd = multitask::MemfdHandle::new_proc_snapshot(link, directory, ino, bytes, group)
        .map_err(memfd_error_to_linux_errno)?;
    multitask::with_current_user_process_state_mut(|_, _, state| {
        state
            .handles_mut()
            .install_with_open_flags_and_rights(
                multitask::KernelHandle::Memfd(memfd),
                flags,
                HandleRights::Memfd(snapshot_rights(flags)),
            )
            .ok_or(LINUX_EMFILE)
    })
    .unwrap_or(Err(LINUX_EBADF))
}

fn snapshot_rights(flags: u64) -> FileHandleRights {
    if flags & linux_abi::O_PATH != 0 {
        FileHandleRights::TRANSFER
    } else {
        FileHandleRights::READ.union(FileHandleRights::TRANSFER)
    }
}

// ---- completion of delegated operations ---------------------------------------------------

/// `open` of a delegated node: `Some` when `aux` is a delegation.
pub(crate) fn complete_open(aux: u64, flags: u64) -> Option<Result<u64, i64>> {
    let (node, number) = abi_procfs::decode_delegate(aux)?;
    Some((|| {
        let pid = caller_pid()?;
        if node == Node::FdLink {
            return reopen_fd(number, flags);
        }
        let bytes = render(node, number, pid)?;
        install_snapshot(
            node_link(pid, node, number),
            node.is_directory(),
            abi_procfs::node_inode(pid, node, number),
            &bytes,
            flags,
        )
    })())
}

/// Stat of a delegated node (`aux` from vfsd), `None` when not delegated.
pub(crate) fn complete_stat(aux: u64, flags: u64, out: StatOut) -> Option<u64> {
    let (node, number) = abi_procfs::decode_delegate(aux)?;
    Some(match stat_node(node, number, flags, out) {
        Ok(result) => result,
        Err(errno) => linux_errno(errno),
    })
}

fn stat_node(node: Node, number: u64, flags: u64, out: StatOut) -> Result<u64, i64> {
    let pid = caller_pid()?;
    if node == Node::FdLink {
        if !current_fd_exists(number) {
            return Err(LINUX_ENOENT);
        }
        if flags & AT_SYMLINK_NOFOLLOW_FLAG == 0 {
            // Magic-link semantics: the result describes the target object.
            return Ok(match out {
                StatOut::Stat { ptr } => syscall_linux_vfs_fstat(number, ptr),
                StatOut::Statx { ptr, mask } => {
                    syscall_linux_vfs_statx_str(number, "", linux_abi::AT_EMPTY_PATH, mask, ptr)
                }
            });
        }
    } else if matches!(node, Node::TaskTidDir | Node::TaskStat | Node::TaskComm | Node::TaskStatus)
        && !tid_is_live(pid, number)
    {
        return Err(LINUX_ENOENT);
    }
    let stat = AnonStat {
        dev: anon_stat::DEV_PROCFS,
        ino: abi_procfs::node_inode(pid, node, number),
        mode: node.mode(),
        size: 0,
    };
    let (uid, gid) = anon_stat::owner_ids();
    let (ptr, bytes): (u64, Vec<u8>) = match out {
        StatOut::Stat { ptr } => (ptr, anon_stat::encode_stat(stat, uid, gid).to_vec()),
        StatOut::Statx { ptr, .. } => (ptr, anon_stat::encode_statx(stat, uid, gid).to_vec()),
    };
    usermem::validate_current_user_write_buffer(ptr, bytes.len())
        .map_err(user_pointer_error_to_linux_errno)?;
    usermem::write_current_user_bytes(ptr, &bytes)
        .map(|()| 0)
        .map_err(user_pointer_error_to_linux_errno)
}

/// `access` of a delegated node, `None` when not delegated.
pub(crate) fn complete_access(aux: u64, mode: u64) -> Option<u64> {
    let (node, number) = abi_procfs::decode_delegate(aux)?;
    Some(match access_node(node, number, mode) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    })
}

fn access_node(node: Node, number: u64, mode: u64) -> Result<(), i64> {
    let pid = caller_pid()?;
    match node {
        Node::FdLink => {
            if !current_fd_exists(number) {
                return Err(LINUX_ENOENT);
            }
        }
        Node::TaskTidDir | Node::TaskStat | Node::TaskComm | Node::TaskStatus => {
            if !tid_is_live(pid, number) {
                return Err(LINUX_ENOENT);
            }
        }
        _ => {}
    }
    if node.is_link() {
        return Ok(());
    }
    // Generated nodes are read-only; only directories are searchable.
    if mode & W_OK != 0 || (mode & X_OK != 0 && !node.is_directory()) {
        return Err(LINUX_EACCES);
    }
    Ok(())
}

/// `readlink` of a delegated node, `None` when not delegated.
pub(crate) fn complete_readlink(aux: u64) -> Option<Result<String, i64>> {
    let (node, number) = abi_procfs::decode_delegate(aux)?;
    Some(if node == Node::FdLink {
        super::vfs_socket::proc_fd_link_text(number)
    } else {
        Err(LINUX_EINVAL)
    })
}

/// `getdents64` on a generated directory snapshot; `None` when `fd` is not one
/// (an ordinary memfd or a non-memfd keeps its normal path).
pub(crate) fn getdents_snapshot(fd: u64, user_ptr: u64, user_len: u64) -> Option<u64> {
    let (memfd, rights) = current_memfd_handle_with_rights(fd)?;
    let info = memfd.proc_snapshot()?;
    if !rights.allows_read() {
        return Some(linux_errno(LINUX_EBADF));
    }
    if !info.directory {
        return Some(linux_errno(LINUX_ENOTDIR));
    }
    let Ok(user_len) = usize::try_from(user_len) else {
        return Some(linux_errno(LINUX_EINVAL));
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return Some(linux_errno(user_pointer_error_to_linux_errno(err)));
    }
    let cursor = memfd.cursor();
    let total = memfd.len();
    if cursor >= total {
        return Some(0);
    }
    let want = (total - cursor).min(user_len);
    let mut buffer = alloc::vec![0_u8; want];
    let got = memfd.read_at(cursor, &mut buffer);
    let take = snap::whole_dirent_prefix(&buffer[..got], user_len);
    if take == 0 {
        // The caller's buffer cannot hold even the next record.
        return Some(linux_errno(LINUX_EINVAL));
    }
    if let Err(err) = usermem::write_current_user_bytes(user_ptr, &buffer[..take]) {
        return Some(linux_errno(user_pointer_error_to_linux_errno(err)));
    }
    let mut advance = memfd.clone();
    let _ = advance.seek(take as i64, multitask::FileHandleSeekWhence::Current);
    Some(take as u64)
}

// ---- `open("/proc/self/fd/N")` reopen -------------------------------------------------------

/// A new open description for the object behind descriptor `target_fd`.
///
/// - memfd: a new description of the same object; read/write intent must be
///   covered by the source rights (`EACCES` otherwise), seals apply as for
///   every write, and `O_TRUNC` needs write.
/// - generated procfs snapshot: a fresh snapshot of the same node.
/// - remote VFS descriptors use a pinned provider object reopen, never a path.
/// - pipes, sockets, eventfd, epoll and everything else: `ENXIO`. A pipe is
///   not reopened (Linux would attach a new end; this kernel has no named
///   attachment point for that).
fn reopen_fd(target_fd: u64, flags: u64) -> Result<u64, i64> {
    let (handle, rights) = multitask::with_current_user_process_state(|_, _, state| {
        state
            .handles()
            .get_entry(target_fd)
            .map(|entry| (entry.handle().clone(), entry.rights()))
    })
    .flatten()
    .ok_or(LINUX_ENOENT)?;
    match handle {
        multitask::KernelHandle::Memfd(memfd) => {
            if let Some(info) = memfd.proc_snapshot() {
                return syscall_linux_vfs_openat_str(
                    linux_abi::AT_FDCWD as u64,
                    &info.link,
                    flags,
                    0,
                );
            }
            let truncate = super::vfs_socket::validate_memfd_reopen_flags(flags, rights)?;
            if truncate {
                memfd.truncate(0).map_err(memfd_error_to_linux_errno)?;
            }
            let reopened = multitask::KernelHandle::Memfd(memfd.reopen());
            multitask::with_current_user_process_state_mut(|_, _, state| {
                state
                    .handles_mut()
                    .install_with_open_flags(reopened, flags)
                    .ok_or(LINUX_EMFILE)
            })
            .unwrap_or(Err(LINUX_EBADF))
        }
        multitask::KernelHandle::RemoteVfs(remote) => {
            super::vfs_socket::reopen_remote_vfs(target_fd, remote.remote_id(), flags)
        }
        multitask::KernelHandle::VfsDirectory(_) => Err(LINUX_EOPNOTSUPP),
        _ => Err(LINUX_ENXIO_ERRNO),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_snapshot_fails_instead_of_returning_a_partial_stream() {
        let max = abi_procfs::PROC_SNAPSHOT_MAX_BYTES;
        assert_eq!(complete_snapshot(alloc::vec![0; max]).unwrap().len(), max);
        assert_eq!(complete_snapshot(alloc::vec![0; max + 1]), Err(LINUX_EOVERFLOW));
        assert_eq!(read_user_range(1, max as u64 + 2), Err(LINUX_EOVERFLOW));
    }

    #[test]
    fn snapshot_path_descriptors_have_no_read_right() {
        assert!(!snapshot_rights(linux_abi::O_PATH).contains(FileHandleRights::READ));
        assert!(snapshot_rights(0).contains(FileHandleRights::READ));
        assert!(snapshot_rights(linux_abi::O_PATH).contains(FileHandleRights::TRANSFER));
    }

    #[test]
    fn memfd_reopen_path_has_no_implicit_read_authority() {
        let no_access = HandleRights::Memfd(FileHandleRights::TRANSFER);
        assert_eq!(super::super::vfs_socket::validate_memfd_reopen_flags(linux_abi::O_PATH, no_access), Ok(false));
        assert_eq!(super::super::vfs_socket::validate_memfd_reopen_flags(0, no_access), Err(LINUX_EACCES));
        let read_only = HandleRights::Memfd(FileHandleRights::READ);
        assert_eq!(super::super::vfs_socket::validate_memfd_reopen_flags(linux_abi::O_RDWR, read_only), Err(LINUX_EACCES));
        assert_eq!(super::super::vfs_socket::validate_memfd_reopen_flags(linux_abi::O_TRUNC, read_only), Err(LINUX_EACCES));
    }

    #[test]
    fn fd_directory_lists_dot_entries_then_one_link_per_descriptor() {
        let names = dir_names(7, Node::FdDir, &[0, 3, 12]);
        let listed: Vec<(&[u8], u8)> =
            names.iter().map(|entry| (entry.name.as_slice(), entry.d_type)).collect();
        assert_eq!(
            listed,
            [
                (&b"."[..], abi_procfs::DT_DIR),
                (&b".."[..], abi_procfs::DT_DIR),
                (&b"0"[..], abi_procfs::DT_LNK),
                (&b"3"[..], abi_procfs::DT_LNK),
                (&b"12"[..], abi_procfs::DT_LNK),
            ]
        );
        assert_eq!(names[2].ino, abi_procfs::node_inode(7, Node::FdLink, 0));
        assert_eq!(names[1].ino, abi_procfs::pid_dir_inode(7));
    }

    #[test]
    fn task_directory_lists_threads_as_directories_and_tid_dir_its_files() {
        let names = dir_names(7, Node::TaskDir, &[7, 9]);
        assert_eq!(names[2].name, b"7");
        assert_eq!(names[3].d_type, abi_procfs::DT_DIR);
        assert_eq!(names[3].ino, abi_procfs::node_inode(7, Node::TaskTidDir, 9));
        let files = task_tid_names(7, 9);
        let listed: Vec<&[u8]> = files.iter().skip(2).map(|entry| entry.name.as_slice()).collect();
        assert_eq!(listed, [&b"comm"[..], &b"stat"[..], &b"status"[..]]);
        assert_eq!(files[3].ino, abi_procfs::node_inode(7, Node::TaskStat, 9));
        assert!(files.iter().skip(2).all(|entry| entry.d_type == abi_procfs::DT_REG));
    }

    #[test]
    fn directory_snapshot_round_trips_whole_records() {
        let bytes = snap::encode_dir_snapshot(&dir_names(7, Node::FdDir, &[0, 1]));
        assert_eq!(snap::whole_dirent_prefix(&bytes, bytes.len()), bytes.len());
        assert_eq!(snap::whole_dirent_prefix(&bytes, 1), 0);
    }

    use super::copy_exec_path;

    #[test]
    fn exec_path_is_copied_whole_or_not_at_all() {
        let mut payload = [0xff_u8; 8];
        assert_eq!(copy_exec_path(&mut payload, "/a/bc"), 5);
        assert_eq!(&payload[..5], b"/a/bc");
        assert_eq!(copy_exec_path(&mut payload, "/too/long/path"), 0);
        assert_eq!(copy_exec_path(&mut payload, ""), 0);
        assert_eq!(copy_exec_path(&mut payload, "/a\0b"), 0);
    }
}
