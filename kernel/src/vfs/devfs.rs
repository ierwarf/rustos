use alloc::string::String;

use crate::io::device as device_ns;
use crate::user::handles::VfsDirectoryHandle;

use super::{VfsBackend, VfsContext, VfsError, VfsMetadata, VfsNodeKind, VfsOpenResult};

pub(crate) static DEVFS: DevFsBackend = DevFsBackend;

pub(crate) struct DevFsBackend;

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
            )));
        }
        if flags & crate::user::linux::O_DIRECTORY != 0 {
            return Err(VfsError::NotDirectory);
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
}

fn map_lookup_error(err: device_ns::DeviceLookupError) -> VfsError {
    match err {
        device_ns::DeviceLookupError::InvalidPath => VfsError::InvalidArgument,
        device_ns::DeviceLookupError::NotFound => VfsError::NotFound,
    }
}
