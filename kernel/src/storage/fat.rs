#![cfg_attr(not(test), allow(dead_code))]

use fatfs::{IoBase, Read, Seek, SeekFrom, Write};
use storage_core::BlockDevice;

pub use storage_core::{IoResult, StorageError as DiskIoError};
pub use storage_fat::{
    FatDirEntry as BootVolumeDirEntry, FatMetadata as BootVolumeMetadata,
    FatNodeKind as BootVolumeNodeKind,
};

pub const FAT_SECTOR_SIZE: usize = 512;

pub struct FatDisk<D: BlockDevice>(storage_fat::FatDisk<D>);

pub(crate) type MountedFatVolume<D> = storage_fat::FatVolume<D>;

impl<D: BlockDevice> FatDisk<D> {
    pub fn new(dev: D) -> Self {
        Self(storage_fat::FatDisk::new(dev))
    }
}

pub(crate) fn open_volume<D: BlockDevice>(
    device: D,
) -> core::result::Result<MountedFatVolume<D>, fatfs::Error<DiskIoError>> {
    storage_fat::FatVolume::new(device)
}

impl<D: BlockDevice> IoBase for FatDisk<D> {
    type Error = DiskIoError;
}

impl<D: BlockDevice> Read for FatDisk<D> {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        self.0.read(buf)
    }
}

impl<D: BlockDevice> Write for FatDisk<D> {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> IoResult<()> {
        self.0.flush()
    }
}

impl<D: BlockDevice> Seek for FatDisk<D> {
    fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
        self.0.seek(pos)
    }
}
