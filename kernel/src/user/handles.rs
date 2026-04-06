use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Debug;
use spin::Mutex;

use crate::io::device::DeviceHandle;
use crate::memory::paging::UserRegion;
use crate::user::abi::device::PIXEL_FORMAT_BGRA8888;
use crate::user::epoll::EpollHandle;
use crate::user::linux as linux_abi;
use crate::user::memfd::MemfdHandle;
use crate::user::socket::SocketHandle;

pub const FIRST_DYNAMIC_FD: u32 = 3;
const PAGE_SIZE: u64 = 4096;
const MAX_DISPLAY_SURFACE_WIDTH: u32 = 7680;
const MAX_DISPLAY_SURFACE_HEIGHT: u32 = 4320;
const MAX_DISPLAY_SURFACE_BYTES: u64 =
    MAX_DISPLAY_SURFACE_WIDTH as u64 * MAX_DISPLAY_SURFACE_HEIGHT as u64 * 4;
pub const FD_CLOEXEC: u32 = 0x1;
const STATUS_FLAG_MASK: u64 =
    linux_abi::O_ACCMODE | linux_abi::O_APPEND | linux_abi::O_NONBLOCK | linux_abi::O_NOCTTY;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplaySurfaceHandle {
    width: u32,
    height: u32,
    stride_bytes: u32,
    bytes_per_pixel: u32,
    pixel_format: u32,
    generation: u64,
    frame_len: u64,
    mapping_len: u64,
    mapped_region: Option<UserRegion>,
}

impl DisplaySurfaceHandle {
    pub fn new(width: u32, height: u32, pixel_format: u32, generation: u64) -> Option<Self> {
        if width == 0
            || height == 0
            || width > MAX_DISPLAY_SURFACE_WIDTH
            || height > MAX_DISPLAY_SURFACE_HEIGHT
            || pixel_format != PIXEL_FORMAT_BGRA8888
            || generation == 0
        {
            return None;
        }

        let bytes_per_pixel = 4_u32;
        let stride_bytes = width.checked_mul(bytes_per_pixel)?;
        let frame_len = (stride_bytes as u64).checked_mul(height as u64)?;
        if frame_len == 0 || frame_len > MAX_DISPLAY_SURFACE_BYTES {
            return None;
        }
        let mapping_len = align_up(frame_len, PAGE_SIZE)?;

        Some(Self {
            width,
            height,
            stride_bytes,
            bytes_per_pixel,
            pixel_format,
            generation,
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

    pub fn generation(self) -> u64 {
        self.generation
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
struct VfsFileState {
    file: Arc<dyn VfsFileObject>,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfsDirectoryEntryKind {
    File,
    Directory,
    Device,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VfsDirectoryEntry {
    name: String,
    inode: u64,
    kind: VfsDirectoryEntryKind,
}

impl VfsDirectoryEntry {
    pub fn new(name: String, inode: u64, kind: VfsDirectoryEntryKind) -> Self {
        Self { name, inode, kind }
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn inode(&self) -> u64 {
        self.inode
    }

    pub fn kind(&self) -> VfsDirectoryEntryKind {
        self.kind
    }
}

pub trait VfsFileObject: Debug + Send + Sync {
    fn path(&self) -> &str;
    fn len(&self) -> usize;
    fn read_at(&self, offset: usize, dest: &mut [u8]) -> usize;
    fn write_at(&self, offset: usize, src: &[u8]) -> Result<usize, FileHandleWriteError>;
}

#[derive(Debug)]
struct ReadOnlyMemoryFile {
    path: String,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct VfsFileHandle {
    inner: Arc<Mutex<VfsFileState>>,
}

#[derive(Clone, Debug)]
pub struct VfsDirectoryHandle {
    path: Arc<str>,
    entries: Arc<[VfsDirectoryEntry]>,
    cursor: usize,
}

impl VfsDirectoryHandle {
    pub fn new(path: String, entries: Vec<VfsDirectoryEntry>) -> Self {
        Self {
            path: Arc::<str>::from(path),
            entries: Arc::<[VfsDirectoryEntry]>::from(entries.into_boxed_slice()),
            cursor: 0,
        }
    }

    pub fn path(&self) -> &str {
        self.path.as_ref()
    }

    pub fn entries(&self) -> &[VfsDirectoryEntry] {
        self.entries.as_ref()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn advance_cursor(&mut self, count: usize) {
        self.cursor = self.cursor.saturating_add(count).min(self.entries.len());
    }
}

impl VfsFileHandle {
    pub fn new(file: Arc<dyn VfsFileObject>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VfsFileState { file, cursor: 0 })),
        }
    }

    pub fn read_only_memory(path: String, bytes: Vec<u8>) -> Self {
        Self::new(Arc::new(ReadOnlyMemoryFile { path, bytes }))
    }

    pub fn path(&self) -> String {
        String::from(self.inner.lock().file.path())
    }

    pub fn read_into(&mut self, dest: &mut [u8]) -> usize {
        let mut state = self.inner.lock();
        let read = state.file.read_at(state.cursor, dest);
        state.cursor = state.cursor.saturating_add(read);
        read
    }

    pub fn read_at(&self, offset: usize, dest: &mut [u8]) -> usize {
        self.inner.lock().file.read_at(offset, dest)
    }

    pub fn write_from(&mut self, src: &[u8]) -> Result<usize, FileHandleWriteError> {
        if src.is_empty() {
            return Ok(0);
        }

        let mut state = self.inner.lock();
        let written = state.file.write_at(state.cursor, src)?;
        state.cursor = state.cursor.saturating_add(written);
        Ok(written)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().file.len()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn cursor(&self) -> usize {
        self.inner.lock().cursor
    }

    pub fn seek(
        &mut self,
        offset: i64,
        whence: FileHandleSeekWhence,
    ) -> Result<u64, FileHandleSeekError> {
        self.inner.lock().seek(offset, whence)
    }
}

impl VfsFileState {
    fn seek(
        &mut self,
        offset: i64,
        whence: FileHandleSeekWhence,
    ) -> Result<u64, FileHandleSeekError> {
        let len = self.file.len() as i128;
        let cursor = self.cursor as i128;
        let next = match whence {
            FileHandleSeekWhence::Start => Some(offset as i128),
            FileHandleSeekWhence::Current => cursor.checked_add(offset as i128),
            FileHandleSeekWhence::End => len.checked_add(offset as i128),
        }
        .ok_or(FileHandleSeekError::InvalidPosition)?;

        if next < 0 || next > len {
            return Err(FileHandleSeekError::InvalidPosition);
        }

        self.cursor = next as usize;
        Ok(next as u64)
    }
}

impl VfsFileObject for ReadOnlyMemoryFile {
    fn path(&self) -> &str {
        self.path.as_str()
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn read_at(&self, offset: usize, dest: &mut [u8]) -> usize {
        if dest.is_empty() || offset >= self.bytes.len() {
            return 0;
        }

        let read = dest.len().min(self.bytes.len() - offset);
        dest[..read].copy_from_slice(&self.bytes[offset..offset + read]);
        read
    }

    fn write_at(&self, _offset: usize, _src: &[u8]) -> Result<usize, FileHandleWriteError> {
        Err(FileHandleWriteError::ReadOnly)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileHandleWriteError {
    ReadOnly,
    // Retained for writable backends that reject writes without being strictly read-only.
    #[allow(dead_code)]
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileHandleSeekWhence {
    Start,
    Current,
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileHandleSeekError {
    InvalidPosition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleStreamKind {
    Input,
    Output,
    Error,
}

#[derive(Debug, Clone)]
pub enum KernelHandle {
    Console(ConsoleStreamKind),
    Device(DeviceHandle),
    Epoll(EpollHandle),
    Memfd(MemfdHandle),
    Socket(SocketHandle),
    VfsFile(VfsFileHandle),
    VfsDirectory(VfsDirectoryHandle),
    DisplaySurface(DisplaySurfaceHandle),
}

#[derive(Debug, Clone)]
pub struct HandleEntry {
    handle: KernelHandle,
    fd_flags: u32,
    status_flags: u64,
}

impl HandleEntry {
    pub fn new(handle: KernelHandle, fd_flags: u32, status_flags: u64) -> Self {
        Self {
            handle,
            fd_flags,
            status_flags: status_flags & STATUS_FLAG_MASK,
        }
    }

    pub fn handle(&self) -> &KernelHandle {
        &self.handle
    }

    pub fn handle_mut(&mut self) -> &mut KernelHandle {
        &mut self.handle
    }

    pub fn into_handle(self) -> KernelHandle {
        self.handle
    }

    pub fn fd_flags(&self) -> u32 {
        self.fd_flags
    }

    pub fn set_fd_flags(&mut self, fd_flags: u32) {
        self.fd_flags = fd_flags & FD_CLOEXEC;
    }

    pub fn status_flags(&self) -> u64 {
        self.status_flags
    }

    pub fn set_status_flags(&mut self, status_flags: u64) {
        let access_mode = self.status_flags & linux_abi::O_ACCMODE;
        let mutable = status_flags & !linux_abi::O_ACCMODE;
        self.status_flags = access_mode | (mutable & STATUS_FLAG_MASK);
    }
}

pub struct HandleTable {
    entries: Vec<Option<HandleEntry>>,
}

impl HandleTable {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn install(&mut self, handle: KernelHandle) -> u64 {
        self.install_with_open_flags(handle, 0)
    }

    pub fn install_with_open_flags(&mut self, handle: KernelHandle, open_flags: u64) -> u64 {
        let fd_flags = if open_flags & linux_abi::O_CLOEXEC != 0 {
            FD_CLOEXEC
        } else {
            0
        };
        let status_flags = open_flags & STATUS_FLAG_MASK;
        self.install_entry(HandleEntry::new(handle, fd_flags, status_flags))
    }

    pub fn install_entry(&mut self, entry: HandleEntry) -> u64 {
        self.install_entry_min(entry, FIRST_DYNAMIC_FD as u64)
    }

    pub fn install_entry_min(&mut self, entry: HandleEntry, min_fd: u64) -> u64 {
        let start_index = dynamic_index(min_fd.max(FIRST_DYNAMIC_FD as u64)).unwrap_or(0);
        if let Some(index) = self
            .entries
            .iter()
            .enumerate()
            .skip(start_index)
            .find_map(|(index, entry)| entry.is_none().then_some(index))
        {
            on_handle_open(entry.handle());
            self.entries[index] = Some(entry);
            return FIRST_DYNAMIC_FD as u64 + index as u64;
        }

        if self.entries.len() < start_index {
            self.entries.resize_with(start_index, || None);
        }
        on_handle_open(entry.handle());
        self.entries.push(Some(entry));
        FIRST_DYNAMIC_FD as u64 + (self.entries.len() - 1) as u64
    }

    pub fn get(&self, fd: u64) -> Option<&KernelHandle> {
        Some(self.get_entry(fd)?.handle())
    }

    pub fn get_mut(&mut self, fd: u64) -> Option<&mut KernelHandle> {
        Some(self.get_entry_mut(fd)?.handle_mut())
    }

    pub fn get_entry(&self, fd: u64) -> Option<&HandleEntry> {
        let index = dynamic_index(fd)?;
        self.entries.get(index)?.as_ref()
    }

    pub fn get_entry_mut(&mut self, fd: u64) -> Option<&mut HandleEntry> {
        let index = dynamic_index(fd)?;
        self.entries.get_mut(index)?.as_mut()
    }

    pub fn ensure_entry_capacity(&mut self, index: usize) {
        if self.entries.len() <= index {
            self.entries.resize_with(index + 1, || None);
        }
    }

    pub fn replace_entry(&mut self, fd: u64, entry: Option<HandleEntry>) -> Option<()> {
        let index = dynamic_index(fd)?;
        self.ensure_entry_capacity(index);
        if let Some(existing) = self.entries[index].as_ref() {
            on_handle_close(existing.handle());
        }
        if let Some(new_entry) = entry.as_ref() {
            on_handle_open(new_entry.handle());
        }
        self.entries[index] = entry;
        Some(())
    }

    pub fn close(&mut self, fd: u64) -> Option<KernelHandle> {
        let index = dynamic_index(fd)?;
        let handle = self
            .entries
            .get_mut(index)?
            .take()
            .map(HandleEntry::into_handle)?;
        on_handle_close(&handle);
        Some(handle)
    }

    pub fn close_cloexec(&mut self) {
        for entry in &mut self.entries {
            let Some(current) = entry.as_ref() else {
                continue;
            };
            if current.fd_flags() & FD_CLOEXEC == 0 {
                continue;
            }

            let handle = entry
                .take()
                .expect("close-on-exec entry must exist")
                .into_handle();
            on_handle_close(&handle);
        }
    }

    pub fn duplicate_min(&mut self, fd: u64, min_fd: u64, close_on_exec: bool) -> Option<u64> {
        let mut entry = self.get_entry(fd)?.clone();
        entry.set_fd_flags(if close_on_exec { FD_CLOEXEC } else { 0 });
        Some(self.install_entry_min(entry, min_fd))
    }

    pub fn duplicate_exact(&mut self, fd: u64, new_fd: u64, close_on_exec: bool) -> Option<u64> {
        if new_fd < FIRST_DYNAMIC_FD as u64 {
            return None;
        }

        let mut entry = self.get_entry(fd)?.clone();
        entry.set_fd_flags(if close_on_exec { FD_CLOEXEC } else { 0 });
        let index = dynamic_index(new_fd)?;
        self.ensure_entry_capacity(index);
        if let Some(existing) = self.entries[index].as_ref() {
            on_handle_close(existing.handle());
        }
        on_handle_open(entry.handle());
        self.entries[index] = Some(entry);
        Some(new_fd)
    }

    pub fn clear_surface_mappings_in_range(&mut self, start: u64, len: u64) {
        let Some(end) = start.checked_add(len) else {
            return;
        };

        for entry in &mut self.entries {
            let Some(entry) = entry.as_mut() else {
                continue;
            };
            let KernelHandle::DisplaySurface(surface) = entry.handle_mut() else {
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

impl Drop for HandleTable {
    fn drop(&mut self) {
        for entry in self.entries.iter().flatten() {
            on_handle_close(entry.handle());
        }
    }
}

fn dynamic_index(fd: u64) -> Option<usize> {
    fd.checked_sub(FIRST_DYNAMIC_FD as u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn on_handle_open(handle: &KernelHandle) {
    if let KernelHandle::Device(device) = handle {
        if device.device_id() == crate::io::device::DeviceId::Input {
            crate::driver::linux::input::consumer_acquire();
        }
    }
}

fn on_handle_close(handle: &KernelHandle) {
    if let KernelHandle::Device(device) = handle {
        if device.device_id() == crate::io::device::DeviceId::Input {
            crate::driver::linux::input::consumer_release();
        }
    }
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

    use super::{
        FD_CLOEXEC, FileHandleSeekError, FileHandleSeekWhence, HandleEntry, HandleTable,
        KernelHandle, VfsFileHandle,
    };
    use crate::user::linux as linux_abi;

    #[test]
    fn read_at_preserves_stream_cursor() {
        let mut handle = VfsFileHandle::read_only_memory("/test".into(), vec![1, 2, 3, 4, 5]);
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
        let mut handle = VfsFileHandle::read_only_memory("/test".into(), vec![0; 8]);

        assert_eq!(handle.seek(3, FileHandleSeekWhence::Start), Ok(3));
        assert_eq!(handle.cursor(), 3);

        assert_eq!(handle.seek(2, FileHandleSeekWhence::Current), Ok(5));
        assert_eq!(handle.cursor(), 5);

        assert_eq!(handle.seek(-1, FileHandleSeekWhence::End), Ok(7));
        assert_eq!(handle.cursor(), 7);
    }

    #[test]
    fn seek_rejects_positions_before_start_or_after_end() {
        let mut handle = VfsFileHandle::read_only_memory("/test".into(), vec![0; 4]);

        assert_eq!(
            handle.seek(-1, FileHandleSeekWhence::Start),
            Err(FileHandleSeekError::InvalidPosition)
        );
        assert_eq!(
            handle.seek(1, FileHandleSeekWhence::End),
            Err(FileHandleSeekError::InvalidPosition)
        );
    }

    #[test]
    fn install_entry_min_keeps_existing_dynamic_fds() {
        let mut table = HandleTable::new();

        let fd0 = table.install(KernelHandle::VfsFile(VfsFileHandle::read_only_memory(
            "/a".into(),
            vec![1],
        )));
        let fd1 = table.install(KernelHandle::VfsFile(VfsFileHandle::read_only_memory(
            "/b".into(),
            vec![2],
        )));
        let fd2 = table.install(KernelHandle::VfsFile(VfsFileHandle::read_only_memory(
            "/c".into(),
            vec![3],
        )));

        assert_eq!(fd0, 3);
        assert_eq!(fd1, 4);
        assert_eq!(fd2, 5);
    }

    #[test]
    fn close_cloexec_removes_only_flagged_entries() {
        let mut table = HandleTable::new();

        let keep_fd = table.install_entry(HandleEntry::new(
            KernelHandle::VfsFile(VfsFileHandle::read_only_memory("/keep".into(), vec![1])),
            0,
            0,
        ));
        let drop_fd = table.install_entry(HandleEntry::new(
            KernelHandle::VfsFile(VfsFileHandle::read_only_memory("/drop".into(), vec![2])),
            FD_CLOEXEC,
            0,
        ));

        table.close_cloexec();

        assert!(table.get(keep_fd).is_some());
        assert!(table.get(drop_fd).is_none());
    }

    #[test]
    fn set_status_flags_preserves_access_mode_and_masks_unknown_bits() {
        let mut entry = HandleEntry::new(
            KernelHandle::VfsFile(VfsFileHandle::read_only_memory("/flags".into(), vec![1])),
            0,
            linux_abi::O_RDWR | linux_abi::O_APPEND,
        );

        entry.set_status_flags(linux_abi::O_RDONLY | linux_abi::O_NONBLOCK | (1_u64 << 63));
        assert_eq!(
            entry.status_flags() & linux_abi::O_ACCMODE,
            linux_abi::O_RDWR
        );
        assert_ne!(entry.status_flags() & linux_abi::O_NONBLOCK, 0);
        assert_eq!(entry.status_flags() & linux_abi::O_APPEND, 0);
        assert_eq!(entry.status_flags() & (1_u64 << 63), 0);
    }

    #[test]
    fn duplicate_exact_replaces_target_and_applies_cloexec_flag() {
        let mut table = HandleTable::new();
        let source_fd = table.install_entry(HandleEntry::new(
            KernelHandle::VfsFile(VfsFileHandle::read_only_memory("/source".into(), vec![1])),
            0,
            linux_abi::O_RDONLY,
        ));
        let target_fd = table.install_entry(HandleEntry::new(
            KernelHandle::VfsFile(VfsFileHandle::read_only_memory("/target".into(), vec![2])),
            0,
            linux_abi::O_RDONLY,
        ));

        assert_eq!(
            table.duplicate_exact(source_fd, target_fd, true),
            Some(target_fd)
        );
        let replaced = table.get_entry(target_fd).expect("duplicated entry");
        assert_eq!(replaced.fd_flags() & FD_CLOEXEC, FD_CLOEXEC);
        match replaced.handle() {
            KernelHandle::VfsFile(file) => assert_eq!(file.path(), "/source"),
            other => panic!("expected VfsFile after dup2-style replace, got {other:?}"),
        }
    }
}
