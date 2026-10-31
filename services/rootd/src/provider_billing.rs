//! Draft generic rootd broker for client-funded provider sessions.
//!
//! Generic provider RPC is dispatched by rootd; netd/vfsd final-ref
//! call-site custody remains staged until actual per-object cutover.
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
const MAX_PROVIDER_OWNERS:usize=128;
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
struct Funded {
    provider:Owner,client:Client,kind:ProviderKind,
    session:u64,token:Token,kernel_token:u64,
    charge_id:u64,state:State,
}
pub(super) struct ProviderBilling {
    funded:Vec<Funded>,
    next_kernel_token:u64,
    // An ACKed terminal receipt can be discarded, but the provider-local
    // token watermark must survive until this exact PID exits. Otherwise a
    // reissued nonce could receive a FRESH kernel token and double-charge.
    owner_watermarks:Vec<(Owner,u64)>,
}
impl ProviderBilling {
    pub(super) const fn new()->Self {
        Self{funded:Vec::new(),next_kernel_token:0,
            owner_watermarks:Vec::new()}
    }
    pub(super) fn unknown(&self,owner:Owner,token:Token)->bool {
        self.find(owner,token).is_some_and(|i|self.funded[i].state==State::ChargeUnknown)
    }
    pub(super) fn find_ticket(&self,owner:Owner,token:Token)
        ->Option<(Client,u64,ProviderKind,State,u64)> {
        let i=self.find(owner,token)?;
        let e=self.funded[i];
        Some((e.client,e.session,e.kind,e.state,e.charge_id))
    }
    fn watermark(&self,owner:Owner)->Option<u64>{
        self.owner_watermarks.iter().find(|(o,_)|*o==owner).map(|(_,high)|*high)
    }
    fn admit_owner_nonce(&mut self,owner:Owner,nonce:u64)->Result<(),i32>{
        if let Some((_,high))=self.owner_watermarks.iter_mut()
            .find(|(o,_)|*o==owner){
            if nonce<=*high{return Err(ESTALE)}
            *high=nonce;
            return Ok(())
        }
        if self.owner_watermarks.len()>=MAX_PROVIDER_OWNERS{return Err(ENOSPC)}
        self.owner_watermarks.try_reserve(1).map_err(|_|12)?;
        self.owner_watermarks.push((owner,nonce));
        Ok(())
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
            return match prior.state {
                State::Charged=>Ok(prior.charge_id),
                State::Settled=>Err(ESTALE),
                _=>Err(EBUSY),
            }
        }
        // Never reuse an already-seen token even if its receipt has been
        // retained only as a settled tombstone. Clients must monotonically
        // advance the provider epoch/nonce pair.
        if self.funded.iter().any(|x|{
            x.provider==ticket.provider
                && x.token.epoch==ticket.token.epoch
                && x.token.nonce>=ticket.token.nonce
        }) {return Err(ESTALE)}
        if self.funded.len()>=MAX_PROVIDER_TICKETS{return Err(ENOSPC)}
        // All fallible memory and token-overflow checks precede the kernel
        // charge; watermark advancement is conservatively irreversible.
        self.funded.try_reserve(1).map_err(|_|12)?;
        let kernel_token=self.next_kernel_token.checked_add(1).ok_or(75)?;
        self.admit_owner_nonce(ticket.provider,ticket.token.nonce)?;
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
    /// A lost ACK after charge must be retried with this SAME kernel token.
    /// The callback must obtain an authoritative kernel result. No other
    /// provider token is spent while its predecessor is unresolved.
    pub(super) fn reconcile_charge(
        &mut self, owner:Owner, token:Token, client:Client,
        session:u64, kind:ProviderKind, sender_pid:u64,
        mut retry:impl FnMut(Client,u64,u64)->Result<u64,i32>,
    )->Result<u64,i32>{
        if sender_pid!=owner.pid {return Err(EACCES)}
        let idx=self.find(owner,token).ok_or(ESTALE)?;
        let e=self.funded[idx];
        if e.client!=client || e.session!=session || e.kind!=kind {
            return Err(EACCES)
        }
        if e.state==State::Charged {return Ok(e.charge_id)}
        if e.state==State::Settled {return Err(ESTALE)}
        if e.state!=State::ChargeUnknown {return Err(EBUSY)}
        let receipt=retry(e.client,owner.pid,e.kernel_token)?;
        if receipt==0 {return Err(ESTALE)}
        self.funded[idx].charge_id=receipt;
        self.funded[idx].state=State::Charged;
        Ok(receipt)
    }
    /// Record release only after a kernel-confirmed terminal refund. A
    /// timeout must leave the entry alive for exact-token reconciliation.
    pub(super) fn release(
        &mut self,owner:Owner,token:Token,kernel_sender_pid:u64,
        expected_client:Client,expected_session:u64,expected_kind:ProviderKind,
        mut refund:impl FnMut(u64,u64,u64)->Result<(),i32>,
    )->Result<(),i32> {
        if kernel_sender_pid!=owner.pid{return Err(EACCES)}
        let idx=self.find(owner,token).ok_or(ESTALE)?;
        let entry=self.funded[idx];
        if entry.client!=expected_client || entry.session!=expected_session
            || entry.kind!=expected_kind {return Err(EACCES)}
        match entry.state {
            State::Settled => return Ok(()),
            State::Charged|State::ReleaseUnknown=>{},
            _=>return Err(EBUSY),
        }
        self.funded[idx].state=State::ReleaseUnknown;
        refund(entry.client.job,owner.pid,entry.kernel_token)?;
        // A successful refund is not the same as a delivered rootd reply.
        // Keep the tombstone until provider exit, so retried releases receive
        // the same success and the old token cannot be re-charged.
        self.funded[idx].state=State::Settled;
        Ok(())
    }
    /// Cancellation of an uncertain CHARGE is NOT another CHARGE. Kernel
    /// RELEASE with no expected JobId fences the original rootd-owned token
    /// even if the original charge request never reached the kernel. It also
    /// refunds an already committed charge without needing an ACKed JobId.
    pub(super) fn cancel(
        &mut self,owner:Owner,token:Token,sender_pid:u64,
        client_pid:u64,session:u64,kind:ProviderKind,
        mut release_any:impl FnMut(u64,u64)->Result<(),i32>,
    )->Result<(),i32>{
        if sender_pid!=owner.pid{return Err(EACCES)}
        let Some(i)=self.find(owner,token) else {
            // No original charge was dispatched through rootd (or an old
            // cancellation was ACK-compacted). Reserve this nonce in the
            // provider watermark, so an in-flight late CHARGE may not create
            // a new kernel token after cancellation was accepted.
            if self.watermark(owner).is_some_and(|high|token.nonce<=high){
                return Ok(())
            }
            self.admit_owner_nonce(owner,token.nonce)?;
            return Ok(())
        };
        let e=self.funded[i];
        if e.client.pid!=client_pid||e.session!=session||e.kind!=kind{
            return Err(EACCES)
        }
        if e.state==State::Settled{return Ok(())}
        if !matches!(e.state,State::ChargeUnknown|State::Charged|
            State::ReleaseUnknown){return Err(EBUSY)}
        self.funded[i].state=State::ReleaseUnknown;
        release_any(owner.pid,e.kernel_token)?;
        self.funded[i].state=State::Settled;
        Ok(())
    }

    /// An exact provider ACK is the only point at which rootd may compact a
    /// settled receipt. The owner watermark survives compaction.
    pub(super) fn acknowledge(
        &mut self,owner:Owner,token:Token,sender_pid:u64,
        client_pid:u64,session:u64,kind:ProviderKind,
        job:u64,generation:u32,
    )->Result<(),i32>{
        if sender_pid!=owner.pid{return Err(EACCES)}
        let Some(i)=self.find(owner,token) else {
            // Only the same owner can acknowledge its already-compacted
            // receipt. The sender authentication has already completed.
            return if self.watermark(owner).is_some_and(|high|
                token.nonce<=high){Ok(())} else {Err(ESTALE)}
        };
        let e=self.funded[i];
        if e.client.pid!=client_pid||e.session!=session||e.kind!=kind{
            return Err(EACCES)
        }
        if job!=0 && (job!=e.client.job || generation!=e.client.job_generation){
            return Err(EACCES)
        }
        if e.state!=State::Settled{return Err(EBUSY)}
        self.funded.swap_remove(i);
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
        self.owner_watermarks.retain(|(o,_)|*o!=owner);
        Ok(before-self.funded.len())
    }
    /// The rootd supervisor passes only a kernel-stamped lifecycle EXIT
    /// for a confirmed netd/vfsd lease. The kernel is authoritative for
    /// cleanup even when the provider's last IPC reply was lost.
    pub(super) fn owner_exit_pid(
        &mut self,pid:u64,
        mut kernel_drain:impl FnMut(u64)->Result<(),i32>,
    )->Result<usize,i32> {
        if pid==0{return Err(EINVAL)}
        let has_provider=self.funded.iter().any(|e|e.provider.pid==pid);
        if !has_provider{
            self.owner_watermarks.retain(|(o,_)|o.pid!=pid);
            return Ok(0)
        }
        kernel_drain(pid)?;
        let before=self.funded.len();
        self.funded.retain(|e|e.provider.pid!=pid);
        self.owner_watermarks.retain(|(o,_)|o.pid!=pid);
        Ok(before-self.funded.len())
    }
    /// Rootd must not acknowledge a provider release merely because its
    /// local book no longer has an entry. An absent entry is ambiguous until
    /// the kernel's own token ledger confirms terminal nonexistence.
    pub(super) fn unknown_tokens_for_pid(&self,pid:u64)->usize {
        self.funded.iter().filter(|e|e.provider.pid==pid
            &&matches!(e.state,State::ChargeUnknown|State::ReleaseUnknown)).count()
    }
    pub(super) fn unresolved(&self,mut f:impl FnMut(Owner,Client,u64)) {
        for x in &self.funded {f(x.provider,x.client,x.kernel_token)}
    }
    pub(super) fn count(&self)->usize{self.funded.len()}
    pub(super) fn unsettled_count(&self)->usize{
        self.funded.iter().filter(|x|x.state!=State::Settled).count()
    }
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
        book.release(owner,ticket.token,8,client,1,ProviderKind::Socket,|job,p,t|{
            assert_eq!((job,p,t),(22,8,1));Ok(())
        }).unwrap();
        assert_eq!(book.count(),1);
        assert_eq!(book.unsettled_count(),0);
        // Exact retry is acknowledged without running the refund a second time.
        book.release(owner,ticket.token,8,client,1,ProviderKind::Socket,
            |_,_,_|panic!("cannot refund twice")).unwrap();
        assert_eq!(book.charge(ticket,8,true,|_,_,_|panic!("cannot revive token")),
            Err(ESTALE));
    }

    #[test]
    fn a_provider_exit_drains_before_forgetting_receipts(){
        let owner=Owner{service:3,pid:19,lease:1};
        let client=Client{pid:44,job:0x100000001,job_generation:1};
        let ticket=Ticket{provider:owner,client,kind:ProviderKind::Socket,
            session:2,token:Token{epoch:1,nonce:2}};
        let mut book=ProviderBilling::new();
        book.charge(ticket,19,true,|_,_,t|Ok(t)).unwrap();
        let mut drains=0;
        assert_eq!(book.owner_exit_pid(19,|pid|{
            assert_eq!(pid,19);drains+=1;Ok(())
        }),Ok(1));
        assert_eq!(drains,1);
        assert_eq!(book.count(),0);
    }
    #[test]
    fn uncertain_charge_reuses_same_kernel_token() {
        let owner=Owner{service:3,pid:51,lease:9};
        let client=Client{pid:71,job:0x100000003,job_generation:3};
        let token=Token{epoch:9,nonce:11};
        let ticket=Ticket{provider:owner,client,kind:ProviderKind::Socket,
            session:81,token};
        let mut book=ProviderBilling::new();
        let mut issued=0;
        assert_eq!(book.charge(ticket,51,true,|_,_,kernel_token|{
            issued=kernel_token;Err(110)
        }),Err(110));
        assert_eq!(book.unknown_tokens_for_pid(51),1);
        assert_eq!(book.reconcile_charge(owner,token,client,81,
            ProviderKind::Socket,51,|_,pid,kernel_token|{
            assert_eq!(pid,51);
            assert_eq!(kernel_token,issued);
            Ok(kernel_token)
        }),Ok(issued));
        assert_eq!(book.unknown_tokens_for_pid(51),0);
    }

    #[test]
    fn uncertain_refund_never_recharges_a_settled_ticket() {
        let owner=Owner{service:2,pid:62,lease:3};
        let client=Client{pid:80,job:0x200000004,job_generation:4};
        let token=Token{epoch:3,nonce:1};
        let ticket=Ticket{provider:owner,client,kind:ProviderKind::OpenDescription,
            session:47,token};
        let mut book=ProviderBilling::new();
        book.charge(ticket,62,true,|_,_,k|Ok(k)).unwrap();
        assert_eq!(book.release(owner,token,62,client,47,
            ProviderKind::OpenDescription,|_,_,_|Err(110)),Err(110));
        assert_eq!(book.unknown_tokens_for_pid(62),1);
        assert_eq!(book.release(owner,token,62,client,47,
            ProviderKind::OpenDescription,|_,_,_|Ok(())),Ok(()));
        assert_eq!(book.release(owner,token,62,client,47,
            ProviderKind::OpenDescription,|_,_,_|Err(5)),Ok(()));
        assert_eq!(book.unsettled_count(),0);
    }

    #[test]
    fn wrong_client_job_or_session_cannot_claim_refund() {
        let owner=Owner{service:3,pid:13,lease:10};
        let client=Client{pid:9,job:0x300000001,job_generation:1};
        let token=Token{epoch:10,nonce:4};
        let ticket=Ticket{provider:owner,client,kind:ProviderKind::Socket,
            session:71,token};
        let mut book=ProviderBilling::new();
        book.charge(ticket,13,true,|_,_,k|Ok(k)).unwrap();
        assert_eq!(book.release(owner,token,13,
            Client{pid:12,..client},71,ProviderKind::Socket,
            |_,_,_|panic!("foreign refund")),Err(EACCES));
        assert_eq!(book.release(owner,token,13,client,72,
            ProviderKind::Socket,|_,_,_|panic!("wrong session")),Err(EACCES));
        assert_eq!(book.unsettled_count(),1);
    }

    #[test]
    fn failed_kernel_drain_keeps_provider_receipts() {
        let owner=Owner{service:3,pid:300,lease:40};
        let client=Client{pid:4,job:0x100000001,job_generation:1};
        let token=Token{epoch:40,nonce:1};
        let ticket=Ticket{provider:owner,client,kind:ProviderKind::Socket,
            session:2,token};
        let mut book=ProviderBilling::new();
        book.charge(ticket,300,true,|_,_,k|Ok(k)).unwrap();
        assert_eq!(book.owner_exit_pid(300,|_|Err(5)),Err(5));
        assert_eq!(book.count(),1);
        assert_eq!(book.owner_exit_pid(300,|_|Ok(())),Ok(1));
        assert_eq!(book.count(),0);
    }

    #[test]
    fn cancel_undelivered_charge_fences_late_same_nonce(){
        let owner=Owner{service:3,pid:42,lease:1};
        let client=Client{pid:10,job:0x100000001,job_generation:1};
        let token=Token{epoch:1,nonce:5};
        let ticket=Ticket{provider:owner,client,kind:ProviderKind::Socket,
            session:8,token};
        let mut book=ProviderBilling::new();
        book.cancel(owner,token,42,10,8,ProviderKind::Socket,
            |_,_|panic!("absent charge must not require refund")).unwrap();
        assert_eq!(book.charge(ticket,42,true,|_,_,_|panic!("late charge")),
            Err(ESTALE));
        book.acknowledge(owner,token,42,10,8,ProviderKind::Socket,0,0).unwrap();
    }

    #[test]
    fn ack_compaction_preserves_the_spent_nonce_fence(){
        let owner=Owner{service:2,pid:62,lease:6};
        let client=Client{pid:7,job:0x200000001,job_generation:1};
        let token=Token{epoch:6,nonce:13};
        let ticket=Ticket{provider:owner,client,kind:ProviderKind::OpenDescription,
            session:22,token};
        let mut b=ProviderBilling::new();
        b.charge(ticket,62,true,|_,_,t|Ok(t)).unwrap();
        b.release(owner,token,62,client,22,ProviderKind::OpenDescription,
            |_,_,_|Ok(())).unwrap();
        b.acknowledge(owner,token,62,7,22,ProviderKind::OpenDescription,
            client.job,client.job_generation).unwrap();
        assert_eq!(b.count(),0);
        assert_eq!(b.charge(ticket,62,true,|_,_,_|panic!("double charge")),
            Err(ESTALE));
        assert_eq!(b.owner_exit_pid(62,|_|Ok(())),Ok(0));
    }

}
