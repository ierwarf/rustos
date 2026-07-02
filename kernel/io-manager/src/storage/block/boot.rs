// RING3-MIGRATION-REFERENCE START: bootstrap exception: storaged owns
// post-bootstrap boot-volume admission. Ring0 keeps exact physical boot-volume
// open fallback until storaged is available early enough for rootfs access.
use alloc::vec::Vec;
use boot_protocol::{BootVolumeIdentity, BootVolumeTransport};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use storage_core::{BootVolumeLocator, PartitionInfo as SharedPartitionInfo};

use super::{
    BLOCK_DEVICES, BlockDeviceHandle, DiskIoError, IoResult, descriptor_without_init, io, registry,
};

static BOOT_FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);
static PHYSICAL_BOOT_OPEN_LOGS_REMAINING: AtomicUsize = AtomicUsize::new(4);

struct RegistryRootBlockDevice {
    root_id: u32,
    logical_block_size: usize,
    block_count: u64,
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
pub(super) fn open_boot_handle() -> IoResult<BlockDeviceHandle> {
    super::ensure_initialized();
    let should_log = !BOOT_FALLBACK_LOGGED.swap(true, Ordering::AcqRel);
    if should_log {
        crate::debug::println!("storage: boot volume fallback opener invoked");
    }
    let transport_hint = crate::storage::boot_volume::boot_volume_transport_hint()
        .unwrap_or(BootVolumeTransport::Unknown);

    let mut root_ids = {
        let devices = BLOCK_DEVICES.lock();
        registry::root_device_ids_locked(&devices)
    };
    registry::sort_root_ids_by_transport_hint(&mut root_ids, transport_hint);
    if should_log && transport_hint != BootVolumeTransport::Unknown {
        crate::debug::println!(
            "storage: boot volume fallback prefers {:?} candidates",
            transport_hint
        );
    }

    for root_id in root_ids {
        let Some(descriptor) = descriptor_without_init(BlockDeviceHandle::new(root_id)) else {
            continue;
        };
        if should_log {
            crate::debug::println!(
                "storage: probing FAT candidate id={} path={} transport={:?} readonly={} block_size={} start_block={} blocks={}",
                root_id,
                descriptor.path,
                descriptor.transport,
                descriptor.readonly,
                descriptor.logical_block_size,
                descriptor.start_block,
                descriptor.block_count
            );
        }

        let detected = match detect_fat_boot_partition_handle(root_id) {
            Ok(value) => value,
            Err(err) => {
                if should_log {
                    crate::debug::println!(
                        "storage: rejected FAT candidate id={} path={} detect error={:?}",
                        root_id,
                        descriptor.path,
                        err
                    );
                }
                continue;
            }
        };

        let Some((handle, partition)) = detected else {
            if should_log {
                crate::debug::println!(
                    "storage: rejected FAT candidate id={} path={}",
                    root_id,
                    descriptor.path
                );
            }
            continue;
        };

        if should_log && let Some(selected) = descriptor_without_init(handle) {
            crate::debug::println!(
                "storage: selected FAT boot candidate id={} path={} start_block={} blocks={}",
                selected.id,
                selected.path,
                partition.start_lba,
                partition.block_count
            );
        }
        return Ok(handle);
    }

    if should_log {
        crate::debug::println!("storage: no FAT boot candidate matched");
    }
    Err(DiskIoError::NotPresent)
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
pub(super) fn open_physical_boot_handle(
    identity: BootVolumeIdentity,
) -> IoResult<BlockDeviceHandle> {
    super::ensure_initialized();
    let Some(locator) = BootVolumeLocator::new(identity) else {
        crate::debug::println!("storage: physical boot opener requested without identity");
        return Err(DiskIoError::NotPresent);
    };
    let should_log = PHYSICAL_BOOT_OPEN_LOGS_REMAINING
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok();
    if should_log {
        crate::debug::println!(
            "storage: physical boot opener identity transport={:?} serial={:#010x} start_lba={} sectors={}",
            identity.transport(),
            identity.fat_volume_id,
            identity.volume_start_lba,
            identity.volume_sector_count
        );
    }

    let mut root_ids = {
        let devices = BLOCK_DEVICES.lock();
        registry::root_device_ids_locked(&devices)
    };
    registry::sort_root_ids_by_transport_hint(&mut root_ids, identity.transport());

    for root_id in root_ids {
        let Some(descriptor) = descriptor_without_init(BlockDeviceHandle::new(root_id)) else {
            continue;
        };

        if should_log {
            crate::debug::println!(
                "storage: physical opener candidate id={} path={} block_size={} start_block={} blocks={}",
                root_id,
                descriptor.path,
                descriptor.logical_block_size,
                descriptor.start_block,
                descriptor.block_count
            );
        }

        let mut root = RegistryRootBlockDevice {
            root_id,
            logical_block_size: descriptor.logical_block_size,
            block_count: descriptor.block_count,
        };
        let partitions = match candidate_partitions(&mut root) {
            Ok(partitions) => partitions,
            Err(err) => {
                if should_log {
                    crate::debug::println!(
                        "storage: physical opener candidate id={} path={} partition scan error={:?}",
                        root_id,
                        descriptor.path,
                        err
                    );
                }
                continue;
            }
        };

        for partition in partitions {
            let is_match = match locator.matches_partition(&mut root, partition) {
                Ok(result) => result,
                Err(err) => {
                    if should_log {
                        crate::debug::println!(
                            "storage: physical opener candidate id={} path={} identity probe error={:?}",
                            root_id,
                            descriptor.path,
                            err
                        );
                    }
                    break;
                }
            };
            if !is_match {
                continue;
            }

            let Some(handle) = find_device_handle_for_partition(root_id, partition) else {
                if should_log {
                    crate::debug::println!(
                        "storage: physical opener candidate id={} path={} matched but no handle was registered",
                        root_id,
                        descriptor.path
                    );
                }
                break;
            };

            if should_log && let Some(selected) = descriptor_without_init(handle) {
                crate::debug::println!(
                    "storage: physical boot opener matched id={} path={}",
                    selected.id,
                    selected.path
                );
            }
            return Ok(handle);
        }
    }

    if should_log {
        crate::debug::println!("storage: physical boot opener found no exact match");
    }
    Err(DiskIoError::NotPresent)
}

fn find_device_handle_for_partition(
    root_id: u32,
    partition: SharedPartitionInfo,
) -> Option<BlockDeviceHandle> {
    let devices = BLOCK_DEVICES.lock();
    devices
        .iter()
        .find(|device| {
            registry::device_root_id_locked(&devices, device.id) == Some(root_id)
                && registry::device_start_block_locked(&devices, device.id)
                    == Some(partition.start_lba)
                && registry::device_block_count_locked(&devices, device.id)
                    == Some(partition.block_count)
        })
        .map(|device| BlockDeviceHandle::new(device.id))
}

fn candidate_partitions(root: &mut RegistryRootBlockDevice) -> IoResult<Vec<SharedPartitionInfo>> {
    let mut partitions = storage_core::detect_partitions(root)?;
    if root.block_count != 0 && partitions.is_empty() {
        partitions.push(SharedPartitionInfo {
            start_lba: 0,
            block_count: root.block_count,
        });
    }
    Ok(partitions)
}

fn detect_fat_boot_partition_handle(
    root_id: u32,
) -> IoResult<Option<(BlockDeviceHandle, SharedPartitionInfo)>> {
    let (logical_block_size, block_count) =
        descriptor_without_init(BlockDeviceHandle::new(root_id))
            .map(|device| (device.logical_block_size, device.block_count))
            .ok_or(DiskIoError::NotPresent)?;
    let mut root = RegistryRootBlockDevice {
        root_id,
        logical_block_size,
        block_count,
    };
    let Some(partition) = storage_core::detect_fat_boot_partition(&mut root)? else {
        return Ok(None);
    };
    let handle =
        find_device_handle_for_partition(root_id, partition).ok_or(DiskIoError::NotPresent)?;
    Ok(Some((handle, partition)))
}

pub(super) fn detect_partitions(root_id: u32) -> IoResult<Vec<SharedPartitionInfo>> {
    let (logical_block_size, block_count) =
        descriptor_without_init(BlockDeviceHandle::new(root_id))
            .map(|device| (device.logical_block_size, device.block_count))
            .ok_or(DiskIoError::NotPresent)?;
    if block_count == 0 {
        return Ok(Vec::new());
    }

    let mut root = RegistryRootBlockDevice {
        root_id,
        logical_block_size,
        block_count,
    };
    candidate_partitions(&mut root).map(|partitions| {
        partitions
            .into_iter()
            .filter(|partition| {
                partition.start_lba != 0 || partition.block_count != root.block_count
            })
            .collect()
    })
}

impl storage_core::BlockDevice for RegistryRootBlockDevice {
    fn logical_block_size(&self) -> usize {
        self.logical_block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> storage_core::IoResult<()> {
        io::validate_block_io_exact(self.logical_block_size, lba, self.block_count, out.len())?;
        io::read_blocks_uncached(self.root_id, lba, out)
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> storage_core::IoResult<()> {
        io::validate_block_io_exact(self.logical_block_size, lba, self.block_count, input.len())?;
        io::write_blocks_uncached(self.root_id, lba, input)
    }

    fn flush(&mut self) -> storage_core::IoResult<()> {
        io::flush_uncached(self.root_id)
    }
}
// RING3-MIGRATION-REFERENCE END: storaged-owned boot-volume admission bootstrap exception.
