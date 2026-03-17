use x86_64::VirtAddr;

use crate::multitask;
use crate::paging::{self, ProcessAddressSpace};

pub(crate) fn current_user_address_space()
-> Result<&'static ProcessAddressSpace, paging::AddressSpaceError> {
    multitask::current_user_address_space().ok_or(paging::AddressSpaceError::NotMapped)
}

pub(crate) fn copy_from_current_user_exact(
    user_ptr: u64,
    dest: &mut [u8],
) -> Result<(), paging::AddressSpaceError> {
    let address_space = current_user_address_space()?;
    address_space.validate_user_read_buffer(VirtAddr::new(user_ptr), dest.len())?;
    address_space.copy_from_user(VirtAddr::new(user_ptr), dest)
}

pub(crate) fn write_current_user_bytes(
    user_ptr: u64,
    bytes: &[u8],
) -> Result<(), paging::AddressSpaceError> {
    let address_space = current_user_address_space()?;
    address_space.validate_user_write_buffer(VirtAddr::new(user_ptr), bytes.len())?;
    address_space.copy_into_user(VirtAddr::new(user_ptr), bytes)
}

pub(crate) fn write_current_user_u32(
    user_ptr: u64,
    value: u32,
) -> Result<(), paging::AddressSpaceError> {
    write_current_user_bytes(user_ptr, &value.to_le_bytes())
}

pub(crate) fn read_current_user_u32(user_ptr: u64) -> Result<u32, paging::AddressSpaceError> {
    let mut bytes = [0_u8; 4];
    copy_from_current_user_exact(user_ptr, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub(crate) fn read_current_user_c_string(
    user_ptr: u64,
    max_len: usize,
) -> Result<alloc::string::String, paging::AddressSpaceError> {
    let address_space = current_user_address_space()?;
    let mut bytes = alloc::vec::Vec::new();

    for offset in 0..max_len {
        let ptr = user_ptr
            .checked_add(offset as u64)
            .ok_or(paging::AddressSpaceError::AddressOverflow)?;
        let mut byte = [0_u8; 1];
        address_space.copy_from_user(VirtAddr::new(ptr), &mut byte)?;
        if byte[0] == 0 {
            return Ok(alloc::string::String::from_utf8_lossy(&bytes).into_owned());
        }
        bytes.push(byte[0]);
    }

    Err(paging::AddressSpaceError::AddressOverflow)
}
