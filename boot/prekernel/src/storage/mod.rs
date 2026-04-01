mod ahci;
mod nvme;

use alloc::vec;
use alloc::vec::Vec;

use boot_protocol::{BootVolumeIdentity, BootVolumeTransport};
use core::fmt;
use storage_core::{BlockDevice, BootVolumeLocator, PartitionInfo};

pub(crate) type DiskIoError = storage_core::StorageError;
pub(crate) type BootVolume = storage_fat::FatVolume<storage_core::BlockSlice<BootBlockDevice>>;

pub(crate) enum BootBlockDevice {
    Ahci(ahci::AhciBlockDevice),
    Nvme(nvme::NvmeBlockDevice),
}

impl BootBlockDevice {
    fn transport_name(&self) -> &'static str {
        match self {
            Self::Ahci(_) => "AHCI",
            Self::Nvme(_) => "NVMe",
        }
    }

    fn transport_kind(&self) -> BootVolumeTransport {
        match self {
            Self::Ahci(_) => BootVolumeTransport::Ahci,
            Self::Nvme(_) => BootVolumeTransport::Nvme,
        }
    }
}

impl BlockDevice for BootBlockDevice {
    fn logical_block_size(&self) -> usize {
        match self {
            Self::Ahci(device) => device.logical_block_size(),
            Self::Nvme(device) => device.logical_block_size(),
        }
    }

    fn block_count(&self) -> u64 {
        match self {
            Self::Ahci(device) => device.block_count(),
            Self::Nvme(device) => device.block_count(),
        }
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> Result<(), DiskIoError> {
        match self {
            Self::Ahci(device) => device.read_blocks(lba, out),
            Self::Nvme(device) => device.read_blocks(lba, out),
        }
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> Result<(), DiskIoError> {
        match self {
            Self::Ahci(device) => device.write_blocks(lba, input),
            Self::Nvme(device) => device.write_blocks(lba, input),
        }
    }

    fn flush(&mut self) -> Result<(), DiskIoError> {
        match self {
            Self::Ahci(device) => device.flush(),
            Self::Nvme(device) => device.flush(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum BootStorageError {
    ControllerUnavailable,
    PartitionScan(DiskIoError),
    IdentityMismatch,
    VolumeNotFound,
    FatMount(fatfs::Error<DiskIoError>),
}

impl fmt::Display for BootStorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControllerUnavailable => f.write_str("no supported storage controller available"),
            Self::PartitionScan(err) => write!(f, "partition scan failed: {err:?}"),
            Self::IdentityMismatch => f.write_str("boot volume identity mismatch"),
            Self::VolumeNotFound => f.write_str("boot FAT volume not found"),
            Self::FatMount(err) => write!(f, "FAT mount failed: {err:?}"),
        }
    }
}

pub(crate) fn open_boot_volume(
    identity: BootVolumeIdentity,
) -> Result<BootVolume, BootStorageError> {
    let locator = BootVolumeLocator::new(identity);
    let mut devices = probe_devices();
    let transport_hint = identity.transport();
    sort_devices_by_transport_hint(&mut devices, transport_hint);
    if devices.is_empty() {
        crate::debug::println!("prekernel storage: controller 발견 실패");
        return Err(BootStorageError::ControllerUnavailable);
    }

    if locator.is_none() {
        if transport_hint == BootVolumeTransport::Unknown {
            crate::debug::println!(
                "prekernel storage: boot volume identity unavailable, falling back to first FAT candidate"
            );
        } else {
            crate::debug::println!(
                "prekernel storage: boot volume identity unavailable, preferring {:?} candidates during fallback",
                transport_hint
            );
        }
    }

    for index in 0..devices.len() {
        let partitions = candidate_partitions(&mut devices[index]).map_err(|err| {
            crate::debug::println!("prekernel storage: partition scan 실패: {:?}", err);
            BootStorageError::PartitionScan(err)
        })?;

        for partition in partitions {
            let is_match = if let Some(locator) = locator.as_ref() {
                locator
                    .matches_partition(&mut devices[index], partition)
                    .map_err(|err| {
                        crate::debug::println!(
                            "prekernel storage: boot volume identity mismatch probe error: {:?}",
                            err
                        );
                        BootStorageError::PartitionScan(err)
                    })?
            } else {
                partition_is_fat(&mut devices[index], partition).map_err(|err| {
                    crate::debug::println!("prekernel storage: FAT probe 실패: {:?}", err);
                    BootStorageError::PartitionScan(err)
                })?
            };
            if !is_match {
                continue;
            }

            crate::debug::println!(
                "prekernel storage: matched {} partition start_lba={} sectors={}",
                devices[index].transport_name(),
                partition.start_lba,
                partition.block_count
            );
            let device = devices.swap_remove(index);
            return storage_fat::FatVolume::from_partition(
                device,
                partition.start_lba,
                partition.block_count,
            )
            .map_err(|err| {
                crate::debug::println!("prekernel storage: FAT mount 실패: {:?}", err);
                BootStorageError::FatMount(err)
            });
        }
    }

    if locator.is_some() {
        crate::debug::println!("prekernel storage: boot volume identity mismatch");
        Err(BootStorageError::IdentityMismatch)
    } else {
        crate::debug::println!("prekernel storage: FAT boot volume not found");
        Err(BootStorageError::VolumeNotFound)
    }
}

fn probe_devices() -> Vec<BootBlockDevice> {
    let mut devices = Vec::new();
    devices.extend(ahci::probe_devices().into_iter().map(BootBlockDevice::Ahci));
    devices.extend(nvme::probe_devices().into_iter().map(BootBlockDevice::Nvme));
    devices
}

fn sort_devices_by_transport_hint(
    devices: &mut [BootBlockDevice],
    transport_hint: BootVolumeTransport,
) {
    if transport_hint == BootVolumeTransport::Unknown {
        return;
    }

    devices.sort_by_key(|device| (device.transport_kind() != transport_hint) as u8);
}

fn candidate_partitions<D: BlockDevice>(dev: &mut D) -> Result<Vec<PartitionInfo>, DiskIoError> {
    let mut partitions = storage_core::detect_partitions(dev)?;
    if dev.block_count() != 0 && partitions.is_empty() {
        partitions.push(PartitionInfo {
            start_lba: 0,
            block_count: dev.block_count(),
        });
    }
    Ok(partitions)
}

fn partition_is_fat<D: BlockDevice>(
    dev: &mut D,
    partition: PartitionInfo,
) -> Result<bool, DiskIoError> {
    let mut slice =
        storage_core::BlockSlice::new(&mut *dev, partition.start_lba, partition.block_count)?;
    let block_size = slice.logical_block_size();
    if block_size < 512 {
        return Err(DiskIoError::InvalidInput);
    }
    let mut sector = vec![0_u8; block_size];
    slice.read_blocks(0, &mut sector)?;
    Ok(storage_core::fat_volume_id_from_boot_sector(&sector).is_some())
}
