use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

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
