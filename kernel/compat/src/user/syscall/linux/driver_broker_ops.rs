use super::*;

use alloc::string::String;
use rustos_user_abi::syscall::{
    DRIVER_BROKER_ALIAS_CAPACITY, DRIVER_BROKER_NAME_CAPACITY, DRIVER_BROKER_PATH_CAPACITY,
    IPC_SERVICE_CAP_DRIVER_POLICY, RustosDriverLoadModuleBrokerArgs,
    RustosDriverProbeAliasBrokerArgs, RustosDriverProviderActiveBrokerArgs,
};

pub(super) fn syscall_linux_rustos_driver_load_module_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_DRIVER_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<RustosDriverLoadModuleBrokerArgs>(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }

    let name = match read_driver_text(args.name_ptr, args.name_len, DRIVER_BROKER_NAME_CAPACITY) {
        Ok(value) => value,
        Err(errno) => return linux_errno(errno),
    };
    let path = match read_driver_text(args.path_ptr, args.path_len, DRIVER_BROKER_PATH_CAPACITY) {
        Ok(value) => value,
        Err(errno) => return linux_errno(errno),
    };
    let linux_driver_names = match read_driver_text(
        args.linux_driver_names_ptr,
        args.linux_driver_names_len,
        DRIVER_BROKER_NAME_CAPACITY,
    ) {
        Ok(value) => value,
        Err(errno) => return linux_errno(errno),
    };

    match kernel_io_manager::driver::load_module_image_from_policy(
        name.as_str(),
        args.class,
        args.bus,
        path.as_str(),
        linux_driver_names.as_str(),
    ) {
        Ok(()) => 0,
        Err(_) => linux_errno(LINUX_EIO),
    }
}

pub(super) fn syscall_linux_rustos_driver_probe_alias_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_DRIVER_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<RustosDriverProbeAliasBrokerArgs>(args_ptr)
    {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let alias = match read_driver_text(args.alias_ptr, args.alias_len, DRIVER_BROKER_ALIAS_CAPACITY)
    {
        Ok(value) => value,
        Err(errno) => return linux_errno(errno),
    };

    if kernel_io_manager::driver::device_alias_present_from_policy(
        alias.as_str(),
        args.class,
        args.bus,
    ) {
        1
    } else {
        0
    }
}

pub(super) fn syscall_linux_rustos_driver_provider_active_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_DRIVER_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args =
        match usermem::read_current_user_struct::<RustosDriverProviderActiveBrokerArgs>(args_ptr) {
            Ok(args) => args,
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
    if args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let group = match read_driver_text(
        args.provider_group_ptr,
        args.provider_group_len,
        DRIVER_BROKER_NAME_CAPACITY,
    ) {
        Ok(value) => value,
        Err(errno) => return linux_errno(errno),
    };

    if kernel_io_manager::driver::provider_group_active_from_policy(group.as_str()) {
        1
    } else {
        0
    }
}

fn read_driver_text(ptr: u64, len: u64, max_len: usize) -> Result<String, i64> {
    let len = usize::try_from(len).map_err(|_| LINUX_EINVAL)?;
    if ptr == 0 || len == 0 || len > max_len {
        return Err(LINUX_EINVAL);
    }
    let mut bytes = alloc::vec![0_u8; len];
    usermem::copy_from_current_user_exact(ptr, &mut bytes)
        .map_err(address_space_error_to_linux_errno)?;
    if bytes.contains(&0) {
        return Err(LINUX_EINVAL);
    }
    String::from_utf8(bytes).map_err(|_| LINUX_EINVAL)
}
