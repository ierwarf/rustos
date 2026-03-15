use crate::user::runtime_api;
use crate::user::runtime_api::{RuntimeApiError, RuntimeRequestError};

use super::usermem::current_user_address_space;

pub(crate) fn generation() -> u64 {
    runtime_api::generation()
}

pub(crate) fn snapshot_programs_to_current_process(
    user_ptr: u64,
    capacity: u64,
) -> Result<usize, RuntimeApiError> {
    let address_space =
        current_user_address_space().map_err(|err| RuntimeApiError::AddressSpace(err))?;
    runtime_api::snapshot_programs_to_user(address_space, user_ptr, capacity)
}

pub(crate) fn snapshot_running_programs_to_current_process(
    user_ptr: u64,
    capacity: u64,
) -> Result<usize, RuntimeApiError> {
    let address_space =
        current_user_address_space().map_err(|err| RuntimeApiError::AddressSpace(err))?;
    runtime_api::snapshot_running_programs_to_user(address_space, user_ptr, capacity)
}

pub(crate) fn request_launch(
    program_id: u64,
    target_kind: u64,
    target_value: u64,
) -> Result<(), RuntimeRequestError> {
    runtime_api::request_launch(program_id, target_kind, target_value)
}

pub(crate) fn request_terminate(
    target_kind: u64,
    target_value: u64,
) -> Result<(), RuntimeRequestError> {
    runtime_api::request_terminate(target_kind, target_value)
}
