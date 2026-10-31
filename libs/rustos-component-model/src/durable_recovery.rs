//! Ring3 crash-replay record format and torn-write detection.
//!
//! This format is NOT a storage driver, cryptographic signature, fsync, or
//! power-loss warranty. Durable acceptance requires the storage owner to
//! report an actual flushed transaction barrier on stable media.
use alloc::vec::Vec;
pub const MAGIC:u32=0x5244_4a31;
pub const RECORD_BYTES:usize=96;
pub const CURRENT_VERSION:u16=1;
#[repr(u16)]
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Kind {
    Begin=1,OldImagePinned=2,CandidateVerified=3,
    Quiesced=4,Activated=5,HealthAccepted=6,
    Commit=7,RollbackRequired=8,RestoreBegin=9,RestoreComplete=10,
}
impl Kind {
    fn from_raw(n:u16)->Option<Self>{
        Some(match n {
            1=>Self::Begin,2=>Self::OldImagePinned,3=>Self::CandidateVerified,
            4=>Self::Quiesced,5=>Self::Activated,6=>Self::HealthAccepted,
            7=>Self::Commit,8=>Self::RollbackRequired,9=>Self::RestoreBegin,
            10=>Self::RestoreComplete,_=>return None
        })
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error {
    Size,Magic,Version,Reserved,Checksum,
    Sequence,Transaction,Service,Order,Overflow,Allocation,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Record {
    pub sequence:u64,
    pub transaction:u64,
    pub service:u64,
    pub kind:Kind,
    pub generation:u64,
    pub payload_hash:[u8;32],
}
impl Record {
    pub fn valid(self)->bool {
        self.sequence!=0 && self.transaction!=0 &&
        self.service!=0 && self.generation!=0
    }
}
/// CRC32C only detects accidental corruption. A malicious actor can forge it;
/// signed image validation and permission checks happen elsewhere.
fn crc32c(bytes:&[u8])->u32 {
    let mut crc=!0u32;
    for &b in bytes {
        crc^=b as u32;
        for _ in 0..8 {
            crc=(crc>>1)^((0u32.wrapping_sub(crc&1))&0x82f6_3b78);
        }
    }
    !crc
}
pub fn encode(r:Record)->Result<[u8;RECORD_BYTES],Error>{
    if !r.valid(){return Err(Error::Transaction)}
    let mut b=[0u8;RECORD_BYTES];
    b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    b[4..6].copy_from_slice(&CURRENT_VERSION.to_le_bytes());
    b[6..8].copy_from_slice(&(r.kind as u16).to_le_bytes());
    b[8..16].copy_from_slice(&r.sequence.to_le_bytes());
    b[16..24].copy_from_slice(&r.transaction.to_le_bytes());
    b[24..32].copy_from_slice(&r.service.to_le_bytes());
    b[32..40].copy_from_slice(&r.generation.to_le_bytes());
    b[40..72].copy_from_slice(&r.payload_hash);
    let crc=crc32c(&b[..RECORD_BYTES-4]);
    b[RECORD_BYTES-4..].copy_from_slice(&crc.to_le_bytes());
    Ok(b)
}
pub fn decode(b:&[u8])->Result<Record,Error>{
    if b.len()!=RECORD_BYTES{return Err(Error::Size)}
    if u32::from_le_bytes(b[0..4].try_into().map_err(|_|Error::Size)?)!=MAGIC {
        return Err(Error::Magic)
    }
    if u16::from_le_bytes([b[4],b[5]])!=CURRENT_VERSION{
        return Err(Error::Version)
    }
    if b[72..92].iter().any(|v|*v!=0){return Err(Error::Reserved)}
    let got=u32::from_le_bytes(b[92..96].try_into().map_err(|_|Error::Size)?);
    if got!=crc32c(&b[..92]){return Err(Error::Checksum)}
    let kind=Kind::from_raw(u16::from_le_bytes([b[6],b[7]]))
        .ok_or(Error::Version)?;
    let u64at=|at:usize|{
        u64::from_le_bytes(b[at..at+8].try_into().expect("checked record size"))
    };
    let mut digest=[0u8;32];digest.copy_from_slice(&b[40..72]);
    let r=Record{kind,sequence:u64at(8),transaction:u64at(16),
        service:u64at(24),generation:u64at(32),payload_hash:digest};
    if !r.valid(){return Err(Error::Transaction)}
    Ok(r)
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Cursor {
    pub last_sequence:u64,pub active_transaction:u64,pub active_service:u64,
    pub generation:u64,pub last_kind:Option<Kind>,pub recovery_required:bool,
}
impl Cursor {
    pub const fn new()->Self{
        Self{last_sequence:0,active_transaction:0,active_service:0,
            generation:0,last_kind:None,recovery_required:false}
    }
    pub fn admit(&self,r:Record)->Result<Self,Error> {
        if !r.valid() || r.sequence!=self.last_sequence.checked_add(1)
            .ok_or(Error::Overflow)? {return Err(Error::Sequence)}
        let mut next=*self;
        if r.kind==Kind::Begin {
            if self.active_transaction!=0
                && !matches!(self.last_kind,Some(Kind::Commit)|Some(Kind::RestoreComplete)) {
                return Err(Error::Order)
            }
            if r.transaction<=self.active_transaction || r.generation<=self.generation {
                return Err(Error::Transaction)
            }
            next.active_transaction=r.transaction;
            next.active_service=r.service;next.generation=r.generation;
        } else {
            if r.transaction!=self.active_transaction ||
              r.service!=self.active_service ||
              r.generation!=self.generation{return Err(Error::Transaction)}
            let legal=matches!((self.last_kind,r.kind),
                (Some(Kind::Begin),Kind::OldImagePinned)|
                (Some(Kind::OldImagePinned),Kind::CandidateVerified)|
                (Some(Kind::CandidateVerified),Kind::Quiesced)|
                (Some(Kind::Quiesced),Kind::Activated)|
                (Some(Kind::Activated),Kind::HealthAccepted)|
                (Some(Kind::HealthAccepted),Kind::Commit)|
                (Some(Kind::RollbackRequired),Kind::RestoreBegin)|
                (Some(Kind::RestoreBegin),Kind::RestoreComplete)
            );
            let rollback=matches!(r.kind,Kind::RollbackRequired)
                && !matches!(self.last_kind,Some(Kind::Commit)|Some(Kind::RestoreComplete));
            if !legal&&!rollback{return Err(Error::Order)}
        }
        if r.kind==Kind::RollbackRequired {next.recovery_required=true}
        if matches!(r.kind,Kind::RestoreComplete|Kind::Commit) {
            next.recovery_required=false;
        }
        next.last_sequence=r.sequence;
        next.last_kind=Some(r.kind);
        Ok(next)
    }
}
/// Replay refuses partial records, corrupt checksums and any skipped sequence.
/// The caller decides whether to truncate an *unacknowledged* torn tail after
/// independent storage readback; this function never guesses commit success.
pub fn replay(bytes:&[u8])->Result<Cursor,Error>{
    if bytes.len()%RECORD_BYTES!=0{return Err(Error::Size)}
    let mut cursor=Cursor::new();
    for segment in bytes.chunks_exact(RECORD_BYTES){
        cursor=cursor.admit(decode(segment)?)?;
    }
    Ok(cursor)
}
pub fn append_preview(cursor:Cursor,r:Record)->Result<(Cursor,Vec<u8>),Error>{
    let next=cursor.admit(r)?;
    let mut bytes=Vec::new();
    bytes.try_reserve_exact(RECORD_BYTES).map_err(|_|Error::Allocation)?;
    bytes.extend_from_slice(&encode(r)?);
    Ok((next,bytes))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_corruption_and_missing_order() {
        let r=Record{sequence:1,transaction:1,service:7,kind:Kind::Begin,
            generation:1,payload_hash:[8;32]};
        let bytes=encode(r).unwrap();
        assert_eq!(replay(&bytes).unwrap().last_sequence,1);
        let mut bad=bytes;bad[22]^=1;
        assert_eq!(decode(&bad),Err(Error::Checksum));
        let mut next=r;next.sequence=2;next.kind=Kind::Commit;
        assert_eq!(Cursor::new().admit(r).unwrap().admit(next),Err(Error::Order));
    }
}
