//! Durable storage orchestration interface for the staged update journal.
//!
//! The underlying storaged/vfsd implementation must provide immutable record
//! ordering, stable-media sync, readback and exclusive rootd ownership. This
//! module never claims that an in-memory kernel journal survives power loss.
use crate::durable_recovery::{self,Cursor,Record,RECORD_BYTES};
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum IoError {
    Unavailable,ShortWrite,Readback,SyncFailed,Full,Corrupt,
    Sequence,WrongOwner,Conflict,Overflow,
}
pub trait JournalDevice {
    /// True only when a complete record exists at exactly this index.
    /// An unclean/torn final record must return an error, not false.
    fn read(&mut self,index:u64,out:&mut [u8;RECORD_BYTES])
        ->Result<bool,IoError>;
    /// Append exactly at the next index, never overwrite earlier records.
    fn append(&mut self,index:u64,bytes:&[u8;RECORD_BYTES])
        ->Result<(),IoError>;
    /// Returns only after all preceding appends reached stable media.
    fn sync(&mut self)->Result<(),IoError>;
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct JournalOwner {
    pub rootd_pid:u64,pub lease_generation:u64,pub boot_epoch:u64,
}
impl JournalOwner{
    pub fn valid(self)->bool{
        self.rootd_pid!=0&&self.lease_generation!=0&&self.boot_epoch!=0
    }
}
pub struct DurableJournal {
    owner:JournalOwner,cursor:Cursor,
    loaded:bool,fail_closed:bool,
}
impl DurableJournal {
    pub const fn new(owner:JournalOwner)->Self {
        Self{owner,cursor:Cursor::new(),loaded:false,fail_closed:false}
    }
    pub fn cursor(&self)->Cursor{self.cursor}
    pub fn failed(&self)->bool{self.fail_closed}
    /// Replaying every sequence into a fresh state machine establishes
    /// custody of prior ACKs. Gaps and malformed records stop recovery.
    pub fn load(&mut self,device:&mut impl JournalDevice,
                actual_owner:JournalOwner,max_records:u64)
        ->Result<Cursor,IoError>{
        if !self.owner.valid()||self.owner!=actual_owner||
           self.loaded || self.fail_closed {return Err(IoError::WrongOwner)}
        let mut cursor=Cursor::new();
        for index in 0..max_records {
            let mut record=[0u8;RECORD_BYTES];
            match device.read(index,&mut record) {
                Ok(false)=>{
                    self.cursor=cursor;self.loaded=true;return Ok(cursor)
                }
                Ok(true)=>{
                    let parsed=durable_recovery::decode(&record)
                        .map_err(|_|IoError::Corrupt)?;
                    cursor=cursor.admit(parsed).map_err(|_|IoError::Sequence)?;
                }
                Err(error)=>{
                    self.fail_closed=true;return Err(error)
                }
            }
        }
        // A full bounded journal may have more data that the caller omitted.
        // Require an exact EOF read before accepting a replay.
        let mut excess=[0u8;RECORD_BYTES];
        match device.read(max_records,&mut excess) {
            Ok(false)=>{
                self.cursor=cursor;self.loaded=true;Ok(cursor)
            }
            Ok(true)=>{
                self.fail_closed=true;Err(IoError::Full)
            }
            Err(error)=>{
                self.fail_closed=true;Err(error)
            }
        }
    }
    /// Append -> sync -> exact readback -> publish new cursor.
    /// A lost ACK after sync is resolved by rereading the record, NEVER by
    /// issuing a different transaction sequence.
    pub fn append_verified(&mut self,device:&mut impl JournalDevice,
         actual_owner:JournalOwner,record:Record)
        ->Result<Cursor,IoError>{
        if !self.loaded || self.fail_closed ||
            self.owner!=actual_owner || !actual_owner.valid() {
            return Err(IoError::WrongOwner)
        }
        let next=self.cursor.admit(record).map_err(|_|IoError::Sequence)?;
        let bytes=durable_recovery::encode(record).map_err(|_|IoError::Corrupt)?;
        let offset=self.cursor.last_sequence;
        // Before any append, an unknown state at the expected index must not
        // be overwritten. If a previous sync ACK was lost, caller reconciles.
        let mut observed=[0u8;RECORD_BYTES];
        match device.read(offset,&mut observed) {
            Ok(true)=>return Err(IoError::Conflict),
            Ok(false)=>{}
            Err(e)=>{self.fail_closed=true;return Err(e)}
        }
        if let Err(e)=device.append(offset,&bytes) {
            self.fail_closed=true;return Err(e)
        }
        if let Err(e)=device.sync() {
            self.fail_closed=true;return Err(e)
        }
        match device.read(offset,&mut observed) {
            Ok(true) if observed==bytes => {
                self.cursor=next;Ok(next)
            }
            Ok(_)=>{
                self.fail_closed=true;Err(IoError::Readback)
            }
            Err(e)=>{
                self.fail_closed=true;Err(e)
            }
        }
    }
    /// The same process can only rotate a live owner after external proof
    /// that all unresolved append results are reconciled.
    pub fn transfer_owner(&mut self,next:JournalOwner,
                          old_revoked:bool)->Result<(),IoError>{
        if !self.loaded||self.fail_closed||!next.valid()||!old_revoked
            ||next.boot_epoch<=self.owner.boot_epoch {
            return Err(IoError::WrongOwner)
        }
        self.owner=next;Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    struct Memory(Vec<[u8;RECORD_BYTES]>);
    impl JournalDevice for Memory {
        fn read(&mut self,i:u64,out:&mut [u8;RECORD_BYTES])
            ->Result<bool,IoError>{
            match self.0.get(i as usize){
                Some(record)=>{*out=*record;Ok(true)}
                None=>Ok(false)
            }
        }
        fn append(&mut self,i:u64,b:&[u8;RECORD_BYTES])->Result<(),IoError>{
            if i as usize!=self.0.len(){return Err(IoError::Conflict)}
            self.0.push(*b);Ok(())
        }
        fn sync(&mut self)->Result<(),IoError>{Ok(())}
    }
    #[test]
    fn one_ack_corresponds_to_one_flushed_record() {
        let owner=JournalOwner{rootd_pid:1,lease_generation:2,boot_epoch:3};
        let mut j=DurableJournal::new(owner);
        let mut mem=Memory(Vec::new());
        j.load(&mut mem,owner,4).unwrap();
        let r=Record{sequence:1,transaction:1,service:10,
            kind:durable_recovery::Kind::Begin,
            generation:1,payload_hash:[7;32]};
        j.append_verified(&mut mem,owner,r).unwrap();
        assert_eq!(mem.0.len(),1);
        assert_eq!(j.append_verified(&mut mem,owner,r),Err(IoError::Sequence));
    }
}
