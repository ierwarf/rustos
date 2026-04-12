extern crate alloc;

use alloc::vec;
use core::mem::size_of;
use core::slice;

use diag_abi::{
    DEBUG_IOCTL_CONFIGURE, DEBUG_IOCTL_GET_STATE, DEBUG_IOCTL_SNAPSHOT_CRASH,
    DEBUG_IOCTL_SNAPSHOT_MODULES, DEBUG_IOCTL_TRIGGER_BREAK, DebugBreakRequest,
    DebugConfigureRequest, DebugCrashSnapshotRequest, DebugModuleInfo, DebugModuleSnapshotRequest,
    DiagLevel, DiagProvider, DiagRecord,
};
use x86_64::VirtAddr;

use crate::debug;
use crate::user::process_state::UserProcessState;
use crate::user::sysops::usermem;

use super::{DeviceError, read_user_struct, write_user_struct};

pub(crate) fn read_to_current_user(user_ptr: u64, user_len: usize) -> Result<usize, DeviceError> {
    let record_size = size_of::<DiagRecord>();
    let capacity = user_len / record_size;
    if capacity == 0 {
        return Ok(0);
    }

    let records = debug::drain_records(capacity);
    let bytes_len = records
        .len()
        .checked_mul(record_size)
        .ok_or(DeviceError::InvalidArgument)?;
    if bytes_len == 0 {
        return Ok(0);
    }

    usermem::current_user_address_space()
        .map_err(DeviceError::AddressSpace)?
        .address_space()
        .validate_user_write_buffer(VirtAddr::new(user_ptr), bytes_len)?;
    let bytes = unsafe { slice::from_raw_parts(records.as_ptr().cast::<u8>(), bytes_len) };
    usermem::write_current_user_bytes(user_ptr, bytes).map_err(DeviceError::AddressSpace)?;
    Ok(bytes_len)
}

pub(crate) fn read_to_user(
    process_state: &mut UserProcessState,
    user_ptr: u64,
    user_len: usize,
) -> Result<usize, DeviceError> {
    let record_size = size_of::<DiagRecord>();
    let capacity = user_len / record_size;
    if capacity == 0 {
        return Ok(0);
    }

    let records = debug::drain_records(capacity);
    let bytes_len = records
        .len()
        .checked_mul(record_size)
        .ok_or(DeviceError::InvalidArgument)?;
    if bytes_len == 0 {
        return Ok(0);
    }

    process_state
        .address_space()
        .validate_user_write_buffer(VirtAddr::new(user_ptr), bytes_len)?;
    let bytes = unsafe { slice::from_raw_parts(records.as_ptr().cast::<u8>(), bytes_len) };
    process_state
        .address_space()
        .copy_into_user(VirtAddr::new(user_ptr), bytes)?;
    Ok(bytes_len)
}

pub(crate) fn ioctl(
    process_state: &mut UserProcessState,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceError> {
    match request {
        DEBUG_IOCTL_GET_STATE => {
            let state = debug::device_state();
            write_user_struct(process_state.address_space(), arg, &state)?;
            Ok(0)
        }
        DEBUG_IOCTL_CONFIGURE => {
            let request =
                read_user_struct::<DebugConfigureRequest>(process_state.address_space(), arg)?;
            debug::configure(request);
            Ok(0)
        }
        DEBUG_IOCTL_SNAPSHOT_CRASH => {
            let mut request =
                read_user_struct::<DebugCrashSnapshotRequest>(process_state.address_space(), arg)?;
            let bytes = debug::snapshot_crash_bytes();
            let copy_len = bytes
                .len()
                .min(usize::try_from(request.capacity).map_err(|_| DeviceError::InvalidArgument)?);
            if copy_len != 0 {
                process_state
                    .address_space()
                    .validate_user_write_buffer(VirtAddr::new(request.bytes_ptr), copy_len)?;
                process_state
                    .address_space()
                    .copy_into_user(VirtAddr::new(request.bytes_ptr), &bytes[..copy_len])?;
            }
            request.count = copy_len as u64;
            write_user_struct(process_state.address_space(), arg, &request)?;
            Ok(0)
        }
        DEBUG_IOCTL_SNAPSHOT_MODULES => {
            let mut request =
                read_user_struct::<DebugModuleSnapshotRequest>(process_state.address_space(), arg)?;
            let capacity =
                usize::try_from(request.capacity).map_err(|_| DeviceError::InvalidArgument)?;
            let mut modules = vec![DebugModuleInfo::empty(); capacity.min(64)];
            let count = crate::driver::snapshot_loaded_modules(&mut modules);
            let bytes_len = count
                .checked_mul(size_of::<DebugModuleInfo>())
                .ok_or(DeviceError::InvalidArgument)?;
            if bytes_len != 0 {
                process_state
                    .address_space()
                    .validate_user_write_buffer(VirtAddr::new(request.modules_ptr), bytes_len)?;
                let bytes =
                    unsafe { slice::from_raw_parts(modules.as_ptr().cast::<u8>(), bytes_len) };
                process_state
                    .address_space()
                    .copy_into_user(VirtAddr::new(request.modules_ptr), bytes)?;
            }
            request.count = count as u64;
            write_user_struct(process_state.address_space(), arg, &request)?;
            Ok(0)
        }
        DEBUG_IOCTL_TRIGGER_BREAK => {
            let request =
                read_user_struct::<DebugBreakRequest>(process_state.address_space(), arg)?;
            debug::emit_text(
                DiagProvider::Debug,
                DiagLevel::Fatal,
                request.reason_code as u16,
                0,
                0,
                "deliberate breakpoint requested",
            );
            #[cfg(not(test))]
            unsafe {
                core::arch::asm!("int3", options(nomem, nostack, preserves_flags));
            }
            Ok(0)
        }
        _ => Err(DeviceError::Unsupported),
    }
}
