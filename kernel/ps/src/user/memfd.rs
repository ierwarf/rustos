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
        let object = open.object.clone();
        let state = object.state.lock();
        let read = read_at_locked(state.frames.as_slice(), state.len, open.cursor, dest);
        drop(state);
        open.cursor = open.cursor.saturating_add(read);
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
        let mut state = open.object.state.lock();
        let end = open
            .cursor
            .checked_add(src.len())
            .ok_or(MemfdError::InvalidArgument)?;
        check_write_seals(&state, end)?;
        ensure_len_locked(&mut state, end)?;
        write_at_locked(state.frames.as_slice(), open.cursor, src);
        drop(state);
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
            state.len = len;
            return Ok(());
        }

        if len > state.len {
            if state.seals & linux_abi::F_SEAL_GROW as u32 != 0 {
                return Err(MemfdError::PermissionDenied);
            }
            ensure_len_locked(&mut state, len)?;
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

fn ensure_len_locked(state: &mut MemfdState, len: usize) -> Result<(), MemfdError> {
    if len <= state.len {
        state.len = len;
        return Ok(());
    }

    let required_pages = len.div_ceil(PAGE_SIZE);
    let original_len = state.frames.len();
    while state.frames.len() < required_pages {
        let Some(frame_phys) = phys::alloc_frame() else {
            for frame_phys in state.frames.drain(original_len..) {
                phys::free_frame(PhysAddr::new(frame_phys));
            }
            return Err(MemfdError::NoMemory);
        };
        // SAFETY: The freshly allocated frame is exclusively owned by this
        // object, the higher-half direct map covers one complete page, and no
        // handle publishes the frame until zeroing completes.
        unsafe {
            ptr::write_bytes(
                kernel_vm::higher_half_addr(frame_phys.as_u64()) as *mut u8,
                0,
                PAGE_SIZE,
            );
        }
        state.frames.push(frame_phys.as_u64());
    }
    state.len = len;
    Ok(())
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
