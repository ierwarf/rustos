//! Periodic, exact-incarnation Ring3 -> rootd health control.
//! This is NEVER called by kernel IPC, scheduler, futex, fault, time or TLB
//! mechanisms. Services own the scheduling cadence and must not hold a
//! state mutex across this bounded supervisor RPC.
//! A lost ACK repeats the exact same sequence. Rootd accepts that replay
//! without renewing health or counting a second candidate health pulse.
use core::mem::size_of;
use rustos_component_model::health_wire::{self,Pulse};
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest,CommercialMaxProtocolResponse,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_HEARTBEAT,IPC_SERVICE_ROOTD,
    SYS_RUSTOS_IPC_CALL_BOUNDED,
};
use crate::{ipc,syscall};
const SYS_GETPID:u64=39;
const SYS_GETTID:u64=186;
const EINVAL:i32=22;
const EAGAIN:i32=11;
const EPROTO:i32=71;
pub const INTERVAL_NS:u64=2_000_000_000;
pub const CALL_LIMIT_MS:u64=250;

pub struct Heartbeat {
    service:u64,sequence:u64,next_due_ns:u64,
    failures:u32,last_deadline_ns:u64,
}
impl Heartbeat {
    pub const fn new(service:u64)->Self{
        Self{service,sequence:0,next_due_ns:0,failures:0,last_deadline_ns:0}
    }
    pub fn service(&self)->u64{self.service}
    pub fn failures(&self)->u32{self.failures}
    pub fn last_deadline_ns(&self)->u64{self.last_deadline_ns}
    pub fn next_due_ns(&self)->u64{self.next_due_ns}
    pub fn poll(&mut self,now_ns:u64)->Result<bool,i32>{
        if now_ns==0||self.service==0{return Err(EINVAL)}
        if now_ns<self.next_due_ns{return Ok(false)}
        let seq=self.sequence.checked_add(1).ok_or(75)?;
        // A failed ACK intentionally leaves sequence unchanged: the next
        // attempt must not invent a fresh witness for a lost reply.
        self.next_due_ns=now_ns.saturating_add(INTERVAL_NS);
        match self.send(seq){
            Ok(deadline)=>{
                if deadline<=now_ns {
                    self.failures=self.failures.saturating_add(1);
                    return Err(EPROTO)
                }
                self.sequence=seq;
                self.last_deadline_ns=deadline;
                self.failures=0;
                Ok(true)
            }
            Err(error)=>{
                self.failures=self.failures.saturating_add(1);
                Err(error)
            }
        }
    }
    pub fn send(&self,sequence:u64)->Result<u64,i32>{
        let endpoint=ipc::lookup_service_endpoint(IPC_SERVICE_ROOTD);
        if endpoint<=0{return Err(EAGAIN)}
        let pid=unsafe{syscall::syscall0(SYS_GETPID)};
        let tid=unsafe{syscall::syscall0(SYS_GETTID)};
        if pid<=0||tid<=0{return Err(EINVAL)}
        let pulse=Pulse{
            service:self.service,incarnation_pid:pid as u64,sequence,
        };
        let wire=health_wire::encode_pulse(pulse).map_err(|_|EINVAL)?;
        let mut request=CommercialMaxProtocolRequest::default();
        if wire.len()>request.payload.len(){return Err(EINVAL)}
        request.header.version=COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
        request.header.protocol=COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
        request.header.op=COMMERCIAL_MAX_ROOTD_OP_SERVICE_HEARTBEAT;
        request.header.subject_pid=pid as u64;
        request.header.subject_tid=tid as u64;
        request.payload_len=wire.len() as u32;
        request.payload[..wire.len()].copy_from_slice(&wire);
        let mut response=CommercialMaxProtocolResponse::default();
        let got=unsafe{syscall::syscall6(
            SYS_RUSTOS_IPC_CALL_BOUNDED,endpoint as u64,
            (&request as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut response as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
            CALL_LIMIT_MS,
        )};
        if got<0{return Err((-got) as i32)}
        if got as usize!=size_of::<CommercialMaxProtocolResponse>()
            ||!response.is_valid_envelope_for(&request)
            ||response.descriptor_count!=0
            ||response.value0!=0
            ||response.value1==0
        {
            return Err(EPROTO)
        }
        if response.status!=0{return Err(response.status)}
        health_wire::decode_accept(
            &response.payload[..response.payload_len as usize],pulse,
        ).map_err(|_|EPROTO)?;
        Ok(response.value1)
    }
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn default_interval_is_slow_and_timeout_below_interval(){
        let h=Heartbeat::new(3);
        assert_eq!(h.sequence,0);
        assert_eq!(h.failures(),0);
        assert!(CALL_LIMIT_MS*1_000_000<INTERVAL_NS);
    }
}
