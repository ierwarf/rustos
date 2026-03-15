use alloc::vec;
use core::mem::{align_of, size_of};
use core::slice;

use crate::io::ui_service;
use crate::user::abi::{device, ui};
use crate::user::process_state::UserProcessState;

use super::DeviceError;

const _: [(); size_of::<device::InputEvent>()] = [(); size_of::<ui::UiInputEvent>()];
const _: [(); align_of::<device::InputEvent>()] = [(); align_of::<ui::UiInputEvent>()];
const MAX_INPUT_EVENTS_PER_READ: usize = 64;

pub fn read_events(dest: &mut [device::InputEvent]) -> usize {
    let legacy_dest = unsafe {
        slice::from_raw_parts_mut(dest.as_mut_ptr().cast::<ui::UiInputEvent>(), dest.len())
    };
    ui_service::read_input_events(legacy_dest)
}

pub(crate) fn read_to_user(
    process_state: &mut UserProcessState,
    user_ptr: u64,
    user_len: usize,
) -> Result<usize, DeviceError> {
    let event_size = size_of::<device::InputEvent>();
    let capacity = user_len / event_size;
    if capacity == 0 {
        return Ok(0);
    }

    let capacity = capacity.min(MAX_INPUT_EVENTS_PER_READ);
    let mut events = vec![device::InputEvent::default(); capacity];
    let read = read_events(&mut events);
    let bytes_len = read
        .checked_mul(event_size)
        .ok_or(DeviceError::InvalidArgument)?;

    let address_space = process_state.address_space();
    address_space.validate_user_write_buffer(x86_64::VirtAddr::new(user_ptr), bytes_len)?;
    if read != 0 {
        let bytes = unsafe { slice::from_raw_parts(events.as_ptr().cast::<u8>(), bytes_len) };
        address_space.copy_into_user(x86_64::VirtAddr::new(user_ptr), bytes)?;
    }
    Ok(bytes_len)
}
