//! Asks rootd whether this loader may launch an image of a given personality.
//!
//! The format of the validated image decides the personality (ELF -> Linux,
//! PE -> Windows); rootd decides, from its exact-incarnation personality policy
//! (canonical IDL 523), whether the calling loader incarnation holds that
//! right. A refusal, an unreachable rootd or a malformed reply all fail the
//! spawn: there is no default-allow. The call is on the slow spawn path only.
use core::mem::size_of;
use rustos_component_model::personality::{Personality, SlowRequest};
use rustos_component_model::slow_authorization::{self, Query};
use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_SLOW_REQUEST_AUTHORIZE, IPC_SERVICE_ROOTD,
    PROC_BROKER_FORMAT_ELF64, PROC_BROKER_FORMAT_PE64, SYS_RUSTOS_IPC_CALL_BOUNDED,
};

const EACCES: i32 = 13;
const EAGAIN: i32 = 11;
const EINVAL: i32 = 22;
const EPROTO: i32 = 71;
const SYS_GETPID: u64 = 39;
const SYS_GETTID: u64 = 186;
/// The namespace every personality starts in; rootd's grant names the same one.
const DEFAULT_NAMESPACE: u64 = 1;

pub fn personality_for_format(format: u16) -> Option<Personality> {
    match format {
        PROC_BROKER_FORMAT_ELF64 => Some(Personality::Linux),
        PROC_BROKER_FORMAT_PE64 => Some(Personality::Windows),
        _ => None,
    }
}

/// Fail-closed: `Ok` only when rootd granted this exact launch.
pub fn authorize_launch(format: u16) -> Result<(), i32> {
    let personality = personality_for_format(format).ok_or(EINVAL)?;
    let endpoint = rustos_svc_runtime::ipc::lookup_service_endpoint(IPC_SERVICE_ROOTD);
    if endpoint <= 0 {
        return Err(EAGAIN);
    }
    let pid = unsafe { rustos_svc_runtime::syscall::syscall0(SYS_GETPID) };
    let tid = unsafe { rustos_svc_runtime::syscall::syscall0(SYS_GETTID) };
    if pid <= 0 || tid <= 0 {
        return Err(EINVAL);
    }
    let wire = slow_authorization::encode(
        Query { personality, request: SlowRequest::ExecutableImage, namespace: DEFAULT_NAMESPACE },
        pid as u64,
    )
    .map_err(|_| EINVAL)?;
    let mut request = CommercialMaxProtocolRequest::default();
    if wire.len() > request.payload.len() {
        return Err(EINVAL);
    }
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = COMMERCIAL_MAX_ROOTD_OP_SLOW_REQUEST_AUTHORIZE;
    request.header.subject_pid = pid as u64;
    request.header.subject_tid = tid as u64;
    request.payload_len = wire.len() as u32;
    request.payload[..wire.len()].copy_from_slice(&wire);
    let mut reply = CommercialMaxProtocolResponse::default();
    let got = unsafe {
        rustos_svc_runtime::syscall::syscall6(
            SYS_RUSTOS_IPC_CALL_BOUNDED,
            endpoint as u64,
            (&request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut reply as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
            IPC_BOOT_CONTROL_HARD_LIMIT_MS,
        )
    };
    if got < 0 {
        return Err(EAGAIN);
    }
    if got as usize != size_of::<CommercialMaxProtocolResponse>()
        || !reply.is_valid_envelope_for(&request)
        || reply.descriptor_count != 0
        || reply.payload_len != 0
    {
        return Err(EPROTO);
    }
    if reply.status != 0 {
        return Err(if reply.status > 0 { reply.status } else { EACCES });
    }
    if reply.value0 == 0 {
        return Err(EPROTO);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_image_format_decides_the_personality() {
        assert_eq!(personality_for_format(PROC_BROKER_FORMAT_ELF64), Some(Personality::Linux));
        assert_eq!(personality_for_format(PROC_BROKER_FORMAT_PE64), Some(Personality::Windows));
        assert_eq!(personality_for_format(0), None);
        assert_eq!(personality_for_format(99), None);
    }
}
