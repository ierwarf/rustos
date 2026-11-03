//! Runtimed forwarding of rootd-authored scheduling-context launch authority.
//!
//! Catalog weights are not CPU-time authority. This boundary submits the exact
//! executable path and kernel-visible requester identity to rootd, then accepts
//! only a canonical request-bound one-shot grant.

use super::*;

pub(super) fn report_rootd_service_lease(
    service_id: u64,
    exec_path: &str,
    pid: i32,
) -> Result<(), i32> {
    let endpoint = lookup_rootd_endpoint()?;
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL;
    request.header.subject_pid = u64::from(std::process::id());
    request.header.subject_tid = current_tid();
    let report = rustos_component_model::lease_report::encode(
        service_id,
        u64::try_from(pid).map_err(|_| libc::EINVAL)?,
        exec_path,
    )
    .map_err(|_| libc::EINVAL)?;
    if report.len() > request.payload.len() {
        return Err(libc::EINVAL);
    }
    request.payload_len = report.len() as u32;
    request.payload[..report.len()].copy_from_slice(&report);

    let mut response = CommercialMaxProtocolResponse::default();
    // SAFETY: Both records are fixed-size, live, and correctly mutable for the
    // syscall; ring0 validates endpoint ownership and complete user ranges.
    let call = unsafe {
        rustos_svc_runtime::syscall::syscall6(
            SYS_RUSTOS_IPC_CALL_BOUNDED,
            endpoint as u64,
            (&request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut response as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
            IPC_BOOT_CONTROL_HARD_LIMIT_MS,
        )
    };
    if call < 0 {
        return Err((-call) as i32);
    }
    check_rootd_status(call, &request, &response)?;
    if response.descriptor_count != 0
        || response.payload_len != 0
        || response.value0 != service_id
        || response.value1 != u64::try_from(pid).map_err(|_| libc::EINVAL)?
    {
        return Err(libc::EPROTO);
    }
    Ok(())
}

pub(super) fn request_scheduling_context_authority(
    exec_path: &str,
) -> Result<RustosSchedulingContextAuthority, i32> {
    let endpoint = lookup_service_endpoint(IPC_SERVICE_ROOTD);
    if endpoint <= 0 {
        return Err(if endpoint < 0 {
            (-endpoint) as i32
        } else {
            libc::ENOENT
        });
    }
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
    // SAFETY: Both fixed-size records remain live for the syscall and ring0
    // validates endpoint authority and complete user ranges before copying.
    let call = unsafe {
        rustos_svc_runtime::syscall::syscall6(
            SYS_RUSTOS_IPC_CALL_BOUNDED,
            endpoint as u64,
            (&request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut response as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
            IPC_BOOT_CONTROL_HARD_LIMIT_MS,
        )
    };
    if call < 0 {
        return Err((-call) as i32);
    }
    check_rootd_status(call, &request, &response)?;
    if response.descriptor_count != 0
        || response.payload_len as usize != size_of::<RustosSchedulingContextAuthority>()
    {
        return Err(libc::EPROTO);
    }
    // SAFETY: The envelope proves an exact fixed repr(C) payload length;
    // unaligned decoding imposes no alignment requirement on the byte buffer.
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

// Error replies do not carry the success-only lease or authority fields.
// Authenticate the envelope first, then preserve rootd's errno so transient
// admission failures remain retryable rather than becoming permanent EINVAL.
fn check_rootd_status(
    received: i64,
    request: &CommercialMaxProtocolRequest,
    response: &CommercialMaxProtocolResponse,
) -> Result<(), i32> {
    if received as usize != size_of::<CommercialMaxProtocolResponse>()
        || !response.is_valid_envelope_for(request)
        || response.status < 0
    {
        return Err(libc::EPROTO);
    }
    if response.status != 0 {
        return Err(response.status);
    }
    Ok(())
}

fn lookup_rootd_endpoint() -> Result<u64, i32> {
    let endpoint = lookup_service_endpoint(IPC_SERVICE_ROOTD);
    if endpoint < 0 {
        Err((-endpoint) as i32)
    } else if endpoint == 0 {
        Err(libc::ENOENT)
    } else {
        Ok(endpoint as u64)
    }
}

fn current_tid() -> u64 {
    // SAFETY: `gettid` has no pointer arguments and returns the calling
    // thread's kernel identity without borrowing userspace memory.
    unsafe { libc::syscall(libc::SYS_gettid as libc::c_long) as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rootd_error_reply_preserves_retryable_errno_without_success_fields() {
        let request = CommercialMaxProtocolRequest::default();
        let mut response = CommercialMaxProtocolResponse {
            header: request.header,
            status: libc::EAGAIN,
            ..CommercialMaxProtocolResponse::default()
        };
        let size = size_of::<CommercialMaxProtocolResponse>() as i64;
        assert_eq!(check_rootd_status(size, &request, &response), Err(libc::EAGAIN));
        response.header.subject_pid = 42;
        assert_eq!(check_rootd_status(size, &request, &response), Err(libc::EPROTO));
    }
}
