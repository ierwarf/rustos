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
    COMMERCIAL_MAX_ROOTD_OP_UPDATE_INSPECT,
    IPC_SERVICE_ROOTD,SYS_RUSTOS_IPC_CALL_BOUNDED,
};
const EINVAL:i32=22;const EPROTO:i32=71;
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub(super) enum Phase{
    Empty,Staged,Armed,Trial,Committed,RollbackRequired,
}
impl Phase{
    fn parse(value:u64)->Result<Self,i32>{
        Ok(match value{
            0=>Self::Empty,1=>Self::Staged,2=>Self::Armed,
            3=>Self::Trial,4=>Self::Committed,5=>Self::RollbackRequired,
            _=>return Err(EPROTO)
        })
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub(super) struct Trial{
    pub phase:Phase,
    pub epoch:u64,
    pub previous_pid:u64,
    pub candidate_pid:u64,
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
pub(super) fn arm(approval:Approval)->Result<(),i32>{
    apply(COMMERCIAL_MAX_ROOTD_OP_UPDATE_ARM,approval,Phase::Armed).map(|_|())
}
pub(super) fn require_rollback(approval:Approval)->Result<(),i32>{
    apply(COMMERCIAL_MAX_ROOTD_OP_UPDATE_ROLLBACK,approval,
        Phase::RollbackRequired).map(|_|())
}
/// A committed three-pulse trial is necessary but insufficient. The caller
/// must first arrange signed candidate pinning and verified previous-image
/// release in storaged. This client does not synthesize that evidence.
pub(super) fn seal(approval:Approval)->Result<(),i32>{
    apply(COMMERCIAL_MAX_ROOTD_OP_UPDATE_SEAL,approval,
        Phase::Committed).map(|_|())
}
pub(super) fn inspect(service:u64)->Result<Trial,i32>{
    if service==0{return Err(EINVAL)}
    let mut request=CommercialMaxProtocolRequest::default();
    request.header.op=COMMERCIAL_MAX_ROOTD_OP_UPDATE_INSPECT;
    request.arg0=service;
    let response=transact(request)?;
    if response.payload_len!=16{return Err(EPROTO)}
    let prev=u64::from_le_bytes(response.payload[..8].try_into().map_err(|_|EPROTO)?);
    let candidate=u64::from_le_bytes(response.payload[8..16].try_into().map_err(|_|EPROTO)?);
    Ok(Trial{phase:Phase::parse(response.value1)?,
        epoch:response.value0,previous_pid:prev,candidate_pid:candidate})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn update_phase_decoding_fails_closed(){
        assert_eq!(Phase::parse(5),Ok(Phase::RollbackRequired));
        assert_eq!(Phase::parse(6),Err(EPROTO));
        assert_eq!(Phase::parse(u64::MAX),Err(EPROTO));
    }
}
