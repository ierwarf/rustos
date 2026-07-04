// RING3-MIGRATION-REFERENCE START: bootstrap exception: storaged/pagerd own
// post-bootstrap block cache policy. Ring0 keeps bounded physical block I/O
// substrate needed by boot-volume and gated block brokers.
use crate::sync::KernelSpinLock as Mutex;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::KernelWaitLock;

use super::{
    BLOCK_DEVICES, BlockDeviceKind, BlockDeviceOps, BlockDeviceRecord, DiskIoError, IoResult,
    MIN_LOGICAL_BLOCK_SIZE,
};

const BLOCK_CACHE_CAPACITY: usize = 256;
const UNCACHED_READ_LOCK_CHUNK_CAP: usize = 128 * 1024;

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
    device: Arc<KernelWaitLock<Box<dyn BlockDeviceOps>>>,
    readonly: bool,
    logical_block_size: usize,
    block_count: u64,
    start_block: u64,
}

fn emit_storage_read_trace(event_id: u16, object_id: u64, _message: String) {
    let _ = (event_id, object_id);
    crate::debug::debug!(storage, "{}", _message);
}

pub(super) fn read_blocks_uncached(device_id: u32, lba: u64, out: &mut [u8]) -> IoResult<()> {
    read_blocks_uncached_local(device_id, lba, out)
}

pub(super) fn read_blocks_uncached_local(device_id: u32, lba: u64, out: &mut [u8]) -> IoResult<()> {
    if nucleus_core::util::fault_injection::should_fail("block.read") {
        crate::debug::warn!(
            storage,
            "fault injection: block.read failed dev={} lba={} bytes={}",
            device_id,
            lba,
            out.len()
        );
        return Err(DiskIoError::DeviceFault);
    }
    let resolved = resolve_root_device(device_id).ok_or(DiskIoError::NotPresent)?;
    validate_block_io_exact(
        resolved.logical_block_size,
        lba,
        resolved.block_count,
        out.len(),
    )?;
    #[cfg(rustos_log_storage_debug)]
    let trace = crate::debug::enabled!(storage, debug);
    #[cfg(not(rustos_log_storage_debug))]
    let trace = false;
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
    let result = read_blocks_uncached_resolved(device_id, &resolved, lba, out, trace);
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

fn read_blocks_uncached_resolved(
    device_id: u32,
    resolved: &ResolvedRootDevice,
    lba: u64,
    out: &mut [u8],
    trace: bool,
) -> IoResult<()> {
    let block_size = resolved.logical_block_size;
    let max_blocks_per_lock = (UNCACHED_READ_LOCK_CHUNK_CAP / block_size).max(1);
    let mut done = 0usize;
    let mut chunk_lba = lba;
    while done < out.len() {
        let remaining_blocks = (out.len() - done) / block_size;
        let chunk_blocks = remaining_blocks.min(max_blocks_per_lock);
        let chunk_len = chunk_blocks * block_size;
        {
            let mut device = resolved.device.lock();
            if trace {
                let raw: *const dyn BlockDeviceOps = &**device;
                let (data_ptr, vtable_ptr): (usize, usize) = unsafe { mem::transmute(raw) };
                emit_storage_read_trace(
                    21,
                    device_id as u64,
                    format!(
                        "storage uncached read: dispatch dev={} data_ptr={:#x} vtable_ptr={:#x} abs_lba={} bytes={}",
                        device_id,
                        data_ptr,
                        vtable_ptr,
                        resolved.start_block + chunk_lba,
                        chunk_len
                    ),
                );
            }
            device.read_blocks(
                resolved.start_block + chunk_lba,
                &mut out[done..done + chunk_len],
            )?;
        }
        done += chunk_len;
        chunk_lba += chunk_blocks as u64;
        if done < out.len() {
            crate::multitask::cond_resched();
        }
    }
    Ok(())
}

pub(super) fn write_blocks_uncached(device_id: u32, lba: u64, input: &[u8]) -> IoResult<()> {
    write_blocks_uncached_local(device_id, lba, input)
}

pub(super) fn write_blocks_uncached_local(device_id: u32, lba: u64, input: &[u8]) -> IoResult<()> {
    if nucleus_core::util::fault_injection::should_fail("block.write") {
        crate::debug::warn!(
            storage,
            "fault injection: block.write failed dev={} lba={} bytes={}",
            device_id,
            lba,
            input.len()
        );
        return Err(DiskIoError::DeviceFault);
    }
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
        BlockDeviceKind::Root(device) => Some(ResolvedRootDevice {
            device: Arc::clone(device),
            readonly: record.readonly,
            logical_block_size: record.logical_block_size,
            block_count: record.block_count,
            start_block: record.start_block,
        }),
        BlockDeviceKind::Slice { parent_id, .. } => {
            let mut resolved = resolve_root_device_locked(devices, *parent_id)?;
            resolved.readonly |= record.readonly;
            resolved.start_block = record.start_block;
            resolved.block_count = record.block_count.min(resolved.block_count);
            resolved.logical_block_size = record.logical_block_size;
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
// RING3-MIGRATION-REFERENCE END: storaged/pagerd-owned block policy bootstrap exception.
