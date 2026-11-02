//! loaderd-owned cryptographically checked update staging client.
//!
//! A successful verifying-key check does not grant the rootd supervisor
//! permission to swap the image. The two exact immutable snapshots must
//! remain pinned and the old one durably retained by storaged until a
//! separate rollback-proof transaction commits.
//! This adapter is deliberately NOT bound to ordinary user executable
//! spawn: normal ELF/PE image execution policy does not imply release trust.
use core::mem::size_of;
use rustos_component_model::update_wire::{self,Approval};
use rustos_release_verifier::{self,KeyRing,Verified};
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest,CommercialMaxProtocolResponse,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_UPDATE_STAGE,IPC_SERVICE_ROOTD,
    SYS_RUSTOS_IPC_CALL_BOUNDED,
};
use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;
const SYS_GETPID:u64=39;
const SYS_GETTID:u64=186;
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub(super) enum AdmissionError{
    Image(rustos_release_verifier::Error),
    Invalid,Policy(i32),Transport(i32),WrongReply,
}
fn stage_approval(approved:Verified,release_epoch:u64,
                  trial_deadline_ns:u64)->Result<Approval,AdmissionError>{
    if release_epoch==0||trial_deadline_ns==0{return Err(AdmissionError::Invalid)}
    let manifest=approved.manifest();
    if release_epoch!=manifest.generation{return Err(AdmissionError::Invalid)}
    Approval {
        service:manifest.service,epoch:release_epoch,
        digest:approved.digest(),trial_deadline_ns,
    }.validate().map_err(|_|AdmissionError::Invalid)
}
fn call_rootd_stage(approval:Approval)->Result<(),AdmissionError>{
    let endpoint=rustos_svc_runtime::ipc::lookup_service_endpoint(IPC_SERVICE_ROOTD);
    if endpoint<=0{return Err(AdmissionError::Transport(11))}
    let pid=unsafe{rustos_svc_runtime::syscall::syscall0(SYS_GETPID)};
    let tid=unsafe{rustos_svc_runtime::syscall::syscall0(SYS_GETTID)};
    if pid<=0||tid<=0{return Err(AdmissionError::Invalid)}
    let wire=update_wire::encode(approval).map_err(|_|AdmissionError::Invalid)?;
    let mut request=CommercialMaxProtocolRequest::default();
    if wire.len()>request.payload.len(){return Err(AdmissionError::Invalid)}
    request.header.version=COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol=COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op=COMMERCIAL_MAX_ROOTD_OP_UPDATE_STAGE;
    request.header.subject_pid=pid as u64;
    request.header.subject_tid=tid as u64;
    request.payload_len=wire.len() as u32;
    request.payload[..wire.len()].copy_from_slice(&wire);
    let mut reply=CommercialMaxProtocolResponse::default();
    let got=unsafe{rustos_svc_runtime::syscall::syscall6(
        SYS_RUSTOS_IPC_CALL_BOUNDED,
        endpoint as u64,
        (&request as *const CommercialMaxProtocolRequest) as u64,
        size_of::<CommercialMaxProtocolRequest>() as u64,
        (&mut reply as *mut CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
        IPC_BOOT_CONTROL_HARD_LIMIT_MS,
    )};
    if got<0{return Err(AdmissionError::Transport((-got) as i32))}
    if got as usize!=size_of::<CommercialMaxProtocolResponse>()
        ||!reply.is_valid_envelope_for(&request)
        ||reply.descriptor_count!=0 || reply.payload_len!=0
        ||reply.value0!=approval.epoch {
        return Err(AdmissionError::WrongReply)
    }
    if reply.status!=0{return Err(AdmissionError::Policy(reply.status))}
    // UpdateBook::Phase::Staged is encoded in rootd's value1.
    if reply.value1!=1{return Err(AdmissionError::WrongReply)}
    Ok(())
}
fn bundle_error(error:loaderd::release_bundle::BundleError)->AdmissionError{
    match error{
        loaderd::release_bundle::BundleError::Invalid=>AdmissionError::Invalid,
        loaderd::release_bundle::BundleError::Image(error)=>AdmissionError::Image(error),
    }
}
/// Verify a bundle against the sealed keyring and stage its digest with rootd.
/// This is the request path behind `COMMERCIAL_MAX_LOADERD_OP_STAGE_RELEASE`;
/// the caller must already have read the bytes from a sealed read-only file.
pub(super) fn stage_release_bundle(
    bytes:&[u8],service:u64,durable_floor:u64,release_epoch:u64,
    trial_deadline_ns:u64,
)->Result<Verified,AdmissionError>{
    let keys=super::trust_anchor::sealed_keys().map_err(AdmissionError::Image)?;
    let ring=KeyRing::new(&keys).map_err(AdmissionError::Image)?;
    let verified=loaderd::release_bundle::admit_bundle(&ring,bytes,service,durable_floor)
        .map_err(bundle_error)?;
    let approval=stage_approval(verified,release_epoch,trial_deadline_ns)?;
    call_rootd_stage(approval)?;
    Ok(verified)
}
#[cfg(test)]
mod tests{
    use super::*;
    use loaderd::release_bundle::{BUNDLE_HEADER_BYTES,BUNDLE_MAGIC};
    fn zero_bundle()->Vec<u8>{
        let mut bytes=vec![0u8;BUNDLE_HEADER_BYTES+rustos_release_verifier::ENVELOPE_BYTES+2];
        bytes[..8].copy_from_slice(&BUNDLE_MAGIC);
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        bytes
    }
    #[test]
    fn a_non_bundle_is_refused_before_any_rootd_call(){
        assert!(matches!(
            stage_release_bundle(&[],3,0,1,1),
            Err(AdmissionError::Image(_))|Err(AdmissionError::Invalid)
        ));
    }
    #[test]
    fn an_unsigned_well_formed_bundle_never_stages(){
        assert!(matches!(
            stage_release_bundle(&zero_bundle(),3,0,1,1),
            Err(AdmissionError::Image(_))
        ));
    }
}
