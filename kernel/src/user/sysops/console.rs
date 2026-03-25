use core::cmp::min;
use core::sync::atomic::{AtomicUsize, Ordering};

use x86_64::VirtAddr;

use crate::io::tty;
use crate::memory::paging;
use crate::multitask;

use super::usermem::current_user_address_space;

const CONSOLE_IO_CHUNK_LEN: usize = 256;
const CONSOLE_IO_DEBUG_LOG_LIMIT: usize = 0;

static CONSOLE_READ_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);
static CONSOLE_WRITE_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn write_from_current_process(
    user_ptr: u64,
    len: usize,
) -> Result<usize, paging::AddressSpaceError> {
    let address_space = current_user_address_space()?;
    address_space.validate_user_read_buffer(VirtAddr::new(user_ptr), len)?;

    let mut copied = 0usize;
    let mut total_written = 0usize;
    let mut chunk = [0_u8; CONSOLE_IO_CHUNK_LEN];
    let session = multitask::current_console_session();
    let pid = multitask::current_user_id().unwrap_or(0);

    while copied < len {
        let chunk_len = min(len - copied, chunk.len());
        let chunk_ptr = user_ptr
            .checked_add(copied as u64)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        address_space.copy_from_user(VirtAddr::new(chunk_ptr), &mut chunk[..chunk_len])?;
        total_written += tty::write_to_session(session, &chunk[..chunk_len]);
        copied += chunk_len;
    }

    if !session.is_system()
        && CONSOLE_WRITE_DEBUG_LOGS.fetch_add(1, Ordering::Relaxed) < CONSOLE_IO_DEBUG_LOG_LIMIT
    {
        crate::debug::println!(
            "console write: pid={} session={} len={} total_written={}",
            pid,
            session.raw(),
            len,
            total_written,
        );
    }

    Ok(total_written)
}

pub(crate) fn read_into_current_process(
    user_ptr: u64,
    len: usize,
) -> Result<usize, paging::AddressSpaceError> {
    let address_space = current_user_address_space()?;
    address_space.validate_user_write_buffer(VirtAddr::new(user_ptr), len)?;

    let mut total_read = 0usize;
    let mut chunk = [0_u8; CONSOLE_IO_CHUNK_LEN];
    let session = multitask::current_console_session();
    let pid = multitask::current_user_id().unwrap_or(0);

    while total_read < len {
        let chunk_len = min(len - total_read, chunk.len());
        let read = if total_read == 0 {
            tty::read_input_blocking_for_session(session, &mut chunk[..chunk_len])
        } else {
            tty::read_input_for_session(session, &mut chunk[..chunk_len])
        };
        if read == 0 {
            break;
        }

        let chunk_ptr = user_ptr
            .checked_add(total_read as u64)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        address_space.copy_into_user(VirtAddr::new(chunk_ptr), &chunk[..read])?;
        total_read += read;
    }

    if !session.is_system()
        && CONSOLE_READ_DEBUG_LOGS.fetch_add(1, Ordering::Relaxed) < CONSOLE_IO_DEBUG_LOG_LIMIT
    {
        crate::debug::println!(
            "console read: pid={} session={} len={} total_read={}",
            pid,
            session.raw(),
            len,
            total_read,
        );
    }

    Ok(total_read)
}
