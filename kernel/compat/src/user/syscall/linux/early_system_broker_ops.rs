// RING3-MIGRATION-REFERENCE START: immutable early-system bootstrap substrate.
// Vfsd owns namespace/open-description policy. Ring0 exposes only exact
// digest-admitted early-system entries and retires no physical-storage path.
use super::*;

use alloc::vec::Vec;
use kernel_io_manager::api::block as block_api;
use rustos_user_abi::syscall::{ServiceCapability,
    EARLY_SYSTEM_BROKER_ABI_VERSION, EARLY_SYSTEM_BROKER_MAX_IO_BYTES, EARLY_SYSTEM_BROKER_OP_INFO,
    EARLY_SYSTEM_BROKER_OP_READ, EarlySystemBrokerArgs, };

pub(super) fn syscall_linux_rustos_early_system_broker(args_ptr: u64) -> u64 {
    linux_result((|| {
        let args: EarlySystemBrokerArgs =
            require_capability_and_read_args(ServiceCapability::VfsPolicy, args_ptr)?;
        let path_len = usize::try_from(args.path_len).unwrap_or(usize::MAX);
        if args.abi_version != EARLY_SYSTEM_BROKER_ABI_VERSION
            || args.reserved0 != 0
            || path_len == 0
            || path_len > args.path.len()
            || args.path[path_len..].iter().any(|byte| *byte != 0)
        {
            return Err(LINUX_EINVAL);
        }
        let path = core::str::from_utf8(&args.path[..path_len]).map_err(|_| LINUX_EINVAL)?;
        match args.op {
            EARLY_SYSTEM_BROKER_OP_INFO => broker_info(&args, path),
            EARLY_SYSTEM_BROKER_OP_READ => broker_read(&args, path),
            _ => Err(LINUX_EINVAL),
        }
    })())
}

fn broker_info(args: &EarlySystemBrokerArgs, path: &str) -> LinuxResult<u64> {
    if args.offset != 0
        || args.buffer_ptr != 0
        || args.buffer_len != 0
        || args.out_file_len_ptr == 0
    {
        return Err(LINUX_EINVAL);
    }
    let len = block_api::bootstrap_file_len(path).map_err(bootstrap_error_to_linux_errno)?;
    write_user_bytes(args.out_file_len_ptr, &len.to_ne_bytes())?;
    Ok(0)
}

fn broker_read(args: &EarlySystemBrokerArgs, path: &str) -> LinuxResult<u64> {
    if args.out_file_len_ptr != 0
        || args.buffer_ptr == 0
        || args.buffer_len == 0
        || args.buffer_len > EARLY_SYSTEM_BROKER_MAX_IO_BYTES as u64
    {
        return Err(LINUX_EINVAL);
    }
    let len = usize::try_from(args.buffer_len).map_err(|_| LINUX_EINVAL)?;
    validate_user_write(args.buffer_ptr, len)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|_| LINUX_ENOMEM)?;
    bytes.resize(len, 0);
    let read = block_api::read_bootstrap_file_range(path, args.offset, &mut bytes)
        .map_err(bootstrap_error_to_linux_errno)?
        .ok_or(LINUX_ENOENT)?;
    write_user_bytes(args.buffer_ptr, &bytes[..read])?;
    Ok(read as u64)
}

fn bootstrap_error_to_linux_errno(
    error: kernel_io_manager::api::BootstrapImageError,
) -> i64 {
    match error {
        kernel_io_manager::api::BootstrapImageError::NotFound => LINUX_ENOENT,
        kernel_io_manager::api::BootstrapImageError::Unavailable
        | kernel_io_manager::api::BootstrapImageError::Invalid => LINUX_EIO,
    }
}
// RING3-MIGRATION-REFERENCE END: immutable early-system bootstrap substrate.
