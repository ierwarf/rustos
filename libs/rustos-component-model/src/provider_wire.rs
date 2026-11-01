//! Provider accounting wire: canonical generated IDL schema 520
//! (`provider_session_accounting`). The kernel-stamped sender is
//! authoritative; none of the fields below authenticate a PID or create a Job
//! capability by themselves. Unknown/duplicate/unordered/missing fields,
//! trailing bytes and a header request id that differs from the body fail
//! closed in the shared codec.
use crate::generated_idl::PROVIDER_SESSION_ACCOUNTING;
use crate::idl::{Builder,IdlError,Message};
use alloc::vec::Vec;
/// Exact canonical size of every frame (24-byte header + 14 fixed fields).
pub const BYTES:usize=24+10*12+8+20;
#[repr(u16)]
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Operation{Charge=1,Cancel=2,Release=3,Reconcile=4}
impl Operation {
    fn parse(n:u16)->Option<Self>{
        Some(match n{1=>Self::Charge,2=>Self::Cancel,3=>Self::Release,
            4=>Self::Reconcile,_=>return None})
    }
}
#[repr(u16)]
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Phase{Request=1,Accepted=2,Committed=3,NotFound=4}
impl Phase{
    fn parse(n:u16)->Option<Self>{
        Some(match n{1=>Self::Request,2=>Self::Accepted,
            3=>Self::Committed,4=>Self::NotFound,_=>return None})
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Frame {
    pub operation:Operation,pub phase:Phase,pub request_id:u64,
    pub provider_service:u64,pub provider_pid:u64,pub provider_lease:u64,
    pub client_pid:u64,pub client_job:u64,pub client_job_generation:u32,
    pub session:u64,pub token_epoch:u64,pub token_nonce:u64,
    pub kernel_receipt:u64,pub status:i32,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error{
    Idl(IdlError),Phase,Invalid,Foreign,
    WrongRequest,Stale,Overflow,Allocation,
}
impl From<IdlError> for Error{
    fn from(e:IdlError)->Self{Self::Idl(e)}
}
impl Frame{
    pub fn valid(self)->bool {
        self.request_id!=0&&self.provider_service!=0&&self.provider_pid!=0
          &&self.provider_lease!=0&&self.client_pid!=0
          &&self.session!=0&&self.token_epoch!=0&&self.token_nonce!=0
          && (((matches!(self.operation,Operation::Charge|Operation::Cancel)
                && self.client_job==0 && self.client_job_generation==0))
              || (self.client_job!=0 && self.client_job_generation!=0))
    }
    /// Only a server that already authenticated exact caller PID and live
    /// lease may use this validation. The wire is not self-authenticating.
    pub fn validate_request(self,actual_provider_pid:u64,
                            live_lease:u64)->Result<(),Error>{
        if !self.valid(){return Err(Error::Invalid)}
        if self.provider_pid!=actual_provider_pid
          ||self.provider_lease!=live_lease {return Err(Error::Foreign)}
        if self.phase!=Phase::Request||self.kernel_receipt!=0||self.status!=0 {
            return Err(Error::Phase)
        }
        if matches!(self.operation,Operation::Charge|Operation::Cancel)
            && (self.client_job!=0||self.client_job_generation!=0){
            return Err(Error::Invalid)
        }
        Ok(())
    }
    pub fn validate_ack(self,request:Frame)->Result<(),Error>{
        if !self.valid(){return Err(Error::Invalid)}
        if self.operation!=request.operation
          ||self.request_id!=request.request_id
          ||self.provider_service!=request.provider_service
          ||self.provider_pid!=request.provider_pid
          ||self.provider_lease!=request.provider_lease
          ||self.client_pid!=request.client_pid
          ||(request.operation!=Operation::Charge
             && (self.client_job!=request.client_job
                 ||self.client_job_generation!=request.client_job_generation))
          ||self.session!=request.session
          ||self.token_epoch!=request.token_epoch
          ||self.token_nonce!=request.token_nonce{return Err(Error::WrongRequest)}
        if request.operation==Operation::Charge
           && (self.client_job==0 || self.client_job_generation==0){
            return Err(Error::Invalid)
        }
        if !matches!(self.phase,Phase::Accepted|Phase::Committed|Phase::NotFound){
            return Err(Error::Phase)
        }
        if self.phase==Phase::Committed
            && self.operation==Operation::Charge && self.kernel_receipt==0 {
            return Err(Error::Invalid)
        }
        Ok(())
    }
}
pub fn encode(f:Frame)->Result<Vec<u8>,Error>{
    if !f.valid(){return Err(Error::Invalid)}
    let mut b=Builder::new(PROVIDER_SESSION_ACCOUNTING)?;
    b.field(1,&f.request_id.to_le_bytes())?;
    b.field(2,&f.provider_service.to_le_bytes())?;
    b.field(3,&f.provider_pid.to_le_bytes())?;
    b.field(4,&f.provider_lease.to_le_bytes())?;
    b.field(5,&f.client_pid.to_le_bytes())?;
    b.field(6,&f.client_job.to_le_bytes())?;
    b.field(7,&f.client_job_generation.to_le_bytes())?;
    b.field(8,&f.session.to_le_bytes())?;
    b.field(9,&f.token_epoch.to_le_bytes())?;
    b.field(10,&f.token_nonce.to_le_bytes())?;
    b.field(11,&f.kernel_receipt.to_le_bytes())?;
    b.field(12,&(f.operation as u16).to_le_bytes())?;
    b.field(13,&(f.phase as u16).to_le_bytes())?;
    b.field(14,&(f.status as u32).to_le_bytes())?;
    Ok(b.finish(f.request_id)?)
}
pub fn decode(bytes:&[u8])->Result<Frame,Error>{
    let m=Message::decode(PROVIDER_SESSION_ACCOUNTING,bytes)?;
    let u64f=|tag:u16|->Result<u64,Error>{
        Ok(m.field(tag).ok_or(IdlError::Missing)?.as_u64()?)
    };
    let request_id=u64f(1)?;
    if m.header().request_id!=request_id{return Err(Error::Invalid)}
    let f=Frame{
        operation:Operation::parse(m.field(12).ok_or(IdlError::Missing)?
            .as_u16()?).ok_or(Error::Invalid)?,
        phase:Phase::parse(m.field(13).ok_or(IdlError::Missing)?
            .as_u16()?).ok_or(Error::Phase)?,
        request_id,provider_service:u64f(2)?,
        provider_pid:u64f(3)?,provider_lease:u64f(4)?,
        client_pid:u64f(5)?,client_job:u64f(6)?,
        client_job_generation:m.field(7).ok_or(IdlError::Missing)?.as_u32()?,
        session:u64f(8)?,token_epoch:u64f(9)?,token_nonce:u64f(10)?,
        kernel_receipt:u64f(11)?,
        status:m.field(14).ok_or(IdlError::Missing)?.as_u32()? as i32,
    };
    if !f.valid(){return Err(Error::Invalid)}
    Ok(f)
}
#[cfg(test)]
mod tests{
    use super::*;
    fn request()->Frame{
        Frame{operation:Operation::Charge,phase:Phase::Request,
            request_id:7,provider_service:3,provider_pid:10,provider_lease:4,
            client_pid:20,client_job:0,client_job_generation:0,
            session:1,token_epoch:3,token_nonce:10,kernel_receipt:0,status:0}
    }
    #[test]
    fn canonical_idl_roundtrip_has_exact_size_and_exact_ack(){
        let r=request();
        let bytes=encode(r).unwrap();
        assert_eq!(bytes.len(),BYTES);
        assert_eq!(decode(&bytes),Ok(r));
        let mut ack=r;ack.phase=Phase::Committed;ack.kernel_receipt=99;
        ack.client_job=0x100000009;ack.client_job_generation=9;
        assert_eq!(decode(&encode(ack).unwrap()),Ok(ack));
        assert_eq!(ack.validate_ack(r),Ok(()));
        ack.client_pid=21;
        assert_eq!(ack.validate_ack(r),Err(Error::WrongRequest));
    }
    #[test]
    fn negative_status_and_every_malformed_shape_fail_closed(){
        let mut r=request();
        r.phase=Phase::NotFound;r.status=-116;
        assert_eq!(decode(&encode(r).unwrap()),Ok(r));
        let good=encode(request()).unwrap();
        // Trailing byte, truncation and a corrupt magic never decode.
        let mut longer=good.clone();longer.push(0);
        assert!(decode(&longer).is_err());
        assert!(decode(&good[..good.len()-1]).is_err());
        let mut bad=good.clone();bad[0]^=1;
        assert!(decode(&bad).is_err());
        // The header request id must equal the body request id.
        let mut skew=good.clone();skew[16]^=1;
        assert_eq!(decode(&skew),Err(Error::Invalid));
        // Opcode of another schema (514..519) is not accepted.
        let mut other=good.clone();other[6]=0x02;other[7]=0x02;
        assert!(decode(&other).is_err());
        // Unknown operation/phase values in an otherwise canonical body.
        let mut bad_op=good.clone();
        let at=good.len()-(4+2)-(4+2)-(4+4)+4; // field 12 value
        bad_op[at]=0xff;
        assert!(decode(&bad_op).is_err());
        // A charge request may never select its own Job.
        let mut selecting=request();selecting.client_job=0x100000001;
        selecting.client_job_generation=1;
        assert!(selecting.validate_request(10,4).is_err());
    }
    #[test]
    fn legacy_fixed_frames_are_not_accepted(){
        let mut legacy=[0u8;112];
        legacy[0..4].copy_from_slice(&0x5250_5332u32.to_le_bytes());
        legacy[4..6].copy_from_slice(&1u16.to_le_bytes());
        assert!(decode(&legacy).is_err());
    }
}
