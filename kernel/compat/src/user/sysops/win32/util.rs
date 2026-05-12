// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this old ring0 implementation as source material for userspace services; do not restore it without an explicit privileged-boundary decision.

// use core::mem::size_of;
// 
// use crate::memory::paging;
// 
// use super::constants::{ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY};
// 
// pub(super) fn address_space_error_to_win32(err: paging::AddressSpaceError) -> u32 {
//     match err {
//         paging::AddressSpaceError::ZeroSizedAllocation => ERROR_INVALID_PARAMETER,
//         paging::AddressSpaceError::AddressOverflow => ERROR_INVALID_PARAMETER,
//         paging::AddressSpaceError::AddressOutOfRange => ERROR_INVALID_PARAMETER,
//         paging::AddressSpaceError::AddressNotPageAligned => ERROR_INVALID_PARAMETER,
//         paging::AddressSpaceError::AlreadyMapped => ERROR_INVALID_PARAMETER,
//         paging::AddressSpaceError::NotMapped => ERROR_INVALID_PARAMETER,
//         paging::AddressSpaceError::ProtectionViolation => ERROR_INVALID_PARAMETER,
//         paging::AddressSpaceError::HugePageConflict => ERROR_INVALID_PARAMETER,
//         paging::AddressSpaceError::InvalidFrameOwnership => ERROR_INVALID_PARAMETER,
//         paging::AddressSpaceError::OutOfFrames => ERROR_NOT_ENOUGH_MEMORY,
//     }
// }
// 
// pub(super) fn as_bytes<T>(value: &T) -> &[u8] {
//     unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
// }
