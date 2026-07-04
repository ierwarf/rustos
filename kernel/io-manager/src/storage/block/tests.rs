use crate::sync::KernelSpinLock as Mutex;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use boot_protocol::BootVolumeIdentity;
use core::sync::atomic::Ordering;
use storage_core::BlockDevice as SharedBlockDevice;

use super::io::{cache_lookup, clear_cache_for_tests, read_cached_block, write_cached_block};
use super::registry::register_root_device;
use super::{
    BLOCK_DEVICES, BLOCK_INIT_DONE, BLOCK_INIT_STATE, BlockDeviceOps, BlockTransportKind,
    MIN_LOGICAL_BLOCK_SIZE, descriptors, flush, lookup, open_boot_block_device,
    open_physical_boot_block_device,
};
use crate::storage::fat::{DiskIoError, IoResult};

const MBR_PARTITION_TABLE_OFFSET: usize = 446;
const TEST_BLOCK_SIZE: usize = MIN_LOGICAL_BLOCK_SIZE;
static TEST_LOCK: Mutex<()> = Mutex::new(());

struct MockBlockDevice {
    blocks: Mutex<Vec<[u8; TEST_BLOCK_SIZE]>>,
    readonly: bool,
}

impl MockBlockDevice {
    fn new(block_count: usize, readonly: bool) -> Self {
        Self {
            blocks: Mutex::new(vec![[0_u8; TEST_BLOCK_SIZE]; block_count]),
            readonly,
        }
    }

    fn with_mbr_partition(start_lba: u32, blocks: u32, readonly: bool) -> Self {
        let device = Self::new((start_lba + blocks + 2) as usize, readonly);
        {
            let mut all_blocks = device.blocks.lock();
            let block0 = &mut all_blocks[0];
            block0[TEST_BLOCK_SIZE - 2] = 0x55;
            block0[TEST_BLOCK_SIZE - 1] = 0xAA;
            let off = MBR_PARTITION_TABLE_OFFSET;
            block0[off + 4] = 0x83;
            block0[off + 8..off + 12].copy_from_slice(&start_lba.to_le_bytes());
            block0[off + 12..off + 16].copy_from_slice(&blocks.to_le_bytes());
        }
        device
    }

    fn with_fat_partition(start_lba: u32, blocks: u32, volume_id: u32, readonly: bool) -> Self {
        let device = Self::with_mbr_partition(start_lba, blocks, readonly);
        {
            let mut all_blocks = device.blocks.lock();
            all_blocks[start_lba as usize] =
                fat_boot_sector(blocks as u16, TEST_BLOCK_SIZE as u16, volume_id);
        }
        device
    }

    fn with_fat_superfloppy(blocks: u32, volume_id: u32, readonly: bool) -> Self {
        let device = Self::new(blocks as usize, readonly);
        {
            let mut all_blocks = device.blocks.lock();
            all_blocks[0] = fat_boot_sector(blocks as u16, TEST_BLOCK_SIZE as u16, volume_id);
        }
        device
    }
}

impl SharedBlockDevice for MockBlockDevice {
    fn logical_block_size(&self) -> usize {
        TEST_BLOCK_SIZE
    }

    fn block_count(&self) -> u64 {
        self.blocks.lock().len() as u64
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        if out.is_empty() || out.len() % TEST_BLOCK_SIZE != 0 {
            return Err(DiskIoError::InvalidInput);
        }
        let blocks = self.blocks.lock();
        for (index, chunk) in out.chunks_exact_mut(TEST_BLOCK_SIZE).enumerate() {
            let Some(data) = blocks.get(lba as usize + index) else {
                return Err(DiskIoError::InvalidInput);
            };
            chunk.copy_from_slice(data);
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        if input.is_empty() || input.len() % TEST_BLOCK_SIZE != 0 {
            return Err(DiskIoError::InvalidInput);
        }
        if self.readonly {
            return Err(DiskIoError::InvalidInput);
        }
        let mut blocks = self.blocks.lock();
        for (index, chunk) in input.chunks_exact(TEST_BLOCK_SIZE).enumerate() {
            let Some(data) = blocks.get_mut(lba as usize + index) else {
                return Err(DiskIoError::InvalidInput);
            };
            data.copy_from_slice(chunk);
        }
        Ok(())
    }

    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

impl BlockDeviceOps for MockBlockDevice {
    fn transport_kind(&self) -> BlockTransportKind {
        BlockTransportKind::Ahci
    }

    fn readonly(&self) -> bool {
        self.readonly
    }
}

fn reset_for_tests() {
    BLOCK_DEVICES.lock().clear();
    clear_cache_for_tests();
    BLOCK_INIT_STATE.store(BLOCK_INIT_DONE, Ordering::Release);
}

fn fat_boot_sector(
    total_blocks: u16,
    bytes_per_sector: u16,
    volume_id: u32,
) -> [u8; TEST_BLOCK_SIZE] {
    let mut block = [0_u8; TEST_BLOCK_SIZE];
    block[0] = 0xEB;
    block[2] = 0x90;
    block[11..13].copy_from_slice(&bytes_per_sector.to_le_bytes());
    block[13] = 1;
    block[14..16].copy_from_slice(&1_u16.to_le_bytes());
    block[16] = 2;
    block[17..19].copy_from_slice(&32_u16.to_le_bytes());
    block[19..21].copy_from_slice(&total_blocks.to_le_bytes());
    block[21] = 0xF8;
    block[22..24].copy_from_slice(&1_u16.to_le_bytes());
    block[39..43].copy_from_slice(&volume_id.to_le_bytes());
    block[510] = 0x55;
    block[511] = 0xAA;
    block
}

#[test]
fn register_root_device_creates_partition_nodes() {
    let _guard = TEST_LOCK.lock();
    reset_for_tests();

    register_root_device(Box::new(MockBlockDevice::with_mbr_partition(1, 3, false)));

    let descriptors = descriptors();
    assert_eq!(descriptors.len(), 2);
    assert_eq!(descriptors[0].path, "/dev/block0");
    assert_eq!(descriptors[1].path, "/dev/block0p1");
}

#[test]
fn partition_writes_are_forwarded_to_parent_sectors_and_cached() {
    let _guard = TEST_LOCK.lock();
    reset_for_tests();

    register_root_device(Box::new(MockBlockDevice::with_mbr_partition(2, 2, false)));

    let root = lookup("/dev/block0").expect("root block device");
    let partition = lookup("/dev/block0p1").expect("partition block device");

    let mut sector = [0_u8; TEST_BLOCK_SIZE];
    sector[0] = 0xAA;
    sector[TEST_BLOCK_SIZE - 1] = 0x55;
    write_cached_block(partition.id(), 0, &sector).expect("partition write");
    flush(partition).expect("flush partition");

    let mut root_sector = [0_u8; TEST_BLOCK_SIZE];
    read_cached_block(root.id(), 2, &mut root_sector).expect("read parent block");
    assert_eq!(root_sector, sector);
    assert_eq!(cache_lookup(root.id(), 2), Some(sector.to_vec()));
}

#[test]
fn readonly_partition_inherits_parent_readonly_state() {
    let _guard = TEST_LOCK.lock();
    reset_for_tests();

    register_root_device(Box::new(MockBlockDevice::with_mbr_partition(1, 2, true)));
    let descriptors = descriptors();
    assert!(descriptors.iter().all(|descriptor| descriptor.readonly));
}

#[test]
fn physical_boot_opener_requires_exact_partition_identity() {
    let _guard = TEST_LOCK.lock();
    reset_for_tests();

    let start_lba = 8;
    let sectors = 16;
    let volume_id = 0xCAFE_BABE;
    register_root_device(Box::new(MockBlockDevice::with_fat_partition(
        start_lba, sectors, volume_id, false,
    )));

    let identity = BootVolumeIdentity {
        fat_volume_id: volume_id,
        _reserved0: 0,
        volume_start_lba: start_lba as u64,
        volume_sector_count: sectors as u64,
    };
    assert!(open_physical_boot_block_device(identity).is_ok());

    let mismatched = BootVolumeIdentity {
        volume_start_lba: (start_lba + 1) as u64,
        ..identity
    };
    assert!(matches!(
        open_physical_boot_block_device(mismatched),
        Err(fatfs::Error::Io(DiskIoError::NotPresent))
    ));
}

#[test]
fn physical_boot_opener_matches_superfloppy_identity() {
    let _guard = TEST_LOCK.lock();
    reset_for_tests();

    let sectors = 32;
    let volume_id = 0xABCD_1234;
    register_root_device(Box::new(MockBlockDevice::with_fat_superfloppy(
        sectors, volume_id, false,
    )));

    let identity = BootVolumeIdentity {
        fat_volume_id: volume_id,
        _reserved0: 0,
        volume_start_lba: 0,
        volume_sector_count: sectors as u64,
    };

    assert!(open_physical_boot_block_device(identity).is_ok());
}

#[test]
fn boot_opener_returns_detected_partition_extent() {
    let _guard = TEST_LOCK.lock();
    reset_for_tests();

    let start_lba = 8;
    let sectors = 16;
    register_root_device(Box::new(MockBlockDevice::with_fat_partition(
        start_lba,
        sectors,
        0x1111_2222,
        false,
    )));

    let device = open_boot_block_device().expect("open detected FAT device");
    assert_eq!(device.logical_block_size(), TEST_BLOCK_SIZE);
    assert_eq!(device.block_count(), sectors as u64);
}
