use std::mem::size_of;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rustos_user_abi::syscall::{
    DvmBlockInfoWire, DvmBlockTicketWire, RustosBlockBrokerArgs, BLOCK_BROKER_ABI_VERSION,
    BLOCK_BROKER_FLAG_FUA, BLOCK_BROKER_MAX_IO_BYTES, BLOCK_BROKER_OP_DVM_CANCEL,
    BLOCK_BROKER_OP_DVM_COLLECT, BLOCK_BROKER_OP_DVM_INFO, BLOCK_BROKER_OP_DVM_SUBMIT_FLUSH,
    BLOCK_BROKER_OP_DVM_SUBMIT_READ, BLOCK_BROKER_OP_DVM_SUBMIT_WRITE, BLOCK_BROKER_OP_DVM_WAIT,
    SYS_RUSTOS_BLOCK_BROKER,
};

const COMPLETION_TIMEOUT: Duration = Duration::from_secs(15);
const STARTUP_READY_TIMEOUT: Duration = Duration::from_secs(15);
const WAIT_SLICE_MILLIS: u64 = 1_000;
const READ_CACHE_WINDOW_LIMIT: usize = 8;
const READ_AHEAD_IN_FLIGHT_LIMIT: usize = READ_CACHE_WINDOW_LIMIT;
static READ_CACHE: Mutex<ReadCacheSet> = Mutex::new(ReadCacheSet::new());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BlockInfo {
    pub generation: u64,
    pub capacity_sectors: u64,
    pub logical_block_size: u32,
    pub physical_block_size: u32,
    pub features: u64,
    pub flags: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReadCache {
    generation: u64,
    lba: u64,
    block_count: u64,
    block_size: usize,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReadAheadWindow {
    lba: u64,
    block_count: u64,
    byte_len: usize,
}

impl ReadCache {
    fn slice(
        &self,
        generation: u64,
        lba: u64,
        block_count: u64,
        byte_len: usize,
    ) -> Result<Option<Vec<u8>>, i32> {
        if self.generation != generation || lba < self.lba {
            return Ok(None);
        }
        let offset_blocks = lba - self.lba;
        if offset_blocks
            .checked_add(block_count)
            .is_none_or(|end| end > self.block_count)
        {
            return Ok(None);
        }
        let expected_len = usize::try_from(self.block_count)
            .ok()
            .and_then(|blocks| blocks.checked_mul(self.block_size))
            .ok_or(libc::EIO)?;
        if self.block_size == 0 || self.bytes.len() != expected_len {
            return Err(libc::EIO);
        }
        let offset = usize::try_from(offset_blocks)
            .ok()
            .and_then(|blocks| blocks.checked_mul(self.block_size))
            .ok_or(libc::EIO)?;
        let end = offset.checked_add(byte_len).ok_or(libc::EIO)?;
        let bytes = self.bytes.get(offset..end).ok_or(libc::EIO)?;
        Ok(Some(bytes.to_vec()))
    }

    fn validate(&self) -> Result<(), i32> {
        let expected_len = usize::try_from(self.block_count)
            .ok()
            .and_then(|blocks| blocks.checked_mul(self.block_size))
            .ok_or(libc::EIO)?;
        if self.generation == 0
            || self.block_count == 0
            || self.block_size == 0
            || self.bytes.len() != expected_len
            || self.lba.checked_add(self.block_count).is_none()
            || self.bytes.len() > BLOCK_BROKER_MAX_IO_BYTES
        {
            return Err(libc::EIO);
        }
        Ok(())
    }

    fn overlaps(&self, other: &Self) -> bool {
        if self.generation != other.generation {
            return false;
        }
        let self_end = self.lba + self.block_count;
        let other_end = other.lba + other.block_count;
        self.lba < other_end && other.lba < self_end
    }
}

#[derive(Debug, Default)]
struct ReadCacheSet {
    entries: Vec<ReadCache>,
    next_read_ahead_lba: Option<(u64, u64)>,
}

impl ReadCacheSet {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_read_ahead_lba: None,
        }
    }

    fn slice(
        &mut self,
        generation: u64,
        lba: u64,
        block_count: u64,
        byte_len: usize,
    ) -> Result<Option<Vec<u8>>, i32> {
        let mut hit = None;
        for index in (0..self.entries.len()).rev() {
            match self.entries[index].slice(generation, lba, block_count, byte_len)? {
                Some(bytes) => {
                    hit = Some((index, bytes));
                    break;
                }
                None => {}
            }
        }
        let Some((index, bytes)) = hit else {
            return Ok(None);
        };
        let entry = self.entries.remove(index);
        self.entries.push(entry);
        Ok(Some(bytes))
    }

    fn insert(&mut self, cache: ReadCache) -> Result<(), i32> {
        cache.validate()?;
        if self
            .entries
            .iter()
            .any(|entry| entry.generation != cache.generation)
        {
            self.entries.clear();
        }
        self.entries.retain(|entry| !entry.overlaps(&cache));
        if self.entries.len() == READ_CACHE_WINDOW_LIMIT {
            self.entries.remove(0);
        }
        self.entries.push(cache);
        debug_assert!(self.entries.len() <= READ_CACHE_WINDOW_LIMIT);
        Ok(())
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.next_read_ahead_lba = None;
    }

    fn miss_continues_read_ahead(&self, generation: u64, lba: u64) -> bool {
        self.next_read_ahead_lba == Some((generation, lba))
    }

    fn record_read_ahead_end(&mut self, generation: u64, lba: u64) {
        self.next_read_ahead_lba = Some((generation, lba));
    }
}

/// Read the current transport state without turning an ordinary not-ready
/// observation into a service-wide startup wait. The readiness supervisor owns
/// the bounded wait; request turns return `EAGAIN` to their caller instead.
pub(super) fn info() -> Result<BlockInfo, i32> {
    info_once()
}

/// Wait for asynchronous DVM bringup with the kernel's atomic
/// check-arm-recheck waiter. Only the singleton readiness supervisor uses this
/// path, never a caller-owned storage RPC turn.
pub(super) fn wait_until_ready() -> Result<BlockInfo, i32> {
    let deadline = Instant::now() + STARTUP_READY_TIMEOUT;
    loop {
        match info_once() {
            Ok(info) => return Ok(info),
            Err(libc::EAGAIN) => wait_for_transport_event(deadline)?,
            Err(error) => return Err(error),
        }
    }
}

fn info_once() -> Result<BlockInfo, i32> {
    let mut wire = DvmBlockInfoWire::default();
    let args = RustosBlockBrokerArgs {
        abi_version: BLOCK_BROKER_ABI_VERSION,
        op: BLOCK_BROKER_OP_DVM_INFO,
        out_info_ptr: (&mut wire as *mut DvmBlockInfoWire) as u64,
        ..RustosBlockBrokerArgs::default()
    };
    syscall_block(&args)?;
    if wire.generation == 0
        || wire.capacity_sectors == 0
        || wire.logical_block_size < 512
        || !wire.logical_block_size.is_power_of_two()
        || !wire.logical_block_size.is_multiple_of(512)
        || wire.physical_block_size < wire.logical_block_size
        || !wire
            .physical_block_size
            .is_multiple_of(wire.logical_block_size)
        || wire.reserved0 != 0
    {
        return Err(libc::EIO);
    }
    Ok(BlockInfo {
        generation: wire.generation,
        capacity_sectors: wire.capacity_sectors,
        logical_block_size: wire.logical_block_size,
        physical_block_size: wire.physical_block_size,
        features: wire.features,
        flags: wire.flags,
    })
}

/// Sleep through the asynchronous DVM startup window using the kernel's
/// atomic check-arm-recheck waiter.  A timeout is terminal for this request;
/// callers may retry the service operation, but no host/bootstrap storage
/// fallback is selected.
pub(super) fn wait_for_transport_event(deadline: Instant) -> Result<(), i32> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let timeout_ms = bounded_wait_timeout_ms(remaining).ok_or(libc::ETIMEDOUT)?;
    let wait = RustosBlockBrokerArgs {
        abi_version: BLOCK_BROKER_ABI_VERSION,
        op: BLOCK_BROKER_OP_DVM_WAIT,
        timeout_ms,
        ..RustosBlockBrokerArgs::default()
    };
    match syscall_block(&wait) {
        Ok(_) | Err(libc::EINTR) => Ok(()),
        Err(libc::ETIMEDOUT) if Instant::now() < deadline => Ok(()),
        Err(error) => Err(error),
    }
}

fn bounded_wait_timeout_ms(remaining: Duration) -> Option<u64> {
    if remaining.is_zero() {
        return None;
    }
    Some(
        u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .clamp(1, WAIT_SLICE_MILLIS),
    )
}

pub(super) fn read(expected_generation: u64, lba: u64, block_count: u64) -> Result<Vec<u8>, i32> {
    let info = info().inspect_err(|errno| {
        clear_read_cache();
        eprintln!("storaged: dvm read rejected stage=info errno={errno}");
    })?;
    require_generation(info, expected_generation).inspect_err(|errno| {
        clear_read_cache();
        eprintln!(
            "storaged: dvm read rejected stage=generation expected={} actual={} errno={errno}",
            expected_generation, info.generation
        );
    })?;
    let byte_len = checked_byte_len(info, lba, block_count).inspect_err(|errno| {
        eprintln!(
            "storaged: dvm read rejected stage=range lba={lba} blocks={block_count} logical={} capacity_sectors={} errno={errno}",
            info.logical_block_size, info.capacity_sectors
        );
    })?;
    let (cached, continues_read_ahead) =
        read_cache_lookup(info.generation, lba, block_count, byte_len)?;
    if let Some(bytes) = cached {
        return Ok(bytes);
    }
    let window_limit = if continues_read_ahead {
        READ_AHEAD_IN_FLIGHT_LIMIT
    } else {
        1
    };
    let windows = read_ahead_plan(info, lba, block_count, window_limit)?;
    let mut in_flight = Vec::with_capacity(windows.len());
    for window in windows {
        match submit(
            BLOCK_BROKER_OP_DVM_SUBMIT_READ,
            window.lba,
            window.block_count,
            0,
            std::ptr::null(),
            window.byte_len,
        ) {
            Ok(ticket) => in_flight.push((window, ticket, vec![0_u8; window.byte_len])),
            Err(errno) => {
                for (_, ticket, _) in in_flight {
                    cancel_best_effort(ticket);
                }
                eprintln!(
                    "storaged: dvm read rejected stage=submit lba={} blocks={} bytes={} errno={errno}",
                    window.lba, window.block_count, window.byte_len
                );
                return Err(errno);
            }
        }
    }
    for index in 0..in_flight.len() {
        let completion = {
            let (_, ticket, prefetched) = &mut in_flight[index];
            wait_and_collect(*ticket, Some(prefetched.as_mut_slice()))
        };
        if let Err(errno) = completion {
            let (window, ticket, _) = &in_flight[index];
            for (_, pending, _) in &in_flight[index + 1..] {
                cancel_best_effort(*pending);
            }
            clear_read_cache();
            eprintln!(
                "storaged: dvm read rejected stage=completion request_id={} slot={} bytes={} errno={errno}",
                ticket.request_id, ticket.data_slot, window.byte_len
            );
            return Err(errno);
        }
    }
    let requested = in_flight
        .first()
        .and_then(|(_, _, bytes)| bytes.get(..byte_len))
        .ok_or(libc::EIO)?
        .to_vec();
    let read_ahead_end = in_flight
        .last()
        .and_then(|(window, _, _)| window.lba.checked_add(window.block_count))
        .ok_or(libc::EIO)?;
    for (window, _, prefetched) in in_flight {
        replace_read_cache(ReadCache {
            generation: info.generation,
            lba: window.lba,
            block_count: window.block_count,
            block_size: info.logical_block_size as usize,
            bytes: prefetched,
        })?;
    }
    record_read_ahead_end(info.generation, read_ahead_end)?;
    Ok(requested)
}

fn read_ahead_plan(
    info: BlockInfo,
    lba: u64,
    requested_blocks: u64,
    window_limit: usize,
) -> Result<Vec<ReadAheadWindow>, i32> {
    let max_blocks = (BLOCK_BROKER_MAX_IO_BYTES as u64) / u64::from(info.logical_block_size);
    if max_blocks == 0
        || requested_blocks == 0
        || requested_blocks > max_blocks
        || window_limit == 0
        || window_limit > READ_AHEAD_IN_FLIGHT_LIMIT
    {
        return Err(libc::EINVAL);
    }
    let capacity_blocks = info.capacity_sectors / u64::from(info.logical_block_size / 512);
    let mut cursor = lba;
    let mut remaining = capacity_blocks.checked_sub(lba).ok_or(libc::EINVAL)?;
    if remaining < requested_blocks {
        return Err(libc::EINVAL);
    }
    let mut windows = Vec::with_capacity(window_limit);
    while remaining != 0 && windows.len() < window_limit {
        let block_count = remaining.min(max_blocks);
        windows.push(ReadAheadWindow {
            lba: cursor,
            block_count,
            byte_len: checked_byte_len(info, cursor, block_count)?,
        });
        cursor = cursor.checked_add(block_count).ok_or(libc::EINVAL)?;
        remaining -= block_count;
    }
    Ok(windows)
}

pub(super) fn write(
    expected_generation: u64,
    lba: u64,
    bytes: &[u8],
    fua: bool,
) -> Result<(), i32> {
    clear_read_cache();
    let info = info()?;
    require_generation(info, expected_generation)?;
    let block_size = info.logical_block_size as usize;
    if bytes.is_empty() || !bytes.len().is_multiple_of(block_size) {
        return Err(libc::EINVAL);
    }
    let block_count = (bytes.len() / block_size) as u64;
    checked_byte_len(info, lba, block_count)?;
    let ticket = submit(
        BLOCK_BROKER_OP_DVM_SUBMIT_WRITE,
        lba,
        block_count,
        if fua { BLOCK_BROKER_FLAG_FUA } else { 0 },
        bytes.as_ptr(),
        bytes.len(),
    )?;
    wait_and_collect(ticket, None)
}

fn read_cache_lookup(
    generation: u64,
    lba: u64,
    block_count: u64,
    byte_len: usize,
) -> Result<(Option<Vec<u8>>, bool), i32> {
    let mut cache = READ_CACHE.lock().map_err(|_| libc::EIO)?;
    let bytes = cache.slice(generation, lba, block_count, byte_len)?;
    let continues = bytes.is_none() && cache.miss_continues_read_ahead(generation, lba);
    Ok((bytes, continues))
}

fn replace_read_cache(cache: ReadCache) -> Result<(), i32> {
    READ_CACHE.lock().map_err(|_| libc::EIO)?.insert(cache)
}

fn record_read_ahead_end(generation: u64, lba: u64) -> Result<(), i32> {
    READ_CACHE
        .lock()
        .map_err(|_| libc::EIO)?
        .record_read_ahead_end(generation, lba);
    Ok(())
}

fn clear_read_cache() {
    if let Ok(mut cache) = READ_CACHE.lock() {
        cache.clear();
    }
}

pub(super) fn flush(expected_generation: u64) -> Result<(), i32> {
    require_generation(info()?, expected_generation)?;
    let mut ticket = DvmBlockTicketWire::default();
    let args = RustosBlockBrokerArgs {
        abi_version: BLOCK_BROKER_ABI_VERSION,
        op: BLOCK_BROKER_OP_DVM_SUBMIT_FLUSH,
        out_ticket_ptr: (&mut ticket as *mut DvmBlockTicketWire) as u64,
        ..RustosBlockBrokerArgs::default()
    };
    syscall_block(&args)?;
    validate_ticket(ticket)?;
    wait_and_collect(ticket, None)
}

fn submit(
    operation: u16,
    lba: u64,
    block_count: u64,
    flags: u32,
    buffer: *const u8,
    buffer_len: usize,
) -> Result<DvmBlockTicketWire, i32> {
    let mut ticket = DvmBlockTicketWire::default();
    let args = RustosBlockBrokerArgs {
        abi_version: BLOCK_BROKER_ABI_VERSION,
        op: operation,
        lba,
        block_count,
        buffer_ptr: buffer as u64,
        buffer_len: buffer_len as u64,
        flags,
        out_ticket_ptr: (&mut ticket as *mut DvmBlockTicketWire) as u64,
        ..RustosBlockBrokerArgs::default()
    };
    syscall_block(&args)?;
    validate_ticket(ticket)?;
    Ok(ticket)
}

fn wait_and_collect(
    ticket: DvmBlockTicketWire,
    mut read_buffer: Option<&mut [u8]>,
) -> Result<(), i32> {
    validate_ticket(ticket)?;
    let deadline = Instant::now() + COMPLETION_TIMEOUT;
    loop {
        let (buffer_ptr, buffer_len) = read_buffer.as_deref_mut().map_or((0, 0), |buffer| {
            (buffer.as_mut_ptr() as u64, buffer.len() as u64)
        });
        let collect = RustosBlockBrokerArgs {
            abi_version: BLOCK_BROKER_ABI_VERSION,
            op: BLOCK_BROKER_OP_DVM_COLLECT,
            buffer_ptr,
            buffer_len,
            ticket,
            ..RustosBlockBrokerArgs::default()
        };
        match syscall_block(&collect) {
            Ok(completed) => {
                if completed != buffer_len {
                    return Err(libc::EIO);
                }
                return Ok(());
            }
            Err(libc::EAGAIN) => {}
            Err(error) => {
                eprintln!(
                    "storaged: dvm completion rejected stage=collect request_id={} slot={} errno={error}",
                    ticket.request_id, ticket.data_slot
                );
                return Err(error);
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            cancel_best_effort(ticket);
            return Err(libc::ETIMEDOUT);
        }
        let Some(timeout_ms) = bounded_wait_timeout_ms(remaining) else {
            cancel_best_effort(ticket);
            return Err(libc::ETIMEDOUT);
        };
        let wait = RustosBlockBrokerArgs {
            abi_version: BLOCK_BROKER_ABI_VERSION,
            op: BLOCK_BROKER_OP_DVM_WAIT,
            timeout_ms,
            ..RustosBlockBrokerArgs::default()
        };
        match syscall_block(&wait) {
            Ok(_) | Err(libc::EINTR) => {}
            Err(libc::ETIMEDOUT) if Instant::now() < deadline => {}
            Err(error) => {
                eprintln!(
                    "storaged: dvm completion rejected stage=wait request_id={} slot={} timeout_ms={timeout_ms} errno={error}",
                    ticket.request_id, ticket.data_slot
                );
                cancel_best_effort(ticket);
                return Err(error);
            }
        }
    }
}

fn cancel_best_effort(ticket: DvmBlockTicketWire) {
    let args = RustosBlockBrokerArgs {
        abi_version: BLOCK_BROKER_ABI_VERSION,
        op: BLOCK_BROKER_OP_DVM_CANCEL,
        ticket,
        ..RustosBlockBrokerArgs::default()
    };
    let _ = syscall_block(&args);
}

fn require_generation(info: BlockInfo, expected: u64) -> Result<(), i32> {
    if expected == 0 || info.generation != expected {
        Err(libc::ESTALE)
    } else {
        Ok(())
    }
}

fn checked_byte_len(info: BlockInfo, lba: u64, block_count: u64) -> Result<usize, i32> {
    if block_count == 0 {
        return Err(libc::EINVAL);
    }
    let byte_len = block_count
        .checked_mul(u64::from(info.logical_block_size))
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or(libc::EINVAL)?;
    let sectors_per_block = u64::from(info.logical_block_size / 512);
    let end_sector = lba
        .checked_add(block_count)
        .and_then(|end| end.checked_mul(sectors_per_block))
        .ok_or(libc::EINVAL)?;
    if end_sector > info.capacity_sectors {
        return Err(libc::EINVAL);
    }
    Ok(byte_len)
}

fn validate_ticket(ticket: DvmBlockTicketWire) -> Result<(), i32> {
    if ticket.generation == 0
        || ticket.request_id == 0
        || ticket.data_slot >= 64
        || ticket.reserved0 != 0
    {
        Err(libc::EIO)
    } else {
        Ok(())
    }
}

fn syscall_block(args: &RustosBlockBrokerArgs) -> Result<u64, i32> {
    debug_assert_eq!(
        size_of::<RustosBlockBrokerArgs>(),
        std::mem::size_of_val(args)
    );
    let status = unsafe {
        rustos_svc_runtime::syscall::syscall1(
            SYS_RUSTOS_BLOCK_BROKER,
            (args as *const RustosBlockBrokerArgs) as u64,
        )
    };
    if status < 0 {
        Err((-status) as i32)
    } else {
        Ok(status as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> BlockInfo {
        BlockInfo {
            generation: 7,
            capacity_sectors: 8192,
            logical_block_size: 4096,
            physical_block_size: 4096,
            features: 1,
            flags: 0,
        }
    }

    #[test]
    fn range_checks_use_logical_blocks_but_bind_sector_capacity() {
        assert_eq!(checked_byte_len(info(), 4, 2), Ok(8192));
        assert_eq!(checked_byte_len(info(), 1023, 2), Err(libc::EINVAL));
        assert_eq!(checked_byte_len(info(), u64::MAX, 1), Err(libc::EINVAL));
    }

    #[test]
    fn generation_mismatch_is_stale_not_a_fallback() {
        assert_eq!(require_generation(info(), 6), Err(libc::ESTALE));
        assert_eq!(require_generation(info(), 7), Ok(()));
    }

    #[test]
    fn read_ahead_cache_is_generation_and_range_bound() {
        let cache = ReadCache {
            generation: 7,
            lba: 100,
            block_count: 4,
            block_size: 4,
            bytes: (0_u8..16).collect(),
        };
        assert_eq!(cache.slice(7, 101, 2, 8), Ok(Some((4_u8..12).collect())));
        assert_eq!(cache.slice(8, 101, 2, 8), Ok(None));
        assert_eq!(cache.slice(7, 99, 1, 4), Ok(None));
        assert_eq!(cache.slice(7, 103, 2, 8), Ok(None));

        let malformed = ReadCache {
            bytes: vec![0; 15],
            ..cache
        };
        assert_eq!(malformed.slice(7, 100, 1, 4), Err(libc::EIO));
    }

    #[test]
    fn read_ahead_cache_set_is_bounded_lru_and_generation_atomic() {
        let window = |generation, lba, byte| ReadCache {
            generation,
            lba,
            block_count: 1,
            block_size: 4,
            bytes: vec![byte; 4],
        };
        let mut caches = ReadCacheSet::new();
        for index in 0..READ_CACHE_WINDOW_LIMIT {
            caches
                .insert(window(7, index as u64, index as u8))
                .expect("insert cache window");
        }
        assert_eq!(caches.entries.len(), READ_CACHE_WINDOW_LIMIT);
        assert_eq!(caches.slice(7, 0, 1, 4), Ok(Some(vec![0; 4])));

        caches
            .insert(window(7, READ_CACHE_WINDOW_LIMIT as u64, 8))
            .expect("evict least recently used window");
        assert_eq!(caches.entries.len(), READ_CACHE_WINDOW_LIMIT);
        assert_eq!(caches.slice(7, 1, 1, 4), Ok(None));
        assert_eq!(caches.slice(7, 0, 1, 4), Ok(Some(vec![0; 4])));

        caches
            .insert(window(8, 100, 9))
            .expect("new generation replaces cache epoch");
        assert_eq!(caches.entries.len(), 1);
        assert_eq!(caches.slice(7, 0, 1, 4), Ok(None));
        assert_eq!(caches.slice(8, 100, 1, 4), Ok(Some(vec![9; 4])));

        caches.clear();
        assert!(caches.entries.is_empty());
    }

    #[test]
    fn overlapping_read_ahead_windows_replace_instead_of_aliasing() {
        let mut caches = ReadCacheSet::new();
        caches
            .insert(ReadCache {
                generation: 7,
                lba: 100,
                block_count: 4,
                block_size: 4,
                bytes: vec![1; 16],
            })
            .expect("insert first window");
        caches
            .insert(ReadCache {
                generation: 7,
                lba: 102,
                block_count: 4,
                block_size: 4,
                bytes: vec![2; 16],
            })
            .expect("replace overlapping window");
        assert_eq!(caches.entries.len(), 1);
        assert_eq!(caches.slice(7, 100, 1, 4), Ok(None));
        assert_eq!(caches.slice(7, 102, 1, 4), Ok(Some(vec![2; 4])));
    }

    #[test]
    fn startup_wait_slice_is_bounded_and_nonzero() {
        assert_eq!(bounded_wait_timeout_ms(Duration::ZERO), None);
        assert_eq!(bounded_wait_timeout_ms(Duration::from_nanos(1)), Some(1));
        assert_eq!(
            bounded_wait_timeout_ms(Duration::from_millis(WAIT_SLICE_MILLIS + 1)),
            Some(WAIT_SLICE_MILLIS)
        );
    }

    #[test]
    fn read_ahead_plan_pipelines_bounded_transport_windows() {
        let windows = read_ahead_plan(info(), 100, 2, READ_AHEAD_IN_FLIGHT_LIMIT)
            .expect("valid read-ahead plan");
        assert_eq!(windows.len(), READ_AHEAD_IN_FLIGHT_LIMIT);
        assert_eq!(
            windows[0],
            ReadAheadWindow {
                lba: 100,
                block_count: 16,
                byte_len: BLOCK_BROKER_MAX_IO_BYTES,
            }
        );
        assert_eq!(windows[1].lba, 116);
        assert!(windows
            .windows(2)
            .all(|pair| pair[0].lba + pair[0].block_count == pair[1].lba));
    }

    #[test]
    fn read_ahead_plan_stops_at_device_end_and_rejects_oversize_requests() {
        let windows = read_ahead_plan(info(), 1_020, 1, READ_AHEAD_IN_FLIGHT_LIMIT)
            .expect("tail read-ahead plan");
        assert_eq!(
            windows,
            vec![ReadAheadWindow {
                lba: 1_020,
                block_count: 4,
                byte_len: 16 * 1_024,
            }]
        );
        assert_eq!(
            read_ahead_plan(info(), 100, 17, READ_AHEAD_IN_FLIGHT_LIMIT),
            Err(libc::EINVAL)
        );
        assert_eq!(
            read_ahead_plan(info(), 1_024, 1, READ_AHEAD_IN_FLIGHT_LIMIT),
            Err(libc::EINVAL)
        );
        assert_eq!(read_ahead_plan(info(), 100, 1, 0), Err(libc::EINVAL));
    }

    #[test]
    fn random_miss_stays_one_window_until_a_contiguous_boundary_miss() {
        let mut caches = ReadCacheSet::new();
        assert!(!caches.miss_continues_read_ahead(7, 100));
        caches.record_read_ahead_end(7, 116);
        assert!(caches.miss_continues_read_ahead(7, 116));
        assert!(!caches.miss_continues_read_ahead(7, 117));
        assert!(!caches.miss_continues_read_ahead(8, 116));
        caches.clear();
        assert!(!caches.miss_continues_read_ahead(7, 116));
    }
}
