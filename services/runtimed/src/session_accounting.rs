//! Authenticated slow sessiond -> rootd Job-accounting control using
//! generated IDL 519 inside a versioned supervisor IPC envelope.
//!
//! The kernel stamps the sender separately. The wire is not an authority.
use std::mem::size_of;
use rustos_component_model::client_session_wire::{
    self, SessionControl, CHARGE, RELEASE,
};
use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_CHARGE, COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_RELEASE,
    IPC_SERVICE_ROOTD, SYS_RUSTOS_IPC_CALL_BOUNDED,
};

fn op_to_wire(op: u16) -> Result<u16, i32> {
    match op {
        COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_CHARGE => Ok(CHARGE),
        COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_RELEASE => Ok(RELEASE),
        _ => Err(libc::EINVAL),
    }
}

fn build_request(
    op: u16,
    token: u64,
    client_pid: u64,
    pid: u64,
    tid: u64,
) -> Result<(CommercialMaxProtocolRequest, SessionControl), i32> {
    if pid == 0 || tid == 0 {
        return Err(libc::EINVAL);
    }
    let operation = op_to_wire(op)?;
    let control = SessionControl::new(token, client_pid, operation)
        .map_err(|_| libc::EINVAL)?;
    let bytes = client_session_wire::encode_request(control)
        .map_err(|_| libc::EINVAL)?;
    let mut req = CommercialMaxProtocolRequest::default();
    if bytes.len() > req.payload.len() {
        return Err(libc::E2BIG);
    }
    req.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    req.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    req.header.op = op;
    req.header.subject_pid = pid;
    req.header.subject_tid = tid;
    req.payload_len = bytes.len() as u32;
    req.payload[..bytes.len()].copy_from_slice(&bytes);
    Ok((req, control))
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
    let (req, control) = build_request(op, token, client_pid, pid as u64, tid as u64)?;
    let mut response = CommercialMaxProtocolResponse::default();
    let received = unsafe {
        rustos_svc_runtime::syscall::syscall6(
            SYS_RUSTOS_IPC_CALL_BOUNDED,
            endpoint as u64,
            (&req as *const CommercialMaxProtocolRequest) as u64,
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
        || !response.is_valid_envelope_for(&req)
        || response.descriptor_count != 0
        || response.value0 != 0
        || response.value1 != 0
    {
        return Err(libc::EPROTO);
    }
    if response.status != 0 {
        return Err(response.status);
    }
    client_session_wire::decode_ack(
        &response.payload[..response.payload_len as usize],
        control,
    ).map_err(|_| libc::EPROTO)?;
    Ok(())
}

fn ambiguous(error: i32) -> bool {
    matches!(error, libc::ETIMEDOUT | libc::EINTR | libc::EAGAIN | libc::ENOMEM | libc::EPROTO)
}

fn bounded_retry(op: u16, token: u64, pid: u64) -> Result<(), i32> {
    let mut last = libc::ETIMEDOUT;
    for _ in 0..3 {
        match call(op, token, pid) {
            Ok(()) => return Ok(()),
            Err(error) if ambiguous(error) => last = error,
            Err(error) => return Err(error),
        }
    }
    Err(last)
}

pub(super) fn charge(token: u64, pid: i32) -> Result<(), i32> {
    if pid <= 0 {
        return Err(libc::EINVAL);
    }
    match bounded_retry(COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_CHARGE, token, pid as u64) {
        Ok(()) => Ok(()),
        Err(error) if ambiguous(error) => {
            // A lost ACK cannot be distinguished from a committed charge.
            // Canonical release writes a tombstone even if the request did
            // not reach rootd. Activation is forbidden until cancellation.
            bounded_retry(COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_RELEASE, token, 0)
                .unwrap_or_else(|refund| {
                    panic!("runtimed: ambiguous client Job charge token={token} refund={refund}");
                });
            Err(error)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn release(token: u64) -> Result<(), i32> {
    bounded_retry(COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_RELEASE, token, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_session_control_requires_exact_subject_and_canonical_payload() {
        let (req, control) = build_request(
            COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_CHARGE, 88, 900, 41, 4,
        ).unwrap();
        assert_eq!(control.operation, CHARGE);
        assert_eq!((req.arg0, req.arg1, req.arg2, req.arg3), (0, 0, 0, 0));
        assert_eq!((req.header.subject_pid, req.header.subject_tid), (41, 4));
        let decoded = client_session_wire::decode_request(
            &req.payload[..req.payload_len as usize],
        );
        assert_eq!(decoded, Ok(control));
        assert!(build_request(
            COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_RELEASE, 88, 900, 41, 4,
        ).is_err());
        let (_, refund) = build_request(
            COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_RELEASE, 88, 0, 41, 4,
        ).unwrap();
        assert_eq!(refund.operation, RELEASE);
        assert!(build_request(COMMERCIAL_MAX_ROOTD_OP_SESSION_ACCOUNT_CHARGE, 0, 900, 41, 4).is_err());
    }
}
