use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use super::{
    BLOCK_DEVICES, BlockDeviceKind, BlockDeviceOps, BlockDeviceRecord, DiskIoError, IoResult,
    MIN_LOGICAL_BLOCK_SIZE,
};

const BLOCK_CACHE_CAPACITY: usize = 256;

static BLOCK_CACHE: Mutex<Vec<BlockCacheEntry>> = Mutex::new(Vec::new());
static BLOCK_DEVICES_PUBLISHED: AtomicBool = AtomicBool::new(false);

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

fn emit_storage_read_trace(event_id: u16, object_id: u64, message: String) {
    crate::debug::emit_text(
        diag_abi::DiagProvider::Io,
        diag_abi::DiagLevel::Debug,
        event_id,
        0,
        object_id,
        message.as_str(),
    );
}

pub(super) fn read_blocks_uncached(device_id: u32, lba: u64, out: &mut [u8]) -> IoResult<()> {
    read_blocks_uncached_local(device_id, lba, out)
}

pub(super) fn read_blocks_uncached_local(device_id: u32, lba: u64, out: &mut [u8]) -> IoResult<()> {
    let resolved = resolve_root_device(device_id).ok_or(DiskIoError::NotPresent)?;
    validate_block_io_exact(
        resolved.logical_block_size,
        lba,
        resolved.block_count,
        out.len(),
    )?;
    let trace = crate::debug::should_emit(diag_abi::DiagProvider::Io, diag_abi::DiagLevel::Debug);
    if trace {
        emit_storage_read_trace(
            20,
            device_id as u64,
            format!(
                "storage uncached read: begin dev={} lba={} bytes={} start_block={} block_size={} blocks={}",
                device_id,
                lba,
                out.len(),
                resolved.start_block,
                resolved.logical_block_size,
                resolved.block_count
            ),
        );
    }
    let mut device = resolved.device.lock();
    if trace {
        let raw: *const dyn BlockDeviceOps = &**device;
        let (data_ptr, vtable_ptr): (usize, usize) = unsafe { mem::transmute(raw) };
        emit_storage_read_trace(
            21,
            device_id as u64,
            format!(
                "storage uncached read: dispatch dev={} data_ptr={:#x} vtable_ptr={:#x} abs_lba={}",
                device_id,
                data_ptr,
                vtable_ptr,
                resolved.start_block + lba
            ),
        );
    }
    let result = device.read_blocks(resolved.start_block + lba, out);
    if trace {
        emit_storage_read_trace(
            22,
            device_id as u64,
            format!(
                "storage uncached read: end dev={} ok={}",
                device_id,
                result.is_ok()
            ),
        );
    }
    result
}

pub(super) fn write_blocks_uncached(device_id: u32, lba: u64, input: &[u8]) -> IoResult<()> {
    write_blocks_uncached_local(device_id, lba, input)
}

pub(super) fn write_blocks_uncached_local(device_id: u32, lba: u64, input: &[u8]) -> IoResult<()> {
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

pub(super) fn flush_uncached(device_id: u32) -> IoResult<()> {
    flush_uncached_local(device_id)
}

pub(super) fn flush_uncached_local(device_id: u32) -> IoResult<()> {
    let resolved = resolve_root_device(device_id).ok_or(DiskIoError::NotPresent)?;
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

pub(super) fn validate_block_io_exact(
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

pub(super) fn read_cached_block(device_id: u32, lba: u64, out: &mut [u8]) -> IoResult<()> {
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

pub(super) fn write_cached_block(device_id: u32, lba: u64, input: &[u8]) -> IoResult<()> {
    write_blocks_uncached(device_id, lba, input)?;
    cache_store(device_id, lba, input);
    Ok(())
}

pub(super) fn cache_store(device_id: u32, lba: u64, data: &[u8]) {
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

pub(super) fn publish_block_devices_snapshot_once() {
    if BLOCK_DEVICES_PUBLISHED.load(Ordering::Acquire) {
        return;
    }
    BLOCK_DEVICES_PUBLISHED.store(true, Ordering::Release);
}

#[cfg(test)]
pub(super) fn cache_lookup(device_id: u32, lba: u64) -> Option<Vec<u8>> {
    BLOCK_CACHE
        .lock()
        .iter()
        .find(|entry| entry.device_id == device_id && entry.lba == lba)
        .map(|entry| entry.data.clone())
}

#[cfg(test)]
pub(super) fn clear_cache_for_tests() {
    BLOCK_CACHE.lock().clear();
}
