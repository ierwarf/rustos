use core::cmp::min;

use x86_64::VirtAddr;

use crate::multitask;
use crate::paging;
use crate::tty;

use super::usermem::current_user_address_space;

const CONSOLE_IO_CHUNK_LEN: usize = 256;

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

    while copied < len {
        let chunk_len = min(len - copied, chunk.len());
        let chunk_ptr = user_ptr
            .checked_add(copied as u64)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        address_space.copy_from_user(VirtAddr::new(chunk_ptr), &mut chunk[..chunk_len])?;
        total_written += tty::write_to_session(session, &chunk[..chunk_len]);
        copied += chunk_len;
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

    Ok(total_read)
}
