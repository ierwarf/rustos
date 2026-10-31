//! Per-personality slow namespace policy with no kernel hot-path hooks.
//! Kernel still performs usercopy, VM, futex, IPC and actual handle checks.
use crate::personality::{Personality,Principal};
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum NamespaceClass { Mount, Network, Identity, Device, Executable, FileAssociation }
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct NamespaceKey {
    pub id:u64,pub generation:u64,pub personality:Personality,
}
impl NamespaceKey {
    pub fn valid(self)->bool{self.id!=0&&self.generation!=0}
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Action{Read,Create,Configure,Delete,Export}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Rule {
    pub key:NamespaceKey,pub class:NamespaceClass,pub action:Action,
    pub owner:Principal,pub allow:bool,pub epoch:u64,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error {
    Invalid,Duplicate,Capacity,Stale,Denied,Foreign,Missing,
    WrongPhase,Busy,EpochExhausted,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Permit {
    pub key:NamespaceKey,pub class:NamespaceClass,pub action:Action,
    pub principal:Principal,pub epoch:u64,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum State{Pending,Active,Revoking}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
struct Namespace {
    key:NamespaceKey,owner:Principal,state:State,
}
pub struct NamespacePolicy<const NS:usize,const RULES:usize> {
    namespaces:[Option<Namespace>;NS],
    rules:[Option<Rule>;RULES],
    epoch:u64,
}
impl<const NS:usize,const RULES:usize> NamespacePolicy<NS,RULES> {
    pub const fn new()->Self {
        Self{namespaces:[None;NS],rules:[None;RULES],epoch:0}
    }
    pub fn epoch(&self)->u64{self.epoch}
    fn locate(&self,key:NamespaceKey)->Result<usize,Error> {
        self.namespaces.iter().position(|n|n.is_some_and(|x|x.key==key))
            .ok_or(Error::Missing)
    }
    pub fn begin_namespace(&mut self,key:NamespaceKey,owner:Principal)->Result<(),Error>{
        if !key.valid()||!owner.valid(){return Err(Error::Invalid)}
        if self.namespaces.iter().flatten().any(|n|{
            n.key.id==key.id && n.key.personality==key.personality
        }){return Err(Error::Duplicate)}
        let idx=self.namespaces.iter().position(Option::is_none).ok_or(Error::Capacity)?;
        self.namespaces[idx]=Some(Namespace{key,owner,state:State::Pending});
        Ok(())
    }
    /// Caller supplies evidence that kernel namespace/handle custody exists.
    pub fn activate(&mut self,key:NamespaceKey,owner:Principal,
                    kernel_custody:bool)->Result<(),Error> {
        if !kernel_custody{return Err(Error::Denied)}
        let idx=self.locate(key)?;
        let n=self.namespaces[idx].as_mut().ok_or(Error::Missing)?;
        if n.owner!=owner{return Err(Error::Foreign)}
        if n.state!=State::Pending{return Err(Error::WrongPhase)}
        n.state=State::Active;Ok(())
    }
    pub fn install(&mut self,new_rules:&[Rule],epoch:u64)->Result<(),Error>{
        if epoch<=self.epoch||epoch==0{return Err(Error::Stale)}
        if new_rules.len()>RULES{return Err(Error::Capacity)}
        for (i,r) in new_rules.iter().enumerate(){
            if !r.owner.valid()||!r.key.valid()||r.epoch!=epoch {
                return Err(Error::Invalid)
            }
            let ns=self.namespaces[self.locate(r.key)?].ok_or(Error::Missing)?;
            if ns.owner!=r.owner || ns.state!=State::Active {
                return Err(Error::Foreign)
            }
            if new_rules[..i].iter().any(|x|{
                x.key==r.key && x.class==r.class && x.action==r.action
            }) {return Err(Error::Duplicate)}
        }
        let mut staged=[None;RULES];
        for (i,r) in new_rules.iter().enumerate(){staged[i]=Some(*r)}
        self.rules=staged;self.epoch=epoch;Ok(())
    }
    pub fn permit(&self,key:NamespaceKey,owner:Principal,
                  class:NamespaceClass,action:Action,min_epoch:u64)
         ->Result<Permit,Error> {
        let ns=self.namespaces[self.locate(key)?].ok_or(Error::Missing)?;
        if !owner.valid()||ns.owner!=owner{return Err(Error::Foreign)}
        if ns.state!=State::Active{return Err(Error::WrongPhase)}
        if self.epoch<min_epoch || min_epoch==0 {return Err(Error::Stale)}
        let rule=self.rules.iter().flatten().find(|r|{
            r.key==key&&r.class==class&&r.action==action
        }).ok_or(Error::Denied)?;
        if !rule.allow || rule.owner!=owner {return Err(Error::Denied)}
        Ok(Permit{key,class,action,principal:owner,epoch:self.epoch})
    }
    pub fn revoke_begin(&mut self,key:NamespaceKey,owner:Principal)
         ->Result<(),Error>{
        let idx=self.locate(key)?;
        let ns=self.namespaces[idx].as_mut().ok_or(Error::Missing)?;
        if ns.owner!=owner{return Err(Error::Foreign)}
        if ns.state!=State::Active{return Err(Error::WrongPhase)}
        ns.state=State::Revoking;
        // Remove the positive policy at the moment the revoke begins.
        for r in &mut self.rules {
            if r.is_some_and(|v|v.key==key){*r=None}
        }
        Ok(())
    }
    pub fn revoke_complete(&mut self,key:NamespaceKey,owner:Principal,
                           kernel_quiescent:bool)->Result<(),Error>{
        if !kernel_quiescent{return Err(Error::Busy)}
        let idx=self.locate(key)?;
        let ns=self.namespaces[idx].ok_or(Error::Missing)?;
        if ns.owner!=owner{return Err(Error::Foreign)}
        if ns.state!=State::Revoking{return Err(Error::WrongPhase)}
        self.namespaces[idx]=None;
        Ok(())
    }
    pub fn invalidate_owner(&mut self,owner:Principal,
                            mut revoke:impl FnMut(NamespaceKey)){
        for ns in &mut self.namespaces {
            if let Some(n)=ns.as_mut(){
                if n.owner==owner && n.state==State::Active {
                    n.state=State::Revoking;revoke(n.key);
                }
            }
        }
        for r in &mut self.rules {
            if r.is_some_and(|v|v.owner==owner){*r=None}
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const P:Principal=Principal{
        pid:1,job_slot:2,job_generation:3,lease_generation:4,
    };
    const KEY:NamespaceKey=NamespaceKey{
        id:8,generation:1,personality:Personality::Windows,
    };
    #[test]
    fn inherited_process_does_not_inherit_namespace_control(){
        let mut n=NamespacePolicy::<2,4>::new();
        n.begin_namespace(KEY,P).unwrap();
        n.activate(KEY,P,true).unwrap();
        n.install(&[Rule{key:KEY,class:NamespaceClass::Mount,
            action:Action::Configure,owner:P,allow:true,epoch:1}],1).unwrap();
        assert!(n.permit(KEY,P,NamespaceClass::Mount,Action::Configure,1).is_ok());
        let fake=Principal{pid:99,..P};
        assert_eq!(n.permit(KEY,fake,NamespaceClass::Mount,Action::Configure,1),
            Err(Error::Foreign));
        n.revoke_begin(KEY,P).unwrap();
        assert_eq!(n.permit(KEY,P,NamespaceClass::Mount,Action::Configure,1),
            Err(Error::WrongPhase));
    }
}
