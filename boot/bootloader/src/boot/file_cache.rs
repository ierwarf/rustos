use alloc::vec;

use uefi::boot;
use uefi::prelude::*;
use uefi::proto::device_path::{DevicePath, DevicePathNodeEnum};
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::block::BlockIO;

use crate::boot_info::{BootVolumeIdentity, BootVolumeTransport};
use crate::debug;

use super::error::BootError;

pub fn extract_boot_volume_identity() -> Result<BootVolumeIdentity, BootError> {
    let loaded_image = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
        .map_err(|err| BootError::CacheBootVolume(err.status()))?;
    let device_handle = loaded_image
        .device()
        .ok_or(BootError::CacheBootVolume(Status::NOT_FOUND))?;
    let device_path = boot::open_protocol_exclusive::<DevicePath>(device_handle)
        .map_err(|err| BootError::CacheBootVolume(err.status()))?;
    let (volume_start_lba, partition_sector_count, transport) = boot_volume_context(&device_path);

    let block_io = boot::open_protocol_exclusive::<BlockIO>(device_handle)
        .map_err(|err| BootError::CacheBootVolume(err.status()))?;
    let media = block_io.media();
    if !media.is_media_present() {
        return Err(BootError::CacheBootVolume(Status::NO_MEDIA));
    }

    let volume_sector_count = if partition_sector_count != 0 {
        partition_sector_count
    } else {
        media.last_block().saturating_add(1)
    };
    if volume_sector_count == 0 {
        return Err(BootError::CacheBootVolume(Status::NO_MEDIA));
    }

    let block_size = media.block_size() as usize;
    if block_size < 512 {
        return Err(BootError::CacheBootVolume(Status::LOAD_ERROR));
    }

    let mut boot_sector = vec![0_u8; block_size];
    block_io
        .read_blocks(media.media_id(), 0, &mut boot_sector)
        .map_err(|err| BootError::CacheBootVolume(err.status()))?;

    let fat_volume_id = storage_core::fat_volume_id_from_boot_sector(&boot_sector)
        .ok_or(BootError::CacheBootVolume(Status::LOAD_ERROR))?;
    debug::println!(
        "bootloader: boot volume identity: transport={:?} serial={:#010x} start_lba={} sectors={} block_size={}",
        transport,
        fat_volume_id,
        volume_start_lba,
        volume_sector_count,
        block_size
    );
    Ok(BootVolumeIdentity {
        fat_volume_id,
        _reserved0: transport as u32,
        volume_start_lba,
        volume_sector_count,
    })
}

fn boot_volume_context(device_path: &DevicePath) -> (u64, u64, BootVolumeTransport) {
    let mut volume_start_lba = 0;
    let mut partition_sector_count = 0;
    let mut transport = BootVolumeTransport::Unknown;

    for node in device_path.node_iter() {
        let Ok(node) = node.as_enum() else {
            continue;
        };
        match node {
            DevicePathNodeEnum::MessagingNvmeNamespace(_) => {
                transport = select_transport(transport, BootVolumeTransport::Nvme);
            }
            DevicePathNodeEnum::MessagingUsb(_)
            | DevicePathNodeEnum::MessagingUsbWwid(_)
            | DevicePathNodeEnum::MessagingUsbClass(_) => {
                transport = select_transport(transport, BootVolumeTransport::Usb);
            }
            DevicePathNodeEnum::MessagingSata(_)
            | DevicePathNodeEnum::MessagingAtapi(_)
            | DevicePathNodeEnum::MessagingScsi(_) => {
                transport = select_transport(transport, BootVolumeTransport::Ahci);
            }
            DevicePathNodeEnum::MediaHardDrive(hard_drive) => {
                volume_start_lba = hard_drive.partition_start();
                partition_sector_count = hard_drive.partition_size();
            }
            _ => {}
        }
    }

    (volume_start_lba, partition_sector_count, transport)
}

fn select_transport(
    current: BootVolumeTransport,
    candidate: BootVolumeTransport,
) -> BootVolumeTransport {
    if transport_priority(candidate) > transport_priority(current) {
        candidate
    } else {
        current
    }
}

fn transport_priority(transport: BootVolumeTransport) -> u8 {
    match transport {
        BootVolumeTransport::Unknown => 0,
        BootVolumeTransport::Ahci => 1,
        BootVolumeTransport::Usb => 2,
        BootVolumeTransport::Nvme => 3,
    }
}
