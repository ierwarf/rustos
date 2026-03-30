use alloc::string::String;
use alloc::vec::Vec;

use x86_64::VirtAddr;

use crate::multitask;
use crate::user::abi::UserAbi;
use crate::user::linux as linux_abi;
use crate::user::process::{self, ProcessLaunchOptions, ProcessLoadError};
use crate::user::windows::WindowsProcessLaunch;

use super::{file, usermem, LinuxExecTransition, LinuxSysopError};

const MAX_EXEC_PATH_LEN: usize = 256;
const MAX_EXEC_ARG_COUNT: usize = 256;
const MAX_EXEC_ENV_COUNT: usize = 256;
const MAX_EXEC_TOTAL_STRING_BYTES: usize = 64 * 1024;

pub(crate) fn execve(
    path_ptr: u64,
    argv_ptr: u64,
    envp_ptr: u64,
) -> Result<LinuxExecTransition, LinuxSysopError> {
    execveat(
        linux_abi::AT_FDCWD as i64 as u64,
        path_ptr,
        argv_ptr,
        envp_ptr,
        0,
    )
}

pub(crate) fn execveat(
    dirfd: u64,
    path_ptr: u64,
    argv_ptr: u64,
    envp_ptr: u64,
    flags: u64,
) -> Result<LinuxExecTransition, LinuxSysopError> {
    if flags != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let path = usermem::read_current_user_c_string(path_ptr, MAX_EXEC_PATH_LEN)?;
    if path.is_empty() {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let absolute_path = file::resolve_path_for_current_process(dirfd, &path)?;
    let argv = read_exec_string_array(argv_ptr, MAX_EXEC_ARG_COUNT, MAX_EXEC_TOTAL_STRING_BYTES)?;
    let env = read_exec_string_array(
        envp_ptr,
        MAX_EXEC_ENV_COUNT,
        MAX_EXEC_TOTAL_STRING_BYTES.saturating_sub(argv.used_bytes),
    )?;
    let image = file::read_path_for_current_process_to_vec(absolute_path.as_str())?;
    let logical_admin = current_process_logical_admin()?;

    let argv_refs = argv.values.iter().map(String::as_str).collect::<Vec<_>>();
    let env_refs = env.values.iter().map(String::as_str).collect::<Vec<_>>();
    let launch = ProcessLaunchOptions {
        linux: crate::user::linux::LinuxProcessLaunch {
            exec_path: absolute_path.as_str(),
            argv: &argv_refs,
            env: &env_refs,
        },
        windows: WindowsProcessLaunch {
            exec_path: absolute_path.as_str(),
            argv: &argv_refs,
            env: &env_refs,
        },
        console_session: multitask::current_console_session(),
        logical_admin,
        ..ProcessLaunchOptions::default()
    };

    let prepared = process::prepare_process_with_launch(image.as_slice(), launch)
        .map_err(map_process_load_error_to_linux)?;
    if prepared.abi != UserAbi::Linux {
        return Err(LinuxSysopError::ExecFormat);
    }

    let transition = LinuxExecTransition {
        user_rip: prepared.bootstrap.entry.as_u64(),
        user_rsp: prepared.bootstrap.stack_pointer.as_u64(),
        registers: prepared.bootstrap.registers,
    };
    if !multitask::exec_current_user_process(prepared.address_space, prepared.bootstrap) {
        return Err(LinuxSysopError::Unsupported);
    }

    Ok(transition)
}

fn current_process_logical_admin() -> Result<bool, LinuxSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        Ok(process_state.security().is_logical_admin())
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

struct ExecStringArray {
    values: Vec<String>,
    used_bytes: usize,
}

fn read_exec_string_array(
    array_ptr: u64,
    max_count: usize,
    max_total_bytes: usize,
) -> Result<ExecStringArray, LinuxSysopError> {
    if array_ptr == 0 {
        return Ok(ExecStringArray {
            values: Vec::new(),
            used_bytes: 0,
        });
    }

    let address_space =
        usermem::current_user_address_space().map_err(LinuxSysopError::AddressSpace)?;
    let mut values = Vec::new();
    let mut used_bytes = 0usize;
    for index in 0..max_count {
        let pointer_ptr = array_ptr
            .checked_add((index * core::mem::size_of::<u64>()) as u64)
            .ok_or(LinuxSysopError::AddressSpace(
                crate::memory::paging::AddressSpaceError::AddressOverflow,
            ))?;
        let mut pointer_bytes = [0_u8; 8];
        address_space.copy_from_user(VirtAddr::new(pointer_ptr), &mut pointer_bytes)?;
        let user_ptr = u64::from_le_bytes(pointer_bytes);
        if user_ptr == 0 {
            return Ok(ExecStringArray { values, used_bytes });
        }

        let remaining = max_total_bytes.saturating_sub(used_bytes);
        let (value, value_bytes) = read_exec_string(user_ptr, remaining)?;
        used_bytes = used_bytes
            .checked_add(value_bytes)
            .ok_or(LinuxSysopError::TooBig)?;
        values.push(value);
    }

    Err(LinuxSysopError::TooBig)
}

fn read_exec_string(user_ptr: u64, max_len: usize) -> Result<(String, usize), LinuxSysopError> {
    if max_len == 0 {
        return Err(LinuxSysopError::TooBig);
    }

    let address_space =
        usermem::current_user_address_space().map_err(LinuxSysopError::AddressSpace)?;
    let mut bytes = Vec::new();
    for offset in 0..max_len {
        let current_ptr =
            user_ptr
                .checked_add(offset as u64)
                .ok_or(LinuxSysopError::AddressSpace(
                    crate::memory::paging::AddressSpaceError::AddressOverflow,
                ))?;
        let mut byte = [0_u8; 1];
        address_space.copy_from_user(VirtAddr::new(current_ptr), &mut byte)?;
        if byte[0] == 0 {
            return Ok((
                String::from_utf8_lossy(&bytes).into_owned(),
                bytes.len() + 1,
            ));
        }
        bytes.push(byte[0]);
    }

    Err(LinuxSysopError::TooBig)
}

fn map_process_load_error_to_linux(err: ProcessLoadError) -> LinuxSysopError {
    match err {
        ProcessLoadError::InvalidElf(_)
        | ProcessLoadError::InvalidPe(_)
        | ProcessLoadError::UnsupportedImport { .. } => LinuxSysopError::ExecFormat,
        ProcessLoadError::InterpreterLoad { .. } => LinuxSysopError::NotFound,
        ProcessLoadError::AddressSpace(address_space) => {
            LinuxSysopError::AddressSpace(address_space)
        }
        ProcessLoadError::Spawn(spawn) => match spawn {
            crate::multitask::SpawnTaskError::InvalidWeightMicros => {
                LinuxSysopError::InvalidArgument
            }
            crate::multitask::SpawnTaskError::NoFreeTaskSlot => LinuxSysopError::TryAgain,
        },
    }
}
