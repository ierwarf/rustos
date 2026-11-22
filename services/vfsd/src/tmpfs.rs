//! Memory-backed `TmpFs`: the writable POSIX filesystem behind `/tmp`,
//! `/dev/shm` and `/run`.
//!
//! - **Owner:** vfsd owns every `TmpFs` instance; each mount has its own inode
//!   table, byte quota and inode limit. Nothing survives a vfsd restart.
//! - **Model:** an inode table keyed by a per-mount unique, never reused
//!   `node_id`. Directories map names to node ids, so rename moves an entry,
//!   not data. File data is a sparse map of 4-KiB chunks.
//! - **Open while unlinked:** the volume trait is path based, so an open
//!   description pins its node (`retain_node`) and all later I/O addresses the
//!   node id. `unlink`/`rmdir`/replacing `rename` only drop a directory entry
//!   and decrement `nlink`; the node and its charged bytes are freed when
//!   `nlink == 0` and the last pin is released.
//! - **Quota:** `used_bytes` counts allocated data chunks plus symlink target
//!   bytes. Sparse `ftruncate` growth allocates nothing. Exhaustion is
//!   `FsError::NoSpace`; a write that can only partly fit is a short write.
//!   A separate inode limit bounds the node table and therefore vfsd's heap.
//! - **Concurrency:** one spin lock serialises all state; no method calls out
//!   while holding it.
//! - **Shared backing:** a writable `MAP_SHARED` mapping must see the frames
//!   `read`/`write`/`ftruncate` use, so `shared_backing_node` moves an inode's
//!   bytes once into a [`SharedFrames`] object (a vfsd-owned memfd in the
//!   service) and the inode is *shared-backed* for the rest of its life: its
//!   chunk map is empty, every I/O goes to the frames, and the memfd length
//!   always equals the file length. Frame ownership is the `Node`, so it dies
//!   with the node; mappings keep the frames alive past that through their own
//!   kernel hold. A shared-backed file is dense: quota is
//!   `ceil(len / 4096)` pages (sparse growth is charged), and a shrink below
//!   the mapped extent is `Busy` (the kernel cannot revoke other processes'
//!   page-table entries), never a silent dangling page.
//! - **Forbidden:** following symlinks (the caller resolves them), reusing a
//!   node id, charging more than the quota, or reading stale bytes after a
//!   shrink-then-grow (the tail of the last chunk is zeroed on shrink).

use alloc::boxed::Box;
use alloc::collections::btree_map::Entry;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

use storage_core::fs::{
    CasePolicy, FsCapabilities, FsCreateAttrs, FsDirEntry, FsError, FsMetadata, FsNodeKind,
    FsOpenOptions, FsSharedBacking, FsStatFs, FsTimestamp, FsVolume,
};

/// `statfs.f_type` of tmpfs (`TMPFS_MAGIC`).
pub const TMPFS_MAGIC: u64 = 0x0102_1994;
pub const TMPFS_BLOCK_SIZE: u64 = 4096;
pub const TMPFS_NAME_MAX: u32 = 255;
/// Longest symlink target (`PATH_MAX - 1`).
pub const TMPFS_SYMLINK_MAX: usize = 4095;
/// Largest file offset/length (`i64::MAX`).
pub const TMPFS_MAX_FILE_SIZE: u64 = i64::MAX as u64;
/// Linux reports a directory's size as `entries * BOGO_DIRENT_SIZE`.
const BOGO_DIRENT_SIZE: u64 = 20;
const CHUNK: usize = TMPFS_BLOCK_SIZE as usize;
const ROOT_NODE: u64 = 1;

pub const TMP_QUOTA_BYTES: u64 = 64 * 1024 * 1024;
pub const DEV_SHM_QUOTA_BYTES: u64 = 64 * 1024 * 1024;
pub const RUN_QUOTA_BYTES: u64 = 16 * 1024 * 1024;
pub const TMP_MAX_INODES: u64 = 16 * 1024;
pub const DEV_SHM_MAX_INODES: u64 = 4 * 1024;
pub const RUN_MAX_INODES: u64 = 4 * 1024;

/// Frames that back one shared-backed regular file. The implementation owns
/// the physical pages (a memfd); `TmpFs` owns the object and therefore the
/// pages' lifetime from the filesystem's point of view.
pub trait SharedFrames: Send {
    /// `(token, object)` the kernel validates before mapping: the service's
    /// descriptor number and the memfd's never-recycled object id.
    fn identity(&self) -> (u64, u64);
    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, FsError>;
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<usize, FsError>;
    /// Sets the exact byte length. Growth zero-fills; shrinking below an
    /// extent that is still mapped is `FsError::Busy`.
    fn set_len(&mut self, len: u64) -> Result<(), FsError>;
}

/// Creates frames holding `len` zero bytes. Failure leaves nothing behind.
pub type SharedFramesFactory = fn(len: u64) -> Result<Box<dyn SharedFrames>, FsError>;

#[derive(Clone, Copy, Debug)]
pub struct TmpFsConfig {
    pub quota_bytes: u64,
    pub max_inodes: u64,
    pub root_mode: u32,
    pub root_uid: u32,
    pub root_gid: u32,
    /// Wall/monotonic time source for `atime`/`mtime`/`ctime` stamps.
    pub clock: fn() -> FsTimestamp,
}

#[derive(Clone, Copy)]
struct DirLink {
    node: u64,
    cookie: u64,
}

enum Body {
    Dir(BTreeMap<String, DirLink>),
    File {
        len: u64,
        chunks: BTreeMap<u64, Box<[u8]>>,
    },
    Symlink(String),
    Socket,
    Fifo,
}

struct Node {
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    atime: FsTimestamp,
    mtime: FsTimestamp,
    ctime: FsTimestamp,
    crtime: FsTimestamp,
    /// Open-description pins (`retain_node` minus `release_node`).
    opens: u32,
    /// `O_TMPFILE` inode that may still receive its first name (Linux
    /// `I_LINKABLE`). An ordinary file unlinked while open is not linkable.
    linkable: bool,
    /// Parent directory (directories only; the root is its own parent).
    parent: u64,
    /// Name under `parent` (directories only; empty for the root). Kept in
    /// step with the parent's entry so a directory's path is an O(depth) walk
    /// up the `parent` chain instead of a scan of every parent's entries.
    dir_name: String,
    body: Body,
    /// Regular files only: when set, the file's bytes live here and the
    /// chunk map is empty (see the module's shared-backing rules).
    shared: Option<Box<dyn SharedFrames>>,
}

fn read_dir_locked(st: &State, ino: u64) -> Result<Vec<FsDirEntry>, FsError> {
    let mut out = Vec::new();
    for (name, link) in st.dir(ino)? {
        let meta = st.metadata_of(link.node)?;
        out.push(FsDirEntry {
            cookie: link.cookie,
            name: name.clone(), node_id: link.node, kind: meta.kind,
            len: if meta.kind == FsNodeKind::Directory { 0 } else { meta.len },
            dos_attrs: 0,
        });
    }
    out.sort_unstable_by_key(|entry| entry.cookie);
    Ok(out)
}

impl Node {
    fn kind(&self) -> FsNodeKind {
        match self.body {
            Body::Dir(_) => FsNodeKind::Directory,
            Body::File { .. } => FsNodeKind::File,
            Body::Symlink(_) => FsNodeKind::Symlink,
            Body::Socket => FsNodeKind::Socket,
            Body::Fifo => FsNodeKind::Fifo,
        }
    }

    /// Bytes this node charges against the quota.
    fn charged_bytes(&self) -> u64 {
        match &self.body {
            Body::File { len, .. } if self.shared.is_some() => pages(*len) * CHUNK as u64,
            Body::File { chunks, .. } => chunks.len() as u64 * CHUNK as u64,
            Body::Symlink(target) => target.len() as u64,
            _ => 0,
        }
    }

    fn touch_modify(&mut self, now: FsTimestamp) {
        self.mtime = now;
        self.ctime = now;
    }
}

struct State {
    nodes: BTreeMap<u64, Node>,
    next_node: u64,
    next_dir_cookie: u64,
    used_bytes: u64,
    quota_bytes: u64,
    max_inodes: u64,
}

pub struct TmpFs {
    clock: fn() -> FsTimestamp,
    shared_frames: Option<SharedFramesFactory>,
    state: Mutex<State>,
}

fn pages(len: u64) -> u64 {
    len.div_ceil(CHUNK as u64)
}

fn split_path(path: &str) -> Result<Vec<&str>, FsError> {
    let mut out = Vec::new();
    for part in path.split('/') {
        if part.is_empty() {
            continue;
        }
        check_name(part)?;
        out.push(part);
    }
    Ok(out)
}

fn check_name(name: &str) -> Result<(), FsError> {
    if name == "." || name == ".." || name.contains('\0') {
        return Err(FsError::InvalidInput);
    }
    if name.len() > TMPFS_NAME_MAX as usize {
        return Err(FsError::NameTooLong);
    }
    Ok(())
}

impl State {
    fn next_dir_cookie(&mut self) -> Result<u64, FsError> {
        let cookie = self.next_dir_cookie;
        if cookie >= crate::directory::FILE_COOKIE_BASE {
            return Err(FsError::NoSpace);
        }
        self.next_dir_cookie += 1;
        Ok(cookie)
    }

    fn dir(&self, ino: u64) -> Result<&BTreeMap<String, DirLink>, FsError> {
        match &self.nodes.get(&ino).ok_or(FsError::NotFound)?.body {
            Body::Dir(entries) => Ok(entries),
            _ => Err(FsError::NotADirectory),
        }
    }

    fn dir_mut(&mut self, ino: u64) -> Result<&mut BTreeMap<String, DirLink>, FsError> {
        match &mut self.nodes.get_mut(&ino).ok_or(FsError::NotFound)?.body {
            Body::Dir(entries) => Ok(entries),
            _ => Err(FsError::NotADirectory),
        }
    }

    /// Resolves `comps` without following symlinks (a symlink in a non-final
    /// position is `NotADirectory`; the caller resolves links beforehand).
    fn walk(&self, comps: &[&str]) -> Result<u64, FsError> {
        let mut cur = ROOT_NODE;
        for name in comps {
            cur = self.dir(cur)?.get(*name).ok_or(FsError::NotFound)?.node;
        }
        Ok(cur)
    }

    /// Splits off the final component: `(parent_dir_node, name)`.
    fn parent_of<'a>(&self, comps: &[&'a str]) -> Result<(u64, &'a str), FsError> {
        let (name, dirs) = comps.split_last().ok_or(FsError::InvalidInput)?;
        let parent = self.walk(dirs)?;
        self.dir(parent)?;
        Ok((parent, name))
    }

    fn metadata_of(&self, ino: u64) -> Result<FsMetadata, FsError> {
        let node = self.nodes.get(&ino).ok_or(FsError::NotFound)?;
        let (len, allocation) = match &node.body {
            Body::Dir(entries) => (entries.len() as u64 * BOGO_DIRENT_SIZE, 0),
            Body::File { len, .. } if node.shared.is_some() => (*len, pages(*len) * CHUNK as u64),
            Body::File { len, chunks } => (*len, chunks.len() as u64 * CHUNK as u64),
            Body::Symlink(target) => (target.len() as u64, 0),
            Body::Socket | Body::Fifo => (0, 0),
        };
        Ok(FsMetadata {
            node_id: ino,
            kind: node.kind(),
            len,
            allocation_size: allocation,
            nlink: node.nlink,
            mode: node.mode,
            uid: node.uid,
            gid: node.gid,
            dos_attrs: 0,
            atime: node.atime,
            mtime: node.mtime,
            ctime: node.ctime,
            crtime: node.crtime,
        })
    }

    fn charge(&mut self, bytes: u64) -> Result<(), FsError> {
        let total = self.used_bytes.checked_add(bytes).ok_or(FsError::NoSpace)?;
        if total > self.quota_bytes {
            return Err(FsError::NoSpace);
        }
        self.used_bytes = total;
        Ok(())
    }

    fn credit(&mut self, bytes: u64) {
        self.used_bytes = self.used_bytes.saturating_sub(bytes);
    }

    fn insert_node(
        &mut self,
        parent: u64,
        name: &str,
        attrs: FsCreateAttrs,
        body: Body,
        now: FsTimestamp,
    ) -> Result<u64, FsError> {
        if self.dir(parent)?.contains_key(name) {
            return Err(FsError::AlreadyExists);
        }
        if self.nodes.len() as u64 >= self.max_inodes {
            return Err(FsError::NoSpace);
        }
        let is_dir = matches!(body, Body::Dir(_));
        if let Body::Symlink(target) = &body {
            self.charge(target.len() as u64)?;
        }
        let cookie = self.next_dir_cookie()?;
        let ino = self.next_node;
        self.next_node += 1;
        self.nodes.insert(
            ino,
            Node {
                mode: attrs.mode & 0o7777,
                uid: attrs.uid,
                gid: attrs.gid,
                nlink: if is_dir { 2 } else { 1 },
                atime: now,
                mtime: now,
                ctime: now,
                crtime: now,
                opens: 0,
                linkable: false,
                parent: if is_dir { parent } else { 0 },
                dir_name: if is_dir { String::from(name) } else { String::new() },
                body,
                shared: None,
            },
        );
        self.dir_mut(parent)?.insert(String::from(name), DirLink { node: ino, cookie });
        let p = self.nodes.get_mut(&parent).ok_or(FsError::NotFound)?;
        if is_dir {
            p.nlink = p.nlink.saturating_add(1);
        }
        p.touch_modify(now);
        Ok(ino)
    }

    /// Frees `ino` when it has no name and no open description.
    fn free_if_orphan(&mut self, ino: u64) {
        let orphan = self
            .nodes
            .get(&ino)
            .is_some_and(|n| n.nlink == 0 && n.opens == 0);
        if orphan {
            if let Some(node) = self.nodes.remove(&ino) {
                self.credit(node.charged_bytes());
            }
        }
    }

    fn is_descendant_or_self(&self, mut ino: u64, ancestor: u64) -> bool {
        loop {
            if ino == ancestor {
                return true;
            }
            if ino == ROOT_NODE {
                return false;
            }
            match self.nodes.get(&ino) {
                Some(node) => ino = node.parent,
                None => return false,
            }
        }
    }

    fn read(&self, ino: u64, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        let node = self.nodes.get(&ino).ok_or(FsError::NotFound)?;
        if let Some(shared) = node.shared.as_ref() {
            let Body::File { len, .. } = &node.body else {
                return Err(FsError::InvalidInput);
            };
            if offset >= *len {
                return Ok(0);
            }
            let count = (*len - offset).min(out.len() as u64) as usize;
            return shared.read_at(offset, &mut out[..count]);
        }
        let (len, chunks) = match &node.body {
            Body::File { len, chunks } => (*len, chunks),
            Body::Dir(_) => return Err(FsError::IsADirectory),
            _ => return Err(FsError::InvalidInput),
        };
        if offset >= len {
            return Ok(0);
        }
        let count = (len - offset).min(out.len() as u64) as usize;
        let mut done = 0usize;
        while done < count {
            let pos = offset + done as u64;
            let idx = pos / CHUNK as u64;
            let within = (pos % CHUNK as u64) as usize;
            let n = (CHUNK - within).min(count - done);
            match chunks.get(&idx) {
                Some(chunk) => out[done..done + n].copy_from_slice(&chunk[within..within + n]),
                None => out[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(count)
    }

    fn write(
        &mut self,
        ino: u64,
        offset: u64,
        data: &[u8],
        now: FsTimestamp,
    ) -> Result<(usize, u64), FsError> {
        let State {
            nodes,
            used_bytes,
            quota_bytes,
            ..
        } = self;
        let node = nodes.get_mut(&ino).ok_or(FsError::NotFound)?;
        if node.shared.is_some() {
            return shared_write(node, used_bytes, *quota_bytes, offset, data, now);
        }
        let (len, chunks) = match &mut node.body {
            Body::File { len, chunks } => (len, chunks),
            Body::Dir(_) => return Err(FsError::IsADirectory),
            _ => return Err(FsError::InvalidInput),
        };
        if data.is_empty() {
            return Ok((0, *len));
        }
        let end = offset
            .checked_add(data.len() as u64)
            .filter(|end| *end <= TMPFS_MAX_FILE_SIZE)
            .ok_or(FsError::TooBig)?;
        let mut done = 0usize;
        while done < data.len() {
            let pos = offset + done as u64;
            let idx = pos / CHUNK as u64;
            let within = (pos % CHUNK as u64) as usize;
            let n = (CHUNK - within).min(data.len() - done);
            let chunk = match chunks.entry(idx) {
                Entry::Occupied(slot) => slot.into_mut(),
                Entry::Vacant(slot) => {
                    if used_bytes.saturating_add(CHUNK as u64) > *quota_bytes {
                        break;
                    }
                    *used_bytes += CHUNK as u64;
                    slot.insert(vec![0u8; CHUNK].into_boxed_slice())
                }
            };
            chunk[within..within + n].copy_from_slice(&data[done..done + n]);
            done += n;
        }
        if done == 0 {
            return Err(FsError::NoSpace);
        }
        let written_end = (offset + done as u64).min(end);
        if written_end > *len {
            *len = written_end;
        }
        let new_len = *len;
        node.touch_modify(now);
        Ok((done, new_len))
    }

    fn truncate(&mut self, ino: u64, new_len: u64, now: FsTimestamp) -> Result<u64, FsError> {
        if new_len > TMPFS_MAX_FILE_SIZE {
            return Err(FsError::TooBig);
        }
        let mut freed = 0u64;
        {
            let node = self.nodes.get_mut(&ino).ok_or(FsError::NotFound)?;
            if node.shared.is_some() {
                let State { nodes, used_bytes, quota_bytes, .. } = self;
                let node = nodes.get_mut(&ino).ok_or(FsError::NotFound)?;
                return shared_truncate(node, used_bytes, *quota_bytes, new_len, now);
            }
            let (len, chunks) = match &mut node.body {
                Body::File { len, chunks } => (len, chunks),
                Body::Dir(_) => return Err(FsError::IsADirectory),
                _ => return Err(FsError::InvalidInput),
            };
            if new_len <= *len {
                // A same-size truncate also discards KEEP_SIZE allocations
                // beyond EOF, even when the file has never contained data.
                // Chunks wholly past the new end go away; the tail of the
                // chunk holding the new end is zeroed so a later growth reads
                // zeros, not the truncated bytes.
                let first_dead = new_len.div_ceil(CHUNK as u64);
                freed = chunks.split_off(&first_dead).len() as u64 * CHUNK as u64;
                let tail = (new_len % CHUNK as u64) as usize;
                if tail != 0 {
                    if let Some(chunk) = chunks.get_mut(&(new_len / CHUNK as u64)) {
                        chunk[tail..].fill(0);
                    }
                }
            }
            *len = new_len;
            node.touch_modify(now);
        }
        self.credit(freed);
        Ok(new_len)
    }

    /// Moves a regular file's chunks into a fresh frame object (idempotent).
    /// Nothing changes on failure: the factory result is dropped and the
    /// chunk map and quota charge stay as they were.
    fn make_shared(
        &mut self,
        ino: u64,
        factory: SharedFramesFactory,
    ) -> Result<FsSharedBacking, FsError> {
        let State { nodes, used_bytes, quota_bytes, .. } = self;
        let node = nodes.get_mut(&ino).ok_or(FsError::NotFound)?;
        let Node { body, shared, .. } = node;
        let (len, chunks) = match body {
            Body::File { len, chunks } => (len, chunks),
            Body::Dir(_) => return Err(FsError::IsADirectory),
            _ => return Err(FsError::InvalidInput),
        };
        if let Some(frames) = shared.as_ref() {
            let (token, object) = frames.identity();
            return Ok(FsSharedBacking { token, object, len: *len });
        }
        let old_charge = chunks.len() as u64 * CHUNK as u64;
        let new_charge = pages(*len) * CHUNK as u64;
        if new_charge > old_charge && used_bytes.saturating_add(new_charge - old_charge) > *quota_bytes {
            return Err(FsError::NoSpace);
        }
        let mut frames = factory(*len)?;
        for (idx, chunk) in chunks.iter() {
            let start = idx * CHUNK as u64;
            if start >= *len {
                continue;
            }
            let n = ((*len - start).min(CHUNK as u64)) as usize;
            if frames.write_at(start, &chunk[..n])? != n {
                return Err(FsError::Io(storage_core::StorageError::DeviceFault));
            }
        }
        let (token, object) = frames.identity();
        *used_bytes = used_bytes.saturating_sub(old_charge) + new_charge;
        chunks.clear();
        *shared = Some(frames);
        Ok(FsSharedBacking { token, object, len: *len })
    }

    fn fallocate(
        &mut self,
        ino: u64,
        offset: u64,
        len: u64,
        keep_size: bool,
        now: FsTimestamp,
    ) -> Result<u64, FsError> {
        if len == 0 {
            return Err(FsError::InvalidInput);
        }
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= TMPFS_MAX_FILE_SIZE)
            .ok_or(FsError::TooBig)?;
        if self.nodes.get(&ino).is_some_and(|n| n.shared.is_some()) {
            let State { nodes, used_bytes, quota_bytes, .. } = self;
            let node = nodes.get_mut(&ino).ok_or(FsError::NotFound)?;
            return shared_fallocate(node, used_bytes, *quota_bytes, end, keep_size, now);
        }
        let first = offset / CHUNK as u64;
        let last = (end - 1) / CHUNK as u64;
        let missing = {
            let node = self.nodes.get(&ino).ok_or(FsError::NotFound)?;
            let chunks = match &node.body {
                Body::File { chunks, .. } => chunks,
                Body::Dir(_) => return Err(FsError::IsADirectory),
                _ => return Err(FsError::InvalidInput),
            };
            (last - first + 1) - chunks.range(first..=last).count() as u64
        };
        let cost = missing.checked_mul(CHUNK as u64).ok_or(FsError::NoSpace)?;
        self.charge(cost)?;
        let node = self.nodes.get_mut(&ino).ok_or(FsError::NotFound)?;
        let Body::File { len: flen, chunks } = &mut node.body else {
            return Err(FsError::InvalidInput);
        };
        if missing != 0 {
            for idx in first..=last {
                chunks
                    .entry(idx)
                    .or_insert_with(|| vec![0u8; CHUNK].into_boxed_slice());
            }
        }
        if !keep_size && end > *flen {
            *flen = end;
        }
        let out = *flen;
        node.touch_modify(now);
        Ok(out)
    }

    fn remove_entry(&mut self, path: &[&str], want_dir: bool, now: FsTimestamp) -> Result<(), FsError> {
        let (parent, name) = self.parent_of(path)?;
        let child = self.dir(parent)?.get(name).ok_or(FsError::NotFound)?.node;
        let is_dir = matches!(self.nodes.get(&child).ok_or(FsError::NotFound)?.body, Body::Dir(_));
        if want_dir {
            if !is_dir {
                return Err(FsError::NotADirectory);
            }
            if !self.dir(child)?.is_empty() {
                return Err(FsError::DirectoryNotEmpty);
            }
        } else if is_dir {
            return Err(FsError::IsADirectory);
        }
        self.dir_mut(parent)?.remove(name);
        {
            let p = self.nodes.get_mut(&parent).ok_or(FsError::NotFound)?;
            if is_dir {
                p.nlink = p.nlink.saturating_sub(1);
            }
            p.touch_modify(now);
        }
        let node = self.nodes.get_mut(&child).ok_or(FsError::NotFound)?;
        node.nlink = if is_dir { 0 } else { node.nlink.saturating_sub(1) };
        node.ctime = now;
        self.free_if_orphan(child);
        Ok(())
    }

    fn rename(
        &mut self,
        src: &[&str],
        dst: &[&str],
        replace_existing: bool,
        no_replace: bool,
        now: FsTimestamp,
    ) -> Result<(), FsError> {
        if src.is_empty() || dst.is_empty() {
            return Err(FsError::Busy);
        }
        let (sp, sname) = self.parent_of(src)?;
        let source = *self.dir(sp)?.get(sname).ok_or(FsError::NotFound)?;
        let sino = source.node;
        let (dp, dname) = self.parent_of(dst)?;
        let src_is_dir = matches!(self.nodes.get(&sino).ok_or(FsError::NotFound)?.body, Body::Dir(_));
        if src_is_dir && self.is_descendant_or_self(dp, sino) {
            // Moving a directory below itself would detach it from the tree.
            return Err(FsError::InvalidInput);
        }
        let existing = self.dir(dp)?.get(dname).map(|link| link.node);
        if let Some(dino) = existing {
            if dino == sino {
                // Same node (same path, or two hard links): POSIX no-op.
                return if no_replace { Err(FsError::AlreadyExists) } else { Ok(()) };
            }
            if no_replace || !replace_existing {
                return Err(FsError::AlreadyExists);
            }
            let dst_is_dir = matches!(
                self.nodes.get(&dino).ok_or(FsError::NotFound)?.body,
                Body::Dir(_)
            );
            match (src_is_dir, dst_is_dir) {
                (true, false) => return Err(FsError::NotADirectory),
                (false, true) => return Err(FsError::IsADirectory),
                (true, true) if !self.dir(dino)?.is_empty() => {
                    return Err(FsError::DirectoryNotEmpty)
                }
                _ => {}
            }
        }
        // Keep the position for same-directory renames. A cross-directory
        // move is a new entry in the destination's enumeration order.
        let cookie = if sp == dp { source.cookie } else { self.next_dir_cookie()? };
        if let Some(dino) = existing {
            let dst_is_dir = matches!(self.nodes[&dino].body, Body::Dir(_));
            self.dir_mut(dp)?.remove(dname);
            if dst_is_dir {
                let p = self.nodes.get_mut(&dp).ok_or(FsError::NotFound)?;
                p.nlink = p.nlink.saturating_sub(1);
            }
            let node = self.nodes.get_mut(&dino).ok_or(FsError::NotFound)?;
            node.nlink = if dst_is_dir { 0 } else { node.nlink.saturating_sub(1) };
            node.ctime = now;
        }
        self.dir_mut(sp)?.remove(sname);
        self.dir_mut(dp)?.insert(String::from(dname), DirLink { node: sino, cookie });
        if src_is_dir && sp != dp {
            let s = self.nodes.get_mut(&sp).ok_or(FsError::NotFound)?;
            s.nlink = s.nlink.saturating_sub(1);
            let d = self.nodes.get_mut(&dp).ok_or(FsError::NotFound)?;
            d.nlink = d.nlink.saturating_add(1);
            self.nodes.get_mut(&sino).ok_or(FsError::NotFound)?.parent = dp;
        }
        if src_is_dir && (sp != dp || sname != dname) {
            self.nodes.get_mut(&sino).ok_or(FsError::NotFound)?.dir_name = String::from(dname);
        }
        for dir in [sp, dp] {
            self.nodes.get_mut(&dir).ok_or(FsError::NotFound)?.touch_modify(now);
        }
        self.nodes.get_mut(&sino).ok_or(FsError::NotFound)?.ctime = now;
        if let Some(dino) = existing {
            self.free_if_orphan(dino);
        }
        Ok(())
    }
}

impl TmpFs {
    pub fn new(config: TmpFsConfig) -> Self {
        let now = (config.clock)();
        let mut nodes = BTreeMap::new();
        nodes.insert(
            ROOT_NODE,
            Node {
                mode: config.root_mode & 0o7777,
                uid: config.root_uid,
                gid: config.root_gid,
                nlink: 2,
                atime: now,
                mtime: now,
                ctime: now,
                crtime: now,
                opens: 0,
                linkable: false,
                parent: ROOT_NODE,
                dir_name: String::new(),
                body: Body::Dir(BTreeMap::new()),
                shared: None,
            },
        );
        Self {
            clock: config.clock,
            shared_frames: None,
            state: Mutex::new(State {
                nodes,
                next_node: ROOT_NODE + 1,
                next_dir_cookie: 1,
                used_bytes: 0,
                quota_bytes: config.quota_bytes,
                max_inodes: config.max_inodes.max(1),
            }),
        }
    }

    /// Enables `shared_backing_node` with `factory` as the frame source.
    pub fn with_shared_frames(mut self, factory: SharedFramesFactory) -> Self {
        self.shared_frames = Some(factory);
        self
    }

    fn now(&self) -> FsTimestamp {
        (self.clock)()
    }

    /// Bytes currently charged against the quota.
    pub fn used_bytes(&self) -> u64 {
        self.state.lock().used_bytes
    }

    pub fn node_count(&self) -> usize {
        self.state.lock().nodes.len()
    }

    fn create(&self, path: &str, attrs: FsCreateAttrs, body: Body) -> Result<u64, FsError> {
        let comps = split_path(path)?;
        if comps.is_empty() {
            // The mount root always exists.
            return Err(FsError::AlreadyExists);
        }
        let now = self.now();
        let mut st = self.state.lock();
        let (parent, name) = st.parent_of(&comps)?;
        st.insert_node(parent, name, attrs, body, now)
    }
}

impl FsVolume for TmpFs {
    fn capabilities(&self) -> FsCapabilities {
        FsCapabilities {
            read_only: false,
            case_policy: CasePolicy::Exact,
            supports_symlinks: true,
            supports_hardlinks: true,
            supports_unlink_while_open: true,
            supports_sparse: true,
            supports_nt_attrs: false,
            supports_streams: false,
            supports_share_modes: false,
            max_name_len: TMPFS_NAME_MAX,
        }
    }

    fn is_immutable(&self) -> bool {
        false
    }

    fn canonical_key(&self, path: &str) -> Result<String, FsError> {
        let comps = split_path(path)?;
        Ok(comps.join("/"))
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, FsError> {
        let comps = split_path(path)?;
        let st = self.state.lock();
        let ino = st.walk(&comps)?;
        st.metadata_of(ino)
    }

    fn read_dir(&self, path: &str) -> Result<Vec<FsDirEntry>, FsError> {
        let comps = split_path(path)?;
        let st = self.state.lock();
        let ino = st.walk(&comps)?;
        read_dir_locked(&st, ino)
    }

    fn read_dir_node(&self, node: u64) -> Result<Vec<FsDirEntry>, FsError> {
        read_dir_locked(&self.state.lock(), node)
    }

    fn directory_path_node(&self, mut node: u64) -> Result<String, FsError> {
        let st = self.state.lock();
        st.dir(node)?;
        // A removed directory (`rmdir` while pinned) has no path. Every
        // ancestor of a live directory is live, so checking the start suffices.
        if st.nodes.get(&node).ok_or(FsError::NotFound)?.nlink == 0 {
            return Err(FsError::NotFound);
        }
        let mut parts: Vec<&str> = Vec::new();
        let mut total = 0usize;
        while node != ROOT_NODE {
            let current = st.nodes.get(&node).ok_or(FsError::NotFound)?;
            total += current.dir_name.len() + 1;
            parts.push(current.dir_name.as_str());
            node = current.parent;
        }
        let mut out = String::with_capacity(total);
        for part in parts.iter().rev() {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(part);
        }
        Ok(out)
    }

    fn statfs(&self) -> Result<FsStatFs, FsError> {
        let st = self.state.lock();
        let total = st.quota_bytes / TMPFS_BLOCK_SIZE;
        let used = st.used_bytes.div_ceil(TMPFS_BLOCK_SIZE);
        let free = total.saturating_sub(used);
        Ok(FsStatFs {
            fs_type_magic: TMPFS_MAGIC,
            block_size: TMPFS_BLOCK_SIZE,
            total_blocks: total,
            free_blocks: free,
            avail_blocks: free,
            total_inodes: st.max_inodes,
            free_inodes: st.max_inodes.saturating_sub(st.nodes.len() as u64),
            max_name_len: TMPFS_NAME_MAX,
            fs_name: "tmpfs",
            case_sensitive: true,
            case_preserved: true,
        })
    }

    fn open_prepare(&self, path: &str, opts: FsOpenOptions) -> Result<FsMetadata, FsError> {
        self.open_prepare_with(path, opts, FsCreateAttrs::new(0o644, 0, 0))
    }

    fn open_prepare_with(
        &self,
        path: &str,
        opts: FsOpenOptions,
        attrs: FsCreateAttrs,
    ) -> Result<FsMetadata, FsError> {
        let comps = split_path(path)?;
        let now = self.now();
        let mut st = self.state.lock();
        let ino = if comps.is_empty() {
            ROOT_NODE
        } else {
            let (parent, name) = st.parent_of(&comps)?;
            match st.dir(parent)?.get(name).map(|link| link.node) {
                Some(existing) => {
                    if opts.create_new {
                        return Err(FsError::AlreadyExists);
                    }
                    existing
                }
                None if opts.create => {
                    st.insert_node(parent, name, attrs, Body::File { len: 0, chunks: BTreeMap::new() }, now)?
                }
                None => return Err(FsError::NotFound),
            }
        };
        let kind = st.nodes.get(&ino).ok_or(FsError::NotFound)?.kind();
        if kind == FsNodeKind::Directory {
            if opts.create_new && comps.is_empty() {
                return Err(FsError::AlreadyExists);
            }
            // `open(dir, O_CREAT)` is EISDIR even read-only.
            if opts.write || opts.truncate || opts.create {
                return Err(FsError::IsADirectory);
            }
        } else if kind == FsNodeKind::File && opts.truncate && opts.write {
            st.truncate(ino, 0, now)?;
        }
        st.metadata_of(ino)
    }

    fn read_at(&self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        let comps = split_path(path)?;
        let st = self.state.lock();
        let ino = st.walk(&comps)?;
        st.read(ino, offset, out)
    }

    fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<(usize, u64), FsError> {
        let comps = split_path(path)?;
        let now = self.now();
        let mut st = self.state.lock();
        let ino = st.walk(&comps)?;
        st.write(ino, offset, data, now)
    }

    fn truncate(&self, path: &str, new_len: u64) -> Result<u64, FsError> {
        let comps = split_path(path)?;
        let now = self.now();
        let mut st = self.state.lock();
        let ino = st.walk(&comps)?;
        st.truncate(ino, new_len, now)
    }

    fn mkdir(&self, path: &str) -> Result<(), FsError> {
        self.mkdir_with(path, FsCreateAttrs::new(0o755, 0, 0))
    }

    fn mkdir_with(&self, path: &str, attrs: FsCreateAttrs) -> Result<(), FsError> {
        self.create(path, attrs, Body::Dir(BTreeMap::new())).map(|_| ())
    }

    fn unlink(&self, path: &str) -> Result<(), FsError> {
        let comps = split_path(path)?;
        if comps.is_empty() {
            return Err(FsError::IsADirectory);
        }
        let now = self.now();
        self.state.lock().remove_entry(&comps, false, now)
    }

    fn rmdir(&self, path: &str) -> Result<(), FsError> {
        let comps = split_path(path)?;
        if comps.is_empty() {
            return Err(FsError::Busy);
        }
        let now = self.now();
        self.state.lock().remove_entry(&comps, true, now)
    }

    fn rename(
        &self,
        src: &str,
        dst: &str,
        replace_existing: bool,
        no_replace: bool,
    ) -> Result<(), FsError> {
        let src = split_path(src)?;
        let dst = split_path(dst)?;
        let now = self.now();
        self.state
            .lock()
            .rename(&src, &dst, replace_existing, no_replace, now)
    }

    fn set_times(
        &self,
        path: &str,
        atime: Option<FsTimestamp>,
        mtime: Option<FsTimestamp>,
        crtime: Option<FsTimestamp>,
    ) -> Result<(), FsError> {
        let comps = split_path(path)?;
        let now = self.now();
        let mut st = self.state.lock();
        let ino = st.walk(&comps)?;
        let node = st.nodes.get_mut(&ino).ok_or(FsError::NotFound)?;
        if let Some(t) = atime {
            node.atime = t;
        }
        if let Some(t) = mtime {
            node.mtime = t;
        }
        if let Some(t) = crtime {
            node.crtime = t;
        }
        node.ctime = now;
        Ok(())
    }

    fn symlink(&self, target: &str, linkpath: &str) -> Result<(), FsError> {
        self.symlink_with(target, linkpath, FsCreateAttrs::new(0o777, 0, 0))
    }

    fn symlink_with(
        &self,
        target: &str,
        linkpath: &str,
        attrs: FsCreateAttrs,
    ) -> Result<(), FsError> {
        if target.is_empty() {
            return Err(FsError::NotFound);
        }
        if target.len() > TMPFS_SYMLINK_MAX || target.contains('\0') {
            return Err(FsError::NameTooLong);
        }
        let attrs = FsCreateAttrs::new(0o777, attrs.uid, attrs.gid);
        self.create(linkpath, attrs, Body::Symlink(String::from(target)))
            .map(|_| ())
    }

    fn readlink(&self, path: &str) -> Result<String, FsError> {
        let comps = split_path(path)?;
        let st = self.state.lock();
        let ino = st.walk(&comps)?;
        match &st.nodes.get(&ino).ok_or(FsError::NotFound)?.body {
            Body::Symlink(target) => Ok(target.clone()),
            _ => Err(FsError::InvalidInput),
        }
    }

    fn mknod_socket(&self, path: &str, attrs: FsCreateAttrs) -> Result<(), FsError> {
        self.create(path, attrs, Body::Socket).map(|_| ())
    }

    fn mknod_fifo(&self, path: &str, attrs: FsCreateAttrs) -> Result<(), FsError> {
        self.create(path, attrs, Body::Fifo).map(|_| ())
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), FsError> {
        let comps = split_path(path)?;
        let now = self.now();
        let mut st = self.state.lock();
        let ino = st.walk(&comps)?;
        let node = st.nodes.get_mut(&ino).ok_or(FsError::NotFound)?;
        node.mode = mode & 0o7777;
        node.ctime = now;
        Ok(())
    }

    fn chown(&self, path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), FsError> {
        let comps = split_path(path)?;
        let now = self.now();
        let mut st = self.state.lock();
        let ino = st.walk(&comps)?;
        chown_locked(&mut st, ino, uid, gid, now)
    }

    fn link(&self, existing: &str, new_path: &str) -> Result<(), FsError> {
        let from = split_path(existing)?;
        let to = split_path(new_path)?;
        let now = self.now();
        let mut st = self.state.lock();
        let ino = st.walk(&from)?;
        link_node_locked(&mut st, ino, &to, now)
    }

    fn fallocate(
        &self,
        path: &str,
        offset: u64,
        len: u64,
        keep_size: bool,
    ) -> Result<u64, FsError> {
        let comps = split_path(path)?;
        let now = self.now();
        let mut st = self.state.lock();
        let ino = st.walk(&comps)?;
        st.fallocate(ino, offset, len, keep_size, now)
    }

    fn retain_node(&self, path: &str) -> Result<u64, FsError> {
        let comps = split_path(path)?;
        let mut st = self.state.lock();
        let ino = st.walk(&comps)?;
        let node = st.nodes.get_mut(&ino).ok_or(FsError::NotFound)?;
        node.opens = node.opens.checked_add(1).ok_or(FsError::Busy)?;
        Ok(ino)
    }

    fn retain_node_id(&self, node: u64) -> Result<(), FsError> {
        let mut st = self.state.lock();
        let node = st.nodes.get_mut(&node).ok_or(FsError::NotFound)?;
        node.opens = node.opens.checked_add(1).ok_or(FsError::Busy)?;
        Ok(())
    }

    fn release_node(&self, node: u64) {
        let mut st = self.state.lock();
        if let Some(n) = st.nodes.get_mut(&node) {
            n.opens = n.opens.saturating_sub(1);
        }
        st.free_if_orphan(node);
    }

    fn open_unnamed(&self, dir: &str, attrs: FsCreateAttrs) -> Result<u64, FsError> {
        let comps = split_path(dir)?;
        let now = self.now();
        let mut st = self.state.lock();
        let parent = st.walk(&comps)?;
        st.dir(parent)?;
        if st.nodes.len() as u64 >= st.max_inodes {
            return Err(FsError::NoSpace);
        }
        let ino = st.next_node;
        st.next_node += 1;
        st.nodes.insert(
            ino,
            Node {
                mode: attrs.mode & 0o7777,
                uid: attrs.uid,
                gid: attrs.gid,
                nlink: 0,
                atime: now,
                mtime: now,
                ctime: now,
                crtime: now,
                opens: 1,
                linkable: true,
                parent: 0,
                dir_name: String::new(),
                body: Body::File {
                    len: 0,
                    chunks: BTreeMap::new(),
                },
                shared: None,
            },
        );
        Ok(ino)
    }

    fn link_node(&self, node: u64, new_path: &str) -> Result<(), FsError> {
        let to = split_path(new_path)?;
        let now = self.now();
        let mut st = self.state.lock();
        link_node_locked(&mut st, node, &to, now)
    }

    fn metadata_node(&self, node: u64) -> Result<FsMetadata, FsError> {
        self.state.lock().metadata_of(node)
    }

    fn read_node_at(&self, node: u64, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        self.state.lock().read(node, offset, out)
    }

    fn write_node_at(&self, node: u64, offset: u64, data: &[u8]) -> Result<(usize, u64), FsError> {
        let now = self.now();
        self.state.lock().write(node, offset, data, now)
    }

    fn truncate_node(&self, node: u64, new_len: u64) -> Result<u64, FsError> {
        let now = self.now();
        self.state.lock().truncate(node, new_len, now)
    }

    fn fallocate_node(
        &self,
        node: u64,
        offset: u64,
        len: u64,
        keep_size: bool,
    ) -> Result<u64, FsError> {
        let now = self.now();
        self.state.lock().fallocate(node, offset, len, keep_size, now)
    }

    fn chmod_node(&self, node: u64, mode: u32) -> Result<(), FsError> {
        let now = self.now();
        let mut st = self.state.lock();
        let n = st.nodes.get_mut(&node).ok_or(FsError::NotFound)?;
        n.mode = mode & 0o7777;
        n.ctime = now;
        Ok(())
    }

    fn chown_node(&self, node: u64, uid: Option<u32>, gid: Option<u32>) -> Result<(), FsError> {
        let now = self.now();
        let mut st = self.state.lock();
        chown_locked(&mut st, node, uid, gid, now)
    }

    fn set_times_node(
        &self,
        node: u64,
        atime: Option<FsTimestamp>,
        mtime: Option<FsTimestamp>,
    ) -> Result<(), FsError> {
        let now = self.now();
        let mut st = self.state.lock();
        let n = st.nodes.get_mut(&node).ok_or(FsError::NotFound)?;
        if let Some(t) = atime {
            n.atime = t;
        }
        if let Some(t) = mtime {
            n.mtime = t;
        }
        n.ctime = now;
        Ok(())
    }

    fn shared_backing_node(&self, node: u64) -> Result<FsSharedBacking, FsError> {
        let factory = self.shared_frames.ok_or(FsError::Unsupported)?;
        self.state.lock().make_shared(node, factory)
    }

    fn flush(&self) -> Result<(), FsError> {
        // Memory-backed: every completed write is already as durable as the
        // filesystem gets.
        Ok(())
    }
}

/// Write into a shared-backed file. Quota is charged in whole pages for the
/// growth the write causes; a write that only partly fits is short, one that
/// cannot place a byte is `NoSpace` (the chunk path's contract).
fn shared_write(
    node: &mut Node,
    used_bytes: &mut u64,
    quota_bytes: u64,
    offset: u64,
    data: &[u8],
    now: FsTimestamp,
) -> Result<(usize, u64), FsError> {
    let Node { body, shared, .. } = node;
    let (Body::File { len, .. }, Some(frames)) = (body, shared.as_mut()) else {
        return Err(FsError::InvalidInput);
    };
    if data.is_empty() {
        return Ok((0, *len));
    }
    offset
        .checked_add(data.len() as u64)
        .filter(|end| *end <= TMPFS_MAX_FILE_SIZE)
        .ok_or(FsError::TooBig)?;
    let cur_pages = pages(*len);
    let spare_pages = quota_bytes.saturating_sub(*used_bytes) / CHUNK as u64;
    let max_end = cur_pages.saturating_add(spare_pages).saturating_mul(CHUNK as u64);
    let count = max_end.saturating_sub(offset).min(data.len() as u64);
    if count == 0 {
        return Err(FsError::NoSpace);
    }
    let old_len = *len;
    let new_len = old_len.max(offset + count);
    if new_len > old_len {
        frames.set_len(new_len)?;
    }
    let written = match frames.write_at(offset, &data[..count as usize]) {
        Ok(written) => written,
        Err(err) => {
            if new_len > old_len {
                // A failed write must not leave the file longer. Nothing maps
                // the extension yet, so the shrink back cannot be refused; if
                // it somehow is, the frames merely stay longer than the file.
                let _ = frames.set_len(old_len);
            }
            return Err(err);
        }
    };
    if new_len > old_len {
        *used_bytes += (pages(new_len) - cur_pages) * CHUNK as u64;
        *len = new_len;
    }
    node.touch_modify(now);
    let Body::File { len, .. } = &node.body else {
        return Err(FsError::InvalidInput);
    };
    Ok((written, *len))
}

/// `ftruncate` of a shared-backed file. The frames are resized first: a
/// refused shrink (`Busy`: still mapped) changes neither length nor quota.
fn shared_truncate(
    node: &mut Node,
    used_bytes: &mut u64,
    quota_bytes: u64,
    new_len: u64,
    now: FsTimestamp,
) -> Result<u64, FsError> {
    if new_len > TMPFS_MAX_FILE_SIZE {
        return Err(FsError::TooBig);
    }
    let Node { body, shared, .. } = node;
    let (Body::File { len, .. }, Some(frames)) = (body, shared.as_mut()) else {
        return Err(FsError::InvalidInput);
    };
    let (old_pages, new_pages) = (pages(*len), pages(new_len));
    if new_pages > old_pages
        && used_bytes.saturating_add((new_pages - old_pages) * CHUNK as u64) > quota_bytes
    {
        return Err(FsError::NoSpace);
    }
    frames.set_len(new_len)?;
    if new_pages >= old_pages {
        *used_bytes += (new_pages - old_pages) * CHUNK as u64;
    } else {
        *used_bytes = used_bytes.saturating_sub((old_pages - new_pages) * CHUNK as u64);
    }
    *len = new_len;
    node.touch_modify(now);
    Ok(new_len)
}

/// `fallocate` of a shared-backed file. Pages exist exactly up to EOF, so
/// extending the size allocates; `KEEP_SIZE` past EOF would need allocation
/// the frames cannot represent and is refused rather than faked.
fn shared_fallocate(
    node: &mut Node,
    used_bytes: &mut u64,
    quota_bytes: u64,
    end: u64,
    keep_size: bool,
    now: FsTimestamp,
) -> Result<u64, FsError> {
    let cur = match &node.body {
        Body::File { len, .. } => *len,
        _ => return Err(FsError::InvalidInput),
    };
    if end <= cur {
        return Ok(cur);
    }
    if keep_size {
        return Err(FsError::Unsupported);
    }
    shared_truncate(node, used_bytes, quota_bytes, end, now)
}

/// `chown(2)` on one inode. Like Linux, a successful change by anyone clears
/// `S_ISUID` and, when the group-execute bit is set, `S_ISGID` on a
/// non-directory (`setattr_should_drop_suidgid`).
fn chown_locked(
    st: &mut State,
    ino: u64,
    uid: Option<u32>,
    gid: Option<u32>,
    now: FsTimestamp,
) -> Result<(), FsError> {
    let node = st.nodes.get_mut(&ino).ok_or(FsError::NotFound)?;
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    if let Some(uid) = uid {
        node.uid = uid;
    }
    if let Some(gid) = gid {
        node.gid = gid;
    }
    if !matches!(node.body, Body::Dir(_)) {
        node.mode &= !0o4000;
        if node.mode & 0o010 != 0 {
            node.mode &= !0o2000;
        }
    }
    node.ctime = now;
    Ok(())
}

fn link_node_locked(
    st: &mut State,
    ino: u64,
    to: &[&str],
    now: FsTimestamp,
) -> Result<(), FsError> {
    let (parent, name) = st.parent_of(to)?;
    match st.nodes.get(&ino).ok_or(FsError::NotFound)?.body {
        // Hard links to directories are not allowed (EPERM).
        Body::Dir(_) => return Err(FsError::PermissionDenied),
        _ => {}
    }
    if st.dir(parent)?.contains_key(name) {
        return Err(FsError::AlreadyExists);
    }
    let cookie = st.next_dir_cookie()?;
    let node = st.nodes.get_mut(&ino).ok_or(FsError::NotFound)?;
    // A nameless inode may only receive its first name when it is an
    // `O_TMPFILE`; an ordinary file unlinked while open is gone (`ENOENT`).
    if node.nlink == 0 && !node.linkable {
        return Err(FsError::NotFound);
    }
    node.nlink = node.nlink.checked_add(1).ok_or(FsError::TooBig)?;
    node.linkable = false;
    node.ctime = now;
    st.dir_mut(parent)?.insert(String::from(name), DirLink { node: ino, cookie });
    st.nodes.get_mut(&parent).ok_or(FsError::NotFound)?.touch_modify(now);
    Ok(())
}

/// Shared-frame backing table for path-addressed filesystems (e.g. FAT) that do
/// not natively keep an in-memory inode per open file. While a file has an
/// active entry here, `read`, `write`, `truncate`, `O_TRUNC`, `stat`, and
/// `MAP_SHARED` all operate on the same [`SharedFrames`] object, and
/// `flush_entry` / `flush_by_object_id` write the frames back to the
/// volume.
pub struct SharedPathEntry {
    pub frames: Box<dyn SharedFrames>,
    pub rel_path: String,
    pub len: u64,
}

pub struct SharedPathStore<K: Ord + Clone> {
    entries: BTreeMap<K, SharedPathEntry>,
    mount_generation: u64,
}

impl<K: Ord + Clone> SharedPathStore<K> {
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            mount_generation: 0,
        }
    }

    pub fn sync_generation(&mut self, mount_generation: u64) {
        if self.mount_generation != mount_generation {
            self.entries.clear();
            self.mount_generation = mount_generation;
        }
    }

    pub fn len_of(&self, key: &K) -> Option<u64> {
        self.entries.get(key).map(|entry| entry.len)
    }

    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    pub fn ensure_shared(
        &mut self,
        key: K,
        rel_path: &str,
        volume: &dyn FsVolume,
        factory: SharedFramesFactory,
        writable: bool,
        mount_generation: u64,
    ) -> Result<FsSharedBacking, FsError> {
        self.sync_generation(mount_generation);
        let caps = volume.capabilities();
        if writable && caps.read_only {
            return Err(FsError::ReadOnly);
        }
        let meta = volume.metadata(rel_path)?;
        if meta.kind != FsNodeKind::File {
            return Err(FsError::IsADirectory);
        }
        if writable && (meta.dos_attrs & storage_core::fs::dos_attrs::READONLY != 0) {
            return Err(FsError::PermissionDenied);
        }
        if let Some(entry) = self.entries.get(&key) {
            let (token, object) = entry.frames.identity();
            return Ok(FsSharedBacking {
                token,
                object,
                len: entry.len,
            });
        }

        let mut frames = factory(meta.len)?;
        let mut buf = [0u8; CHUNK];
        let mut offset = 0u64;
        while offset < meta.len {
            let want = (meta.len - offset).min(CHUNK as u64) as usize;
            let n = volume.read_at(rel_path, offset, &mut buf[..want])?;
            if n == 0 || n > want {
                // Do not publish a partially initialized mapping as a
                // complete snapshot of the metadata's declared file size.
                return Err(FsError::Io(storage_core::StorageError::DeviceFault));
            }
            let mut copied = 0usize;
            while copied < n {
                let count = frames.write_at(offset + copied as u64, &buf[copied..n])?;
                if count == 0 || count > n - copied {
                    return Err(FsError::Io(storage_core::StorageError::DeviceFault));
                }
                copied += count;
            }
            offset += n as u64;
        }
        let (token, object) = frames.identity();
        self.entries.insert(
            key,
            SharedPathEntry {
                frames,
                rel_path: String::from(rel_path),
                len: meta.len,
            },
        );
        Ok(FsSharedBacking {
            token,
            object,
            len: meta.len,
        })
    }

    pub fn read_at(&self, key: &K, offset: u64, out: &mut [u8]) -> Option<Result<usize, FsError>> {
        let entry = self.entries.get(key)?;
        if out.is_empty() || offset >= entry.len {
            return Some(Ok(0));
        }
        let want = (entry.len - offset).min(out.len() as u64) as usize;
        Some(entry.frames.read_at(offset, &mut out[..want]))
    }

    pub fn write_at(
        &mut self,
        key: &K,
        offset: u64,
        data: &[u8],
    ) -> Option<Result<(usize, u64), FsError>> {
        let entry = self.entries.get_mut(key)?;
        // A zero-byte pwrite must not extend either the file or its memfd.
        if data.is_empty() {
            return Some(Ok((0, entry.len)));
        }
        let end = match offset.checked_add(data.len() as u64) {
            Some(end) if end <= u64::from(u32::MAX) => end,
            _ => return Some(Err(FsError::TooBig)),
        };
        if end > entry.len {
            if let Err(err) = entry.frames.set_len(end) {
                return Some(Err(err));
            }
        }
        let written = match entry.frames.write_at(offset, data) {
            Ok(n) => n,
            Err(err) => {
                if end > entry.len {
                    // Keep the frame length equal to the file length.
                    let _ = entry.frames.set_len(entry.len);
                }
                return Some(Err(err));
            }
        };
        entry.len = entry.len.max(offset + written as u64);
        Some(Ok((written, entry.len)))
    }

    pub fn truncate(&mut self, key: &K, new_len: u64) -> Option<Result<u64, FsError>> {
        let entry = self.entries.get_mut(key)?;
        if new_len > u64::from(u32::MAX) {
            return Some(Err(FsError::TooBig));
        }
        if let Err(err) = entry.frames.set_len(new_len) {
            return Some(Err(err));
        }
        entry.len = new_len;
        Some(Ok(new_len))
    }

    pub fn flush_entry(&mut self, key: &K, volume: &dyn FsVolume) -> Result<(), FsError> {
        let Some(entry) = self.entries.get(key) else {
            return Ok(());
        };
        flush_shared_entry(entry, volume)
    }

    pub fn flush_by_object_id(
        &mut self,
        object_id: u64,
        mut resolve_volume: impl FnMut(&K) -> Option<alloc::sync::Arc<dyn FsVolume>>,
    ) -> Result<(), FsError> {
        for (key, entry) in &self.entries {
            if object_id != 0 && entry.frames.identity().1 != object_id {
                continue;
            }
            if let Some(volume) = resolve_volume(key) {
                flush_shared_entry(entry, &*volume)?;
            }
            if object_id != 0 {
                break;
            }
        }
        Ok(())
    }

    pub fn remove(&mut self, key: &K) {
        self.entries.remove(key);
    }

}

impl SharedPathStore<(u64, String)> {
    pub fn rename(&mut self, old_key: &(u64, String), new_key: (u64, String), new_rel_path: String) {
        let mut sources = crate::mount_table::descendant_keys(&self.entries, old_key);
        if self.entries.contains_key(old_key) {
            sources.push(old_key.clone());
        }
        let mut moved = Vec::new();
        let old_components = old_key.1.split('/').count();
        for key in sources {
            let mut entry = self.entries.remove(&key).unwrap();
            let suffix = &key.1[old_key.1.len()..];
            let destination = (new_key.0, alloc::format!("{}{}", new_key.1, suffix));
            let relative_suffix = entry.rel_path.split('/').skip(old_components).collect::<Vec<_>>().join("/");
            entry.rel_path = if relative_suffix.is_empty() {
                new_rel_path.clone()
            } else {
                alloc::format!("{}/{}", new_rel_path, relative_suffix)
            };
            moved.push((destination, entry));
        }
        // A replaced file's old backing must not remain attached to its name.
        self.entries.remove(&new_key);
        for (key, entry) in moved {
            self.entries.insert(key, entry);
        }
    }
}

fn flush_shared_entry(entry: &SharedPathEntry, volume: &dyn FsVolume) -> Result<(), FsError> {
    if volume.capabilities().read_only {
        return Ok(());
    }
    let on_disk_len = volume.metadata(&entry.rel_path)?.len;
    if entry.len == 0 {
        if on_disk_len != 0 {
            volume.truncate(&entry.rel_path, 0)?;
        }
        volume.flush()?;
        return Ok(());
    }
    if on_disk_len > entry.len {
        volume.truncate(&entry.rel_path, entry.len)?;
    }
    let mut buf = [0u8; CHUNK];
    let mut offset = 0u64;
    while offset < entry.len {
        let want = (entry.len - offset).min(CHUNK as u64) as usize;
        let n = entry.frames.read_at(offset, &mut buf[..want])?;
        if n == 0 || n > want {
            // Unexpected EOF in a known-length frame object is not a
            // successful flush of the requested file contents.
            return Err(FsError::Io(storage_core::StorageError::DeviceFault));
        }
        let mut written = 0usize;
        while written < n {
            let (w, _) = volume.write_at(&entry.rel_path, offset + written as u64, &buf[written..n])?;
            if w == 0 {
                return Err(FsError::Io(storage_core::StorageError::WriteZero));
            }
            if w > n - written {
                return Err(FsError::Io(storage_core::StorageError::DeviceFault));
            }
            written += w;
        }
        offset += n as u64;
    }
    volume.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicI64, Ordering};

    static TICK: AtomicI64 = AtomicI64::new(1000);
    fn clock() -> FsTimestamp {
        FsTimestamp {
            secs: TICK.fetch_add(1, Ordering::Relaxed),
            nanos: 0,
        }
    }

    fn fs_with(quota: u64, inodes: u64) -> TmpFs {
        TmpFs::new(TmpFsConfig {
            quota_bytes: quota,
            max_inodes: inodes,
            root_mode: 0o1777,
            root_uid: 0,
            root_gid: 0,
            clock,
        })
    }

    fn fs() -> TmpFs {
        fs_with(1024 * 1024, 1024)
    }

    const A: FsCreateAttrs = FsCreateAttrs::new(0o600, 1000, 1000);
    const CREATE: FsOpenOptions = FsOpenOptions {
        create: true,
        create_new: false,
        truncate: false,
        write: true,
        append: false,
    };

    fn put(fs: &TmpFs, path: &str, bytes: &[u8]) -> u64 {
        let meta = fs.open_prepare_with(path, CREATE, A).unwrap();
        fs.write_at(path, 0, bytes).unwrap();
        meta.node_id
    }

    fn read_all(fs: &TmpFs, path: &str) -> Vec<u8> {
        let len = fs.metadata(path).unwrap().len as usize;
        let mut out = vec![0u8; len];
        assert_eq!(fs.read_at(path, 0, &mut out).unwrap(), len);
        out
    }

    #[test]
    fn root_metadata_and_capabilities() {
        let fs = fs();
        let root = fs.metadata("").unwrap();
        assert_eq!(root.kind, FsNodeKind::Directory);
        assert_eq!(root.mode, 0o1777);
        assert_eq!(root.nlink, 2);
        assert_eq!(fs.metadata("/").unwrap().node_id, root.node_id);
        let caps = fs.capabilities();
        assert_eq!(caps.case_policy, CasePolicy::Exact);
        assert!(caps.posix_nodes());
        assert!(!caps.read_only);
    }

    #[test]
    fn create_write_read_metadata_roundtrip() {
        let fs = fs();
        let meta = fs.open_prepare_with("a", CREATE, A).unwrap();
        assert_eq!(meta.kind, FsNodeKind::File);
        assert_eq!((meta.mode, meta.uid, meta.gid, meta.nlink), (0o600, 1000, 1000, 1));
        assert_eq!(fs.write_at("a", 0, b"hello").unwrap(), (5, 5));
        assert_eq!(fs.write_at("a", 3, b"LO!!").unwrap(), (4, 7));
        assert_eq!(read_all(&fs, "a"), b"helLO!!");
        let m2 = fs.metadata("a").unwrap();
        assert!(m2.mtime.secs > meta.mtime.secs);
        assert_eq!(m2.node_id, meta.node_id);
        let mut buf = [0u8; 4];
        assert_eq!(fs.read_at("a", 5, &mut buf).unwrap(), 2);
        assert_eq!(fs.read_at("a", 99, &mut buf).unwrap(), 0);
    }

    #[test]
    fn node_ids_are_unique_and_never_reused() {
        let fs = fs();
        let a = put(&fs, "a", b"1");
        fs.unlink("a").unwrap();
        let b = put(&fs, "b", b"2");
        assert_ne!(a, b);
        assert!(b > a);
        fs.mkdir("d").unwrap();
        let d = fs.metadata("d").unwrap().node_id;
        assert!(d > b);
    }

    #[test]
    fn open_flags_exclusive_and_missing() {
        let fs = fs();
        let no_create = FsOpenOptions { create: false, ..CREATE };
        assert_eq!(fs.open_prepare("a", no_create), Err(FsError::NotFound));
        fs.open_prepare("a", CREATE).unwrap();
        let excl = FsOpenOptions { create_new: true, ..CREATE };
        assert_eq!(fs.open_prepare("a", excl), Err(FsError::AlreadyExists));
        assert_eq!(fs.open_prepare("x/y", CREATE), Err(FsError::NotFound));
        fs.write_at("a", 0, b"abc").unwrap();
        fs.mkdir("d").unwrap();
        assert_eq!(fs.open_prepare("d", CREATE), Err(FsError::IsADirectory));
        let trunc = FsOpenOptions { truncate: true, create: false, ..CREATE };
        assert_eq!(fs.open_prepare("a", trunc).unwrap().len, 0);
        assert_eq!(fs.open_prepare("a/b", CREATE), Err(FsError::NotADirectory));
    }

    #[test]
    fn path_validation() {
        let fs = fs();
        assert_eq!(fs.metadata("a/../b"), Err(FsError::InvalidInput));
        let long = "x".repeat(256);
        assert_eq!(fs.mkdir(&long), Err(FsError::NameTooLong));
        assert_eq!(fs.mkdir("nul\0"), Err(FsError::InvalidInput));
        fs.mkdir(&"y".repeat(255)).unwrap();
        assert_eq!(fs.mkdir(""), Err(FsError::AlreadyExists));
        assert_eq!(fs.unlink(""), Err(FsError::IsADirectory));
    }

    #[test]
    fn mkdir_rmdir_nlink_and_errors() {
        let fs = fs();
        fs.mkdir_with("d", FsCreateAttrs::new(0o700, 7, 8)).unwrap();
        let d = fs.metadata("d").unwrap();
        assert_eq!((d.mode, d.uid, d.gid, d.nlink), (0o700, 7, 8, 2));
        assert_eq!(fs.mkdir("d"), Err(FsError::AlreadyExists));
        assert_eq!(fs.mkdir("nope/x"), Err(FsError::NotFound));
        assert_eq!(fs.metadata("").unwrap().nlink, 3);
        put(&fs, "d/f", b"x");
        assert_eq!(fs.rmdir("d"), Err(FsError::DirectoryNotEmpty));
        assert_eq!(fs.rmdir("d/f"), Err(FsError::NotADirectory));
        assert_eq!(fs.unlink("d"), Err(FsError::IsADirectory));
        fs.unlink("d/f").unwrap();
        fs.rmdir("d").unwrap();
        assert_eq!(fs.metadata("d"), Err(FsError::NotFound));
        assert_eq!(fs.metadata("").unwrap().nlink, 2);
        assert_eq!(fs.rmdir(""), Err(FsError::Busy));
        assert_eq!(fs.node_count(), 1);
    }

    #[test]
    fn read_dir_lists_kinds_in_cookie_order() {
        let fs = fs();
        fs.mkdir("d").unwrap();
        put(&fs, "b", b"12");
        fs.symlink("b", "l").unwrap();
        fs.mknod_socket("s", A).unwrap();
        let entries = fs.read_dir("").unwrap();
        let view: Vec<(&str, FsNodeKind)> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert_eq!(
            view,
            [
                ("d", FsNodeKind::Directory),
                ("b", FsNodeKind::File),
                ("l", FsNodeKind::Symlink),
                ("s", FsNodeKind::Socket)
            ]
        );
        assert_eq!(fs.read_dir("b"), Err(FsError::NotADirectory));
    }

    #[test]
    fn quota_charges_chunks_and_returns_nospace() {
        let fs = fs_with(3 * 4096, 64);
        put(&fs, "a", &[1u8; 4096]);
        assert_eq!(fs.used_bytes(), 4096);
        // Overwrite does not charge again.
        fs.write_at("a", 0, &[2u8; 100]).unwrap();
        assert_eq!(fs.used_bytes(), 4096);
        // Two more chunks fit, a third does not.
        assert_eq!(fs.write_at("a", 4096, &vec![3u8; 2 * 4096]).unwrap().0, 2 * 4096);
        assert_eq!(fs.used_bytes(), 3 * 4096);
        assert_eq!(fs.write_at("a", 3 * 4096, b"x"), Err(FsError::NoSpace));
        // Short write: only the part that fits is accepted.
        fs.truncate("a", 4096).unwrap();
        assert_eq!(fs.used_bytes(), 4096);
        let (n, len) = fs.write_at("a", 4096, &vec![9u8; 5 * 4096]).unwrap();
        assert_eq!((n, len), (2 * 4096, 3 * 4096));
        // Freed on unlink.
        fs.unlink("a").unwrap();
        assert_eq!(fs.used_bytes(), 0);
    }

    #[test]
    fn quota_boundary_is_exact_and_statfs_tracks_it() {
        let fs = fs_with(2 * 4096, 64);
        let st = fs.statfs().unwrap();
        assert_eq!(st.fs_type_magic, 0x0102_1994);
        assert_eq!((st.total_blocks, st.free_blocks, st.fs_name), (2, 2, "tmpfs"));
        assert!(st.case_sensitive && st.case_preserved);
        put(&fs, "a", &[7u8; 4096]);
        assert_eq!(fs.statfs().unwrap().free_blocks, 1);
        // One byte into the second chunk takes the whole chunk.
        fs.write_at("a", 4096, b"z").unwrap();
        let st = fs.statfs().unwrap();
        assert_eq!((st.free_blocks, st.avail_blocks), (0, 0));
        assert_eq!(fs.write_at("a", 8192, b"z"), Err(FsError::NoSpace));
        assert_eq!(st.total_inodes, 64);
        assert_eq!(st.free_inodes, 62);
    }

    #[test]
    fn sparse_truncate_growth_allocates_nothing_and_reads_zeros() {
        let fs = fs_with(4096, 64);
        put(&fs, "a", b"abc");
        // Growth far beyond the quota is sparse and charges nothing extra.
        assert_eq!(fs.truncate("a", 1 << 40).unwrap(), 1 << 40);
        assert_eq!(fs.used_bytes(), 4096);
        assert_eq!(fs.metadata("a").unwrap().len, 1 << 40);
        assert_eq!(fs.metadata("a").unwrap().allocation_size, 4096);
        let mut buf = [0xffu8; 8];
        assert_eq!(fs.read_at("a", 1 << 30, &mut buf).unwrap(), 8);
        assert_eq!(buf, [0u8; 8]);
        // Writing in the hole needs a fresh chunk, which the quota refuses.
        assert_eq!(fs.write_at("a", 1 << 30, b"x"), Err(FsError::NoSpace));
        assert_eq!(fs.truncate("a", TMPFS_MAX_FILE_SIZE + 1), Err(FsError::TooBig));
    }

    #[test]
    fn shrink_then_grow_reads_zeros_not_stale_bytes() {
        let fs = fs();
        put(&fs, "a", &[0xaau8; 6000]);
        fs.truncate("a", 100).unwrap();
        assert_eq!(fs.used_bytes(), 4096);
        fs.truncate("a", 6000).unwrap();
        let data = read_all(&fs, "a");
        assert!(data[..100].iter().all(|b| *b == 0xaa));
        assert!(data[100..].iter().all(|b| *b == 0));
        fs.truncate("a", 0).unwrap();
        assert_eq!(fs.used_bytes(), 0);
    }

    #[test]
    fn write_past_eof_zero_fills_gap() {
        let fs = fs();
        put(&fs, "a", b"ab");
        fs.write_at("a", 10_000, b"Z").unwrap();
        let data = read_all(&fs, "a");
        assert_eq!(data.len(), 10_001);
        assert_eq!(&data[..2], b"ab");
        assert!(data[2..10_000].iter().all(|b| *b == 0));
        assert_eq!(data[10_000], b'Z');
        assert_eq!(fs.write_at("a", TMPFS_MAX_FILE_SIZE, b"xy"), Err(FsError::TooBig));
        assert_eq!(fs.write_at("a", 0, b"").unwrap(), (0, 10_001));
    }

    #[test]
    fn same_size_truncate_releases_keep_size_quota_for_unlinked_open_file() {
        let fs = fs_with(8192, 16);
        fs.open_prepare("a", CREATE).unwrap();
        let node = fs.retain_node("a").unwrap();
        fs.fallocate_node(node, 4096, 4096, true).unwrap();
        assert_eq!(fs.used_bytes(), 4096);
        fs.unlink("a").unwrap();
        assert_eq!(fs.used_bytes(), 4096);
        fs.truncate_node(node, 0).unwrap();
        assert_eq!(fs.used_bytes(), 0);
        fs.release_node(node);
        assert_eq!(fs.used_bytes(), 0);
    }

    #[test]
    fn fallocate_charges_and_respects_keep_size() {
        let fs = fs_with(4 * 4096, 64);
        put(&fs, "a", b"x");
        assert_eq!(fs.fallocate("a", 0, 8192, false).unwrap(), 8192);
        assert_eq!(fs.used_bytes(), 2 * 4096);
        assert_eq!(fs.metadata("a").unwrap().len, 8192);
        // KEEP_SIZE allocates but leaves the length alone.
        assert_eq!(fs.fallocate("a", 8192, 4096, true).unwrap(), 8192);
        assert_eq!(fs.used_bytes(), 3 * 4096);
        assert_eq!(fs.metadata("a").unwrap().len, 8192);
        // Too big for the quota: nothing is allocated.
        assert_eq!(fs.fallocate("a", 0, 1 << 30, false), Err(FsError::NoSpace));
        assert_eq!(fs.used_bytes(), 3 * 4096);
        assert_eq!(fs.metadata("a").unwrap().len, 8192);
        assert_eq!(fs.fallocate("a", 0, 0, false), Err(FsError::InvalidInput));
        assert_eq!(fs.fallocate("a", u64::MAX, 2, false), Err(FsError::TooBig));
        fs.mkdir("d").unwrap();
        assert_eq!(fs.fallocate("d", 0, 1, false), Err(FsError::IsADirectory));
    }

    #[test]
    fn inode_limit_returns_nospace_and_recovers() {
        let fs = fs_with(1 << 20, 3);
        fs.mkdir("a").unwrap();
        fs.mkdir("b").unwrap();
        assert_eq!(fs.mkdir("c"), Err(FsError::NoSpace));
        assert_eq!(fs.open_prepare("f", CREATE), Err(FsError::NoSpace));
        assert_eq!(fs.symlink("t", "l"), Err(FsError::NoSpace));
        fs.rmdir("b").unwrap();
        fs.mkdir("c").unwrap();
    }

    #[test]
    fn rename_basic_replace_and_noreplace() {
        let fs = fs();
        let a = put(&fs, "a", b"AAA");
        put(&fs, "b", b"BBBBBB");
        assert_eq!(fs.rename("a", "b", false, true), Err(FsError::AlreadyExists));
        assert_eq!(fs.rename("a", "b", false, false), Err(FsError::AlreadyExists));
        let used = fs.used_bytes();
        fs.rename("a", "b", true, false).unwrap();
        assert_eq!(fs.metadata("a"), Err(FsError::NotFound));
        assert_eq!(fs.metadata("b").unwrap().node_id, a);
        assert_eq!(read_all(&fs, "b"), b"AAA");
        // The replaced file's chunk was credited.
        assert_eq!(fs.used_bytes(), used - 4096);
        fs.rename("b", "c", true, true).unwrap();
        assert_eq!(fs.rename("zz", "q", true, false), Err(FsError::NotFound));
        // Same path: no-op, but NOREPLACE still reports EEXIST like Linux.
        assert_eq!(fs.rename("c", "c", true, false), Ok(()));
        assert_eq!(fs.rename("c", "c", true, true), Err(FsError::AlreadyExists));
        assert_eq!(fs.rename("", "x", true, false), Err(FsError::Busy));
    }

    #[test]
    fn rename_type_mismatch_and_non_empty_dir() {
        let fs = fs();
        fs.mkdir("d").unwrap();
        fs.mkdir("e").unwrap();
        put(&fs, "e/x", b"1");
        put(&fs, "f", b"1");
        assert_eq!(fs.rename("d", "f", true, false), Err(FsError::NotADirectory));
        assert_eq!(fs.rename("f", "d", true, false), Err(FsError::IsADirectory));
        assert_eq!(fs.rename("d", "e", true, false), Err(FsError::DirectoryNotEmpty));
        fs.unlink("e/x").unwrap();
        fs.rename("d", "e", true, false).unwrap();
        assert_eq!(fs.metadata("d"), Err(FsError::NotFound));
        assert_eq!(fs.metadata("").unwrap().nlink, 3);
    }

    #[test]
    fn rename_directory_cycle_protection_and_nlink() {
        let fs = fs();
        fs.mkdir("a").unwrap();
        fs.mkdir("a/b").unwrap();
        fs.mkdir("a/b/c").unwrap();
        assert_eq!(fs.rename("a", "a/b/c/a2", true, false), Err(FsError::InvalidInput));
        assert_eq!(fs.rename("a", "a/x", true, false), Err(FsError::InvalidInput));
        assert_eq!(fs.rename("a/b", "a/b/c/b", true, false), Err(FsError::InvalidInput));
        // The tree is intact.
        assert!(fs.metadata("a/b/c").is_ok());
        // Legal move updates link counts and the cycle guard follows the move.
        fs.mkdir("t").unwrap();
        assert_eq!(fs.metadata("a").unwrap().nlink, 3);
        fs.rename("a/b", "t/b", true, false).unwrap();
        assert_eq!(fs.metadata("a").unwrap().nlink, 2);
        assert_eq!(fs.metadata("t").unwrap().nlink, 3);
        assert!(fs.metadata("t/b/c").is_ok());
        assert_eq!(fs.rename("t", "t/b/c/t", true, false), Err(FsError::InvalidInput));
        fs.rename("a", "t/b/c/a", true, false).unwrap();
        assert!(fs.metadata("t/b/c/a").is_ok());
    }

    #[test]
    fn rename_hardlink_to_self_is_noop_and_keeps_both_names() {
        let fs = fs();
        put(&fs, "a", b"x");
        fs.link("a", "b").unwrap();
        assert_eq!(fs.metadata("a").unwrap().nlink, 2);
        fs.rename("a", "b", true, false).unwrap();
        assert!(fs.metadata("a").is_ok() && fs.metadata("b").is_ok());
        assert_eq!(fs.rename("a", "b", true, true), Err(FsError::AlreadyExists));
    }

    #[test]
    fn hardlink_shares_data_and_frees_with_last_name() {
        let fs = fs();
        let ino = put(&fs, "a", b"data");
        fs.link("a", "b").unwrap();
        assert_eq!(fs.metadata("b").unwrap().node_id, ino);
        fs.write_at("b", 4, b"!").unwrap();
        assert_eq!(read_all(&fs, "a"), b"data!");
        fs.unlink("a").unwrap();
        assert_eq!(fs.metadata("b").unwrap().nlink, 1);
        assert_eq!(fs.used_bytes(), 4096);
        fs.unlink("b").unwrap();
        assert_eq!(fs.used_bytes(), 0);
        assert_eq!(fs.node_count(), 1);
        fs.mkdir("d").unwrap();
        assert_eq!(fs.link("d", "d2"), Err(FsError::PermissionDenied));
        put(&fs, "f", b"");
        assert_eq!(fs.link("f", "d"), Err(FsError::AlreadyExists));
        assert_eq!(fs.link("nope", "x"), Err(FsError::NotFound));
    }

    #[test]
    fn unlink_while_open_keeps_data_until_last_release() {
        let fs = fs();
        put(&fs, "a", b"persist");
        let node = fs.retain_node("a").unwrap();
        let node2 = fs.retain_node("a").unwrap();
        assert_eq!(node, node2);
        fs.unlink("a").unwrap();
        assert_eq!(fs.metadata("a"), Err(FsError::NotFound));
        // The name can be reused immediately with an unrelated node.
        let fresh = put(&fs, "a", b"new");
        assert_ne!(fresh, node);
        // The pinned node is still readable, writable and growable.
        let mut buf = [0u8; 16];
        assert_eq!(fs.read_node_at(node, 0, &mut buf).unwrap(), 7);
        assert_eq!(&buf[..7], b"persist");
        assert_eq!(fs.write_node_at(node, 7, b"ed").unwrap(), (2, 9));
        assert_eq!(fs.truncate_node(node, 100).unwrap(), 100);
        assert_eq!(fs.metadata_node(node).unwrap().nlink, 0);
        assert_eq!(read_all(&fs, "a"), b"new");
        let held = fs.used_bytes();
        assert_eq!(held, 2 * 4096);
        fs.release_node(node);
        // One pin left: still alive.
        assert!(fs.metadata_node(node).is_ok());
        assert_eq!(fs.used_bytes(), held);
        fs.release_node(node2);
        assert_eq!(fs.metadata_node(node), Err(FsError::NotFound));
        assert_eq!(fs.used_bytes(), 4096);
    }

    #[test]
    fn rename_over_open_target_keeps_replaced_data_alive() {
        let fs = fs();
        put(&fs, "src", b"S");
        put(&fs, "dst", b"DDD");
        let old = fs.retain_node("dst").unwrap();
        fs.rename("src", "dst", true, false).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(fs.read_node_at(old, 0, &mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"DDD");
        assert_eq!(read_all(&fs, "dst"), b"S");
        fs.release_node(old);
        assert_eq!(fs.metadata_node(old), Err(FsError::NotFound));
        assert_eq!(fs.used_bytes(), 4096);
    }

    #[test]
    fn pinned_directory_tracks_rename_and_not_replacement() {
        let fs = fs();
        fs.mkdir("a").unwrap();
        put(&fs, "a/original", b"x");
        let node = fs.retain_node("a").unwrap();
        fs.rename("a", "b", false, false).unwrap();
        fs.mkdir("a").unwrap();
        put(&fs, "a/replacement", b"y");
        assert_eq!(fs.directory_path_node(node).unwrap(), "b");
        assert_eq!(fs.read_dir_node(node).unwrap()[0].name, "original");
        fs.release_node(node);
    }

    #[test]
    fn retain_node_id_keeps_unlinked_inode_alive() {
        let fs = fs();
        fs.open_prepare("file", FsOpenOptions { create: true, write: true, ..FsOpenOptions::default() }).unwrap();
        let node = fs.retain_node("file").unwrap();
        fs.unlink("file").unwrap();
        fs.retain_node_id(node).unwrap();
        fs.release_node(node);
        assert!(fs.metadata_node(node).is_ok());
        fs.release_node(node);
        assert_eq!(fs.retain_node_id(node), Err(FsError::NotFound));
    }

    #[test]
    fn open_node_follows_rename_and_release_of_linked_node_keeps_it() {
        let fs = fs();
        put(&fs, "a", b"keep");
        let node = fs.retain_node("a").unwrap();
        fs.rename("a", "b", true, false).unwrap();
        assert_eq!(fs.metadata("b").unwrap().node_id, node);
        fs.release_node(node);
        assert_eq!(read_all(&fs, "b"), b"keep");
        // Spurious extra release never underflows or frees a linked node.
        fs.release_node(node);
        assert_eq!(read_all(&fs, "b"), b"keep");
    }

    #[test]
    fn unlinked_node_counts_against_inode_limit_until_released() {
        let fs = fs_with(1 << 20, 2);
        put(&fs, "a", b"x");
        let node = fs.retain_node("a").unwrap();
        fs.unlink("a").unwrap();
        assert_eq!(fs.open_prepare("b", CREATE), Err(FsError::NoSpace));
        fs.release_node(node);
        fs.open_prepare("b", CREATE).unwrap();
    }

    #[test]
    fn open_unnamed_and_link_node() {
        let fs = fs();
        fs.mkdir("d").unwrap();
        let node = fs.open_unnamed("d", A).unwrap();
        let meta = fs.metadata_node(node).unwrap();
        assert_eq!((meta.nlink, meta.mode, meta.uid, meta.kind), (0, 0o600, 1000, FsNodeKind::File));
        assert!(fs.read_dir("d").unwrap().is_empty());
        fs.write_node_at(node, 0, b"tmp").unwrap();
        fs.link_node(node, "d/named").unwrap();
        assert_eq!(fs.metadata("d/named").unwrap().nlink, 1);
        assert_eq!(read_all(&fs, "d/named"), b"tmp");
        fs.release_node(node);
        assert_eq!(read_all(&fs, "d/named"), b"tmp");
        // Never linked: freed on release.
        let lone = fs.open_unnamed("d", A).unwrap();
        fs.write_node_at(lone, 0, b"x").unwrap();
        let before = fs.used_bytes();
        fs.release_node(lone);
        assert_eq!(fs.used_bytes(), before - 4096);
        assert_eq!(fs.open_unnamed("d/named", A), Err(FsError::NotADirectory));
    }

    #[test]
    fn symlink_readlink_metadata_and_quota() {
        let fs = fs_with(1 << 20, 64);
        fs.symlink_with("../target/file", "l", FsCreateAttrs::new(0o600, 5, 6)).unwrap();
        assert_eq!(fs.readlink("l").unwrap(), "../target/file");
        let meta = fs.metadata("l").unwrap();
        assert_eq!(meta.kind, FsNodeKind::Symlink);
        assert_eq!(meta.len, 14);
        assert_eq!((meta.mode, meta.uid, meta.gid), (0o777, 5, 6));
        assert_eq!(fs.used_bytes(), 14);
        assert_eq!(fs.symlink("x", "l"), Err(FsError::AlreadyExists));
        assert_eq!(fs.symlink("", "e"), Err(FsError::NotFound));
        assert_eq!(fs.symlink(&"t".repeat(4096), "big"), Err(FsError::NameTooLong));
        put(&fs, "f", b"");
        assert_eq!(fs.readlink("f"), Err(FsError::InvalidInput));
        assert_eq!(fs.readlink("missing"), Err(FsError::NotFound));
        // A dangling link is a link; walking through it is not a directory walk.
        assert_eq!(fs.metadata("l/x"), Err(FsError::NotADirectory));
        fs.unlink("l").unwrap();
        assert_eq!(fs.used_bytes(), 0);
    }

    /// `O_NOFOLLOW` resolution leaves the final link in place; the volume then
    /// reports a symlink node, which vfsd's `open_posix_node` maps to `ELOOP`.
    /// An open with `O_TRUNC` must not touch the link or what it names.
    #[test]
    fn open_of_a_symlink_reports_the_link_and_never_truncates_its_target() {
        let fs = fs();
        put(&fs, "target", b"keep");
        fs.symlink("target", "l").unwrap();
        let opts = FsOpenOptions { create: false, create_new: false, truncate: true, write: true, append: false };
        let meta = fs.open_prepare_with("l", opts, A).unwrap();
        assert_eq!(meta.kind, FsNodeKind::Symlink);
        let mut buf = [0u8; 4];
        assert_eq!(fs.read_at("target", 0, &mut buf), Ok(4));
        assert_eq!(&buf, b"keep");
        // O_CREAT|O_EXCL on the link name collides instead of following it.
        let excl = FsOpenOptions { create: true, create_new: true, truncate: false, write: false, append: false };
        assert_eq!(fs.open_prepare_with("l", excl, A), Err(FsError::AlreadyExists));
    }

    #[test]
    fn symlink_loops_are_data_not_followed_by_volume() {
        let fs = fs();
        fs.symlink("b", "a").unwrap();
        fs.symlink("a", "b").unwrap();
        // The volume never chases links, so a cycle cannot hang it.
        assert_eq!(fs.readlink("a").unwrap(), "b");
        assert_eq!(fs.readlink("b").unwrap(), "a");
        assert_eq!(fs.metadata("a").unwrap().kind, FsNodeKind::Symlink);
        let mut buf = [0u8; 1];
        assert_eq!(fs.read_at("a", 0, &mut buf), Err(FsError::InvalidInput));
    }

    #[test]
    fn socket_nodes_exist_with_mode_and_block_data_io() {
        let fs = fs();
        fs.mknod_socket("sock", FsCreateAttrs::new(0o755, 1000, 1000)).unwrap();
        let meta = fs.metadata("sock").unwrap();
        assert_eq!(meta.kind, FsNodeKind::Socket);
        assert_eq!((meta.mode, meta.uid, meta.len), (0o755, 1000, 0));
        assert_eq!(fs.mknod_socket("sock", A), Err(FsError::AlreadyExists));
        assert_eq!(fs.mknod_socket("nodir/s", A), Err(FsError::NotFound));
        assert_eq!(fs.write_at("sock", 0, b"x"), Err(FsError::InvalidInput));
        assert_eq!(fs.truncate("sock", 0), Err(FsError::InvalidInput));
        // Re-binding requires unlink, exactly as AF_UNIX does.
        fs.unlink("sock").unwrap();
        fs.mknod_socket("sock", A).unwrap();
        // Opening for write is not offered by the node kind.
        let open = fs.open_prepare("sock", CREATE).unwrap();
        assert_eq!(open.kind, FsNodeKind::Socket);
    }

    #[test]
    fn fifo_nodes_exist_with_mode_and_block_data_io() {
        let fs = fs();
        fs.mknod_fifo("pipe", FsCreateAttrs::new(0o600, 1000, 1000)).unwrap();
        let meta = fs.metadata("pipe").unwrap();
        assert_eq!(meta.kind, FsNodeKind::Fifo);
        assert_eq!((meta.mode, meta.uid, meta.len), (0o600, 1000, 0));
        assert_eq!(fs.mknod_fifo("pipe", A), Err(FsError::AlreadyExists));
        assert_eq!(fs.mknod_fifo("nodir/p", A), Err(FsError::NotFound));
        assert_eq!(fs.write_at("pipe", 0, b"x"), Err(FsError::InvalidInput));
        assert_eq!(fs.truncate("pipe", 0), Err(FsError::InvalidInput));
        let open = fs.open_prepare("pipe", CREATE).unwrap();
        assert_eq!(open.kind, FsNodeKind::Fifo);
    }

    #[test]
    fn chmod_and_set_times_update_ctime() {
        let fs = fs();
        put(&fs, "a", b"x");
        let before = fs.metadata("a").unwrap();
        fs.chmod("a", 0o4755).unwrap();
        let after = fs.metadata("a").unwrap();
        assert_eq!(after.mode, 0o4755);
        assert!(after.ctime.secs > before.ctime.secs);
        assert_eq!(after.mtime, before.mtime);
        fs.chmod("a", 0o170000 | 0o644).unwrap();
        assert_eq!(fs.metadata("a").unwrap().mode, 0o644);
        let t = FsTimestamp { secs: 42, nanos: 7 };
        fs.set_times("a", Some(t), None, None).unwrap();
        let m = fs.metadata("a").unwrap();
        assert_eq!(m.atime, t);
        assert_eq!(m.mtime, before.mtime);
        fs.set_times("a", None, Some(t), None).unwrap();
        assert_eq!(fs.metadata("a").unwrap().mtime, t);
        assert_eq!(fs.chmod("zz", 0o600), Err(FsError::NotFound));
        // Node-addressed variants work on an unlinked file too.
        let node = fs.retain_node("a").unwrap();
        fs.unlink("a").unwrap();
        fs.chmod_node(node, 0o600).unwrap();
        fs.set_times_node(node, None, Some(FsTimestamp { secs: 9, nanos: 0 })).unwrap();
        let m = fs.metadata_node(node).unwrap();
        assert_eq!((m.mode, m.mtime.secs), (0o600, 9));
        fs.release_node(node);
    }

    #[test]
    fn timestamps_preserve_nanoseconds_for_path_and_unlinked_node() {
        fn precise_clock() -> FsTimestamp {
            FsTimestamp { secs: 1_700_000_000, nanos: 123_456_789 }
        }
        let fs = TmpFs::new(TmpFsConfig {
            quota_bytes: 4096,
            max_inodes: 16,
            root_mode: 0o1777,
            root_uid: 0,
            root_gid: 0,
            clock: precise_clock,
        });
        let created = fs.open_prepare_with("a", CREATE, A).unwrap();
        assert_eq!(created.mtime, precise_clock());
        let atime = FsTimestamp { secs: -1, nanos: 999_999_999 };
        let mtime = FsTimestamp { secs: 42, nanos: 987_654_321 };
        fs.set_times("a", Some(atime), Some(mtime), None).unwrap();
        let meta = fs.metadata("a").unwrap();
        assert_eq!((meta.atime, meta.mtime, meta.ctime), (atime, mtime, precise_clock()));
        let node = fs.retain_node("a").unwrap();
        fs.unlink("a").unwrap();
        fs.set_times_node(node, Some(mtime), Some(atime)).unwrap();
        let meta = fs.metadata_node(node).unwrap();
        assert_eq!((meta.atime, meta.mtime, meta.ctime), (mtime, atime, precise_clock()));
        fs.release_node(node);
    }

    #[test]
    fn directory_times_change_on_entry_mutation() {
        let fs = fs();
        fs.mkdir("d").unwrap();
        let t0 = fs.metadata("d").unwrap().mtime;
        put(&fs, "d/f", b"");
        let t1 = fs.metadata("d").unwrap().mtime;
        assert!(t1.secs > t0.secs);
        fs.unlink("d/f").unwrap();
        assert!(fs.metadata("d").unwrap().mtime.secs > t1.secs);
    }

    #[test]
    fn open_directory_rules() {
        let fs = fs();
        fs.mkdir("d").unwrap();
        let ro = FsOpenOptions::default();
        assert_eq!(fs.open_prepare("d", ro).unwrap().kind, FsNodeKind::Directory);
        assert_eq!(fs.open_prepare("", ro).unwrap().kind, FsNodeKind::Directory);
        let wr = FsOpenOptions { write: true, ..ro };
        assert_eq!(fs.open_prepare("d", wr), Err(FsError::IsADirectory));
        assert_eq!(fs.read_at("d", 0, &mut [0u8; 1]), Err(FsError::IsADirectory));
        assert_eq!(fs.write_at("d", 0, b"x"), Err(FsError::IsADirectory));
        assert_eq!(fs.truncate("d", 0), Err(FsError::IsADirectory));
    }

    #[test]
    fn canonical_key_and_flush() {
        let fs = fs();
        assert_eq!(fs.canonical_key("/a//b/").unwrap(), "a/b");
        assert!(!fs.is_immutable());
        assert_eq!(fs.flush(), Ok(()));
    }

    // ---- shared backing -------------------------------------------------

    /// Stand-in for the memfd: `data` is the frame contents, `mapped_len` the
    /// extent a kernel mapping pins (a shrink below it is refused like the
    /// real memfd's), `closed` is set when the owning node drops it.
    struct FakeInner {
        data: Vec<u8>,
        mapped_len: usize,
        closed: bool,
    }
    type Fake = alloc::sync::Arc<Mutex<FakeInner>>;
    static FAKES: Mutex<Vec<(u64, Fake)>> = Mutex::new(Vec::new());
    static NEXT_OBJECT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(100);

    struct FakeFrames {
        object: u64,
        inner: Fake,
    }
    impl SharedFrames for FakeFrames {
        fn identity(&self) -> (u64, u64) {
            (7, self.object)
        }
        fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
            let g = self.inner.lock();
            let start = (offset as usize).min(g.data.len());
            let n = out.len().min(g.data.len() - start);
            out[..n].copy_from_slice(&g.data[start..start + n]);
            Ok(n)
        }
        fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<usize, FsError> {
            let mut g = self.inner.lock();
            let end = offset as usize + data.len();
            if end > g.data.len() {
                g.data.resize(end, 0);
            }
            g.data[offset as usize..end].copy_from_slice(data);
            Ok(data.len())
        }
        fn set_len(&mut self, len: u64) -> Result<(), FsError> {
            let mut g = self.inner.lock();
            if (len as usize) < g.mapped_len {
                return Err(FsError::Busy);
            }
            g.data.resize(len as usize, 0);
            Ok(())
        }
    }
    impl Drop for FakeFrames {
        fn drop(&mut self) {
            self.inner.lock().closed = true;
        }
    }
    fn fake_factory(len: u64) -> Result<Box<dyn SharedFrames>, FsError> {
        let object = NEXT_OBJECT.fetch_add(1, Ordering::Relaxed);
        let inner: Fake = alloc::sync::Arc::new(Mutex::new(FakeInner {
            data: vec![0; len as usize],
            mapped_len: 0,
            closed: false,
        }));
        FAKES.lock().push((object, inner.clone()));
        Ok(Box::new(FakeFrames { object, inner }))
    }
    // Exercise the contract that a frame writer can make partial progress.
    struct ShortWriteFrames(Box<dyn SharedFrames>);
    impl SharedFrames for ShortWriteFrames {
        fn identity(&self) -> (u64, u64) { self.0.identity() }
        fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
            self.0.read_at(offset, out)
        }
        fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<usize, FsError> {
            self.0.write_at(offset, &data[..data.len().min(2)])
        }
        fn set_len(&mut self, len: u64) -> Result<(), FsError> { self.0.set_len(len) }
    }
    fn short_frame_write_factory(len: u64) -> Result<Box<dyn SharedFrames>, FsError> {
        Ok(Box::new(ShortWriteFrames(fake_factory(len)?)))
    }
    fn fake(object: u64) -> Fake {
        FAKES.lock().iter().find(|(id, _)| *id == object).unwrap().1.clone()
    }
    fn shared_fs(quota: u64) -> TmpFs {
        fs_with(quota, 64).with_shared_frames(fake_factory)
    }

    #[test]
    fn shared_backing_moves_chunks_and_keeps_every_byte_and_the_quota_exact() {
        let fs = shared_fs(64 * 4096);
        let node = put(&fs, "f", &[7u8; 5000]);
        assert_eq!(fs.used_bytes(), 2 * 4096);
        let backing = fs.shared_backing_node(node).unwrap();
        assert_eq!((backing.token, backing.len), (7, 5000));
        let frames = fake(backing.object);
        assert_eq!(frames.lock().data, vec![7u8; 5000]);
        assert_eq!(fs.used_bytes(), 2 * 4096, "dense charge equals the old chunk charge");
        assert_eq!(fs.metadata("f").unwrap().allocation_size, 2 * 4096);
        assert_eq!(fs.shared_backing_node(node).unwrap(), backing, "idempotent");
        assert_eq!(read_all(&fs, "f"), vec![7u8; 5000]);
    }

    #[test]
    fn shared_backing_is_coherent_in_both_directions() {
        let fs = shared_fs(64 * 4096);
        let node = put(&fs, "f", b"abcdef");
        let backing = fs.shared_backing_node(node).unwrap();
        let frames = fake(backing.object);
        // A write through the filesystem lands in the frames a mapping reads.
        fs.write_node_at(node, 2, b"ZZ").unwrap();
        assert_eq!(&frames.lock().data[..], b"abZZef");
        // A store through a mapping is what pread sees, with no syscall.
        frames.lock().data[0] = b'M';
        assert_eq!(read_all(&fs, "f"), b"MbZZef");
        // Growth by write extends the frames and the file together.
        let (n, len) = fs.write_node_at(node, 6000, b"tail").unwrap();
        assert_eq!((n, len), (4, 6004));
        assert_eq!(frames.lock().data.len(), 6004);
        assert_eq!(fs.used_bytes(), 2 * 4096);
        let mut gap = [1u8; 8];
        assert_eq!(fs.read_node_at(node, 100, &mut gap).unwrap(), 8);
        assert_eq!(gap, [0u8; 8], "the hole between EOF and the write reads zeros");
    }

    #[test]
    fn shared_backing_requires_a_frame_source_and_a_regular_file() {
        let plain = fs();
        let node = put(&plain, "f", b"x");
        assert_eq!(plain.shared_backing_node(node), Err(FsError::Unsupported));
        let fs = shared_fs(64 * 4096);
        fs.mkdir("d").unwrap();
        let dir = fs.metadata("d").unwrap().node_id;
        assert_eq!(fs.shared_backing_node(dir), Err(FsError::IsADirectory));
        assert_eq!(fs.shared_backing_node(9999), Err(FsError::NotFound));
    }

    #[test]
    fn failed_upgrade_changes_nothing() {
        fn fail_factory(_: u64) -> Result<Box<dyn SharedFrames>, FsError> { Err(FsError::NoSpace) }
        let fs = fs_with(64 * 4096, 64).with_shared_frames(fail_factory);
        let node = put(&fs, "f", &[3u8; 9000]);
        let before = fs.used_bytes();
        let failed = fs.shared_backing_node(node);
        assert_eq!(failed, Err(FsError::NoSpace));
        assert_eq!(fs.used_bytes(), before);
        assert_eq!(read_all(&fs, "f"), vec![3u8; 9000]);
        assert_eq!(fs.shared_backing_node(node), Err(FsError::NoSpace));
    }

    #[test]
    fn shared_file_is_dense_and_quota_bound() {
        let fs = shared_fs(4 * 4096);
        let node = put(&fs, "f", b"x");
        fs.shared_backing_node(node).unwrap();
        assert_eq!(fs.used_bytes(), 4096);
        // Sparse growth is charged page for page, up to the quota.
        assert_eq!(fs.truncate_node(node, 4 * 4096).unwrap(), 4 * 4096);
        assert_eq!(fs.used_bytes(), 4 * 4096);
        assert_eq!(fs.truncate_node(node, 4 * 4096 + 1), Err(FsError::NoSpace));
        assert_eq!(fs.metadata("f").unwrap().len, 4 * 4096, "refused growth changed nothing");
        // A write that only partly fits is short; one that cannot place a byte is ENOSPC.
        let (n, len) = fs.write_node_at(node, 4 * 4096 - 2, b"abcd").unwrap();
        assert_eq!((n, len), (2, 4 * 4096));
        assert_eq!(fs.write_node_at(node, 4 * 4096, b"z"), Err(FsError::NoSpace));
    }

    #[test]
    fn shrinking_below_a_mapped_extent_is_busy_and_changes_nothing() {
        let fs = shared_fs(64 * 4096);
        let node = put(&fs, "f", &[9u8; 3 * 4096]);
        let backing = fs.shared_backing_node(node).unwrap();
        let frames = fake(backing.object);
        frames.lock().mapped_len = 2 * 4096;
        assert_eq!(fs.truncate_node(node, 4096), Err(FsError::Busy));
        assert_eq!(fs.metadata("f").unwrap().len, 3 * 4096);
        assert_eq!(fs.used_bytes(), 3 * 4096);
        assert_eq!(frames.lock().data.len(), 3 * 4096);
        // Shrinking to a size that keeps the mapped extent is allowed.
        assert_eq!(fs.truncate_node(node, 2 * 4096).unwrap(), 2 * 4096);
        assert_eq!(fs.used_bytes(), 2 * 4096);
        // Once unmapped the shrink completes and regrowth reads zeros.
        frames.lock().mapped_len = 0;
        fs.truncate_node(node, 10).unwrap();
        assert_eq!(fs.used_bytes(), 4096);
        fs.truncate_node(node, 100).unwrap();
        let mut out = [1u8; 100];
        assert_eq!(fs.read_node_at(node, 0, &mut out).unwrap(), 100);
        assert_eq!(&out[..10], &[9u8; 10]);
        assert_eq!(&out[10..], &[0u8; 90][..], "bytes past the old EOF are zero");
    }

    #[test]
    fn shared_frames_live_until_the_last_pin_and_name_are_gone() {
        let fs = shared_fs(64 * 4096);
        let node = put(&fs, "f", &[1u8; 100]);
        fs.retain_node_id(node).unwrap();
        let backing = fs.shared_backing_node(node).unwrap();
        let frames = fake(backing.object);
        fs.unlink("f").unwrap();
        assert!(!frames.lock().closed, "an open description keeps the frames");
        assert_eq!(fs.used_bytes(), 4096);
        let mut out = [0u8; 100];
        assert_eq!(fs.read_node_at(node, 0, &mut out).unwrap(), 100);
        fs.release_node(node);
        assert!(frames.lock().closed, "the last release drops the frames");
        assert_eq!(fs.used_bytes(), 0);
        assert_eq!(fs.node_count(), 1);
    }

    #[test]
    fn shared_fallocate_extends_but_refuses_unrepresentable_keep_size() {
        let fs = shared_fs(8 * 4096);
        let node = put(&fs, "f", b"x");
        fs.shared_backing_node(node).unwrap();
        assert_eq!(fs.fallocate_node(node, 0, 1, true).unwrap(), 1, "inside EOF: already allocated");
        assert_eq!(fs.fallocate_node(node, 0, 8192, true), Err(FsError::Unsupported));
        assert_eq!(fs.fallocate_node(node, 0, 8192, false).unwrap(), 8192);
        assert_eq!(fs.used_bytes(), 2 * 4096);
        assert_eq!(fs.fallocate_node(node, 0, 9 * 4096, false), Err(FsError::NoSpace));
    }

    #[test]
    fn o_trunc_of_a_shared_file_releases_pages_when_unmapped() {
        let fs = shared_fs(64 * 4096);
        let node = put(&fs, "f", &[5u8; 3 * 4096]);
        fs.shared_backing_node(node).unwrap();
        let opts = FsOpenOptions { truncate: true, ..CREATE };
        assert_eq!(fs.open_prepare("f", opts).unwrap().len, 0);
        assert_eq!(fs.used_bytes(), 0);
    }


    #[test]
    fn mixed_stress_never_leaks_quota_or_nodes() {
        let fs = fs_with(64 * 4096, 256);
        for round in 0..20u32 {
            for i in 0..8u32 {
                let name = alloc::format!("f{i}");
                let (written, _) = {
                    fs.open_prepare(&name, CREATE).unwrap();
                    fs.write_at(&name, u64::from(i) * 100, &vec![round as u8; 5000]).unwrap()
                };
                assert_eq!(written, 5000);
                if i % 2 == 0 {
                    fs.rename(&name, &alloc::format!("g{i}"), true, false).unwrap();
                }
            }
            for i in 0..8u32 {
                let _ = fs.unlink(&alloc::format!("f{i}"));
                let _ = fs.unlink(&alloc::format!("g{i}"));
            }
        }
        assert_eq!(fs.used_bytes(), 0);
        assert_eq!(fs.node_count(), 1);
    }

    #[test]
    fn shared_path_store_read_write_truncate_and_flush_are_coherent() {
        let vol = alloc::sync::Arc::new(fs());
        put(&vol, "fat_file", b"initial-data");
        let mut store: SharedPathStore<(u64, String)> = SharedPathStore::new();
        let key = (1u64, String::from("fat_file"));

        let backing = store
            .ensure_shared(key.clone(), "fat_file", &*vol, fake_factory, true, 1)
            .unwrap();
        assert_eq!(backing.len, 12);
        // Idempotent second call returns the same backing identity.
        assert_eq!(
            store
                .ensure_shared(key.clone(), "fat_file", &*vol, fake_factory, true, 1)
                .unwrap(),
            backing
        );

        let mut buf = [0u8; 32];
        assert_eq!(store.read_at(&key, 0, &mut buf), Some(Ok(12)));
        assert_eq!(&buf[..12], b"initial-data");

        // Write extends the shared backing without touching the volume yet.
        assert_eq!(store.write_at(&key, 8, b"BYTES!"), Some(Ok((6, 14))));
        assert_eq!(store.len_of(&key), Some(14));
        assert_eq!(read_all(&vol, "fat_file"), b"initial-data");

        // Flush by object id (msync path) persists the shared bytes to the volume.
        store
            .flush_by_object_id(backing.object, |_| Some(vol.clone()))
            .unwrap();
        assert_eq!(read_all(&vol, "fat_file"), b"initial-BYTES!");

        // Truncate shrink + flush_entry (fsync/close path) persists the shorter length.
        assert_eq!(store.truncate(&key, 7), Some(Ok(7)));
        store.flush_entry(&key, &*vol).unwrap();
        assert_eq!(read_all(&vol, "fat_file"), b"initial");

        // Truncate to zero + flush_entry.
        assert_eq!(store.truncate(&key, 0), Some(Ok(0)));
        store.flush_entry(&key, &*vol).unwrap();
        assert_eq!(read_all(&vol, "fat_file"), b"");
    }

    #[test]
    fn shared_path_store_empty_write_does_not_extend_file_or_frames() {
        let vol = alloc::sync::Arc::new(fs());
        put(&vol, "file", b"hello");
        let mut store: SharedPathStore<(u64, String)> = SharedPathStore::new();
        let key = (1, String::from("file"));
        let backing = store.ensure_shared(key.clone(), "file", &*vol, fake_factory, true, 1).unwrap();
        assert_eq!(store.write_at(&key, 8192, b""), Some(Ok((0, 5))));
        assert_eq!(store.write_at(&key, u64::MAX, b""), Some(Ok((0, 5))));
        assert_eq!(store.len_of(&key), Some(5));
        assert_eq!(fake(backing.object).lock().data, b"hello".to_vec());
        store.flush_entry(&key, &*vol).unwrap();
        assert_eq!(read_all(&vol, "file"), b"hello");
    }

    #[test]
    fn shared_path_store_retries_short_frame_writes_before_publication() {
        let vol = alloc::sync::Arc::new(fs());
        put(&vol, "file", b"0123456789abcdef");
        let mut store: SharedPathStore<(u64, String)> = SharedPathStore::new();
        let key = (1, String::from("file"));
        let backing = store.ensure_shared(key, "file", &*vol, short_frame_write_factory, true, 1).unwrap();
        assert_eq!(fake(backing.object).lock().data, b"0123456789abcdef".to_vec());
    }

    #[test]
    fn shared_path_store_flush_rejects_premature_frame_eof() {
        let vol = alloc::sync::Arc::new(fs());
        put(&vol, "file", b"durable-data");
        let mut store: SharedPathStore<(u64, String)> = SharedPathStore::new();
        let key = (1, String::from("file"));
        let backing = store.ensure_shared(key.clone(), "file", &*vol, fake_factory, true, 1).unwrap();
        fake(backing.object).lock().data.clear();
        assert!(matches!(
            store.flush_entry(&key, &*vol),
            Err(FsError::Io(storage_core::StorageError::DeviceFault))
        ));
        assert_eq!(read_all(&vol, "file"), b"durable-data");
    }

    #[test]
    fn shared_path_store_tracks_rename_remove_and_generation() {
        let vol = alloc::sync::Arc::new(fs());
        put(&vol, "old", b"hello");
        let mut store: SharedPathStore<(u64, String)> = SharedPathStore::new();
        let old_key = (1u64, String::from("old"));
        let new_key = (1u64, String::from("new"));

        store
            .ensure_shared(old_key.clone(), "old", &*vol, fake_factory, true, 1)
            .unwrap();
        vol.rename("old", "new", true, false).unwrap();
        store.rename(&old_key, new_key.clone(), String::from("new"));
        assert!(!store.contains(&old_key));
        assert!(store.contains(&new_key));

        store.write_at(&new_key, 5, b"-world").unwrap().unwrap();
        store.flush_entry(&new_key, &*vol).unwrap();
        assert_eq!(read_all(&vol, "new"), b"hello-world");

        // Generation bump invalidates stale entries.
        store.sync_generation(2);
        assert!(!store.contains(&new_key));
    }

    #[test]
    fn shared_path_store_directory_rename_preserves_descendant_backing() {
        let vol = alloc::sync::Arc::new(fs());
        vol.mkdir("Old").unwrap();
        vol.mkdir("Old/sub").unwrap();
        vol.mkdir("Oldish").unwrap();
        put(&vol, "Old/sub/file", b"original");
        put(&vol, "Oldish/file", b"other");
        let mut store = SharedPathStore::new();
        let old = (1, String::from("old/sub/file"));
        let other = (1, String::from("oldish/file"));
        let backing = store.ensure_shared(old.clone(), "Old/sub/file", &*vol, fake_factory, true, 1).unwrap();
        store.ensure_shared(other.clone(), "Oldish/file", &*vol, fake_factory, true, 1).unwrap();
        store.write_at(&old, 0, b"changed!").unwrap().unwrap();
        vol.rename("Old", "New", true, false).unwrap();
        store.rename(&(1, String::from("old")), (1, String::from("new")), String::from("New"));
        let new = (1, String::from("new/sub/file"));
        assert!(!store.contains(&old));
        assert!(store.contains(&other));
        assert_eq!(store.ensure_shared(new.clone(), "New/sub/file", &*vol, fake_factory, true, 1).unwrap(), backing);
        store.flush_by_object_id(backing.object, |_| Some(vol.clone())).unwrap();
        assert_eq!(read_all(&vol, "New/sub/file"), b"changed!");
        assert_eq!(read_all(&vol, "Oldish/file"), b"other");
    }

    #[test]
    fn shared_path_store_rename_overwrites_destination_and_handles_same_key() {
        let vol = fs();
        put(&vol, "old", b"source");
        put(&vol, "new", b"target");
        let mut store = SharedPathStore::new();
        let old = (1, String::from("old"));
        let new = (1, String::from("new"));
        let source = store.ensure_shared(old.clone(), "old", &vol, fake_factory, true, 1).unwrap();
        store.ensure_shared(new.clone(), "new", &vol, fake_factory, true, 1).unwrap();
        vol.rename("old", "new", true, false).unwrap();
        store.rename(&old, new.clone(), String::from("new"));
        assert_eq!(store.ensure_shared(new.clone(), "new", &vol, fake_factory, true, 1).unwrap(), source);
        store.rename(&new, new.clone(), String::from("new"));
        assert_eq!(store.ensure_shared(new, "new", &vol, fake_factory, true, 1).unwrap(), source);
    }

    #[test]
    fn directory_cookies_follow_names_without_renumbering_survivors() {
        let fs = fs();
        fs.mkdir("dir").unwrap();
        fs.mkdir("other").unwrap();
        put(&fs, "dir/z", b"1");
        let inode = put(&fs, "dir/a", b"2");
        fs.link_node(inode, "dir/hardlink").unwrap();
        let original = fs.read_dir("dir").unwrap();
        assert_eq!(original.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["z", "a", "hardlink"]);
        assert_ne!(original[1].cookie, original[2].cookie);
        assert_eq!(original[1].node_id, original[2].node_id);
        fs.unlink("dir/z").unwrap();
        fs.rename("dir/a", "dir/b", false, false).unwrap();
        fs.rename("dir/hardlink", "other/moved", false, false).unwrap();
        let resumed = fs.read_dir("dir").unwrap();
        assert_eq!(resumed[0].name, "b");
        assert_eq!(resumed[0].cookie, original[1].cookie);
        assert!(fs.read_dir("other").unwrap()[0].cookie > original[2].cookie);
    }

    #[test]
    fn directory_path_follows_nested_renames_and_dies_with_rmdir() {
        let fs = fs();
        fs.mkdir("a").unwrap();
        fs.mkdir("a/b").unwrap();
        fs.mkdir("a/b/c").unwrap();
        let c = fs.retain_node("a/b/c").unwrap();
        let b = fs.retain_node("a/b").unwrap();
        assert_eq!(fs.directory_path_node(c).unwrap(), "a/b/c");
        assert_eq!(fs.directory_path_node(ROOT_NODE).unwrap(), "");
        // Renaming an ancestor and the directory itself both show in the path.
        fs.mkdir("t").unwrap();
        fs.rename("a/b", "t/bb", true, false).unwrap();
        assert_eq!(fs.directory_path_node(c).unwrap(), "t/bb/c");
        fs.rename("t/bb/c", "t/cc", true, false).unwrap();
        assert_eq!(fs.directory_path_node(c).unwrap(), "t/cc");
        // A file node has no directory path.
        let f = put(&fs, "t/f", b"x");
        assert_eq!(fs.directory_path_node(f), Err(FsError::NotADirectory));
        // A removed directory has none, even when its name is reused.
        fs.rmdir("t/cc").unwrap();
        fs.mkdir("t/cc").unwrap();
        assert_eq!(fs.directory_path_node(c), Err(FsError::NotFound));
        assert_eq!(fs.directory_path_node(b).unwrap(), "t/bb");
        fs.release_node(c);
        fs.release_node(b);
    }

    #[test]
    fn create_flag_on_an_existing_directory_is_eisdir_even_read_only() {
        let fs = fs();
        fs.mkdir("d").unwrap();
        let ro_create = FsOpenOptions { create: true, ..FsOpenOptions::default() };
        assert_eq!(fs.open_prepare("d", ro_create), Err(FsError::IsADirectory));
        assert_eq!(fs.open_prepare("", ro_create), Err(FsError::IsADirectory));
        // Without O_CREAT a read-only directory open is still fine.
        assert!(fs.open_prepare("d", FsOpenOptions::default()).is_ok());
    }

    #[test]
    fn truncate_to_chunk_boundary_frees_exactly_the_tail() {
        let fs = fs();
        put(&fs, "a", &[1u8; 3 * 4096]);
        assert_eq!(fs.used_bytes(), 3 * 4096);
        fs.truncate("a", 4096).unwrap();
        assert_eq!(fs.used_bytes(), 4096);
        fs.truncate("a", 4095).unwrap();
        assert_eq!(fs.used_bytes(), 4096);
        let data = read_all(&fs, "a");
        assert!(data.iter().all(|b| *b == 1));
        fs.truncate("a", 8192).unwrap();
        let data = read_all(&fs, "a");
        assert!(data[4095..].iter().all(|b| *b == 0), "regrowth reads zeros");
    }

    static FAIL_OBJECT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    static FLAKY_TEST_LOCK: Mutex<()> = Mutex::new(());
    struct FlakyFrames(Box<dyn SharedFrames>);
    impl SharedFrames for FlakyFrames {
        fn identity(&self) -> (u64, u64) { self.0.identity() }
        fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
            self.0.read_at(offset, out)
        }
        fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<usize, FsError> {
            if self.0.identity().1 == FAIL_OBJECT.load(Ordering::Relaxed) {
                return Err(FsError::Io(storage_core::StorageError::DeviceFault));
            }
            self.0.write_at(offset, data)
        }
        fn set_len(&mut self, len: u64) -> Result<(), FsError> { self.0.set_len(len) }
    }
    fn flaky_factory(len: u64) -> Result<Box<dyn SharedFrames>, FsError> {
        Ok(Box::new(FlakyFrames(fake_factory(len)?)))
    }

    #[test]
    fn failed_shared_write_does_not_grow_the_file_or_leak_quota() {
        let _guard = FLAKY_TEST_LOCK.lock();
        let fs = fs_with(64 * 4096, 64).with_shared_frames(flaky_factory);
        let node = put(&fs, "f", b"abc");
        let backing = fs.shared_backing_node(node).unwrap();
        let frames = fake(backing.object);
        let used = fs.used_bytes();
        FAIL_OBJECT.store(backing.object, Ordering::Relaxed);
        let failed = fs.write_node_at(node, 3 * 4096, b"zz");
        FAIL_OBJECT.store(0, Ordering::Relaxed);
        assert!(matches!(failed, Err(FsError::Io(_))));
        assert_eq!(fs.metadata("f").unwrap().len, 3);
        assert_eq!(fs.used_bytes(), used);
        assert_eq!(frames.lock().data.len(), 3, "frame length tracks file length");
        assert_eq!(fs.write_node_at(node, 3, b"d").unwrap(), (1, 4));
    }

    #[test]
    fn failed_shared_path_write_keeps_frame_and_file_length_equal() {
        let _guard = FLAKY_TEST_LOCK.lock();
        let vol = alloc::sync::Arc::new(fs());
        put(&vol, "file", b"hello");
        let mut store: SharedPathStore<(u64, String)> = SharedPathStore::new();
        let key = (1, String::from("file"));
        let backing = store.ensure_shared(key.clone(), "file", &*vol, flaky_factory, true, 1).unwrap();
        FAIL_OBJECT.store(backing.object, Ordering::Relaxed);
        let failed = store.write_at(&key, 100, b"zz");
        FAIL_OBJECT.store(0, Ordering::Relaxed);
        assert!(matches!(failed, Some(Err(FsError::Io(_)))));
        assert_eq!(store.len_of(&key), Some(5));
        assert_eq!(fake(backing.object).lock().data.len(), 5);
    }

    #[test]
    fn shared_path_flush_entry_persists_frames_dirtied_outside_the_store() {
        // MAP_SHARED stores hit the frames directly and never pass through
        // `write_at`, so close/fsync must write every page back, not only the
        // pages the store saw being written.
        let vol = alloc::sync::Arc::new(fs());
        let initial = alloc::vec![0xAA_u8; 3 * CHUNK];
        put(&vol, "paged", &initial);
        let mut store: SharedPathStore<(u64, String)> = SharedPathStore::new();
        let key = (1, String::from("paged"));
        let backing = store
            .ensure_shared(key.clone(), "paged", &*vol, fake_factory, true, 1)
            .unwrap();
        fake(backing.object).lock().data[CHUNK + 8] = 0x5C;
        store.flush_entry(&key, &*vol).unwrap();
        let mut got = [0u8; 1];
        vol.read_at("paged", (CHUNK + 8) as u64, &mut got).unwrap();
        assert_eq!(got[0], 0x5C);
    }
}
