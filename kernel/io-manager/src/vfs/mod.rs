pub(crate) mod procfs;
mod runtime;

use crate::vfs_core as core_vfs;
use crate::vfs_core::MountRole;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use fatfs::{Read, Seek, SeekFrom};
use spin::Mutex;

use crate::io::device::DeviceHandle;
use crate::multitask;
use crate::storage::block;
use crate::user::abi::UserAbi;
use crate::user::handles::{
    KernelHandle, VfsDirectoryEntry, VfsDirectoryEntryKind, VfsDirectoryHandle, VfsFileHandle,
    VfsFileObject,
};
use crate::user::process_state::UserProcessState;
use storage_fat::FatNodeKind;

const SUPPORTED_MOUNT_FLAGS: u64 = crate::user::linux::MS_RDONLY;
const ROOT_FILE_CACHE_MAX_ENTRIES: usize = 32;
const ROOT_FILE_CACHE_MAX_BYTES: usize = 16 * 1024 * 1024;
const ROOT_FILE_CACHE_MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;
const ROOT_FILE_EXACT_READ_MAX_BYTES: usize = 64 * 1024 * 1024;
const ROOT_FILE_PAGE_SIZE: usize = 64 * 1024;
const ROOT_FILE_PAGE_CACHE_MAX_ENTRIES: usize = 512;
const ROOT_FILE_PAGE_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;
const ROOT_DIR_CACHE_MAX_ENTRIES: usize = 32;
const ROOT_METADATA_CACHE_MAX_ENTRIES: usize = 128;

static MOUNTS: Mutex<Vec<VfsMount>> = Mutex::new(Vec::new());
static MOUNT_GENERATION: AtomicU64 = AtomicU64::new(1);
static ROOT_VOLUME: Mutex<Option<crate::storage::fat::MountedFatVolume<block::FatRegistryDevice>>> =
    Mutex::new(None);
static ROOT_CACHE: Mutex<RootCache> = Mutex::new(RootCache::new());

struct RootFileCacheEntry {
    path: String,
    bytes: Arc<[u8]>,
    last_used: u64,
}

struct RootDirCacheEntry {
    path: String,
    entries: Vec<storage_fat::FatDirEntry>,
    last_used: u64,
}

struct RootPageCacheEntry {
    path: String,
    page_index: u64,
    bytes: Arc<[u8]>,
    last_used: u64,
}

struct RootMetadataCacheEntry {
    path: String,
    metadata: VfsMetadata,
    last_used: u64,
}

struct RootCache {
    generation: u64,
    next_use: u64,
    total_file_bytes: usize,
    total_page_bytes: usize,
    files: Vec<RootFileCacheEntry>,
    pages: Vec<RootPageCacheEntry>,
    dirs: Vec<RootDirCacheEntry>,
    metadata: Vec<RootMetadataCacheEntry>,
}

impl RootCache {
    const fn new() -> Self {
        Self {
            generation: 0,
            next_use: 1,
            total_file_bytes: 0,
            total_page_bytes: 0,
            files: Vec::new(),
            pages: Vec::new(),
            dirs: Vec::new(),
            metadata: Vec::new(),
        }
    }

    fn prepare_generation(&mut self, generation: u64) {
        if self.generation == generation {
            return;
        }
        self.generation = generation;
        self.next_use = 1;
        self.total_file_bytes = 0;
        self.total_page_bytes = 0;
        self.files.clear();
        self.pages.clear();
        self.dirs.clear();
        self.metadata.clear();
    }

    fn next_use(&mut self) -> u64 {
        if self.next_use == u64::MAX {
            self.rebase_lru_epochs();
        }
        let current = self.next_use;
        self.next_use += 1;
        current
    }

    fn rebase_lru_epochs(&mut self) {
        self.next_use = 1;
        for entry in &mut self.files {
            entry.last_used = 0;
        }
        for entry in &mut self.pages {
            entry.last_used = 0;
        }
        for entry in &mut self.dirs {
            entry.last_used = 0;
        }
        for entry in &mut self.metadata {
            entry.last_used = 0;
        }
    }

    fn lookup_file(&mut self, generation: u64, path: &str) -> Option<Arc<[u8]>> {
        self.prepare_generation(generation);
        let last_used = self.next_use();
        let index = self.files.iter().position(|entry| entry.path == path)?;
        self.files[index].last_used = last_used;
        Some(Arc::clone(&self.files[index].bytes))
    }

    fn insert_file(&mut self, generation: u64, path: &str, bytes: Arc<[u8]>) {
        self.prepare_generation(generation);
        if bytes.len() > ROOT_FILE_CACHE_MAX_ENTRY_BYTES {
            return;
        }

        if let Some(index) = self.files.iter().position(|entry| entry.path == path) {
            self.total_file_bytes = self
                .total_file_bytes
                .saturating_sub(self.files[index].bytes.len());
            self.files.remove(index);
        }

        while self.files.len() >= ROOT_FILE_CACHE_MAX_ENTRIES || self.file_cache_full(bytes.len()) {
            let Some((index, _)) = self
                .files
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
            else {
                break;
            };
            self.total_file_bytes = self
                .total_file_bytes
                .saturating_sub(self.files[index].bytes.len());
            self.files.remove(index);
        }

        let last_used = self.next_use();
        let Some(total_file_bytes) = self.total_file_bytes.checked_add(bytes.len()) else {
            return;
        };
        if total_file_bytes > ROOT_FILE_CACHE_MAX_BYTES {
            return;
        }
        self.total_file_bytes = total_file_bytes;
        self.files.push(RootFileCacheEntry {
            path: String::from(path),
            bytes,
            last_used,
        });
    }

    fn file_cache_full(&self, incoming_len: usize) -> bool {
        self.total_file_bytes
            .checked_add(incoming_len)
            .map(|total| total > ROOT_FILE_CACHE_MAX_BYTES)
            .unwrap_or(true)
    }

    fn lookup_page(&mut self, generation: u64, path: &str, page_index: u64) -> Option<Arc<[u8]>> {
        self.prepare_generation(generation);
        let last_used = self.next_use();
        let index = self
            .pages
            .iter()
            .position(|entry| entry.path == path && entry.page_index == page_index)?;
        self.pages[index].last_used = last_used;
        Some(Arc::clone(&self.pages[index].bytes))
    }

    fn insert_page(&mut self, generation: u64, path: &str, page_index: u64, bytes: Arc<[u8]>) {
        self.prepare_generation(generation);
        if bytes.len() > ROOT_FILE_PAGE_SIZE {
            return;
        }

        if let Some(index) = self
            .pages
            .iter()
            .position(|entry| entry.path == path && entry.page_index == page_index)
        {
            self.total_page_bytes = self
                .total_page_bytes
                .saturating_sub(self.pages[index].bytes.len());
            self.pages.remove(index);
        }

        while self.pages.len() >= ROOT_FILE_PAGE_CACHE_MAX_ENTRIES
            || self.page_cache_full(bytes.len())
        {
            let Some((index, _)) = self
                .pages
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
            else {
                break;
            };
            self.total_page_bytes = self
                .total_page_bytes
                .saturating_sub(self.pages[index].bytes.len());
            self.pages.remove(index);
        }

        let last_used = self.next_use();
        let Some(total_page_bytes) = self.total_page_bytes.checked_add(bytes.len()) else {
            return;
        };
        if total_page_bytes > ROOT_FILE_PAGE_CACHE_MAX_BYTES {
            return;
        }
        self.total_page_bytes = total_page_bytes;
        self.pages.push(RootPageCacheEntry {
            path: String::from(path),
            page_index,
            bytes,
            last_used,
        });
    }

    fn page_cache_full(&self, incoming_len: usize) -> bool {
        self.total_page_bytes
            .checked_add(incoming_len)
            .map(|total| total > ROOT_FILE_PAGE_CACHE_MAX_BYTES)
            .unwrap_or(true)
    }

    fn lookup_dir(&mut self, generation: u64, path: &str) -> Option<Vec<storage_fat::FatDirEntry>> {
        self.prepare_generation(generation);
        let last_used = self.next_use();
        let index = self.dirs.iter().position(|entry| entry.path == path)?;
        self.dirs[index].last_used = last_used;
        Some(self.dirs[index].entries.clone())
    }

    fn insert_dir(&mut self, generation: u64, path: &str, entries: Vec<storage_fat::FatDirEntry>) {
        self.prepare_generation(generation);
        if let Some(index) = self.dirs.iter().position(|entry| entry.path == path) {
            self.dirs.remove(index);
        }
        while self.dirs.len() >= ROOT_DIR_CACHE_MAX_ENTRIES {
            let Some((index, _)) = self
                .dirs
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
            else {
                break;
            };
            self.dirs.remove(index);
        }
        let last_used = self.next_use();
        self.dirs.push(RootDirCacheEntry {
            path: String::from(path),
            entries,
            last_used,
        });
    }

    fn lookup_metadata(&mut self, generation: u64, path: &str) -> Option<VfsMetadata> {
        self.prepare_generation(generation);
        let last_used = self.next_use();
        let index = self.metadata.iter().position(|entry| entry.path == path)?;
        self.metadata[index].last_used = last_used;
        Some(self.metadata[index].metadata)
    }

    fn insert_metadata(&mut self, generation: u64, path: &str, metadata: VfsMetadata) {
        self.prepare_generation(generation);
        if let Some(index) = self.metadata.iter().position(|entry| entry.path == path) {
            self.metadata.remove(index);
        }
        while self.metadata.len() >= ROOT_METADATA_CACHE_MAX_ENTRIES {
            let Some((index, _)) = self
                .metadata
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
            else {
                break;
            };
            self.metadata.remove(index);
        }
        let last_used = self.next_use();
        self.metadata.push(RootMetadataCacheEntry {
            path: String::from(path),
            metadata,
            last_used,
        });
    }
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
pub fn init() {
    crate::debug::println!("vfs: mount /proc begin");
    let _ = mount_internal("/proc", "proc", 0, None, true);
    crate::debug::println!("vfs: mount /proc done");
    crate::debug::println!("vfs: mount /dev begin");
    let _ = mount_internal("/dev", "devfs", 0, None, true);
    crate::debug::println!("vfs: mount /dev done");
    crate::debug::println!("vfs: mount /run begin");
    let _ = mount_internal("/run", "runfs", 0, None, true);
    crate::debug::println!("vfs: mount /run done");
    crate::debug::println!("vfs: mount / begin");
    match block::current_boot_volume_handle() {
        Some(_handle) => {
            if let Err(error) = mount_internal(
                "/",
                "fat",
                crate::user::linux::MS_RDONLY,
                Some("role=system-image"),
                true,
            ) {
                crate::debug::println!("vfs: mount / failed: {:?}", error);
            }
        }
        None => crate::debug::println!("vfs: mount / skipped: no boot volume block handle"),
    }
    crate::debug::println!("vfs: mount / done");
}

pub fn mount_for_current_process(
    source_path: &str,
    target_path: &str,
    filesystem_type: &str,
    flags: u64,
    options: Option<&str>,
) -> Result<(), MountError> {
    validate_mount_flags(flags)?;
    let source_path = normalize_mount_path(source_path)?;
    let target_path = normalize_mount_path(target_path)?;
    validate_filesystem_type(filesystem_type)?;
    let _source = block::lookup(source_path.as_str()).ok_or(MountError::InvalidSource)?;
    let metadata =
        metadata_for_current_process_path(target_path.as_str()).map_err(MountError::from)?;
    if metadata.kind != VfsNodeKind::Directory {
        return Err(MountError::NotDirectory);
    }
    let _ = (source_path, target_path, filesystem_type, flags, options);
    Err(MountError::UnsupportedFilesystem)
}

pub fn umount_for_current_process(target_path: &str) -> Result<(), MountError> {
    let target_path = normalize_mount_path(target_path)?;
    let target_path = target_path.as_str();
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

    remove_local_mount_mirror(target_path)
        .then_some(())
        .ok_or(MountError::NotFound)
}

fn mount_internal(
    path: &str,
    filesystem_type: &str,
    flags: u64,
    options: Option<&str>,
    pinned: bool,
) -> Result<(), MountError> {
    validate_mount_flags(flags)?;
    let path = normalize_mount_path(path)?;
    validate_filesystem_type(filesystem_type)?;
    let mount_options = parse_mount_options(options)?;

    let mut mounts = MOUNTS.lock();
    if mounts.iter().any(|mount| mount.path == path.as_str()) {
        return Err(MountError::Busy);
    }
    mounts.push(VfsMount {
        path,
        role: mount_options.role,
        pinned,
    });
    bump_mount_generation();
    Ok(())
}

pub fn open_path_for_current_process(
    absolute_path: &str,
    flags: u64,
    mode: u64,
) -> Result<u64, VfsError> {
    if let Some(target) = resolve_fd_link_path(absolute_path)? {
        return open_path_for_current_process(target.as_str(), flags, mode);
    }

    if let Some(opened) = procfs::open_special_path(absolute_path)? {
        let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
            install_open_result(process_state, opened, flags)
        }) else {
            return Err(VfsError::Unsupported);
        };
        return result;
    }

    let mount = resolve_mount(absolute_path)?;
    let retained = multitask::retain_current_user_process_state().ok_or(VfsError::Unsupported)?;
    ensure_user_mount_access(
        &mount,
        absolute_path,
        retained.abi(),
        retained.process_state(),
    )?;
    let opened = open_local_path(absolute_path, flags, mode)?;
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        install_open_result(process_state, opened, flags)
    }) else {
        return Err(VfsError::Unsupported);
    };
    result
}

pub fn metadata_for_current_process_path(absolute_path: &str) -> Result<VfsMetadata, VfsError> {
    if let Some(target) = resolve_fd_link_path(absolute_path)? {
        return metadata_for_current_process_path(target.as_str());
    }

    if let Some(metadata) = procfs::metadata_for_special_path(absolute_path)? {
        return Ok(metadata);
    }

    let mount = resolve_mount(absolute_path)?;
    let retained = multitask::retain_current_user_process_state().ok_or(VfsError::Unsupported)?;
    ensure_user_mount_access(
        &mount,
        absolute_path,
        retained.abi(),
        retained.process_state(),
    )?;
    metadata_local(absolute_path)
}

pub fn read_path_to_vec_for_kernel(absolute_path: &str) -> Result<Vec<u8>, VfsError> {
    let absolute_path = normalize_kernel_path(absolute_path)?;
    let trace_module_path = absolute_path.starts_with("/system/drivers/");
    if trace_module_path {
        crate::debug::println!("kernel vfs: read begin path={}", absolute_path);
    }
    let result = read_root_file_cached(absolute_path.as_str());
    if trace_module_path {
        crate::debug::println!(
            "kernel vfs: rootfs read end path={} status={}",
            absolute_path,
            if result.is_ok() { "ok" } else { "err" },
        );
    }
    result
}

pub fn open_path_for_kernel_file(absolute_path: &str) -> Result<VfsFileHandle, VfsError> {
    let absolute_path = normalize_kernel_path(absolute_path)?;
    match open_local_path(absolute_path.as_str(), crate::user::linux::O_RDONLY, 0)? {
        VfsOpenResult::File(file) => Ok(file),
        _ => Err(VfsError::PermissionDenied),
    }
}

#[derive(Debug)]
struct RootFsStreamingFile {
    path: String,
    len: usize,
}

impl VfsFileObject for RootFsStreamingFile {
    fn path(&self) -> &str {
        self.path.as_str()
    }

    fn len(&self) -> usize {
        self.len
    }

    fn read_at(&self, offset: usize, dest: &mut [u8]) -> usize {
        if dest.is_empty() || offset >= self.len {
            return 0;
        }

        let mut done = 0usize;
        let read_len = dest.len().min(self.len - offset);
        while done < read_len {
            let Some(absolute_offset) = offset.checked_add(done) else {
                break;
            };
            let page_index = absolute_offset / ROOT_FILE_PAGE_SIZE;
            let page_offset = absolute_offset % ROOT_FILE_PAGE_SIZE;
            let page = match read_root_file_page_shared(self.path.as_str(), self.len, page_index) {
                Ok(page) => page,
                Err(_) => break,
            };
            if page_offset >= page.len() {
                break;
            }

            let available = page.len() - page_offset;
            let remaining = read_len - done;
            let to_copy = available.min(remaining);
            dest[done..done + to_copy].copy_from_slice(&page[page_offset..page_offset + to_copy]);
            done += to_copy;
        }

        done
    }

    fn write_at(
        &self,
        _offset: usize,
        _src: &[u8],
    ) -> Result<usize, crate::user::handles::FileHandleWriteError> {
        Err(crate::user::handles::FileHandleWriteError::ReadOnly)
    }
}

pub(crate) fn read_dir_names_for_kernel(absolute_path: &str) -> Result<Vec<String>, VfsError> {
    let absolute_path = normalize_kernel_path(absolute_path)?;
    Ok(read_root_dir_cached(absolute_path.as_str())?
        .into_iter()
        .map(|entry| entry.name)
        .collect())
}

pub fn normalize_kernel_path(path: &str) -> Result<String, VfsError> {
    core_vfs::normalize_kernel_path(path).map_err(|_| VfsError::InvalidArgument)
}

pub(crate) fn check_access_for_current_process(
    absolute_path: &str,
    mode: u64,
) -> Result<(), VfsError> {
    if let Some(target) = resolve_fd_link_path(absolute_path)? {
        return check_access_for_current_process(target.as_str(), mode);
    }

    if procfs::is_local_special_path(absolute_path) {
        ensure_read_access_only(mode)?;
        let _ = metadata_for_special_case_exists(absolute_path)?;
        return Ok(());
    }

    let mount = resolve_mount(absolute_path)?;
    let retained = multitask::retain_current_user_process_state().ok_or(VfsError::Unsupported)?;
    ensure_user_mount_access(
        &mount,
        absolute_path,
        retained.abi(),
        retained.process_state(),
    )?;
    ensure_read_access_only(mode)?;
    metadata_local(absolute_path).map(|_| ())
}

pub(crate) fn readlink_for_current_process(absolute_path: &str) -> Result<String, VfsError> {
    if absolute_path.starts_with("/dev/fd/") || absolute_path.starts_with("/proc/self/fd/") {
        return procfs::read_fd_link(absolute_path);
    }
    Err(VfsError::NotFound)
}

fn resolve_fd_link_path(path: &str) -> Result<Option<String>, VfsError> {
    procfs::fd_link_target(path)
}

fn open_local_path(absolute_path: &str, flags: u64, _mode: u64) -> Result<VfsOpenResult, VfsError> {
    if let Ok(device) = crate::io::device::open(absolute_path) {
        return Ok(VfsOpenResult::Device(device));
    }

    let metadata = metadata_local(absolute_path)?;
    match metadata.kind {
        VfsNodeKind::Device => crate::io::device::open(absolute_path)
            .map(VfsOpenResult::Device)
            .map_err(|_| VfsError::NotFound),
        VfsNodeKind::Directory => read_dir_entries_local(absolute_path).map(|entries| {
            VfsOpenResult::Directory(VfsDirectoryHandle::new(
                String::from(absolute_path),
                entries,
            ))
        }),
        VfsNodeKind::File => {
            validate_read_only_open_flags(flags)?;
            let file_len = usize::try_from(metadata.len).map_err(|_| VfsError::InvalidArgument)?;
            if metadata.len <= ROOT_FILE_CACHE_MAX_ENTRY_BYTES as u64 {
                let bytes = read_root_file_shared(absolute_path)?;
                Ok(VfsOpenResult::File(VfsFileHandle::read_only_memory_shared(
                    String::from(absolute_path),
                    bytes,
                )))
            } else {
                Ok(VfsOpenResult::File(VfsFileHandle::new(Arc::new(
                    RootFsStreamingFile {
                        path: String::from(absolute_path),
                        len: file_len,
                    },
                ))))
            }
        }
    }
}

fn metadata_local(absolute_path: &str) -> Result<VfsMetadata, VfsError> {
    if let Ok(device) = crate::io::device::lookup(absolute_path) {
        return Ok(directory_entry_metadata(
            absolute_path,
            VfsNodeKind::Device,
            0,
            path_inode(device.path.as_bytes()).max(1),
        ));
    }

    if is_virtual_directory(absolute_path) {
        return Ok(directory_entry_metadata(
            absolute_path,
            VfsNodeKind::Directory,
            0,
            path_inode(absolute_path.as_bytes()).max(1),
        ));
    }

    let metadata = read_root_metadata_cached(absolute_path)?;
    Ok(metadata)
}

fn read_root_file_cached(absolute_path: &str) -> Result<Vec<u8>, VfsError> {
    Ok(read_root_file_shared(absolute_path)?.as_ref().to_vec())
}

fn read_root_file_shared(absolute_path: &str) -> Result<Arc<[u8]>, VfsError> {
    let generation = current_mount_generation();
    if let Some(bytes) = ROOT_CACHE.lock().lookup_file(generation, absolute_path) {
        return Ok(bytes);
    }

    let bytes = Arc::<[u8]>::from(read_root_file_to_vec_exact(absolute_path)?.into_boxed_slice());
    ROOT_CACHE
        .lock()
        .insert_file(generation, absolute_path, Arc::clone(&bytes));
    Ok(bytes)
}

fn read_root_file_to_vec_exact(absolute_path: &str) -> Result<Vec<u8>, VfsError> {
    let metadata = read_root_metadata_cached(absolute_path)?;
    if metadata.kind != VfsNodeKind::File {
        return Err(VfsError::PermissionDenied);
    }

    let file_len = usize::try_from(metadata.len).map_err(|_| VfsError::InvalidArgument)?;
    if file_len > ROOT_FILE_EXACT_READ_MAX_BYTES {
        return Err(VfsError::InvalidArgument);
    }
    let mut bytes = vec![0_u8; file_len];
    let path_id = path_inode(absolute_path.as_bytes()).max(1);
    with_root_volume(|volume| {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Vfs,
            "vfs-read-open",
            file_len as u64,
            path_id,
        );
        crate::debug::debug!(
            vfs,
            "vfs exact read open begin path={} len={}",
            absolute_path,
            file_len
        );
        let mut file = volume.open_file(absolute_path)?;
        file.seek(SeekFrom::Start(0))?;
        let mut done = 0usize;
        while done < file_len {
            let chunk_len = ROOT_FILE_PAGE_SIZE.min(file_len - done);
            let read = match file.read(&mut bytes[done..done + chunk_len]) {
                Ok(read) => read,
                Err(fatfs::Error::InvalidInput) => {
                    crate::debug::record_milestone(
                        crate::debug::LogCategory::Vfs,
                        "vfs-read-error",
                        done as u64,
                        path_id,
                    );
                    crate::debug::warn!(
                        vfs,
                        "vfs exact read invalid input path={} done={} len={}",
                        absolute_path,
                        done,
                        file_len
                    );
                    return Err(fatfs::Error::InvalidInput);
                }
                Err(fatfs::Error::Io(crate::storage::fat::DiskIoError::InvalidInput)) => {
                    crate::debug::record_milestone(
                        crate::debug::LogCategory::Vfs,
                        "vfs-read-io-invalid",
                        done as u64,
                        path_id,
                    );
                    return Err(fatfs::Error::Io(
                        crate::storage::fat::DiskIoError::InvalidInput,
                    ));
                }
                Err(fatfs::Error::Io(crate::storage::fat::DiskIoError::UnexpectedEof)) => {
                    crate::debug::record_milestone(
                        crate::debug::LogCategory::Vfs,
                        "vfs-read-io-eof",
                        done as u64,
                        path_id,
                    );
                    return Err(fatfs::Error::Io(
                        crate::storage::fat::DiskIoError::UnexpectedEof,
                    ));
                }
                Err(fatfs::Error::Io(crate::storage::fat::DiskIoError::NotPresent)) => {
                    crate::debug::record_milestone(
                        crate::debug::LogCategory::Vfs,
                        "vfs-read-not-present",
                        done as u64,
                        path_id,
                    );
                    return Err(fatfs::Error::Io(
                        crate::storage::fat::DiskIoError::NotPresent,
                    ));
                }
                Err(error) => {
                    crate::debug::record_milestone(
                        crate::debug::LogCategory::Vfs,
                        "vfs-read-error-other",
                        done as u64,
                        path_id,
                    );
                    return Err(error);
                }
            };
            if read == 0 {
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Vfs,
                    "vfs-read-short",
                    done as u64,
                    path_id,
                );
                return Err(fatfs::Error::Io(
                    crate::storage::fat::DiskIoError::UnexpectedEof,
                ));
            }
            done += read;
            if done == file_len || done % (256 * 1024) == 0 {
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Vfs,
                    "vfs-read-progress",
                    done as u64,
                    file_len as u64,
                );
            }
        }
        crate::debug::debug!(
            vfs,
            "vfs exact read done path={} len={}",
            absolute_path,
            file_len
        );
        Ok(())
    })?;
    Ok(bytes)
}

fn read_root_file_page_shared(
    absolute_path: &str,
    file_len: usize,
    page_index: usize,
) -> Result<Arc<[u8]>, VfsError> {
    let generation = current_mount_generation();
    let page_index_u64 = page_index as u64;
    if let Some(bytes) = ROOT_CACHE
        .lock()
        .lookup_page(generation, absolute_path, page_index_u64)
    {
        return Ok(bytes);
    }

    let offset = page_index
        .checked_mul(ROOT_FILE_PAGE_SIZE)
        .ok_or(VfsError::InvalidArgument)?;
    if offset >= file_len {
        return Ok(Arc::<[u8]>::from([]));
    }

    let page_len = ROOT_FILE_PAGE_SIZE.min(file_len - offset);
    let mut bytes = vec![0_u8; page_len];
    let read = read_root_file_range_into(absolute_path, offset as u64, bytes.as_mut_slice())?;
    bytes.truncate(read.min(page_len));
    let bytes = Arc::<[u8]>::from(bytes.into_boxed_slice());
    ROOT_CACHE.lock().insert_page(
        generation,
        absolute_path,
        page_index_u64,
        Arc::clone(&bytes),
    );
    Ok(bytes)
}

fn read_root_file_range_into(
    absolute_path: &str,
    offset: u64,
    dest: &mut [u8],
) -> Result<usize, VfsError> {
    if dest.is_empty() {
        return Ok(0);
    }

    with_root_volume(|volume| {
        let mut file = volume.open_file(absolute_path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut done = 0usize;
        while done < dest.len() {
            let count = file.read(&mut dest[done..])?;
            if count == 0 {
                break;
            }
            done += count;
        }
        Ok(done)
    })
}

fn read_root_dir_cached(absolute_path: &str) -> Result<Vec<storage_fat::FatDirEntry>, VfsError> {
    let generation = current_mount_generation();
    if let Some(entries) = ROOT_CACHE.lock().lookup_dir(generation, absolute_path) {
        return Ok(entries);
    }

    let entries = with_root_volume(|volume| volume.read_dir(absolute_path))?;
    ROOT_CACHE
        .lock()
        .insert_dir(generation, absolute_path, entries.clone());
    Ok(entries)
}

fn read_root_metadata_cached(absolute_path: &str) -> Result<VfsMetadata, VfsError> {
    let generation = current_mount_generation();
    if let Some(metadata) = ROOT_CACHE.lock().lookup_metadata(generation, absolute_path) {
        return Ok(metadata);
    }

    let metadata = with_root_volume(|volume| volume.metadata(absolute_path))?;
    let kind = match metadata.kind {
        FatNodeKind::File => VfsNodeKind::File,
        FatNodeKind::Directory => VfsNodeKind::Directory,
    };
    let metadata = directory_entry_metadata(
        absolute_path,
        kind,
        metadata.len,
        path_inode(absolute_path.as_bytes()).max(1),
    );
    ROOT_CACHE
        .lock()
        .insert_metadata(generation, absolute_path, metadata);
    Ok(metadata)
}

fn directory_entry_metadata(path: &str, kind: VfsNodeKind, len: u64, inode: u64) -> VfsMetadata {
    let _ = path;
    VfsMetadata {
        inode,
        kind,
        len,
        block_size: 4096,
        blocks: len.div_ceil(512),
        link_count: 1,
        atime: VfsTimestamp::default(),
        mtime: VfsTimestamp::default(),
        ctime: VfsTimestamp::default(),
    }
}

fn read_dir_entries_local(path: &str) -> Result<Vec<VfsDirectoryEntry>, VfsError> {
    if let Some(entries) = virtual_directory_entries(path)? {
        return Ok(entries);
    }

    read_dir_entries_local_rootfs(path)
}

fn virtual_directory_entries(path: &str) -> Result<Option<Vec<VfsDirectoryEntry>>, VfsError> {
    let mut entries = match path {
        "/" => {
            let mut entries = read_dir_entries_local_rootfs("/")?;
            push_virtual_entry(&mut entries, "dev", VfsDirectoryEntryKind::Directory);
            push_virtual_entry(&mut entries, "proc", VfsDirectoryEntryKind::Directory);
            push_virtual_entry(&mut entries, "run", VfsDirectoryEntryKind::Directory);
            return Ok(Some(entries));
        }
        "/dev" => crate::io::device::descriptors()
            .iter()
            .map(|descriptor| {
                VfsDirectoryEntry::new(
                    descriptor
                        .path
                        .trim_start_matches("/dev/")
                        .split('/')
                        .next()
                        .unwrap_or(descriptor.path)
                        .to_string(),
                    path_inode(descriptor.path.as_bytes()).max(1),
                    if descriptor.path.contains('/') {
                        VfsDirectoryEntryKind::Directory
                    } else {
                        VfsDirectoryEntryKind::Device
                    },
                )
            })
            .collect::<Vec<_>>(),
        "/dev/input" => vec![VfsDirectoryEntry::new(
            String::from("event0"),
            path_inode(b"/dev/input/event0").max(1),
            VfsDirectoryEntryKind::Device,
        )],
        "/dev/dri" => vec![VfsDirectoryEntry::new(
            String::from("card0"),
            path_inode(b"/dev/dri/card0").max(1),
            VfsDirectoryEntryKind::Device,
        )],
        "/proc" => vec![
            VfsDirectoryEntry::new(
                String::from("rustos"),
                path_inode(procfs::PROC_RUSTOS_DIR_PATH.as_bytes()).max(1),
                VfsDirectoryEntryKind::Directory,
            ),
            VfsDirectoryEntry::new(
                String::from("self"),
                path_inode(b"/proc/self").max(1),
                VfsDirectoryEntryKind::Directory,
            ),
        ],
        "/proc/rustos" => vec![VfsDirectoryEntry::new(
            String::from("log"),
            path_inode(procfs::PROC_RUSTOS_LOG_PATH.as_bytes()).max(1),
            VfsDirectoryEntryKind::File,
        )],
        "/proc/self" => vec![
            VfsDirectoryEntry::new(
                String::from("fd"),
                path_inode(b"/proc/self/fd").max(1),
                VfsDirectoryEntryKind::Directory,
            ),
            VfsDirectoryEntry::new(
                String::from("maps"),
                path_inode(procfs::PROC_SELF_MAPS_PATH.as_bytes()).max(1),
                VfsDirectoryEntryKind::File,
            ),
        ],
        "/proc/self/fd" | "/dev/fd" => current_process_fd_entries()?,
        "/run" => vec![VfsDirectoryEntry::new(
            String::from("user"),
            path_inode(b"/run/user").max(1),
            VfsDirectoryEntryKind::Directory,
        )],
        "/run/user" => current_runtime_user_entries()?,
        _ => {
            if is_current_runtime_user_dir(path) {
                Vec::new()
            } else {
                return Ok(None);
            }
        }
    };

    entries.sort_by(|lhs, rhs| lhs.name().cmp(rhs.name()));
    entries.dedup_by(|lhs, rhs| lhs.name() == rhs.name());
    Ok(Some(entries))
}

fn read_dir_entries_local_rootfs(path: &str) -> Result<Vec<VfsDirectoryEntry>, VfsError> {
    let entries = read_root_dir_cached(path)?;
    Ok(entries
        .into_iter()
        .map(|entry| {
            let child_path = if path == "/" {
                alloc::format!("/{}", entry.name)
            } else {
                alloc::format!("{}/{}", path.trim_end_matches('/'), entry.name)
            };
            VfsDirectoryEntry::new(
                entry.name,
                path_inode(child_path.as_bytes()).max(1),
                match entry.kind {
                    FatNodeKind::File => VfsDirectoryEntryKind::File,
                    FatNodeKind::Directory => VfsDirectoryEntryKind::Directory,
                },
            )
        })
        .collect())
}

fn with_root_volume<T>(
    f: impl FnOnce(
        &crate::storage::fat::MountedFatVolume<block::FatRegistryDevice>,
    ) -> core::result::Result<T, fatfs::Error<crate::storage::fat::DiskIoError>>,
) -> Result<T, VfsError> {
    let mut root_volume = ROOT_VOLUME.lock();
    if root_volume.is_none() {
        let handle = block::current_boot_volume_handle().ok_or(VfsError::NotFound)?;
        let volume = crate::storage::fat::open_volume(block::FatRegistryDevice::new(handle))
            .map_err(map_bootstrap_fs_error)?;
        *root_volume = Some(volume);
    }
    f(root_volume.as_ref().unwrap()).map_err(map_bootstrap_fs_error)
}

fn push_virtual_entry(dest: &mut Vec<VfsDirectoryEntry>, name: &str, kind: VfsDirectoryEntryKind) {
    if dest.iter().any(|entry| entry.name() == name) {
        return;
    }
    let path = alloc::format!("/{}", name);
    dest.push(VfsDirectoryEntry::new(
        String::from(name),
        path_inode(path.as_bytes()).max(1),
        kind,
    ));
}

fn current_process_fd_entries() -> Result<Vec<VfsDirectoryEntry>, VfsError> {
    let mut entries = Vec::new();
    for fd in 0_u64..4096 {
        if procfs::fd_link_target(alloc::format!("/proc/self/fd/{fd}").as_str())?.is_some() {
            let path = alloc::format!("/proc/self/fd/{fd}");
            entries.push(VfsDirectoryEntry::new(
                fd.to_string(),
                path_inode(path.as_bytes()).max(1),
                VfsDirectoryEntryKind::File,
            ));
        }
    }
    Ok(entries)
}

fn current_runtime_user_entries() -> Result<Vec<VfsDirectoryEntry>, VfsError> {
    let uid = multitask::with_current_process_credentials(|security| security.euid())
        .ok_or(VfsError::Unsupported)?;
    let path = alloc::format!("/run/user/{uid}");
    Ok(vec![VfsDirectoryEntry::new(
        uid.to_string(),
        path_inode(path.as_bytes()).max(1),
        VfsDirectoryEntryKind::Directory,
    )])
}

fn is_current_runtime_user_dir(path: &str) -> bool {
    let Some(uid) = multitask::with_current_process_credentials(|security| security.euid()) else {
        return false;
    };
    path == alloc::format!("/run/user/{uid}")
}

fn is_virtual_directory(path: &str) -> bool {
    matches!(
        path,
        "/" | "/dev"
            | "/dev/input"
            | "/dev/dri"
            | "/proc"
            | "/proc/rustos"
            | "/proc/self"
            | "/proc/self/fd"
            | "/dev/fd"
            | "/run"
            | "/run/user"
    ) || is_current_runtime_user_dir(path)
}

fn metadata_for_special_case_exists(absolute_path: &str) -> Result<(), VfsError> {
    if procfs::metadata_for_special_path(absolute_path)?.is_some() {
        Ok(())
    } else if absolute_path.starts_with("/proc/self/fd/") || absolute_path.starts_with("/dev/fd/") {
        procfs::read_fd_link(absolute_path).map(|_| ())
    } else {
        Err(VfsError::NotFound)
    }
}

pub(crate) fn path_inode(path: &[u8]) -> u64 {
    core_vfs::path_inode(path)
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
pub enum VfsError {
    BadFileDescriptor,
    InvalidArgument,
    NotFound,
    NotDirectory,
    PermissionDenied,
    ReadOnlyFilesystem,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfsNodeKind {
    File,
    Directory,
    Device,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VfsTimestamp {
    pub sec: i64,
    pub nsec: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsMetadata {
    pub inode: u64,
    pub kind: VfsNodeKind,
    pub len: u64,
    pub block_size: u64,
    pub blocks: u64,
    pub link_count: u64,
    pub atime: VfsTimestamp,
    pub mtime: VfsTimestamp,
    pub ctime: VfsTimestamp,
}

pub(crate) enum VfsOpenResult {
    File(VfsFileHandle),
    Directory(VfsDirectoryHandle),
    Device(DeviceHandle),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountError {
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

struct VfsMount {
    path: String,
    role: MountRole,
    pinned: bool,
}

struct ResolvedMount {
    role: MountRole,
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

    Ok(ResolvedMount { role: best.role })
}

fn validate_mount_flags(flags: u64) -> Result<(), MountError> {
    core_vfs::validate_mount_flags(flags, SUPPORTED_MOUNT_FLAGS).map_err(|error| match error {
        core_vfs::MountConfigError::InvalidArgument => MountError::InvalidArgument,
        core_vfs::MountConfigError::UnsupportedMountFlags => MountError::UnsupportedMountFlags,
    })
}

fn parse_mount_options(options: Option<&str>) -> Result<core_vfs::ParsedMountOptions, MountError> {
    core_vfs::parse_mount_options(options).map_err(|_| MountError::InvalidArgument)
}

fn normalize_mount_path(path: &str) -> Result<String, MountError> {
    core_vfs::normalize_kernel_path(path).map_err(|_| MountError::InvalidArgument)
}

fn validate_filesystem_type(filesystem_type: &str) -> Result<(), MountError> {
    let filesystem_type = filesystem_type.trim();
    if filesystem_type.is_empty()
        || filesystem_type.len() > 64
        || !filesystem_type
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(MountError::UnsupportedFilesystem);
    }
    Ok(())
}

fn ensure_user_mount_access(
    mount: &ResolvedMount,
    absolute_path: &str,
    abi: UserAbi,
    process_state: &UserProcessState,
) -> Result<(), VfsError> {
    if mount.role != MountRole::SystemImage {
        return Ok(());
    }

    if runtime::linux_runtime_access_allows_path(absolute_path, abi, process_state) {
        return Ok(());
    }

    if process_state.security().is_logical_admin() {
        Ok(())
    } else {
        let _ = multitask::with_current_user_process_state_mut(|_, _, process_state| {
            let _ = process_state.require_logical_admin_for_file_access(absolute_path);
        });
        Err(VfsError::PermissionDenied)
    }
}

fn bump_mount_generation() {
    MOUNT_GENERATION.fetch_add(1, Ordering::Relaxed);
}

pub(super) fn current_mount_generation() -> u64 {
    MOUNT_GENERATION.load(Ordering::Relaxed)
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
        VfsOpenResult::Device(handle) => process_state.handles_mut().install_with_open_flags(
            KernelHandle::Device(legacy_device_handle(handle)),
            open_flags,
        ),
    })
}

fn legacy_device_handle(handle: DeviceHandle) -> kernel_object::api::device::DeviceHandle {
    kernel_object::api::device::DeviceHandle::with_access(
        legacy_device_id(handle.device_id()),
        legacy_device_access_kind(handle.access_kind()),
    )
}

fn legacy_device_id(id: crate::io::device::DeviceId) -> kernel_object::api::device::DeviceId {
    match id {
        crate::io::device::DeviceId::Console => kernel_object::api::device::DeviceId::Console,
        crate::io::device::DeviceId::Display => kernel_object::api::device::DeviceId::Display,
        crate::io::device::DeviceId::Input => kernel_object::api::device::DeviceId::Input,
    }
}

fn legacy_device_access_kind(
    kind: crate::io::device::DeviceAccessKind,
) -> kernel_object::api::device::DeviceAccessKind {
    match kind {
        crate::io::device::DeviceAccessKind::Native => {
            kernel_object::api::device::DeviceAccessKind::Native
        }
        crate::io::device::DeviceAccessKind::Evdev => {
            kernel_object::api::device::DeviceAccessKind::Evdev
        }
    }
}

fn path_is_within_mount(absolute_path: &str, mount_path: &str) -> bool {
    core_vfs::path_is_within_mount(absolute_path, mount_path)
}

fn remove_local_mount_mirror(target_path: &str) -> bool {
    let mut mounts = MOUNTS.lock();
    let Some(index) = mounts.iter().position(|mount| mount.path == target_path) else {
        return false;
    };
    mounts.remove(index);
    bump_mount_generation();
    true
}

fn map_bootstrap_fs_error(error: fatfs::Error<crate::storage::fat::DiskIoError>) -> VfsError {
    match error {
        fatfs::Error::NotFound => VfsError::NotFound,
        fatfs::Error::InvalidInput => VfsError::InvalidArgument,
        fatfs::Error::Io(crate::storage::fat::DiskIoError::InvalidInput) => {
            VfsError::InvalidArgument
        }
        _ => VfsError::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use spin::Mutex;

    use super::{
        MOUNTS, MountError, MountRole, mount_internal, normalize_kernel_path, path_is_within_mount,
        resolve_mount, umount_for_current_process,
    };

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset_for_tests() {
        MOUNTS.lock().clear();
    }

    #[test]
    fn normalize_kernel_path_accepts_absolute_and_relative() {
        assert_eq!(normalize_kernel_path("/bin/init").unwrap(), "/bin/init");
        assert_eq!(normalize_kernel_path("bin/init").unwrap(), "/bin/init");
        assert_eq!(
            normalize_kernel_path("/lib64//./x86_64-linux-gnu/../ld-linux-x86-64.so.2").unwrap(),
            "/lib64/ld-linux-x86-64.so.2"
        );
        assert!(normalize_kernel_path("").is_err());
        assert!(normalize_kernel_path("/lib64/\0ld-linux-x86-64.so.2").is_err());
    }

    #[test]
    fn mount_matching_helpers_handle_root_and_nested_mounts() {
        assert!(path_is_within_mount("/mnt/data/file", "/mnt"));
        assert!(!path_is_within_mount("/mnt2/data", "/mnt"));
        assert_eq!(
            crate::vfs_core::path_relative_to_mount("/mnt/data/file", "/mnt"),
            "/data/file"
        );
        assert_eq!(
            crate::vfs_core::path_relative_to_mount("/data/file", "/"),
            "/data/file"
        );
    }

    #[test]
    fn resolve_mount_prefers_nested_mounts_and_blocks_parent_unmount() {
        let _guard = TEST_LOCK.lock();
        reset_for_tests();
        mount_internal("/", "bootfs", 0, None, false).unwrap();
        mount_internal("/mnt", "bootfs", 0, None, false).unwrap();
        mount_internal("/mnt/sub", "bootfs", 0, Some("role=system-image"), false).unwrap();

        let resolved = resolve_mount("/mnt/sub/file").unwrap();
        assert_eq!(resolved.role, MountRole::SystemImage);
        assert_eq!(umount_for_current_process("/mnt"), Err(MountError::Busy));
    }
}
