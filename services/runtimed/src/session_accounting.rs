//! Production sessiond -> rootd client-Job session accounting.
//!
//! Only slow lifecycle transitions call this route; terminal output, input,
//! normal session reads and the scheduler never perform a charging syscall.
//! The requester binds its own kernel PID/TID to the envelope; rootd checks
//! that sender against the unique live sessiond reporter chain before using
//! its root Job descriptor to admit/refund a client's accounting charge.
use std::mem::size_of;
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_CHARGE, COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_RELEASE,
    IPC_SERVICE_ROOTD, SYS_RUSTOS_IPC_CALL_BOUNDED,
};
use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;

fn build_request(op: u16, token: u64, client_pid: u64, pid: u64, tid: u64)
    -> Result<CommercialMaxProtocolRequest, i32>
{
    if token == 0 || pid == 0 || tid == 0 {
        return Err(libc::EINVAL);
    }
    if op == COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_CHARGE && client_pid == 0 {
        return Err(libc::EINVAL);
    }
    if op == COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_RELEASE && client_pid != 0 {
        return Err(libc::EINVAL);
    }
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = op;
    request.header.subject_pid = pid;
    request.header.subject_tid = tid;
    request.arg0 = token;
    request.arg1 = client_pid;
    Ok(request)
}

fn call(op: u16, token: u64, client_pid: u64) -> Result<(), i32> {
    let endpoint = super::lookup_service_endpoint(IPC_SERVICE_ROOTD);
    if endpoint <= 0 {
        return Err(if endpoint == 0 { libc::EIO } else { (-endpoint) as i32 });
    }
    let pid = unsafe { libc::syscall(libc::SYS_getpid as libc::c_long) as i64 };
    let tid = unsafe { libc::syscall(libc::SYS_gettid as libc::c_long) as i64 };
    if pid <= 0 || tid <= 0 {
        return Err(libc::ESRCH);
    }
    let request = build_request(op, token, client_pid, pid as u64, tid as u64)?;
    let mut response = CommercialMaxProtocolResponse::default();
    let received = unsafe {
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
    if received < 0 {
        return Err((-received) as i32);
    }
    if received as usize != size_of::<CommercialMaxProtocolResponse>()
        || !response.is_valid_envelope_for(&request)
        || response.descriptor_count != 0
        || response.payload_len != 0
    {
        return Err(libc::EPROTO);
    }
    if response.status != 0 {
        return Err(response.status);
    }
    if response.value0 == 0 {
        return Err(libc::EPROTO);
    }
    Ok(())
}

pub(super) fn charge(token: u64, pid: i32) -> Result<(), i32> {
    if pid <= 0 {
        return Err(libc::EINVAL);
    }
    call(COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_CHARGE, token, pid as u64)
}

pub(super) fn release(token: u64) -> Result<(), i32> {
    call(COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_RELEASE, token, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_shape_guards_charges_and_refunds() {
        assert!(build_request(COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_CHARGE, 0, 5, 41, 4).is_err());
        assert!(build_request(COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_CHARGE, 1, 0, 41, 4).is_err());
        let req = build_request(COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_CHARGE, 88, 900, 41, 4).unwrap();
        assert_eq!((req.arg0, req.arg1), (88, 900));
        assert!(req.subject_is_exact_sender(41, 4));
        assert!(!req.subject_is_exact_sender(42, 4));
        assert_eq!(req.payload_len, 0);
        assert_eq!(req.path_len, 0);
        assert!(build_request(COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_RELEASE, 88, 900, 41, 4).is_err());
        let refund = build_request(COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_RELEASE, 88, 0, 41, 4).unwrap();
        assert_eq!(refund.arg1, 0);
    }
}
