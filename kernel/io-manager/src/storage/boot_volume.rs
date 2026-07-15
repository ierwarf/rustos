// RING3-MIGRATION-REFERENCE START: bootstrap exception: vfsd/storaged own
// post-bootstrap root extent leases and normal runtime boot-volume policy.
// Ring0 keeps boot info, bootloader-supplied root extent reads, and physical
// boot-volume substrate until vfsd/storaged can serve the root filesystem.
#![cfg_attr(not(test), allow(dead_code))]

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use boot_protocol::{
    BootExtentManifest, BootInfo, BootVolumeIdentity, BootVolumeTransport, FramebufferInfo,
};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};

use crate::sync::{KernelSpinLock as Mutex, KernelWaitLock};
use fatfs::{IoBase, Read, Seek, SeekFrom, Write};
use storage_core::BlockDevice;

use crate::storage::fat::{self, DiskIoError};

pub use crate::storage::fat::{BootVolumeDirEntry, BootVolumeMetadata};

static BOOT_INFO_PTR: AtomicPtr<BootInfo> = AtomicPtr::new(ptr::null_mut());
static PHYSICAL_BOOT_BLOCK_DEVICE_OPENER: Mutex<Option<PhysicalBootBlockDeviceOpener>> =
    Mutex::new(None);
static BOOTSTRAP_PHASE: AtomicU8 = AtomicU8::new(BootstrapPhase::EarlyBootstrap as u8);

pub type PhysicalBootBlockDeviceOpener =
    fn(BootVolumeIdentity) -> core::result::Result<Box<dyn BlockDevice>, fatfs::Error<DiskIoError>>;

type BootVolumeFs = fat::MountedFatVolume<Box<dyn BlockDevice>>;
type PhysicalBootVolumeFileInner<'a> = storage_fat::FatFile<'a, Box<dyn BlockDevice>>;
const BOOT_VOLUME_READ_CHUNK_CAP: usize = 64 * 1024;
const ROOT_EXTENT_READ_CHUNK_CAP: usize = 512 * 1024;
const ROOT_EXTENT_FILE_CACHE_CAPACITY: usize = 8;
const ROOT_EXTENT_FILE_CACHE_MAX_ENTRY_BYTES: usize = 768 * 1024;
const ROOT_EXTENT_FILE_CACHE_MAX_BYTES: usize = 2 * 1024 * 1024;

static ROOT_FILE_EXTENTS: KernelWaitLock<RootFileExtentState> =
    KernelWaitLock::new(RootFileExtentState::Uninitialized);
static ROOT_EXTENT_FILE_CACHE: KernelWaitLock<Vec<RootExtentFileCacheEntry>> =
    KernelWaitLock::new(Vec::new());
static ROOT_EXTENT_LOGS_REMAINING: AtomicUsize = AtomicUsize::new(32);

#[derive(Clone, Debug)]
struct RootFileExtent {
    offset: u64,
    len: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootFileExtent {
    pub offset: u64,
    pub len: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootFileExtentLease {
    pub len: u64,
    pub generation: u64,
    pub extents: Vec<BootFileExtent>,
}

#[derive(Clone, Debug)]
struct RootFileExtentEntry {
    path: String,
    len: u64,
    extents: Vec<RootFileExtent>,
}

#[derive(Debug)]
struct RootExtentFileCacheEntry {
    path: String,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct RootFileExtentTable {
    entries: Vec<RootFileExtentEntry>,
}

#[derive(Debug)]
enum RootFileExtentState {
    Uninitialized,
    Ready(RootFileExtentTable),
    Disabled,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapPhase {
    EarlyBootstrap = 0,
    CoreHostsLaunching = 1,
    KernelVfsReady = 2,
    UserspaceReady = 3,
}

impl BootstrapPhase {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::CoreHostsLaunching,
            2 => Self::KernelVfsReady,
            3 => Self::UserspaceReady,
            _ => Self::EarlyBootstrap,
        }
    }
}

pub struct PhysicalBootVolume {
    fs: BootVolumeFs,
}

pub struct PhysicalBootVolumeFile<'a>(PhysicalBootVolumeFileInner<'a>);

pub fn init_boot_info(boot_info_ptr: *const BootInfo) {
    BOOT_INFO_PTR.store(boot_info_ptr.cast_mut(), Ordering::Release);
}

pub fn bootstrap_phase() -> BootstrapPhase {
    BootstrapPhase::from_raw(BOOTSTRAP_PHASE.load(Ordering::Acquire))
}

pub fn kernel_vfs_runtime_active() -> bool {
    matches!(
        bootstrap_phase(),
        BootstrapPhase::KernelVfsReady | BootstrapPhase::UserspaceReady
    )
}

pub fn userspace_runtime_active() -> bool {
    bootstrap_phase() == BootstrapPhase::UserspaceReady
}

fn set_bootstrap_phase(phase: BootstrapPhase) {
    BOOTSTRAP_PHASE.store(phase as u8, Ordering::Release);
    crate::debug::println!("bootstrap phase -> {:?}", phase);
}

pub fn enter_kernel_vfs_runtime() {
    seal_boot_volume_fat_runtime();
    set_bootstrap_phase(BootstrapPhase::KernelVfsReady);
}

pub fn enter_userspace_runtime() {
    set_bootstrap_phase(BootstrapPhase::UserspaceReady);
}

pub fn boot_framebuffer_info() -> Option<FramebufferInfo> {
    boot_info().map(|info| info.framebuffer)
}

pub fn boot_volume_identity() -> Option<BootVolumeIdentity> {
    let identity = boot_info()?.boot_volume;
    identity.is_present().then_some(identity)
}

/// Multiboot2 supplies the root extent manifest but not a physical-volume
/// identity. In that format, the block substrate may use only an unambiguous
/// FAT volume selected for this manifest; it may never guess when an identity
/// was supplied but did not match.
pub fn boot_extent_manifest_present() -> bool {
    boot_extent_manifest().is_some()
}

pub fn boot_volume_transport_hint() -> Option<BootVolumeTransport> {
    Some(boot_info()?.boot_volume.transport())
}

pub fn set_physical_boot_block_device_opener(opener: PhysicalBootBlockDeviceOpener) {
    *PHYSICAL_BOOT_BLOCK_DEVICE_OPENER.lock() = Some(opener);
}

impl IoBase for PhysicalBootVolumeFile<'_> {
    type Error = fatfs::Error<DiskIoError>;
}

impl Read for PhysicalBootVolumeFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        self.0.read(buf)
    }
}

impl Write for PhysicalBootVolumeFile<'_> {
    fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.0.flush()
    }
}

impl Seek for PhysicalBootVolumeFile<'_> {
    fn seek(&mut self, pos: SeekFrom) -> core::result::Result<u64, fatfs::Error<DiskIoError>> {
        self.0.seek(pos)
    }
}

impl PhysicalBootVolumeFile<'_> {
    pub fn truncate(&mut self) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.0.truncate()
    }
}

fn read_file_to_vec_from_fs(
    fs: &BootVolumeFs,
    path: &str,
) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
    let len = usize::try_from(fs.metadata(path)?.len).map_err(|_| fatfs::Error::InvalidInput)?;
    let mut bytes = vec![0_u8; len];
    let read = read_file_into_from_fs(fs, path, &mut bytes, |_, _| {})?;
    bytes.truncate(read);
    Ok(bytes)
}

fn normalized_extent_path(path: &str) -> Option<&str> {
    let path = path.strip_prefix('/').unwrap_or(path);
    (!path.is_empty() && !path.contains("..")).then_some(path)
}

fn ensure_root_file_extent_table_loaded() {
    if !matches!(
        *ROOT_FILE_EXTENTS.lock(),
        RootFileExtentState::Uninitialized
    ) {
        return;
    }

    if kernel_vfs_runtime_active() {
        crate::debug::warn!(
            storage,
            "boot volume extents: late load rejected phase={:?}",
            bootstrap_phase()
        );
        *ROOT_FILE_EXTENTS.lock() = RootFileExtentState::Disabled;
        return;
    }

    let loaded = match load_root_file_extent_table() {
        Ok(table) => {
            crate::debug::info!(
                storage,
                "boot volume extents: loaded entries={}",
                table.entries.len()
            );
            RootFileExtentState::Ready(table)
        }
        Err(err) => {
            crate::debug::warn!(storage, "boot volume extents: disabled error={:?}", err);
            RootFileExtentState::Disabled
        }
    };

    let mut cache = ROOT_FILE_EXTENTS.lock();
    if matches!(*cache, RootFileExtentState::Uninitialized) {
        *cache = loaded;
    }
}

fn seal_boot_volume_fat_runtime() {
    ensure_root_file_extent_table_loaded();
    crate::debug::info!(storage, "boot volume helper: extent manifest sealed");
}

fn read_file_to_vec_from_extents(
    path: &str,
) -> core::result::Result<Option<Vec<u8>>, fatfs::Error<DiskIoError>> {
    let Some(path) = normalized_extent_path(path) else {
        return Ok(None);
    };
    ensure_root_file_extent_table_loaded();
    let cache = ROOT_FILE_EXTENTS.lock();
    let entry = match &*cache {
        RootFileExtentState::Ready(table) => table.find(path).cloned(),
        _ => None,
    };
    drop(cache);
    let Some(entry) = entry else {
        if path.starts_with("system/registry/") {
            crate::debug::warn!(storage, "boot volume extents: miss path={}", path);
        }
        return Ok(None);
    };
    trace_extent_read(entry.path.as_str(), entry.len);
    if let Some(bytes) = root_extent_file_cache_lookup(entry.path.as_str()) {
        return Ok(Some(bytes));
    }
    let bytes = read_extent_entry(&entry)?;
    root_extent_file_cache_store(entry.path.as_str(), &bytes);
    Ok(Some(bytes))
}

pub fn read_file_range_from_extents(
    path: &str,
    file_offset: u64,
    dest: &mut [u8],
) -> core::result::Result<Option<usize>, fatfs::Error<DiskIoError>> {
    let Some(path) = normalized_extent_path(path) else {
        return Ok(None);
    };
    ensure_root_file_extent_table_loaded();
    let cache = ROOT_FILE_EXTENTS.lock();
    let entry = match &*cache {
        RootFileExtentState::Ready(table) => table.find(path).cloned(),
        _ => None,
    };
    drop(cache);
    let Some(entry) = entry else {
        return Ok(None);
    };
    if dest.is_empty() || file_offset >= entry.len {
        return Ok(Some(0));
    }

    let requested = dest
        .len()
        .min(usize::try_from(entry.len - file_offset).map_err(|_| fatfs::Error::InvalidInput)?);
    let request_end = file_offset
        .checked_add(requested as u64)
        .ok_or(fatfs::Error::InvalidInput)?;
    let handle = crate::storage::block::current_boot_volume_handle()
        .ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
    let mut device = crate::storage::block::FatRegistryDevice::new(handle);
    let block_size = Some(device.logical_block_size())
        .filter(|block_size| *block_size != 0)
        .ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;

    let mut logical_start = 0_u64;
    let mut written = 0usize;
    for extent in &entry.extents {
        let logical_end = logical_start
            .checked_add(extent.len)
            .ok_or(fatfs::Error::InvalidInput)?;
        let overlap_start = file_offset.max(logical_start);
        let overlap_end = request_end.min(logical_end);
        if overlap_start < overlap_end {
            let within_extent = overlap_start - logical_start;
            let physical_offset = extent
                .offset
                .checked_add(within_extent)
                .ok_or(fatfs::Error::InvalidInput)?;
            let count = usize::try_from(overlap_end - overlap_start)
                .map_err(|_| fatfs::Error::InvalidInput)?;
            read_extent_bytes(
                &mut device,
                block_size,
                usize::try_from(physical_offset).map_err(|_| fatfs::Error::InvalidInput)?,
                count,
                &mut dest[written..written + count],
            )?;
            written += count;
            if written == requested {
                break;
            }
        }
        logical_start = logical_end;
    }
    if written != requested {
        return Err(fatfs::Error::UnexpectedEof);
    }
    Ok(Some(written))
}

fn metadata_from_extents(
    path: &str,
) -> core::result::Result<Option<BootVolumeMetadata>, fatfs::Error<DiskIoError>> {
    let Some(path) = normalized_extent_path(path) else {
        return Ok(None);
    };
    ensure_root_file_extent_table_loaded();
    let cache = ROOT_FILE_EXTENTS.lock();
    let RootFileExtentState::Ready(table) = &*cache else {
        return Ok(None);
    };
    Ok(table.find(path).map(|entry| BootVolumeMetadata {
        kind: storage_fat::FatNodeKind::File,
        len: entry.len,
    }))
}

fn trace_extent_read(path: &str, len: u64) {
    if ROOT_EXTENT_LOGS_REMAINING
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok()
    {
        crate::debug::info!(
            storage,
            "boot volume extents: read path={} len={}",
            path,
            len
        );
    }
}

fn root_extent_file_cache_lookup(path: &str) -> Option<Vec<u8>> {
    let mut cache = ROOT_EXTENT_FILE_CACHE.lock();
    let index = cache.iter().position(|entry| entry.path == path)?;
    let entry = cache.remove(index);
    let bytes = entry.bytes.clone();
    cache.push(entry);
    Some(bytes)
}

fn root_extent_file_cache_store(path: &str, bytes: &[u8]) {
    if bytes.is_empty() || bytes.len() > ROOT_EXTENT_FILE_CACHE_MAX_ENTRY_BYTES {
        return;
    }
    let mut cache = ROOT_EXTENT_FILE_CACHE.lock();
    if let Some(index) = cache.iter().position(|entry| entry.path == path) {
        let mut entry = cache.remove(index);
        entry.bytes.clear();
        entry.bytes.extend_from_slice(bytes);
        cache.push(entry);
        return;
    }
    while cache.len() >= ROOT_EXTENT_FILE_CACHE_CAPACITY
        || root_extent_file_cache_bytes(&cache).saturating_add(bytes.len())
            > ROOT_EXTENT_FILE_CACHE_MAX_BYTES
    {
        if cache.is_empty() {
            break;
        }
        cache.remove(0);
    }
    cache.push(RootExtentFileCacheEntry {
        path: path.to_string(),
        bytes: bytes.to_vec(),
    });
}

fn root_extent_file_cache_bytes(cache: &[RootExtentFileCacheEntry]) -> usize {
    cache.iter().map(|entry| entry.bytes.len()).sum()
}

pub fn boot_file_extent_lease(
    path: &str,
) -> core::result::Result<Option<BootFileExtentLease>, fatfs::Error<DiskIoError>> {
    let Some(path) = normalized_extent_path(path) else {
        return Ok(None);
    };
    ensure_root_file_extent_table_loaded();
    let cache = ROOT_FILE_EXTENTS.lock();
    let RootFileExtentState::Ready(table) = &*cache else {
        return Ok(None);
    };
    Ok(table.find(path).map(|entry| BootFileExtentLease {
        len: entry.len,
        generation: root_file_extent_generation(entry),
        extents: entry
            .extents
            .iter()
            .map(|extent| BootFileExtent {
                offset: extent.offset,
                len: extent.len,
            })
            .collect(),
    }))
}

fn root_file_extent_generation(entry: &RootFileExtentEntry) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in entry.path.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for value in [entry.len] {
        for byte in value.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    for extent in &entry.extents {
        for value in [extent.offset, extent.len] {
            for byte in value.to_le_bytes() {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    hash.max(1)
}

impl RootFileExtentTable {
    fn find(&self, path: &str) -> Option<&RootFileExtentEntry> {
        self.entries.iter().find(|entry| entry.path == path)
    }
}

fn load_root_file_extent_table()
-> core::result::Result<RootFileExtentTable, fatfs::Error<DiskIoError>> {
    if let Some(manifest) = boot_extent_manifest() {
        crate::debug::info!(
            storage,
            "boot volume extents: manifest ptr={:#x} len={}",
            manifest.ptr,
            manifest.len
        );
        crate::debug::info!(
            storage,
            "boot volume extents: loading BootInfo manifest len={}",
            manifest.len
        );
        let bytes = unsafe {
            core::slice::from_raw_parts(
                manifest.ptr as *const u8,
                usize::try_from(manifest.len).map_err(|_| fatfs::Error::InvalidInput)?,
            )
        };
        let table = parse_root_file_extent_table(bytes).map_err(|_| fatfs::Error::InvalidInput)?;
        crate::debug::info!(
            storage,
            "boot volume extents: parsed entries={}",
            table.entries.len()
        );
        return Ok(table);
    }

    crate::debug::warn!(storage, "boot volume extents: missing BootInfo manifest");
    Err(fatfs::Error::Io(DiskIoError::NotPresent))
}

fn parse_root_file_extent_table(bytes: &[u8]) -> Result<RootFileExtentTable, ()> {
    let text = core::str::from_utf8(bytes).map_err(|_| ())?;
    let mut entries = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let path = registry_field(line, "path").ok_or(())?;
        let len = registry_field(line, "len")
            .ok_or(())?
            .parse::<u64>()
            .map_err(|_| ())?;
        let extents = parse_extent_list(registry_field(line, "extents").ok_or(())?)?;
        let path = normalized_extent_path(path).ok_or(())?.to_string();
        entries.push(RootFileExtentEntry { path, len, extents });
    }
    Ok(RootFileExtentTable { entries })
}

fn registry_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split('\t').find_map(|field| {
        let (field_key, value) = field.split_once('=')?;
        (field_key == key).then_some(value)
    })
}

fn parse_extent_list(text: &str) -> Result<Vec<RootFileExtent>, ()> {
    let mut extents = Vec::new();
    if text.is_empty() {
        return Ok(extents);
    }
    for item in text.split(',') {
        let (offset, len) = item.split_once(':').ok_or(())?;
        let offset = offset.parse::<u64>().map_err(|_| ())?;
        let len = len.parse::<u64>().map_err(|_| ())?;
        if len == 0 {
            continue;
        }
        extents.push(RootFileExtent { offset, len });
    }
    Ok(extents)
}

fn read_extent_entry(
    entry: &RootFileExtentEntry,
) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
    let len = usize::try_from(entry.len).map_err(|_| fatfs::Error::InvalidInput)?;
    let mut bytes = vec![0_u8; len];
    let mut written = 0usize;
    let handle = crate::storage::block::current_boot_volume_handle()
        .ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
    let mut device = crate::storage::block::FatRegistryDevice::new(handle);
    let block_size = Some(device.logical_block_size())
        .filter(|block_size| *block_size != 0)
        .ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;

    for extent in &entry.extents {
        if written == bytes.len() {
            break;
        }
        let extent_offset =
            usize::try_from(extent.offset).map_err(|_| fatfs::Error::InvalidInput)?;
        let extent_len = usize::try_from(extent.len).map_err(|_| fatfs::Error::InvalidInput)?;
        let readable = extent_len.min(bytes.len() - written);
        if readable == 0 {
            continue;
        }
        read_extent_bytes(
            &mut device,
            block_size,
            extent_offset,
            readable,
            &mut bytes[written..written + readable],
        )?;
        written += readable;
    }
    if written != bytes.len() {
        return Err(fatfs::Error::UnexpectedEof);
    }
    Ok(bytes)
}

fn read_extent_bytes(
    device: &mut crate::storage::block::FatRegistryDevice,
    block_size: usize,
    mut offset: usize,
    mut len: usize,
    dest: &mut [u8],
) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
    let mut done = 0usize;
    while len != 0 {
        let block_offset = offset % block_size;
        let aligned_offset = offset - block_offset;
        let chunk_payload = len.min(ROOT_EXTENT_READ_CHUNK_CAP);
        let read_len = (block_offset + chunk_payload).div_ceil(block_size) * block_size;
        let lba = (aligned_offset / block_size) as u64;
        let mut scratch = vec![0_u8; read_len];
        device
            .read_blocks(lba, scratch.as_mut_slice())
            .map_err(fatfs::Error::Io)?;
        dest[done..done + chunk_payload]
            .copy_from_slice(&scratch[block_offset..block_offset + chunk_payload]);
        done += chunk_payload;
        offset += chunk_payload;
        len -= chunk_payload;
        if len != 0 {
            crate::multitask::cond_resched();
        }
    }
    Ok(())
}
fn read_file_into_from_fs(
    fs: &BootVolumeFs,
    path: &str,
    dest: &mut [u8],
    mut after_chunk: impl FnMut(usize, usize),
) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
    let mut file = fs.open_file(path)?;
    let mut done = 0usize;
    while done < dest.len() {
        let remaining = dest.len() - done;
        let chunk_len = remaining.min(BOOT_VOLUME_READ_CHUNK_CAP);
        let count = file.read(&mut dest[done..done + chunk_len])?;
        after_chunk(done, count);
        if count == 0 {
            break;
        }
        done += count;
        if done < dest.len() {
            crate::multitask::cond_resched();
        }
    }
    Ok(done)
}

impl PhysicalBootVolume {
    pub fn open(
        identity: BootVolumeIdentity,
    ) -> core::result::Result<Self, fatfs::Error<DiskIoError>> {
        if identity.validate().is_err() || !identity.is_present() {
            return Err(fatfs::Error::Io(DiskIoError::NotPresent));
        }
        let opener = (*PHYSICAL_BOOT_BLOCK_DEVICE_OPENER.lock())
            .ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
        let device = opener(identity)?;
        let fs = fat::open_volume(device)?;
        Ok(Self { fs })
    }

    pub fn open_current() -> core::result::Result<Self, fatfs::Error<DiskIoError>> {
        let identity = boot_volume_identity().ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
        Self::open(identity)
    }

    pub fn open_or_create_truncated_file(
        &self,
        path: &str,
    ) -> core::result::Result<PhysicalBootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        let mut file = self.create_file(path)?;
        file.seek(SeekFrom::Start(0))?;
        file.truncate()?;
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    }

    pub fn open_or_create_append_file(
        &self,
        path: &str,
    ) -> core::result::Result<PhysicalBootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        let mut file = self.create_file(path)?;
        file.seek(SeekFrom::End(0))?;
        Ok(file)
    }

    pub fn open_file(
        &self,
        path: &str,
    ) -> core::result::Result<PhysicalBootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        self.fs.open_file(path).map(PhysicalBootVolumeFile)
    }

    pub fn create_file(
        &self,
        path: &str,
    ) -> core::result::Result<PhysicalBootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        self.fs.create_file(path).map(PhysicalBootVolumeFile)
    }

    pub fn metadata(
        &self,
        path: &str,
    ) -> core::result::Result<BootVolumeMetadata, fatfs::Error<DiskIoError>> {
        self.fs.metadata(path)
    }

    pub fn read_dir(
        &self,
        path: &str,
    ) -> core::result::Result<Vec<BootVolumeDirEntry>, fatfs::Error<DiskIoError>> {
        self.fs.read_dir(path)
    }

    pub fn create_dir(&self, path: &str) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.create_dir(path)
    }

    pub fn remove_file(&self, path: &str) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.remove_file(path)
    }

    pub fn remove_dir(&self, path: &str) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.remove_dir(path)
    }

    pub fn rename(
        &self,
        src: &str,
        dst: &str,
    ) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.rename(src, dst)
    }

    pub fn read_file_to_vec(
        &self,
        path: &str,
    ) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
        read_file_to_vec_from_fs(&self.fs, path)
    }

    pub fn read_file_into(
        &self,
        path: &str,
        dest: &mut [u8],
    ) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        read_file_into_from_fs(&self.fs, path, dest, |_, _| {})
    }

    pub fn append_bytes(
        &self,
        path: &str,
        bytes: &[u8],
        flush: bool,
    ) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        let mut file = self.open_or_create_append_file(path)?;
        let mut written = 0usize;
        while written < bytes.len() {
            let count = file.write(&bytes[written..])?;
            if count == 0 {
                return Err(fatfs::Error::Io(DiskIoError::WriteZero));
            }
            written += count;
        }
        if flush {
            file.flush()?;
        }
        Ok(())
    }

    pub fn close(self) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.unmount()
    }
}

pub fn read_file_to_vec(path: &str) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
    if let Some(bytes) = read_file_to_vec_from_extents(path)? {
        return Ok(bytes);
    }
    if path == "services/rootd/rootd.elf" {
        let (state, entries) = match &*ROOT_FILE_EXTENTS.lock() {
            RootFileExtentState::Uninitialized => ("uninitialized", 0),
            RootFileExtentState::Ready(table) => ("ready", table.entries.len()),
            RootFileExtentState::Disabled => ("disabled", 0),
        };
        crate::debug::warn!(
            storage,
            "boot volume extents: services/rootd lookup state={} entries={}",
            state,
            entries
        );
    }
    Err(fatfs::Error::NotFound)
}

pub fn read_file_into(
    path: &str,
    dest: &mut [u8],
) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
    if let Some(bytes) = read_file_to_vec_from_extents(path)? {
        let len = bytes.len().min(dest.len());
        dest[..len].copy_from_slice(&bytes[..len]);
        return Ok(len);
    }
    Err(fatfs::Error::NotFound)
}

pub fn metadata(path: &str) -> core::result::Result<BootVolumeMetadata, fatfs::Error<DiskIoError>> {
    if let Some(metadata) = metadata_from_extents(path)? {
        return Ok(metadata);
    }
    Err(fatfs::Error::NotFound)
}

pub fn read_dir(
    path: &str,
) -> core::result::Result<Vec<BootVolumeDirEntry>, fatfs::Error<DiskIoError>> {
    let _ = path;
    Err(fatfs::Error::Io(DiskIoError::Unsupported))
}

fn boot_info() -> Option<&'static BootInfo> {
    let boot_info_ptr = BOOT_INFO_PTR.load(Ordering::Acquire);
    unsafe { BootInfo::from_ptr(boot_info_ptr.cast_const()) }.ok()
}

fn boot_extent_manifest() -> Option<BootExtentManifest> {
    let manifest = boot_info()?.boot_extent_manifest;
    manifest.validate().is_ok().then_some(manifest)
}
// RING3-MIGRATION-REFERENCE END: vfsd/storaged-owned boot-volume policy bootstrap exception.
