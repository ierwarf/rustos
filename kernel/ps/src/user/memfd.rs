//! Shared memfd object, open-description, seal, and frame lifetime substrate.
//!
//! - **Owner:** `syscalld` owns creation/mapping policy; ring0 owns exact
//!   descriptor state, seal enforcement, page frames, and shared mappings.
//! - **Boundary:** Names, sizes, offsets, seal transitions, and mapping modes
//!   are untrusted until overflow, bounds, and authority checks complete.
//! - **Lifecycle:** Allocate frames into one shared object, duplicate only the
//!   open description, account mappings, then release each frame exactly once.
//! - **Concurrency:** The object-state lock serializes seal/size/mapping state;
//!   user copies and page-table work do not hold unrelated process locks.
//! - **Failure:** Allocation, mapping, copy, and seal conflicts leave the old
//!   object state and frame ownership intact.
//! - **Forbidden:** No policy fallback, W+X widening, seal rollback, cursor
//!   sharing across distinct open descriptions, or frame reuse before release.
//! - **Accounting:** each backing frame is one page of the resident-page
//!   charge group of the Job that created the memfd (`charge_group`, recorded
//!   once at creation; none means unaccounted). A page is reserved before its
//!   frame is allocated and refunded on allocation failure, staged-grow
//!   rollback, shrink and exactly once in the object's `Drop`. The reservation
//!   itself keeps the group slot from being reused while frames exist (the Job
//!   layer waits for usage to drain). Shared mappings of a memfd map external
//!   leaves, which the root's data-leaf ledger does not count, so this
//!   object-level charge is the only one for them; immutable executable
//!   backing leases additionally bill the retaining root (separate custody).
//! - **Evidence:** `memory-map`, `physical-frame-lifecycle`,
//!   `vfs-open-description`, `memory-charge-conservation`.
// RING3-MIGRATION-REFERENCE START: memfd-kernel-substrate exception:
// syscalld validates memfd_create policy and pagerd/mm broker owns mapping
// admission. Ring0 keeps fd-local cursor, seal enforcement, physical frame
// backing, and page-table-visible shared mapping substrate.
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
#[cfg(not(test))]
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
#[cfg(not(test))]
use x86_64::PhysAddr;

use kernel_mm::api::charge;

#[cfg(not(test))]
use crate::memory::kernel_vm;
#[cfg(not(test))]
use crate::memory::phys;
use crate::user::handles::{FileHandleSeekError, FileHandleSeekWhence};
use crate::user::linux as linux_abi;

const PAGE_SIZE: usize = 4096;
static NEXT_MEMFD_OBJECT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MemfdError {
    Busy,
    InvalidArgument,
    NoMemory,
    PermissionDenied,
}

/// Failure of a memfd write whose bytes come from a fallible source.
#[derive(Debug, Eq, PartialEq)]
pub enum MemfdWriteError<E> {
    /// The memfd refused the write; nothing was committed.
    Memfd(MemfdError),
    /// The byte source failed; nothing was committed and no frame stayed
    /// reachable through the object.
    Source(E),
}

impl MemfdWriteError<core::convert::Infallible> {
    fn into_memfd(self) -> MemfdError {
        match self {
            Self::Memfd(err) => err,
            Self::Source(never) => match never {},
        }
    }
}

#[derive(Debug)]
struct MemfdState {
    name: String,
    len: usize,
    frames: Vec<u64>,
    seals: u32,
    mapping_count: usize,
    writable_mapping_count: usize,
}

/// Marks a memfd that is really a generated `/proc/<pid>/...` snapshot. The
/// object is created fully sealed and is never mappable, so the bytes are the
/// frozen view taken at `open`. `link` is what `readlink(/proc/self/fd/N)`
/// reports (the procfs path, not a memfd name) and `directory` selects
/// `S_IFDIR` stat and `getdents64` service.
#[derive(Debug)]
struct ProcSnapshotTag {
    link: String,
    directory: bool,
    ino: u64,
    node: u8,
    number: u64,
}

/// What a procfs snapshot memfd reports about itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcSnapshotInfo {
    /// `readlink(/proc/self/fd/N)` result.
    pub link: String,
    pub directory: bool,
    /// Stable inode of the procfs node (the same value `statx(path)` reports).
    pub ino: u64,
    /// `ProcKernelNode::code` of the node and its fd/tid number, so a write
    /// to a writable node (`comm`, `oom_score_adj`) can be routed without
    /// parsing `link`.
    pub node: u8,
    pub number: u64,
}

#[derive(Debug)]
struct MemfdObject {
    object_id: u64,
    /// Resident-page charge group of the creating Job; immutable for life.
    charge_group: Option<usize>,
    proc_snapshot: Option<ProcSnapshotTag>,
    /// Serializes `truncate` + PTE reconciliation against concurrent `mmap`
    /// (`reconcile_lock` -> `ProcessStateLock` -> `state`).
    reconcile_lock: Mutex<()>,
    state: Mutex<MemfdState>,
}

#[derive(Debug)]
struct MemfdOpenState {
    object: Arc<MemfdObject>,
    cursor: usize,
}

#[derive(Clone, Debug)]
pub struct MemfdHandle {
    inner: Arc<Mutex<MemfdOpenState>>,
}

#[derive(Debug)]
struct MemfdMappingToken {
    object: Arc<MemfdObject>,
    writable: bool,
}

#[derive(Clone, Debug)]
pub struct MemfdMappingHold {
    token: Arc<MemfdMappingToken>,
}

impl MemfdHandle {
    /// Creates an empty memfd whose backing frames are charged to
    /// `charge_group` (the creating Job's resident-page group), if any.
    pub fn new(name: String, allow_sealing: bool, charge_group: Option<usize>) -> Self {
        if let Some(group) = charge_group {
            kernel_mm::api::charge::retain(group);
        }
        let initial_seals = if allow_sealing {
            0
        } else {
            linux_abi::F_SEAL_SEAL as u32
        };
        let object = Arc::new(MemfdObject {
            object_id: allocate_memfd_object_id(),
            charge_group,
            proc_snapshot: None,
            reconcile_lock: Mutex::new(()),
            state: Mutex::new(MemfdState {
                name,
                len: 0,
                frames: Vec::new(),
                seals: initial_seals,
                mapping_count: 0,
                writable_mapping_count: 0,
            }),
        });
        Self {
            inner: Arc::new(Mutex::new(MemfdOpenState { object, cursor: 0 })),
        }
    }

    /// Creates the frozen backing of a generated procfs file or directory.
    /// `bytes` is written once, then every seal is applied, so the object can
    /// never change or be mapped. Pages are charged to `charge_group` like any
    /// memfd. Nothing is retained on failure.
    pub fn new_proc_snapshot(
        link: String,
        directory: bool,
        ino: u64,
        node: u8,
        number: u64,
        bytes: &[u8],
        charge_group: Option<usize>,
    ) -> Result<Self, MemfdError> {
        if let Some(group) = charge_group {
            kernel_mm::api::charge::retain(group);
        }
        let object = Arc::new(MemfdObject {
            object_id: allocate_memfd_object_id(),
            charge_group,
            proc_snapshot: Some(ProcSnapshotTag { link, directory, ino, node, number }),
            reconcile_lock: Mutex::new(()),
            state: Mutex::new(MemfdState {
                name: String::from("proc"),
                len: 0,
                frames: Vec::new(),
                seals: 0,
                mapping_count: 0,
                writable_mapping_count: 0,
            }),
        });
        let mut scratch = Vec::new();
        write_with_source(&object, 0, bytes.len(), &mut scratch, &mut |at, dst| {
            dst.copy_from_slice(&bytes[at..at + dst.len()]);
            Ok::<(), core::convert::Infallible>(())
        })
        .map_err(MemfdWriteError::into_memfd)?;
        object.state.lock().seals = (linux_abi::F_SEAL_SEAL
            | linux_abi::F_SEAL_SHRINK
            | linux_abi::F_SEAL_GROW
            | linux_abi::F_SEAL_WRITE
            | linux_abi::F_SEAL_FUTURE_WRITE) as u32;
        Ok(Self {
            inner: Arc::new(Mutex::new(MemfdOpenState { object, cursor: 0 })),
        })
    }

    /// Identity of a procfs snapshot, or `None` for an ordinary memfd.
    pub fn proc_snapshot(&self) -> Option<ProcSnapshotInfo> {
        self.object().proc_snapshot.as_ref().map(|tag| ProcSnapshotInfo {
            link: tag.link.clone(),
            directory: tag.directory,
            ino: tag.ino,
            node: tag.node,
            number: tag.number,
        })
    }

    /// Current read position of this open description.
    pub fn cursor(&self) -> usize {
        self.inner.lock().cursor
    }

    /// The cursor and up to `max` bytes at it, read under the description
    /// lock so both belong to one instant. Pair with
    /// [`Self::advance_cursor_if`]: a reader that consumes a prefix commits it
    /// only if no other thread moved the shared cursor in between, which is
    /// what keeps a record stream (`getdents64`) from duplicating or skipping
    /// entries when two threads read one open description.
    pub fn peek_at_cursor(&self, max: usize) -> (usize, Vec<u8>) {
        let open = self.inner.lock();
        let state = open.object.state.lock();
        let available = state.len.saturating_sub(open.cursor).min(max);
        let mut bytes = alloc::vec![0_u8; available];
        let read = read_at_locked(state.frames.as_slice(), state.len, open.cursor, &mut bytes);
        bytes.truncate(read);
        (open.cursor, bytes)
    }

    /// Moves the cursor from `expected` to `expected + count` only if it is
    /// still at `expected`. `false` means another reader consumed first.
    pub fn advance_cursor_if(&self, expected: usize, count: usize) -> bool {
        let mut open = self.inner.lock();
        if open.cursor != expected {
            return false;
        }
        open.cursor = expected.saturating_add(count);
        true
    }

    /// Creates a distinct open description backed by the same shared object.
    pub fn reopen(&self) -> Self {
        let object = self.object();
        Self {
            inner: Arc::new(Mutex::new(MemfdOpenState { object, cursor: 0 })),
        }
    }

    pub fn path(&self) -> String {
        let object = self.object();
        if let Some(tag) = object.proc_snapshot.as_ref() {
            return tag.link.clone();
        }
        let state = object.state.lock();
        alloc::format!("anon_inode:[memfd:{}]", state.name)
    }

    pub fn len(&self) -> usize {
        self.object().state.lock().len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn token_id(&self) -> u64 {
        Arc::as_ptr(&self.inner) as usize as u64
    }

    /// Identity of the shared object behind this open description: the same
    /// for every `dup`, `reopen` and mapping of one memfd, distinct between
    /// memfds even when they share a name, and never recycled. This is the
    /// `st_ino` of the file; `token_id` names only the open description.
    pub fn object_id(&self) -> u64 {
        self.object().object_id
    }

    pub fn read_into(&mut self, dest: &mut [u8]) -> usize {
        let mut open = self.inner.lock();
        // Split the borrow so the object stays reachable through the open
        // description without an `Arc` clone on every sequential read.
        let MemfdOpenState { object, cursor } = &mut *open;
        let state = object.state.lock();
        let read = read_at_locked(state.frames.as_slice(), state.len, *cursor, dest);
        drop(state);
        *cursor = cursor.saturating_add(read);
        read
    }

    pub fn read_at(&self, offset: usize, dest: &mut [u8]) -> usize {
        let object = self.object();
        let state = object.state.lock();
        read_at_locked(state.frames.as_slice(), state.len, offset, dest)
    }

    pub fn write_from(&mut self, src: &[u8]) -> Result<usize, MemfdError> {
        let mut scratch = Vec::new();
        self.write_from_with(src.len(), &mut scratch, &mut |at, dst| {
            dst.copy_from_slice(&src[at..at + dst.len()]);
            Ok::<(), core::convert::Infallible>(())
        })
        .map_err(MemfdWriteError::into_memfd)
    }

    /// Writes `src` at `offset` without modifying this open description's cursor.
    pub fn write_at(&self, offset: usize, src: &[u8]) -> Result<usize, MemfdError> {
        let mut scratch = Vec::new();
        self.write_at_with(offset, src.len(), &mut scratch, &mut |at, dst| {
            dst.copy_from_slice(&src[at..at + dst.len()]);
            Ok::<(), core::convert::Infallible>(())
        })
        .map_err(MemfdWriteError::into_memfd)
    }

    /// Writes `len` bytes at the cursor, taking the bytes from `fill` instead
    /// of a kernel slice (see [`write_with_source`] for the `fill` and
    /// `scratch` contract).
    ///
    /// The cursor range is reserved under the open-description lock and the
    /// copy runs without it, because `fill` may fault in user memory. A failed
    /// or short write returns the cursor to the start of the range unless
    /// another writer or a seek has moved it meanwhile; in that case the
    /// later cursor owner wins, as it would for any interleaved sequential
    /// writers.
    pub fn write_from_with<E>(
        &mut self,
        len: usize,
        scratch: &mut Vec<u8>,
        fill: &mut dyn FnMut(usize, &mut [u8]) -> Result<(), E>,
    ) -> Result<usize, MemfdWriteError<E>> {
        let (object, start) = {
            let mut open = self.inner.lock();
            let start = open.cursor;
            let end = start
                .checked_add(len)
                .ok_or(MemfdWriteError::Memfd(MemfdError::InvalidArgument))?;
            open.cursor = end;
            (open.object.clone(), start)
        };
        let result = write_with_source(&object, start, len, scratch, fill);
        let committed = *result.as_ref().unwrap_or(&0);
        if committed != len {
            let mut open = self.inner.lock();
            if open.cursor == start + len {
                open.cursor = start + committed;
            }
        }
        result
    }

    /// [`Self::write_from_with`] at an explicit offset; the cursor is untouched.
    pub fn write_at_with<E>(
        &self,
        offset: usize,
        len: usize,
        scratch: &mut Vec<u8>,
        fill: &mut dyn FnMut(usize, &mut [u8]) -> Result<(), E>,
    ) -> Result<usize, MemfdWriteError<E>> {
        let object = self.object();
        write_with_source(&object, offset, len, scratch, fill)
    }

    pub fn seek(
        &mut self,
        offset: i64,
        whence: FileHandleSeekWhence,
    ) -> Result<u64, FileHandleSeekError> {
        let mut open = self.inner.lock();
        let len = open.object.state.lock().len as i128;
        let cursor = open.cursor as i128;
        let next = match whence {
            FileHandleSeekWhence::Start => Some(offset as i128),
            FileHandleSeekWhence::Current => cursor.checked_add(offset as i128),
            FileHandleSeekWhence::End => len.checked_add(offset as i128),
        }
        .ok_or(FileHandleSeekError::InvalidPosition)?;

        if next < 0 {
            return Err(FileHandleSeekError::InvalidPosition);
        }

        open.cursor = usize::try_from(next).map_err(|_| FileHandleSeekError::InvalidPosition)?;
        Ok(open.cursor as u64)
    }

    pub fn truncate(&self, len: usize) -> Result<(), MemfdError> {
        let object = self.object();
        let _reconcile = object.reconcile_lock.lock();
        let mut state = object.state.lock();
        if len < state.len {
            if state.seals & linux_abi::F_SEAL_SHRINK as u32 != 0 {
                return Err(MemfdError::PermissionDenied);
            }
            let keep_pages = len.div_ceil(PAGE_SIZE);
            let released = state.frames.split_off(keep_pages);
            let released_pages = released.len();
            // The last kept page still holds the old bytes past the new end.
            // A later grow (or a write after a seek beyond EOF) must observe
            // zeros there, as ftruncate(2) specifies, not the stale contents.
            let tail = len % PAGE_SIZE;
            if tail != 0
                && let Some(&last_frame) = state.frames.last()
            {
                zero_frame_tail(last_frame, tail);
            }
            state.len = len;
            let reconcile_frames = (state.mapping_count != 0 && released_pages != 0)
                .then(|| state.frames.clone());
            drop(state);
            // Replace shrunk leaves with EOF guards and complete TLB shootdowns
            // before freeing any physical frame.
            if let Some(frames) = reconcile_frames {
                reconcile_memfd_mappings(object.object_id, &frames);
            }
            for frame_phys in released {
                free_backing_frame(frame_phys);
            }
            refund_pages(object.charge_group, released_pages);
            return Ok(());
        }

        let old_pages = state.frames.len();
        drop(state);
        grow_then(
            &object,
            len,
            |state| {
                if state.len < len && state.seals & linux_abi::F_SEAL_GROW as u32 != 0 {
                    return Err(MemfdError::PermissionDenied);
                }
                Ok(())
            },
            |_| (),
        )?;
        let state = object.state.lock();
        let reconcile_frames = (state.mapping_count != 0 && state.frames.len() > old_pages)
            .then(|| state.frames.clone());
        drop(state);
        if let Some(frames) = reconcile_frames {
            reconcile_memfd_mappings(object.object_id, &frames);
        }
        Ok(())
    }

    pub fn seals(&self) -> u32 {
        self.object().state.lock().seals
    }

    pub fn add_seals(&self, seals: u32) -> Result<(), MemfdError> {
        let allowed = (linux_abi::F_SEAL_SEAL
            | linux_abi::F_SEAL_SHRINK
            | linux_abi::F_SEAL_GROW
            | linux_abi::F_SEAL_WRITE
            | linux_abi::F_SEAL_FUTURE_WRITE) as u32;
        if seals & !allowed != 0 {
            return Err(MemfdError::InvalidArgument);
        }

        let object = self.object();
        let mut state = object.state.lock();
        if state.seals & linux_abi::F_SEAL_SEAL as u32 != 0 {
            return Err(MemfdError::PermissionDenied);
        }
        // F_SEAL_FUTURE_WRITE blocks *new* writes and *new* writable mappings
        // while allowing already-active writable mappings to keep writing.
        if seals & linux_abi::F_SEAL_WRITE as u32 != 0 && state.writable_mapping_count != 0 {
            return Err(MemfdError::Busy);
        }
        state.seals |= seals;
        Ok(())
    }

    pub fn with_reconcile_lock<R>(&self, f: impl FnOnce() -> R) -> R {
        let object = self.object();
        let _reconcile = object.reconcile_lock.lock();
        f()
    }

    pub fn acquire_mapping_with_eof_guard(
        &self,
        offset: usize,
        len: usize,
        writable: bool,
    ) -> Result<(Vec<u64>, usize, MemfdMappingHold), MemfdError> {
        if len == 0 || offset & (PAGE_SIZE - 1) != 0 {
            return Err(MemfdError::InvalidArgument);
        }

        let object = self.object();
        if object.proc_snapshot.is_some() {
            return Err(MemfdError::InvalidArgument);
        }
        let page_count = len.div_ceil(PAGE_SIZE);
        let start_page = offset / PAGE_SIZE;
        let mut state = object.state.lock();
        if writable
            && state.seals & (linux_abi::F_SEAL_WRITE | linux_abi::F_SEAL_FUTURE_WRITE) as u32 != 0
        {
            return Err(MemfdError::PermissionDenied);
        }
        let available_end = state.frames.len().min(start_page.saturating_add(page_count));
        let mut frames = Vec::new();
        if start_page < available_end {
            frames
                .try_reserve_exact(available_end - start_page)
                .map_err(|_| MemfdError::NoMemory)?;
            frames.extend_from_slice(&state.frames[start_page..available_end]);
        }
        let (mapping_count, writable_mapping_count) =
            next_mapping_counts(state.mapping_count, state.writable_mapping_count, writable)?;
        state.mapping_count = mapping_count;
        state.writable_mapping_count = writable_mapping_count;
        drop(state);

        Ok((
            frames,
            page_count,
            MemfdMappingHold {
                token: Arc::new(MemfdMappingToken { object, writable }),
            },
        ))
    }

    pub fn acquire_mapping(
        &self,
        offset: usize,
        len: usize,
        writable: bool,
    ) -> Result<(Vec<u64>, MemfdMappingHold), MemfdError> {
        if len == 0 || offset & (PAGE_SIZE - 1) != 0 {
            return Err(MemfdError::InvalidArgument);
        }

        let object = self.object();
        // procfs files are not mappable (Linux answers ENODEV).
        if object.proc_snapshot.is_some() {
            return Err(MemfdError::InvalidArgument);
        }
        let mut state = object.state.lock();
        let end = offset.checked_add(len).ok_or(MemfdError::InvalidArgument)?;
        let mapped_len = align_up_len(state.len).ok_or(MemfdError::InvalidArgument)?;
        if offset >= mapped_len || end > mapped_len {
            return Err(MemfdError::InvalidArgument);
        }
        if writable
            && state.seals & (linux_abi::F_SEAL_WRITE | linux_abi::F_SEAL_FUTURE_WRITE) as u32 != 0
        {
            return Err(MemfdError::PermissionDenied);
        }

        let start_page = offset / PAGE_SIZE;
        let end_page = end.div_ceil(PAGE_SIZE);
        let mut frames = Vec::new();
        frames.try_reserve_exact(end_page - start_page)
            .map_err(|_| MemfdError::NoMemory)?;
        frames.extend_from_slice(&state.frames[start_page..end_page]);
        let (mapping_count, writable_mapping_count) =
            next_mapping_counts(state.mapping_count, state.writable_mapping_count, writable)?;
        state.mapping_count = mapping_count;
        state.writable_mapping_count = writable_mapping_count;
        drop(state);

        Ok((
            frames,
            MemfdMappingHold {
                token: Arc::new(MemfdMappingToken { object, writable }),
            },
        ))
    }

    fn object(&self) -> Arc<MemfdObject> {
        self.inner.lock().object.clone()
    }
}

impl MemfdMappingHold {
    /// Stable object identity shared by every mapping of this memfd. IDs are
    /// never recycled, so an unmap/remap cannot alias a sleeping futex key.
    pub fn object_id(&self) -> u64 {
        self.token.object.object_id
    }

    /// Number of frames pinned by the complete object behind this hold.
    pub fn resident_pages(&self) -> u64 {
        self.token.object.state.lock().frames.len() as u64
    }

    /// Whether this mapping was admitted writable. A mapping admitted
    /// read-only (for example under `F_SEAL_WRITE`) must never be upgraded to
    /// write later, or the seal would be bypassed through `mprotect`.
    pub fn is_writable(&self) -> bool {
        self.token.writable
    }

    pub fn path(&self) -> String {
        let state = self.token.object.state.lock();
        alloc::format!("anon_inode:[memfd:{}]", state.name)
    }

    pub fn with_reconcile_and_frames<R>(&self, f: impl FnOnce(u64, &[u64]) -> R) -> R {
        let _reconcile = self.token.object.reconcile_lock.lock();
        let frames = self.token.object.state.lock().frames.clone();
        f(self.token.object.object_id, &frames)
    }
}

fn reconcile_memfd_mappings(object_id: u64, frames: &[u64]) {
    crate::multitask::for_each_visible_process_state_mut(|state| {
        let _ = state.reconcile_shared_memfd_mappings(object_id, frames);
    });
}

fn allocate_memfd_object_id() -> u64 {
    // ORDERING: identity allocation does not publish object contents; atomic
    // modification is required only to make IDs unique across CPUs.
    NEXT_MEMFD_OBJECT_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1).filter(|next| *next != 0)
        })
        .unwrap_or_else(|_| panic!("memfd object identity exhausted"))
}

impl Drop for MemfdMappingToken {
    fn drop(&mut self) {
        let mut state = self.object.state.lock();
        state.mapping_count = state.mapping_count.saturating_sub(1);
        if self.writable {
            state.writable_mapping_count = state.writable_mapping_count.saturating_sub(1);
        }
    }
}

impl Drop for MemfdObject {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        let pages = state.frames.len();
        for frame_phys in state.frames.drain(..) {
            free_backing_frame(frame_phys);
        }
        refund_pages(self.charge_group, pages);
        if let Some(group) = self.charge_group {
            kernel_mm::api::charge::release(group);
        }
    }
}

fn check_write_seals(state: &MemfdState, end: usize) -> Result<(), MemfdError> {
    if state.seals & (linux_abi::F_SEAL_WRITE | linux_abi::F_SEAL_FUTURE_WRITE) as u32 != 0 {
        return Err(MemfdError::PermissionDenied);
    }
    if end > state.len && state.seals & linux_abi::F_SEAL_GROW as u32 != 0 {
        return Err(MemfdError::PermissionDenied);
    }
    Ok(())
}

fn next_mapping_counts(
    mapping_count: usize,
    writable_mapping_count: usize,
    writable: bool,
) -> Result<(usize, usize), MemfdError> {
    let mapping_count = mapping_count.checked_add(1).ok_or(MemfdError::Busy)?;
    let writable_mapping_count = if writable {
        writable_mapping_count
            .checked_add(1)
            .ok_or(MemfdError::Busy)?
    } else {
        writable_mapping_count
    };
    Ok((mapping_count, writable_mapping_count))
}

/// Frames allocated outside the object lock and private to the allocating
/// call until installed. Dropping them frees (and refunds) every frame that was
/// not installed, so every early return releases its pages.
struct StagedFrames {
    frames: Vec<u64>,
    /// Group each staged frame is charged to; refunded when a frame is freed
    /// here. Frames moved into the object take their charge with them.
    group: Option<usize>,
}

/// Frames requested from the allocator (and charged) per round.
const STAGE_BATCH_FRAMES: usize = 64;

impl StagedFrames {
    /// Allocates `count` frames without holding any lock, so a large grow never
    /// stalls readers, mappers or sealers of the same object. Frames come from
    /// the allocator in batches of [`STAGE_BATCH_FRAMES`]. They are zero-filled
    /// only when `zero` is set; otherwise they hold arbitrary stale memory and
    /// the caller must overwrite every byte before installing them.
    fn allocate(count: usize, group: Option<usize>, zero: bool) -> Result<Self, MemfdError> {
        let mut staged = Self {
            frames: Vec::new(),
            group,
        };
        staged
            .frames
            .try_reserve_exact(count)
            .map_err(|_| MemfdError::NoMemory)?;
        let mut remaining = count;
        while remaining != 0 {
            let want = remaining.min(STAGE_BATCH_FRAMES);
            // Reserve the pages before allocating so a Job at its limit never
            // takes a frame; unfilled reservations are returned below.
            if !try_charge_pages(group, want) {
                return Err(MemfdError::NoMemory);
            }
            let mut batch = [0u64; STAGE_BATCH_FRAMES];
            let got = alloc_backing_frames(&mut batch[..want]);
            refund_pages(group, want - got);
            // Recorded (and each charge now owned by `staged`) before the
            // frames are touched so a failure below cannot leak either.
            staged.frames.extend_from_slice(&batch[..got]);
            if got < want {
                return Err(MemfdError::NoMemory);
            }
            if zero {
                for &frame_phys in &batch[..got] {
                    zero_frame(frame_phys);
                }
            }
            remaining -= got;
        }
        Ok(staged)
    }
}

impl Drop for StagedFrames {
    fn drop(&mut self) {
        let pages = self.frames.len();
        for frame_phys in self.frames.drain(..) {
            free_backing_frame(frame_phys);
        }
        refund_pages(self.group, pages);
    }
}

fn try_charge_pages(group: Option<usize>, pages: usize) -> bool {
    group.is_none_or(|group| charge::try_charge(group, pages as u64))
}

fn refund_pages(group: Option<usize>, pages: usize) {
    if let Some(group) = group
        && pages != 0
    {
        charge::uncharge(group, pages as u64);
    }
}

/// Fills `out` with user-class frames and returns how many were obtained; a
/// short fill means pressure or exhaustion.
#[cfg(not(test))]
fn alloc_backing_frames(out: &mut [u64]) -> usize {
    let mut frames = [PhysAddr::new(0); STAGE_BATCH_FRAMES];
    let frames = &mut frames[..out.len()];
    let got = phys::alloc_frames_batch_user(frames);
    for (slot, frame) in out.iter_mut().zip(&frames[..got]) {
        *slot = frame.as_u64();
    }
    got
}

/// Kernel (direct-map) pointer to the first byte of the frame at `frame_phys`.
#[cfg(not(test))]
fn frame_ptr(frame_phys: u64) -> *mut u8 {
    kernel_vm::higher_half_addr(frame_phys) as *mut u8
}

#[cfg(not(test))]
fn free_backing_frame(frame_phys: u64) {
    phys::free_frame(PhysAddr::new(frame_phys));
}

#[cfg(not(test))]
fn zero_frame(frame_phys: u64) {
    // SAFETY: the freshly allocated frame is exclusively owned by the caller's
    // staging buffer, the higher-half direct map covers one complete page, and
    // nothing publishes the frame until it is installed.
    unsafe {
        ptr::write_bytes(frame_ptr(frame_phys), 0, PAGE_SIZE);
    }
}

#[cfg(not(test))]
fn zero_frame_tail(frame_phys: u64, tail: usize) {
    zero_frame_range(frame_phys, tail, PAGE_SIZE);
}

/// Zeroes bytes `from..to` of a frame.
#[cfg(not(test))]
fn zero_frame_range(frame_phys: u64, from: usize, to: usize) {
    debug_assert!(from <= to && to <= PAGE_SIZE);
    // SAFETY: the frame is owned by the caller (staged) or by the object and
    // retained under the object-state lock; the direct map covers the whole
    // page and `from..to` stays inside it.
    unsafe {
        ptr::write_bytes(frame_ptr(frame_phys).add(from), 0, to - from);
    }
}

// Host tests have no physical allocator or direct map: a counting fake stands
// in so charge conservation is checkable without touching memory.
#[cfg(test)]
extern crate std;

#[cfg(test)]
std::thread_local! {
    static FAKE_FRAMES_LEFT: core::cell::Cell<usize> = const { core::cell::Cell::new(usize::MAX) };
    static FAKE_FRAMES_LIVE: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// Host fakes: frames are real, 0xAA-poisoned heap pages addressed by their
/// own pointer, so stale-content and copy behavior is observable.
#[cfg(test)]
fn fake_page_layout() -> std::alloc::Layout {
    std::alloc::Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap()
}

#[cfg(test)]
fn alloc_backing_frames(out: &mut [u64]) -> usize {
    let mut got = 0;
    for slot in out.iter_mut() {
        let allowed = FAKE_FRAMES_LEFT.with(|left| {
            if left.get() == 0 {
                return false;
            }
            left.set(left.get() - 1);
            true
        });
        if !allowed {
            break;
        }
        // SAFETY: non-zero-sized layout.
        let page = unsafe { std::alloc::alloc(fake_page_layout()) };
        assert!(!page.is_null());
        // SAFETY: `page` is a fresh PAGE_SIZE allocation.
        unsafe { core::ptr::write_bytes(page, 0xAA, PAGE_SIZE) };
        FAKE_FRAMES_LIVE.with(|live| live.set(live.get() + 1));
        *slot = page as u64;
        got += 1;
    }
    got
}

#[cfg(test)]
fn free_backing_frame(frame_phys: u64) {
    FAKE_FRAMES_LIVE.with(|live| live.set(live.get() - 1));
    // SAFETY: allocated by `alloc_backing_frames` with the same layout.
    unsafe { std::alloc::dealloc(frame_phys as *mut u8, fake_page_layout()) };
}

#[cfg(test)]
fn frame_ptr(frame_phys: u64) -> *mut u8 {
    frame_phys as *mut u8
}

#[cfg(test)]
fn zero_frame(frame_phys: u64) {
    zero_frame_range(frame_phys, 0, PAGE_SIZE);
}

#[cfg(test)]
fn zero_frame_tail(frame_phys: u64, tail: usize) {
    zero_frame_range(frame_phys, tail, PAGE_SIZE);
}

#[cfg(test)]
fn zero_frame_range(frame_phys: u64, from: usize, to: usize) {
    // SAFETY: fake frames are live PAGE_SIZE heap pages.
    unsafe { core::ptr::write_bytes(frame_ptr(frame_phys).add(from), 0, to - from) };
}

/// Bounded retries when concurrent grow/shrink keep invalidating a staged plan.
const GROW_ATTEMPTS: usize = 8;

/// Grows the object to at least `len` bytes, then runs `then` under the object
/// lock with the length guaranteed.
///
/// Frame allocation and zeroing happen with the lock dropped; the plan is
/// re-validated (`check`) and re-sized after relocking because a seal, a
/// truncate or another grower may have changed the object meanwhile. Frames that
/// are no longer needed are freed, and a plan that keeps being invalidated fails
/// with `Busy` instead of looping forever.
fn grow_then<R>(
    object: &MemfdObject,
    len: usize,
    check: impl Fn(&MemfdState) -> Result<(), MemfdError>,
    then: impl FnOnce(&mut MemfdState) -> R,
) -> Result<R, MemfdError> {
    let required_pages = len.div_ceil(PAGE_SIZE);
    let mut then = Some(then);
    for _ in 0..GROW_ATTEMPTS {
        let missing = {
            let mut state = object.state.lock();
            check(&state)?;
            if state.frames.len() >= required_pages {
                // Only grows: a write ending inside the file must not shorten
                // it (shrinking is `truncate`'s job).
                state.len = state.len.max(len);
                return Ok((then.take().expect("grow_then closure"))(&mut state));
            }
            required_pages - state.frames.len()
        };
        let mut staged = StagedFrames::allocate(missing, object.charge_group, true)?;
        let mut state = object.state.lock();
        check(&state)?;
        let still_missing = required_pages.saturating_sub(state.frames.len());
        let take = still_missing.min(staged.frames.len());
        state
            .frames
            .try_reserve(take)
            .map_err(|_| MemfdError::NoMemory)?;
        let keep = staged.frames.len() - take;
        state.frames.extend(staged.frames.drain(keep..));
        let outcome = if state.frames.len() >= required_pages {
            state.len = state.len.max(len);
            Some((then.take().expect("grow_then closure"))(&mut state))
        } else {
            None
        };
        // Surplus frames (a concurrent grower won part of the range) are freed
        // by `staged`'s Drop after the lock is released below.
        drop(state);
        drop(staged);
        if let Some(result) = outcome {
            return Ok(result);
        }
    }
    Err(MemfdError::Busy)
}

fn align_up_len(len: usize) -> Option<usize> {
    if len == 0 {
        return Some(0);
    }
    let rem = len % PAGE_SIZE;
    if rem == 0 {
        Some(len)
    } else {
        len.checked_add(PAGE_SIZE - rem)
    }
}

fn read_at_locked(frames: &[u64], len: usize, offset: usize, dest: &mut [u8]) -> usize {
    if dest.is_empty() || offset >= len {
        return 0;
    }
    let read_len = dest.len().min(len - offset);
    // SAFETY: `frames` are retained by the locked object snapshot and cover
    // `offset..offset + read_len` (`read_len` is clamped to `len`); `dest` owns
    // at least `read_len` writable bytes.
    unsafe {
        copy_frame_span(frames, offset, dest.as_mut_ptr(), read_len, SpanDirection::FromFrames);
    }
    read_len
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SpanDirection {
    ToFrames,
    FromFrames,
}

/// Copies `len` bytes between the contiguous buffer `buf` and the page-backed
/// byte range starting at `offset`, one page-bounded piece at a time. A span of
/// at least one page takes ONE wide-SIMD section for all its pieces
/// (`copy_fast` would save and restore the YMM upper halves for every 4 KiB
/// piece); shorter spans use `copy_fast`'s own small-copy paths.
///
/// # Safety
///
/// `frames` must be retained and cover `offset..offset + len`; `buf` must be
/// valid for `len` bytes in the copy direction and must not overlap the frames.
unsafe fn copy_frame_span(
    frames: &[u64],
    offset: usize,
    buf: *mut u8,
    len: usize,
    direction: SpanDirection,
) {
    let wide = len >= PAGE_SIZE && crate::arch::simd::avx_enabled();
    let _section = wide.then(crate::arch::simd::wide_simd_section);
    let mut copied = 0usize;
    while copied < len {
        let absolute = offset + copied;
        let page_offset = absolute % PAGE_SIZE;
        let piece = (len - copied).min(PAGE_SIZE - page_offset);
        // SAFETY: bounds above keep the piece inside one retained page.
        let frame = unsafe { frame_ptr(frames[absolute / PAGE_SIZE]).add(page_offset) };
        let (src, dst) = match direction {
            SpanDirection::ToFrames => (unsafe { buf.add(copied) } as *const u8, frame),
            SpanDirection::FromFrames => (frame as *const u8, unsafe { buf.add(copied) }),
        };
        // SAFETY: both ranges are valid for `piece` bytes and disjoint.
        unsafe {
            if wide {
                crate::arch::simd::copy_ymm(src, dst, piece);
            } else {
                crate::arch::simd::copy_fast(src, dst, piece);
            }
        }
        copied += piece;
    }
}

/// Writes `len` bytes at `offset`, taking them from `fill`, without ever
/// calling `fill` under an object lock.
///
/// `fill(at, dst)` must copy bytes `at..at + dst.len()` of the write into
/// `dst`. It is called for disjoint ranges that together cover `0..len`, may
/// fault or block, and may be called again for the same range when a concurrent
/// truncate invalidates the plan, so it must be idempotent. `dst` may be live
/// frame memory that holds stale bytes; `fill` must only write it.
///
/// Pages the write extends the object into are allocated unzeroed and private:
/// `fill` copies straight into them, bytes of a partially covered page outside
/// the write (a hole before `offset`, the tail after the end) are zeroed, and
/// only then are they installed under the lock. A failing `fill` therefore
/// drops the private frames and the object, its readers and its mappers never
/// see them. Bytes landing in pages that already existed are staged in
/// `scratch` (grown on demand, reusable across calls, zeroed only when it
/// grows) and copied under the lock after the seals are re-checked.
fn write_with_source<E>(
    object: &MemfdObject,
    offset: usize,
    len: usize,
    scratch: &mut Vec<u8>,
    fill: &mut dyn FnMut(usize, &mut [u8]) -> Result<(), E>,
) -> Result<usize, MemfdWriteError<E>> {
    if len == 0 {
        return Ok(0);
    }
    let end = offset
        .checked_add(len)
        .ok_or(MemfdWriteError::Memfd(MemfdError::InvalidArgument))?;
    let required_pages = end.div_ceil(PAGE_SIZE);
    for _ in 0..GROW_ATTEMPTS {
        let base_pages = {
            let state = object.state.lock();
            check_write_seals(&state, end).map_err(MemfdWriteError::Memfd)?;
            state.frames.len()
        };
        let base_bytes = base_pages.saturating_mul(PAGE_SIZE);
        let existing_len = if offset < base_bytes {
            end.min(base_bytes) - offset
        } else {
            0
        };
        if existing_len != 0 {
            if scratch.len() < existing_len {
                scratch
                    .try_reserve(existing_len - scratch.len())
                    .map_err(|_| MemfdWriteError::Memfd(MemfdError::NoMemory))?;
                scratch.resize(existing_len, 0);
            }
            fill(0, &mut scratch[..existing_len]).map_err(MemfdWriteError::Source)?;
        }
        let missing = required_pages.saturating_sub(base_pages);
        let mut staged = StagedFrames::allocate(missing, object.charge_group, false)
            .map_err(MemfdWriteError::Memfd)?;
        fill_staged_pages(&staged.frames, base_pages, offset, end, fill)
            .map_err(MemfdWriteError::Source)?;

        let mut state = object.state.lock();
        check_write_seals(&state, end).map_err(MemfdWriteError::Memfd)?;
        if state.frames.len() < base_pages {
            // A truncate removed pages the plan was written against.
            continue;
        }
        // Concurrent growers may have installed the first staged pages.
        let installed = (state.frames.len() - base_pages).min(staged.frames.len());
        state
            .frames
            .try_reserve(staged.frames.len() - installed)
            .map_err(|_| MemfdWriteError::Memfd(MemfdError::NoMemory))?;
        if existing_len != 0 {
            // SAFETY: the frames cover `base_pages` pages (re-checked above)
            // and `offset..offset + existing_len` lies inside them; `scratch`
            // holds `existing_len` bytes.
            unsafe {
                copy_frame_span(
                    state.frames.as_slice(),
                    offset,
                    scratch.as_mut_ptr(),
                    existing_len,
                    SpanDirection::ToFrames,
                );
            }
        }
        for (index, &frame_phys) in staged.frames[..installed].iter().enumerate() {
            let page_start = (base_pages + index) * PAGE_SIZE;
            let (lo, hi) = (offset.max(page_start), end.min(page_start + PAGE_SIZE));
            if lo < hi {
                // SAFETY: the staged frame is private and fully initialized;
                // the destination page exists in the locked object.
                unsafe {
                    copy_frame_span(
                        state.frames.as_slice(),
                        lo,
                        frame_ptr(frame_phys).add(lo - page_start),
                        hi - lo,
                        SpanDirection::ToFrames,
                    );
                }
            }
        }
        state.frames.extend(staged.frames.drain(installed..));
        state.len = state.len.max(end);
        // Staged frames a concurrent grower made redundant are freed after the
        // lock is released.
        drop(state);
        drop(staged);
        return Ok(len);
    }
    Err(MemfdWriteError::Memfd(MemfdError::Busy))
}

/// Fills the private frames for pages `base_pages..` with the part of the write
/// `[offset, end)` that falls on them and zeroes everything else on them. The
/// source bytes are copied over the longest physically adjacent runs.
fn fill_staged_pages<E>(
    frames: &[u64],
    base_pages: usize,
    offset: usize,
    end: usize,
    fill: &mut dyn FnMut(usize, &mut [u8]) -> Result<(), E>,
) -> Result<(), E> {
    // Covered byte range of staged frame `index` inside its page.
    let covered = |index: usize| {
        let page_start = (base_pages + index) * PAGE_SIZE;
        let lo = offset.max(page_start).saturating_sub(page_start);
        let hi = end.min(page_start + PAGE_SIZE).saturating_sub(page_start);
        if lo < hi { (lo, hi) } else { (0, 0) }
    };
    for (index, &frame_phys) in frames.iter().enumerate() {
        let (lo, hi) = covered(index);
        if lo == hi {
            zero_frame_range(frame_phys, 0, PAGE_SIZE);
        } else {
            zero_frame_range(frame_phys, 0, lo);
            zero_frame_range(frame_phys, hi, PAGE_SIZE);
        }
    }
    let mut index = 0usize;
    while index < frames.len() {
        let (lo, hi) = covered(index);
        if lo == hi {
            index += 1;
            continue;
        }
        let page_start = (base_pages + index) * PAGE_SIZE;
        let at = page_start + lo - offset;
        let mut run_len = hi - lo;
        let mut last = index;
        // A page extends the run only if the previous page is covered to its
        // end, this one from its start, and the frames are adjacent.
        while covered(last).1 == PAGE_SIZE
            && last + 1 < frames.len()
            && frames[last + 1] == frames[last] + PAGE_SIZE as u64
        {
            let (next_lo, next_hi) = covered(last + 1);
            if next_lo != 0 || next_hi == 0 {
                break;
            }
            run_len += next_hi;
            last += 1;
        }
        // SAFETY: the frames are private, the run lies inside their pages and
        // adjacent frames are adjacent in the direct map. `fill` only writes.
        let dst = unsafe { core::slice::from_raw_parts_mut(frame_ptr(frames[index]).add(lo), run_len) };
        fill(at, dst)?;
        index = last + 1;
    }
    Ok(())
}
// RING3-MIGRATION-REFERENCE END: memfd kernel backing substrate exception.

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_object(seals: u32) -> MemfdObject {
        MemfdObject {
            object_id: 1,
            charge_group: None,
            proc_snapshot: None,
            reconcile_lock: Mutex::new(()),
            state: Mutex::new(MemfdState {
                name: String::from("test"),
                len: 0,
                frames: Vec::new(),
                seals,
                mapping_count: 0,
                writable_mapping_count: 0,
            }),
        }
    }

    #[test]
    fn grow_then_checks_before_it_allocates_and_runs_the_action_under_the_lock() {
        // A seal that refuses the grow must fail before any frame is staged.
        // This host test has no frame allocator, so reaching `StagedFrames`
        // would fail with `NoMemory` instead of the seal's error.
        let sealed = empty_object(linux_abi::F_SEAL_GROW as u32);
        let refused = grow_then(
            &sealed,
            PAGE_SIZE,
            |state| check_write_seals(state, PAGE_SIZE),
            |_| (),
        );
        assert_eq!(refused, Err(MemfdError::PermissionDenied));

        // A zero-length request needs no frames and still runs the action.
        let object = empty_object(0);
        let ran = grow_then(&object, 0, |_| Ok(()), |state| state.len);
        assert_eq!(ran, Ok(0));
        assert!(object.state.lock().frames.is_empty());
    }

    #[test]
    fn memfd_seals_reject_growth_and_mapping_counter_overflow() {
        let state = MemfdState {
            name: String::from("test"),
            len: PAGE_SIZE,
            frames: Vec::new(),
            seals: linux_abi::F_SEAL_GROW as u32,
            mapping_count: 0,
            writable_mapping_count: 0,
        };
        assert_eq!(check_write_seals(&state, PAGE_SIZE), Ok(()));
        assert_eq!(
            check_write_seals(&state, PAGE_SIZE + 1),
            Err(MemfdError::PermissionDenied)
        );
        assert_eq!(
            next_mapping_counts(usize::MAX, 0, false),
            Err(MemfdError::Busy)
        );
        assert_eq!(
            next_mapping_counts(1, usize::MAX, true),
            Err(MemfdError::Busy)
        );
        let future_sealed = MemfdState {
            seals: linux_abi::F_SEAL_FUTURE_WRITE as u32,
            ..state
        };
        assert_eq!(
            check_write_seals(&future_sealed, PAGE_SIZE),
            Err(MemfdError::PermissionDenied)
        );
    }

    #[test]
    fn memfd_objects_receive_nonzero_never_reused_futex_identities() {
        let first = MemfdHandle::new(String::from("first"), true, None);
        let second = MemfdHandle::new(String::from("second"), true, None);
        let first_id = first.object().object_id;
        let second_id = second.object().object_id;
        assert_ne!(first_id, 0);
        assert_ne!(first_id, second_id);
    }

    // Charge-group slots are process-wide statics shared with other kernel-ps
    // tests: these use 50..=53 (kmem tests use a different counter family).
    fn fake_reset(frames_left: usize) {
        FAKE_FRAMES_LEFT.with(|left| left.set(frames_left));
        FAKE_FRAMES_LIVE.with(|live| live.set(0));
    }

    fn fake_live() -> usize {
        FAKE_FRAMES_LIVE.with(|live| live.get())
    }

    fn grown(group: usize, pages: usize) -> Result<MemfdHandle, MemfdError> {
        let handle = MemfdHandle::new(String::from("charged"), true, Some(group));
        handle.truncate(pages * PAGE_SIZE).map(|()| handle)
    }

    #[test]
    fn charge_refusal_leaves_no_partial_frames_and_no_leaked_charge() {
        charge::init_group(50, None);
        charge::set_limit(50, 3);
        fake_reset(usize::MAX);
        let handle = MemfdHandle::new(String::from("limited"), true, Some(50));
        assert_eq!(handle.truncate(5 * PAGE_SIZE), Err(MemfdError::NoMemory));
        assert_eq!(charge::usage(50), 0, "the staged charge is refunded");
        assert_eq!(fake_live(), 0, "the staged frames are freed");
        assert_eq!(handle.len(), 0);
        assert_eq!(handle.truncate(3 * PAGE_SIZE), Ok(()));
        assert_eq!(charge::usage(50), 3);
        assert_eq!(handle.truncate(4 * PAGE_SIZE), Err(MemfdError::NoMemory));
        assert_eq!(charge::usage(50), 3, "a refused grow keeps the old charge");
        assert_eq!(handle.len(), 3 * PAGE_SIZE);
        drop(handle);
        assert_eq!(charge::usage(50), 0);
        assert_eq!(fake_live(), 0);
    }

    #[test]
    fn allocation_failure_refunds_the_reservation_and_staged_pages() {
        charge::init_group(51, None);
        fake_reset(2);
        let handle = MemfdHandle::new(String::from("oom"), true, Some(51));
        assert_eq!(handle.truncate(4 * PAGE_SIZE), Err(MemfdError::NoMemory));
        assert_eq!(charge::usage(51), 0);
        assert_eq!(fake_live(), 0);
        fake_reset(usize::MAX);
    }

    #[test]
    fn shrink_refunds_released_pages_and_drop_refunds_exactly_once() {
        charge::init_group(52, None);
        fake_reset(usize::MAX);
        let handle = grown(52, 6).unwrap();
        assert_eq!(charge::usage(52), 6);
        assert_eq!(handle.truncate(2 * PAGE_SIZE + 1), Ok(()));
        assert_eq!(charge::usage(52), 3, "keeps ceil(len / page) pages");
        assert_eq!(fake_live(), 3);
        assert_eq!(handle.truncate(0), Ok(()));
        assert_eq!(charge::usage(52), 0);
        assert_eq!(handle.truncate(2 * PAGE_SIZE), Ok(()));
        let clone = handle.clone();
        assert_eq!(charge::usage(52), 2);
        drop(handle);
        assert_eq!(charge::usage(52), 2, "a duplicated description keeps the object");
        drop(clone);
        assert_eq!(charge::usage(52), 0, "the last drop refunds once");
        assert_eq!(fake_live(), 0);
    }

    #[test]
    fn a_mapped_memfd_shrinks_and_releases_frames_after_reconcile() {
        fake_reset(usize::MAX);
        let handle = MemfdHandle::new(String::from("mapped"), true, None);
        handle.truncate(3 * PAGE_SIZE).unwrap();
        let (_frames, hold) = handle.acquire_mapping(0, 3 * PAGE_SIZE, true).unwrap();
        // Shrinking past a page boundary reconciles mappings and releases page 3.
        assert_eq!(handle.truncate(2 * PAGE_SIZE), Ok(()));
        assert_eq!(handle.len(), 2 * PAGE_SIZE);
        assert_eq!(fake_live(), 2);
        // Keeping the last page: only EOF and the tail move.
        assert_eq!(handle.truncate(PAGE_SIZE + 1), Ok(()));
        assert_eq!(handle.len(), PAGE_SIZE + 1);
        assert_eq!(fake_live(), 2);
        // Growing back allocates the new frame.
        assert_eq!(handle.truncate(3 * PAGE_SIZE), Ok(()));
        assert_eq!(fake_live(), 3);
        drop(hold);
        assert_eq!(handle.truncate(PAGE_SIZE), Ok(()));
        assert_eq!(fake_live(), 1);
        drop(handle);
        assert_eq!(fake_live(), 0);
    }

    #[test]
    fn acquire_mapping_with_eof_guard_admits_ranges_past_eof() {
        fake_reset(usize::MAX);
        let handle = MemfdHandle::new(String::from("eof-guard"), true, None);
        handle.truncate(PAGE_SIZE).unwrap();
        let (frames, page_count, _hold) = handle
            .acquire_mapping_with_eof_guard(0, 3 * PAGE_SIZE, true)
            .unwrap();
        assert_eq!(page_count, 3);
        assert_eq!(frames.len(), 1);

        let (tail_frames, tail_pages, _tail_hold) = handle
            .acquire_mapping_with_eof_guard(2 * PAGE_SIZE, 2 * PAGE_SIZE, false)
            .unwrap();
        assert_eq!(tail_pages, 2);
        assert!(tail_frames.is_empty());
    }

    #[test]
    fn an_unaccounted_memfd_never_touches_a_group() {
        charge::init_group(53, None);
        fake_reset(usize::MAX);
        let handle = MemfdHandle::new(String::from("w"), true, Some(53));
        assert_eq!(handle.truncate(PAGE_SIZE), Ok(()));
        assert_eq!(charge::usage(53), 1);
        drop(handle);
        assert_eq!(charge::usage(53), 0);
        let free = MemfdHandle::new(String::from("free"), true, None);
        assert_eq!(free.truncate(2 * PAGE_SIZE), Ok(()));
        assert_eq!(charge::usage(53), 0);
    }

    #[test]
    fn reopen_creates_distinct_open_description_sharing_backing_object() {
        fake_reset(usize::MAX);
        let mut primary = MemfdHandle::new(String::from("shared"), true, None);
        assert_eq!(primary.truncate(PAGE_SIZE), Ok(()));
        assert_eq!(
            primary.seek(128, FileHandleSeekWhence::Start),
            Ok(128)
        );
        let mut reopened = primary.reopen();
        assert_ne!(primary.token_id(), reopened.token_id());
        assert_eq!(primary.object().object_id, reopened.object().object_id);
        assert_eq!(reopened.len(), PAGE_SIZE);
        assert_eq!(
            reopened.seek(0, FileHandleSeekWhence::Current),
            Ok(0),
            "reopened description starts at cursor 0"
        );
        assert_eq!(
            primary.seek(0, FileHandleSeekWhence::Current),
            Ok(128),
            "reopening or seeking the new description leaves the primary cursor untouched"
        );
        assert_eq!(reopened.add_seals(linux_abi::F_SEAL_SHRINK as u32), Ok(()));
        assert_eq!(primary.seals(), reopened.seals());
    }

    #[test]
    fn proc_snapshot_is_frozen_unmappable_and_reports_its_link() {
        // Host tests have no direct map behind fake frames, so content I/O is
        // exercised in the kernel-compat formatter tests; this covers the
        // sealing, identity and mapping rules.
        fake_reset(usize::MAX);
        let handle =
            MemfdHandle::new_proc_snapshot(String::from("/proc/7/maps"), false, 99, 1, 0, b"", None)
                .unwrap();
        assert_eq!(handle.path(), "/proc/7/maps");
        assert_eq!(
            handle.proc_snapshot(),
            Some(ProcSnapshotInfo {
                link: String::from("/proc/7/maps"),
                directory: false,
                ino: 99,
                node: 1,
                number: 0,
            })
        );
        assert_eq!(handle.len(), 0);
        assert_eq!(handle.write_at(0, b"x"), Err(MemfdError::PermissionDenied));
        assert_eq!(handle.truncate(PAGE_SIZE), Err(MemfdError::PermissionDenied));
        assert_eq!(
            handle.add_seals(linux_abi::F_SEAL_WRITE as u32),
            Err(MemfdError::PermissionDenied)
        );
        assert_eq!(
            handle.acquire_mapping(0, PAGE_SIZE, false).err(),
            Some(MemfdError::InvalidArgument)
        );
        let again = handle.reopen();
        assert_eq!(
            again.proc_snapshot().map(|info| info.link),
            Some(String::from("/proc/7/maps"))
        );
        assert_eq!(again.object_id(), handle.object_id());
        assert_eq!(again.cursor(), 0);
        let dir = MemfdHandle::new_proc_snapshot(String::from("/proc/7/fd"), true, 5, 32, 0, b"", None)
            .unwrap();
        assert!(dir.proc_snapshot().unwrap().directory);
        assert_eq!(
            MemfdHandle::new(String::from("m"), true, None).proc_snapshot(),
            None
        );
    }

    #[test]
    fn shared_cursor_advances_only_for_the_reader_that_saw_it() {
        fake_reset(usize::MAX);
        // Length 0: host tests cannot read content through fake frames, and
        // the cursor protocol does not depend on it.
        let mut a = MemfdHandle::new(String::from("shared-cursor"), true, None);
        let b = a.clone();
        // Both handles are one open description: one cursor.
        assert_eq!(a.peek_at_cursor(16), (0, Vec::new()));
        assert!(a.advance_cursor_if(0, 24));
        // A second reader that peeked at 0 before the first one committed
        // loses the race instead of re-consuming the same bytes.
        assert!(!b.advance_cursor_if(0, 24));
        assert_eq!(b.cursor(), 24);
        assert!(b.advance_cursor_if(24, 8));
        assert_eq!(a.seek(0, FileHandleSeekWhence::Current), Ok(32));
        // Reaching the end peeks nothing.
        assert!(a.advance_cursor_if(32, PAGE_SIZE));
        assert_eq!(a.peek_at_cursor(16), (32 + PAGE_SIZE, Vec::new()));
    }

    #[test]
    fn write_at_preserves_cursor_and_respects_seals() {
        fake_reset(usize::MAX);
        let mut handle = MemfdHandle::new(String::from("pwrite"), true, None);
        assert_eq!(handle.truncate(PAGE_SIZE), Ok(()));
        assert_eq!(handle.seek(128, FileHandleSeekWhence::Start), Ok(128));
        assert_eq!(handle.write_at(8, b""), Ok(0));
        assert_eq!(
            handle.seek(0, FileHandleSeekWhence::Current),
            Ok(128),
            "write_at must not move the cursor"
        );
        assert_eq!(
            handle.write_at(usize::MAX - 2, b"abcd"),
            Err(MemfdError::InvalidArgument)
        );
        assert_eq!(handle.add_seals(linux_abi::F_SEAL_GROW as u32), Ok(()));
        assert_eq!(
            handle.write_at(PAGE_SIZE, b"x"),
            Err(MemfdError::PermissionDenied)
        );
        assert_eq!(handle.add_seals(linux_abi::F_SEAL_WRITE as u32), Ok(()));
        assert_eq!(
            handle.write_at(0, b"X"),
            Err(MemfdError::PermissionDenied)
        );
    }

    fn pattern(len: usize, salt: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(7).wrapping_add(salt) | 1).collect()
    }

    fn slice_fill<'a>(
        src: &'a [u8],
    ) -> impl FnMut(usize, &mut [u8]) -> Result<(), ()> + 'a {
        move |at, dst| {
            dst.copy_from_slice(&src[at..at + dst.len()]);
            Ok(())
        }
    }

    fn read_all(handle: &MemfdHandle) -> Vec<u8> {
        let mut out = alloc::vec![0u8; handle.len()];
        assert_eq!(handle.read_at(0, &mut out), out.len());
        out
    }

    /// Bytes of the backing frames past `len`, which must stay zero.
    fn bytes_past_len(handle: &MemfdHandle) -> Vec<u8> {
        let object = handle.object();
        let state = object.state.lock();
        let mut out = Vec::new();
        for (page, &frame) in state.frames.iter().enumerate() {
            for byte in 0..PAGE_SIZE {
                if page * PAGE_SIZE + byte >= state.len {
                    // SAFETY: fake frames are live heap pages.
                    out.push(unsafe { *frame_ptr(frame).add(byte) });
                }
            }
        }
        out
    }

    #[test]
    fn unzeroed_growth_zeroes_holes_heads_and_tails_of_partial_pages() {
        fake_reset(usize::MAX);
        let handle = MemfdHandle::new(String::from("sparse"), true, None);
        // Starts in page 1 (page 0 is a pure hole) and ends mid page 3.
        let offset = PAGE_SIZE + 1000;
        let data = pattern(2 * PAGE_SIZE + 500, 3);
        let mut scratch = Vec::new();
        assert_eq!(
            handle.write_at_with(offset, data.len(), &mut scratch, &mut slice_fill(&data)),
            Ok(data.len())
        );
        assert!(scratch.is_empty(), "pure growth needs no bounce buffer");
        assert_eq!(handle.len(), offset + data.len());
        let all = read_all(&handle);
        assert!(all[..offset].iter().all(|&b| b == 0), "hole and head are zero, not 0xAA");
        assert_eq!(&all[offset..], &data[..]);
        assert!(
            bytes_past_len(&handle).iter().all(|&b| b == 0),
            "the unwritten tail of the last page is zero"
        );
        drop(handle);
        assert_eq!(fake_live(), 0);
    }

    #[test]
    fn a_write_spanning_more_than_one_allocation_batch_round_trips() {
        fake_reset(usize::MAX);
        let mut handle = MemfdHandle::new(String::from("big"), true, None);
        let pages = STAGE_BATCH_FRAMES * 2 + 3;
        let data = pattern(pages * PAGE_SIZE - 17, 9);
        let mut scratch = Vec::new();
        assert_eq!(
            handle.write_from_with(data.len(), &mut scratch, &mut slice_fill(&data)),
            Ok(data.len())
        );
        assert_eq!(handle.seek(0, FileHandleSeekWhence::Current), Ok(data.len() as u64));
        assert_eq!(read_all(&handle), data);
        assert_eq!(fake_live(), pages);
        // A second append continues exactly where the cursor stopped.
        let more = pattern(PAGE_SIZE + 5, 5);
        assert_eq!(handle.write_from(&more), Ok(more.len()));
        let all = read_all(&handle);
        assert_eq!(&all[..data.len()], &data[..]);
        assert_eq!(&all[data.len()..], &more[..]);
        assert!(bytes_past_len(&handle).iter().all(|&b| b == 0));
    }

    #[test]
    fn a_source_failing_mid_copy_exposes_nothing_and_refunds_everything() {
        charge::init_group(54, None);
        fake_reset(usize::MAX);
        let handle = MemfdHandle::new(String::from("fault"), true, Some(54));
        let mut calls = 0;
        let mut scratch = Vec::new();
        // The first run copies half of its bytes (into stale 0xAA frames) and
        // then faults.
        let result = handle.write_at_with(0, 5 * PAGE_SIZE, &mut scratch, &mut |_, dst| {
            calls += 1;
            let half = dst.len() / 2;
            dst[..half].fill(0x55);
            Err("fault")
        });
        assert_eq!(result, Err(MemfdWriteError::Source("fault")));
        assert_eq!(calls, 1);
        assert_eq!(handle.len(), 0);
        assert!(handle.object().state.lock().frames.is_empty());
        assert_eq!(charge::usage(54), 0, "the staged charge is refunded");
        assert_eq!(fake_live(), 0, "the private frames are freed");

        // A fault in the part overwriting existing pages changes nothing.
        let base = pattern(2 * PAGE_SIZE, 1);
        assert_eq!(handle.write_at(0, &base), Ok(base.len()));
        let live = fake_live();
        let result = handle.write_at_with(PAGE_SIZE, 3 * PAGE_SIZE, &mut scratch, &mut |_, dst| {
            dst.fill(0x66);
            Err("fault")
        });
        assert_eq!(result, Err(MemfdWriteError::Source("fault")));
        assert_eq!(read_all(&handle), base);
        assert_eq!(fake_live(), live);
        assert_eq!(charge::usage(54), 2);
        drop(handle);
        assert_eq!(charge::usage(54), 0);
        assert_eq!(fake_live(), 0);
    }

    #[test]
    fn overwrite_crossing_the_end_updates_existing_and_new_pages() {
        fake_reset(usize::MAX);
        let handle = MemfdHandle::new(String::from("cross"), true, None);
        let base = pattern(PAGE_SIZE + 100, 1);
        assert_eq!(handle.write_at(0, &base), Ok(base.len()));
        let patch = pattern(2 * PAGE_SIZE, 77);
        let offset = PAGE_SIZE - 300;
        let mut scratch = Vec::new();
        assert_eq!(
            handle.write_at_with(offset, patch.len(), &mut scratch, &mut slice_fill(&patch)),
            Ok(patch.len())
        );
        assert_eq!(scratch.len(), 2 * PAGE_SIZE - offset, "only the already-backed bytes bounce");
        let mut expected = base.clone();
        expected.resize(offset + patch.len(), 0);
        expected[offset..].copy_from_slice(&patch);
        assert_eq!(read_all(&handle), expected);
        assert!(bytes_past_len(&handle).iter().all(|&b| b == 0));
    }

    #[test]
    fn writes_keep_seal_quota_and_size_limits() {
        charge::init_group(55, None);
        charge::set_limit(55, 2);
        fake_reset(usize::MAX);
        let handle = MemfdHandle::new(String::from("quota"), true, Some(55));
        let data = pattern(3 * PAGE_SIZE, 2);
        assert_eq!(handle.write_at(0, &data), Err(MemfdError::NoMemory));
        assert_eq!(charge::usage(55), 0);
        assert_eq!(fake_live(), 0);
        assert_eq!(handle.write_at(0, &data[..2 * PAGE_SIZE]), Ok(2 * PAGE_SIZE));
        assert_eq!(charge::usage(55), 2);
        assert_eq!(handle.add_seals(linux_abi::F_SEAL_GROW as u32), Ok(()));
        assert_eq!(handle.write_at(2 * PAGE_SIZE, b"x"), Err(MemfdError::PermissionDenied));
        assert_eq!(handle.write_at(0, b"y"), Ok(1));
        assert_eq!(
            handle.write_at(usize::MAX - 1, b"zz"),
            Err(MemfdError::InvalidArgument)
        );
        assert_eq!(fake_live(), 2);
    }

    #[test]
    fn a_failed_cursor_write_returns_the_cursor() {
        fake_reset(usize::MAX);
        let mut handle = MemfdHandle::new(String::from("cursor"), true, None);
        assert_eq!(handle.write_from(b"abcd"), Ok(4));
        let mut scratch = Vec::new();
        let result = handle.write_from_with(PAGE_SIZE, &mut scratch, &mut |_, _| Err(()));
        assert_eq!(result, Err(MemfdWriteError::Source(())));
        assert_eq!(handle.seek(0, FileHandleSeekWhence::Current), Ok(4));
        assert_eq!(handle.len(), 4);
    }
}
