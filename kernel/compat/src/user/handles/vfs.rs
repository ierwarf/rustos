use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Debug;
use spin::Mutex;

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
    fn shared_bytes(&self) -> Option<Arc<[u8]>> {
        None
    }
}

#[derive(Debug)]
struct ReadOnlyMemoryFile {
    path: String,
    bytes: Arc<[u8]>,
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

    pub(crate) fn token_id(&self) -> u64 {
        self.entries.first().map(|entry| entry.inode()).unwrap_or(0)
    }
}

impl VfsFileHandle {
    pub fn new(file: Arc<dyn VfsFileObject>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VfsFileState { file, cursor: 0 })),
        }
    }

    pub fn read_only_memory(path: String, bytes: Vec<u8>) -> Self {
        Self::read_only_memory_shared(path, Arc::<[u8]>::from(bytes.into_boxed_slice()))
    }

    pub fn read_only_memory_shared(path: String, bytes: Arc<[u8]>) -> Self {
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

    pub fn shared_bytes(&self) -> Option<Arc<[u8]>> {
        self.inner.lock().file.shared_bytes()
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

    pub(crate) fn token_id(&self) -> u64 {
        Arc::as_ptr(&self.inner) as usize as u64
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

    fn shared_bytes(&self) -> Option<Arc<[u8]>> {
        Some(self.bytes.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileHandleWriteError {
    ReadOnly,
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

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{FileHandleSeekError, FileHandleSeekWhence, VfsFileHandle};

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
}
