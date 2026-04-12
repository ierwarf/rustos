use x86_64::VirtAddr;

use crate::memory::paging::ProcessAddressSpace;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ProcessStateError {
    InvalidPe(&'static str),
    AddressSpace(crate::memory::paging::AddressSpaceError),
}

impl From<crate::memory::paging::AddressSpaceError> for ProcessStateError {
    fn from(value: crate::memory::paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

#[repr(C)]
struct TebLite {
    _pad0: [u8; 0x40],
    client_id_unique_process: u64,
    client_id_unique_thread: u64,
}

pub fn initialize_windows_thread_identifiers(
    address_space: &mut ProcessAddressSpace,
    teb_address: u64,
    process_id: u64,
    thread_id: u64,
) -> Result<(), ProcessStateError> {
    let process_ptr = teb_address
        .checked_add(core::mem::offset_of!(TebLite, client_id_unique_process) as u64)
        .ok_or(ProcessStateError::InvalidPe(
            "Windows TEB process id pointer overflow",
        ))?;
    let thread_ptr = teb_address
        .checked_add(core::mem::offset_of!(TebLite, client_id_unique_thread) as u64)
        .ok_or(ProcessStateError::InvalidPe(
            "Windows TEB thread id pointer overflow",
        ))?;
    address_space.initialize_user_bytes(VirtAddr::new(process_ptr), &process_id.to_le_bytes())?;
    address_space.initialize_user_bytes(VirtAddr::new(thread_ptr), &thread_id.to_le_bytes())?;
    Ok(())
}
