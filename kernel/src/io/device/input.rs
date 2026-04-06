use core::mem::size_of;
use core::slice;

use crate::input::event_queue;
use crate::user::abi::device;
use crate::user::process_state::UserProcessState;
use crate::user::sysops::usermem;

use super::DeviceError;

const MAX_INPUT_EVENTS_PER_READ: usize = 1024;

pub fn read_events(dest: &mut [device::InputEvent]) -> usize {
    event_queue::read_input_events(dest)
}

pub(crate) fn read_to_user(
    process_state: &mut UserProcessState,
    user_ptr: u64,
    user_len: usize,
) -> Result<usize, DeviceError> {
    read_events_to_user(
        |bytes_len| {
            process_state
                .address_space()
                .validate_user_write_buffer(x86_64::VirtAddr::new(user_ptr), bytes_len)
                .map_err(DeviceError::AddressSpace)
        },
        |bytes| {
            process_state
                .address_space()
                .copy_into_user(x86_64::VirtAddr::new(user_ptr), bytes)
                .map_err(DeviceError::AddressSpace)
        },
        user_len,
    )
}

pub(crate) fn read_to_current_user(user_ptr: u64, user_len: usize) -> Result<usize, DeviceError> {
    read_events_to_user(
        |bytes_len| {
            usermem::current_user_address_space()
                .map_err(DeviceError::AddressSpace)?
                .address_space()
                .validate_user_write_buffer(x86_64::VirtAddr::new(user_ptr), bytes_len)
                .map_err(DeviceError::AddressSpace)
        },
        |bytes| {
            usermem::write_current_user_bytes(user_ptr, bytes).map_err(DeviceError::AddressSpace)
        },
        user_len,
    )
}

fn read_events_to_user(
    validate: impl FnOnce(usize) -> Result<(), DeviceError>,
    write: impl FnOnce(&[u8]) -> Result<(), DeviceError>,
    user_len: usize,
) -> Result<usize, DeviceError> {
    let event_size = size_of::<device::InputEvent>();
    let capacity = user_len / event_size;
    if capacity == 0 {
        return Ok(0);
    }

    let capacity = capacity.min(MAX_INPUT_EVENTS_PER_READ);
    let mut events = [device::InputEvent::default(); MAX_INPUT_EVENTS_PER_READ];
    let read = read_events(&mut events[..capacity]);
    let bytes_len = read
        .checked_mul(event_size)
        .ok_or(DeviceError::InvalidArgument)?;

    validate(bytes_len)?;
    if read != 0 {
        let bytes = unsafe { slice::from_raw_parts(events.as_ptr().cast::<u8>(), bytes_len) };
        write(bytes)?;
    }
    Ok(bytes_len)
}
