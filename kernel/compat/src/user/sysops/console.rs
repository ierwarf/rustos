use alloc::string::String;
use core::cmp::min;
use core::sync::atomic::{AtomicUsize, Ordering};

use x86_64::VirtAddr;

use crate::io::tty;
use crate::memory::paging;
use crate::multitask;

const CONSOLE_IO_CHUNK_LEN: usize = 256;
const CONSOLE_IO_DEBUG_LOG_LIMIT: usize = 0;

static CONSOLE_READ_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);
static CONSOLE_WRITE_DEBUG_LOGS: AtomicUsize = AtomicUsize::new(0);

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
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
        .unwrap_or_else(multitask::current_console_session);
    let pid = snapshot.map(|user| user.thread_id()).unwrap_or(0);

    while copied < len {
        let chunk_len = min(len - copied, chunk.len());
        let chunk_ptr = user_ptr
            .checked_add(copied as u64)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        let read_result = multitask::with_current_mm(|address_space| {
            if copied == 0 {
                address_space.validate_user_read_buffer(VirtAddr::new(user_ptr), len)?;
            }
            address_space.copy_from_user(VirtAddr::new(chunk_ptr), &mut chunk[..chunk_len])
        })
        .ok_or(paging::AddressSpaceError::NotMapped)?;
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

fn log_console_user_buffer_failure(
    stage: &str,
    user_ptr: u64,
    len: usize,
    fault_ptr: u64,
    fault_len: usize,
    err: paging::AddressSpaceError,
) {
    let Some(buffer_state) = multitask::with_current_mm(|address_space| {
        let end_addr = fault_ptr.saturating_add(fault_len.saturating_sub(1) as u64);
        (
            address_space.root_phys().as_u64(),
            address_space.translate_user_with_flags(VirtAddr::new(fault_ptr)),
            address_space.translate_user_with_flags(VirtAddr::new(end_addr)),
        )
    }) else {
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
    let (address_space_root, start, end) = buffer_state;
    let end_addr = fault_ptr.saturating_add(fault_len.saturating_sub(1) as u64);
    let snapshot = multitask::current_user_snapshot();
    if let Some((tid, abi, exec_path)) =
        multitask::with_current_user_process_state(|tid, abi, process_state| {
            (tid, abi, String::from(process_state.exec_path()))
        })
    {
        crate::debug::println!(
            "console write {} failed: tid={} abi={:?} exec={} root={:#x} ptr={:#x} len={} fault_ptr={:#x} fault_len={} end={:#x} start_map={:?} end_map={:?} err={:?}",
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
            start,
            end,
            err,
        );
    } else {
        crate::debug::println!(
            "console write {} failed: snapshot={:?} ptr={:#x} len={} fault_ptr={:#x} fault_len={} end={:#x} root={:#x} start_map={:?} end_map={:?} err={:?}",
            stage,
            snapshot,
            user_ptr,
            len,
            fault_ptr,
            fault_len,
            end_addr,
            address_space_root,
            start,
            end,
            err,
        );
    }
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
pub(crate) fn read_into_current_process(
    user_ptr: u64,
    len: usize,
) -> Result<usize, paging::AddressSpaceError> {
    multitask::with_current_mm(|address_space| {
        address_space.validate_user_write_buffer(VirtAddr::new(user_ptr), len)
    })
    .ok_or(paging::AddressSpaceError::NotMapped)??;

    let mut total_read = 0usize;
    let mut chunk = [0_u8; CONSOLE_IO_CHUNK_LEN];
    let snapshot = multitask::current_user_snapshot();
    let session = snapshot
        .map(|user| user.console_session())
        .unwrap_or_else(multitask::current_console_session);
    let pid = snapshot.map(|user| user.thread_id()).unwrap_or(0);

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
        multitask::with_current_mm(|address_space| {
            address_space.copy_into_user(VirtAddr::new(chunk_ptr), &chunk[..read])
        })
        .ok_or(paging::AddressSpaceError::NotMapped)??;
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
