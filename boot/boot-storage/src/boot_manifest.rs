use alloc::borrow::Cow;
use boot_protocol::{
    BootFileEntry, BootFileManifest, BootInfo, BootVolumeIdentity, FramebufferInfo,
};
use core::ptr;
use core::slice;
use core::str;
use core::sync::atomic::{AtomicPtr, Ordering};

use alloc::string::String;
use alloc::vec::Vec;

use crate::{BootVolumeDirEntry, BootVolumeMetadata, BootVolumeNodeKind};

static BOOT_INFO_PTR: AtomicPtr<BootInfo> = AtomicPtr::new(ptr::null_mut());

pub type BootFileBytes = Cow<'static, [u8]>;

pub fn init_boot_info(boot_info_ptr: *const BootInfo) {
    BOOT_INFO_PTR.store(boot_info_ptr.cast_mut(), Ordering::Release);
}

pub fn boot_framebuffer_info() -> Option<FramebufferInfo> {
    boot_info().map(|info| info.framebuffer)
}

pub fn boot_volume_identity() -> Option<BootVolumeIdentity> {
    let identity = boot_info()?.boot_volume;
    identity.is_present().then_some(identity)
}

fn boot_info() -> Option<&'static BootInfo> {
    let boot_info_ptr = BOOT_INFO_PTR.load(Ordering::Acquire);
    if boot_info_ptr.is_null() {
        None
    } else {
        Some(unsafe { &*boot_info_ptr.cast_const() })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CachedBootVolume {
    manifest: BootFileManifest,
}

impl CachedBootVolume {
    pub(crate) fn from_boot_info() -> Option<Self> {
        let boot_info = boot_info()?;
        if boot_info.boot_files.entry_count == 0 {
            return None;
        }
        Some(Self {
            manifest: boot_info.boot_files,
        })
    }

    pub(crate) fn is_manifest_valid(&self) -> bool {
        boot_file_entries(&self.manifest).is_some()
    }

    pub(crate) fn open_file(&self, normalized_path: &str) -> Option<CachedBootFile> {
        let entries = boot_file_entries(&self.manifest)?;
        for entry in entries {
            let path = boot_file_path(entry)?;
            if fat_paths_match(normalized_path, path) {
                return Some(CachedBootFile {
                    data: boot_file_data(entry)?,
                    pos: 0,
                });
            }
        }
        None
    }

    pub(crate) fn metadata(&self, normalized_path: &str) -> Option<BootVolumeMetadata> {
        if normalized_path.is_empty() {
            return Some(BootVolumeMetadata {
                kind: BootVolumeNodeKind::Directory,
                len: 0,
            });
        }

        let entries = boot_file_entries(&self.manifest)?;
        let mut has_child = false;
        for entry in entries {
            let path = boot_file_path(entry)?;
            if fat_paths_match(normalized_path, path) {
                return Some(BootVolumeMetadata {
                    kind: BootVolumeNodeKind::File,
                    len: entry.data_len,
                });
            }
            if fat_path_has_directory_prefix(path, normalized_path) {
                has_child = true;
            }
        }

        has_child.then_some(BootVolumeMetadata {
            kind: BootVolumeNodeKind::Directory,
            len: 0,
        })
    }

    pub(crate) fn read_dir(&self, normalized_path: &str) -> Option<Vec<BootVolumeDirEntry>> {
        let entries = boot_file_entries(&self.manifest)?;
        let mut output = Vec::new();
        for entry in entries {
            let path = boot_file_path(entry)?;
            let Some((name, kind)) = immediate_child_for_directory(path, normalized_path) else {
                continue;
            };
            push_unique_dir_entry(&mut output, name, kind);
        }
        Some(output)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CachedBootFile {
    pub(crate) data: &'static [u8],
    pub(crate) pos: usize,
}

fn boot_file_entries(manifest: &BootFileManifest) -> Option<&'static [BootFileEntry]> {
    if manifest.entry_count == 0 {
        return Some(&[]);
    }
    if manifest.entries_ptr == 0 {
        return None;
    }

    Some(unsafe {
        slice::from_raw_parts(
            manifest.entries_ptr as *const BootFileEntry,
            manifest.entry_count as usize,
        )
    })
}

fn boot_file_path(entry: &BootFileEntry) -> Option<&'static str> {
    if entry.path_len == 0 || entry.path_ptr == 0 {
        return None;
    }

    let bytes =
        unsafe { slice::from_raw_parts(entry.path_ptr as *const u8, entry.path_len as usize) };
    str::from_utf8(bytes).ok()
}

fn boot_file_data(entry: &BootFileEntry) -> Option<&'static [u8]> {
    if entry.data_len == 0 {
        return Some(&[]);
    }
    if entry.data_ptr == 0 {
        return None;
    }

    Some(unsafe { slice::from_raw_parts(entry.data_ptr as *const u8, entry.data_len as usize) })
}

pub(crate) fn fat_paths_match(lhs: &str, rhs: &str) -> bool {
    lhs.eq_ignore_ascii_case(rhs)
}

fn fat_path_has_directory_prefix(path: &str, directory: &str) -> bool {
    if directory.is_empty() {
        return true;
    }
    path.len() > directory.len()
        && path[..directory.len()].eq_ignore_ascii_case(directory)
        && path.as_bytes()[directory.len()] == b'/'
}

fn immediate_child_for_directory<'a>(
    path: &'a str,
    directory: &str,
) -> Option<(&'a str, BootVolumeNodeKind)> {
    let remainder = if directory.is_empty() {
        path
    } else if fat_path_has_directory_prefix(path, directory) {
        &path[directory.len() + 1..]
    } else {
        return None;
    };

    if remainder.is_empty() {
        return None;
    }

    if let Some((child, _)) = remainder.split_once('/') {
        if child.is_empty() {
            return None;
        }
        Some((child, BootVolumeNodeKind::Directory))
    } else {
        Some((remainder, BootVolumeNodeKind::File))
    }
}

fn push_unique_dir_entry(
    entries: &mut Vec<BootVolumeDirEntry>,
    name: &str,
    kind: BootVolumeNodeKind,
) {
    if entries
        .iter()
        .any(|entry| entry.name.eq_ignore_ascii_case(name))
    {
        return;
    }

    entries.push(BootVolumeDirEntry {
        name: String::from(name),
        kind,
    });
}
