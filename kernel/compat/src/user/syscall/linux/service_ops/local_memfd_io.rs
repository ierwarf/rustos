//! Bounded local memfd copy transport for Linux read/write syscalls.
//!
//! - **Owner:** Compat owns current-process copyin/copyout and reschedule
//!   cadence; `kernel-ps` owns memfd frame, seal, and cursor state.
//! - **Boundary:** User pointers, lengths, offsets, and allocation failure are
//!   untrusted and checked before use.
//! - **Lifecycle:** Allocate one bounded bounce buffer, copy one chunk, commit
//!   it to/from the exact open description, then expose a reschedule point.
//! - **Concurrency:** No user copy or reschedule occurs while a memfd object
//!   guard is live.
//! - **Failure:** Invalid ranges, allocation failure, seals, and short I/O
//!   return explicit Linux errors without fabricated progress.
//! - **Forbidden:** No sub-page lock loop, unbounded allocation, raw user
//!   pointer dereference, or hidden fairness loop.
//! - **Evidence:** `vfs-open-description` and performance source contracts.

use alloc::vec::Vec;

use super::*;

// The former 256-byte write loop acquired two memfd locks and called
// `cond_resched` more than twelve thousand times for the uiserver snapshot.
// 64 KiB matches the read path, spans only sixteen backing pages, and retains
// one explicit fairness point between bounded chunks.
const LOCAL_MEMFD_IO_CHUNK_BYTES: usize = 64 * 1024;

pub(super) fn read(memfd: &mut multitask::MemfdHandle, user_ptr: u64, user_len: u64) -> u64 {
    read_with(user_ptr, user_len, |_, chunk| memfd.read_into(chunk))
}

pub(super) fn read_at(
    memfd: &multitask::MemfdHandle,
    user_ptr: u64,
    user_len: u64,
    offset: usize,
) -> u64 {
    read_with(user_ptr, user_len, |copied, chunk| {
        offset
            .checked_add(copied)
            .map(|position| memfd.read_at(position, chunk))
            .unwrap_or(0)
    })
}

pub(super) fn write(memfd: &mut multitask::MemfdHandle, user_ptr: u64, user_len: u64) -> u64 {
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    let mut chunk = match bounded_buffer(user_len) {
        Ok(chunk) => chunk,
        Err(errno) => return linux_errno(errno),
    };
    let mut copied = 0usize;
    while copied < user_len {
        let chunk_len = (user_len - copied).min(chunk.len());
        let Some(src) = user_ptr.checked_add(copied as u64) else {
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::copy_from_current_user_exact(src, &mut chunk[..chunk_len]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let written = match memfd.write_from(&chunk[..chunk_len]) {
            Ok(written) => written,
            Err(err) => return linux_errno(memfd_error_to_linux_errno(err)),
        };
        copied += written;
        multitask::cond_resched();
        if written < chunk_len {
            break;
        }
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
    if let Err(err) = usermem::validate_current_user_write_buffer(user_ptr, user_len) {
        return linux_errno(address_space_error_to_linux_errno(err));
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
            return linux_errno(LINUX_EINVAL);
        };
        if let Err(err) = usermem::write_current_user_bytes(dest, &chunk[..read_len]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        copied += read_len;
        multitask::cond_resched();
        if read_len < chunk_len {
            break;
        }
    }
    copied as u64
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
