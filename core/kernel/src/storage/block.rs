use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use boot_protocol::BootVolumeIdentity;
use core::cmp::min;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use crate::storage::fat::{BlockDevice as FatBlockDevice, DiskIoError, IoResult, FAT_SECTOR_SIZE};

const GPT_HEADER_LBA: u64 = 1;
const GPT_SIGNATURE: [u8; 8] = *b"EFI PART";
const MBR_PARTITION_TABLE_OFFSET: usize = 446;
const MBR_PARTITION_ENTRY_LEN: usize = 16;
const BLOCK_CACHE_CAPACITY: usize = 256;

static BLOCK_DEVICES: Mutex<Vec<BlockDeviceRecord>> = Mutex::new(Vec::new());
static BLOCK_CACHE: Mutex<Vec<BlockCacheEntry>> = Mutex::new(Vec::new());
static BLOCK_INIT_DONE: AtomicBool = AtomicBool::new(false);
static BLOCK_INIT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockDeviceHandle {
    id: u32,
}

impl BlockDeviceHandle {
    pub(crate) const fn new(id: u32) -> Self {
        Self { id }
    }

    pub(crate) const fn id(self) -> u32 {
        self.id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BlockDeviceDescriptor {
    pub(crate) id: u32,
    pub(crate) path: String,
    pub(crate) transport: BlockTransportKind,
    pub(crate) readonly: bool,
    pub(crate) start_lba: u64,
    pub(crate) sector_count: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockTransportKind {
    Ahci,
    Nvme,
}

pub(crate) trait BlockDeviceOps: Send + Sync {
    fn transport_kind(&self) -> BlockTransportKind;
    fn sector_count(&self) -> u64;
    fn read_sector(&self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()>;
    fn write_sector(&self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()>;
    fn flush(&self) -> IoResult<()>;
    fn readonly(&self) -> bool;
}

enum BlockDeviceKind {
    Root(Arc<dyn BlockDeviceOps>),
    Slice {
        parent_id: u32,
        start_lba: u64,
        sectors: u64,
    },
}

struct BlockDeviceRecord {
    id: u32,
    path: String,
    transport: BlockTransportKind,
    readonly: bool,
    kind: BlockDeviceKind,
}

#[derive(Clone)]
struct BlockCacheEntry {
    device_id: u32,
    lba: u64,
    data: [u8; FAT_SECTOR_SIZE],
}

pub(crate) fn init() {
    ensure_initialized();
}

pub(crate) fn register_boot_volume_opener() {
    crate::storage::fat::set_boot_block_device_opener(open_boot_block_device);
    crate::storage::fat::set_physical_boot_block_device_opener(open_physical_boot_block_device);
}

fn ensure_initialized() {
    if BLOCK_INIT_DONE.load(Ordering::Acquire) {
        return;
    }

    let _guard = BLOCK_INIT_LOCK.lock();
    if BLOCK_INIT_DONE.load(Ordering::Relaxed) {
        return;
    }

    initialize_root_devices();
    BLOCK_INIT_DONE.store(true, Ordering::Release);
}

fn initialize_root_devices() {
    #[cfg(test)]
    {
        return;
    }

    #[cfg(not(test))]
    {
        let mut registered = 0usize;
        for device in crate::storage::ahci::probe_devices() {
            register_root_device(device);
            registered += 1;
        }

        crate::debug::println!("storage: registered {} block device(s)", registered);
    }
}

pub(crate) fn descriptors() -> Vec<BlockDeviceDescriptor> {
    ensure_initialized();
    let devices = BLOCK_DEVICES.lock();
    devices
        .iter()
        .map(|device| BlockDeviceDescriptor {
            id: device.id,
            path: device.path.clone(),
            transport: device.transport,
            readonly: device.readonly,
            start_lba: device_start_lba_locked(&devices, device.id).unwrap_or(0),
            sector_count: device_sector_count_locked(&devices, device.id).unwrap_or(0),
        })
        .collect()
}

pub(crate) fn lookup(path: &str) -> Option<BlockDeviceHandle> {
    ensure_initialized();
    let devices = BLOCK_DEVICES.lock();
    devices
        .iter()
        .find(|device| device.path == path)
        .map(|device| BlockDeviceHandle::new(device.id))
}

pub(crate) fn descriptor(handle: BlockDeviceHandle) -> Option<BlockDeviceDescriptor> {
    ensure_initialized();
    descriptor_without_init(handle)
}

fn descriptor_without_init(handle: BlockDeviceHandle) -> Option<BlockDeviceDescriptor> {
    let devices = BLOCK_DEVICES.lock();
    let record = devices.iter().find(|device| device.id == handle.id())?;
    Some(BlockDeviceDescriptor {
        id: record.id,
        path: record.path.clone(),
        transport: record.transport,
        readonly: record.readonly,
        start_lba: device_start_lba_locked(&devices, handle.id()).unwrap_or(0),
        sector_count: device_sector_count_locked(&devices, handle.id()).unwrap_or(0),
    })
}

pub(crate) fn read_sector(
    handle: BlockDeviceHandle,
    lba: u64,
    out: &mut [u8; FAT_SECTOR_SIZE],
) -> IoResult<()> {
    ensure_initialized();
    if let Some(cached) = cache_lookup(handle.id(), lba) {
        out.copy_from_slice(&cached);
        return Ok(());
    }

    read_sector_uncached(handle.id(), lba, out)?;
    cache_store(handle.id(), lba, out);
    Ok(())
}

pub(crate) fn write_sector(
    handle: BlockDeviceHandle,
    lba: u64,
    input: &[u8; FAT_SECTOR_SIZE],
) -> IoResult<()> {
    ensure_initialized();
    write_sector_uncached(handle.id(), lba, input)?;
    cache_store(handle.id(), lba, input);
    Ok(())
}

pub(crate) fn flush(handle: BlockDeviceHandle) -> IoResult<()> {
    ensure_initialized();
    flush_uncached(handle.id())
}

pub(crate) fn is_readonly(handle: BlockDeviceHandle) -> bool {
    descriptor(handle)
        .map(|device| device.readonly)
        .unwrap_or(true)
}

pub(crate) struct FatRegistryDevice {
    handle: BlockDeviceHandle,
}

impl FatRegistryDevice {
    pub(crate) fn new(handle: BlockDeviceHandle) -> Self {
        Self { handle }
    }
}

impl FatBlockDevice for FatRegistryDevice {
    fn sector_count(&self) -> u64 {
        descriptor(self.handle)
            .map(|device| device.sector_count)
            .unwrap_or(0)
    }

    fn read_sector(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        read_sector(self.handle, lba, out)
    }

    fn write_sector(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        write_sector(self.handle, lba, input)
    }

    fn flush(&mut self) -> IoResult<()> {
        flush(self.handle)
    }
}

fn open_boot_block_device(
) -> core::result::Result<Box<dyn FatBlockDevice>, fatfs::Error<DiskIoError>> {
    ensure_initialized();
    crate::debug::println!("storage: boot volume fallback opener invoked");

    for descriptor in descriptors() {
        let handle = BlockDeviceHandle::new(descriptor.id);
        crate::debug::println!(
            "storage: probing FAT candidate id={} path={} transport={:?} readonly={} start_lba={} sectors={}",
            descriptor.id,
            descriptor.path,
            descriptor.transport,
            descriptor.readonly,
            descriptor.start_lba,
            descriptor.sector_count
        );
        if fatfs::FileSystem::new(
            crate::storage::fat::FatDisk::new(FatRegistryDevice::new(handle)),
            fatfs::FsOptions::new(),
        )
        .is_ok()
        {
            crate::debug::println!(
                "storage: selected FAT boot candidate id={} path={} start_lba={} sectors={}",
                descriptor.id,
                descriptor.path,
                descriptor.start_lba,
                descriptor.sector_count
            );
            return Ok(Box::new(FatRegistryDevice::new(handle)) as Box<dyn FatBlockDevice>);
        }
        crate::debug::println!(
            "storage: rejected FAT candidate id={} path={}",
            descriptor.id,
            descriptor.path
        );
    }

    crate::debug::println!("storage: no FAT boot candidate matched");
    Err(fatfs::Error::Io(DiskIoError::NotPresent))
}

fn open_physical_boot_block_device(
    identity: BootVolumeIdentity,
) -> core::result::Result<Box<dyn FatBlockDevice>, fatfs::Error<DiskIoError>> {
    ensure_initialized();
    if !identity.is_present() {
        crate::debug::println!("storage: physical boot opener requested without identity");
        return Err(fatfs::Error::Io(DiskIoError::NotPresent));
    }
    crate::debug::println!(
        "storage: physical boot opener identity serial={:#010x} start_lba={} sectors={}",
        identity.fat_volume_id,
        identity.volume_start_lba,
        identity.volume_sector_count
    );

    for descriptor in descriptors() {
        if descriptor.start_lba != identity.volume_start_lba
            || descriptor.sector_count != identity.volume_sector_count
        {
            continue;
        }

        let handle = BlockDeviceHandle::new(descriptor.id);
        crate::debug::println!(
            "storage: physical opener candidate id={} path={} start_lba={} sectors={}",
            descriptor.id,
            descriptor.path,
            descriptor.start_lba,
            descriptor.sector_count
        );
        let mut sector0 = [0_u8; FAT_SECTOR_SIZE];
        if read_sector(handle, 0, &mut sector0).is_err() {
            crate::debug::println!(
                "storage: physical opener candidate id={} path={} sector0 read failed",
                descriptor.id,
                descriptor.path
            );
            continue;
        }
        if crate::storage::fat::fat_volume_id_from_boot_sector(&sector0)
            != Some(identity.fat_volume_id)
        {
            crate::debug::println!(
                "storage: physical opener candidate id={} path={} serial mismatch actual={:#010x}",
                descriptor.id,
                descriptor.path,
                crate::storage::fat::fat_volume_id_from_boot_sector(&sector0).unwrap_or(0)
            );
            continue;
        }

        crate::debug::println!(
            "storage: physical opener matched id={} path={}",
            descriptor.id,
            descriptor.path
        );
        return Ok(Box::new(FatRegistryDevice::new(handle)) as Box<dyn FatBlockDevice>);
    }

    crate::debug::println!("storage: physical boot opener found no exact match");
    Err(fatfs::Error::Io(DiskIoError::NotPresent))
}

fn register_root_device(device: Arc<dyn BlockDeviceOps>) {
    let root_id = {
        let mut devices = BLOCK_DEVICES.lock();
        let id = devices.len() as u32;
        let transport = device.transport_kind();
        let readonly = device.readonly();
        devices.push(BlockDeviceRecord {
            id,
            path: alloc::format!("/dev/block{id}"),
            transport,
            readonly,
            kind: BlockDeviceKind::Root(device.clone()),
        });
        id
    };

    register_partitions(root_id);
}

fn register_partitions(root_id: u32) {
    let partitions = match detect_partitions(root_id) {
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
                start_lba: partition.start_lba,
                sectors: partition.sectors,
            },
        });
    }
}

fn read_sector_uncached(device_id: u32, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
    let devices = BLOCK_DEVICES.lock();
    read_sector_from_records(&devices, device_id, lba, out)
}

fn write_sector_uncached(device_id: u32, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
    let devices = BLOCK_DEVICES.lock();
    write_sector_from_records(&devices, device_id, lba, input)
}

fn flush_uncached(device_id: u32) -> IoResult<()> {
    let devices = BLOCK_DEVICES.lock();
    flush_from_records(&devices, device_id)
}

fn device_sector_count_locked(devices: &[BlockDeviceRecord], device_id: u32) -> Option<u64> {
    let record = devices.iter().find(|device| device.id == device_id)?;
    match &record.kind {
        BlockDeviceKind::Root(device) => Some(device.sector_count()),
        BlockDeviceKind::Slice { sectors, .. } => Some(*sectors),
    }
}

fn device_start_lba_locked(devices: &[BlockDeviceRecord], device_id: u32) -> Option<u64> {
    let record = devices.iter().find(|device| device.id == device_id)?;
    match &record.kind {
        BlockDeviceKind::Root(_) => Some(0),
        BlockDeviceKind::Slice {
            parent_id,
            start_lba,
            ..
        } => Some(device_start_lba_locked(devices, *parent_id)?.saturating_add(*start_lba)),
    }
}

fn read_sector_from_records(
    devices: &[BlockDeviceRecord],
    device_id: u32,
    lba: u64,
    out: &mut [u8; FAT_SECTOR_SIZE],
) -> IoResult<()> {
    let record = devices
        .iter()
        .find(|device| device.id == device_id)
        .ok_or(DiskIoError::NotPresent)?;
    match &record.kind {
        BlockDeviceKind::Root(device) => device.read_sector(lba, out),
        BlockDeviceKind::Slice {
            parent_id,
            start_lba,
            sectors,
        } => {
            if lba >= *sectors {
                return Err(DiskIoError::InvalidInput);
            }
            read_sector_from_records(devices, *parent_id, start_lba + lba, out)
        }
    }
}

fn write_sector_from_records(
    devices: &[BlockDeviceRecord],
    device_id: u32,
    lba: u64,
    input: &[u8; FAT_SECTOR_SIZE],
) -> IoResult<()> {
    let record = devices
        .iter()
        .find(|device| device.id == device_id)
        .ok_or(DiskIoError::NotPresent)?;
    if record.readonly {
        return Err(DiskIoError::InvalidInput);
    }
    match &record.kind {
        BlockDeviceKind::Root(device) => device.write_sector(lba, input),
        BlockDeviceKind::Slice {
            parent_id,
            start_lba,
            sectors,
        } => {
            if lba >= *sectors {
                return Err(DiskIoError::InvalidInput);
            }
            write_sector_from_records(devices, *parent_id, start_lba + lba, input)
        }
    }
}

fn flush_from_records(devices: &[BlockDeviceRecord], device_id: u32) -> IoResult<()> {
    let record = devices
        .iter()
        .find(|device| device.id == device_id)
        .ok_or(DiskIoError::NotPresent)?;
    match &record.kind {
        BlockDeviceKind::Root(device) => device.flush(),
        BlockDeviceKind::Slice { parent_id, .. } => flush_from_records(devices, *parent_id),
    }
}

fn cache_lookup(device_id: u32, lba: u64) -> Option<[u8; FAT_SECTOR_SIZE]> {
    let cache = BLOCK_CACHE.lock();
    cache
        .iter()
        .find(|entry| entry.device_id == device_id && entry.lba == lba)
        .map(|entry| entry.data)
}

fn cache_store(device_id: u32, lba: u64, data: &[u8; FAT_SECTOR_SIZE]) {
    let mut cache = BLOCK_CACHE.lock();
    if let Some(entry) = cache
        .iter_mut()
        .find(|entry| entry.device_id == device_id && entry.lba == lba)
    {
        entry.data.copy_from_slice(data);
        return;
    }
    if cache.len() >= BLOCK_CACHE_CAPACITY {
        cache.remove(0);
    }
    cache.push(BlockCacheEntry {
        device_id,
        lba,
        data: *data,
    });
}

#[derive(Clone, Copy)]
struct PartitionInfo {
    start_lba: u64,
    sectors: u64,
}

fn detect_partitions(root_id: u32) -> IoResult<Vec<PartitionInfo>> {
    let total = descriptor_without_init(BlockDeviceHandle::new(root_id))
        .map(|device| device.sector_count)
        .ok_or(DiskIoError::NotPresent)?;
    if total == 0 {
        return Ok(Vec::new());
    }

    let mut sector0 = [0_u8; FAT_SECTOR_SIZE];
    read_sector_uncached(root_id, 0, &mut sector0)?;

    let mut partitions = detect_gpt_partitions(root_id, total)?;
    if !partitions.is_empty() {
        return Ok(partitions);
    }

    let sig_ok = sector0[FAT_SECTOR_SIZE - 2] == 0x55 && sector0[FAT_SECTOR_SIZE - 1] == 0xAA;
    if !sig_ok {
        return Ok(Vec::new());
    }

    for idx in 0..4 {
        let off = MBR_PARTITION_TABLE_OFFSET + idx * MBR_PARTITION_ENTRY_LEN;
        let part_type = sector0[off + 4];
        if part_type == 0 || part_type == 0xEE {
            continue;
        }

        let start_lba = le_u32(&sector0, off + 8) as u64;
        let sectors = le_u32(&sector0, off + 12) as u64;
        let Some(end_lba) = start_lba.checked_add(sectors) else {
            continue;
        };
        if start_lba == 0 || sectors == 0 || end_lba > total {
            continue;
        }
        partitions.push(PartitionInfo { start_lba, sectors });
    }

    Ok(partitions)
}

fn detect_gpt_partitions(root_id: u32, total_sectors: u64) -> IoResult<Vec<PartitionInfo>> {
    if total_sectors <= GPT_HEADER_LBA {
        return Ok(Vec::new());
    }

    let mut header = [0_u8; FAT_SECTOR_SIZE];
    read_sector_uncached(root_id, GPT_HEADER_LBA, &mut header)?;
    if header[..GPT_SIGNATURE.len()] != GPT_SIGNATURE {
        return Ok(Vec::new());
    }

    let header_size = le_u32(&header, 12) as usize;
    if !(92..=FAT_SECTOR_SIZE).contains(&header_size) {
        return Ok(Vec::new());
    }

    let entries_lba = le_u64(&header, 72);
    let entry_count = le_u32(&header, 80) as u64;
    let entry_size = le_u32(&header, 84) as u64;
    if entry_count == 0 || entry_size < 128 || entry_size % 8 != 0 {
        return Ok(Vec::new());
    }

    let table_offset = entries_lba
        .checked_mul(FAT_SECTOR_SIZE as u64)
        .ok_or(DiskIoError::InvalidInput)?;
    let table_bytes = entry_count
        .checked_mul(entry_size)
        .ok_or(DiskIoError::InvalidInput)?;
    let table_end = table_offset
        .checked_add(table_bytes)
        .ok_or(DiskIoError::InvalidInput)?;
    let disk_bytes = total_sectors
        .checked_mul(FAT_SECTOR_SIZE as u64)
        .ok_or(DiskIoError::InvalidInput)?;
    if table_end > disk_bytes {
        return Ok(Vec::new());
    }

    let mut partitions = Vec::new();
    let mut entry_prefix = [0_u8; 48];
    for index in 0..entry_count {
        let Some(entry_offset) = index
            .checked_mul(entry_size)
            .and_then(|offset| table_offset.checked_add(offset))
        else {
            continue;
        };
        read_device_bytes(root_id, entry_offset, &mut entry_prefix)?;
        if entry_prefix[..16].iter().all(|byte| *byte == 0) {
            continue;
        }

        let first_lba = le_u64(&entry_prefix, 32);
        let last_lba = le_u64(&entry_prefix, 40);
        let Some(sectors) = last_lba
            .checked_sub(first_lba)
            .and_then(|delta| delta.checked_add(1))
        else {
            continue;
        };
        let Some(end_lba) = first_lba.checked_add(sectors) else {
            continue;
        };
        if first_lba == 0 || end_lba > total_sectors {
            continue;
        }
        partitions.push(PartitionInfo {
            start_lba: first_lba,
            sectors,
        });
    }

    Ok(partitions)
}

fn read_device_bytes(device_id: u32, offset: u64, out: &mut [u8]) -> IoResult<()> {
    if out.is_empty() {
        return Ok(());
    }

    let mut scratch = [0_u8; FAT_SECTOR_SIZE];
    let mut copied = 0usize;
    while copied < out.len() {
        let absolute = offset
            .checked_add(copied as u64)
            .ok_or(DiskIoError::InvalidInput)?;
        let lba = absolute / FAT_SECTOR_SIZE as u64;
        let sector_offset = (absolute as usize) % FAT_SECTOR_SIZE;
        read_sector_uncached(device_id, lba, &mut scratch)?;

        let chunk = min(FAT_SECTOR_SIZE - sector_offset, out.len() - copied);
        out[copied..copied + chunk].copy_from_slice(&scratch[sector_offset..sector_offset + chunk]);
        copied += chunk;
    }

    Ok(())
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec;
    use boot_protocol::BootVolumeIdentity;
    use core::sync::atomic::Ordering;
    use spin::Mutex;

    use super::{
        cache_lookup, descriptors, flush, lookup, open_physical_boot_block_device, read_sector,
        register_root_device, write_sector, BlockDeviceOps, BlockTransportKind, BLOCK_CACHE,
        BLOCK_DEVICES, BLOCK_INIT_DONE, FAT_SECTOR_SIZE, MBR_PARTITION_TABLE_OFFSET,
    };
    use crate::storage::fat::{DiskIoError, IoResult};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct MockBlockDevice {
        sectors: Mutex<Vec<[u8; FAT_SECTOR_SIZE]>>,
        readonly: bool,
    }

    impl MockBlockDevice {
        fn new(sector_count: usize, readonly: bool) -> Self {
            Self {
                sectors: Mutex::new(vec![[0_u8; FAT_SECTOR_SIZE]; sector_count]),
                readonly,
            }
        }

        fn with_mbr_partition(start_lba: u32, sectors: u32, readonly: bool) -> Self {
            let device = Self::new((start_lba + sectors + 2) as usize, readonly);
            {
                let mut all_sectors = device.sectors.lock();
                let sector0 = &mut all_sectors[0];
                sector0[FAT_SECTOR_SIZE - 2] = 0x55;
                sector0[FAT_SECTOR_SIZE - 1] = 0xAA;
                let off = MBR_PARTITION_TABLE_OFFSET;
                sector0[off + 4] = 0x83;
                sector0[off + 8..off + 12].copy_from_slice(&start_lba.to_le_bytes());
                sector0[off + 12..off + 16].copy_from_slice(&sectors.to_le_bytes());
            }
            device
        }

        fn with_fat_partition(
            start_lba: u32,
            sectors: u32,
            volume_id: u32,
            readonly: bool,
        ) -> Self {
            let device = Self::with_mbr_partition(start_lba, sectors, readonly);
            {
                let mut all_sectors = device.sectors.lock();
                all_sectors[start_lba as usize] =
                    fat_boot_sector(sectors as u16, FAT_SECTOR_SIZE as u16, volume_id);
            }
            device
        }
    }

    impl BlockDeviceOps for MockBlockDevice {
        fn transport_kind(&self) -> BlockTransportKind {
            BlockTransportKind::Ahci
        }

        fn sector_count(&self) -> u64 {
            self.sectors.lock().len() as u64
        }

        fn read_sector(&self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
            let sectors = self.sectors.lock();
            let Some(data) = sectors.get(lba as usize) else {
                return Err(DiskIoError::InvalidInput);
            };
            out.copy_from_slice(data);
            Ok(())
        }

        fn write_sector(&self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
            if self.readonly {
                return Err(DiskIoError::InvalidInput);
            }
            let mut sectors = self.sectors.lock();
            let Some(data) = sectors.get_mut(lba as usize) else {
                return Err(DiskIoError::InvalidInput);
            };
            data.copy_from_slice(input);
            Ok(())
        }

        fn flush(&self) -> IoResult<()> {
            Ok(())
        }

        fn readonly(&self) -> bool {
            self.readonly
        }
    }

    fn reset_for_tests() {
        BLOCK_DEVICES.lock().clear();
        BLOCK_CACHE.lock().clear();
        BLOCK_INIT_DONE.store(true, Ordering::Release);
    }

    fn fat_boot_sector(
        total_sectors: u16,
        bytes_per_sector: u16,
        volume_id: u32,
    ) -> [u8; FAT_SECTOR_SIZE] {
        let mut sector = [0_u8; FAT_SECTOR_SIZE];
        sector[0] = 0xEB;
        sector[2] = 0x90;
        sector[11..13].copy_from_slice(&bytes_per_sector.to_le_bytes());
        sector[13] = 1;
        sector[14..16].copy_from_slice(&1_u16.to_le_bytes());
        sector[16] = 2;
        sector[17..19].copy_from_slice(&32_u16.to_le_bytes());
        sector[19..21].copy_from_slice(&total_sectors.to_le_bytes());
        sector[21] = 0xF8;
        sector[22..24].copy_from_slice(&1_u16.to_le_bytes());
        sector[39..43].copy_from_slice(&volume_id.to_le_bytes());
        sector[510] = 0x55;
        sector[511] = 0xAA;
        sector
    }

    #[test]
    fn register_root_device_creates_partition_nodes() {
        let _guard = TEST_LOCK.lock();
        reset_for_tests();

        register_root_device(Arc::new(MockBlockDevice::with_mbr_partition(1, 3, false)));

        let descriptors = descriptors();
        assert_eq!(descriptors.len(), 2);
        assert_eq!(descriptors[0].path, "/dev/block0");
        assert_eq!(descriptors[1].path, "/dev/block0p1");
    }

    #[test]
    fn partition_writes_are_forwarded_to_parent_sectors_and_cached() {
        let _guard = TEST_LOCK.lock();
        reset_for_tests();

        register_root_device(Arc::new(MockBlockDevice::with_mbr_partition(2, 2, false)));

        let root = lookup("/dev/block0").expect("root block device");
        let partition = lookup("/dev/block0p1").expect("partition block device");

        let mut sector = [0_u8; FAT_SECTOR_SIZE];
        sector[0] = 0xAA;
        sector[FAT_SECTOR_SIZE - 1] = 0x55;
        write_sector(partition, 0, &sector).expect("partition write");
        flush(partition).expect("flush partition");

        let mut root_sector = [0_u8; FAT_SECTOR_SIZE];
        read_sector(root, 2, &mut root_sector).expect("read parent sector");
        assert_eq!(root_sector, sector);
        assert_eq!(cache_lookup(root.id(), 2), Some(sector));
    }

    #[test]
    fn readonly_partition_inherits_parent_readonly_state() {
        let _guard = TEST_LOCK.lock();
        reset_for_tests();

        register_root_device(Arc::new(MockBlockDevice::with_mbr_partition(1, 2, true)));
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
        register_root_device(Arc::new(MockBlockDevice::with_fat_partition(
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
}
