mod bootfs;
mod devfs;
mod procfs;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

use crate::io::device::DeviceHandle;
use crate::multitask;
use crate::storage::block;
use crate::user::abi::UserAbi;
use crate::user::handles::{
    KernelHandle, VfsDirectoryEntry, VfsDirectoryEntryKind, VfsDirectoryHandle, VfsFileHandle,
};
use crate::user::process_state::UserProcessState;

const DEFAULT_BLOCK_SIZE: u64 = 4096;
const SUPPORTED_MOUNT_FLAGS: u64 = crate::user::linux::MS_RDONLY;

static MOUNTS: Mutex<Vec<VfsMount>> = Mutex::new(Vec::new());
static FILESYSTEMS: Mutex<Vec<&'static dyn FilesystemProvider>> = Mutex::new(Vec::new());

pub(crate) fn init() {
    crate::debug::println!("vfs: register filesystem proc");
    register_filesystem(&procfs::PROCFS_PROVIDER);
    crate::debug::println!("vfs: register filesystem devfs");
    register_filesystem(&devfs::DEVFS_PROVIDER);
    crate::debug::println!("vfs: register filesystem fat");
    register_filesystem(&bootfs::FAT_PROVIDER);

    crate::debug::println!("vfs: mount /proc begin");
    let _ = mount_internal("/proc", "proc", MountSource::None, 0, None, true);
    crate::debug::println!("vfs: mount /proc done");
    crate::debug::println!("vfs: mount /dev begin");
    let _ = mount_internal("/dev", "devfs", MountSource::None, 0, None, true);
    crate::debug::println!("vfs: mount /dev done");
    crate::debug::println!("vfs: mount / begin");
    let _ = mount_internal(
        "/",
        "fat",
        MountSource::BootVolume,
        crate::user::linux::MS_RDONLY,
        None,
        true,
    );
    crate::debug::println!("vfs: mount / done");
}

pub(crate) fn register_filesystem(provider: &'static dyn FilesystemProvider) {
    let mut filesystems = FILESYSTEMS.lock();
    if filesystems
        .iter()
        .any(|current| current.name() == provider.name())
    {
        return;
    }
    filesystems.push(provider);
}

pub(crate) fn mount_for_current_process(
    source_path: &str,
    target_path: &str,
    filesystem_type: &str,
    flags: u64,
    options: Option<&str>,
) -> Result<(), MountError> {
    validate_mount_flags(flags)?;
    let source = block::lookup(source_path).ok_or(MountError::InvalidSource)?;
    let metadata = metadata_for_current_process_path(target_path).map_err(MountError::from)?;
    if metadata.kind != VfsNodeKind::Directory {
        return Err(MountError::NotDirectory);
    }
    mount_internal(
        target_path,
        filesystem_type,
        MountSource::BlockDevice(source),
        flags,
        options,
        false,
    )
}

pub(crate) fn umount_for_current_process(target_path: &str) -> Result<(), MountError> {
    if target_path == "/" {
        return Err(MountError::Busy);
    }

    {
        let mounts = MOUNTS.lock();
        let Some(target) = mounts.iter().find(|mount| mount.path == target_path) else {
            return Err(MountError::NotFound);
        };
        if target.pinned {
            return Err(MountError::Busy);
        }
        if mounts.iter().any(|mount| {
            mount.path != target_path && path_is_within_mount(mount.path.as_str(), target_path)
        }) {
            return Err(MountError::Busy);
        }
    }

    let busy = multitask::any_user_process_state(|_, process_state| {
        if path_is_within_mount(process_state.cwd(), target_path) {
            return true;
        }

        let mut in_use = false;
        for fd in crate::user::handles::FIRST_DYNAMIC_FD as u64
            ..crate::user::handles::FIRST_DYNAMIC_FD as u64 + 4096
        {
            let Some(handle) = process_state.handles().get(fd) else {
                continue;
            };
            let path = match handle {
                KernelHandle::VfsFile(file) => file.path(),
                KernelHandle::VfsDirectory(directory) => String::from(directory.path()),
                _ => continue,
            };
            if path_is_within_mount(path.as_str(), target_path) {
                in_use = true;
                break;
            }
        }
        in_use
    });
    if busy {
        return Err(MountError::Busy);
    }

    let mut mounts = MOUNTS.lock();
    let Some(index) = mounts.iter().position(|mount| mount.path == target_path) else {
        return Err(MountError::NotFound);
    };
    mounts.remove(index);
    Ok(())
}

fn mount_internal(
    path: &str,
    filesystem_type: &str,
    source: MountSource,
    flags: u64,
    options: Option<&str>,
    pinned: bool,
) -> Result<(), MountError> {
    validate_mount_flags(flags)?;
    if !path.starts_with('/') {
        return Err(MountError::InvalidArgument);
    }

    let provider = {
        let filesystems = FILESYSTEMS.lock();
        filesystems
            .iter()
            .copied()
            .find(|provider| provider.name() == filesystem_type)
            .ok_or(MountError::UnsupportedFilesystem)?
    };

    let backend = provider.mount(source, flags, options)?;
    let mut mounts = MOUNTS.lock();
    if mounts.iter().any(|mount| mount.path == path) {
        return Err(MountError::Busy);
    }
    mounts.push(VfsMount {
        path: String::from(path),
        backend,
        filesystem_type: String::from(filesystem_type),
        flags,
        pinned,
    });
    Ok(())
}

pub(crate) fn open_path_for_current_process(
    absolute_path: &str,
    flags: u64,
    mode: u64,
) -> Result<u64, VfsError> {
    let mount = resolve_mount(absolute_path)?;
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        let mut context = VfsContext::new_user(abi, process_state);
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
        let mut context = VfsContext::new_user(abi, process_state);
        mount
            .backend
            .metadata(absolute_path, mount.relative_path.as_str(), &mut context)
    }) else {
        return Err(VfsError::Unsupported);
    };

    result
}

pub(crate) fn read_path_to_vec_for_kernel(absolute_path: &str) -> Result<Vec<u8>, VfsError> {
    let absolute_path = normalize_kernel_path(absolute_path)?;
    let mount = resolve_mount(absolute_path.as_str())?;
    crate::debug::println!(
        "vfs kernel read: path={} mount={} fs={} relative={}",
        absolute_path,
        mount.mount_path,
        mount.filesystem_type,
        mount.relative_path
    );
    let mut context = VfsContext::new_kernel();
    match mount.backend.open(
        absolute_path.as_str(),
        mount.relative_path.as_str(),
        crate::user::linux::O_RDONLY,
        0,
        &mut context,
    )? {
        VfsOpenResult::File(file) => {
            crate::debug::println!(
                "vfs kernel read: opened file path={} len={}",
                absolute_path,
                file.len()
            );
            let mut bytes = vec![0_u8; file.len()];
            let mut copied = 0usize;
            while copied < bytes.len() {
                let read = file.read_at(copied, &mut bytes[copied..]);
                if read == 0 {
                    bytes.truncate(copied);
                    break;
                }
                copied += read;
            }
            crate::debug::println!(
                "vfs kernel read: completed path={} bytes={}",
                absolute_path,
                bytes.len()
            );
            Ok(bytes)
        }
        VfsOpenResult::Directory(_) => Err(VfsError::NotDirectory),
        VfsOpenResult::Device(_) => Err(VfsError::Unsupported),
    }
}

pub(crate) fn normalize_kernel_path(path: &str) -> Result<String, VfsError> {
    if path.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    if path.starts_with('/') {
        return Ok(String::from(path));
    }
    Ok(alloc::format!("/{path}"))
}

pub(crate) fn check_access_for_current_process(
    absolute_path: &str,
    mode: u64,
) -> Result<(), VfsError> {
    let mount = resolve_mount(absolute_path)?;
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        let mut context = VfsContext::new_user(abi, process_state);
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
                let mut context = VfsContext::new_user(abi, process_state);
                procfs::read_fd_link(absolute_path, &mut context)
            })
        else {
            return Err(VfsError::Unsupported);
        };
        return result;
    }

    let mount = resolve_mount(absolute_path)?;
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        let mut context = VfsContext::new_user(abi, process_state);
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

pub(crate) trait VfsBackend: Send + Sync {
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

    fn read_dir(
        &self,
        absolute_path: &str,
        relative_path: &str,
        context: &mut VfsContext<'_>,
    ) -> Result<Vec<VfsDirectoryEntry>, VfsError>;
}

#[derive(Clone, Copy)]
pub(crate) enum MountSource {
    None,
    BootVolume,
    BlockDevice(block::BlockDeviceHandle),
}

pub(crate) trait FilesystemProvider: Sync {
    fn name(&self) -> &'static str;
    fn mount(
        &self,
        source: MountSource,
        flags: u64,
        options: Option<&str>,
    ) -> Result<Arc<dyn VfsBackend>, MountError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MountError {
    Busy,
    InvalidArgument,
    InvalidSource,
    NotDirectory,
    NotFound,
    PermissionDenied,
    ReadOnlyFilesystem,
    UnsupportedFilesystem,
    UnsupportedMountFlags,
}

impl From<VfsError> for MountError {
    fn from(value: VfsError) -> Self {
        match value {
            VfsError::BadFileDescriptor | VfsError::InvalidArgument => Self::InvalidArgument,
            VfsError::NotFound => Self::NotFound,
            VfsError::NotDirectory => Self::NotDirectory,
            VfsError::PermissionDenied => Self::PermissionDenied,
            VfsError::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            VfsError::Unsupported => Self::InvalidArgument,
        }
    }
}

pub(crate) struct VfsContext<'a> {
    abi: Option<UserAbi>,
    process_state: Option<&'a mut UserProcessState>,
}

impl<'a> VfsContext<'a> {
    pub(crate) fn new_user(abi: UserAbi, process_state: &'a mut UserProcessState) -> Self {
        Self {
            abi: Some(abi),
            process_state: Some(process_state),
        }
    }

    pub(crate) const fn new_kernel() -> Self {
        Self {
            abi: None,
            process_state: None,
        }
    }

    pub(crate) fn abi(&self) -> Option<UserAbi> {
        self.abi
    }

    pub(crate) fn process_state(&self) -> Option<&UserProcessState> {
        self.process_state.as_deref()
    }

    pub(crate) fn process_state_mut(&mut self) -> Option<&mut UserProcessState> {
        self.process_state.as_deref_mut()
    }

    pub(crate) fn is_kernel(&self) -> bool {
        self.process_state.is_none()
    }
}

struct VfsMount {
    path: String,
    backend: Arc<dyn VfsBackend>,
    filesystem_type: String,
    flags: u64,
    pinned: bool,
}

struct ResolvedMount {
    backend: Arc<dyn VfsBackend>,
    mount_path: String,
    filesystem_type: String,
    relative_path: String,
}

fn resolve_mount(absolute_path: &str) -> Result<ResolvedMount, VfsError> {
    if !absolute_path.starts_with('/') {
        return Err(VfsError::InvalidArgument);
    }

    let mounts = MOUNTS.lock();
    let mut best: Option<&VfsMount> = None;
    for mount in mounts.iter() {
        if !path_is_within_mount(absolute_path, mount.path.as_str()) {
            continue;
        }
        let replace = best
            .map(|current| mount.path.len() > current.path.len())
            .unwrap_or(true);
        if replace {
            best = Some(mount);
        }
    }

    let Some(best) = best else {
        return Err(VfsError::NotFound);
    };

    Ok(ResolvedMount {
        backend: Arc::clone(&best.backend),
        mount_path: best.path.clone(),
        filesystem_type: best.filesystem_type.clone(),
        relative_path: path_relative_to_mount(absolute_path, best.path.as_str()),
    })
}

fn validate_mount_flags(flags: u64) -> Result<(), MountError> {
    if flags & !SUPPORTED_MOUNT_FLAGS != 0 {
        return Err(MountError::UnsupportedMountFlags);
    }
    Ok(())
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

pub(crate) fn directory_entry(absolute_path: &str, kind: VfsNodeKind) -> VfsDirectoryEntry {
    let name = absolute_path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("/");
    VfsDirectoryEntry::new(
        String::from(name),
        path_inode(absolute_path.as_bytes()),
        match kind {
            VfsNodeKind::File => VfsDirectoryEntryKind::File,
            VfsNodeKind::Directory => VfsDirectoryEntryKind::Directory,
            VfsNodeKind::Device => VfsDirectoryEntryKind::Device,
        },
    )
}

pub(crate) fn append_mount_entries(entries: &mut Vec<VfsDirectoryEntry>, absolute_dir_path: &str) {
    let mounts = MOUNTS.lock();
    for mount in mounts.iter() {
        if mount.path == "/" {
            continue;
        }

        let Some(child_name) = mount_child_name(absolute_dir_path, mount.path.as_str()) else {
            continue;
        };
        if entries.iter().any(|entry| entry.name() == child_name) {
            continue;
        }

        let child_path = if absolute_dir_path == "/" {
            alloc::format!("/{child_name}")
        } else {
            alloc::format!("{absolute_dir_path}/{child_name}")
        };
        entries.push(directory_entry(child_path.as_str(), VfsNodeKind::Directory));
    }
}

fn mount_child_name<'a>(parent_path: &str, mount_path: &'a str) -> Option<&'a str> {
    if parent_path == "/" {
        return mount_path
            .strip_prefix('/')?
            .split('/')
            .next()
            .filter(|name| !name.is_empty());
    }

    let suffix = mount_path.strip_prefix(parent_path)?.strip_prefix('/')?;
    suffix.split('/').next().filter(|name| !name.is_empty())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use spin::Mutex;

    use crate::user::handles::{VfsDirectoryEntry, VfsDirectoryHandle, VfsFileHandle};

    use super::{
        mount_internal, normalize_kernel_path, path_is_within_mount, path_relative_to_mount,
        read_path_to_vec_for_kernel, resolve_mount, umount_for_current_process, FilesystemProvider,
        MountError, MountSource, VfsBackend, VfsContext, VfsError, VfsMetadata, VfsNodeKind,
        VfsOpenResult, FILESYSTEMS, MOUNTS,
    };

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    static DUMMY_PROVIDER: DummyProvider = DummyProvider;

    struct DummyProvider;
    struct DummyBackend;

    impl FilesystemProvider for DummyProvider {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn mount(
            &self,
            _source: MountSource,
            _flags: u64,
            _options: Option<&str>,
        ) -> Result<Arc<dyn VfsBackend>, MountError> {
            Ok(Arc::new(DummyBackend))
        }
    }

    impl VfsBackend for DummyBackend {
        fn open(
            &self,
            absolute_path: &str,
            relative_path: &str,
            flags: u64,
            _mode: u64,
            context: &mut VfsContext<'_>,
        ) -> Result<VfsOpenResult, VfsError> {
            match relative_path {
                "/" | "/mnt" | "/sub" => Ok(VfsOpenResult::Directory(VfsDirectoryHandle::new(
                    absolute_path.into(),
                    self.read_dir(absolute_path, relative_path, context)?,
                ))),
                "/system/test.bin" => {
                    if flags & crate::user::linux::O_DIRECTORY != 0 {
                        return Err(VfsError::NotDirectory);
                    }
                    Ok(VfsOpenResult::File(VfsFileHandle::read_only_memory(
                        absolute_path.into(),
                        b"dummy-image".to_vec(),
                    )))
                }
                _ => Err(VfsError::NotFound),
            }
        }

        fn metadata(
            &self,
            absolute_path: &str,
            relative_path: &str,
            _context: &mut VfsContext<'_>,
        ) -> Result<VfsMetadata, VfsError> {
            match relative_path {
                "/" | "/mnt" | "/sub" => Ok(super::default_metadata(
                    absolute_path,
                    VfsNodeKind::Directory,
                    0,
                )),
                "/system/test.bin" => Ok(super::default_metadata(
                    absolute_path,
                    VfsNodeKind::File,
                    11,
                )),
                _ => Err(VfsError::NotFound),
            }
        }

        fn check_access(
            &self,
            absolute_path: &str,
            relative_path: &str,
            _mode: u64,
            context: &mut VfsContext<'_>,
        ) -> Result<(), VfsError> {
            self.metadata(absolute_path, relative_path, context)
                .map(|_| ())
        }

        fn readlink(
            &self,
            _absolute_path: &str,
            _relative_path: &str,
            _context: &mut VfsContext<'_>,
        ) -> Result<alloc::string::String, VfsError> {
            Err(VfsError::NotFound)
        }

        fn read_dir(
            &self,
            _absolute_path: &str,
            relative_path: &str,
            _context: &mut VfsContext<'_>,
        ) -> Result<Vec<VfsDirectoryEntry>, VfsError> {
            match relative_path {
                "/" => Ok(Vec::new()),
                "/mnt" => Ok(Vec::new()),
                "/sub" => Ok(Vec::new()),
                _ => Err(VfsError::NotDirectory),
            }
        }
    }

    fn reset_for_tests() {
        FILESYSTEMS.lock().clear();
        MOUNTS.lock().clear();
    }

    #[test]
    fn normalize_kernel_path_accepts_absolute_and_relative() {
        assert_eq!(normalize_kernel_path("/bin/init").unwrap(), "/bin/init");
        assert_eq!(normalize_kernel_path("bin/init").unwrap(), "/bin/init");
        assert!(normalize_kernel_path("").is_err());
    }

    #[test]
    fn mount_matching_helpers_handle_root_and_nested_mounts() {
        assert!(path_is_within_mount("/mnt/data/file", "/mnt"));
        assert!(!path_is_within_mount("/mnt2/data", "/mnt"));
        assert_eq!(
            path_relative_to_mount("/mnt/data/file", "/mnt"),
            "/data/file"
        );
        assert_eq!(path_relative_to_mount("/data/file", "/"), "/data/file");
    }

    #[test]
    fn read_path_to_vec_for_kernel_normalizes_relative_paths() {
        let _guard = TEST_LOCK.lock();
        reset_for_tests();
        super::register_filesystem(&DUMMY_PROVIDER);
        mount_internal("/", "dummy", MountSource::None, 0, None, false).unwrap();

        assert_eq!(
            read_path_to_vec_for_kernel("system/test.bin").unwrap(),
            b"dummy-image"
        );
    }

    #[test]
    fn resolve_mount_prefers_nested_mounts_and_blocks_parent_unmount() {
        let _guard = TEST_LOCK.lock();
        reset_for_tests();
        super::register_filesystem(&DUMMY_PROVIDER);
        mount_internal("/", "dummy", MountSource::None, 0, None, false).unwrap();
        mount_internal("/mnt", "dummy", MountSource::None, 0, None, false).unwrap();
        mount_internal("/mnt/sub", "dummy", MountSource::None, 0, None, false).unwrap();

        let resolved = resolve_mount("/mnt/sub/file").unwrap();
        assert_eq!(resolved.relative_path, "/file");
        assert_eq!(umount_for_current_process("/mnt"), Err(MountError::Busy));
    }
}
