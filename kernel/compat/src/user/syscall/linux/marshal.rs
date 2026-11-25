//! Common Linux syscall marshaling, user-copy, capability-gate, and service RPC helpers.
//!
//! - **Owner:** `kernel-compat` owns the syscall-frame ABI, user-pointer
//!   copyin/copyout error mapping, capability admission checks, and fixed-wire
//!   envelope decoding for Ring3 service brokers.
//! - **Boundary:** Every user pointer, length, and Ring3 service reply slice is
//!   untrusted until validated against exact size and alignment invariants.
//! - **Lifecycle:** Stateless per-call helpers; capability gates run before
//!   user-struct reads, and user buffers are validated before irreversible side
//!   effects or reply copyout.
//! - **Concurrency:** Helpers perform no internal locking and never hold a
//!   process-state guard across user copies or service IPC.
//! - **Failure:** Distinguishes buffer-pointer errno (`user_pointer_error_to_linux_errno`,
//!   returning `EFAULT`) from address-space mapping errno
//!   (`address_space_error_to_linux_errno`, returning `EINVAL` for range errors).
//! - **Forbidden:** Silently substituting `EINVAL` for `EFAULT` on buffer
//!   pointers, bypassing capability checks, or trusting unvalidated payload lengths.
//! - **Evidence:** Unit tests in `marshal::tests` and `kernel-compat` ABI tests.

use core::mem::size_of;
use core::slice;

use rustos_user_abi::syscall::{
    DevmgrdDeviceIoctlRequest, InputdIpcRequest, NetdIpcRequest, ServiceCapability,
    DEVMGRD_IPC_ABI_VERSION, INPUTD_IPC_ABI_VERSION, NETD_IPC_ABI_VERSION,
};
use rustos_user_abi::wire::UserWire;

use super::*;

pub(super) type LinuxResult<T = u64> = Result<T, i64>;

/// Converts a `Result<u64, i64>` Linux syscall outcome into the raw register return value.
#[inline]
pub(super) fn linux_result(result: LinuxResult<u64>) -> u64 {
    match result {
        Ok(value) => value,
        Err(errno) => linux_errno(errno),
    }
}

/// Converts a `Result<(), i64>` Linux syscall outcome into `0` or a negative errno.
#[inline]
pub(super) fn linux_unit_result(result: LinuxResult<()>) -> u64 {
    match result {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

// ---------------------------------------------------------------------------
// User-memory helpers: buffer-pointer (`EFAULT`) vs address-space (`EINVAL`)
// ---------------------------------------------------------------------------

#[inline]
pub(super) fn read_user_struct<T: Copy + Default>(ptr: u64) -> LinuxResult<T> {
    usermem::read_current_user_struct::<T>(ptr).map_err(user_pointer_error_to_linux_errno)
}

#[inline]
pub(super) fn read_user_optional_struct<T: Copy + Default>(ptr: u64) -> LinuxResult<Option<T>> {
    if ptr == 0 {
        return Ok(None);
    }
    read_user_struct::<T>(ptr).map(Some)
}

#[inline]
pub(super) fn write_user_struct<T: UserWire>(ptr: u64, value: &T) -> LinuxResult<()> {
    usermem::write_current_user_struct(ptr, value).map_err(user_pointer_error_to_linux_errno)
}

#[inline]
pub(super) fn read_user_bytes(ptr: u64, dest: &mut [u8]) -> LinuxResult<()> {
    usermem::copy_from_current_user_exact(ptr, dest).map_err(user_pointer_error_to_linux_errno)
}

#[inline]
pub(super) fn write_user_bytes(ptr: u64, bytes: &[u8]) -> LinuxResult<()> {
    usermem::write_current_user_bytes(ptr, bytes).map_err(user_pointer_error_to_linux_errno)
}

#[inline]
pub(super) fn validate_user_write(ptr: u64, len: usize) -> LinuxResult<()> {
    usermem::validate_current_user_write_buffer(ptr, len)
        .map_err(user_pointer_error_to_linux_errno)
}

#[inline]
pub(super) fn validate_user_write_struct<T>(ptr: u64) -> LinuxResult<()> {
    validate_user_write(ptr, size_of::<T>())
}

#[inline]
pub(super) fn read_user_struct_as<T: Copy + Default>(ptr: u64) -> LinuxResult<T> {
    usermem::read_current_user_struct::<T>(ptr).map_err(address_space_error_to_linux_errno)
}

#[inline]
pub(super) fn write_user_struct_as<T: UserWire>(ptr: u64, value: &T) -> LinuxResult<()> {
    usermem::write_current_user_struct(ptr, value).map_err(address_space_error_to_linux_errno)
}

#[inline]
pub(super) fn read_user_bytes_as(ptr: u64, dest: &mut [u8]) -> LinuxResult<()> {
    usermem::copy_from_current_user_exact(ptr, dest).map_err(address_space_error_to_linux_errno)
}

#[inline]
pub(super) fn write_user_bytes_as(ptr: u64, bytes: &[u8]) -> LinuxResult<()> {
    usermem::write_current_user_bytes(ptr, bytes).map_err(address_space_error_to_linux_errno)
}

#[inline]
pub(super) fn validate_user_write_as(ptr: u64, len: usize) -> LinuxResult<()> {
    usermem::validate_current_user_write_buffer(ptr, len)
        .map_err(address_space_error_to_linux_errno)
}

// ---------------------------------------------------------------------------
// Capability & caller identity helpers
// ---------------------------------------------------------------------------

#[inline]
pub(super) fn require_service_capability(cap: ServiceCapability) -> LinuxResult<()> {
    if !ipc_ops::current_process_has_service_capability(cap) {
        return Err(LINUX_EPERM);
    }
    Ok(())
}

#[inline]
pub(super) fn require_any_service_capability(caps: &[ServiceCapability]) -> LinuxResult<()> {
    if !caps
        .iter()
        .copied()
        .any(ipc_ops::current_process_has_service_capability)
    {
        return Err(LINUX_EPERM);
    }
    Ok(())
}

/// Checks `cap` before reading the broker argument struct from user memory.
#[inline]
pub(super) fn require_capability_and_read_args<T: Copy + Default>(
    cap: ServiceCapability,
    args_ptr: u64,
) -> LinuxResult<T> {
    require_service_capability(cap)?;
    read_user_struct::<T>(args_ptr)
}

pub(super) fn require_capability_and_read_args_as<T: Copy + Default>(
    cap: ServiceCapability,
    args_ptr: u64,
) -> LinuxResult<T> {
    require_service_capability(cap)?;
    read_user_struct_as::<T>(args_ptr)
}

#[inline]
pub(super) fn current_user_pid() -> LinuxResult<u64> {
    multitask::current_user_process_id().ok_or(LINUX_ESRCH)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct CallerStamp {
    pub pid: u64,
    pub tid: u64,
    pub session_handle: u64,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
}

impl CallerStamp {
    /// Captures the current user snapshot and process credentials for wire headers.
    pub(super) fn current() -> Self {
        let mut stamp = Self::current_ids_only();
        if let Some(security) = multitask::with_current_process_credentials(|security| security) {
            stamp.uid = security.uid();
            stamp.gid = security.gid();
            stamp.euid = security.euid();
            stamp.egid = security.egid();
        }
        stamp
    }

    /// Captures only `(pid, tid, session_handle)` without taking process credentials.
    pub(super) fn current_ids_only() -> Self {
        Self::try_current_ids_only().unwrap_or_default()
    }

    /// Captures `(pid, tid, session_handle)` or returns `LINUX_ESRCH` if no
    /// current user snapshot is published.
    pub(super) fn try_current_ids_only() -> LinuxResult<Self> {
        let snapshot = multitask::current_user_snapshot().ok_or(LINUX_ESRCH)?;
        Ok(Self {
            pid: snapshot.process_id(),
            tid: snapshot.thread_id(),
            session_handle: snapshot.console_session().raw(),
            ..Self::default()
        })
    }
}

// ---------------------------------------------------------------------------
// Fixed-size service wire & payload helpers
// ---------------------------------------------------------------------------

#[inline]
pub(super) fn wire_as_bytes<T>(value: &T) -> &[u8] {
    // SAFETY: `value` is live and immutably borrowed for the returned slice;
    // the exact object representation is read before the borrow ends.
    unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

pub(super) use wire_as_bytes as as_bytes;

#[inline]
pub(super) fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    assert!(bytes.len() >= size_of::<T>());
    // SAFETY: The assertion proves a complete `T` representation is readable;
    // unaligned reads have no pointer alignment requirement.
    unsafe { bytes.as_ptr().cast::<T>().read_unaligned() }
}

#[inline]
pub(super) fn check_ipc_status(status: i32) -> LinuxResult<()> {
    if status != 0 {
        return Err(i64::from(status.unsigned_abs()));
    }
    Ok(())
}

#[inline]
pub(super) fn decode_fixed_response<R: Copy>(bytes: &[u8]) -> LinuxResult<R> {
    if bytes.len() != size_of::<R>() {
        return Err(LINUX_EINVAL);
    }
    // SAFETY: The length check above proves a complete `R` representation is
    // readable, and `read_unaligned` has no alignment requirement.
    Ok(unsafe { bytes.as_ptr().cast::<R>().read_unaligned() })
}

#[inline]
pub(super) fn exact_payload<'a>(
    payload: &'a [u8],
    payload_len: u32,
    expected: usize,
) -> LinuxResult<&'a [u8]> {
    let len = payload_len as usize;
    if len != expected || len > payload.len() {
        return Err(LINUX_EINVAL);
    }
    Ok(&payload[..len])
}

#[inline]
pub(super) fn decode_exact_payload<T: Copy>(payload: &[u8], payload_len: u32) -> LinuxResult<T> {
    let bytes = exact_payload(payload, payload_len, size_of::<T>())?;
    // SAFETY: `exact_payload` guarantees `bytes.len() == size_of::<T>()`, and
    // `read_unaligned` requires no pointer alignment.
    Ok(unsafe { bytes.as_ptr().cast::<T>().read_unaligned() })
}

#[inline]
pub(super) fn copy_exact_payload_to_user(
    out_ptr: u64,
    payload: &[u8],
    payload_len: u32,
    expected: usize,
) -> LinuxResult<()> {
    let slice = exact_payload(payload, payload_len, expected)?;
    write_user_bytes(out_ptr, slice)
}

#[inline]
pub(super) fn copy_exact_payload_to_user_as(
    out_ptr: u64,
    payload: &[u8],
    payload_len: u32,
    expected: usize,
) -> LinuxResult<()> {
    let slice = exact_payload(payload, payload_len, expected)?;
    write_user_bytes_as(out_ptr, slice)
}

#[inline]
pub(super) fn copy_struct_into_payload<T>(
    dst: &mut [u8],
    dst_len: &mut u32,
    value: &T,
) -> LinuxResult<()> {
    let bytes = wire_as_bytes(value);
    if bytes.len() > dst.len() {
        return Err(LINUX_EINVAL);
    }
    dst[..bytes.len()].copy_from_slice(bytes);
    *dst_len = bytes.len() as u32;
    Ok(())
}

pub(super) fn call_fixed_service_rpc<Req, Resp: Copy>(
    service_id: u64,
    request: &Req,
    class: ipc_ops::ServiceIpcClass,
    timeout_ms: Option<u64>,
) -> LinuxResult<Resp> {
    let bytes = match timeout_ms {
        Some(timeout_ms) => ipc_ops::call_service_endpoint_with_class_deadline(
            service_id,
            wire_as_bytes(request),
            class,
            timeout_ms,
        )?,
        None => {
            ipc_ops::call_service_endpoint_with_class(service_id, wire_as_bytes(request), class)?
        }
    };
    decode_fixed_response::<Resp>(bytes.as_slice())
}

pub(super) fn new_netd_request(op: u16) -> NetdIpcRequest {
    let stamp = CallerStamp::current();
    NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        op,
        pid: stamp.pid,
        tid: stamp.tid,
        uid: stamp.uid,
        gid: stamp.gid,
        euid: stamp.euid,
        egid: stamp.egid,
        ..NetdIpcRequest::default()
    }
}

pub(super) fn new_devmgrd_ioctl_request(
    op: u16,
    fd: u64,
    request_number: u64,
    arg: u64,
) -> DevmgrdDeviceIoctlRequest {
    let stamp = CallerStamp::current();
    DevmgrdDeviceIoctlRequest {
        version: DEVMGRD_IPC_ABI_VERSION,
        op,
        fd,
        request: request_number,
        arg,
        pid: stamp.pid,
        tid: stamp.tid,
        session_handle: stamp.session_handle,
        uid: stamp.uid,
        gid: stamp.gid,
        euid: stamp.euid,
        egid: stamp.egid,
        ..DevmgrdDeviceIoctlRequest::default()
    }
}

pub(super) fn new_inputd_request(op: u16, access: u16) -> InputdIpcRequest {
    let stamp = CallerStamp::current_ids_only();
    InputdIpcRequest {
        version: INPUTD_IPC_ABI_VERSION,
        op,
        access,
        pid: stamp.pid,
        tid: stamp.tid,
        ..InputdIpcRequest::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_result_and_unit_result_encode_ok_and_errno() {
        assert_eq!(linux_result(Ok(42)), 42);
        assert_eq!(linux_result(Err(LINUX_EINVAL)), (-LINUX_EINVAL) as u64);
        assert_eq!(linux_unit_result(Ok(())), 0);
        assert_eq!(linux_unit_result(Err(LINUX_EPERM)), (-LINUX_EPERM) as u64);
    }

    #[test]
    fn check_ipc_status_maps_nonzero_status_to_positive_errno() {
        assert_eq!(check_ipc_status(0), Ok(()));
        assert_eq!(check_ipc_status(-22), Err(22));
        assert_eq!(check_ipc_status(13), Err(13));
    }

    #[test]
    fn decode_fixed_response_rejects_short_or_oversized_buffers() {
        let val: u64 = 0x1122_3344_5566_7788;
        let bytes = val.to_le_bytes();
        assert_eq!(decode_fixed_response::<u64>(&bytes), Ok(val));
        assert_eq!(decode_fixed_response::<u64>(&bytes[..7]), Err(LINUX_EINVAL));
        let mut long = [0_u8; 9];
        long[..8].copy_from_slice(&bytes);
        assert_eq!(decode_fixed_response::<u64>(&long), Err(LINUX_EINVAL));
    }

    #[test]
    fn exact_payload_and_decode_reject_mismatched_or_overflowing_lengths() {
        let buf = [1_u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(exact_payload(&buf, 4, 4), Ok(&buf[..4]));
        assert_eq!(exact_payload(&buf, 3, 4), Err(LINUX_EINVAL));
        assert_eq!(exact_payload(&buf, 5, 4), Err(LINUX_EINVAL));
        assert_eq!(exact_payload(&buf[..3], 4, 4), Err(LINUX_EINVAL));

        assert_eq!(
            decode_exact_payload::<u32>(&buf, 4),
            Ok(u32::from_le_bytes([1, 2, 3, 4]))
        );
        assert_eq!(decode_exact_payload::<u32>(&buf, 8), Err(LINUX_EINVAL));
    }

    #[test]
    fn copy_struct_into_payload_checks_capacity() {
        let val: u64 = 0xdead_beef_cafe_babe;
        let mut dst = [0_u8; 8];
        let mut len = 0_u32;
        assert_eq!(copy_struct_into_payload(&mut dst, &mut len, &val), Ok(()));
        assert_eq!(len, 8);
        assert_eq!(dst, val.to_ne_bytes());

        let mut small = [0_u8; 4];
        let mut small_len = 0_u32;
        assert_eq!(
            copy_struct_into_payload(&mut small, &mut small_len, &val),
            Err(LINUX_EINVAL)
        );
        assert_eq!(small_len, 0);
    }
}
