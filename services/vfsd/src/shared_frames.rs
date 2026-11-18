//! vfsd-owned memfd frames behind shared-backed TmpFs files.
//!
//! - **Owner:** vfsd owns the descriptor; the TmpFs node owns this object and
//!   closes the descriptor when the node is freed. Mappings keep the frames
//!   alive past that through their own kernel hold.
//! - **Boundary:** all I/O is positional (`pread64`/`pwrite64`) so no cursor
//!   is shared with the kernel's view of the memfd, and vfsd never maps the
//!   memfd itself, which would make its own mapping block every shrink.
//! - **Failure:** short transfers are retried to completion; a refused
//!   shrink (`EBUSY`, extent still mapped) is reported unchanged.
//! - **Forbidden:** sealing (the file stays writable), keeping a descriptor
//!   whose creation or sizing failed.
use alloc::boxed::Box;
use alloc::ffi::CString;

use rustos_svc_runtime::syscall::{syscall1, syscall2, syscall4};
use rustos_user_abi::linux as linux_abi;
use storage_core::fs::FsError;
use storage_core::StorageError;
use vfsd::tmpfs::SharedFrames;

const EBUSY: i64 = 16;
const EFBIG: i64 = 27;
const EINVAL: i64 = 22;
const ENOMEM: i64 = 12;
const ENFILE: i64 = 23;
const EMFILE: i64 = 24;
const ENOSPC: i64 = 28;
const EDQUOT: i64 = 122;
/// Offset of `st_ino` in the x86-64 `struct stat` (144 bytes).
const STAT_INO_OFFSET: usize = 8;
const STAT_SIZE: usize = 144;

fn fs_error(status: i64) -> FsError {
    match status.checked_neg().unwrap_or(0) {
        EBUSY => FsError::Busy,
        EINVAL => FsError::InvalidInput,
        EFBIG => FsError::TooBig,
        ENOMEM | ENFILE | EMFILE | ENOSPC | EDQUOT => FsError::NoSpace,
        _ => FsError::Io(StorageError::DeviceFault),
    }
}

pub(crate) struct MemfdFrames {
    fd: i32,
    object: u64,
}

impl MemfdFrames {
    fn create(len: u64) -> Result<Self, FsError> {
        let name = CString::new("vfsd-tmpfs").map_err(|_| FsError::InvalidInput)?;
        // SAFETY: the NUL-terminated name outlives the call; the result is a
        // private descriptor until this function returns it.
        let status = unsafe {
            syscall2(linux_abi::SYS_MEMFD_CREATE, name.as_ptr() as u64, linux_abi::MFD_CLOEXEC)
        };
        if status < 0 {
            return Err(fs_error(status));
        }
        let fd = i32::try_from(status).map_err(|_| FsError::Io(StorageError::DeviceFault))?;
        // `frames` closes the descriptor on every early return below.
        let mut frames = Self { fd, object: 0 };
        frames.set_len(len)?;
        let mut stat = [0u8; STAT_SIZE];
        // SAFETY: `stat` is a writable 144-byte buffer, the size of the
        // kernel's `struct stat`, valid for the whole call.
        let status = unsafe { syscall2(linux_abi::SYS_FSTAT, fd as u64, stat.as_mut_ptr() as u64) };
        if status < 0 {
            return Err(fs_error(status));
        }
        let mut ino = [0u8; 8];
        ino.copy_from_slice(&stat[STAT_INO_OFFSET..STAT_INO_OFFSET + 8]);
        frames.object = u64::from_le_bytes(ino);
        if frames.object == 0 {
            return Err(FsError::Io(StorageError::DeviceFault));
        }
        Ok(frames)
    }
}

impl SharedFrames for MemfdFrames {
    fn identity(&self) -> (u64, u64) {
        (self.fd as u64, self.object)
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        let mut done = 0usize;
        while done < out.len() {
            let position = offset.checked_add(done as u64).ok_or(FsError::TooBig)?;
            // SAFETY: `out[done..]` is a live writable range of the stated
            // length for the duration of the call.
            let status = unsafe {
                syscall4(
                    linux_abi::SYS_PREAD64,
                    self.fd as u64,
                    out[done..].as_mut_ptr() as u64,
                    (out.len() - done) as u64,
                    position,
                )
            };
            if status < 0 {
                return Err(fs_error(status));
            }
            if status == 0 {
                break;
            }
            done += status as usize;
        }
        Ok(done)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        let mut done = 0usize;
        while done < data.len() {
            let position = offset.checked_add(done as u64).ok_or(FsError::TooBig)?;
            // SAFETY: `data[done..]` is a live readable range of the stated
            // length for the duration of the call.
            let status = unsafe {
                syscall4(
                    linux_abi::SYS_PWRITE64,
                    self.fd as u64,
                    data[done..].as_ptr() as u64,
                    (data.len() - done) as u64,
                    position,
                )
            };
            if status < 0 {
                return Err(fs_error(status));
            }
            if status == 0 {
                return Err(FsError::NoSpace);
            }
            done += status as usize;
        }
        Ok(done)
    }

    fn set_len(&mut self, len: u64) -> Result<(), FsError> {
        // SAFETY: takes only integers; a stale descriptor yields EBADF.
        let status = unsafe { syscall2(linux_abi::SYS_FTRUNCATE, self.fd as u64, len) };
        if status < 0 {
            return Err(fs_error(status));
        }
        Ok(())
    }
}

impl Drop for MemfdFrames {
    fn drop(&mut self) {
        // SAFETY: the descriptor is owned by this object and not used again.
        let _ = unsafe { syscall1(linux_abi::SYS_CLOSE, self.fd as u64) };
    }
}

pub(crate) fn create_memfd_frames(len: u64) -> Result<Box<dyn SharedFrames>, FsError> {
    Ok(Box::new(MemfdFrames::create(len)?))
}
