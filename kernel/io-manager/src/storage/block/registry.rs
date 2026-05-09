use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{
    BLOCK_DEVICES, BlockDeviceHandle, BlockDeviceKind, BlockDeviceOps, BlockDeviceRecord,
    BlockTransportKind, descriptor_without_init,
};
use crate::sync::KernelWaitLock;

pub(super) fn register_root_device(device: Box<dyn BlockDeviceOps>) {
    let transport = device.transport_kind();
    let readonly = device.readonly();
    let root_id = {
        let mut devices = BLOCK_DEVICES.lock();
        let id = devices.len() as u32;
        devices.push(BlockDeviceRecord {
            id,
            path: alloc::format!("/dev/block{id}"),
            transport,
            readonly,
            kind: BlockDeviceKind::Root(Arc::new(KernelWaitLock::new(device))),
        });
        id
    };

    register_partitions(root_id);
}

fn register_partitions(root_id: u32) {
    let partitions = match super::boot::detect_partitions(root_id) {
        Ok(partitions) => partitions,
        Err(_) => return,
    };
    if partitions.is_empty() {
        return;
    }

    let readonly = descriptor_without_init(BlockDeviceHandle::new(root_id))
        .map(|device| device.readonly)
        .unwrap_or(true);
    let transport = descriptor_without_init(BlockDeviceHandle::new(root_id))
        .map(|device| device.transport)
        .unwrap_or(BlockTransportKind::Ahci);

    let mut devices = BLOCK_DEVICES.lock();
    for (index, partition) in partitions.into_iter().enumerate() {
        let id = devices.len() as u32;
        let partition_number = index + 1;
        devices.push(BlockDeviceRecord {
            id,
            path: alloc::format!("/dev/block{root_id}p{partition_number}"),
            transport,
            readonly,
            kind: BlockDeviceKind::Slice {
                parent_id: root_id,
                start_block: partition.start_lba,
                block_count: partition.block_count,
            },
        });
    }
}

pub(super) fn device_logical_block_size_locked(
    devices: &[BlockDeviceRecord],
    device_id: u32,
) -> Option<usize> {
    let record = devices.iter().find(|device| device.id == device_id)?;
    match &record.kind {
        BlockDeviceKind::Root(device) => Some(device.lock().logical_block_size()),
        BlockDeviceKind::Slice { parent_id, .. } => {
            device_logical_block_size_locked(devices, *parent_id)
        }
    }
}

pub(super) fn device_block_count_locked(
    devices: &[BlockDeviceRecord],
    device_id: u32,
) -> Option<u64> {
    let record = devices.iter().find(|device| device.id == device_id)?;
    match &record.kind {
        BlockDeviceKind::Root(device) => Some(device.lock().block_count()),
        BlockDeviceKind::Slice { block_count, .. } => Some(*block_count),
    }
}

pub(super) fn device_start_block_locked(
    devices: &[BlockDeviceRecord],
    device_id: u32,
) -> Option<u64> {
    let record = devices.iter().find(|device| device.id == device_id)?;
    match &record.kind {
        BlockDeviceKind::Root(_) => Some(0),
        BlockDeviceKind::Slice {
            parent_id,
            start_block,
            ..
        } => Some(device_start_block_locked(devices, *parent_id)?.saturating_add(*start_block)),
    }
}

pub(super) fn device_root_id_locked(devices: &[BlockDeviceRecord], device_id: u32) -> Option<u32> {
    let record = devices.iter().find(|device| device.id == device_id)?;
    match &record.kind {
        BlockDeviceKind::Root(_) => Some(record.id),
        BlockDeviceKind::Slice { parent_id, .. } => device_root_id_locked(devices, *parent_id),
    }
}

pub(super) fn root_device_ids_locked(devices: &[BlockDeviceRecord]) -> Vec<u32> {
    devices
        .iter()
        .filter_map(|device| match &device.kind {
            BlockDeviceKind::Root(_) => Some(device.id),
            BlockDeviceKind::Slice { .. } => None,
        })
        .collect()
}

pub(super) fn sort_root_ids_by_transport_hint(
    root_ids: &mut [u32],
    transport_hint: boot_protocol::BootVolumeTransport,
) {
    if transport_hint == boot_protocol::BootVolumeTransport::Unknown {
        return;
    }

    root_ids.sort_by_key(|root_id| {
        let device_transport = descriptor_without_init(BlockDeviceHandle::new(*root_id))
            .map(|descriptor| boot_transport_from_block(descriptor.transport))
            .unwrap_or(boot_protocol::BootVolumeTransport::Unknown);
        (device_transport != transport_hint) as u8
    });
}

fn boot_transport_from_block(transport: BlockTransportKind) -> boot_protocol::BootVolumeTransport {
    match transport {
        BlockTransportKind::Ahci => boot_protocol::BootVolumeTransport::Ahci,
        BlockTransportKind::Nvme => boot_protocol::BootVolumeTransport::Nvme,
        BlockTransportKind::Usb => boot_protocol::BootVolumeTransport::Usb,
    }
}
