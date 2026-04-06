use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr;

use spin::Mutex;
use x86_64::PhysAddr;

use crate::memory::{kernel_vm, phys};
use crate::user::handles::{FileHandleSeekError, FileHandleSeekWhence};
use crate::user::linux as linux_abi;

const PAGE_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum MemfdError {
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
    state: Mutex<MemfdState>,
}

#[derive(Debug)]
struct MemfdOpenState {
    object: Arc<MemfdObject>,
    cursor: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct MemfdHandle {
    inner: Arc<Mutex<MemfdOpenState>>,
}

#[derive(Debug)]
struct MemfdMappingToken {
    object: Arc<MemfdObject>,
    writable: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct MemfdMappingHold {
    token: Arc<MemfdMappingToken>,
}

impl MemfdHandle {
    pub(crate) fn new(name: String, allow_sealing: bool) -> Self {
        let initial_seals = if allow_sealing {
            0
        } else {
            linux_abi::F_SEAL_SEAL as u32
        };
        let object = Arc::new(MemfdObject {
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

    pub(crate) fn path(&self) -> String {
        let object = self.object();
        let state = object.state.lock();
        alloc::format!("anon_inode:[memfd:{}]", state.name)
    }

    pub(crate) fn len(&self) -> usize {
        self.object().state.lock().len
    }

    pub(crate) fn read_into(&mut self, dest: &mut [u8]) -> usize {
        let mut open = self.inner.lock();
        let object = open.object.clone();
        let state = object.state.lock();
        let read = read_at_locked(state.frames.as_slice(), state.len, open.cursor, dest);
        drop(state);
        open.cursor = open.cursor.saturating_add(read);
        read
    }

    pub(crate) fn read_at(&self, offset: usize, dest: &mut [u8]) -> usize {
        let object = self.object();
        let state = object.state.lock();
        read_at_locked(state.frames.as_slice(), state.len, offset, dest)
    }

    pub(crate) fn write_from(&mut self, src: &[u8]) -> Result<usize, MemfdError> {
        if src.is_empty() {
            return Ok(0);
        }

        let mut open = self.inner.lock();
        let mut state = open.object.state.lock();
        check_write_seal(&state)?;
        let end = open
            .cursor
            .checked_add(src.len())
            .ok_or(MemfdError::InvalidArgument)?;
        ensure_len_locked(&mut state, end)?;
        write_at_locked(state.frames.as_slice(), open.cursor, src);
        drop(state);
        open.cursor = end;
        Ok(src.len())
    }

    pub(crate) fn seek(
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

    pub(crate) fn truncate(&self, len: usize) -> Result<(), MemfdError> {
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

    pub(crate) fn seals(&self) -> u32 {
        self.object().state.lock().seals
    }

    pub(crate) fn add_seals(&self, seals: u32) -> Result<(), MemfdError> {
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

    pub(crate) fn acquire_mapping(
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
        state.mapping_count = state.mapping_count.saturating_add(1);
        if writable {
            state.writable_mapping_count = state.writable_mapping_count.saturating_add(1);
        }
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
    pub(crate) fn path(&self) -> String {
        let state = self.token.object.state.lock();
        alloc::format!("anon_inode:[memfd:{}]", state.name)
    }
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

fn check_write_seal(state: &MemfdState) -> Result<(), MemfdError> {
    if state.seals & linux_abi::F_SEAL_WRITE as u32 != 0 {
        return Err(MemfdError::PermissionDenied);
    }
    Ok(())
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
        unsafe {
            ptr::copy_nonoverlapping(src, dest.as_mut_ptr().add(copied), chunk_len);
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
        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr().add(copied), dest, chunk_len);
        }
        copied += chunk_len;
    }
}
