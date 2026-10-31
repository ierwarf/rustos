//! Ordered service rollout planner for Ring3 supervisor slow paths.
//!
//! Unlike a live process controller, this planner has no authority to spawn,
//! kill, or issue handles. Dependencies are manifest-authored and must be
//! satisfied against exact kernel-stamped lease generations before execution.
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Service {
    pub id:u64,pub generation:u64,pub parent:u64,
    pub restart_limit:u16,pub critical:bool,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Phase{NotStarted,Prepared,Stopped,Started,Ready,Committed,Failed}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error{Empty,Duplicate,ForwardParent,Capacity,Conflict,
    Stale,Missing,WrongPhase,Dependency,Limit,Overflow}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
struct Step {
    service:Service,phase:Phase,retries:u16,
    old_pid:u64,new_pid:u64,
}
pub struct Rollout<const N:usize>{
    steps:[Option<Step>;N],transaction:u64,
}
impl<const N:usize> Rollout<N>{
    pub const fn new()->Self{
        Self{steps:[None;N],transaction:0}
    }
    pub fn transaction(&self)->u64{self.transaction}
    fn index(&self,id:u64)->Result<usize,Error>{
        self.steps.iter().position(|s|s.is_some_and(|v|v.service.id==id))
          .ok_or(Error::Missing)
    }
    pub fn prepare(&mut self,services:&[Service],transaction:u64)
       ->Result<(),Error>{
        if services.is_empty(){return Err(Error::Empty)}
        if services.len()>N{return Err(Error::Capacity)}
        if transaction<=self.transaction||transaction==0{return Err(Error::Stale)}
        if self.steps.iter().flatten().any(|s|{
            !matches!(s.phase,Phase::Committed|Phase::Failed)
        }){return Err(Error::Conflict)}
        for (i,s) in services.iter().enumerate(){
            if s.id==0||s.generation==0{return Err(Error::Empty)}
            if services[..i].iter().any(|x|x.id==s.id){return Err(Error::Duplicate)}
            if s.parent==s.id || s.parent!=0
                && !services[..i].iter().any(|x|x.id==s.parent){
                return Err(Error::ForwardParent)
            }
        }
        self.steps=[None;N];
        for (i,s) in services.iter().enumerate(){
            self.steps[i]=Some(Step{service:*s,phase:Phase::Prepared,
                retries:0,old_pid:0,new_pid:0});
        }
        self.transaction=transaction;Ok(())
    }
    pub fn record_old(&mut self,id:u64,tx:u64,pid:u64,
                      authenticated:bool)->Result<(),Error>{
        if tx!=self.transaction||pid==0{return Err(Error::Stale)}
        if !authenticated{return Err(Error::Dependency)}
        let i=self.index(id)?;
        let s=self.steps[i].as_mut().ok_or(Error::Missing)?;
        if s.phase!=Phase::Prepared{return Err(Error::WrongPhase)}
        s.old_pid=pid;Ok(())
    }
    pub fn stopped(&mut self,id:u64,tx:u64,
                   old_pid:u64,kernel_revoke_confirmed:bool)->Result<(),Error>{
        if tx!=self.transaction{return Err(Error::Stale)}
        let i=self.index(id)?;
        let s=self.steps[i].as_mut().ok_or(Error::Missing)?;
        if s.phase!=Phase::Prepared||s.old_pid==0||s.old_pid!=old_pid
            ||!kernel_revoke_confirmed{return Err(Error::Dependency)}
        s.phase=Phase::Stopped;Ok(())
    }
    pub fn started(&mut self,id:u64,tx:u64,new_pid:u64,
                   kernel_lease_confirmed:bool)->Result<(),Error>{
        if tx!=self.transaction||new_pid==0{return Err(Error::Stale)}
        let i=self.index(id)?;
        let s=self.steps[i].ok_or(Error::Missing)?;
        if s.phase!=Phase::Stopped{return Err(Error::WrongPhase)}
        if s.old_pid==new_pid||!kernel_lease_confirmed{return Err(Error::Dependency)}
        if s.service.parent!=0 {
            let p=self.steps[self.index(s.service.parent)?].ok_or(Error::Dependency)?;
            if !matches!(p.phase,Phase::Ready|Phase::Committed){
                return Err(Error::Dependency)
            }
        }
        self.steps[i].as_mut().ok_or(Error::Missing)?.new_pid=new_pid;
        self.steps[i].as_mut().ok_or(Error::Missing)?.phase=Phase::Started;
        Ok(())
    }
    pub fn ready(&mut self,id:u64,tx:u64,pid:u64,
                 health_authenticated:bool)->Result<(),Error>{
        if tx!=self.transaction||!health_authenticated{return Err(Error::Stale)}
        let i=self.index(id)?;
        let s=self.steps[i].as_mut().ok_or(Error::Missing)?;
        if s.phase!=Phase::Started||s.new_pid!=pid{return Err(Error::WrongPhase)}
        s.phase=Phase::Ready;Ok(())
    }
    pub fn committed(&mut self,id:u64,tx:u64,
                     durable:bool)->Result<(),Error>{
        if tx!=self.transaction||!durable{return Err(Error::Stale)}
        let i=self.index(id)?;
        let s=self.steps[i].as_mut().ok_or(Error::Missing)?;
        if s.phase!=Phase::Ready{return Err(Error::WrongPhase)}
        s.phase=Phase::Committed;Ok(())
    }
    pub fn retry(&mut self,id:u64,tx:u64)->Result<(),Error>{
        if tx!=self.transaction{return Err(Error::Stale)}
        let i=self.index(id)?;
        let s=self.steps[i].as_mut().ok_or(Error::Missing)?;
        if !matches!(s.phase,Phase::Stopped|Phase::Started|Phase::Failed){
            return Err(Error::WrongPhase)
        }
        if s.retries>=s.service.restart_limit{return Err(Error::Limit)}
        s.retries=s.retries.checked_add(1).ok_or(Error::Overflow)?;
        s.phase=Phase::Stopped;s.new_pid=0;Ok(())
    }
    pub fn failed(&mut self,id:u64,tx:u64)->Result<(),Error>{
        if tx!=self.transaction{return Err(Error::Stale)}
        let i=self.index(id)?;
        let s=self.steps[i].as_mut().ok_or(Error::Missing)?;
        if s.phase==Phase::Committed{return Err(Error::Conflict)}
        s.phase=Phase::Failed;Ok(())
    }
    pub fn snapshot(&self,mut f:impl FnMut(Service,Phase,u16)){
        for s in self.steps.iter().flatten(){f(s.service,s.phase,s.retries)}
    }
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn parent_ready_precedes_child_activation(){
        let mut b=Rollout::<4>::new();
        let parent=Service{id:1,generation:1,parent:0,
            restart_limit:3,critical:true};
        let child=Service{id:2,generation:1,parent:1,
            restart_limit:3,critical:false};
        b.prepare(&[parent,child],1).unwrap();
        b.record_old(1,1,10,true).unwrap();
        b.record_old(2,1,20,true).unwrap();
        b.stopped(1,1,10,true).unwrap();
        b.stopped(2,1,20,true).unwrap();
        assert_eq!(b.started(2,1,21,true),Err(Error::Dependency));
        b.started(1,1,11,true).unwrap();
        b.ready(1,1,11,true).unwrap();
        b.started(2,1,21,true).unwrap();
    }
}
