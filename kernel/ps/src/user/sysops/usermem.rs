use x86_64::VirtAddr;

use crate::memory::paging;
use crate::multitask;

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
        address_space.validate_user_read_buffer(VirtAddr::new(user_ptr), dest.len())?;
        address_space.copy_from_user(VirtAddr::new(user_ptr), dest)
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
        address_space.validate_user_write_buffer(VirtAddr::new(user_ptr), bytes.len())?;
        address_space.copy_into_user(VirtAddr::new(user_ptr), bytes)
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
    })
}
