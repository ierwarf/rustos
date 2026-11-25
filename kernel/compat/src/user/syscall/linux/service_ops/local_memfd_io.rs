//! Bounded local memfd copy transport for Linux read/write syscalls.
//!
//! - **Owner:** Compat owns current-process copyin/copyout and reschedule
//!   cadence; `kernel-ps` owns memfd frame, seal, and cursor state.
//! - **Boundary:** User pointers, lengths, offsets, and allocation failure are
//!   untrusted and checked before use.
//! - **Lifecycle:** Reads allocate one bounded bounce buffer and copy one chunk
//!   per round. Writes copy user bytes straight into the memfd's private,
//!   unzeroed destination frames (`kernel-ps` installs them only after the
//!   copy completed); a bounce buffer exists only for bytes landing on pages
//!   that already existed, is allocated once per syscall and never re-zeroed.
//!   Each chunk is then followed by a reschedule point.
//! - **Concurrency:** No user copy or reschedule occurs while a memfd object
//!   guard is live.
//! - **Failure:** Invalid ranges, allocation failure, seals, and short I/O
//!   return explicit Linux errors without fabricated progress.
//! - **Forbidden:** No sub-page lock loop, unbounded allocation, raw user
//!   pointer dereference, or hidden fairness loop.
//! - **Evidence:** `vfs-open-description` and performance source contracts.

use alloc::vec::Vec;

use kernel_ps::api::memfd::MemfdWriteError;

use super::*;

// The former 256-byte write loop acquired two memfd locks and called
// `cond_resched` more than twelve thousand times for the uiserver snapshot.
// 64 KiB keeps the read bounce buffer small.
const LOCAL_MEMFD_IO_CHUNK_BYTES: usize = 64 * 1024;

// A write chunk is the unit of frame staging (one allocator/charge round per
// 64 frames), of user-copy binding, of the object-lock install, and of
// fault atomicity: a chunk is written entirely or not at all. 256 KiB spans
// sixty-four pages, amortizes those fixed costs, and still leaves a fairness
// point every few tens of microseconds of copying.
const LOCAL_MEMFD_WRITE_CHUNK_BYTES: usize = 256 * 1024;

/// `EISDIR`: a generated procfs directory snapshot is read only by `getdents64`.
const LINUX_EISDIR_ERRNO: i64 = 21;

fn is_directory_snapshot(memfd: &multitask::MemfdHandle) -> bool {
    memfd.proc_snapshot().is_some_and(|info| info.directory)
}

pub(super) fn read(memfd: &mut multitask::MemfdHandle, user_ptr: u64, user_len: u64) -> u64 {
    if is_directory_snapshot(memfd) {
        return linux_errno(LINUX_EISDIR_ERRNO);
    }
    read_with(user_ptr, user_len, |_, chunk| memfd.read_into(chunk))
}

pub(super) fn read_at(
    memfd: &multitask::MemfdHandle,
    user_ptr: u64,
    user_len: u64,
    offset: usize,
) -> u64 {
    if is_directory_snapshot(memfd) {
        return linux_errno(LINUX_EISDIR_ERRNO);
    }
    read_with(user_ptr, user_len, |copied, chunk| {
        offset
            .checked_add(copied)
            .map(|position| memfd.read_at(position, chunk))
            .unwrap_or(0)
    })
}

/// pread from an app DMA-BUF capability. Rights, range and liveness come from
/// the held capability; an admission hold over the whole (page-multiple)
/// allocation excludes provider retirement, scrub and reuse for the duration.
/// Revoked or stale capabilities fail with ESTALE and never read bytes.
pub(super) fn read_device_buffer_at(
    buffer: &kernel_ps::api::handles::BufferCapability,
    user_ptr: u64,
    user_len: u64,
    offset: u64,
) -> u64 {
    use kernel_ps::api::handles::{BufferError, BufferRights};
    if !buffer.rights().contains(BufferRights::READ) {
        return linux_errno(LINUX_EACCES);
    }
    let _hold = match buffer.admit_mapping(0, buffer.len(), false) {
        Ok(hold) => hold,
        Err(BufferError::Stale) => return linux_errno(LINUX_ESTALE),
        Err(_) => return linux_errno(LINUX_EINVAL),
    };
    let len = buffer.len();
    if offset >= len {
        return 0;
    }
    let available = len - offset;
    let mut failed = false;
    let result = read_with(user_ptr, user_len.min(available), |copied, chunk| {
        let Some(position) = offset.checked_add(copied as u64) else { failed = true; return 0 };
        let Some(aperture) = buffer.aperture_offset().checked_add(position) else { failed = true; return 0 };
        if kernel_io_manager::api::app_buffer::read_range(aperture, chunk) {
            chunk.len()
        } else {
            failed = true;
            0
        }
    });
    if failed && result == 0 { linux_errno(LINUX_ENODEV) } else { result }
}

pub(super) fn write(memfd: &mut multitask::MemfdHandle, user_ptr: u64, user_len: u64) -> u64 {
    if let Some(result) = super::proc_identity::write_snapshot(memfd, user_ptr, user_len) {
        return result;
    }
    write_with(user_ptr, user_len, |_written_so_far, len, scratch, fill| {
        memfd.write_from_with(len, scratch, fill)
    })
}

pub(super) fn write_at(
    memfd: &multitask::MemfdHandle,
    user_ptr: u64,
    user_len: u64,
    offset: u64,
) -> u64 {
    if memfd.proc_snapshot().is_some() {
        if offset != 0 {
            return linux_errno(LINUX_EINVAL);
        }
        if let Some(result) = super::proc_identity::write_snapshot(memfd, user_ptr, user_len) {
            return result;
        }
    }
    let Ok(base) = usize::try_from(offset) else {
        return linux_errno(LINUX_EINVAL);
    };
    write_with(user_ptr, user_len, |written_so_far, len, scratch, fill| {
        let pos = base
            .checked_add(written_so_far)
            .ok_or(MemfdWriteError::Memfd(
                multitask::MemfdError::InvalidArgument,
            ))?;
        memfd.write_at_with(pos, len, scratch, fill)
    })
}

type UserFill<'a> = &'a mut dyn FnMut(usize, &mut [u8]) -> Result<(), paging::AddressSpaceError>;

/// Copies `user_len` bytes from user memory into a memfd in chunks.
/// `write_chunk(done, len, scratch, fill)` commits the next `len` bytes, taking
/// them from `fill` (see `MemfdHandle::write_at_with`); `scratch` is one
/// buffer reused by every chunk of this syscall.
fn write_with<F>(user_ptr: u64, user_len: u64, mut write_chunk: F) -> u64
where
    F: FnMut(
        usize,
        usize,
        &mut Vec<u8>,
        UserFill<'_>,
    ) -> Result<usize, MemfdWriteError<paging::AddressSpaceError>>,
{
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    if user_ptr.checked_add(user_len as u64).is_none() {
        return linux_errno(LINUX_EFAULT);
    }
    let mut scratch = Vec::new();
    let mut copied = 0usize;
    while copied < user_len {
        let chunk_len = (user_len - copied).min(LOCAL_MEMFD_WRITE_CHUNK_BYTES);
        // In range: `user_ptr + user_len` was checked above.
        let chunk_ptr = user_ptr + copied as u64;
        let mut fill = |at: usize, dst: &mut [u8]| {
            usermem::copy_from_current_user_exact(chunk_ptr + at as u64, dst)
        };
        match write_chunk(copied, chunk_len, &mut scratch, &mut fill) {
            Ok(written) => copied += written,
            Err(MemfdWriteError::Memfd(err)) => {
                return completed_or_error(copied, memfd_error_to_linux_errno(err));
            }
            Err(MemfdWriteError::Source(err)) => {
                return completed_or_error(copied, user_pointer_error_to_linux_errno(err));
            }
        }
        multitask::cond_resched();
    }
    copied as u64
}

fn read_with<F>(user_ptr: u64, user_len: u64, mut read: F) -> u64
where
    F: FnMut(usize, &mut [u8]) -> usize,
{
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    if let Err(errno) = validate_user_write(user_ptr, user_len) {
        return linux_errno(errno);
    }
    let mut chunk = match bounded_buffer(user_len) {
        Ok(chunk) => chunk,
        Err(errno) => return linux_errno(errno),
    };
    let mut copied = 0usize;
    while copied < user_len {
        let chunk_len = (user_len - copied).min(chunk.len());
        let read_len = read(copied, &mut chunk[..chunk_len]);
        if read_len == 0 {
            break;
        }
        let Some(dest) = user_ptr.checked_add(copied as u64) else {
            return completed_or_error(copied, LINUX_EFAULT);
        };
        if let Err(errno) = write_user_bytes(dest, &chunk[..read_len]) {
            return completed_or_error(copied, errno);
        }
        copied += read_len;
        multitask::cond_resched();
        if read_len < chunk_len {
            break;
        }
    }
    copied as u64
}

fn completed_or_error(completed: usize, errno: i64) -> u64 {
    if completed != 0 {
        completed as u64
    } else {
        linux_errno(errno)
    }
}

fn bounded_buffer(user_len: usize) -> Result<Vec<u8>, i64> {
    let chunk_len = local_memfd_io_chunk_len(user_len);
    let mut chunk = Vec::new();
    chunk
        .try_reserve_exact(chunk_len)
        .map_err(|_| LINUX_ENOMEM)?;
    chunk.resize(chunk_len, 0);
    Ok(chunk)
}

fn local_memfd_io_chunk_len(user_len: usize) -> usize {
    user_len.min(LOCAL_MEMFD_IO_CHUNK_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_after_completed_chunks_return_the_byte_count() {
        assert_eq!(completed_or_error(0, LINUX_EFAULT), linux_errno(LINUX_EFAULT));
        assert_eq!(completed_or_error(LOCAL_MEMFD_IO_CHUNK_BYTES, LINUX_EFAULT), LOCAL_MEMFD_IO_CHUNK_BYTES as u64);
        assert_eq!(completed_or_error(1, LINUX_EINVAL), 1);
    }

    #[test]
    fn local_memfd_write_chunk_is_page_multiple_and_bounded() {
        assert_eq!(LOCAL_MEMFD_WRITE_CHUNK_BYTES % 4096, 0);
        assert!(LOCAL_MEMFD_WRITE_CHUNK_BYTES >= LOCAL_MEMFD_IO_CHUNK_BYTES);
    }

    #[test]
    fn local_memfd_io_chunk_is_page_multiple_and_strictly_bounded() {
        assert_eq!(LOCAL_MEMFD_IO_CHUNK_BYTES % 4096, 0);
        assert_eq!(local_memfd_io_chunk_len(0), 0);
        assert_eq!(local_memfd_io_chunk_len(1), 1);
        assert_eq!(
            local_memfd_io_chunk_len(LOCAL_MEMFD_IO_CHUNK_BYTES + 1),
            LOCAL_MEMFD_IO_CHUNK_BYTES
        );
    }
}
