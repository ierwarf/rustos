//! Draft generic rootd broker for client-funded provider sessions.
//!
//! NOT WIRED TO THE PUBLIC SUPERVISOR RPC YET. This exists to allow the
//! netd/vfsd conversion to be made atomically in a later Codex cutover.
//! Its inputs MUST come from kernel-stamped sender identity, not from a
//! provider-supplied PID. The kernel Job token ledger remains authoritative.
use alloc::vec::Vec;
use rustos_component_model::provider_sessions::{
    Client, Owner, ProviderKind, State, Ticket, Token,
};
const EINVAL:i32=22;
const EACCES:i32=13;
const ENOSPC:i32=28;
const ESTALE:i32=116;
const EBUSY:i32=16;
const MAX_PROVIDER_TICKETS:usize=4096;
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
struct Funded {
    provider:Owner,client:Client,kind:ProviderKind,
    session:u64,token:Token,kernel_token:u64,
    charge_id:u64,state:State,
}
pub(super) struct ProviderBilling {
    funded:Vec<Funded>,
    next_kernel_token:u64,
}
impl ProviderBilling {
    pub(super) const fn new()->Self {
        Self{funded:Vec::new(),next_kernel_token:0}
    }
    fn find(&self,owner:Owner,token:Token)->Option<usize> {
        self.funded.iter().position(|x|x.provider==owner&&x.token==token)
    }
    /// No kernel side effects until memory for the receipt is reserved.
    /// The caller owns all kernel queries and confirms that the client PID
    /// is currently bound to the exact Job generation in the request.
    pub(super) fn charge(
        &mut self,ticket:Ticket,kernel_sender_pid:u64,
        client_job_verified:bool,
        mut charge:impl FnMut(Client,u64,u64)->Result<u64,i32>,
    )->Result<u64,i32> {
        if !ticket.provider.valid()||!ticket.client.valid()
           ||!ticket.token.valid()||ticket.session==0{return Err(EINVAL)}
        if kernel_sender_pid!=ticket.provider.pid || !client_job_verified {
            return Err(EACCES)
        }
        if let Some(idx)=self.find(ticket.provider,ticket.token) {
            let prior=self.funded[idx];
            if prior.client!=ticket.client || prior.session!=ticket.session
                || prior.kind!=ticket.kind{return Err(EACCES)}
            if prior.state!=State::Charged{return Err(EBUSY)}
            return Ok(prior.charge_id)
        }
        if self.funded.len()>=MAX_PROVIDER_TICKETS{return Err(ENOSPC)}
        self.funded.try_reserve(1).map_err(|_|12)?;
        let kernel_token=self.next_kernel_token.checked_add(1).ok_or(75)?;
        // A kernel charge may commit even if the copyout fails. The callback
        // MUST reconcile by this token; it may not retry with a fresh token.
        // The token and ambiguous intent MUST be stored before an RPC can
        // commit the kernel charge. Error leaves ChargeUnknown in custody.
        self.next_kernel_token=kernel_token;
        self.funded.push(Funded{
            provider:ticket.provider,client:ticket.client,
            kind:ticket.kind,session:ticket.session,token:ticket.token,
            kernel_token,charge_id:0,state:State::ChargeUnknown,
        });
        match charge(ticket.client,ticket.provider.pid,kernel_token) {
            Ok(charge_id) if charge_id!=0 => {
                let slot=self.find(ticket.provider,ticket.token).ok_or(ESTALE)?;
                self.funded[slot].charge_id=charge_id;
                self.funded[slot].state=State::Charged;
                Ok(charge_id)
            }
            Ok(_) => Err(ESTALE),
            Err(err) => Err(err),
        }
    }
    /// Record release only after a kernel-confirmed terminal refund. A
    /// timeout must leave the entry alive for exact-token reconciliation.
    pub(super) fn release(
        &mut self,owner:Owner,token:Token,kernel_sender_pid:u64,
        mut refund:impl FnMut(u64,u64,u64)->Result<(),i32>,
    )->Result<(),i32> {
        if kernel_sender_pid!=owner.pid{return Err(EACCES)}
        let idx=self.find(owner,token).ok_or(ESTALE)?;
        let entry=self.funded[idx];
        if !matches!(entry.state,State::Charged|State::ReleaseUnknown){
            return Err(EBUSY)
        }
        self.funded[idx].state=State::ReleaseUnknown;
        refund(entry.client.job,owner.pid,entry.kernel_token)?;
        self.funded.swap_remove(idx);
        Ok(())
    }
    /// On provider exit, do NOT trust stale provider IPC to trigger refund.
    /// Rootd requires kernel-authenticated lifecycle evidence, then drains
    /// kernel-owned outstanding tokens before any PID reuse.
    pub(super) fn owner_exit(
        &mut self,owner:Owner,lifecycle_verified:bool,
        mut drain:impl FnMut(u64)->Result<(),i32>,
    )->Result<usize,i32>{
        if !owner.valid()||!lifecycle_verified{return Err(EACCES)}
        drain(owner.pid)?;
        let before=self.funded.len();
        self.funded.retain(|e|e.provider!=owner);
        Ok(before-self.funded.len())
    }
    pub(super) fn unresolved(&self,mut f:impl FnMut(Owner,Client,u64)) {
        for x in &self.funded {f(x.provider,x.client,x.kernel_token)}
    }
    pub(super) fn count(&self)->usize{self.funded.len()}
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn billing_records_exact_client_not_provider() {
        let owner=Owner{service:3,pid:8,lease:2};
        let client=Client{pid:40,job:22,job_generation:9};
        let ticket=Ticket{provider:owner,client,session:1,
            kind:ProviderKind::Socket,token:Token{epoch:2,nonce:1}};
        let mut book=ProviderBilling::new();
        let mut calls=0;
        book.charge(ticket,8,true,|c,p,t|{
            assert_eq!(c,client);assert_eq!(p,8);assert_eq!(t,1);
            calls+=1;Ok(9)
        }).unwrap();
        book.charge(ticket,8,true,|_,_,_|panic!("must never double charge")).unwrap();
        assert_eq!(calls,1);
        book.release(owner,ticket.token,8,|job,p,t|{
            assert_eq!((job,p,t),(22,8,1));Ok(())
        }).unwrap();
        assert_eq!(book.count(),0);
    }
}
