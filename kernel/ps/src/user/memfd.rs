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
//! - **Evidence:** `memory-map`, `physical-frame-lifecycle`,
//!   `vfs-open-description`.
// RING3-MIGRATION-REFERENCE START: memfd-kernel-substrate exception:
// syscalld validates memfd_create policy and pagerd/mm broker owns mapping
// admission. Ring0 keeps fd-local cursor, seal enforcement, physical frame
// backing, and page-table-visible shared mapping substrate.
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
use x86_64::PhysAddr;

use crate::memory::{kernel_vm, phys};
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

#[derive(Debug)]
struct MemfdState {
    name: String,
    len: usize,
    frames: Vec<u64>,
    seals: u32,
    mapping_count: usize,
    writable_mapping_count: usize,
}

#[derive(Debug)]
struct MemfdObject {
    object_id: u64,
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
    pub fn new(name: String, allow_sealing: bool) -> Self {
        let initial_seals = if allow_sealing {
            0
        } else {
            linux_abi::F_SEAL_SEAL as u32
        };
        let object = Arc::new(MemfdObject {
            object_id: allocate_memfd_object_id(),
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

    pub fn path(&self) -> String {
        let object = self.object();
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
        if src.is_empty() {
            return Ok(0);
        }

        let mut open = self.inner.lock();
        let end = open
            .cursor
            .checked_add(src.len())
            .ok_or(MemfdError::InvalidArgument)?;
        let cursor = open.cursor;
        grow_then(
            &open.object,
            end,
            |state| check_write_seals(state, end),
            |state| write_at_locked(state.frames.as_slice(), cursor, src),
        )?;
        open.cursor = end;
        Ok(src.len())
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
        let mut state = object.state.lock();
        if len < state.len {
            if state.seals & linux_abi::F_SEAL_SHRINK as u32 != 0 {
                return Err(MemfdError::PermissionDenied);
            }
            if state.mapping_count != 0 {
                return Err(MemfdError::Busy);
            }
            let keep_pages = len.div_ceil(PAGE_SIZE);
            let released = state.frames.split_off(keep_pages);
            for frame_phys in released {
                phys::free_frame(PhysAddr::new(frame_phys));
            }
            // The last kept page still holds the old bytes past the new end.
            // A later grow (or a write after a seek beyond EOF) must observe
            // zeros there, as ftruncate(2) specifies, not the stale contents.
            let tail = len % PAGE_SIZE;
            if tail != 0
                && let Some(&last_frame) = state.frames.last()
            {
                // SAFETY: the frame is owned by this object and retained under
                // the object-state lock; the direct map covers the whole page
                // and `tail..PAGE_SIZE` stays inside it.
                unsafe {
                    ptr::write_bytes(
                        (kernel_vm::higher_half_addr(last_frame) as *mut u8).add(tail),
                        0,
                        PAGE_SIZE - tail,
                    );
                }
            }
            state.len = len;
            return Ok(());
        }

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
        )
    }

    pub fn seals(&self) -> u32 {
        self.object().state.lock().seals
    }

    pub fn add_seals(&self, seals: u32) -> Result<(), MemfdError> {
        let allowed = (linux_abi::F_SEAL_SEAL
            | linux_abi::F_SEAL_SHRINK
            | linux_abi::F_SEAL_GROW
            | linux_abi::F_SEAL_WRITE) as u32;
        if seals & !allowed != 0 {
            return Err(MemfdError::InvalidArgument);
        }

        let object = self.object();
        let mut state = object.state.lock();
        if state.seals & linux_abi::F_SEAL_SEAL as u32 != 0 {
            return Err(MemfdError::PermissionDenied);
        }
        if seals & linux_abi::F_SEAL_WRITE as u32 != 0 && state.writable_mapping_count != 0 {
            return Err(MemfdError::Busy);
        }
        state.seals |= seals;
        Ok(())
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
        let mut state = object.state.lock();
        let end = offset.checked_add(len).ok_or(MemfdError::InvalidArgument)?;
        let mapped_len = align_up_len(state.len).ok_or(MemfdError::InvalidArgument)?;
        if offset >= mapped_len || end > mapped_len {
            return Err(MemfdError::InvalidArgument);
        }
        if writable && state.seals & linux_abi::F_SEAL_WRITE as u32 != 0 {
            return Err(MemfdError::PermissionDenied);
        }

        let start_page = offset / PAGE_SIZE;
        let end_page = end.div_ceil(PAGE_SIZE);
        let frames = state.frames[start_page..end_page].to_vec();
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
        for frame_phys in state.frames.drain(..) {
            phys::free_frame(PhysAddr::new(frame_phys));
        }
    }
}

fn check_write_seals(state: &MemfdState, end: usize) -> Result<(), MemfdError> {
    if state.seals & linux_abi::F_SEAL_WRITE as u32 != 0 {
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

/// Zero-filled frames allocated outside the object lock. Dropping them frees
/// any frame that was not installed, so every early return releases its pages.
struct StagedFrames(Vec<u64>);

impl StagedFrames {
    /// Allocates and zeroes `count` frames without holding any lock, so a large
    /// grow never stalls readers, mappers or sealers of the same object.
    fn zeroed(count: usize) -> Result<Self, MemfdError> {
        let mut staged = Self(Vec::new());
        staged.0.try_reserve_exact(count).map_err(|_| MemfdError::NoMemory)?;
        for _ in 0..count {
            let frame_phys = phys::alloc_frame_user().ok_or(MemfdError::NoMemory)?;
            // The frame is recorded before it is touched so a failure below
            // cannot leak it.
            staged.0.push(frame_phys.as_u64());
            // SAFETY: the freshly allocated frame is exclusively owned by this
            // staging buffer, the higher-half direct map covers one complete
            // page, and nothing publishes the frame until it is installed.
            unsafe {
                ptr::write_bytes(
                    kernel_vm::higher_half_addr(frame_phys.as_u64()) as *mut u8,
                    0,
                    PAGE_SIZE,
                );
            }
        }
        Ok(staged)
    }
}

impl Drop for StagedFrames {
    fn drop(&mut self) {
        for frame_phys in self.0.drain(..) {
            phys::free_frame(PhysAddr::new(frame_phys));
        }
    }
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
    for _ in 0..GROW_ATTEMPTS {
        let missing = {
            let mut state = object.state.lock();
            check(&state)?;
            if state.frames.len() >= required_pages {
                // Only grows: a write ending inside the file must not shorten
                // it (shrinking is `truncate`'s job).
                state.len = state.len.max(len);
                return Ok(then(&mut state));
            }
            required_pages - state.frames.len()
        };
        let mut staged = StagedFrames::zeroed(missing)?;
        let mut state = object.state.lock();
        check(&state)?;
        let still_missing = required_pages.saturating_sub(state.frames.len());
        let take = still_missing.min(staged.0.len());
        state
            .frames
            .try_reserve(take)
            .map_err(|_| MemfdError::NoMemory)?;
        let keep = staged.0.len() - take;
        state.frames.extend(staged.0.drain(keep..));
        // Surplus frames (a concurrent grower won part of the range) are freed
        // by `staged`'s Drop after the lock is released below.
        drop(state);
        drop(staged);
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
    let mut copied = 0usize;
    while copied < read_len {
        let absolute = offset + copied;
        let page_index = absolute / PAGE_SIZE;
        let page_offset = absolute % PAGE_SIZE;
        let chunk_len = (read_len - copied).min(PAGE_SIZE - page_offset);
        let src =
            (kernel_vm::higher_half_addr(frames[page_index] + page_offset as u64)) as *const u8;
        // SAFETY: `frames[page_index]` is retained by the locked object
        // snapshot, bounds above keep the source within that page, and `dest`
        // owns at least `read_len` writable bytes.
        unsafe {
            crate::arch::simd::copy_fast(src, dest.as_mut_ptr().add(copied), chunk_len);
        }
        copied += chunk_len;
    }
    read_len
}

fn write_at_locked(frames: &[u64], offset: usize, src: &[u8]) {
    let mut copied = 0usize;
    while copied < src.len() {
        let absolute = offset + copied;
        let page_index = absolute / PAGE_SIZE;
        let page_offset = absolute % PAGE_SIZE;
        let chunk_len = (src.len() - copied).min(PAGE_SIZE - page_offset);
        let dest =
            (kernel_vm::higher_half_addr(frames[page_index] + page_offset as u64)) as *mut u8;
        // SAFETY: The retained frame belongs to this memfd object, the
        // page/chunk bounds stay within its direct-map page, and `src` owns the
        // complete immutable input slice.
        unsafe {
            crate::arch::simd::copy_fast(src.as_ptr().add(copied), dest, chunk_len);
        }
        copied += chunk_len;
    }
}
// RING3-MIGRATION-REFERENCE END: memfd kernel backing substrate exception.

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_object(seals: u32) -> MemfdObject {
        MemfdObject {
            object_id: 1,
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
    }

    #[test]
    fn memfd_objects_receive_nonzero_never_reused_futex_identities() {
        let first = MemfdHandle::new(String::from("first"), true);
        let second = MemfdHandle::new(String::from("second"), true);
        let first_id = first.object().object_id;
        let second_id = second.object().object_id;
        assert_ne!(first_id, 0);
        assert_ne!(first_id, second_id);
    }
}
