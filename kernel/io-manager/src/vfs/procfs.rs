use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::multitask;
use crate::user::abi::UserAbi;
use crate::user::handles::VfsFileHandle;
use crate::user::linux::LinuxVmaName;

use super::{VfsError, VfsMetadata, VfsNodeKind, VfsOpenResult};

pub(crate) const PROC_SELF_MAPS_PATH: &str = "/proc/self/maps";
pub(crate) const PROC_RUSTOS_DIR_PATH: &str = "/proc/rustos";
pub(crate) const PROC_RUSTOS_LOG_PATH: &str = "/proc/rustos/log";

pub(crate) fn is_local_special_path(path: &str) -> bool {
    path == PROC_SELF_MAPS_PATH
        || path == PROC_RUSTOS_LOG_PATH
        || path.starts_with("/proc/self/fd/")
        || path.starts_with("/dev/fd/")
}

pub(crate) fn open_special_path(path: &str) -> Result<Option<VfsOpenResult>, VfsError> {
    if path == PROC_SELF_MAPS_PATH {
        let bytes = proc_self_maps_snapshot()?;
        return Ok(Some(VfsOpenResult::File(VfsFileHandle::read_only_memory(
            String::from(path),
            bytes,
        ))));
    }
    if path == PROC_RUSTOS_LOG_PATH {
        let bytes = nucleus_core::debug::snapshot_structured_log_bytes();
        return Ok(Some(VfsOpenResult::File(VfsFileHandle::read_only_memory(
            String::from(path),
            bytes,
        ))));
    }
    Ok(None)
}

pub(crate) fn metadata_for_special_path(path: &str) -> Result<Option<VfsMetadata>, VfsError> {
    if path == PROC_SELF_MAPS_PATH {
        let len = proc_self_maps_snapshot()?.len() as u64;
        return Ok(Some(VfsMetadata {
            inode: crate::vfs::path_inode(path.as_bytes()).max(1),
            kind: VfsNodeKind::File,
            len,
            block_size: 4096,
            blocks: len.div_ceil(512),
            link_count: 1,
            atime: super::VfsTimestamp::default(),
            mtime: super::VfsTimestamp::default(),
            ctime: super::VfsTimestamp::default(),
        }));
    }
    if path == PROC_RUSTOS_LOG_PATH {
        let len = nucleus_core::debug::snapshot_structured_log_bytes().len() as u64;
        return Ok(Some(VfsMetadata {
            inode: crate::vfs::path_inode(path.as_bytes()).max(1),
            kind: VfsNodeKind::File,
            len,
            block_size: 4096,
            blocks: len.div_ceil(512),
            link_count: 1,
            atime: super::VfsTimestamp::default(),
            mtime: super::VfsTimestamp::default(),
            ctime: super::VfsTimestamp::default(),
        }));
    }
    Ok(None)
}

pub(crate) fn read_fd_link(absolute_path: &str) -> Result<String, VfsError> {
    fd_link_target(absolute_path)?.ok_or(VfsError::NotFound)
}

pub(crate) fn fd_link_target(absolute_path: &str) -> Result<Option<String>, VfsError> {
    let Some(fd) = parse_self_fd_link(absolute_path) else {
        return Ok(None);
    };
    resolve_fd_link_target(fd).map(Some)
}

fn parse_self_fd_link(path: &str) -> Option<u64> {
    let suffix = path
        .strip_prefix("/proc/self/fd/")
        .or_else(|| path.strip_prefix("/dev/fd/"))?;
    if suffix.is_empty() || suffix.bytes().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse::<u64>().ok()
}

pub(crate) fn proc_self_maps_snapshot() -> Result<Vec<u8>, VfsError> {
    let retained = multitask::retain_current_user_process_state().ok_or(VfsError::NotFound)?;
    if retained.abi() != UserAbi::Linux {
        return Err(VfsError::NotFound);
    }

    let Some(memory_map) = retained.process_state().linux_memory_map() else {
        return Err(VfsError::NotFound);
    };

    let mut output = String::new();
    for area in memory_map.areas() {
        let inode = proc_maps_inode_for_vma(area);
        write!(
            &mut output,
            "{:016x}-{:016x} {}{}{}{} {:08x} 00:00 {:>5}",
            area.start,
            area.end,
            if area.flags.read { 'r' } else { '-' },
            if area.flags.write { 'w' } else { '-' },
            if area.flags.execute { 'x' } else { '-' },
            if area.flags.private { 'p' } else { 's' },
            area.offset,
            inode,
        )
        .map_err(|_| VfsError::Unsupported)?;

        if let Some(name) = proc_maps_name(&area.name) {
            output.push(' ');
            output.push_str(name);
        }
        output.push('\n');
    }

    Ok(output.into_bytes())
}

fn resolve_fd_link_target(fd: u64) -> Result<String, VfsError> {
    let retained = multitask::retain_current_user_process_state().ok_or(VfsError::NotFound)?;
    let process_state = retained.process_state();

    if fd == 0 {
        return Ok(String::from("/dev/stdin"));
    }
    if fd == 1 {
        return Ok(String::from("/dev/stdout"));
    }
    if fd == 2 {
        return Ok(String::from("/dev/stderr"));
    }

    let Some(entry) = process_state.handles().get_entry(fd) else {
        return Err(VfsError::BadFileDescriptor);
    };

    Ok(entry.handle().procfs_link_target(entry.token()))
}

fn proc_maps_inode_for_vma(_area: &crate::user::linux::LinuxVma) -> u64 {
    0
}

fn proc_maps_name(name: &LinuxVmaName) -> Option<&str> {
    match name {
        LinuxVmaName::None => None,
        LinuxVmaName::Path(path) => Some(path.as_str()),
        LinuxVmaName::Label(label) => Some(*label),
    }
}
