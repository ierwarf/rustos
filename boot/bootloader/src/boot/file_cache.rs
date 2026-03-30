use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::size_of;
use core::ptr;

use uefi::boot::{self, AllocateType, MemoryType};
use uefi::data_types::FromStrError;
use uefi::fs::{Error as FsError, FileSystem};
use uefi::prelude::*;
use uefi::proto::device_path::{DevicePath, DevicePathNodeEnum};
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::block::BlockIO;
use uefi::proto::media::disk::DiskIo;
use uefi::CString16;

use crate::boot_info::{
    BootFileEntry, BootFileManifest, BootVolumeIdentity, BOOT_FILE_MANIFEST_TRUNCATED,
};
use crate::debug;

use super::error::BootError;

const PAGE_SIZE: usize = 4096;
const MAX_BOOT_FILE_COUNT: usize = 4096;
// Allow substantially larger staged modules to stay in the boot manifest so the
// kernel can borrow them without transient heap copies during load.
const MAX_BOOT_FILE_BYTES: usize = 256 * 1024 * 1024;
const MAX_BOOT_TOTAL_BYTES: usize = 768 * 1024 * 1024;
const BOOT_FILE_DATA_ALIGN: usize = 16;

const PRIORITY_BOOT_FILES: [PriorityBootFile; 1] =
    [PriorityBootFile::required("kernel.elf", cstr16!("\\kernel.elf"))];

const BOOT_FILE_LIST_PATHS: [&uefi::CStr16; 4] = [
    cstr16!("\\system\\registry\\boot\\bootfiles.txt"),
    cstr16!("\\BOOTFILES.TXT"),
    cstr16!("\\EFI\\BOOT\\BOOTFILES.TXT"),
    cstr16!("\\RUSTOS.BOOT"),
];

pub struct BootVolumeSnapshot {
    pub manifest: BootFileManifest,
    pub identity: BootVolumeIdentity,
}

pub fn snapshot_boot_volume() -> Result<BootVolumeSnapshot, BootError> {
    let sfs = boot::get_image_file_system(boot::image_handle())
        .map_err(|err| BootError::OpenFileSystem(err.status()))?;
    let mut fs = FileSystem::new(sfs);
    let mut snapshot = BootFileSnapshot::new();

    for priority in PRIORITY_BOOT_FILES {
        cache_priority_file(&mut snapshot, &mut fs, priority)?;
    }
    cache_manifest_files(&mut snapshot, &mut fs)?;

    let manifest = snapshot.finalize()?;
    let identity = match extract_boot_volume_identity() {
        Ok(identity) => {
            debug::println!(
                "bootloader: boot volume identity: serial={:#010x} start_lba={} sectors={}",
                identity.fat_volume_id,
                identity.volume_start_lba,
                identity.volume_sector_count
            );
            identity
        }
        Err(status) => {
            debug::println!(
                "bootloader: boot volume identity unavailable: {:?}; file logging disabled",
                status
            );
            BootVolumeIdentity::empty()
        }
    };
    debug::println!(
        "bootloader: cached boot files: files={} bytes={}{}",
        manifest.entry_count,
        manifest.total_bytes,
        if manifest.flags & BOOT_FILE_MANIFEST_TRUNCATED != 0 {
            " (truncated)"
        } else {
            ""
        }
    );
    Ok(BootVolumeSnapshot { manifest, identity })
}

fn extract_boot_volume_identity() -> Result<BootVolumeIdentity, Status> {
    let loaded_image = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
        .map_err(|err| err.status())?;
    let device_handle = loaded_image.device().ok_or(Status::NOT_FOUND)?;
    let device_path =
        boot::open_protocol_exclusive::<DevicePath>(device_handle).map_err(|err| err.status())?;
    let (volume_start_lba, partition_sector_count) = boot_volume_bounds(&device_path);

    let block_io =
        boot::open_protocol_exclusive::<BlockIO>(device_handle).map_err(|err| err.status())?;
    let media = block_io.media();
    if !media.is_media_present() {
        return Err(Status::NO_MEDIA);
    }
    let volume_sector_count = if partition_sector_count != 0 {
        partition_sector_count
    } else {
        media.last_block().saturating_add(1)
    };
    if volume_sector_count == 0 {
        return Err(Status::NO_MEDIA);
    }

    let disk_io =
        boot::open_protocol_exclusive::<DiskIo>(device_handle).map_err(|err| err.status())?;
    let mut boot_sector = [0_u8; boot_storage::FAT_SECTOR_SIZE];
    disk_io
        .read_disk(media.media_id(), 0, &mut boot_sector)
        .map_err(|err| err.status())?;

    let fat_volume_id =
        boot_storage::fat_volume_id_from_boot_sector(&boot_sector).ok_or(Status::LOAD_ERROR)?;
    Ok(BootVolumeIdentity {
        fat_volume_id,
        _reserved0: 0,
        volume_start_lba,
        volume_sector_count,
    })
}

fn boot_volume_bounds(device_path: &DevicePath) -> (u64, u64) {
    for node in device_path.node_iter() {
        let Ok(node) = node.as_enum() else {
            continue;
        };
        if let DevicePathNodeEnum::MediaHardDrive(hard_drive) = node {
            return (hard_drive.partition_start(), hard_drive.partition_size());
        }
    }

    (0, 0)
}

#[derive(Clone, Copy)]
struct PriorityBootFile {
    normalized_path: &'static str,
    uefi_path: &'static uefi::CStr16,
    required: bool,
}

impl PriorityBootFile {
    const fn required(normalized_path: &'static str, uefi_path: &'static uefi::CStr16) -> Self {
        Self {
            normalized_path,
            uefi_path,
            required: true,
        }
    }
}

struct BootFileSnapshot {
    entries: Vec<BootFileEntry>,
    known_paths: Vec<String>,
    total_bytes: usize,
    flags: u32,
}

impl BootFileSnapshot {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            known_paths: Vec::new(),
            total_bytes: 0,
            flags: 0,
        }
    }

    fn contains_path(&self, path: &str) -> bool {
        self.known_paths
            .iter()
            .any(|known| known.eq_ignore_ascii_case(path))
    }

    fn mark_truncated(&mut self) {
        self.flags |= BOOT_FILE_MANIFEST_TRUNCATED;
    }

    fn cannot_store(&self, file_size: usize) -> bool {
        let exceeds_total = match self.total_bytes.checked_add(file_size) {
            Some(total) => total > MAX_BOOT_TOTAL_BYTES,
            None => true,
        };
        file_size > MAX_BOOT_FILE_BYTES
            || self.entries.len() >= MAX_BOOT_FILE_COUNT
            || exceeds_total
    }

    fn try_store_file(&mut self, path: &str, bytes: &[u8]) -> Result<bool, Status> {
        if self.contains_path(path) {
            return Ok(true);
        }
        if self.cannot_store(bytes.len()) {
            self.mark_truncated();
            return Ok(false);
        }

        let path_bytes = path.as_bytes();
        let data_offset = align_up(path_bytes.len(), BOOT_FILE_DATA_ALIGN)?;
        let blob_len = data_offset
            .checked_add(bytes.len())
            .ok_or(Status::OUT_OF_RESOURCES)?;
        let blob = allocate_loader_bytes(blob_len)?;

        unsafe {
            ptr::copy_nonoverlapping(path_bytes.as_ptr(), blob, path_bytes.len());
            ptr::copy_nonoverlapping(bytes.as_ptr(), blob.add(data_offset), bytes.len());
        }

        self.entries.push(BootFileEntry {
            path_ptr: blob as u64,
            path_len: path_bytes.len() as u32,
            _reserved0: 0,
            data_ptr: unsafe { blob.add(data_offset) } as u64,
            data_len: bytes.len() as u64,
        });
        self.known_paths.push(path.to_owned());
        self.total_bytes += bytes.len();
        Ok(true)
    }

    fn finalize(self) -> Result<BootFileManifest, BootError> {
        if self.entries.is_empty() {
            return Ok(BootFileManifest::empty());
        }

        let entry_bytes = self
            .entries
            .len()
            .checked_mul(size_of::<BootFileEntry>())
            .ok_or(BootError::CacheBootVolume(Status::OUT_OF_RESOURCES))?;
        let entries_ptr = allocate_loader_bytes(entry_bytes).map_err(BootError::CacheBootVolume)?
            as *mut BootFileEntry;

        unsafe {
            ptr::copy_nonoverlapping(self.entries.as_ptr(), entries_ptr, self.entries.len());
        }

        Ok(BootFileManifest {
            entries_ptr: entries_ptr as u64,
            entry_count: self.entries.len() as u32,
            flags: self.flags,
            total_bytes: self.total_bytes as u64,
        })
    }
}

fn align_up(value: usize, align: usize) -> Result<usize, Status> {
    if align == 0 || !align.is_power_of_two() {
        return Err(Status::OUT_OF_RESOURCES);
    }

    value
        .checked_add(align - 1)
        .map(|rounded| rounded & !(align - 1))
        .ok_or(Status::OUT_OF_RESOURCES)
}

fn cache_priority_file(
    snapshot: &mut BootFileSnapshot,
    fs: &mut FileSystem,
    file: PriorityBootFile,
) -> Result<(), BootError> {
    let normalized_path = normalize_boot_path(file.normalized_path);
    if snapshot.contains_path(&normalized_path) {
        return Ok(());
    }

    let metadata = match fs.metadata(file.uefi_path) {
        Ok(metadata) => metadata,
        Err(err) => {
            let status = fs_error_status(&err);
            if !file.required && status == Status::NOT_FOUND {
                return Ok(());
            }
            if file.required {
                return Err(BootError::CacheBootVolume(status));
            }
            snapshot.mark_truncated();
            debug::println!(
                "bootloader: skipping optional boot file {}: {:?}",
                normalized_path,
                err
            );
            return Ok(());
        }
    };

    let file_size = metadata.file_size() as usize;
    if snapshot.cannot_store(file_size) {
        if file.required {
            return Err(BootError::CacheBootVolume(Status::OUT_OF_RESOURCES));
        }
        snapshot.mark_truncated();
        return Ok(());
    }

    let bytes = fs
        .read(file.uefi_path)
        .map_err(|err| BootError::CacheBootVolume(fs_error_status(&err)))?;
    let stored = snapshot
        .try_store_file(&normalized_path, &bytes)
        .map_err(BootError::CacheBootVolume)?;
    if file.required && !stored {
        return Err(BootError::CacheBootVolume(Status::OUT_OF_RESOURCES));
    }
    Ok(())
}

fn cache_manifest_files(
    snapshot: &mut BootFileSnapshot,
    fs: &mut FileSystem,
) -> Result<(), BootError> {
    let Some(list_path) = BOOT_FILE_LIST_PATHS
        .iter()
        .find_map(|path| match fs.read(path) {
            Ok(bytes) => Some(Ok((*path, bytes))),
            Err(err) if fs_error_status(&err) == Status::NOT_FOUND => None,
            Err(err) => Some(Err(BootError::CacheBootVolume(fs_error_status(&err)))),
        })
    else {
        return Ok(());
    };

    let (list_path, bytes) = list_path?;
    let manifest =
        String::from_utf8(bytes).map_err(|_| BootError::CacheBootVolume(Status::LOAD_ERROR))?;
    debug::println!("bootloader: loading extra boot files from {}", list_path);

    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        let normalized_path = normalize_boot_path(line);
        if normalized_path.is_empty() || snapshot.contains_path(&normalized_path) {
            continue;
        }

        let uefi_path = path_to_uefi(&normalized_path)
            .map_err(|_| BootError::CacheBootVolume(Status::LOAD_ERROR))?;
        let metadata = fs
            .metadata(uefi_path.as_ref())
            .map_err(|err| BootError::CacheBootVolume(fs_error_status(&err)))?;
        let file_size = metadata.file_size() as usize;
        if snapshot.cannot_store(file_size) {
            return Err(BootError::CacheBootVolume(Status::OUT_OF_RESOURCES));
        }

        let bytes = fs
            .read(uefi_path.as_ref())
            .map_err(|err| BootError::CacheBootVolume(fs_error_status(&err)))?;
        let stored = snapshot
            .try_store_file(&normalized_path, &bytes)
            .map_err(BootError::CacheBootVolume)?;
        if !stored {
            return Err(BootError::CacheBootVolume(Status::OUT_OF_RESOURCES));
        }
    }

    Ok(())
}

fn allocate_loader_bytes(byte_len: usize) -> Result<*mut u8, Status> {
    let byte_len = byte_len.max(1);
    let page_count = byte_len.div_ceil(PAGE_SIZE);
    let ptr = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, page_count)
        .map_err(|err| err.status())?;

    unsafe {
        ptr::write_bytes(ptr.as_ptr(), 0, page_count * PAGE_SIZE);
    }

    Ok(ptr.as_ptr())
}

fn fs_error_status(err: &FsError) -> Status {
    match err {
        FsError::Io(io) => io.uefi_error.status(),
        FsError::Path(_) | FsError::Utf8Encoding(_) => Status::LOAD_ERROR,
    }
}

fn normalize_boot_path(path: &str) -> String {
    let mut normalized = String::with_capacity(path.len());
    for component in path.split(['/', '\\']) {
        if component.is_empty() || component == "." {
            continue;
        }
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    normalized
}

fn path_to_uefi(path: &str) -> Result<CString16, FromStrError> {
    let mut uefi_path = String::with_capacity(path.len() + 1);
    uefi_path.push('\\');
    for ch in path.chars() {
        uefi_path.push(if ch == '/' { '\\' } else { ch });
    }
    CString16::try_from(uefi_path.as_str())
}
