use alloc::vec::Vec;
use core::mem::size_of;
use core::slice;

use crate::input::event_queue;
use crate::user::abi::device;
use crate::user::process_state::UserProcessState;
use crate::user::sysops::usermem;

use super::DeviceError;

const MAX_INPUT_EVENTS_PER_READ: usize = 1024;
const MAX_EVDEV_EVENTS_PER_READ: usize =
    MAX_INPUT_EVENTS_PER_READ * crate::input_core::MAX_EVDEV_EVENTS_PER_INPUT_EVENT;

// RING3-MIGRATION-REFERENCE START: inputd should own input device read policy,
// reader state, native/evdev translation, and buffer sizing. Ring0 keeps the
// current-process user-copy broker used to deliver already-authorized bytes.
pub fn read_events(dest: &mut [device::InputEvent]) -> usize {
    event_queue::read_input_events(dest)
}

pub fn has_pending_events() -> bool {
    event_queue::has_pending_input_events()
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

pub(crate) fn read_evdev_to_user(
    process_state: &mut UserProcessState,
    user_ptr: u64,
    user_len: usize,
) -> Result<usize, DeviceError> {
    read_evdev_events_to_user(
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

pub(crate) fn read_evdev_to_current_user(
    user_ptr: u64,
    user_len: usize,
) -> Result<usize, DeviceError> {
    read_evdev_events_to_user(
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
    let max_bytes_len = capacity
        .checked_mul(event_size)
        .ok_or(DeviceError::InvalidArgument)?;
    validate(max_bytes_len)?;

    let mut events = input_event_scratch(capacity)?;
    let read = read_events(&mut events[..capacity]);
    let bytes_len = read
        .checked_mul(event_size)
        .ok_or(DeviceError::InvalidArgument)?;

    if read != 0 {
        let bytes = unsafe { slice::from_raw_parts(events.as_ptr().cast::<u8>(), bytes_len) };
        write(bytes)?;
    }
    Ok(bytes_len)
}

fn read_evdev_events_to_user(
    validate: impl FnOnce(usize) -> Result<(), DeviceError>,
    write: impl FnOnce(&[u8]) -> Result<(), DeviceError>,
    user_len: usize,
) -> Result<usize, DeviceError> {
    let event_size = size_of::<LinuxInputEvent>();
    let output_capacity = (user_len / event_size).min(MAX_EVDEV_EVENTS_PER_READ);
    if output_capacity < 3 {
        return Ok(0);
    }

    let input_capacity = (output_capacity / 3).min(MAX_INPUT_EVENTS_PER_READ);
    let max_bytes_len = output_capacity
        .checked_mul(event_size)
        .ok_or(DeviceError::InvalidArgument)?;
    validate(max_bytes_len)?;

    let mut input_events = input_event_scratch(input_capacity)?;
    let read = read_events(&mut input_events[..input_capacity]);
    if read == 0 {
        return Ok(0);
    }

    let mut output = evdev_event_scratch(output_capacity)?;
    let written = crate::input_core::translate_input_events_to_evdev(
        abi_events_as_input_core(&mut input_events[..read]),
        &mut output,
    )
    .map_err(|_| DeviceError::InvalidArgument)?;
    let bytes_len = written
        .checked_mul(event_size)
        .ok_or(DeviceError::InvalidArgument)?;
    let bytes = unsafe { slice::from_raw_parts(output.as_ptr().cast::<u8>(), bytes_len) };
    write(bytes)?;
    Ok(bytes_len)
}

fn input_event_scratch(capacity: usize) -> Result<Vec<device::InputEvent>, DeviceError> {
    let mut events = Vec::new();
    events
        .try_reserve_exact(capacity)
        .map_err(|_| allocation_error())?;
    events.resize(capacity, device::InputEvent::default());
    Ok(events)
}

fn evdev_event_scratch(capacity: usize) -> Result<Vec<LinuxInputEvent>, DeviceError> {
    let mut events = Vec::new();
    events
        .try_reserve_exact(capacity)
        .map_err(|_| allocation_error())?;
    events.resize(capacity, LinuxInputEvent::default());
    Ok(events)
}

fn allocation_error() -> DeviceError {
    DeviceError::AddressSpace(crate::memory::paging::AddressSpaceError::OutOfFrames)
}

type LinuxInputEvent = crate::input_core::LinuxInputEvent;

fn abi_events_as_input_core(
    dest: &mut [device::InputEvent],
) -> &mut [crate::input_core::InputEvent] {
    unsafe { slice::from_raw_parts_mut(dest.as_mut_ptr().cast(), dest.len()) }
}
// RING3-MIGRATION-REFERENCE END: inputd-owned input device read policy.
