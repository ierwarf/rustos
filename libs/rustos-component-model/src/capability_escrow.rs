//! Ring3-side custody bookkeeping for transfer of kernel-created handles.
//!
//! This module NEVER mints a kernel descriptor. The descriptor is copied
//! through the existing kernel handle-transfer primitive only after a live
//! authenticated route and exact custody token have been checked. An ACK
//! without kernel evidence is NOT proof of a successful descriptor transfer.
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Incarnation { pub service:u64, pub pid:u64, pub lease:u64 }
impl Incarnation {
    pub fn valid(self)->bool { self.service!=0 && self.pid!=0 && self.lease!=0 }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct HandleRef { pub raw:u64, pub generation:u64, pub kind:u16, pub rights:u64 }
impl HandleRef {
    pub fn valid(self)->bool {
        self.raw!=0 && self.generation!=0 && self.kind!=0 && self.rights!=0
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Route { pub origin:Incarnation, pub target:Incarnation, pub capability:u64 }
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct TransferId {pub sequence:u64, pub epoch:u64}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Phase {Prepared, Sent, Accepted, Revoked}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error {
    Invalid, Unroutable, WeakRights, Capacity, Duplicate, Missing,
    WrongPeer, WrongHandle, WrongEpoch, WrongPhase, Exhausted,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
struct Entry {
    id:TransferId, route:Route, handle:HandleRef,
    granted_rights:u64, phase:Phase,
}
pub struct Escrow<const N:usize> {
    entries:[Option<Entry>;N], epoch:u64, sequence:u64,
}
impl<const N:usize> Escrow<N> {
    pub const fn new(epoch:u64)->Self {
        Self{entries:[None;N],epoch,sequence:0}
    }
    pub fn generation(&self)->u64 {self.epoch}
    fn locate(&self,id:TransferId)->Result<usize,Error> {
        if id.epoch!=self.epoch || id.epoch==0 {return Err(Error::WrongEpoch)}
        self.entries.iter().position(|e|e.is_some_and(|v|v.id==id)).ok_or(Error::Missing)
    }
    /// Production caller first authenticates route against exact live PID,
    /// JobPolicy and the positive rootd service grant, *then* prepares.
    pub fn prepare(&mut self,route:Route,handle:HandleRef,rights:u64,
                   authorized:bool)->Result<TransferId,Error> {
        if !authorized {return Err(Error::Unroutable)}
        if !route.origin.valid() || !route.target.valid()
            || route.origin==route.target || route.capability==0
            || !handle.valid() || rights==0 {return Err(Error::Invalid)}
        if rights & !handle.rights != 0 {return Err(Error::WeakRights)}
        let free=self.entries.iter().position(Option::is_none).ok_or(Error::Capacity)?;
        let next=self.sequence.checked_add(1).ok_or(Error::Exhausted)?;
        if self.epoch==0{return Err(Error::WrongEpoch)}
        let id=TransferId{sequence:next,epoch:self.epoch};
        self.entries[free]=Some(Entry{
            id,route,handle,granted_rights:rights,phase:Phase::Prepared
        });
        self.sequence=next;
        Ok(id)
    }
    /// Only call after transfer syscall has actually accepted the source
    /// handle + attenuated rights. If copyout fails, do not guess success:
    /// query kernel custody by the exact transfer key before deciding.
    pub fn sent(&mut self,id:TransferId,source:Incarnation,observed:HandleRef)
        ->Result<(),Error> {
        let idx=self.locate(id)?;
        let e=self.entries[idx].as_mut().ok_or(Error::Missing)?;
        if e.route.origin!=source {return Err(Error::WrongPeer)}
        if e.handle!=observed {return Err(Error::WrongHandle)}
        match e.phase {
            Phase::Prepared=>{e.phase=Phase::Sent;Ok(())}
            Phase::Sent=>Ok(()),
            _=>Err(Error::WrongPhase),
        }
    }
    /// Receipt MUST be authenticated against the receiver's exact live
    /// incarnation, not merely the name of a service.
    pub fn accepted(&mut self,id:TransferId,target:Incarnation,
                    observed_rights:u64,kernel_evidence:bool)->Result<(),Error> {
        let idx=self.locate(id)?;
        let e=self.entries[idx].as_mut().ok_or(Error::Missing)?;
        if e.route.target!=target {return Err(Error::WrongPeer)}
        if !kernel_evidence || observed_rights!=e.granted_rights {
            return Err(Error::WrongHandle)
        }
        match e.phase {
            Phase::Sent=>{e.phase=Phase::Accepted;Ok(())}
            Phase::Accepted=>Ok(()),
            _=>Err(Error::WrongPhase),
        }
    }
    pub fn phase(&self,id:TransferId)->Result<Phase,Error> {
        Ok(self.entries[self.locate(id)?].ok_or(Error::Missing)?.phase)
    }
    pub fn revoke(&mut self,id:TransferId,owner:Incarnation,
                  kernel_revocation_observed:bool)->Result<(),Error> {
        let idx=self.locate(id)?;
        let e=self.entries[idx].as_mut().ok_or(Error::Missing)?;
        if e.route.origin!=owner {return Err(Error::WrongPeer)}
        if !kernel_revocation_observed {return Err(Error::WrongPhase)}
        e.phase=Phase::Revoked;
        Ok(())
    }
    /// Tombstone retention is mandatory until kernel custody confirms that
    /// no outstanding transferred handle can be resurrected by a late ACK.
    pub fn retire(&mut self,id:TransferId,kernel_quiescent:bool)
        ->Result<(),Error> {
        let idx=self.locate(id)?;
        let e=self.entries[idx].ok_or(Error::Missing)?;
        if e.phase!=Phase::Revoked || !kernel_quiescent {return Err(Error::WrongPhase)}
        self.entries[idx]=None;
        Ok(())
    }
    /// A service crash removes its authority, but does NOT silently destroy
    /// an outstanding kernel descriptor. Caller must issue kernel revoke and
    /// reconcile each selected ID before retiring any entry.
    pub fn affected(&self,principal:Incarnation,
                    mut observe:impl FnMut(TransferId,Phase)) {
        for e in self.entries.iter().flatten() {
            if e.route.origin==principal || e.route.target==principal {
                observe(e.id,e.phase)
            }
        }
    }
    pub fn rotate(&mut self,next_epoch:u64)->Result<(),Error> {
        if next_epoch<=self.epoch || next_epoch==0 {return Err(Error::WrongEpoch)}
        if self.entries.iter().any(Option::is_some){return Err(Error::WrongPhase)}
        self.epoch=next_epoch;
        self.sequence=0;
        Ok(())
    }
    pub fn active_count(&self)->usize {
        self.entries.iter().filter(|e|e.is_some()).count()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const A:Incarnation=Incarnation{service:1,pid:10,lease:2};
    const B:Incarnation=Incarnation{service:2,pid:11,lease:7};
    const H:HandleRef=HandleRef{raw:1,generation:1,kind:2,rights:0xff};
    #[test]
    fn custody_is_distinct_from_policy() {
        let mut x=Escrow::<2>::new(5);
        let route=Route{origin:A,target:B,capability:8};
        assert_eq!(x.prepare(route,H,3,false),Err(Error::Unroutable));
        let id=x.prepare(route,H,3,true).unwrap();
        assert_eq!(x.accepted(id,B,3,true),Err(Error::WrongPhase));
        x.sent(id,A,H).unwrap();
        assert_eq!(x.accepted(id,B,3,false),Err(Error::WrongHandle));
        x.accepted(id,B,3,true).unwrap();
        x.revoke(id,A,true).unwrap();
        x.retire(id,true).unwrap();
        assert_eq!(x.active_count(),0);
    }
}
