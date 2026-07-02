// RING3-MIGRATION-REFERENCE START: rootd/vfsd/storaged should own post-bootstrap
// root extent policy and normal runtime boot-volume file access. Ring0 keeps
// early bootstrap file reads and physical boot-volume substrate.
#![cfg_attr(not(test), allow(dead_code))]

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use boot_protocol::{BootInfo, BootVolumeIdentity, BootVolumeTransport, FramebufferInfo};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};

use crate::sync::{KernelSpinLock as Mutex, KernelWaitLock};
use fatfs::{IoBase, Read, Seek, SeekFrom, Write};
use storage_core::BlockDevice;

use crate::storage::fat::{self, DiskIoError};

pub use crate::storage::fat::{BootVolumeDirEntry, BootVolumeMetadata};

static BOOT_INFO_PTR: AtomicPtr<BootInfo> = AtomicPtr::new(ptr::null_mut());
static BOOT_BLOCK_DEVICE_OPENER: Mutex<Option<BootBlockDeviceOpener>> = Mutex::new(None);
static PHYSICAL_BOOT_BLOCK_DEVICE_OPENER: Mutex<Option<PhysicalBootBlockDeviceOpener>> =
    Mutex::new(None);
static BOOTSTRAP_PHASE: AtomicU8 = AtomicU8::new(BootstrapPhase::EarlyBootstrap as u8);

pub type BootBlockDeviceOpener =
    fn() -> core::result::Result<Box<dyn BlockDevice>, fatfs::Error<DiskIoError>>;
pub type PhysicalBootBlockDeviceOpener =
    fn(BootVolumeIdentity) -> core::result::Result<Box<dyn BlockDevice>, fatfs::Error<DiskIoError>>;

type BootVolumeFs = fat::MountedFatVolume<Box<dyn BlockDevice>>;
type BootVolumeFileInner<'a> = storage_fat::FatFile<'a, Box<dyn BlockDevice>>;
const BOOT_VOLUME_READ_CHUNK_CAP: usize = 64 * 1024;
const ROOT_FILE_EXTENTS_REGISTRY_PATH: &str = "system/registry/kernel/root-file-extents.tsv";
const ROOT_EXTENT_READ_CHUNK_CAP: usize = 256 * 1024;

static CACHED_BOOT_VOLUME_FS: KernelWaitLock<Option<BootVolumeFs>> = KernelWaitLock::new(None);
static ROOT_FILE_EXTENTS: KernelWaitLock<RootFileExtentState> =
    KernelWaitLock::new(RootFileExtentState::Uninitialized);
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

fn should_trace_boot_path(path: &str) -> bool {
    crate::debug::enabled!(storage, debug)
        && (path.contains("services/")
            || path.starts_with("lib/")
            || path.starts_with("/lib/")
            || path.starts_with("lib64/")
            || path.starts_with("/lib64/"))
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn ensure_bootstrap_fs_access(path: &str) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
    if kernel_vfs_runtime_active() {
        crate::debug::println!(
            "bootstrap fs: rejected late direct access path={} phase={:?}",
            path,
            bootstrap_phase()
        );
        return Err(fatfs::Error::Io(DiskIoError::Unsupported));
    }
    Ok(())
}

pub struct BootVolume {
    fs: BootVolumeFs,
}

pub struct BootVolumeFile<'a>(BootVolumeFileInner<'a>);

pub struct PhysicalBootVolume {
    fs: BootVolumeFs,
}

pub struct PhysicalBootVolumeFile<'a>(BootVolumeFileInner<'a>);

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

pub fn boot_volume_transport_hint() -> Option<BootVolumeTransport> {
    Some(boot_info()?.boot_volume.transport())
}

pub fn set_boot_block_device_opener(opener: BootBlockDeviceOpener) {
    *BOOT_BLOCK_DEVICE_OPENER.lock() = Some(opener);
}

pub fn set_physical_boot_block_device_opener(opener: PhysicalBootBlockDeviceOpener) {
    *PHYSICAL_BOOT_BLOCK_DEVICE_OPENER.lock() = Some(opener);
}

impl IoBase for BootVolumeFile<'_> {
    type Error = fatfs::Error<DiskIoError>;
}

impl Read for BootVolumeFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        self.0.read(buf)
    }
}

impl Seek for BootVolumeFile<'_> {
    fn seek(&mut self, pos: SeekFrom) -> core::result::Result<u64, fatfs::Error<DiskIoError>> {
        self.0.seek(pos)
    }
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

fn read_file_to_vec_from_extents(
    path: &str,
) -> core::result::Result<Option<Vec<u8>>, fatfs::Error<DiskIoError>> {
    let Some(path) = normalized_extent_path(path) else {
        return Ok(None);
    };
    let mut cache = ROOT_FILE_EXTENTS.lock();
    if matches!(*cache, RootFileExtentState::Uninitialized) {
        *cache = match load_root_file_extent_table() {
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
    }
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
    read_extent_entry(&entry).map(Some)
}

fn metadata_from_extents(
    path: &str,
) -> core::result::Result<Option<BootVolumeMetadata>, fatfs::Error<DiskIoError>> {
    let Some(path) = normalized_extent_path(path) else {
        return Ok(None);
    };
    let mut cache = ROOT_FILE_EXTENTS.lock();
    if matches!(*cache, RootFileExtentState::Uninitialized) {
        *cache = match load_root_file_extent_table() {
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
    }
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

pub fn boot_file_extent_lease(
    path: &str,
) -> core::result::Result<Option<BootFileExtentLease>, fatfs::Error<DiskIoError>> {
    let Some(path) = normalized_extent_path(path) else {
        return Ok(None);
    };
    let mut cache = ROOT_FILE_EXTENTS.lock();
    if matches!(*cache, RootFileExtentState::Uninitialized) {
        *cache = match load_root_file_extent_table() {
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
    }
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
    let bytes =
        with_open_boot_volume(|fs| read_file_to_vec_from_fs(fs, ROOT_FILE_EXTENTS_REGISTRY_PATH))?;
    parse_root_file_extent_table(&bytes).map_err(|_| fatfs::Error::InvalidInput)
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
    let block_size = crate::storage::block::descriptor(handle)
        .map(|descriptor| descriptor.logical_block_size)
        .filter(|block_size| *block_size != 0)
        .ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
    let mut device = crate::storage::block::FatRegistryDevice::new(handle);

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
        crate::multitask::cond_resched();
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
        crate::multitask::cond_resched();
    }
    Ok(done)
}

impl BootVolume {
    pub fn open() -> core::result::Result<Self, fatfs::Error<DiskIoError>> {
        let opener =
            (*BOOT_BLOCK_DEVICE_OPENER.lock()).ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
        let device = opener()?;
        let fs = fat::open_volume(device)?;
        Ok(Self { fs })
    }

    pub fn open_file(
        &self,
        path: &str,
    ) -> core::result::Result<BootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        self.fs.open_file(path).map(BootVolumeFile)
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

    pub fn read_file_to_vec(
        &self,
        path: &str,
    ) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
        if should_trace_boot_path(path) {
            crate::debug::println!("boot volume: read_file_to_vec enter path={}", path);
        }
        read_file_to_vec_from_fs(&self.fs, path)
    }

    pub fn read_file_into(
        &self,
        path: &str,
        dest: &mut [u8],
    ) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        if should_trace_boot_path(path) {
            crate::debug::println!(
                "boot volume: read_file_into enter path={} len={}",
                path,
                dest.len()
            );
        }
        let trace = should_trace_boot_path(path);
        let done = read_file_into_from_fs(&self.fs, path, dest, |done, count| {
            if trace && done == 0 {
                crate::debug::println!(
                    "boot volume: read_file_into first read done path={} count={}",
                    path,
                    count
                );
            } else if trace && done < (BOOT_VOLUME_READ_CHUNK_CAP * 4) {
                crate::debug::println!(
                    "boot volume: read_file_into chunk done path={} offset={} count={}",
                    path,
                    done,
                    count
                );
            }
        })?;
        if trace {
            crate::debug::println!(
                "boot volume: read_file_into exit path={} ok={} read={}",
                path,
                true,
                done
            );
        }
        Ok(done)
    }

    pub fn close(self) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.unmount()
    }
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

pub fn open_physical_boot_volume(
    identity: BootVolumeIdentity,
) -> core::result::Result<PhysicalBootVolume, fatfs::Error<DiskIoError>> {
    PhysicalBootVolume::open(identity)
}

pub fn open_current_physical_boot_volume()
-> core::result::Result<PhysicalBootVolume, fatfs::Error<DiskIoError>> {
    PhysicalBootVolume::open_current()
}

pub fn read_bootstrap_file_to_vec(
    path: &str,
) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
    if should_trace_boot_path(path) {
        crate::debug::println!("boot volume helper: read_file_to_vec begin path={}", path);
    }
    ensure_bootstrap_fs_access(path)?;
    if let Some(bytes) = read_file_to_vec_from_extents(path)? {
        return Ok(bytes);
    }
    with_open_boot_volume(|fs| read_file_to_vec_from_fs(fs, path))
}

pub fn read_file_to_vec(path: &str) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
    if should_trace_boot_path(path) {
        crate::debug::println!(
            "boot volume helper: runtime read_file_to_vec begin path={}",
            path
        );
    }
    if let Some(bytes) = read_file_to_vec_from_extents(path)? {
        return Ok(bytes);
    }
    let result = with_open_boot_volume(|fs| read_file_to_vec_from_fs(fs, path));
    if result.is_err() && path.starts_with("system/registry/") {
        crate::debug::warn!(storage, "boot volume helper: fallback failed path={}", path);
    }
    result
}

pub fn read_file_into(
    path: &str,
    dest: &mut [u8],
) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
    if should_trace_boot_path(path) {
        crate::debug::println!(
            "boot volume helper: read_file_into begin path={} len={}",
            path,
            dest.len()
        );
    }
    if let Some(bytes) = read_file_to_vec_from_extents(path)? {
        let len = bytes.len().min(dest.len());
        dest[..len].copy_from_slice(&bytes[..len]);
        return Ok(len);
    }
    with_open_boot_volume(|fs| read_file_into_from_fs(fs, path, dest, |_, _| {}))
}

pub fn metadata(path: &str) -> core::result::Result<BootVolumeMetadata, fatfs::Error<DiskIoError>> {
    if should_trace_boot_path(path) {
        crate::debug::println!("boot volume helper: metadata begin path={}", path);
    }
    if let Some(metadata) = metadata_from_extents(path)? {
        return Ok(metadata);
    }
    with_open_boot_volume(|fs| fs.metadata(path))
}

pub fn read_dir(
    path: &str,
) -> core::result::Result<Vec<BootVolumeDirEntry>, fatfs::Error<DiskIoError>> {
    if should_trace_boot_path(path) {
        crate::debug::println!("boot volume helper: read_dir begin path={}", path);
    }
    with_open_boot_volume(|fs| fs.read_dir(path))
}

fn with_open_boot_volume<T>(
    f: impl FnOnce(&BootVolumeFs) -> core::result::Result<T, fatfs::Error<DiskIoError>>,
) -> core::result::Result<T, fatfs::Error<DiskIoError>> {
    let trace = crate::debug::enabled!(storage, debug);
    let mut cache = CACHED_BOOT_VOLUME_FS.lock();
    if cache.is_none() {
        if trace {
            crate::debug::debug!(storage, "boot volume helper: open begin");
        }
        let opener =
            (*BOOT_BLOCK_DEVICE_OPENER.lock()).ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
        let device = opener()?;
        let fs = fat::open_volume(device)?;
        *cache = Some(fs);
        if trace {
            crate::debug::debug!(storage, "boot volume helper: open done");
        }
    }
    let volume_fs = cache.as_ref().expect("cache populated above");
    let result = f(volume_fs);
    if trace {
        crate::debug::debug!(
            storage,
            "boot volume helper: callback done ok={}",
            result.is_ok()
        );
    }
    if result.is_err() {
        // Drop the cache on error so the next call retries with a freshly mounted volume.
        *cache = None;
    }
    result
}

fn boot_info() -> Option<&'static BootInfo> {
    let boot_info_ptr = BOOT_INFO_PTR.load(Ordering::Acquire);
    unsafe { BootInfo::from_ptr(boot_info_ptr.cast_const()) }.ok()
}
// RING3-MIGRATION-REFERENCE END: rootd/vfsd/storaged-owned root extent and boot file policy.
