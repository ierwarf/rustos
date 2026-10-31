//! Bounded supervisor crash reconciliation plan. Not a kernel authority.
//!
//! Exact kernel lease and token queries are prerequisites to taking actions.
//! A crash report is never authorization to kill a PID observed by a stale
//! process generation. This is a planning queue, not a background daemon.
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Principal {
    pub service:u64,pub pid:u64,pub lease_generation:u64,
    pub job_generation:u32,
}
impl Principal{
    pub fn valid(self)->bool{
        self.service!=0&&self.pid!=0&&self.lease_generation!=0&&self.job_generation!=0
    }
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Cause{
    Exited,HealthTimedOut,ParentRevoked,UpdateFailed,
    SessionLeak,CheckpointConflict,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Step{
    FreezeRoutes,DrainSessions,RevokeDescriptors,
    RetireCheckpoint,TerminateExactProcess,RestartEligible,
}
impl Step {
    pub const ALL:[Self;6]=[
        Self::FreezeRoutes,Self::DrainSessions,Self::RevokeDescriptors,
        Self::RetireCheckpoint,Self::TerminateExactProcess,
        Self::RestartEligible,
    ];
    const fn index(self)->usize {self as usize}
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Status {Pending,InFlight,Complete}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error{
    Invalid,Capacity,Duplicate,Unknown,Stale,WrongOrder,
    MissingEvidence,InFlight,Overflow,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Plan {
    pub key:u64,pub subject:Principal,pub cause:Cause,
    pub epoch:u64,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
struct Entry {plan:Plan,steps:[Status;6]}
pub struct ReconcileQueue<const N:usize>{
    slots:[Option<Entry>;N],epoch:u64,completed:u64,
}
impl<const N:usize> ReconcileQueue<N>{
    pub const fn new(epoch:u64)->Self{
        Self{slots:[None;N],epoch,completed:0}
    }
    fn locate(&self,key:u64)->Result<usize,Error>{
        self.slots.iter().position(|e|e.is_some_and(|s|s.plan.key==key))
            .ok_or(Error::Unknown)
    }
    pub fn schedule(&mut self,plan:Plan)->Result<(),Error>{
        if !plan.subject.valid()||plan.key==0||plan.epoch!=self.epoch {
            return Err(Error::Invalid)
        }
        if self.locate(plan.key).is_ok(){return Err(Error::Duplicate)}
        let free=self.slots.iter().position(Option::is_none).ok_or(Error::Capacity)?;
        self.slots[free]=Some(Entry{plan,steps:[Status::Pending;6]});
        Ok(())
    }
    /// Frozen routes and drained sessions must precede termination.
    /// Without an exact kernel snapshot the caller may not claim a step.
    pub fn dispatch(&mut self,key:u64,step:Step,actual_lease:Principal)
        ->Result<(),Error>{
        let idx=self.locate(key)?;
        let e=self.slots[idx].as_mut().ok_or(Error::Unknown)?;
        if e.plan.subject!=actual_lease{return Err(Error::Stale)}
        let slot=step.index();
        if e.steps[slot]!=Status::Pending{return Err(Error::InFlight)}
        for before in &e.steps[..slot] {
            if *before!=Status::Complete{return Err(Error::WrongOrder)}
        }
        e.steps[slot]=Status::InFlight;
        Ok(())
    }
    pub fn confirm(&mut self,key:u64,step:Step,
                   actual_lease:Principal,kernel_evidence:bool)
        ->Result<(),Error>{
        let idx=self.locate(key)?;
        let e=self.slots[idx].as_mut().ok_or(Error::Unknown)?;
        if actual_lease!=e.plan.subject{return Err(Error::Stale)}
        if !kernel_evidence{return Err(Error::MissingEvidence)}
        if e.steps[step.index()]!=Status::InFlight{return Err(Error::WrongOrder)}
        e.steps[step.index()]=Status::Complete;
        Ok(())
    }
    pub fn retry_unresolved(&mut self,key:u64,step:Step,
                            kernel_absence_verified:bool)->Result<(),Error>{
        let idx=self.locate(key)?;
        let e=self.slots[idx].as_mut().ok_or(Error::Unknown)?;
        if e.steps[step.index()]!=Status::InFlight{return Err(Error::WrongOrder)}
        if !kernel_absence_verified{return Err(Error::MissingEvidence)}
        e.steps[step.index()]=Status::Pending;Ok(())
    }
    pub fn next(&self,key:u64)->Result<Option<Step>,Error>{
        let e=self.slots[self.locate(key)?].ok_or(Error::Unknown)?;
        for step in Step::ALL {
            if e.steps[step.index()]!=Status::Complete {
                return Ok(Some(step))
            }
        }
        Ok(None)
    }
    pub fn retire(&mut self,key:u64)->Result<(),Error>{
        let idx=self.locate(key)?;
        if self.slots[idx].ok_or(Error::Unknown)?.steps
            .iter().any(|s|*s!=Status::Complete) {
            return Err(Error::WrongOrder)
        }
        self.completed=self.completed.checked_add(1).ok_or(Error::Overflow)?;
        self.slots[idx]=None;Ok(())
    }
    pub fn count(&self)->usize{self.slots.iter().filter(|s|s.is_some()).count()}
    pub fn completed(&self)->u64{self.completed}
    pub fn rotate(&mut self,epoch:u64)->Result<(),Error>{
        if epoch<=self.epoch{return Err(Error::Stale)}
        if self.count()!=0{return Err(Error::InFlight)}
        self.epoch=epoch;Ok(())
    }
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn process_exit_cannot_skip_refunds() {
        let p=Principal{service:1,pid:2,lease_generation:3,job_generation:4};
        let mut q=ReconcileQueue::<2>::new(1);
        q.schedule(Plan{key:1,subject:p,cause:Cause::Exited,epoch:1}).unwrap();
        assert_eq!(q.dispatch(1,Step::TerminateExactProcess,p),
            Err(Error::WrongOrder));
        q.dispatch(1,Step::FreezeRoutes,p).unwrap();
        q.confirm(1,Step::FreezeRoutes,p,true).unwrap();
        assert_eq!(q.next(1),Ok(Some(Step::DrainSessions)));
    }
}
