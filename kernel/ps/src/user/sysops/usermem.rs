//! Exact-generation user-memory copy and range admission.
//!
//! - **Owner:** `kernel-ps` owns process-bound user-copy substrate.
//! - **Boundary:** Every user pointer, length, direction, and typed layout is
//!   untrusted.
//! - **Lifecycle:** Retain the process generation, validate the complete live
//!   page span, copy all bytes or none, then release the hold.
//! - **Concurrency:** Exec/exit cannot reclaim the retained address space while
//!   a copy is active; no service call occurs under page-table access.
//! - **Failure:** Noncanonical, overflowing, unmapped, permission, alignment,
//!   and short-span errors return before caller state publication.
//! - **Forbidden:** No raw slice construction before admission, partial
//!   authority copy, or foreign-process access by PID alone.
//! - **Evidence:** `user-memory-access`.
use x86_64::VirtAddr;

use crate::memory::paging;
use crate::multitask;

const USER_C_STRING_COPY_CHUNK: usize = 256;
const USER_PAGE_SIZE: usize = 4096;

#[track_caller]
pub fn current_user_address_space()
-> Result<multitask::RetainedCurrentUserAddressSpace, paging::AddressSpaceError> {
    multitask::current_user_address_space().ok_or(paging::AddressSpaceError::NotMapped)
}

#[track_caller]
fn with_current_address_space<R>(
    f: impl FnOnce(&paging::ProcessAddressSpace) -> Result<R, paging::AddressSpaceError>,
) -> Result<R, paging::AddressSpaceError> {
    multitask::with_current_mm(f).ok_or(paging::AddressSpaceError::NotMapped)?
}

pub fn copy_from_current_user_exact(
    user_ptr: u64,
    dest: &mut [u8],
) -> Result<(), paging::AddressSpaceError> {
    with_current_address_space(|address_space| {
        let start = user_virt_addr(user_ptr, dest.len())?;
        address_space.validate_user_read_buffer(start, dest.len())?;
        address_space.copy_from_user(start, dest)
    })
}

pub fn read_current_user_struct<T: Copy + Default>(
    user_ptr: u64,
) -> Result<T, paging::AddressSpaceError> {
    let mut value = T::default();
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(value).cast::<u8>(),
            core::mem::size_of::<T>(),
        )
    };
    copy_from_current_user_exact(user_ptr, bytes)?;
    Ok(value)
}

pub fn write_current_user_bytes(
    user_ptr: u64,
    bytes: &[u8],
) -> Result<(), paging::AddressSpaceError> {
    with_current_address_space(|address_space| {
        let start = user_virt_addr(user_ptr, bytes.len())?;
        address_space.validate_user_write_buffer(start, bytes.len())?;
        address_space.copy_into_user(start, bytes)
    })
}

pub fn validate_current_user_write_buffer(
    user_ptr: u64,
    len: usize,
) -> Result<(), paging::AddressSpaceError> {
    with_current_address_space(|address_space| {
        address_space.validate_user_write_buffer(user_virt_addr(user_ptr, len)?, len)
    })
}

pub fn write_current_user_struct<T: Copy>(
    user_ptr: u64,
    value: &T,
) -> Result<(), paging::AddressSpaceError> {
    let bytes = unsafe {
        core::slice::from_raw_parts(
            core::ptr::addr_of!(*value).cast::<u8>(),
            core::mem::size_of::<T>(),
        )
    };
    write_current_user_bytes(user_ptr, bytes)
}

pub fn write_current_user_u32(user_ptr: u64, value: u32) -> Result<(), paging::AddressSpaceError> {
    write_current_user_bytes(user_ptr, &value.to_le_bytes())
}

pub fn read_current_user_u32(user_ptr: u64) -> Result<u32, paging::AddressSpaceError> {
    let mut bytes = [0_u8; 4];
    copy_from_current_user_exact(user_ptr, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub fn read_current_user_c_string(
    user_ptr: u64,
    max_len: usize,
) -> Result<alloc::string::String, paging::AddressSpaceError> {
    with_current_address_space(|address_space| {
        let mut bytes = alloc::vec::Vec::new();
        let mut chunk = [0_u8; USER_C_STRING_COPY_CHUNK];

        while bytes.len() < max_len {
            let ptr = user_ptr
                .checked_add(bytes.len() as u64)
                .ok_or(paging::AddressSpaceError::AddressOverflow)?;
            let page_remaining = USER_PAGE_SIZE - ((ptr as usize) & (USER_PAGE_SIZE - 1));
            let chunk_len = (max_len - bytes.len()).min(chunk.len()).min(page_remaining);
            let start = user_virt_addr(ptr, chunk_len)?;
            address_space.copy_from_user(start, &mut chunk[..chunk_len])?;
            if let Some(nul_index) = chunk[..chunk_len].iter().position(|byte| *byte == 0) {
                bytes.extend_from_slice(&chunk[..nul_index]);
                return Ok(alloc::string::String::from_utf8_lossy(&bytes).into_owned());
            }
            bytes.extend_from_slice(&chunk[..chunk_len]);
        }

        Err(paging::AddressSpaceError::AddressOverflow)
    })
}

fn user_virt_addr(user_ptr: u64, len: usize) -> Result<VirtAddr, paging::AddressSpaceError> {
    if len == 0 {
        return Ok(VirtAddr::new(paging::USER_SPACE_BASE));
    }
    let last = user_ptr
        .checked_add(len as u64 - 1)
        .ok_or(paging::AddressSpaceError::AddressOverflow)?;
    if user_ptr < paging::USER_SPACE_BASE || last >= paging::USER_SPACE_END_EXCLUSIVE {
        return Err(paging::AddressSpaceError::AddressOutOfRange);
    }
    Ok(VirtAddr::new(user_ptr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_virt_addr_rejects_out_of_range_without_panicking() {
        assert_eq!(
            user_virt_addr(paging::USER_SPACE_BASE, 1).map(VirtAddr::as_u64),
            Ok(paging::USER_SPACE_BASE)
        );
        assert_eq!(
            user_virt_addr(paging::USER_SPACE_END_EXCLUSIVE, 1),
            Err(paging::AddressSpaceError::AddressOutOfRange)
        );
        assert_eq!(
            user_virt_addr(u64::MAX, 8),
            Err(paging::AddressSpaceError::AddressOverflow)
        );
    }
}
