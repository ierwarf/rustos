use super::*;

use rustos_user_abi::syscall::{
    BOOT_EXTENT_FLAG_READONLY, BOOT_EXTENT_PATH_CAPACITY, BootExtentLeaseWire,
    IPC_SERVICE_CAP_STORAGE_POLICY, RustosBootExtentBrokerArgs, STORAGE_FLAG_READONLY,
    STORAGE_LIST_MAX_DESCRIPTORS, STORAGE_LIST_PATH_CAPACITY, StorageBlockDescriptorWire,
    StorageListBrokerArgs,
};
use storage_core::TransportKind;

pub(super) fn syscall_linux_rustos_storage_list_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_STORAGE_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<StorageListBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != 1
        || args.reserved0 != 0
        || args.reserved1 != 0
        || args.reserved2 != 0
        || args.out_descriptors_ptr == 0
        || args.out_count_ptr == 0
    {
        return linux_errno(LINUX_EINVAL);
    }

    let capacity = usize::try_from(args.out_capacity).unwrap_or(usize::MAX);
    if capacity > STORAGE_LIST_MAX_DESCRIPTORS {
        return linux_errno(LINUX_EINVAL);
    }

    let descriptors = kernel_io_manager::api::block::descriptors();
    let out_count = descriptors.len().min(capacity);
    for (index, descriptor) in descriptors.iter().take(out_count).enumerate() {
        let wire = storage_descriptor_wire(descriptor);
        let offset = index
            .checked_mul(core::mem::size_of::<StorageBlockDescriptorWire>())
            .and_then(|offset| u64::try_from(offset).ok())
            .ok_or(LINUX_EINVAL);
        let offset = match offset {
            Ok(offset) => offset,
            Err(errno) => return linux_errno(errno),
        };
        if let Err(err) =
            usermem::write_current_user_struct(args.out_descriptors_ptr + offset, &wire)
        {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    }

    match usermem::write_current_user_u32(args.out_count_ptr, out_count as u32) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

pub(super) fn syscall_linux_rustos_boot_extent_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_STORAGE_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<RustosBootExtentBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != rustos_user_abi::syscall::STORAGED_IPC_ABI_VERSION
        || args.flags != 0
        || args.reserved0 != 0
        || args.path_ptr == 0
        || args.path_len == 0
        || args.path_len as usize > BOOT_EXTENT_PATH_CAPACITY
        || args.out_lease_ptr == 0
    {
        return linux_errno(LINUX_EINVAL);
    }

    let Ok(path_len) = usize::try_from(args.path_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    let mut path_bytes = alloc::vec![0_u8; path_len];
    if let Err(err) = usermem::copy_from_current_user_exact(args.path_ptr, &mut path_bytes) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    if path_bytes.iter().any(|byte| *byte == 0) {
        return linux_errno(LINUX_EINVAL);
    }
    let Ok(path) = core::str::from_utf8(&path_bytes) else {
        return linux_errno(LINUX_EINVAL);
    };
    let normalized = path.strip_prefix('/').unwrap_or(path);
    let mut lease = BootExtentLeaseWire {
        path_len: path_len as u32,
        flags: BOOT_EXTENT_FLAG_READONLY,
        ..BootExtentLeaseWire::default()
    };
    match kernel_io_manager::api::vfs::boot_path_extent_lease_for_kernel(normalized) {
        Ok(Some(extent_lease)) => {
            if extent_lease.extents.len() > lease.extents.len() {
                return linux_errno(LINUX_EOVERFLOW);
            }
            lease.file_len = extent_lease.file_len;
            lease.hash_or_generation = extent_lease.generation;
            lease.extent_count = extent_lease.extents.len() as u32;
            for (dest, src) in lease.extents.iter_mut().zip(extent_lease.extents.iter()) {
                dest.disk_offset = src.disk_offset;
                dest.len = src.len;
            }
        }
        Ok(None) => {
            lease.file_len =
                match kernel_io_manager::api::vfs::boot_path_file_len_for_kernel(normalized) {
                    Ok(file_len) => file_len,
                    Err(_) => return linux_errno(LINUX_ENOENT),
                };
        }
        Err(_) => return linux_errno(LINUX_ENOENT),
    }
    lease.path[..path_len].copy_from_slice(&path_bytes);
    match usermem::write_current_user_struct(args.out_lease_ptr, &lease) {
        Ok(()) => 0,
        Err(err) => linux_errno(address_space_error_to_linux_errno(err)),
    }
}

fn storage_descriptor_wire(
    descriptor: &kernel_io_manager::api::BlockDescriptor,
) -> StorageBlockDescriptorWire {
    let mut wire = StorageBlockDescriptorWire {
        id: descriptor.id,
        transport: storage_transport_wire(descriptor.transport),
        flags: if descriptor.readonly {
            STORAGE_FLAG_READONLY
        } else {
            0
        },
        logical_block_size: descriptor.logical_block_size as u32,
        start_block: descriptor.start_block,
        block_count: descriptor.block_count,
        ..StorageBlockDescriptorWire::default()
    };
    let path = descriptor.path.as_bytes();
    let path_len = path.len().min(STORAGE_LIST_PATH_CAPACITY);
    wire.path_len = path_len as u32;
    wire.path[..path_len].copy_from_slice(&path[..path_len]);
    wire
}

fn storage_transport_wire(transport: TransportKind) -> u32 {
    match transport {
        TransportKind::Ahci => 1,
        TransportKind::Nvme => 2,
        TransportKind::Usb => 3,
    }
}
