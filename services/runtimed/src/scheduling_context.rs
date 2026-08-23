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
    request.arg0 = service_id;
    request.arg1 = u64::try_from(pid).map_err(|_| libc::EINVAL)?;
    let path = exec_path.as_bytes();
    if path.is_empty() || path.len() > request.path.len() || path.contains(&0) {
        return Err(libc::EINVAL);
    }
    request.path_len = path.len() as u32;
    request.path[..path.len()].copy_from_slice(path);

    let mut response = CommercialMaxProtocolResponse::default();
    // SAFETY: Both records are fixed-size, live, and correctly mutable for the
    // syscall; ring0 validates endpoint ownership and complete user ranges.
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
        || response.descriptor_count != 0
        || response.payload_len != 0
        || response.value0 != service_id
        || response.value1 != u64::try_from(pid).map_err(|_| libc::EINVAL)?
    {
        return Err(libc::EINVAL);
    }
    if response.status != 0 {
        return Err(response.status);
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
