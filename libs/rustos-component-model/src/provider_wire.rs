//! Versioned generic provider accounting wire (staging ABI).
//! The kernel-stamped sender is authoritative; none of the fields below
//! authenticate a PID or create a Job capability by themselves.
use alloc::vec::Vec;
pub const MAGIC:u32=0x5250_5332;
pub const VERSION:u16=1;
pub const BYTES:usize=112;
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
    Length,Magic,Version,Reserved,Phase,Invalid,Foreign,
    WrongRequest,Stale,Overflow,Allocation,
}
impl Frame{
    pub fn valid(self)->bool {
        self.request_id!=0&&self.provider_service!=0&&self.provider_pid!=0
          &&self.provider_lease!=0&&self.client_pid!=0
          &&self.session!=0&&self.token_epoch!=0&&self.token_nonce!=0
          && ((self.operation==Operation::Charge && self.phase==Phase::Request
                && self.client_job==0 && self.client_job_generation==0)
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
        if self.operation==Operation::Charge
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
pub fn encode(f:Frame)->Result<[u8;BYTES],Error>{
    if !f.valid(){return Err(Error::Invalid)}
    let mut out=[0u8;BYTES];
    out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    out[4..6].copy_from_slice(&VERSION.to_le_bytes());
    out[6..8].copy_from_slice(&(f.operation as u16).to_le_bytes());
    out[8..10].copy_from_slice(&(f.phase as u16).to_le_bytes());
    let fields=[
        f.request_id,f.provider_service,f.provider_pid,f.provider_lease,
        f.client_pid,f.client_job,f.session,f.token_epoch,f.token_nonce,
        f.kernel_receipt
    ];
    for (i,v) in fields.iter().enumerate(){
        let at=16+i*8;out[at..at+8].copy_from_slice(&v.to_le_bytes());
    }
    out[96..100].copy_from_slice(&f.client_job_generation.to_le_bytes());
    out[100..104].copy_from_slice(&f.status.to_le_bytes());
    Ok(out)
}
pub fn decode(bytes:&[u8])->Result<Frame,Error>{
    if bytes.len()!=BYTES{return Err(Error::Length)}
    let u16at=|at:usize|u16::from_le_bytes([bytes[at],bytes[at+1]]);
    let u32at=|at:usize|u32::from_le_bytes(bytes[at..at+4].try_into()
       .expect("checked frame length"));
    let u64at=|at:usize|u64::from_le_bytes(bytes[at..at+8].try_into()
       .expect("checked frame length"));
    if u32at(0)!=MAGIC{return Err(Error::Magic)}
    if u16at(4)!=VERSION{return Err(Error::Version)}
    if bytes[10..16].iter().any(|b|*b!=0)
       ||bytes[104..112].iter().any(|b|*b!=0){return Err(Error::Reserved)}
    let f=Frame{
        operation:Operation::parse(u16at(6)).ok_or(Error::Invalid)?,
        phase:Phase::parse(u16at(8)).ok_or(Error::Phase)?,
        request_id:u64at(16),provider_service:u64at(24),
        provider_pid:u64at(32),provider_lease:u64at(40),
        client_pid:u64at(48),client_job:u64at(56),
        session:u64at(64),token_epoch:u64at(72),
        token_nonce:u64at(80),kernel_receipt:u64at(88),
        client_job_generation:u32at(96),
        status:i32::from_le_bytes(bytes[100..104].try_into()
            .expect("checked frame length")),
    };
    if !f.valid(){return Err(Error::Invalid)}
    Ok(f)
}
pub fn encode_vec(f:Frame)->Result<Vec<u8>,Error>{
    let fixed=encode(f)?;
    let mut v=Vec::new();v.try_reserve_exact(BYTES)
        .map_err(|_|Error::Allocation)?;
    v.extend_from_slice(&fixed);Ok(v)
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn no_scalar_fallback_and_exact_reply() {
        let r=Frame{operation:Operation::Charge,phase:Phase::Request,
            request_id:7,provider_service:3,provider_pid:10,provider_lease:4,
            client_pid:20,client_job:0,client_job_generation:0,
            session:1,token_epoch:3,token_nonce:10,kernel_receipt:0,status:0};
        let bytes=encode(r).unwrap();
        assert_eq!(decode(&bytes),Ok(r));
        let mut bad=bytes;bad[105]=1;
        assert_eq!(decode(&bad),Err(Error::Reserved));
        let mut ack=r;ack.phase=Phase::Committed;ack.kernel_receipt=99;
        ack.client_job=0x100000009;ack.client_job_generation=9;
        assert_eq!(ack.validate_ack(r),Ok(()));
        ack.client_pid=21;
        assert_eq!(ack.validate_ack(r),Err(Error::WrongRequest));
    }
}
