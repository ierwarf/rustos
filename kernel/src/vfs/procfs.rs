use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::user::abi::UserAbi;
use crate::user::handles::{KernelHandle, VfsDirectoryHandle, VfsFileHandle};
use crate::user::linux::LinuxVmaName;

use super::{VfsBackend, VfsContext, VfsError, VfsMetadata, VfsNodeKind, VfsOpenResult};

const PROC_ROOT_PATH: &str = "/proc";
const PROC_SELF_PATH: &str = "/proc/self";
const PROC_SELF_MAPS_PATH: &str = "/proc/self/maps";

pub(crate) static PROCFS: ProcFsBackend = ProcFsBackend;

pub(crate) struct ProcFsBackend;

impl VfsBackend for ProcFsBackend {
    fn open(
        &self,
        absolute_path: &str,
        _relative_path: &str,
        flags: u64,
        _mode: u64,
        context: &mut VfsContext<'_>,
    ) -> Result<VfsOpenResult, VfsError> {
        super::validate_read_only_open_flags(flags)?;

        match absolute_path {
            PROC_ROOT_PATH | PROC_SELF_PATH => Ok(VfsOpenResult::Directory(
                VfsDirectoryHandle::new(String::from(absolute_path)),
            )),
            PROC_SELF_MAPS_PATH => {
                if flags & crate::user::linux::O_DIRECTORY != 0 {
                    return Err(VfsError::NotDirectory);
                }
                Ok(VfsOpenResult::File(VfsFileHandle::read_only_memory(
                    String::from(absolute_path),
                    build_proc_self_maps_snapshot(context)?,
                )))
            }
            _ => Err(VfsError::NotFound),
        }
    }

    fn metadata(
        &self,
        absolute_path: &str,
        _relative_path: &str,
        context: &mut VfsContext<'_>,
    ) -> Result<VfsMetadata, VfsError> {
        match absolute_path {
            PROC_ROOT_PATH | PROC_SELF_PATH => Ok(super::default_metadata(
                absolute_path,
                VfsNodeKind::Directory,
                0,
            )),
            PROC_SELF_MAPS_PATH => Ok(super::default_metadata(
                absolute_path,
                VfsNodeKind::File,
                build_proc_self_maps_snapshot(context)?.len() as u64,
            )),
            _ => Err(VfsError::NotFound),
        }
    }

    fn check_access(
        &self,
        absolute_path: &str,
        _relative_path: &str,
        mode: u64,
        context: &mut VfsContext<'_>,
    ) -> Result<(), VfsError> {
        super::ensure_read_access_only(mode)?;
        let _ = self.metadata(absolute_path, "/", context)?;
        Ok(())
    }

    fn readlink(
        &self,
        absolute_path: &str,
        _relative_path: &str,
        context: &mut VfsContext<'_>,
    ) -> Result<String, VfsError> {
        match absolute_path {
            "/proc/self/cwd" => Ok(String::from("/")),
            "/proc/self/exe" => {
                let path = context.process_state().exec_path();
                if path.is_empty() {
                    Err(VfsError::NotFound)
                } else {
                    Ok(String::from(path))
                }
            }
            _ => read_fd_link(absolute_path, context),
        }
    }
}

pub(crate) fn read_fd_link(
    absolute_path: &str,
    context: &mut VfsContext<'_>,
) -> Result<String, VfsError> {
    let Some(fd) = parse_self_fd_link(absolute_path) else {
        return Err(VfsError::NotFound);
    };

    if fd == 0 {
        return Ok(String::from("/dev/stdin"));
    }
    if fd == 1 {
        return Ok(String::from("/dev/stdout"));
    }
    if fd == 2 {
        return Ok(String::from("/dev/stderr"));
    }

    let Some(handle) = context.process_state().handles().get(fd) else {
        return Err(VfsError::BadFileDescriptor);
    };

    match handle {
        KernelHandle::Console(_) => Ok(String::from("/dev/tty")),
        KernelHandle::Device(device) => Ok(String::from(device.device_id().path())),
        KernelHandle::VfsFile(file) => Ok(file.path()),
        KernelHandle::VfsDirectory(directory) => Ok(String::from(directory.path())),
        KernelHandle::DisplaySurface(_) => Ok(String::from("anon_inode:[rustos-display-surface]")),
    }
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

fn build_proc_self_maps_snapshot(context: &mut VfsContext<'_>) -> Result<Vec<u8>, VfsError> {
    if context.abi() != UserAbi::Linux {
        return Err(VfsError::NotFound);
    }

    let Some(memory_map) = context.process_state().linux_memory_map() else {
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

fn proc_maps_inode_for_vma(area: &crate::user::linux::LinuxVma) -> u64 {
    match &area.name {
        LinuxVmaName::Path(path) if path.starts_with('/') && !path.starts_with("/proc/") => {
            super::metadata_for_current_process_path(path.as_str())
                .map(|metadata| metadata.inode)
                .unwrap_or(0)
        }
        _ => 0,
    }
}

fn proc_maps_name(name: &LinuxVmaName) -> Option<&str> {
    match name {
        LinuxVmaName::None => None,
        LinuxVmaName::Path(path) => Some(path.as_str()),
        LinuxVmaName::Label(label) => Some(label),
    }
}
