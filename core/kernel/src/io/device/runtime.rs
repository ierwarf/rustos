use crate::user::abi::runtime::{
    self, RuntimeGenerationInfo, RuntimeLaunchRequest, RuntimeSnapshotProgramsRequest,
    RuntimeSnapshotRunningProgramsRequest, RuntimeTerminateRequest,
};
use crate::user::process_state::UserProcessState;
use crate::user::runtime::DesktopRuntimeError;
use crate::user::runtime_api;

use super::{read_user_struct, write_user_struct, DeviceError};

pub(crate) fn ioctl(
    process_state: &mut UserProcessState,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceError> {
    match request {
        runtime::RUNTIME_IOCTL_GET_GENERATION => {
            let info = RuntimeGenerationInfo {
                generation: runtime_api::generation(),
            };
            write_user_struct(process_state.address_space(), arg, &info)?;
            Ok(0)
        }
        runtime::RUNTIME_IOCTL_SNAPSHOT_PROGRAMS => {
            let mut snapshot = read_user_struct::<RuntimeSnapshotProgramsRequest>(
                process_state.address_space(),
                arg,
            )?;
            let count = runtime_api::snapshot_programs_to_user(
                process_state.address_space(),
                snapshot.programs_ptr,
                snapshot.capacity,
            )
            .map_err(map_runtime_api_error)?;
            snapshot.count = count as u64;
            write_user_struct(process_state.address_space(), arg, &snapshot)?;
            Ok(0)
        }
        runtime::RUNTIME_IOCTL_SNAPSHOT_RUNNING_PROGRAMS => {
            let mut snapshot = read_user_struct::<RuntimeSnapshotRunningProgramsRequest>(
                process_state.address_space(),
                arg,
            )?;
            let count = runtime_api::snapshot_running_programs_to_user(
                process_state.address_space(),
                snapshot.programs_ptr,
                snapshot.capacity,
            )
            .map_err(map_runtime_api_error)?;
            snapshot.count = count as u64;
            write_user_struct(process_state.address_space(), arg, &snapshot)?;
            Ok(0)
        }
        runtime::RUNTIME_IOCTL_REQUEST_LAUNCH => {
            let launch =
                read_user_struct::<RuntimeLaunchRequest>(process_state.address_space(), arg)?;
            runtime_api::request_launch(
                launch.program_id,
                u64::from(launch.target_kind),
                launch.target_value,
            )
            .map_err(map_runtime_request_error)?;
            Ok(0)
        }
        runtime::RUNTIME_IOCTL_REQUEST_TERMINATE => {
            let terminate =
                read_user_struct::<RuntimeTerminateRequest>(process_state.address_space(), arg)?;
            runtime_api::request_terminate(
                u64::from(terminate.target_kind),
                terminate.target_value,
            )
            .map_err(map_runtime_request_error)?;
            Ok(0)
        }
        _ => Err(DeviceError::Unsupported),
    }
}

fn map_runtime_api_error(err: runtime_api::RuntimeApiError) -> DeviceError {
    match err {
        runtime_api::RuntimeApiError::AddressSpace(err) => DeviceError::AddressSpace(err),
        runtime_api::RuntimeApiError::InvalidArgument => DeviceError::InvalidArgument,
    }
}

fn map_runtime_request_error(err: runtime_api::RuntimeRequestError) -> DeviceError {
    match err {
        runtime_api::RuntimeRequestError::InvalidArgument => DeviceError::InvalidArgument,
        runtime_api::RuntimeRequestError::Runtime(runtime_err) => match runtime_err {
            DesktopRuntimeError::AlreadyBootstrapped
            | DesktopRuntimeError::InvalidProgramWeight { .. } => DeviceError::InvalidArgument,
            DesktopRuntimeError::Load { .. } | DesktopRuntimeError::ProgramNotFound { .. } => {
                DeviceError::NotFound
            }
            DesktopRuntimeError::Registry { .. } => DeviceError::NotFound,
            DesktopRuntimeError::RequestQueueFull
            | DesktopRuntimeError::NoAvailableSession
            | DesktopRuntimeError::SessionBusy { .. } => DeviceError::Busy,
            DesktopRuntimeError::SessionNotFound { .. } => DeviceError::NotFound,
        },
    }
}
