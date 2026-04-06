use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use boot_protocol::{BootVolumeIdentity, BootVolumeTransport};
use core::mem;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::Mutex;
use storage_core::{
    BlockDevice as SharedBlockDevice, BootVolumeLocator, PartitionInfo as SharedPartitionInfo,
};

use crate::storage::fat::{DiskIoError, IoResult};

pub(crate) use storage_core::TransportKind as BlockTransportKind;

const BLOCK_CACHE_CAPACITY: usize = 256;
const MIN_LOGICAL_BLOCK_SIZE: usize = 512;
#[cfg(test)]
const MBR_PARTITION_TABLE_OFFSET: usize = 446;

static BLOCK_DEVICES: Mutex<Vec<BlockDeviceRecord>> = Mutex::new(Vec::new());
static BLOCK_CACHE: Mutex<Vec<BlockCacheEntry>> = Mutex::new(Vec::new());
static BLOCK_INIT_DONE: AtomicBool = AtomicBool::new(false);
static BLOCK_INIT_LOCK: Mutex<()> = Mutex::new(());
static BOOT_FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);
static READ_BLOCKS_TRACE_BUDGET: AtomicUsize = AtomicUsize::new(0);

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
    pub(crate) logical_block_size: usize,
    pub(crate) start_block: u64,
    pub(crate) block_count: u64,
}

pub(crate) trait BlockDeviceOps: SharedBlockDevice + Send {
    fn transport_kind(&self) -> BlockTransportKind;
    fn readonly(&self) -> bool;
}

enum BlockDeviceKind {
    Root(Arc<Mutex<Box<dyn BlockDeviceOps>>>),
    Slice {
        parent_id: u32,
        start_block: u64,
        block_count: u64,
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
    data: Vec<u8>,
}

#[derive(Clone)]
struct ResolvedRootDevice {
    device: Arc<Mutex<Box<dyn BlockDeviceOps>>>,
    readonly: bool,
    logical_block_size: usize,
    block_count: u64,
    start_block: u64,
}

struct RegistryRootBlockDevice {
    root_id: u32,
    logical_block_size: usize,
    block_count: u64,
}

pub(crate) fn init() {
    ensure_initialized();
}

pub(crate) fn register_boot_volume_opener() {
    crate::storage::boot_volume::set_boot_block_device_opener(open_boot_block_device);
    crate::storage::boot_volume::set_physical_boot_block_device_opener(
        open_physical_boot_block_device,
    );
}

pub(crate) fn current_boot_volume_handle() -> Option<BlockDeviceHandle> {
    ensure_initialized();

    if let Some(identity) = crate::storage::boot_volume::boot_volume_identity() {
        if let Ok(handle) = open_physical_boot_handle(identity) {
            return Some(handle);
        }
    }

    open_boot_handle().ok()
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
        let mut _registered = 0usize;
        for device in crate::storage::ahci::probe_devices() {
            register_root_device(device);
            _registered += 1;
        }
        for device in crate::storage::nvme::probe_devices() {
            register_root_device(device);
            _registered += 1;
        }

        crate::debug::println!("storage: registered {} block device(s)", _registered);
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
            logical_block_size: device_logical_block_size_locked(&devices, device.id).unwrap_or(0),
            start_block: device_start_block_locked(&devices, device.id).unwrap_or(0),
            block_count: device_block_count_locked(&devices, device.id).unwrap_or(0),
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
        logical_block_size: device_logical_block_size_locked(&devices, handle.id()).unwrap_or(0),
        start_block: device_start_block_locked(&devices, handle.id()).unwrap_or(0),
        block_count: device_block_count_locked(&devices, handle.id()).unwrap_or(0),
    })
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

impl SharedBlockDevice for FatRegistryDevice {
    fn logical_block_size(&self) -> usize {
        descriptor(self.handle)
            .map(|device| device.logical_block_size)
            .unwrap_or(0)
    }

    fn block_count(&self) -> u64 {
        descriptor(self.handle)
            .map(|device| device.block_count)
            .unwrap_or(0)
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        let block_size = self.logical_block_size();
        validate_block_io_exact(block_size, lba, self.block_count(), out.len())?;
        if out.len() > block_size {
            // Large FAT streaming reads are latency-sensitive during service and ELF loads.
            // The single-block cache is useful for metadata walks, but using it for every
            // sector of a multi-block read turns one sequential request into many registry
            // lookups, cache locks, and root-device lock acquisitions.
            return read_blocks_uncached(self.handle.id(), lba, out);
        }
        for (index, chunk) in out.chunks_exact_mut(block_size).enumerate() {
            read_cached_block(self.handle.id(), lba + index as u64, chunk)?;
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        let block_size = self.logical_block_size();
        validate_block_io_exact(block_size, lba, self.block_count(), input.len())?;
        if input.len() > block_size {
            let blocks = input.len() / block_size;
            write_blocks_uncached(self.handle.id(), lba, input)?;
            for (index, chunk) in input.chunks_exact(block_size).enumerate().take(blocks) {
                cache_store(self.handle.id(), lba + index as u64, chunk);
            }
            return Ok(());
        }
        for (index, chunk) in input.chunks_exact(block_size).enumerate() {
            write_cached_block(self.handle.id(), lba + index as u64, chunk)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> IoResult<()> {
        flush(self.handle)
    }
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn open_boot_block_device()
-> core::result::Result<Box<dyn SharedBlockDevice>, fatfs::Error<DiskIoError>> {
    let handle = open_boot_handle().map_err(fatfs::Error::Io)?;
    Ok(Box::new(FatRegistryDevice::new(handle)) as Box<dyn SharedBlockDevice>)
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn open_physical_boot_block_device(
    identity: BootVolumeIdentity,
) -> core::result::Result<Box<dyn SharedBlockDevice>, fatfs::Error<DiskIoError>> {
    let handle = open_physical_boot_handle(identity).map_err(fatfs::Error::Io)?;
    Ok(Box::new(FatRegistryDevice::new(handle)) as Box<dyn SharedBlockDevice>)
}

fn register_root_device(device: Box<dyn BlockDeviceOps>) {
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
            kind: BlockDeviceKind::Root(Arc::new(Mutex::new(device))),
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
                start_block: partition.start_lba,
                block_count: partition.block_count,
            },
        });
    }
}

fn read_blocks_uncached(device_id: u32, lba: u64, out: &mut [u8]) -> IoResult<()> {
    let resolved = resolve_root_device(device_id).ok_or(DiskIoError::NotPresent)?;
    validate_block_io_exact(
        resolved.logical_block_size,
        lba,
        resolved.block_count,
        out.len(),
    )?;
    let trace = READ_BLOCKS_TRACE_BUDGET
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |budget| {
            budget.checked_sub(1)
        })
        .is_ok();
    if trace {
        crate::debug::println!(
            "storage uncached read: begin dev={} lba={} bytes={} start_block={} block_size={} blocks={}",
            device_id,
            lba,
            out.len(),
            resolved.start_block,
            resolved.logical_block_size,
            resolved.block_count
        );
    }
    let mut device = resolved.device.lock();
    if trace {
        let raw: *const dyn BlockDeviceOps = &**device;
        let (data_ptr, vtable_ptr): (usize, usize) = unsafe { mem::transmute(raw) };
        crate::debug::println!(
            "storage uncached read: dispatch dev={} data_ptr={:#x} vtable_ptr={:#x} abs_lba={}",
            device_id,
            data_ptr,
            vtable_ptr,
            resolved.start_block + lba
        );
    }
    let result = device.read_blocks(resolved.start_block + lba, out);
    if trace {
        crate::debug::println!(
            "storage uncached read: end dev={} ok={}",
            device_id,
            result.is_ok()
        );
    }
    result
}

fn write_blocks_uncached(device_id: u32, lba: u64, input: &[u8]) -> IoResult<()> {
    let resolved = resolve_root_device(device_id).ok_or(DiskIoError::NotPresent)?;
    if resolved.readonly {
        return Err(DiskIoError::InvalidInput);
    }
    validate_block_io_exact(
        resolved.logical_block_size,
        lba,
        resolved.block_count,
        input.len(),
    )?;
    let mut device = resolved.device.lock();
    device.write_blocks(resolved.start_block + lba, input)
}

fn flush_uncached(device_id: u32) -> IoResult<()> {
    let resolved = resolve_root_device(device_id).ok_or(DiskIoError::NotPresent)?;
    resolved.device.lock().flush()
}

fn device_logical_block_size_locked(
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

fn device_block_count_locked(devices: &[BlockDeviceRecord], device_id: u32) -> Option<u64> {
    let record = devices.iter().find(|device| device.id == device_id)?;
    match &record.kind {
        BlockDeviceKind::Root(device) => Some(device.lock().block_count()),
        BlockDeviceKind::Slice { block_count, .. } => Some(*block_count),
    }
}

fn device_start_block_locked(devices: &[BlockDeviceRecord], device_id: u32) -> Option<u64> {
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

fn device_root_id_locked(devices: &[BlockDeviceRecord], device_id: u32) -> Option<u32> {
    let record = devices.iter().find(|device| device.id == device_id)?;
    match &record.kind {
        BlockDeviceKind::Root(_) => Some(record.id),
        BlockDeviceKind::Slice { parent_id, .. } => device_root_id_locked(devices, *parent_id),
    }
}

fn root_device_ids_locked(devices: &[BlockDeviceRecord]) -> Vec<u32> {
    devices
        .iter()
        .filter_map(|device| match &device.kind {
            BlockDeviceKind::Root(_) => Some(device.id),
            BlockDeviceKind::Slice { .. } => None,
        })
        .collect()
}

fn sort_root_ids_by_transport_hint(root_ids: &mut [u32], transport_hint: BootVolumeTransport) {
    if transport_hint == BootVolumeTransport::Unknown {
        return;
    }

    root_ids.sort_by_key(|root_id| {
        let device_transport = descriptor_without_init(BlockDeviceHandle::new(*root_id))
            .map(|descriptor| boot_transport_from_block(descriptor.transport))
            .unwrap_or(BootVolumeTransport::Unknown);
        (device_transport != transport_hint) as u8
    });
}

fn boot_transport_from_block(transport: BlockTransportKind) -> BootVolumeTransport {
    match transport {
        BlockTransportKind::Ahci => BootVolumeTransport::Ahci,
        BlockTransportKind::Nvme => BootVolumeTransport::Nvme,
        BlockTransportKind::Usb => BootVolumeTransport::Usb,
    }
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn open_boot_handle() -> IoResult<BlockDeviceHandle> {
    ensure_initialized();
    let should_log = !BOOT_FALLBACK_LOGGED.swap(true, Ordering::AcqRel);
    if should_log {
        crate::debug::println!("storage: boot volume fallback opener invoked");
    }
    let transport_hint = crate::storage::boot_volume::boot_volume_transport_hint()
        .unwrap_or(BootVolumeTransport::Unknown);

    let mut root_ids = {
        let devices = BLOCK_DEVICES.lock();
        root_device_ids_locked(&devices)
    };
    sort_root_ids_by_transport_hint(&mut root_ids, transport_hint);
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
fn open_physical_boot_handle(identity: BootVolumeIdentity) -> IoResult<BlockDeviceHandle> {
    ensure_initialized();
    let Some(locator) = BootVolumeLocator::new(identity) else {
        crate::debug::println!("storage: physical boot opener requested without identity");
        return Err(DiskIoError::NotPresent);
    };
    crate::debug::println!(
        "storage: physical boot opener identity transport={:?} serial={:#010x} start_lba={} sectors={}",
        identity.transport(),
        identity.fat_volume_id,
        identity.volume_start_lba,
        identity.volume_sector_count
    );

    let mut root_ids = {
        let devices = BLOCK_DEVICES.lock();
        root_device_ids_locked(&devices)
    };
    sort_root_ids_by_transport_hint(&mut root_ids, identity.transport());

    for root_id in root_ids {
        let Some(descriptor) = descriptor_without_init(BlockDeviceHandle::new(root_id)) else {
            continue;
        };

        crate::debug::println!(
            "storage: physical opener candidate id={} path={} block_size={} start_block={} blocks={}",
            root_id,
            descriptor.path,
            descriptor.logical_block_size,
            descriptor.start_block,
            descriptor.block_count
        );

        let mut root = RegistryRootBlockDevice {
            root_id,
            logical_block_size: descriptor.logical_block_size,
            block_count: descriptor.block_count,
        };
        let partitions = match candidate_partitions(&mut root) {
            Ok(partitions) => partitions,
            Err(err) => {
                crate::debug::println!(
                    "storage: physical opener candidate id={} path={} partition scan error={:?}",
                    root_id,
                    descriptor.path,
                    err
                );
                continue;
            }
        };

        for partition in partitions {
            let is_match = match locator.matches_partition(&mut root, partition) {
                Ok(result) => result,
                Err(err) => {
                    crate::debug::println!(
                        "storage: physical opener candidate id={} path={} identity probe error={:?}",
                        root_id,
                        descriptor.path,
                        err
                    );
                    break;
                }
            };
            if !is_match {
                continue;
            }

            let Some(handle) = find_device_handle_for_partition(root_id, partition) else {
                crate::debug::println!(
                    "storage: physical opener candidate id={} path={} matched but no handle was registered",
                    root_id,
                    descriptor.path
                );
                break;
            };

            if let Some(selected) = descriptor_without_init(handle) {
                crate::debug::println!(
                    "storage: physical boot opener matched id={} path={}",
                    selected.id,
                    selected.path
                );
            }
            return Ok(handle);
        }
    }

    crate::debug::println!("storage: physical boot opener found no exact match");
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
            device_root_id_locked(&devices, device.id) == Some(root_id)
                && device_start_block_locked(&devices, device.id) == Some(partition.start_lba)
                && device_block_count_locked(&devices, device.id) == Some(partition.block_count)
        })
        .map(|device| BlockDeviceHandle::new(device.id))
}

fn read_blocks_from_records(
    devices: &[BlockDeviceRecord],
    device_id: u32,
    lba: u64,
    out: &mut [u8],
) -> IoResult<()> {
    let resolved = resolve_root_device_locked(devices, device_id).ok_or(DiskIoError::NotPresent)?;
    validate_block_io_exact(
        resolved.logical_block_size,
        lba,
        resolved.block_count,
        out.len(),
    )?;
    let mut device = resolved.device.lock();
    device.read_blocks(resolved.start_block + lba, out)
}

fn write_blocks_from_records(
    devices: &[BlockDeviceRecord],
    device_id: u32,
    lba: u64,
    input: &[u8],
) -> IoResult<()> {
    let resolved = resolve_root_device_locked(devices, device_id).ok_or(DiskIoError::NotPresent)?;
    if resolved.readonly {
        return Err(DiskIoError::InvalidInput);
    }
    validate_block_io_exact(
        resolved.logical_block_size,
        lba,
        resolved.block_count,
        input.len(),
    )?;
    let mut device = resolved.device.lock();
    device.write_blocks(resolved.start_block + lba, input)
}

fn flush_from_records(devices: &[BlockDeviceRecord], device_id: u32) -> IoResult<()> {
    let resolved = resolve_root_device_locked(devices, device_id).ok_or(DiskIoError::NotPresent)?;
    resolved.device.lock().flush()
}

fn resolve_root_device(device_id: u32) -> Option<ResolvedRootDevice> {
    let devices = BLOCK_DEVICES.lock();
    resolve_root_device_locked(&devices, device_id)
}

fn resolve_root_device_locked(
    devices: &[BlockDeviceRecord],
    device_id: u32,
) -> Option<ResolvedRootDevice> {
    let record = devices.iter().find(|device| device.id == device_id)?;
    match &record.kind {
        BlockDeviceKind::Root(device) => {
            let (logical_block_size, block_count) = {
                let device = device.lock();
                (device.logical_block_size(), device.block_count())
            };
            Some(ResolvedRootDevice {
                device: Arc::clone(device),
                readonly: record.readonly,
                logical_block_size,
                block_count,
                start_block: 0,
            })
        }
        BlockDeviceKind::Slice {
            parent_id,
            start_block,
            block_count,
        } => {
            let mut resolved = resolve_root_device_locked(devices, *parent_id)?;
            resolved.readonly |= record.readonly;
            resolved.start_block = resolved.start_block.saturating_add(*start_block);
            resolved.block_count = (*block_count).min(resolved.block_count);
            Some(resolved)
        }
    }
}

fn validate_block_io_exact(
    block_size: usize,
    lba: u64,
    total_blocks: u64,
    len: usize,
) -> IoResult<()> {
    if block_size < MIN_LOGICAL_BLOCK_SIZE || len == 0 || len % block_size != 0 {
        return Err(DiskIoError::InvalidInput);
    }
    let blocks = (len / block_size) as u64;
    let end = lba.checked_add(blocks).ok_or(DiskIoError::InvalidInput)?;
    if end > total_blocks {
        return Err(DiskIoError::InvalidInput);
    }
    Ok(())
}

fn read_cached_block(device_id: u32, lba: u64, out: &mut [u8]) -> IoResult<()> {
    {
        let cache = BLOCK_CACHE.lock();
        if let Some(entry) = cache
            .iter()
            .find(|entry| entry.device_id == device_id && entry.lba == lba)
            && entry.data.len() == out.len()
        {
            out.copy_from_slice(&entry.data);
            return Ok(());
        }
    }
    read_blocks_uncached(device_id, lba, out)?;
    cache_store(device_id, lba, out);
    Ok(())
}

fn write_cached_block(device_id: u32, lba: u64, input: &[u8]) -> IoResult<()> {
    write_blocks_uncached(device_id, lba, input)?;
    cache_store(device_id, lba, input);
    Ok(())
}

fn cache_store(device_id: u32, lba: u64, data: &[u8]) {
    let mut cache = BLOCK_CACHE.lock();
    if let Some(entry) = cache
        .iter_mut()
        .find(|entry| entry.device_id == device_id && entry.lba == lba)
    {
        entry.data.clear();
        entry.data.extend_from_slice(data);
        return;
    }
    if cache.len() >= BLOCK_CACHE_CAPACITY {
        cache.remove(0);
    }
    cache.push(BlockCacheEntry {
        device_id,
        lba,
        data: data.to_vec(),
    });
}

#[cfg(test)]
fn cache_lookup(device_id: u32, lba: u64) -> Option<Vec<u8>> {
    BLOCK_CACHE
        .lock()
        .iter()
        .find(|entry| entry.device_id == device_id && entry.lba == lba)
        .map(|entry| entry.data.clone())
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

fn detect_partitions(root_id: u32) -> IoResult<Vec<SharedPartitionInfo>> {
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
        validate_block_io_exact(self.logical_block_size, lba, self.block_count, out.len())?;
        read_blocks_uncached(self.root_id, lba, out)
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> storage_core::IoResult<()> {
        validate_block_io_exact(self.logical_block_size, lba, self.block_count, input.len())?;
        write_blocks_uncached(self.root_id, lba, input)
    }

    fn flush(&mut self) -> storage_core::IoResult<()> {
        flush_uncached(self.root_id)
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;
    use boot_protocol::BootVolumeIdentity;
    use core::sync::atomic::Ordering;
    use spin::Mutex;
    use storage_core::BlockDevice as SharedBlockDevice;

    use super::{
        BLOCK_CACHE, BLOCK_DEVICES, BLOCK_INIT_DONE, BlockDeviceOps, BlockTransportKind,
        MBR_PARTITION_TABLE_OFFSET, MIN_LOGICAL_BLOCK_SIZE, cache_lookup, descriptors, flush,
        lookup, open_boot_block_device, open_physical_boot_block_device, read_cached_block,
        register_root_device, write_cached_block,
    };
    use crate::storage::fat::{DiskIoError, IoResult};

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
        BLOCK_CACHE.lock().clear();
        BLOCK_INIT_DONE.store(true, Ordering::Release);
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
}
