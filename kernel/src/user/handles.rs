use alloc::vec::Vec;

use crate::io::device::DeviceHandle;
use crate::paging::UserRegion;
use crate::user::abi::device::PIXEL_FORMAT_BGRA8888;

pub const FIRST_DYNAMIC_FD: u32 = 3;
const PAGE_SIZE: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplaySurfaceHandle {
    width: u32,
    height: u32,
    stride_bytes: u32,
    bytes_per_pixel: u32,
    pixel_format: u32,
    frame_len: u64,
    mapping_len: u64,
    mapped_region: Option<UserRegion>,
}

impl DisplaySurfaceHandle {
    pub fn new(width: u32, height: u32, pixel_format: u32) -> Option<Self> {
        if width == 0 || height == 0 || pixel_format != PIXEL_FORMAT_BGRA8888 {
            return None;
        }

        let bytes_per_pixel = 4_u32;
        let stride_bytes = width.checked_mul(bytes_per_pixel)?;
        let frame_len = (stride_bytes as u64).checked_mul(height as u64)?;
        let mapping_len = align_up(frame_len, PAGE_SIZE)?;

        Some(Self {
            width,
            height,
            stride_bytes,
            bytes_per_pixel,
            pixel_format,
            frame_len,
            mapping_len,
            mapped_region: None,
        })
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }

    pub fn stride_bytes(self) -> u32 {
        self.stride_bytes
    }

    pub fn bytes_per_pixel(self) -> u32 {
        self.bytes_per_pixel
    }

    pub fn pixel_format(self) -> u32 {
        self.pixel_format
    }

    pub fn frame_len(self) -> u64 {
        self.frame_len
    }

    pub fn mapping_len(self) -> u64 {
        self.mapping_len
    }

    pub fn mapped_region(self) -> Option<UserRegion> {
        self.mapped_region
    }

    pub fn set_mapped_region(&mut self, region: UserRegion) {
        self.mapped_region = Some(region);
    }

    pub fn clear_mapping(&mut self) {
        self.mapped_region = None;
    }
}

#[derive(Debug)]
pub struct BootFileHandle {
    bytes: Vec<u8>,
    cursor: usize,
    writable: bool,
}

impl BootFileHandle {
    pub fn read_only(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            cursor: 0,
            writable: false,
        }
    }

    pub fn read_into(&mut self, dest: &mut [u8]) -> usize {
        let read = self.read_at(self.cursor, dest);
        self.cursor = self.cursor.saturating_add(read);
        read
    }

    pub fn read_at(&self, offset: usize, dest: &mut [u8]) -> usize {
        if dest.is_empty() || offset >= self.bytes.len() {
            return 0;
        }

        let read = dest.len().min(self.bytes.len() - offset);
        dest[..read].copy_from_slice(&self.bytes[offset..offset + read]);
        read
    }

    pub fn write_from(&mut self, src: &[u8]) -> Result<usize, BootFileWriteError> {
        if src.is_empty() {
            return Ok(0);
        }
        if !self.writable {
            return Err(BootFileWriteError::ReadOnly);
        }

        Err(BootFileWriteError::Unsupported)
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn seek(
        &mut self,
        offset: i64,
        whence: BootFileSeekWhence,
    ) -> Result<u64, BootFileSeekError> {
        let len = self.bytes.len() as i128;
        let cursor = self.cursor as i128;
        let next = match whence {
            BootFileSeekWhence::Start => Some(offset as i128),
            BootFileSeekWhence::Current => cursor.checked_add(offset as i128),
            BootFileSeekWhence::End => len.checked_add(offset as i128),
        }
        .ok_or(BootFileSeekError::InvalidPosition)?;

        if next < 0 || next > len {
            return Err(BootFileSeekError::InvalidPosition);
        }

        self.cursor = next as usize;
        Ok(next as u64)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootFileWriteError {
    ReadOnly,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootFileSeekWhence {
    Start,
    Current,
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootFileSeekError {
    InvalidPosition,
}

#[derive(Debug)]
pub enum KernelHandle {
    Device(DeviceHandle),
    BootFile(BootFileHandle),
    DisplaySurface(DisplaySurfaceHandle),
}

pub struct HandleTable {
    entries: Vec<Option<KernelHandle>>,
}

impl HandleTable {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn install(&mut self, handle: KernelHandle) -> u64 {
        if let Some(index) = self.entries.iter().position(|entry| entry.is_none()) {
            self.entries[index] = Some(handle);
            return FIRST_DYNAMIC_FD as u64 + index as u64;
        }

        self.entries.push(Some(handle));
        FIRST_DYNAMIC_FD as u64 + (self.entries.len() - 1) as u64
    }

    pub fn get(&self, fd: u64) -> Option<&KernelHandle> {
        let index = dynamic_index(fd)?;
        self.entries.get(index)?.as_ref()
    }

    pub fn get_mut(&mut self, fd: u64) -> Option<&mut KernelHandle> {
        let index = dynamic_index(fd)?;
        self.entries.get_mut(index)?.as_mut()
    }

    pub fn close(&mut self, fd: u64) -> Option<KernelHandle> {
        let index = dynamic_index(fd)?;
        self.entries.get_mut(index)?.take()
    }

    pub fn clear_surface_mappings_in_range(&mut self, start: u64, len: u64) {
        let Some(end) = start.checked_add(len) else {
            return;
        };

        for entry in &mut self.entries {
            let Some(KernelHandle::DisplaySurface(surface)) = entry.as_mut() else {
                continue;
            };
            let Some(region) = surface.mapped_region() else {
                continue;
            };
            let region_start = region.start.as_u64();
            let region_end = region.end().as_u64();
            if start < region_end && end > region_start {
                surface.clear_mapping();
            }
        }
    }
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

fn dynamic_index(fd: u64) -> Option<usize> {
    fd.checked_sub(FIRST_DYNAMIC_FD as u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|aligned| aligned & !(align - 1))
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{BootFileHandle, BootFileSeekError, BootFileSeekWhence};

    #[test]
    fn read_at_preserves_stream_cursor() {
        let mut handle = BootFileHandle::read_only(vec![1, 2, 3, 4, 5]);
        let mut direct = [0_u8; 2];
        assert_eq!(handle.read_at(2, &mut direct), 2);
        assert_eq!(direct, [3, 4]);
        assert_eq!(handle.cursor(), 0);

        let mut sequential = [0_u8; 2];
        assert_eq!(handle.read_into(&mut sequential), 2);
        assert_eq!(sequential, [1, 2]);
        assert_eq!(handle.cursor(), 2);
    }

    #[test]
    fn seek_updates_cursor_with_linux_style_offsets() {
        let mut handle = BootFileHandle::read_only(vec![0; 8]);

        assert_eq!(handle.seek(3, BootFileSeekWhence::Start), Ok(3));
        assert_eq!(handle.cursor(), 3);

        assert_eq!(handle.seek(2, BootFileSeekWhence::Current), Ok(5));
        assert_eq!(handle.cursor(), 5);

        assert_eq!(handle.seek(-1, BootFileSeekWhence::End), Ok(7));
        assert_eq!(handle.cursor(), 7);
    }

    #[test]
    fn seek_rejects_positions_before_start_or_after_end() {
        let mut handle = BootFileHandle::read_only(vec![0; 4]);

        assert_eq!(
            handle.seek(-1, BootFileSeekWhence::Start),
            Err(BootFileSeekError::InvalidPosition)
        );
        assert_eq!(
            handle.seek(1, BootFileSeekWhence::End),
            Err(BootFileSeekError::InvalidPosition)
        );
    }
}
