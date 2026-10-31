//! Ring3 image custody admission model.
//!
//! A digest, path or boolean supplied by a candidate is not a signature.
//! The verifier and storage service MUST independently authenticate bytes,
//! signing key, release manifest, rollback floor and durable old-image pin.
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Image {
    pub service:u64,pub generation:u64,pub length:u64,
    pub digest:[u8;32],pub signer:[u8;32],pub manifest_hash:[u8;32],
}
impl Image{
    pub fn valid(self)->bool{
        self.service!=0&&self.generation!=0&&self.length!=0
           &&self.digest.iter().any(|x|*x!=0)
           &&self.signer.iter().any(|x|*x!=0)
           &&self.manifest_hash.iter().any(|x|*x!=0)
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Verification {
    pub image:Image,pub verifier_pid:u64,pub verifier_lease:u64,
    pub evidence_nonce:u64,pub signature_verified:bool,
    pub manifest_verified:bool,pub bytes_sealed:bool,
}
impl Verification{
    pub fn valid(self)->bool{
        self.image.valid()&&self.verifier_pid!=0
        &&self.verifier_lease!=0&&self.evidence_nonce!=0
        &&self.signature_verified&&self.manifest_verified&&self.bytes_sealed
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct StoragePin {
    pub image:Image,pub storage_lease:u64,pub immutable_id:u64,
    pub durable_sequence:u64,pub readback_verified:bool,
}
impl StoragePin{
    pub fn valid(self)->bool{
        self.image.valid()&&self.storage_lease!=0
        &&self.immutable_id!=0&&self.durable_sequence!=0
        &&self.readback_verified
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Phase{Empty,PreviousPinned,CandidateAdmitted,
    Trial,Committed,RollbackRequired,Restored}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error{
    Invalid,Unknown,Capacity,WrongImage,WrongEvidence,
    AntiRollback,Busy,WrongPhase,Unpinned,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
struct Slot {
    service:u64,previous:Image,previous_pin:StoragePin,
    candidate:Option<Image>,phase:Phase,release_epoch:u64,
}
pub struct Custody<const N:usize> {
    slots:[Option<Slot>;N],
    floors:[(u64,u64);N],
}
impl<const N:usize> Custody<N>{
    pub const fn new()->Self{
        Self{slots:[None;N],floors:[(0,0);N]}
    }
    fn position(&self,service:u64)->Result<usize,Error>{
        self.slots.iter().position(|x|x.is_some_and(|s|s.service==service))
            .ok_or(Error::Unknown)
    }
    fn floor(&self,service:u64)->u64{
        self.floors.iter().find(|(s,_)|*s==service).map_or(0,|(_,g)|*g)
    }
    pub fn set_verified_floor(&mut self,service:u64,generation:u64,
                              durable:bool)->Result<(),Error>{
        if !durable||service==0||generation==0{return Err(Error::Invalid)}
        let old=self.floor(service);
        if generation<old{return Err(Error::AntiRollback)}
        if let Some((_,gen))=self.floors.iter_mut().find(|(s,_)|*s==service){
            *gen=generation;return Ok(())
        }
        let slot=self.floors.iter_mut().find(|(s,_)|*s==0).ok_or(Error::Capacity)?;
        *slot=(service,generation);Ok(())
    }
    pub fn begin(&mut self,previous:Verification,pin:StoragePin,
                 release_epoch:u64,verifier_authenticated:bool,
                 storage_authenticated:bool)->Result<(),Error> {
        if !verifier_authenticated||!storage_authenticated
           || !previous.valid()||!pin.valid()||previous.image!=pin.image
           || release_epoch==0 {return Err(Error::WrongEvidence)}
        if previous.image.generation<self.floor(previous.image.service){
            return Err(Error::AntiRollback)
        }
        if self.position(previous.image.service).is_ok(){return Err(Error::Busy)}
        let i=self.slots.iter().position(Option::is_none).ok_or(Error::Capacity)?;
        self.slots[i]=Some(Slot{service:previous.image.service,
            previous:previous.image,previous_pin:pin,
            candidate:None,phase:Phase::PreviousPinned,release_epoch});
        Ok(())
    }
    pub fn admit_candidate(&mut self,service:u64,evidence:Verification,
                           release_epoch:u64,verifier_authenticated:bool)
       ->Result<(),Error>{
        let i=self.position(service)?;
        let s=self.slots[i].as_mut().ok_or(Error::Unknown)?;
        if !verifier_authenticated||!evidence.valid(){return Err(Error::WrongEvidence)}
        if s.release_epoch!=release_epoch ||
            evidence.image.service!=service{return Err(Error::WrongImage)}
        if s.phase!=Phase::PreviousPinned{return Err(Error::WrongPhase)}
        if evidence.image.generation<=s.previous.generation ||
            evidence.image.generation<self.floor(service){return Err(Error::AntiRollback)}
        if evidence.image==s.previous{return Err(Error::WrongImage)}
        s.candidate=Some(evidence.image);
        s.phase=Phase::CandidateAdmitted;Ok(())
    }
    pub fn begin_trial(&mut self,service:u64,release_epoch:u64,
                       activated_image:Image,kernel_lease_verified:bool)
        ->Result<(),Error>{
        let i=self.position(service)?;
        let s=self.slots[i].as_mut().ok_or(Error::Unknown)?;
        if s.release_epoch!=release_epoch||!kernel_lease_verified
           ||s.candidate!=Some(activated_image){return Err(Error::WrongEvidence)}
        if s.phase!=Phase::CandidateAdmitted{return Err(Error::WrongPhase)}
        s.phase=Phase::Trial;Ok(())
    }
    pub fn require_rollback(&mut self,service:u64,release_epoch:u64)
         ->Result<(),Error>{
        let i=self.position(service)?;
        let s=self.slots[i].as_mut().ok_or(Error::Unknown)?;
        if s.release_epoch!=release_epoch{return Err(Error::WrongEvidence)}
        if !matches!(s.phase,Phase::CandidateAdmitted|Phase::Trial|
            Phase::RollbackRequired){return Err(Error::WrongPhase)}
        s.phase=Phase::RollbackRequired;Ok(())
    }
    /// Do not mark rollback complete until the exact previously pinned
    /// executable has been restored, verified and durably committed.
    pub fn restored(&mut self,service:u64,release_epoch:u64,
                    restored_pin:StoragePin,verified:bool,durable:bool)
        ->Result<(),Error>{
        let i=self.position(service)?;
        let s=self.slots[i].as_mut().ok_or(Error::Unknown)?;
        if s.release_epoch!=release_epoch||s.previous_pin!=restored_pin
            ||!verified{return Err(Error::WrongEvidence)}
        if s.phase!=Phase::RollbackRequired{return Err(Error::WrongPhase)}
        if !durable{return Err(Error::Unpinned)}
        s.phase=Phase::Restored;Ok(())
    }
    pub fn committed(&mut self,service:u64,release_epoch:u64,
                     new_pin:StoragePin,health_authenticated:bool,
                     durable:bool)->Result<(),Error>{
        let i=self.position(service)?;
        let s=self.slots[i].as_mut().ok_or(Error::Unknown)?;
        if s.release_epoch!=release_epoch || !health_authenticated
            ||s.candidate!=Some(new_pin.image)||!new_pin.valid() {
            return Err(Error::WrongEvidence)
        }
        if s.phase!=Phase::Trial{return Err(Error::WrongPhase)}
        if !durable{return Err(Error::Unpinned)}
        s.phase=Phase::Committed;Ok(())
    }
    pub fn retire(&mut self,service:u64,release_epoch:u64,
                  old_handle_released:bool)->Result<(),Error>{
        let i=self.position(service)?;
        let s=self.slots[i].ok_or(Error::Unknown)?;
        if s.release_epoch!=release_epoch{return Err(Error::WrongEvidence)}
        if !matches!(s.phase,Phase::Committed|Phase::Restored)
            ||!old_handle_released{return Err(Error::Unpinned)}
        self.slots[i]=None;Ok(())
    }
    pub fn phase(&self,service:u64)->Result<Phase,Error>{
        Ok(self.slots[self.position(service)?].ok_or(Error::Unknown)?.phase)
    }
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn requires_real_image_custody() {
        let image=Image{service:1,generation:4,length:100,
          digest:[1;32],signer:[2;32],manifest_hash:[3;32]};
        let v=Verification{image,verifier_pid:7,verifier_lease:1,
          evidence_nonce:9,signature_verified:true,
          manifest_verified:true,bytes_sealed:true};
        let pin=StoragePin{image,storage_lease:9,immutable_id:10,
          durable_sequence:2,readback_verified:true};
        let mut c=Custody::<2>::new();
        assert_eq!(c.begin(v,pin,1,false,true),Err(Error::WrongEvidence));
        c.begin(v,pin,1,true,true).unwrap();
        assert_eq!(c.phase(1),Ok(Phase::PreviousPinned));
    }
}
