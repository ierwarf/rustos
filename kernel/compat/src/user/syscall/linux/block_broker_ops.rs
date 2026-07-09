// RING3-MIGRATION-REFERENCE START: vfsd/storaged should own post-bootstrap
// boot extent policy. Ring0 keeps the gated physical boot-block read substrate.
use super::*;

use alloc::vec::Vec;
use kernel_io_manager::api::block as block_api;
use rustos_user_abi::syscall::{
    BLOCK_BROKER_ABI_VERSION, BLOCK_BROKER_MAX_IO_BYTES, BLOCK_BROKER_OP_BOOT_INFO,
    BLOCK_BROKER_OP_BOOT_READ, IPC_SERVICE_CAP_VFS_POLICY, RustosBlockBrokerArgs,
};

pub(super) fn syscall_linux_rustos_block_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_VFS_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<RustosBlockBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != BLOCK_BROKER_ABI_VERSION || args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }

    match args.op {
        BLOCK_BROKER_OP_BOOT_INFO => broker_boot_info(&args),
        BLOCK_BROKER_OP_BOOT_READ => broker_boot_read(&args),
        _ => linux_errno(LINUX_EINVAL),
    }
}

fn broker_boot_info(args: &RustosBlockBrokerArgs) -> u64 {
    let Some(descriptor) = block_api::boot_volume_descriptor() else {
        return linux_errno(LINUX_ENODEV);
    };
    if args.out_logical_block_size_ptr == 0 || args.out_block_count_ptr == 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let logical_block_size = descriptor.logical_block_size as u64;
    if let Err(err) = usermem::write_current_user_bytes(
        args.out_logical_block_size_ptr,
        &logical_block_size.to_ne_bytes(),
    ) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if let Err(err) = usermem::write_current_user_bytes(
        args.out_block_count_ptr,
        &descriptor.block_count.to_ne_bytes(),
    ) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    0
}

fn broker_boot_read(args: &RustosBlockBrokerArgs) -> u64 {
    let Some(descriptor) = block_api::boot_volume_descriptor() else {
        return linux_errno(LINUX_ENODEV);
    };
    let Ok(block_size) = u64::try_from(descriptor.logical_block_size) else {
        return linux_errno(LINUX_EINVAL);
    };
    if block_size == 0
        || args.block_count == 0
        || args.buffer_ptr == 0
        || args.buffer_len > BLOCK_BROKER_MAX_IO_BYTES as u64
    {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(byte_len) = args.block_count.checked_mul(block_size) else {
        return linux_errno(LINUX_EINVAL);
    };
    if byte_len != args.buffer_len {
        return linux_errno(LINUX_EINVAL);
    }
    if args
        .lba
        .checked_add(args.block_count)
        .is_none_or(|end| end > descriptor.block_count)
    {
        return linux_errno(LINUX_EINVAL);
    }
    let Ok(byte_len_usize) = usize::try_from(byte_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if let Err(err) = usermem::validate_current_user_write_buffer(args.buffer_ptr, byte_len_usize) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }

    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(byte_len_usize).is_err() {
        return linux_errno(LINUX_ENOMEM);
    }
    bytes.resize(byte_len_usize, 0);
    if let Err(err) = block_api::read_boot_volume_blocks(args.lba, &mut bytes) {
        return linux_errno(storage_error_to_linux_errno(err));
    }
    match usermem::write_current_user_bytes(args.buffer_ptr, &bytes) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

fn storage_error_to_linux_errno(err: storage_core::StorageError) -> i64 {
    match err {
        storage_core::StorageError::Interrupted => LINUX_EINTR,
        storage_core::StorageError::InvalidInput => LINUX_EINVAL,
        storage_core::StorageError::Timeout => LINUX_ETIMEDOUT,
        storage_core::StorageError::NotPresent => LINUX_ENODEV,
        storage_core::StorageError::Unsupported => LINUX_ENOSYS,
        storage_core::StorageError::UnexpectedEof
        | storage_core::StorageError::WriteZero
        | storage_core::StorageError::DeviceFault => LINUX_EIO,
    }
}
// RING3-MIGRATION-REFERENCE END: vfsd/storaged-owned boot-block substrate exception.
