// RING3-MIGRATION-REFERENCE START: bootstrap exception: storaged owns
// post-bootstrap inventory/root selection. Ring0 keeps physical descriptor
// publication and current boot-volume transport substrate for early boot and
// gated block brokers.
mod boot;
mod io;
mod registry;
#[cfg(test)]
mod tests;

use crate::sync::KernelSpinLock as Mutex;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use boot_protocol::BootVolumeIdentity;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use storage_core::BlockDevice as SharedBlockDevice;

use crate::storage::fat::{DiskIoError, IoResult};
use crate::sync::KernelWaitLock;

pub use storage_core::TransportKind as BlockTransportKind;

const MIN_LOGICAL_BLOCK_SIZE: usize = 512;
const BLOCK_INIT_UNINITIALIZED: u8 = 0;
const BLOCK_INIT_INITIALIZING: u8 = 1;
const BLOCK_INIT_DONE: u8 = 2;

static BLOCK_DEVICES: Mutex<Vec<BlockDeviceRecord>> = Mutex::new(Vec::new());
static BLOCK_INIT_STATE: AtomicU8 = AtomicU8::new(BLOCK_INIT_UNINITIALIZED);
static BOOT_VOLUME_HANDLE_LOGGED: AtomicBool = AtomicBool::new(false);
static BOOT_VOLUME_HANDLE_ID: AtomicU32 = AtomicU32::new(u32::MAX);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockDeviceHandle {
    id: u32,
}

impl BlockDeviceHandle {
    pub const fn new(id: u32) -> Self {
        Self { id }
    }

    pub const fn id(self) -> u32 {
        self.id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockDeviceDescriptor {
    pub id: u32,
    pub path: String,
    pub transport: BlockTransportKind,
    pub readonly: bool,
    pub logical_block_size: usize,
    pub start_block: u64,
    pub block_count: u64,
}

pub(crate) trait BlockDeviceOps: SharedBlockDevice + Send {
    fn transport_kind(&self) -> BlockTransportKind;
    fn readonly(&self) -> bool;
}

enum BlockDeviceKind {
    Root(Arc<KernelWaitLock<Box<dyn BlockDeviceOps>>>),
    Slice { parent_id: u32 },
}

struct BlockDeviceRecord {
    id: u32,
    path: String,
    transport: BlockTransportKind,
    readonly: bool,
    logical_block_size: usize,
    start_block: u64,
    block_count: u64,
    root_id: u32,
    kind: BlockDeviceKind,
}

pub fn init() {
    ensure_initialized();
}

pub fn register_boot_volume_opener() {
    crate::storage::boot_volume::set_physical_boot_block_device_opener(
        open_physical_boot_block_device,
    );
}

pub fn current_boot_volume_handle() -> Option<BlockDeviceHandle> {
    ensure_initialized();
    if let Some(handle) = cached_boot_volume_handle() {
        return Some(handle);
    }

    match crate::storage::boot_volume::boot_volume_identity() {
        Some(identity) => {
            if let Ok(handle) = boot::open_physical_boot_handle(identity) {
                return Some(cache_boot_volume_handle(handle));
            }
        }
        None if crate::storage::boot_volume::boot_extent_manifest_present() => {
            if let Ok(handle) = boot::open_unambiguous_manifest_boot_handle() {
                return Some(cache_boot_volume_handle(handle));
            }
        }
        None => {}
    }
    None
}

fn ensure_initialized() {
    if BLOCK_INIT_STATE.load(Ordering::Acquire) == BLOCK_INIT_DONE {
        return;
    }

    if BLOCK_INIT_STATE
        .compare_exchange(
            BLOCK_INIT_UNINITIALIZED,
            BLOCK_INIT_INITIALIZING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        initialize_root_devices();
        io::publish_block_devices_snapshot_once();
        BLOCK_INIT_STATE.store(BLOCK_INIT_DONE, Ordering::Release);
        return;
    }

    while BLOCK_INIT_STATE.load(Ordering::Acquire) != BLOCK_INIT_DONE {
        spin_loop();
        crate::multitask::cond_resched();
    }
}

fn initialize_root_devices() {
    #[cfg(test)]
    {
        return;
    }

    #[cfg(not(test))]
    {
        // Boot only requires the transport that actually loaded the image.
        // Probe that first so an unrelated controller cannot stall userspace startup.
        let transport_hint = crate::storage::boot_volume::boot_volume_transport_hint()
            .unwrap_or(boot_protocol::BootVolumeTransport::Unknown);
        match transport_hint {
            boot_protocol::BootVolumeTransport::Nvme => {
                for device in crate::storage::nvme::probe_devices() {
                    registry::register_root_device(device);
                }
            }
            boot_protocol::BootVolumeTransport::Ahci => {
                for device in crate::storage::ahci::probe_devices() {
                    registry::register_root_device(device);
                }
            }
            _ => {
                for device in crate::storage::ahci::probe_devices() {
                    registry::register_root_device(device);
                }
                for device in crate::storage::nvme::probe_devices() {
                    registry::register_root_device(device);
                }
            }
        }

        let registered = {
            let devices = BLOCK_DEVICES.lock();
            registry::root_device_ids_locked(&devices).len()
        };
        crate::debug::println!("storage: registered {} root block device(s)", registered);
    }
}

pub fn descriptors() -> Vec<BlockDeviceDescriptor> {
    ensure_initialized();
    let devices = BLOCK_DEVICES.lock();
    devices
        .iter()
        .map(|device| BlockDeviceDescriptor {
            id: device.id,
            path: device.path.clone(),
            transport: device.transport,
            readonly: device.readonly,
            logical_block_size: device.logical_block_size,
            start_block: device.start_block,
            block_count: device.block_count,
        })
        .collect()
}

pub fn lookup(path: &str) -> Option<BlockDeviceHandle> {
    lookup_local(path)
}

fn lookup_local(path: &str) -> Option<BlockDeviceHandle> {
    ensure_initialized();
    let devices = BLOCK_DEVICES.lock();
    devices
        .iter()
        .find(|device| device.path == path)
        .map(|device| BlockDeviceHandle::new(device.id))
}

pub fn descriptor(handle: BlockDeviceHandle) -> Option<BlockDeviceDescriptor> {
    descriptor_local(handle)
}

fn descriptor_local(handle: BlockDeviceHandle) -> Option<BlockDeviceDescriptor> {
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
        logical_block_size: record.logical_block_size,
        start_block: record.start_block,
        block_count: record.block_count,
    })
}

fn log_boot_volume_handle_once(handle: BlockDeviceHandle) {
    if BOOT_VOLUME_HANDLE_LOGGED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Some(descriptor) = descriptor_without_init(handle) {
        crate::debug::info!(
            storage,
            format!(
                "boot volume handle selected id={} path={} transport={:?}",
                descriptor.id, descriptor.path, descriptor.transport
            )
            .as_str()
        );
        crate::debug::record_milestone(
            crate::debug::LogCategory::Storage,
            "boot-volume-handle",
            descriptor.id as u64,
            descriptor.start_block,
        );
    }
}

fn cached_boot_volume_handle() -> Option<BlockDeviceHandle> {
    match BOOT_VOLUME_HANDLE_ID.load(Ordering::Acquire) {
        u32::MAX => None,
        id => Some(BlockDeviceHandle::new(id)),
    }
}

fn cache_boot_volume_handle(handle: BlockDeviceHandle) -> BlockDeviceHandle {
    let _ = BOOT_VOLUME_HANDLE_ID.compare_exchange(
        u32::MAX,
        handle.id(),
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    let selected = cached_boot_volume_handle().unwrap_or(handle);
    log_boot_volume_handle_once(selected);
    selected
}

pub(crate) fn flush(handle: BlockDeviceHandle) -> IoResult<()> {
    ensure_initialized();
    io::flush_uncached_local(handle.id())
}

pub(crate) struct FatRegistryDevice {
    handle: BlockDeviceHandle,
    logical_block_size: usize,
    block_count: u64,
}

impl FatRegistryDevice {
    pub(crate) fn new(handle: BlockDeviceHandle) -> Self {
        let descriptor = descriptor(handle);
        Self {
            handle,
            logical_block_size: descriptor
                .as_ref()
                .map(|device| device.logical_block_size)
                .unwrap_or(0),
            block_count: descriptor.map(|device| device.block_count).unwrap_or(0),
        }
    }
}

impl SharedBlockDevice for FatRegistryDevice {
    fn logical_block_size(&self) -> usize {
        self.logical_block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        let block_size = self.logical_block_size();
        io::validate_block_io_exact(block_size, lba, self.block_count(), out.len())?;
        if out.len() > block_size {
            // Large FAT streaming reads are latency-sensitive during service and ELF loads.
            // The single-block cache is useful for metadata walks, but using it for every
            // sector of a multi-block read turns one sequential request into many registry
            // lookups, cache locks, and root-device lock acquisitions.
            return io::read_blocks_uncached(self.handle.id(), lba, out);
        }
        for (index, chunk) in out.chunks_exact_mut(block_size).enumerate() {
            io::read_cached_block(self.handle.id(), lba + index as u64, chunk)?;
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        let block_size = self.logical_block_size();
        io::validate_block_io_exact(block_size, lba, self.block_count(), input.len())?;
        if input.len() > block_size {
            let blocks = input.len() / block_size;
            io::write_blocks_uncached(self.handle.id(), lba, input)?;
            for (index, chunk) in input.chunks_exact(block_size).enumerate().take(blocks) {
                io::cache_store(self.handle.id(), lba + index as u64, chunk);
            }
            return Ok(());
        }
        for (index, chunk) in input.chunks_exact(block_size).enumerate() {
            io::write_cached_block(self.handle.id(), lba + index as u64, chunk)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> IoResult<()> {
        flush(self.handle)
    }
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn open_physical_boot_block_device(
    identity: BootVolumeIdentity,
) -> core::result::Result<Box<dyn SharedBlockDevice>, fatfs::Error<DiskIoError>> {
    let handle = boot::open_physical_boot_handle(identity).map_err(fatfs::Error::Io)?;
    Ok(Box::new(FatRegistryDevice::new(handle)) as Box<dyn SharedBlockDevice>)
}
// RING3-MIGRATION-REFERENCE END: storaged-owned block policy bootstrap exception.
