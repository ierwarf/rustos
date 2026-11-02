//! Typed initd -> rootd update transaction control.
//!
//! Operations require the kernel-stamped caller to be the exact initd
//! supervisor. An unsigned digest is never authorization to arm or seal.
//! Actual old-image durability, candidate activation and restored-image
//! verification remain independently owned by loaderd/storaged.
use core::mem::size_of;
use rustos_component_model::update_wire::{self,Approval};
use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest,CommercialMaxProtocolResponse,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_UPDATE_ARM,
    COMMERCIAL_MAX_ROOTD_OP_UPDATE_ROLLBACK,
    COMMERCIAL_MAX_ROOTD_OP_UPDATE_SEAL,
    IPC_SERVICE_ROOTD,SYS_RUSTOS_IPC_CALL_BOUNDED,
    COMMERCIAL_MAX_LOADERD_OP_STAGE_RELEASE,COMMERCIAL_MAX_PROTOCOL_LOADERD,
    IPC_SERVICE_LOADERD,
};
const EINVAL:i32=22;const EPROTO:i32=71;
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub(super) enum Phase{
    Empty,Staged,Armed,Trial,Committed,RollbackRequired,RolledBack,
}
impl Phase{
    fn parse(value:u64)->Result<Self,i32>{
        Ok(match value{
            0=>Self::Empty,1=>Self::Staged,2=>Self::Armed,
            3=>Self::Trial,4=>Self::Committed,5=>Self::RollbackRequired,
            6=>Self::RolledBack,
            _=>return Err(EPROTO)
        })
    }
}
fn exact_subject()->Result<(u64,u64),i32>{
    let pid=unsafe{libc::syscall(libc::SYS_getpid) as i64};
    let tid=unsafe{libc::syscall(libc::SYS_gettid) as i64};
    if pid<=0||tid<=0{return Err(libc::ESRCH)}
    Ok((pid as u64,tid as u64))
}
fn transact(mut request:CommercialMaxProtocolRequest)
    ->Result<CommercialMaxProtocolResponse,i32>{
    let endpoint=rustos_svc_runtime::ipc::lookup_service_endpoint(IPC_SERVICE_ROOTD);
    if endpoint<=0{return Err(if endpoint==0{libc::EAGAIN}else{(-endpoint) as i32})}
    let (pid,tid)=exact_subject()?;
    request.header.version=COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol=COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.subject_pid=pid;
    request.header.subject_tid=tid;
    let mut response=CommercialMaxProtocolResponse::default();
    let got=unsafe{rustos_svc_runtime::syscall::syscall6(
        SYS_RUSTOS_IPC_CALL_BOUNDED,endpoint as u64,
        (&request as *const CommercialMaxProtocolRequest) as u64,
        size_of::<CommercialMaxProtocolRequest>() as u64,
        (&mut response as *mut CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
        IPC_BOOT_CONTROL_HARD_LIMIT_MS,
    )};
    if got<0{return Err((-got) as i32)}
    if got as usize!=size_of::<CommercialMaxProtocolResponse>()
       ||!response.is_valid_envelope_for(&request)
       ||response.descriptor_count!=0{return Err(EPROTO)}
    if response.status!=0{return Err(response.status)}
    Ok(response)
}
fn apply(op:u16,approval:Approval,expected:Phase)->Result<Phase,i32>{
    if !matches!(op,COMMERCIAL_MAX_ROOTD_OP_UPDATE_ARM|
        COMMERCIAL_MAX_ROOTD_OP_UPDATE_ROLLBACK|
        COMMERCIAL_MAX_ROOTD_OP_UPDATE_SEAL){return Err(EINVAL)}
    let wire=update_wire::encode(approval).map_err(|_|EINVAL)?;
    let mut request=CommercialMaxProtocolRequest::default();
    if wire.len()>request.payload.len(){return Err(EINVAL)}
    request.header.op=op;
    request.payload_len=wire.len() as u32;
    request.payload[..wire.len()].copy_from_slice(&wire);
    let response=transact(request)?;
    if response.payload_len!=0||response.value0!=approval.epoch{return Err(EPROTO)}
    let phase=Phase::parse(response.value1)?;
    if phase!=expected{return Err(EPROTO)}
    Ok(phase)
}
/// Retire a transaction whose restored incarnation passed rootd's health
/// quorum. The approval is rebuilt from rootd's own INSPECT snapshot.
pub(super) fn seal_rolled_back(approval:Approval)->Result<(),i32>{
    apply(COMMERCIAL_MAX_ROOTD_OP_UPDATE_SEAL,approval,
        Phase::RolledBack).map(|_|())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn update_phase_decoding_fails_closed(){
        assert_eq!(Phase::parse(5),Ok(Phase::RollbackRequired));
        assert_eq!(Phase::parse(6),Ok(Phase::RolledBack));
        assert_eq!(Phase::parse(7),Err(EPROTO));
        assert_eq!(Phase::parse(u64::MAX),Err(EPROTO));
    }
}

/// Typed initd -> loaderd request: verify the signed release bundle at `path`
/// (a sealed read-only file) and stage its digest with rootd. Returns the
/// verified release generation. Loaderd, not initd, holds the keyring and
/// decides; initd only names the bundle, service, epoch and deadline.
pub(super) fn stage_release_via_loaderd(
    path:&str,service:u64,epoch:u64,trial_deadline_ns:u64,durable_floor:u64,
)->Result<u64,i32>{
    let mut request=CommercialMaxProtocolRequest::default();
    if path.is_empty()||path.len()>request.path.len()||path.as_bytes().contains(&0){
        return Err(EINVAL)
    }
    let endpoint=super::lookup_service_endpoint(IPC_SERVICE_LOADERD)?;
    request.header.version=COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol=COMMERCIAL_MAX_PROTOCOL_LOADERD;
    request.header.op=COMMERCIAL_MAX_LOADERD_OP_STAGE_RELEASE;
    request.header.subject_pid=u64::from(std::process::id());
    request.header.subject_tid=super::current_tid();
    request.arg0=service;request.arg1=epoch;
    request.arg2=trial_deadline_ns;request.arg3=durable_floor;
    request.path_len=path.len() as u32;
    request.path[..path.len()].copy_from_slice(path.as_bytes());
    let mut response=CommercialMaxProtocolResponse::default();
    // SAFETY: both records are initialized, exact-sized ABI structs that stay
    // exclusively live for the whole bounded synchronous call.
    let got=unsafe{rustos_svc_runtime::syscall::syscall6(
        SYS_RUSTOS_IPC_CALL_BOUNDED,
        endpoint,
        (&request as *const CommercialMaxProtocolRequest) as u64,
        size_of::<CommercialMaxProtocolRequest>() as u64,
        (&mut response as *mut CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
        STAGE_RELEASE_CALL_TIMEOUT_MS,
    )};
    if got<0{return Err((-got) as i32)}
    if got as usize!=size_of::<CommercialMaxProtocolResponse>()
        ||!response.is_valid_envelope_for(&request)
        ||response.descriptor_count!=0
    {
        return Err(EPROTO)
    }
    if response.status!=0{return Err(response.status)}
    if response.value0!=epoch||response.value1==0{return Err(EPROTO)}
    Ok(response.value1)
}
/// Reading and hashing two images is bulk work, not a heartbeat.
const STAGE_RELEASE_CALL_TIMEOUT_MS:u64=15_000;
