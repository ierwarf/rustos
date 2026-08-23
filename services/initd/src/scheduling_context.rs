//! Initd forwarding of rootd-authored scheduling-context launch authority.
//!
//! Initd supplies its kernel-observed identity and exact executable path, then
//! accepts only a canonical, descriptor-free response bound to that request.

use super::*;

pub(super) fn request_scheduling_context_authority(
    exec_path: &str,
) -> Result<RustosSchedulingContextAuthority, i32> {
    let endpoint = lookup_service_endpoint(IPC_SERVICE_ROOTD)?;
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = COMMERCIAL_MAX_ROOTD_OP_SCHEDULING_CONTEXT_GRANT;
    request.header.subject_pid = u64::from(std::process::id());
    request.header.subject_tid = current_tid();
    let path = exec_path.as_bytes();
    if path.is_empty() || path.len() > request.path.len() || path.contains(&0) {
        return Err(libc::EINVAL);
    }
    request.path_len = path.len() as u32;
    request.path[..path.len()].copy_from_slice(path);
    let mut response = CommercialMaxProtocolResponse::default();
    // SAFETY: Both fixed-size request and response live for the syscall, their
    // complete ranges are writable/readable as required, and ring0 validates
    // endpoint ownership plus every user range before copying.
    let call = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_CALL_BOUNDED as libc::c_long,
            endpoint,
            (&request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut response as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
            IPC_BOOT_CONTROL_HARD_LIMIT_MS,
        ) as i64
    };
    if call < 0 {
        return Err((-call) as i32);
    }
    if call as usize != size_of::<CommercialMaxProtocolResponse>()
        || !response.is_valid_envelope_for(&request)
        || response.status != 0
        || response.descriptor_count != 0
        || response.payload_len as usize != size_of::<RustosSchedulingContextAuthority>()
    {
        return Err(libc::EINVAL);
    }
    // SAFETY: The validated payload length is exactly the fixed repr(C) wire
    // type. `read_unaligned` avoids imposing alignment on the byte payload.
    let authority = unsafe {
        core::ptr::read_unaligned(
            response
                .payload
                .as_ptr()
                .cast::<RustosSchedulingContextAuthority>(),
        )
    };
    if authority.token == 0
        || response.value0 != authority.token
        || !authority.policy.is_canonical()
    {
        return Err(libc::EINVAL);
    }
    Ok(authority)
}
