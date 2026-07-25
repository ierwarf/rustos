use alloc::format;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, DvmBlockInfoWire,
    StoragedBulkReadResponse, BLOCK_BROKER_INFO_FLAG_READ_ONLY,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_STORAGED,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE,
    IPC_SERVICE_STORAGED, STORAGED_BULK_READ_PAYLOAD_CAPACITY, SYS_RUSTOS_IPC_CALL,
    SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
};
use storage_core::{BlockDevice, IoResult, StorageError};

use super::{EAGAIN, EINVAL, EIO, ENODEV, ENOSYS};
use vfsd::{
    admit_dvm_block_geometry, cooperative_bulk_yield_state, storage_error_from_linux_status,
    validate_dvm_block_range,
};

const IPC_BLOCK_PAYLOAD_BYTES: usize =
    rustos_user_abi::syscall::COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY;
// FAT may satisfy a large executable snapshot as hundreds of cache-hot block
// requests after one DVM read-ahead completion. Bound the uninterrupted
// System-class service burst independently of individual IPC chunk size.
const BULK_COOPERATIVE_YIELD_BYTES: usize = 64 * 1024;
static mut STORAGED_BULK_RESPONSE_SLOT: StoragedBulkReadResponse =
    StoragedBulkReadResponse::zeroed();
// Not-ready is an expected, bounded state while the DVM proves its initial
// generation. Keep one diagnostic transition instead of turning every glibc
// negative probe into debugcon I/O and scheduler pressure.
static STORAGE_NOT_READY_DIAGNOSTIC_EMITTED: AtomicBool = AtomicBool::new(false);

pub(super) struct BootBlockDevice {
    pub(super) generation: u64,
    pub(super) block_size: usize,
    pub(super) block_count: u64,
    bulk_bytes_since_yield: usize,
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
        let (block_size, block_count) = admit_dvm_block_geometry(
            info,
            response.value0,
            response.value1,
            IPC_BLOCK_PAYLOAD_BYTES,
            BLOCK_BROKER_INFO_FLAG_READ_ONLY,
        )?;
        Ok(Self {
            generation: info.generation,
            block_size,
            block_count,
            bulk_bytes_since_yield: 0,
        })
    }

    fn read_transfer(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        validate_dvm_block_range(self.block_size, self.block_count, lba, out.len())?;
        let max_blocks = (STORAGED_BULK_READ_PAYLOAD_CAPACITY / self.block_size) as u64;
        if max_blocks == 0 {
            return Err(StorageError::Unsupported);
        }
        let mut done = 0usize;
        while done < out.len() {
            let remaining_blocks = ((out.len() - done) / self.block_size) as u64;
            let block_count = remaining_blocks.min(max_blocks);
            let byte_len = block_count as usize * self.block_size;
            let chunk_lba = lba
                .checked_add((done / self.block_size) as u64)
                .ok_or(StorageError::InvalidInput)?;
            let mut request = storage_request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK);
            request.arg0 = self.generation;
            request.arg1 = chunk_lba;
            request.arg2 = block_count;
            call_storaged_bulk(&request, &mut out[done..done + byte_len])
                .map_err(|errno| storage_error_from_linux_status(-(errno as i64)))?;
            self.account_bulk_progress(byte_len);
            done += byte_len;
        }
        Ok(())
    }

    fn write_transfer(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        validate_dvm_block_range(self.block_size, self.block_count, lba, input.len())?;
        let max_blocks = (IPC_BLOCK_PAYLOAD_BYTES / self.block_size) as u64;
        if max_blocks == 0 {
            return Err(StorageError::Unsupported);
        }
        let mut done = 0usize;
        while done < input.len() {
            let remaining_blocks = ((input.len() - done) / self.block_size) as u64;
            let block_count = remaining_blocks.min(max_blocks);
            let byte_len = block_count as usize * self.block_size;
            let chunk_lba = lba
                .checked_add((done / self.block_size) as u64)
                .ok_or(StorageError::InvalidInput)?;
            let mut request = storage_request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE);
            request.arg0 = self.generation;
            request.arg1 = chunk_lba;
            request.arg2 = block_count;
            request.payload_len = byte_len as u32;
            request.payload[..byte_len].copy_from_slice(&input[done..done + byte_len]);
            let response = call_storaged(&request)
                .map_err(|errno| storage_error_from_linux_status(-(errno as i64)))?;
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
        self.write_transfer(lba, input)
    }

    fn flush(&mut self) -> IoResult<()> {
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

fn storage_request(operation: u16) -> CommercialMaxProtocolRequest {
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_STORAGED;
    request.header.op = operation;
    let pid = unsafe { rustos_svc_runtime::syscall::syscall0(rustos_user_abi::linux::SYS_GETPID) };
    let tid = unsafe { rustos_svc_runtime::syscall::syscall0(rustos_user_abi::linux::SYS_GETTID) };
    if pid > 0 && tid > 0 {
        request.header.subject_pid = pid as u64;
        request.header.subject_tid = tid as u64;
    }
    request
}

fn call_storaged(
    request: &CommercialMaxProtocolRequest,
) -> Result<CommercialMaxProtocolResponse, i32> {
    let endpoint = unsafe {
        rustos_svc_runtime::syscall::syscall1(
            SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
            IPC_SERVICE_STORAGED,
        )
    };
    if endpoint <= 0 {
        let errno = if endpoint < 0 {
            (-endpoint) as i32
        } else {
            ENODEV
        };
        if should_log_storage_failure(errno) {
            super::debug_line(&format!(
                "vfsd: storaged call rejected stage=lookup errno={errno}"
            ));
        }
        return Err(errno);
    }
    let mut response = CommercialMaxProtocolResponse::default();
    let received = unsafe {
        rustos_svc_runtime::syscall::syscall5(
            SYS_RUSTOS_IPC_CALL,
            endpoint as u64,
            (request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut response as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
        )
    };
    if received < 0 {
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

fn call_storaged_bulk(request: &CommercialMaxProtocolRequest, out: &mut [u8]) -> Result<(), i32> {
    if out.is_empty() || out.len() > STORAGED_BULK_READ_PAYLOAD_CAPACITY {
        return Err(EINVAL);
    }
    let endpoint = unsafe {
        rustos_svc_runtime::syscall::syscall1(
            SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
            IPC_SERVICE_STORAGED,
        )
    };
    if endpoint <= 0 {
        let errno = if endpoint < 0 {
            (-endpoint) as i32
        } else {
            ENODEV
        };
        if should_log_storage_failure(errno) {
            super::debug_line(&format!(
                "vfsd: storaged bulk call rejected stage=lookup errno={errno}"
            ));
        }
        return Err(errno);
    }

    let response = core::ptr::addr_of_mut!(STORAGED_BULK_RESPONSE_SLOT);
    unsafe {
        core::ptr::write_bytes(
            response.cast::<u8>(),
            0,
            size_of::<StoragedBulkReadResponse>(),
        );
    }
    let received = unsafe {
        rustos_svc_runtime::syscall::syscall5(
            SYS_RUSTOS_IPC_CALL,
            endpoint as u64,
            (request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            response as u64,
            size_of::<StoragedBulkReadResponse>() as u64,
        )
    };
    if received < 0 {
        let errno = (-received) as i32;
        if should_log_storage_failure(errno) {
            super::debug_line(&format!(
                "vfsd: storaged bulk call rejected stage=ipc-call errno={errno}"
            ));
        }
        return Err(errno);
    }
    let response_ref = unsafe { &*response };
    if received as usize != size_of::<StoragedBulkReadResponse>()
        || !response_ref.is_valid_envelope_for(request)
    {
        super::debug_line("vfsd: storaged bulk call rejected stage=response-envelope errno=5");
        return Err(EIO);
    }
    if response_ref.status != 0 {
        if should_log_storage_failure(response_ref.status) {
            super::debug_line(&format!(
                "vfsd: storaged bulk call rejected stage=service-status op={} errno={}",
                request.header.op, response_ref.status
            ));
        }
        return Err(response_ref.status);
    }
    note_storage_ready();
    if response_ref.generation != request.arg0
        || response_ref.lba != request.arg1
        || response_ref.block_count != request.arg2
        || response_ref.payload_len as usize != out.len()
    {
        super::debug_line("vfsd: storaged bulk call rejected stage=response-binding errno=5");
        return Err(EIO);
    }
    out.copy_from_slice(&response_ref.payload[..out.len()]);
    Ok(())
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
    fn only_startup_storage_absence_is_diagnostic_rate_limited() {
        assert!(is_transient_storage_not_ready(EAGAIN));
        assert!(is_transient_storage_not_ready(ENODEV));
        assert!(is_transient_storage_not_ready(ENOSYS));
        assert!(!is_transient_storage_not_ready(EIO));
    }
}
