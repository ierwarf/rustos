//! Generalized client-paid provider session control-plane ledger.
//!
//! netd, vfsd and other slow providers may use this protocol to ask rootd
//! for a kernel Job charge on behalf of an *authenticated client*.
//! No provider may debit its own Job instead of the client, and a PID alone
//! is never proof of a Job generation. Kernel-owned token inventory is the
//! ultimate source of truth after lost ACKs and rootd process restarts.
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum ProviderKind {Console,Socket,OpenDescription,DeviceChannel,Other(u16)}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Owner {
    pub service:u64,pub pid:u64,pub lease:u64,
}
impl Owner {pub fn valid(self)->bool{self.service!=0&&self.pid!=0&&self.lease!=0}}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Client {
    pub pid:u64,pub job:u64,pub job_generation:u32,
}
impl Client {pub fn valid(self)->bool{self.pid!=0&&self.job!=0&&self.job_generation!=0}}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Token {
    pub epoch:u64,pub nonce:u64,
}
impl Token {pub fn valid(self)->bool{self.epoch!=0&&self.nonce!=0}}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum State {
    Prepared, ChargeUnknown, Charged, ReleaseUnknown,
    Settled, Cancelled,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error {
    Invalid, Capacity, Duplicate, Foreign, Stale,
    Missing, Phase, Evidence, Overflow, Outstanding,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Ticket {
    pub token:Token,pub provider:Owner,pub client:Client,
    pub kind:ProviderKind,pub session:u64,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
struct Entry{ticket:Ticket,state:State,kernel_charge_id:u64}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Observe {
    /// Authenticated kernel lookup confirms the charge exists exactly once.
    Charged(u64),
    /// Kernel confirms the token never charged (or is fully refunded).
    Absent,
    /// Unknown / RPC unavailable. Must not infer a refund.
    Unknown,
}
pub struct ProviderBook<const N:usize>{
    owner:Owner,epoch:u64,entries:[Option<Entry>;N],
    highest_nonce:u64,completed:u64,
}
impl<const N:usize> ProviderBook<N>{
    pub const fn new(owner:Owner,epoch:u64)->Self{
        Self{owner,epoch,entries:[None;N],highest_nonce:0,completed:0}
    }
    pub fn owner(&self)->Owner{self.owner}
    fn locate(&self,token:Token)->Result<usize,Error>{
        self.entries.iter().position(|e|e.is_some_and(|x|x.ticket.token==token))
           .ok_or(Error::Missing)
    }
    /// A new provider epoch is admitted only after the old inventory is
    /// reconciled with kernel token custody.
    pub fn rebind(&mut self,new_owner:Owner,epoch:u64)->Result<(),Error>{
        if !new_owner.valid()||epoch<=self.epoch{return Err(Error::Stale)}
        if self.entries.iter().flatten().any(|e|{
            e.state!=State::Settled && e.state!=State::Cancelled
        }) {return Err(Error::Outstanding)}
        self.owner=new_owner;self.epoch=epoch;self.highest_nonce=0;
        self.entries=[None;N];
        Ok(())
    }
    pub fn prepare(&mut self,ticket:Ticket)->Result<(),Error>{
        if !self.owner.valid() || ticket.provider!=self.owner
          || !ticket.client.valid() || !ticket.token.valid()
          || ticket.token.epoch!=self.epoch || ticket.session==0 {
            return Err(Error::Invalid)
        }
        if self.entries.iter().flatten().any(|e|{
            e.ticket.token==ticket.token || (
                e.ticket.provider==ticket.provider &&
                e.ticket.kind==ticket.kind &&
                e.ticket.session==ticket.session &&
                e.state!=State::Settled && e.state!=State::Cancelled)
        }) {return Err(Error::Duplicate)}
        if ticket.token.nonce<=self.highest_nonce {return Err(Error::Stale)}
        let idx=self.entries.iter().position(Option::is_none).ok_or(Error::Capacity)?;
        self.entries[idx]=Some(Entry{ticket,state:State::Prepared,kernel_charge_id:0});
        self.highest_nonce=ticket.token.nonce;
        Ok(())
    }
    /// Called BEFORE issuing the first charge syscall/RPC. A crash during
    /// transit leaves an UNKNOWN, never a confirmed absent debit.
    pub fn charge_dispatched(&mut self,token:Token)->Result<(),Error>{
        let idx=self.locate(token)?;
        let e=self.entries[idx].as_mut().ok_or(Error::Missing)?;
        if e.state!=State::Prepared{return Err(Error::Phase)}
        e.state=State::ChargeUnknown;Ok(())
    }
    pub fn charge_evidence(&mut self,token:Token,observation:Observe)
        ->Result<(),Error> {
        let i=self.locate(token)?;
        let e=self.entries[i].as_mut().ok_or(Error::Missing)?;
        if e.state!=State::ChargeUnknown && e.state!=State::Charged {
            return Err(Error::Phase)
        }
        match observation {
            Observe::Charged(id) if id!=0 => {
                if e.kernel_charge_id!=0 && e.kernel_charge_id!=id {
                    return Err(Error::Evidence)
                }
                e.kernel_charge_id=id;
                e.state=State::Charged;
            }
            Observe::Absent if e.kernel_charge_id==0 => {
                e.state=State::Cancelled;
            }
            Observe::Unknown => {}
            _=>return Err(Error::Evidence)
        }
        Ok(())
    }
    pub fn release_dispatched(&mut self,token:Token)->Result<(),Error>{
        let idx=self.locate(token)?;
        let e=self.entries[idx].as_mut().ok_or(Error::Missing)?;
        match e.state {
            State::Charged|State::ReleaseUnknown=>{
                e.state=State::ReleaseUnknown;Ok(())
            }
            _=>Err(Error::Phase)
        }
    }
    pub fn release_evidence(&mut self,token:Token,observation:Observe)
        ->Result<(),Error>{
        let idx=self.locate(token)?;
        let e=self.entries[idx].as_mut().ok_or(Error::Missing)?;
        if e.state!=State::ReleaseUnknown{return Err(Error::Phase)}
        match observation {
            Observe::Absent=>{e.state=State::Settled;Ok(())}
            Observe::Charged(id) if id==e.kernel_charge_id=>Ok(()),
            Observe::Unknown=>Ok(()),
            _=>Err(Error::Evidence)
        }
    }
    pub fn cancel_unsent(&mut self,token:Token)->Result<(),Error>{
        let idx=self.locate(token)?;
        let e=self.entries[idx].as_mut().ok_or(Error::Missing)?;
        if e.state!=State::Prepared{return Err(Error::Phase)}
        e.state=State::Cancelled;Ok(())
    }
    pub fn entry(&self,token:Token)->Result<(Ticket,State,u64),Error>{
        let e=self.entries[self.locate(token)?].ok_or(Error::Missing)?;
        Ok((e.ticket,e.state,e.kernel_charge_id))
    }
    pub fn retire_receipt(&mut self,token:Token,
                          verified_kernel_terminal:bool)->Result<(),Error>{
        let idx=self.locate(token)?;
        let e=self.entries[idx].ok_or(Error::Missing)?;
        if !verified_kernel_terminal ||
            !matches!(e.state,State::Settled|State::Cancelled) {
            return Err(Error::Evidence)
        }
        self.entries[idx]=None;
        self.completed=self.completed.checked_add(1).ok_or(Error::Overflow)?;
        Ok(())
    }
    /// Exactly the unresolved tickets are reported on provider exit so rootd
    /// can reconcile a charge before permitting an incarnation replacement.
    pub fn unresolved(&self,mut f:impl FnMut(Ticket,State,u64)){
        for e in self.entries.iter().flatten() {
            if !matches!(e.state,State::Settled|State::Cancelled) {
                f(e.ticket,e.state,e.kernel_charge_id)
            }
        }
    }
    pub fn live(&self)->usize{
        self.entries.iter().flatten()
            .filter(|e|!matches!(e.state,State::Settled|State::Cancelled)).count()
    }
    pub fn completed(&self)->u64{self.completed}
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn unknown_is_not_refund() {
        let p=Owner{service:2,pid:11,lease:3};
        let c=Client{pid:88,job:5,job_generation:9};
        let t=Ticket{token:Token{epoch:1,nonce:2},provider:p,client:c,
            kind:ProviderKind::Socket,session:7};
        let mut b=ProviderBook::<2>::new(p,1);
        b.prepare(t).unwrap();
        b.charge_dispatched(t.token).unwrap();
        b.charge_evidence(t.token,Observe::Unknown).unwrap();
        assert_eq!(b.live(),1);
        b.charge_evidence(t.token,Observe::Charged(19)).unwrap();
        b.release_dispatched(t.token).unwrap();
        b.release_evidence(t.token,Observe::Unknown).unwrap();
        assert_eq!(b.live(),1);
        b.release_evidence(t.token,Observe::Absent).unwrap();
        b.retire_receipt(t.token,true).unwrap();
        assert_eq!(b.live(),0);
    }
}
