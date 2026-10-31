//! Transactional Ring3 component activation, independent of kernel IPC.
//!
//! A graph plan is not a live grant. Service PIDs, kernel handles and lease
//! generations must be verified by the external supervisor before publication.
//! In particular, a rollback must revoke all published handles before it can
//! free an old generation for reuse.
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Component {
    pub id:u64,pub generation:u64,pub parent:u64,
    pub executable:u64,pub dependency_mask:u64,
}
impl Component {
    pub fn valid(self)->bool {
        self.id!=0 && self.generation!=0 && self.executable!=0
            && self.parent!=self.id
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct LeaseEvidence {
    pub component:u64,pub generation:u64,
    pub pid:u64,pub lease:u64,pub job_generation:u32,
}
impl LeaseEvidence {
    pub fn valid(self)->bool {
        self.component!=0 && self.generation!=0 &&
        self.pid!=0 && self.lease!=0 && self.job_generation!=0
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum State {Empty,Prepared,Spawned,Ready,Published,Draining,Revoked}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error {
    Invalid,Duplicate,UnknownParent,ForwardParent,Dependency,
    Capacity,Conflict,Stale,Missing,Witness,WrongPhase,
    Irreversible,Overflow,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
struct Slot {
    component:Component,state:State,evidence:Option<LeaseEvidence>,
    transaction:u64,
}
pub struct ActivationBook<const N:usize> {
    slots:[Option<Slot>;N],
    epoch:u64,transaction:u64,
}
impl<const N:usize> ActivationBook<N> {
    pub const fn new(epoch:u64)->Self {
        Self{slots:[None;N],epoch,transaction:0}
    }
    fn locate(&self,id:u64)->Result<usize,Error>{
        self.slots.iter().position(|x|x.is_some_and(|s|s.component.id==id))
            .ok_or(Error::Missing)
    }
    pub fn epoch(&self)->u64 { self.epoch }
    pub fn transaction(&self)->u64 {self.transaction}
    pub fn preflight(&self,manifest:&[Component])->Result<(),Error> {
        if manifest.len()>N || manifest.is_empty(){return Err(Error::Capacity)}
        for (i,node) in manifest.iter().enumerate() {
            if !node.valid(){return Err(Error::Invalid)}
            if manifest[..i].iter().any(|p|p.id==node.id){return Err(Error::Duplicate)}
            if node.parent!=0 && !manifest[..i].iter().any(|p|p.id==node.parent)
               && self.locate(node.parent).is_err() {
                return Err(Error::UnknownParent)
            }
            // Dependencies inside the transaction are strictly backwards;
            // committed existing services may be referenced out of band.
            for bit in 0..64 {
                if (node.dependency_mask & (1u64<<bit))==0 {continue}
                let dep_index=bit as usize;
                if dep_index>=manifest.len(){return Err(Error::Dependency)}
                if dep_index>=i {return Err(Error::ForwardParent)}
            }
            if self.slots.iter().flatten().any(|s|{
                s.component.id==node.id && s.state!=State::Revoked
            }) {return Err(Error::Conflict)}
        }
        Ok(())
    }
    /// Atomic publication of a new *plan*, not of live kernel endpoints.
    pub fn prepare(&mut self,manifest:&[Component],transaction:u64)
        ->Result<(),Error> {
        self.preflight(manifest)?;
        if transaction==0 || transaction<=self.transaction {
            return Err(Error::Stale)
        }
        let count=self.slots.iter().filter(|s|s.is_none()).count();
        if count<manifest.len(){return Err(Error::Capacity)}
        for &node in manifest {
            let idx=self.slots.iter().position(Option::is_none)
                .ok_or(Error::Capacity)?;
            self.slots[idx]=Some(Slot{
                component:node,state:State::Prepared,
                evidence:None,transaction,
            });
        }
        self.transaction=transaction;
        Ok(())
    }
    pub fn spawned(&mut self,id:u64,tx:u64,evidence:LeaseEvidence)
        ->Result<(),Error> {
        if !evidence.valid() || evidence.component!=id {
            return Err(Error::Witness)
        }
        let idx=self.locate(id)?;
        let slot=self.slots[idx].as_mut().ok_or(Error::Missing)?;
        if tx!=slot.transaction || evidence.generation!=slot.component.generation {
            return Err(Error::Stale)
        }
        if slot.state!=State::Prepared {return Err(Error::WrongPhase)}
        slot.evidence=Some(evidence);slot.state=State::Spawned;Ok(())
    }
    pub fn ready(&mut self,id:u64,tx:u64,witness:LeaseEvidence)
        ->Result<(),Error> {
        let idx=self.locate(id)?;
        let s=self.slots[idx].as_mut().ok_or(Error::Missing)?;
        if s.transaction!=tx || s.evidence!=Some(witness){
            return Err(Error::Witness)
        }
        if s.state!=State::Spawned {return Err(Error::WrongPhase)}
        s.state=State::Ready;Ok(())
    }
    pub fn can_publish(&self,id:u64,tx:u64)->Result<LeaseEvidence,Error>{
        let i=self.locate(id)?;
        let s=self.slots[i].ok_or(Error::Missing)?;
        if s.transaction!=tx {return Err(Error::Stale)}
        if s.state!=State::Ready{return Err(Error::WrongPhase)}
        if s.component.parent!=0 {
            let p=self.slots[self.locate(s.component.parent)?]
                .ok_or(Error::UnknownParent)?;
            if p.state!=State::Published {return Err(Error::Dependency)}
        }
        for bit in 0..64 {
            if s.component.dependency_mask & (1u64<<bit)!=0 {
                let dependency=self.slots.iter().flatten()
                    .filter(|x|x.transaction==tx).nth(bit as usize)
                    .ok_or(Error::Dependency)?;
                if dependency.state!=State::Published {
                    return Err(Error::Dependency)
                }
            }
        }
        s.evidence.ok_or(Error::Witness)
    }
    /// Kernel publication is performed before the caller records this state;
    /// an ambiguous copyout requires exact kernel lease reconciliation first.
    pub fn published(&mut self,id:u64,tx:u64,witness:LeaseEvidence,
                     kernel_observed:bool)->Result<(),Error>{
        if !kernel_observed{return Err(Error::Witness)}
        if self.can_publish(id,tx)?!=witness{return Err(Error::Witness)}
        let i=self.locate(id)?;
        self.slots[i].as_mut().ok_or(Error::Missing)?.state=State::Published;
        Ok(())
    }
    pub fn begin_drain(&mut self,id:u64,tx:u64)->Result<(),Error>{
        let idx=self.locate(id)?;
        let s=self.slots[idx].as_mut().ok_or(Error::Missing)?;
        if s.transaction!=tx{return Err(Error::Stale)}
        if s.state!=State::Published && s.state!=State::Ready
            && s.state!=State::Spawned {return Err(Error::WrongPhase)}
        s.state=State::Draining;
        Ok(())
    }
    pub fn revoked(&mut self,id:u64,tx:u64,
                   kernel_revocation_observed:bool)->Result<(),Error> {
        let idx=self.locate(id)?;
        let s=self.slots[idx].as_mut().ok_or(Error::Missing)?;
        if s.transaction!=tx{return Err(Error::Stale)}
        if s.state!=State::Draining || !kernel_revocation_observed {
            return Err(Error::Irreversible)
        }
        s.state=State::Revoked;Ok(())
    }
    pub fn retire(&mut self,id:u64,tx:u64)->Result<(),Error>{
        let idx=self.locate(id)?;
        let s=self.slots[idx].ok_or(Error::Missing)?;
        if s.state!=State::Revoked || s.transaction!=tx {
            return Err(Error::WrongPhase)
        }
        if self.slots.iter().flatten().any(|x|{
            x.component.parent==id && x.state!=State::Revoked
        }){return Err(Error::Dependency)}
        self.slots[idx]=None;
        Ok(())
    }
    pub fn active(&self,mut f:impl FnMut(Component,State,Option<LeaseEvidence>)){
        for x in self.slots.iter().flatten(){
            f(x.component,x.state,x.evidence)
        }
    }
    pub fn rotate_epoch(&mut self,next:u64)->Result<(),Error>{
        if next<=self.epoch || self.slots.iter().any(Option::is_some) {
            return Err(Error::Conflict)
        }
        self.epoch=next;self.transaction=0;Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn published_child_waits_for_parent() {
        let p=Component{id:1,generation:1,parent:0,executable:9,dependency_mask:0};
        let c=Component{id:2,generation:1,parent:1,executable:10,dependency_mask:1};
        let mut b=ActivationBook::<3>::new(1);
        b.prepare(&[p,c],2).unwrap();
        let wp=LeaseEvidence{component:1,generation:1,pid:100,lease:9,job_generation:3};
        let wc=LeaseEvidence{component:2,generation:1,pid:101,lease:9,job_generation:4};
        b.spawned(1,2,wp).unwrap();b.ready(1,2,wp).unwrap();
        b.spawned(2,2,wc).unwrap();b.ready(2,2,wc).unwrap();
        assert_eq!(b.can_publish(2,2),Err(Error::Dependency));
        b.published(1,2,wp,true).unwrap();
        b.published(2,2,wc,true).unwrap();
    }
}
