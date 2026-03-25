mod bootfs;
mod devfs;
mod procfs;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

use crate::io::device::DeviceHandle;
use crate::multitask;
use crate::user::abi::UserAbi;
use crate::user::handles::{KernelHandle, VfsDirectoryHandle, VfsFileHandle};
use crate::user::process_state::UserProcessState;

const DEFAULT_BLOCK_SIZE: u64 = 4096;

static MOUNTS: Mutex<Vec<VfsMount>> = Mutex::new(Vec::new());

pub(crate) fn init() {
    register_mount("/proc", &procfs::PROCFS);
    register_mount("/dev", &devfs::DEVFS);
    register_mount("/", &bootfs::BOOTFS);
}

pub(crate) fn register_mount(path: &'static str, backend: &'static dyn VfsBackend) {
    let mut mounts = MOUNTS.lock();
    if mounts.iter().any(|mount| mount.path == path) {
        return;
    }

    mounts.push(VfsMount { path, backend });
}

pub(crate) fn open_path_for_current_process(
    absolute_path: &str,
    flags: u64,
    mode: u64,
) -> Result<u64, VfsError> {
    let mount = resolve_mount(absolute_path)?;
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        let mut context = VfsContext { abi, process_state };
        let opened = mount.backend.open(
            absolute_path,
            mount.relative_path.as_str(),
            flags,
            mode,
            &mut context,
        )?;
        install_open_result(process_state, opened, flags)
    }) else {
        return Err(VfsError::Unsupported);
    };

    result
}

pub(crate) fn metadata_for_current_process_path(
    absolute_path: &str,
) -> Result<VfsMetadata, VfsError> {
    let mount = resolve_mount(absolute_path)?;
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        let mut context = VfsContext { abi, process_state };
        mount
            .backend
            .metadata(absolute_path, mount.relative_path.as_str(), &mut context)
    }) else {
        return Err(VfsError::Unsupported);
    };

    result
}

pub(crate) fn check_access_for_current_process(
    absolute_path: &str,
    mode: u64,
) -> Result<(), VfsError> {
    let mount = resolve_mount(absolute_path)?;
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        let mut context = VfsContext { abi, process_state };
        mount.backend.check_access(
            absolute_path,
            mount.relative_path.as_str(),
            mode,
            &mut context,
        )
    }) else {
        return Err(VfsError::Unsupported);
    };

    result
}

pub(crate) fn readlink_for_current_process(absolute_path: &str) -> Result<String, VfsError> {
    if absolute_path.starts_with("/dev/fd/") {
        let Some(result) =
            multitask::with_current_user_process_state_mut(|_, abi, process_state| {
                let mut context = VfsContext { abi, process_state };
                procfs::read_fd_link(absolute_path, &mut context)
            })
        else {
            return Err(VfsError::Unsupported);
        };
        return result;
    }

    let mount = resolve_mount(absolute_path)?;
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        let mut context = VfsContext { abi, process_state };
        mount
            .backend
            .readlink(absolute_path, mount.relative_path.as_str(), &mut context)
    }) else {
        return Err(VfsError::Unsupported);
    };

    result
}

pub(crate) fn default_metadata(path: &str, kind: VfsNodeKind, len: u64) -> VfsMetadata {
    VfsMetadata {
        inode: path_inode(path.as_bytes()),
        kind,
        len,
        block_size: DEFAULT_BLOCK_SIZE,
        blocks: len.div_ceil(512),
        link_count: default_link_count(kind),
        atime: VfsTimestamp::default(),
        mtime: VfsTimestamp::default(),
        ctime: VfsTimestamp::default(),
    }
}

pub(crate) fn path_inode(path: &[u8]) -> u64 {
    fnv1a64(path).max(1)
}

pub(crate) fn validate_read_only_open_flags(flags: u64) -> Result<(), VfsError> {
    const READ_ONLY_OPEN_FLAGS: u64 = crate::user::linux::O_RDONLY
        | crate::user::linux::O_CLOEXEC
        | crate::user::linux::O_DIRECTORY
        | crate::user::linux::O_NONBLOCK
        | crate::user::linux::O_NOCTTY;

    if flags & !READ_ONLY_OPEN_FLAGS != 0 {
        return Err(VfsError::ReadOnlyFilesystem);
    }

    match flags & crate::user::linux::O_ACCMODE {
        crate::user::linux::O_RDONLY => Ok(()),
        crate::user::linux::O_WRONLY | crate::user::linux::O_RDWR => {
            Err(VfsError::ReadOnlyFilesystem)
        }
        _ => Err(VfsError::InvalidArgument),
    }
}

pub(crate) fn validate_access_mode(mode: u64) -> Result<(), VfsError> {
    if mode
        & !(crate::user::linux::R_OK
            | crate::user::linux::W_OK
            | crate::user::linux::X_OK
            | crate::user::linux::F_OK)
        != 0
    {
        return Err(VfsError::InvalidArgument);
    }

    Ok(())
}

pub(crate) fn ensure_read_access_only(mode: u64) -> Result<(), VfsError> {
    validate_access_mode(mode)?;
    if mode & (crate::user::linux::W_OK | crate::user::linux::X_OK) != 0 {
        return Err(VfsError::PermissionDenied);
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VfsError {
    BadFileDescriptor,
    InvalidArgument,
    NotFound,
    NotDirectory,
    PermissionDenied,
    ReadOnlyFilesystem,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VfsNodeKind {
    File,
    Directory,
    Device,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct VfsTimestamp {
    pub(crate) sec: i64,
    pub(crate) nsec: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VfsMetadata {
    pub(crate) inode: u64,
    pub(crate) kind: VfsNodeKind,
    pub(crate) len: u64,
    pub(crate) block_size: u64,
    pub(crate) blocks: u64,
    pub(crate) link_count: u64,
    pub(crate) atime: VfsTimestamp,
    pub(crate) mtime: VfsTimestamp,
    pub(crate) ctime: VfsTimestamp,
}

pub(crate) enum VfsOpenResult {
    File(VfsFileHandle),
    Directory(VfsDirectoryHandle),
    Device(DeviceHandle),
}

pub(crate) trait VfsBackend: Sync {
    fn open(
        &self,
        absolute_path: &str,
        relative_path: &str,
        flags: u64,
        mode: u64,
        context: &mut VfsContext<'_>,
    ) -> Result<VfsOpenResult, VfsError>;

    fn metadata(
        &self,
        absolute_path: &str,
        relative_path: &str,
        context: &mut VfsContext<'_>,
    ) -> Result<VfsMetadata, VfsError>;

    fn check_access(
        &self,
        absolute_path: &str,
        relative_path: &str,
        mode: u64,
        context: &mut VfsContext<'_>,
    ) -> Result<(), VfsError>;

    fn readlink(
        &self,
        absolute_path: &str,
        relative_path: &str,
        context: &mut VfsContext<'_>,
    ) -> Result<String, VfsError>;
}

pub(crate) struct VfsContext<'a> {
    abi: UserAbi,
    process_state: &'a mut UserProcessState,
}

impl<'a> VfsContext<'a> {
    pub(crate) fn abi(&self) -> UserAbi {
        self.abi
    }

    pub(crate) fn process_state(&self) -> &UserProcessState {
        self.process_state
    }

    pub(crate) fn process_state_mut(&mut self) -> &mut UserProcessState {
        self.process_state
    }
}

#[derive(Clone, Copy)]
struct VfsMount {
    path: &'static str,
    backend: &'static dyn VfsBackend,
}

struct ResolvedMount {
    backend: &'static dyn VfsBackend,
    relative_path: String,
}

fn resolve_mount(absolute_path: &str) -> Result<ResolvedMount, VfsError> {
    if !absolute_path.starts_with('/') {
        return Err(VfsError::InvalidArgument);
    }

    let mounts = MOUNTS.lock();
    let mut best: Option<VfsMount> = None;
    for mount in mounts.iter().copied() {
        if !path_is_within_mount(absolute_path, mount.path) {
            continue;
        }
        let replace = best
            .map(|current| mount.path.len() > current.path.len())
            .unwrap_or(true);
        if replace {
            best = Some(mount);
        }
    }
    drop(mounts);

    let Some(best) = best else {
        return Err(VfsError::NotFound);
    };

    Ok(ResolvedMount {
        backend: best.backend,
        relative_path: path_relative_to_mount(absolute_path, best.path),
    })
}

fn install_open_result(
    process_state: &mut UserProcessState,
    opened: VfsOpenResult,
    open_flags: u64,
) -> Result<u64, VfsError> {
    Ok(match opened {
        VfsOpenResult::File(handle) => process_state
            .handles_mut()
            .install_with_open_flags(KernelHandle::VfsFile(handle), open_flags),
        VfsOpenResult::Directory(handle) => process_state
            .handles_mut()
            .install_with_open_flags(KernelHandle::VfsDirectory(handle), open_flags),
        VfsOpenResult::Device(handle) => process_state
            .handles_mut()
            .install_with_open_flags(KernelHandle::Device(handle), open_flags),
    })
}

fn path_is_within_mount(absolute_path: &str, mount_path: &str) -> bool {
    if mount_path == "/" {
        return absolute_path.starts_with('/');
    }

    absolute_path == mount_path
        || (absolute_path.starts_with(mount_path)
            && absolute_path.as_bytes().get(mount_path.len()) == Some(&b'/'))
}

fn path_relative_to_mount(absolute_path: &str, mount_path: &str) -> String {
    if mount_path == "/" {
        return absolute_path.to_string();
    }

    let suffix = &absolute_path[mount_path.len()..];
    if suffix.is_empty() {
        String::from("/")
    } else {
        suffix.to_string()
    }
}

fn default_link_count(kind: VfsNodeKind) -> u64 {
    match kind {
        VfsNodeKind::Directory => 2,
        VfsNodeKind::File | VfsNodeKind::Device => 1,
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
