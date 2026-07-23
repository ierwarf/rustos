// RING3-MIGRATION-REFERENCE START: immutable early-system bootstrap substrate.
// Vfsd owns namespace/open-description policy. Ring0 exposes only exact
// digest-admitted early-system entries and retires no physical-storage path.
use super::*;

use alloc::vec::Vec;
use kernel_io_manager::api::block as block_api;
use rustos_user_abi::syscall::{
    EARLY_SYSTEM_BROKER_ABI_VERSION, EARLY_SYSTEM_BROKER_MAX_IO_BYTES, EARLY_SYSTEM_BROKER_OP_INFO,
    EARLY_SYSTEM_BROKER_OP_READ, EarlySystemBrokerArgs, IPC_SERVICE_CAP_VFS_POLICY,
};

pub(super) fn syscall_linux_rustos_early_system_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_VFS_POLICY) {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<EarlySystemBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(error) => return linux_errno(address_space_error_to_linux_errno(error)),
    };
    let path_len = usize::try_from(args.path_len).unwrap_or(usize::MAX);
    if args.abi_version != EARLY_SYSTEM_BROKER_ABI_VERSION
        || args.reserved0 != 0
        || path_len == 0
        || path_len > args.path.len()
        || args.path[path_len..].iter().any(|byte| *byte != 0)
    {
        return linux_errno(LINUX_EINVAL);
    }
    let path = match core::str::from_utf8(&args.path[..path_len]) {
        Ok(path) => path,
        Err(_) => return linux_errno(LINUX_EINVAL),
    };
    match args.op {
        EARLY_SYSTEM_BROKER_OP_INFO => broker_info(&args, path),
        EARLY_SYSTEM_BROKER_OP_READ => broker_read(&args, path),
        _ => linux_errno(LINUX_EINVAL),
    }
}

fn broker_info(args: &EarlySystemBrokerArgs, path: &str) -> u64 {
    if args.offset != 0
        || args.buffer_ptr != 0
        || args.buffer_len != 0
        || args.out_file_len_ptr == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    let len = match block_api::bootstrap_file_len(path) {
        Ok(len) => len,
        Err(error) => return linux_errno(bootstrap_error_to_linux_errno(error)),
    };
    match usermem::write_current_user_bytes(args.out_file_len_ptr, &len.to_ne_bytes()) {
        Ok(()) => 0,
        Err(error) => linux_errno(address_space_error_to_linux_errno(error)),
    }
}

fn broker_read(args: &EarlySystemBrokerArgs, path: &str) -> u64 {
    if args.out_file_len_ptr != 0
        || args.buffer_ptr == 0
        || args.buffer_len == 0
        || args.buffer_len > EARLY_SYSTEM_BROKER_MAX_IO_BYTES as u64
    {
        return linux_errno(LINUX_EINVAL);
    }
    let len = match usize::try_from(args.buffer_len) {
        Ok(len) => len,
        Err(_) => return linux_errno(LINUX_EINVAL),
    };
    if let Err(error) = usermem::validate_current_user_write_buffer(args.buffer_ptr, len) {
        return linux_errno(address_space_error_to_linux_errno(error));
    }
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(len).is_err() {
        return linux_errno(LINUX_ENOMEM);
    }
    bytes.resize(len, 0);
    let read = match block_api::read_bootstrap_file_range(path, args.offset, &mut bytes) {
        Ok(Some(read)) => read,
        Ok(None) => return linux_errno(LINUX_ENOENT),
        Err(error) => return linux_errno(bootstrap_error_to_linux_errno(error)),
    };
    match usermem::write_current_user_bytes(args.buffer_ptr, &bytes[..read]) {
        Ok(()) => read as u64,
        Err(error) => linux_errno(address_space_error_to_linux_errno(error)),
    }
}

fn bootstrap_error_to_linux_errno(
    error: kernel_io_manager::storage::boot_volume::BootstrapImageError,
) -> i64 {
    match error {
        kernel_io_manager::storage::boot_volume::BootstrapImageError::NotFound => LINUX_ENOENT,
        kernel_io_manager::storage::boot_volume::BootstrapImageError::Unavailable
        | kernel_io_manager::storage::boot_volume::BootstrapImageError::Invalid => LINUX_EIO,
    }
}
// RING3-MIGRATION-REFERENCE END: immutable early-system bootstrap substrate.
