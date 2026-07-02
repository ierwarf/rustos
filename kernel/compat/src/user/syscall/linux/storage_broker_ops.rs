use super::*;

use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_STORAGE_POLICY, STORAGE_FLAG_READONLY, STORAGE_LIST_MAX_DESCRIPTORS,
    STORAGE_LIST_PATH_CAPACITY, StorageBlockDescriptorWire, StorageListBrokerArgs,
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
