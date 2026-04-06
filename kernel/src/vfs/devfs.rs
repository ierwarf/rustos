use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::io::device as device_ns;
use crate::storage::block;
use crate::user::handles::{VfsDirectoryEntry, VfsDirectoryHandle};

use super::{
    FilesystemProvider, MountError, MountSource, VfsBackend, VfsContext, VfsError, VfsMetadata,
    VfsNodeKind, VfsOpenResult,
};

pub(crate) static DEVFS_PROVIDER: DevFsProvider = DevFsProvider;

pub(crate) struct DevFsProvider;
pub(crate) struct DevFsBackend;

impl FilesystemProvider for DevFsProvider {
    fn name(&self) -> &'static str {
        "devfs"
    }

    fn mount(
        &self,
        source: MountSource,
        _flags: u64,
        _options: Option<&str>,
    ) -> Result<Arc<dyn VfsBackend>, MountError> {
        if !matches!(source, MountSource::None) {
            return Err(MountError::InvalidSource);
        }
        Ok(Arc::new(DevFsBackend))
    }
}

impl VfsBackend for DevFsBackend {
    fn open(
        &self,
        absolute_path: &str,
        relative_path: &str,
        flags: u64,
        _mode: u64,
        _context: &mut VfsContext<'_>,
    ) -> Result<VfsOpenResult, VfsError> {
        if relative_path == "/" {
            return Ok(VfsOpenResult::Directory(VfsDirectoryHandle::new(
                String::from(absolute_path),
                self.read_dir(absolute_path, relative_path, _context)?,
            )));
        }
        if flags & crate::user::linux::O_DIRECTORY != 0 {
            return Err(VfsError::NotDirectory);
        }
        if block::lookup(absolute_path).is_some() {
            return Err(VfsError::Unsupported);
        }
        let handle = device_ns::open(absolute_path).map_err(map_lookup_error)?;
        Ok(VfsOpenResult::Device(handle))
    }

    fn metadata(
        &self,
        absolute_path: &str,
        relative_path: &str,
        _context: &mut VfsContext<'_>,
    ) -> Result<VfsMetadata, VfsError> {
        if relative_path == "/" {
            return Ok(super::default_metadata(
                absolute_path,
                VfsNodeKind::Directory,
                0,
            ));
        }
        if block::lookup(absolute_path).is_some() {
            return Ok(super::default_metadata(
                absolute_path,
                VfsNodeKind::Device,
                0,
            ));
        }
        let descriptor = device_ns::lookup(absolute_path).map_err(map_lookup_error)?;
        Ok(super::default_metadata(
            descriptor.path,
            VfsNodeKind::Device,
            0,
        ))
    }

    fn check_access(
        &self,
        absolute_path: &str,
        relative_path: &str,
        mode: u64,
        _context: &mut VfsContext<'_>,
    ) -> Result<(), VfsError> {
        super::ensure_read_access_only(mode)?;
        if relative_path == "/" {
            return Ok(());
        }
        if block::lookup(absolute_path).is_some() {
            return Ok(());
        }
        let _ = device_ns::lookup(absolute_path).map_err(map_lookup_error)?;
        Ok(())
    }

    fn readlink(
        &self,
        _absolute_path: &str,
        _relative_path: &str,
        _context: &mut VfsContext<'_>,
    ) -> Result<String, VfsError> {
        Err(VfsError::NotFound)
    }

    fn read_dir(
        &self,
        _absolute_path: &str,
        relative_path: &str,
        _context: &mut VfsContext<'_>,
    ) -> Result<Vec<VfsDirectoryEntry>, VfsError> {
        if relative_path != "/" {
            return Err(VfsError::NotDirectory);
        }

        Ok(device_ns::descriptors()
            .iter()
            .map(|descriptor| super::directory_entry(descriptor.path, VfsNodeKind::Device))
            .chain(block::descriptors().into_iter().map(|descriptor| {
                super::directory_entry(descriptor.path.as_str(), VfsNodeKind::Device)
            }))
            .collect())
    }
}

fn map_lookup_error(err: device_ns::DeviceLookupError) -> VfsError {
    match err {
        device_ns::DeviceLookupError::InvalidPath => VfsError::InvalidArgument,
        device_ns::DeviceLookupError::NotFound => VfsError::NotFound,
    }
}
