use core::mem::size_of;
use core::slice;

use crate::memory::paging::AddressSpaceError;

use super::constants::{ERROR_INVALID_ADDRESS, ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY};

// RING3-MIGRATION-COMMENTED-OUT START: Win32 errno mapper + Windows ABI byte
// helpers belong in the Windows ABI user service.
/*
pub(super) fn address_space_error_to_win32(error: AddressSpaceError) -> u32 {
    match error {
        AddressSpaceError::OutOfFrames => ERROR_NOT_ENOUGH_MEMORY,
        AddressSpaceError::AddressOutOfRange
        | AddressSpaceError::NotMapped
        | AddressSpaceError::ProtectionViolation => ERROR_INVALID_ADDRESS,
        _ => ERROR_INVALID_PARAMETER,
    }
}

pub(super) fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

*/
// RING3-MIGRATION-COMMENTED-OUT END
