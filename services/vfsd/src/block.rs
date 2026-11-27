use alloc::format;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, DvmBlockInfoWire,
    StoragedBulkReadResponse, StoragedBulkWriteRequest, BLOCK_BROKER_INFO_FLAG_READ_ONLY,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_STORAGED,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE_BULK, IPC_SERVICE_STORAGED,
    STORAGED_BULK_READ_PAYLOAD_CAPACITY, STORAGED_BULK_READ_RESPONSE_HEADER_BYTES,
    STORAGED_BULK_WRITE_PAYLOAD_CAPACITY, SYS_RUSTOS_IPC_CALL,
    SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
};
use storage_core::{BlockDevice, IoResult, StorageError};

use super::{EAGAIN, EINVAL, EIO, ENODEV, ENOMEM, ENOSYS};
use vfsd::{
    admit_dvm_block_geometry_with_allowed_flags, cooperative_bulk_yield_state,
    storage_error_from_linux_status, storage_service_lookup_errno, validate_dvm_block_range,
};

const IPC_BLOCK_PAYLOAD_BYTES: usize =
    rustos_user_abi::syscall::COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY;
// FAT may satisfy a large executable snapshot as hundreds of cache-hot block
// requests after one DVM read-ahead completion. Bound the uninterrupted
// System-class service burst independently of individual IPC chunk size. One
// inline storaged reply carries just under 64 KiB; yielding after every reply
// turned a 900 KiB executable into fifteen scheduler handoffs and routinely
// exhausted the public two-second snapshot deadline on SMP. Four replies keep
// the burst bounded while matching the snapshot writer's 256 KiB progress
// quantum.
const BULK_COOPERATIVE_YIELD_BYTES: usize = 256 * 1024;
// A detached old mount and a newly admitted mount can overlap. Response
// storage belongs to each device lease, not one unsynchronized global slot.
const _: () =
    assert!(core::mem::align_of::<StoragedBulkReadResponse>() <= core::mem::align_of::<u64>());
// Not-ready is an expected, bounded state while the DVM proves its initial
// generation. Keep one diagnostic transition instead of turning every glibc
// negative probe into debugcon I/O and scheduler pressure.
static STORAGE_NOT_READY_DIAGNOSTIC_EMITTED: AtomicBool = AtomicBool::new(false);

// Aggregate cost of the vfsd -> storaged bulk-read round trips. A large
// executable snapshot is thousands of these calls; the per-snapshot report
// uses the totals to separate "storaged's device read is slow" from "vfsd is
// slow between calls" without a per-call debug line. Calls are counted exactly;
// time is sampled (one call in `TIMING_SAMPLE_PERIOD` pays two clock syscalls)
// and scaled by the period, so the sum is an estimate and the maximum is the
// longest sampled call.
const TIMING_SAMPLE_PERIOD: u64 = 32;
static BULK_READ_CALLS: AtomicU64 = AtomicU64::new(0);
static BULK_READ_NS: AtomicU64 = AtomicU64::new(0);
static BULK_READ_MAX_NS: AtomicU64 = AtomicU64::new(0);

// The process id never changes, so one `getpid` serves every request. The tid
// is per thread and this `no_std` service has no thread-local storage, so each
// request still asks the kernel for it.
static CACHED_PID: AtomicU64 = AtomicU64::new(0);
// Endpoint of the live storaged, cached across calls. Zero means "look it up".
// It is dropped on any failed call and replaced by every control-path call, so
// a restarted storaged is found again (see `storaged_endpoint`).
static STORAGED_ENDPOINT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Default)]
pub(super) struct BulkReadCounters {
    pub(super) calls: u64,
    pub(super) nanos: u64,
}

impl BulkReadCounters {
    pub(super) fn since(self, earlier: Self) -> Self {
        Self {
            calls: self.calls.saturating_sub(earlier.calls),
            nanos: self.nanos.saturating_sub(earlier.nanos),
        }
    }
}

pub(super) fn bulk_read_counters() -> BulkReadCounters {
    BulkReadCounters {
        calls: BULK_READ_CALLS.load(Ordering::Relaxed),
        nanos: BULK_READ_NS.load(Ordering::Relaxed),
    }
}

/// Longest single bulk call since the previous query.
pub(super) fn take_bulk_read_max_nanos() -> u64 {
    BULK_READ_MAX_NS.swap(0, Ordering::Relaxed)
}

pub(super) struct BootBlockDevice {
    pub(super) generation: u64,
    pub(super) block_size: usize,
    pub(super) block_count: u64,
    pub(super) read_only: bool,
    bulk_bytes_since_yield: usize,
    bulk_response: alloc::vec::Vec<u64>,
    bulk_write_request: alloc::vec::Vec<u64>,
}

impl BootBlockDevice {
    pub(super) fn open() -> Result<Self, i32> {
        let request = storage_request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO);
        let response = call_storaged(&request)?;
        if response.status != 0
            || response.value0 == 0
            || response.payload_len as usize != size_of::<DvmBlockInfoWire>()
        {
            return Err(if response.status != 0 {
                response.status
            } else {
                EIO
            });
        }
        let info = read_block_info(&response.payload).ok_or(EIO)?;
        let (block_size, block_count) = admit_dvm_block_geometry_with_allowed_flags(
            info,
            response.value0,
            response.value1,
            IPC_BLOCK_PAYLOAD_BYTES,
            BLOCK_BROKER_INFO_FLAG_READ_ONLY,
        )?;
        let read_only = info.flags & BLOCK_BROKER_INFO_FLAG_READ_ONLY != 0;
        let words = size_of::<StoragedBulkReadResponse>().div_ceil(size_of::<u64>());
        let mut bulk_response = alloc::vec::Vec::new();
        bulk_response.try_reserve_exact(words).map_err(|_| ENOMEM)?;
        bulk_response.resize(words, 0);
        let write_words = size_of::<StoragedBulkWriteRequest>().div_ceil(size_of::<u64>());
        let mut bulk_write_request = alloc::vec::Vec::new();
        bulk_write_request
            .try_reserve_exact(write_words)
            .map_err(|_| ENOMEM)?;
        bulk_write_request.resize(write_words, 0);
        Ok(Self {
            generation: info.generation,
            block_size,
            block_count,
            read_only,
            bulk_bytes_since_yield: 0,
            bulk_response,
            bulk_write_request,
        })
    }

    fn read_transfer(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        validate_dvm_block_range(self.block_size, self.block_count, lba, out.len())?;
        let max_blocks = (STORAGED_BULK_READ_PAYLOAD_CAPACITY / self.block_size) as u64;
        if max_blocks == 0 {
            return Err(StorageError::Unsupported);
        }
        // A large range keeps several storaged calls in flight; a small one, or
        // a pool that is unavailable or busy, reads on this thread.
        if let Some(result) = super::bulk_lanes::read_parallel(
            self.generation,
            self.block_size,
            max_blocks,
            lba,
            out,
            &mut self.bulk_response,
        ) {
            result.map_err(|errno| storage_error_from_linux_status(-(errno as i64)))?;
            // Parallel lanes already block across storaged IPC round trips and
            // run demoted to user scheduling class; avoid forcing an extra
            // `sched_yield` barrier between every 4 MiB snapshot window once
            // demoted.
            if !super::SNAPSHOT_WORKER_DEMOTED.load(Ordering::Relaxed) {
                self.account_bulk_progress(out.len());
            }
            return Ok(());
        }
        let mut request = storage_request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK);
        request.arg0 = self.generation;
        let mut done = 0usize;
        while done < out.len() {
            let remaining_blocks = ((out.len() - done) / self.block_size) as u64;
            let block_count = remaining_blocks.min(max_blocks);
            let byte_len = block_count as usize * self.block_size;
            let chunk_lba = lba
                .checked_add((done / self.block_size) as u64)
                .ok_or(StorageError::InvalidInput)?;
            request.arg1 = chunk_lba;
            request.arg2 = block_count;
            call_storaged_bulk(
                &request,
                &mut out[done..done + byte_len],
                &mut self.bulk_response,
            )
            .map_err(|errno| storage_error_from_linux_status(-(errno as i64)))?;
            self.account_bulk_progress(byte_len);
            done += byte_len;
        }
        Ok(())
    }

    fn write_transfer(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        self.write_transfer_blocks(lba, input)
    }

    fn write_transfer_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        validate_dvm_block_range(self.block_size, self.block_count, lba, input.len())?;
        let small_max_blocks = (IPC_BLOCK_PAYLOAD_BYTES / self.block_size) as u64;
        let bulk_max_blocks = (STORAGED_BULK_WRITE_PAYLOAD_CAPACITY / self.block_size) as u64;
        if small_max_blocks == 0 || bulk_max_blocks == 0 {
            return Err(StorageError::Unsupported);
        }
        let mut done = 0usize;
        while done < input.len() {
            let remaining = input.len() - done;
            let remaining_blocks = (remaining / self.block_size) as u64;
            let chunk_lba = lba
                .checked_add((done / self.block_size) as u64)
                .ok_or(StorageError::InvalidInput)?;
            let (block_count, byte_len, response) = if remaining > IPC_BLOCK_PAYLOAD_BYTES {
                let block_count = remaining_blocks.min(bulk_max_blocks);
                let byte_len = block_count as usize * self.block_size;
                let response = call_storaged_bulk_write(
                    self.generation,
                    chunk_lba,
                    block_count,
                    &input[done..done + byte_len],
                    &mut self.bulk_write_request,
                )
                .map_err(|errno| storage_error_from_linux_status(-(errno as i64)))?;
                (block_count, byte_len, response)
            } else {
                let block_count = remaining_blocks.min(small_max_blocks);
                let byte_len = block_count as usize * self.block_size;
                let mut request = storage_request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE);
                request.arg0 = self.generation;
                request.arg1 = chunk_lba;
                request.arg2 = block_count;
                request.payload_len = byte_len as u32;
                request.payload[..byte_len].copy_from_slice(&input[done..done + byte_len]);
                let response = call_storaged(&request)
                    .map_err(|errno| storage_error_from_linux_status(-(errno as i64)))?;
                (block_count, byte_len, response)
            };
            let _ = block_count;
            if response.status != 0
                || response.value0 != self.generation
                || response.value1 != byte_len as u64
                || response.payload_len != 0
            {
                return Err(storage_error_from_linux_status(
                    -(if response.status != 0 {
                        response.status
                    } else {
                        EIO
                    } as i64),
                ));
            }
            self.account_bulk_progress(byte_len);
            done += byte_len;
        }
        Ok(())
    }

    fn account_bulk_progress(&mut self, completed_bytes: usize) {
        let total = self.bulk_bytes_since_yield.saturating_add(completed_bytes);
        let (remainder, should_yield) =
            cooperative_bulk_yield_state(total, BULK_COOPERATIVE_YIELD_BYTES);
        self.bulk_bytes_since_yield = remainder;
        if should_yield {
            unsafe {
                rustos_svc_runtime::syscall::syscall0(rustos_user_abi::linux::SYS_SCHED_YIELD);
            }
        }
    }
}

impl BlockDevice for BootBlockDevice {
    fn logical_block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        self.read_transfer(lba, out)
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        if self.read_only {
            return Err(StorageError::Unsupported);
        }
        self.write_transfer(lba, input)
    }

    fn flush(&mut self) -> IoResult<()> {
        if self.read_only {
            return Ok(());
        }
        let mut request = storage_request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH);
        request.arg0 = self.generation;
        let response = call_storaged(&request)
            .map_err(|errno| storage_error_from_linux_status(-(errno as i64)))?;
        if response.status != 0
            || response.value0 != self.generation
            || response.value1 != 0
            || response.payload_len != 0
        {
            return Err(storage_error_from_linux_status(
                -(if response.status != 0 {
                    response.status
                } else {
                    EIO
                } as i64),
            ));
        }
        Ok(())
    }
}

pub(super) fn storage_request(operation: u16) -> CommercialMaxProtocolRequest {
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_STORAGED;
    request.header.op = operation;
    let mut pid = CACHED_PID.load(Ordering::Relaxed);
    if pid == 0 {
        let raw =
            unsafe { rustos_svc_runtime::syscall::syscall0(rustos_user_abi::linux::SYS_GETPID) };
        if raw > 0 {
            pid = raw as u64;
            CACHED_PID.store(pid, Ordering::Relaxed);
        }
    }
    let tid = unsafe { rustos_svc_runtime::syscall::syscall0(rustos_user_abi::linux::SYS_GETTID) };
    if pid > 0 && tid > 0 {
        request.header.subject_pid = pid;
        request.header.subject_tid = tid as u64;
    }
    request
}

/// Looks the storaged endpoint up in the kernel registry and caches it.
fn lookup_storaged_endpoint() -> Result<u64, i32> {
    let endpoint = unsafe {
        rustos_svc_runtime::syscall::syscall1(
            SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
            IPC_SERVICE_STORAGED,
        )
    };
    if endpoint <= 0 {
        STORAGED_ENDPOINT.store(0, Ordering::Relaxed);
        return Err(storage_service_lookup_errno(endpoint));
    }
    STORAGED_ENDPOINT.store(endpoint as u64, Ordering::Relaxed);
    Ok(endpoint as u64)
}

/// The storaged endpoint for a bulk call: the cached one, or a fresh lookup.
/// The bool is whether the endpoint came from the cache.
///
/// A cached endpoint can only go stale when storaged is replaced. Its calls
/// then fail, and `retry_with_fresh_endpoint` drops the cache and repeats the
/// call once against the registry's current endpoint. The control path
/// (`call_storaged`, run at every mount open) always re-reads the registry.
fn storaged_endpoint() -> Result<(u64, bool), i32> {
    match STORAGED_ENDPOINT.load(Ordering::Relaxed) {
        0 => lookup_storaged_endpoint().map(|endpoint| (endpoint, false)),
        endpoint => Ok((endpoint, true)),
    }
}

/// After a failed call on a cached endpoint: drops the cache and returns the
/// registry's endpoint if it differs from the one that failed (storaged was
/// replaced), so the caller retries exactly once. `None` means the same
/// endpoint is still current and the failure is genuine.
fn retry_with_fresh_endpoint(failed: u64) -> Option<u64> {
    STORAGED_ENDPOINT.store(0, Ordering::Relaxed);
    match lookup_storaged_endpoint() {
        Ok(fresh) if fresh != failed => Some(fresh),
        _ => None,
    }
}

fn call_storaged(
    request: &CommercialMaxProtocolRequest,
) -> Result<CommercialMaxProtocolResponse, i32> {
    let endpoint = match lookup_storaged_endpoint() {
        Ok(endpoint) => endpoint,
        Err(errno) => {
            if should_log_storage_failure(errno) {
                super::debug_line(&format!(
                    "vfsd: storaged call rejected stage=lookup errno={errno}"
                ));
            }
            return Err(errno);
        }
    };
    let mut response = CommercialMaxProtocolResponse::default();
    let received = unsafe {
        rustos_svc_runtime::syscall::syscall5(
            SYS_RUSTOS_IPC_CALL,
            endpoint,
            (request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut response as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
        )
    };
    if received < 0 {
        STORAGED_ENDPOINT.store(0, Ordering::Relaxed);
        let errno = (-received) as i32;
        if should_log_storage_failure(errno) {
            super::debug_line(&format!(
                "vfsd: storaged call rejected stage=ipc-call errno={errno}"
            ));
        }
        return Err(errno);
    }
    if received as usize != size_of::<CommercialMaxProtocolResponse>()
        || !response.is_valid_envelope_for(request)
    {
        super::debug_line("vfsd: storaged call rejected stage=response-envelope errno=5");
        return Err(EIO);
    }
    if response.status != 0 {
        if should_log_storage_failure(response.status) {
            super::debug_line(&format!(
                "vfsd: storaged call rejected stage=service-status op={} errno={}",
                request.header.op, response.status
            ));
        }
    } else {
        note_storage_ready();
    }
    Ok(response)
}

/// One bulk-read round trip. Returns the kernel's reply length or `-errno`.
/// A bounded call, so a wedged storaged cannot block this lane forever.
///
/// # Safety
/// `response` must point to `size_of::<StoragedBulkReadResponse>()` writable
/// bytes that no one else accesses for the duration of the call.
unsafe fn bulk_read_round_trip(
    endpoint: u64,
    request: &CommercialMaxProtocolRequest,
    response: *mut StoragedBulkReadResponse,
) -> i64 {
    // ORDERING: Relaxed; a statistics counter. The returned sequence number
    // only picks which calls pay for a clock reading.
    let sequence = BULK_READ_CALLS.fetch_add(1, Ordering::Relaxed);
    let sampled = sequence % TIMING_SAMPLE_PERIOD == 0;
    let started = if sampled {
        rustos_svc_runtime::syscall::monotonic_nanos()
    } else {
        0
    };
    // The reply buffer is deliberately not cleared: the kernel writes exactly
    // the reply, and `payload_of_reply` never exposes bytes past it.
    let received = unsafe {
        rustos_svc_runtime::syscall::syscall6(
            rustos_user_abi::syscall::SYS_RUSTOS_IPC_CALL_BOUNDED,
            endpoint,
            (request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            response as u64,
            size_of::<StoragedBulkReadResponse>() as u64,
            rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS,
        )
    };
    if sampled {
        let call_ns = rustos_svc_runtime::syscall::monotonic_nanos().saturating_sub(started);
        BULK_READ_NS.fetch_add(call_ns.saturating_mul(TIMING_SAMPLE_PERIOD), Ordering::Relaxed);
        BULK_READ_MAX_NS.fetch_max(call_ns, Ordering::Relaxed);
    }
    received
}

/// Checks a reply of `received` bytes against `request` and returns the
/// payload bytes it carries: the reply must be exactly header + `payload_len`,
/// carry the request's envelope, report success and bind to the requested
/// generation, LBA and block count. Nothing past `received` is read.
fn validate_bulk_reply<'a>(
    request: &CommercialMaxProtocolRequest,
    response: &'a StoragedBulkReadResponse,
    received: usize,
    expected_len: usize,
) -> Result<&'a [u8], i32> {
    if received < STORAGED_BULK_READ_RESPONSE_HEADER_BYTES
        || !response.is_valid_envelope_for(request)
    {
        // Not a storaged reply to this request: do not keep trusting the endpoint.
        STORAGED_ENDPOINT.store(0, Ordering::Relaxed);
        super::debug_line("vfsd: storaged bulk call rejected stage=response-envelope errno=5");
        return Err(EIO);
    }
    if response.status != 0 {
        if should_log_storage_failure(response.status) {
            super::debug_line(&format!(
                "vfsd: storaged bulk call rejected stage=service-status op={} errno={}",
                request.header.op, response.status
            ));
        }
        return Err(response.status);
    }
    let payload = response.payload_of_reply(received);
    match payload {
        Some(payload)
            if payload.len() == expected_len
                && response.generation == request.arg0
                && response.lba == request.arg1
                && response.block_count == request.arg2 =>
        {
            note_storage_ready();
            Ok(payload)
        }
        _ => {
            STORAGED_ENDPOINT.store(0, Ordering::Relaxed);
            super::debug_line("vfsd: storaged bulk call rejected stage=response-binding errno=5");
            Err(EIO)
        }
    }
}

pub(super) fn call_storaged_bulk(
    request: &CommercialMaxProtocolRequest,
    out: &mut [u8],
    response_words: &mut [u64],
) -> Result<(), i32> {
    if out.is_empty()
        || out.len() > STORAGED_BULK_READ_PAYLOAD_CAPACITY
        || response_words.len() < size_of::<StoragedBulkReadResponse>().div_ceil(size_of::<u64>())
    {
        return Err(EINVAL);
    }
    let (mut endpoint, mut cached) = storaged_endpoint().inspect_err(|errno| {
        if should_log_storage_failure(*errno) {
            super::debug_line(&format!(
                "vfsd: storaged bulk call rejected stage=lookup errno={errno}"
            ));
        }
    })?;
    // SAFETY: u64 storage has sufficient size/alignment (asserted above),
    // is exclusively borrowed, and the all-integer wire type admits any bits.
    let response = response_words
        .as_mut_ptr()
        .cast::<StoragedBulkReadResponse>();
    let received = loop {
        // SAFETY: `response` is the exclusive, large-enough reply buffer.
        let received = unsafe { bulk_read_round_trip(endpoint, request, response) };
        if received >= 0 {
            break received;
        }
        if cached {
            if let Some(fresh) = retry_with_fresh_endpoint(endpoint) {
                endpoint = fresh;
                cached = false;
                continue;
            }
        } else {
            STORAGED_ENDPOINT.store(0, Ordering::Relaxed);
        }
        let errno = (-received) as i32;
        if should_log_storage_failure(errno) {
            super::debug_line(&format!(
                "vfsd: storaged bulk call rejected stage=ipc-call errno={errno}"
            ));
        }
        return Err(errno);
    };
    // SAFETY: the buffer is initialized u64 storage and outlives this borrow.
    let response_ref = unsafe { &*response };
    let payload = validate_bulk_reply(request, response_ref, received as usize, out.len())?;
    // The one copy: the kernel reply is a single contiguous header + payload
    // message, so the payload cannot land in `out` directly.
    out.copy_from_slice(payload);
    Ok(())
}

fn call_storaged_bulk_write(
    generation: u64,
    lba: u64,
    block_count: u64,
    bytes: &[u8],
    request_words: &mut [u64],
) -> Result<CommercialMaxProtocolResponse, i32> {
    let request_len = StoragedBulkWriteRequest::request_len_for(bytes.len()).ok_or(EINVAL)?;
    if bytes.is_empty()
        || request_words.len() < size_of::<StoragedBulkWriteRequest>().div_ceil(size_of::<u64>())
    {
        return Err(EINVAL);
    }
    let (mut endpoint, mut cached) = storaged_endpoint().inspect_err(|errno| {
        if should_log_storage_failure(*errno) {
            super::debug_line(&format!(
                "vfsd: storaged bulk write rejected stage=lookup errno={errno}"
            ));
        }
    })?;
    let base = storage_request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE_BULK);
    let req_ptr = request_words
        .as_mut_ptr()
        .cast::<StoragedBulkWriteRequest>();
    // SAFETY: `request_words` is exclusively borrowed, 8-aligned and large
    // enough for the whole struct (checked above). Only the header and exactly
    // `bytes.len()` payload bytes are written, and only `request_len` bytes
    // (those) are sent. The payload copy into the contiguous request message
    // is required by the single-buffer IPC ABI.
    unsafe {
        (*req_ptr).header = base.header;
        (*req_ptr).flags = 0;
        (*req_ptr).payload_len = bytes.len() as u32;
        (*req_ptr).generation = generation;
        (*req_ptr).lba = lba;
        (*req_ptr).block_count = block_count;
        (*req_ptr).reserved0 = 0;
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            core::ptr::addr_of_mut!((*req_ptr).payload).cast::<u8>(),
            bytes.len(),
        );
    }
    let mut response = CommercialMaxProtocolResponse::default();
    let received = loop {
        let received = unsafe {
            rustos_svc_runtime::syscall::syscall5(
                SYS_RUSTOS_IPC_CALL,
                endpoint,
                req_ptr as u64,
                request_len as u64,
                (&mut response as *mut CommercialMaxProtocolResponse) as u64,
                size_of::<CommercialMaxProtocolResponse>() as u64,
            )
        };
        if received >= 0 {
            break received;
        }
        // The retry is an idempotent rewrite of the same blocks at the same
        // generation, and only happens when storaged was replaced.
        if cached {
            if let Some(fresh) = retry_with_fresh_endpoint(endpoint) {
                endpoint = fresh;
                cached = false;
                continue;
            }
        } else {
            STORAGED_ENDPOINT.store(0, Ordering::Relaxed);
        }
        let errno = (-received) as i32;
        if should_log_storage_failure(errno) {
            super::debug_line(&format!(
                "vfsd: storaged bulk write rejected stage=ipc-call errno={errno}"
            ));
        }
        return Err(errno);
    };
    if received as usize != size_of::<CommercialMaxProtocolResponse>()
        || response.header != base.header
    {
        STORAGED_ENDPOINT.store(0, Ordering::Relaxed);
        super::debug_line("vfsd: storaged bulk write rejected stage=response-envelope errno=5");
        return Err(EIO);
    }
    if response.status != 0 {
        if should_log_storage_failure(response.status) {
            super::debug_line(&format!(
                "vfsd: storaged bulk write rejected stage=service-status errno={}",
                response.status
            ));
        }
    } else {
        note_storage_ready();
    }
    Ok(response)
}

pub(super) fn is_transient_storage_not_ready(errno: i32) -> bool {
    matches!(errno, EAGAIN | ENODEV | ENOSYS)
}

fn should_log_storage_failure(errno: i32) -> bool {
    !is_transient_storage_not_ready(errno)
        || !STORAGE_NOT_READY_DIAGNOSTIC_EMITTED.swap(true, Ordering::AcqRel)
}

fn note_storage_ready() {
    STORAGE_NOT_READY_DIAGNOSTIC_EMITTED.store(false, Ordering::Release);
}

fn read_block_info(payload: &[u8]) -> Option<DvmBlockInfoWire> {
    if payload.len() < size_of::<DvmBlockInfoWire>() {
        return None;
    }
    let mut value = DvmBlockInfoWire::default();
    unsafe {
        ptr::copy_nonoverlapping(
            payload.as_ptr(),
            (&mut value as *mut DvmBlockInfoWire).cast::<u8>(),
            size_of::<DvmBlockInfoWire>(),
        );
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_requests_are_versioned_and_target_storaged() {
        let request = storage_request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK);
        assert_eq!(request.header.version, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION);
        assert_eq!(request.header.protocol, COMMERCIAL_MAX_PROTOCOL_STORAGED);
        assert_eq!(request.payload_len, 0);
    }

    #[test]
    fn bulk_read_chunk_is_block_aligned_and_strictly_inside_inline_capacity() {
        let block_size = 4096;
        let max_blocks = STORAGED_BULK_READ_PAYLOAD_CAPACITY / block_size;
        assert_eq!(max_blocks, 15);
        assert_eq!(max_blocks * block_size, 60 * 1024);
        assert!(max_blocks * block_size <= STORAGED_BULK_READ_PAYLOAD_CAPACITY);
        assert!((max_blocks + 1) * block_size > STORAGED_BULK_READ_PAYLOAD_CAPACITY);
    }

    #[test]
    fn bulk_cooperative_yield_batches_four_inline_replies() {
        let inline = 60 * 1024;
        let (after_one, yielded) =
            cooperative_bulk_yield_state(inline, BULK_COOPERATIVE_YIELD_BYTES);
        assert_eq!(after_one, inline);
        assert!(!yielded);

        let (after_four, yielded) =
            cooperative_bulk_yield_state(inline * 4, BULK_COOPERATIVE_YIELD_BYTES);
        assert_eq!(after_four, inline * 4);
        assert!(!yielded);

        let (remainder, yielded) =
            cooperative_bulk_yield_state(inline * 5, BULK_COOPERATIVE_YIELD_BYTES);
        assert_eq!(remainder, inline * 5 - BULK_COOPERATIVE_YIELD_BYTES);
        assert!(yielded);
    }

    #[test]
    fn only_startup_storage_absence_is_diagnostic_rate_limited() {
        assert!(is_transient_storage_not_ready(EAGAIN));
        assert!(is_transient_storage_not_ready(ENODEV));
        assert!(is_transient_storage_not_ready(ENOSYS));
        assert!(!is_transient_storage_not_ready(EIO));
    }
}
