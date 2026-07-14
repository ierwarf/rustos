use alloc::string::String;
use core::cmp::min;

use crate::io::tty;
use crate::memory::paging;
use crate::multitask;
use crate::user::sysops::usermem;

const CONSOLE_IO_CHUNK_LEN: usize = 256;

pub(crate) fn write_from_current_process(
    user_ptr: u64,
    len: usize,
) -> Result<usize, paging::AddressSpaceError> {
    let mut copied = 0usize;
    let mut total_written = 0usize;
    let mut chunk = [0_u8; CONSOLE_IO_CHUNK_LEN];
    let snapshot = multitask::current_user_snapshot();
    let session = snapshot
        .map(|user| user.console_session())
        .ok_or(paging::AddressSpaceError::AddressOutOfRange)?;

    while copied < len {
        let chunk_len = min(len - copied, chunk.len());
        let chunk_ptr = user_ptr
            .checked_add(copied as u64)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        let read_result = usermem::copy_from_current_user_exact(chunk_ptr, &mut chunk[..chunk_len]);
        if let Err(err) = read_result {
            crate::debug::trace_loc!();
            log_console_user_buffer_failure("copy", user_ptr, len, chunk_ptr, chunk_len, err);
            return Err(err);
        }
        total_written += tty::write_to_session(session, &chunk[..chunk_len]);
        if session.is_system() {
            crate::debug::write_bytes(&chunk[..chunk_len]);
        }
        copied += chunk_len;
    }

    Ok(total_written)
}

fn log_console_user_buffer_failure(
    stage: &str,
    user_ptr: u64,
    len: usize,
    fault_ptr: u64,
    fault_len: usize,
    err: paging::AddressSpaceError,
) {
    let Some(address_space_root) =
        multitask::with_current_mm(|address_space| address_space.root_phys().as_u64())
    else {
        crate::debug::println!(
            "console write {} failed: ptr={:#x} len={} fault_ptr={:#x} fault_len={} err={:?} reason=no-current-mm",
            stage,
            user_ptr,
            len,
            fault_ptr,
            fault_len,
            err,
        );
        return;
    };
    let end_addr = fault_ptr.saturating_add(fault_len.saturating_sub(1) as u64);
    let snapshot = multitask::current_user_snapshot();
    if let Some((tid, abi, exec_path)) =
        multitask::with_current_user_process_state(|tid, abi, process_state| {
            (tid, abi, String::from(process_state.exec_path()))
        })
    {
        crate::debug::println!(
            "console write {} failed: tid={} abi={:?} exec={} root={:#x} ptr={:#x} len={} fault_ptr={:#x} fault_len={} end={:#x} err={:?}",
            stage,
            tid,
            abi,
            exec_path,
            address_space_root,
            user_ptr,
            len,
            fault_ptr,
            fault_len,
            end_addr,
            err,
        );
    } else {
        crate::debug::println!(
            "console write {} failed: snapshot={:?} ptr={:#x} len={} fault_ptr={:#x} fault_len={} end={:#x} root={:#x} err={:?}",
            stage,
            snapshot,
            user_ptr,
            len,
            fault_ptr,
            fault_len,
            end_addr,
            address_space_root,
            err,
        );
    }
}

pub(crate) fn read_into_current_process(
    user_ptr: u64,
    len: usize,
) -> Result<usize, paging::AddressSpaceError> {
    usermem::validate_current_user_write_buffer(user_ptr, len)?;

    let mut total_read = 0usize;
    let mut chunk = [0_u8; CONSOLE_IO_CHUNK_LEN];
    let session = multitask::current_user_snapshot()
        .map(|user| user.console_session())
        .ok_or(paging::AddressSpaceError::AddressOutOfRange)?;

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
        usermem::write_current_user_bytes(chunk_ptr, &chunk[..read])?;
        total_read += read;
    }

    Ok(total_read)
}
