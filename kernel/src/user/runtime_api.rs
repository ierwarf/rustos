use core::convert::TryFrom;
use core::mem::size_of;
use core::slice;

use x86_64::VirtAddr;

use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging;
use crate::user::abi::console::MAX_CONSOLE_SESSIONS;
use crate::user::abi::runtime::{
    LAUNCH_TARGET_ALL_SESSIONS, LAUNCH_TARGET_NEW_SESSION, LAUNCH_TARGET_SESSION,
    RuntimeProgramInfo, RuntimeRunningProgramInfo, TERMINATE_TARGET_ALL_SESSIONS,
    TERMINATE_TARGET_PID, TERMINATE_TARGET_SESSION,
};
use crate::user::runtime;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeApiError {
    AddressSpace(paging::AddressSpaceError),
    InvalidArgument,
}

#[derive(Debug)]
pub enum RuntimeRequestError {
    InvalidArgument,
    Runtime(runtime::DesktopRuntimeError),
}

const MAX_RUNTIME_SNAPSHOT_PROGRAMS: usize = 64;

impl From<paging::AddressSpaceError> for RuntimeApiError {
    fn from(value: paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

impl From<runtime::DesktopRuntimeError> for RuntimeRequestError {
    fn from(value: runtime::DesktopRuntimeError) -> Self {
        Self::Runtime(value)
    }
}

pub fn generation() -> u64 {
    runtime::presentation_generation()
}

pub fn snapshot_programs_to_user(
    address_space: &crate::memory::paging::ProcessAddressSpace,
    user_ptr: u64,
    capacity: u64,
) -> Result<usize, RuntimeApiError> {
    let capacity = usize::try_from(capacity).map_err(|_| RuntimeApiError::InvalidArgument)?;
    if capacity == 0 {
        return Ok(0);
    }

    let copy_capacity = capacity.min(MAX_RUNTIME_SNAPSHOT_PROGRAMS);
    let mut programs = [runtime::DesktopProgramInfo {
        id: runtime::DesktopProgramId::from_index(0),
        display_name: "",
        exec_path: "",
        weight_micros: 0,
    }; MAX_RUNTIME_SNAPSHOT_PROGRAMS];
    let count = runtime::snapshot_programs(&mut programs[..copy_capacity]).min(copy_capacity);
    let mut snapshot = [RuntimeProgramInfo::default(); MAX_RUNTIME_SNAPSHOT_PROGRAMS];
    for (dest, source) in snapshot.iter_mut().zip(programs.into_iter().take(count)) {
        dest.program_id = source.id.index() as u32;
        dest.weight_micros = source.weight_micros;
        dest.set_display_name(source.display_name);
        dest.set_exec_path(source.exec_path);
    }

    let bytes_len = count
        .checked_mul(size_of::<RuntimeProgramInfo>())
        .ok_or(RuntimeApiError::InvalidArgument)?;
    address_space.validate_user_write_buffer(VirtAddr::new(user_ptr), bytes_len)?;

    if count != 0 {
        let bytes = unsafe { slice::from_raw_parts(snapshot.as_ptr().cast::<u8>(), bytes_len) };
        address_space.copy_into_user(VirtAddr::new(user_ptr), bytes)?;
    }

    Ok(count)
}

pub fn snapshot_running_programs_to_user(
    address_space: &crate::memory::paging::ProcessAddressSpace,
    user_ptr: u64,
    capacity: u64,
) -> Result<usize, RuntimeApiError> {
    let capacity = usize::try_from(capacity).map_err(|_| RuntimeApiError::InvalidArgument)?;
    if capacity == 0 {
        return Ok(0);
    }

    let copy_capacity = capacity.min(MAX_CONSOLE_SESSIONS);
    let mut running = [runtime::DesktopRunningProgramInfo::default(); MAX_CONSOLE_SESSIONS];
    let count =
        runtime::snapshot_running_programs(&mut running[..copy_capacity]).min(copy_capacity);
    let mut snapshot = [RuntimeRunningProgramInfo::default(); MAX_CONSOLE_SESSIONS];
    for (dest, source) in snapshot.iter_mut().zip(running.into_iter().take(count)) {
        dest.pid = source.pid;
        dest.program_id = source.program_id.index() as u32;
        dest.session_handle = source.session_handle.raw();
        dest.set_display_name(source.display_name);
    }

    let bytes_len = count
        .checked_mul(size_of::<RuntimeRunningProgramInfo>())
        .ok_or(RuntimeApiError::InvalidArgument)?;
    address_space.validate_user_write_buffer(VirtAddr::new(user_ptr), bytes_len)?;

    if count != 0 {
        let bytes = unsafe { slice::from_raw_parts(snapshot.as_ptr().cast::<u8>(), bytes_len) };
        address_space.copy_into_user(VirtAddr::new(user_ptr), bytes)?;
    }

    Ok(count)
}

pub fn request_launch(
    program_id: u64,
    target_kind: u64,
    target_value: u64,
) -> Result<(), RuntimeRequestError> {
    let program_id = runtime::DesktopProgramId::from_index(program_id as usize);
    let target = match target_kind as u16 {
        LAUNCH_TARGET_SESSION => {
            let session = ConsoleSessionHandle::from_raw(target_value);
            runtime::DesktopLaunchTarget::Session(session)
        }
        LAUNCH_TARGET_NEW_SESSION => runtime::DesktopLaunchTarget::NewSession,
        LAUNCH_TARGET_ALL_SESSIONS => runtime::DesktopLaunchTarget::AllSessions,
        _ => return Err(RuntimeRequestError::InvalidArgument),
    };

    runtime::request_launch(program_id, target).map_err(Into::into)
}

pub fn request_terminate(target_kind: u64, target_value: u64) -> Result<(), RuntimeRequestError> {
    let target = match target_kind as u16 {
        TERMINATE_TARGET_SESSION => {
            runtime::DesktopTerminateTarget::Session(ConsoleSessionHandle::from_raw(target_value))
        }
        TERMINATE_TARGET_PID => runtime::DesktopTerminateTarget::ProcessId(target_value),
        TERMINATE_TARGET_ALL_SESSIONS => runtime::DesktopTerminateTarget::AllSessions,
        _ => return Err(RuntimeRequestError::InvalidArgument),
    };

    runtime::request_terminate(target).map_err(Into::into)
}
