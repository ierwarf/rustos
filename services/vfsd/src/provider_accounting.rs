//! vfsd open-description client Job billing, no_std Ring3 receive owner.
//!
//! Only the VfsState service receiver mutates this ledger. The kernel remains
//! authoritative for client PID -> generational Job ID, session charge, and
//! provider-exit refunds. This module never borrows the VFS storage lock.
//! No application or driver is permitted to supply a trusted Job ID.
use super::*;
use rustos_svc_runtime::provider_client::{
    ProviderClient, Receipt, State, Error, call_rootd,
};
use rustos_component_model::provider_wire::Frame;
use rustos_user_abi::syscall::IPC_SERVICE_VFSD;
const EPROTO:i32=71;
const EBUSY:i32=16;
const EAGAIN:i32=11;
const MAX_RETRIES:usize=3;

fn errno(error:Error)->i32{
    match error {
        Error::Invalid=>22,Error::Capacity=>28,
        Error::Existing=>17,Error::Missing=>2,
        Error::Phase=>EBUSY,Error::WrongAck=>EPROTO,
        Error::Uncertain=>EAGAIN,Error::Overflow=>75,
        Error::Kernel(code)=>code,
    }
}
fn control_retry(frame:Frame)->Result<Frame,i32>{
    let mut latest=EAGAIN;
    for _ in 0..MAX_RETRIES {
        match call_rootd(frame) {
            Ok(ack)=>return Ok(ack),
            Err(err) if matches!(err,11|12|4|71|110) =>latest=err,
            Err(err)=>return Err(err),
        }
    }
    Err(latest)
}
pub(super) struct OpenDescriptionBilling {
    ledger:ProviderClient,
}
impl OpenDescriptionBilling {
    pub(super) fn new(pid:u64,epoch:u64)->Result<Self,i32>{
        Ok(Self{ledger:ProviderClient::new(IPC_SERVICE_VFSD,pid,epoch)
            .map_err(errno)?})
    }
    fn ensure_open(&mut self,remote_id:u64,client_pid:u64)
        ->Result<Receipt,i32>{
        if remote_id==0 || client_pid==0 {return Err(22)}
        match self.ledger.begin_charge(remote_id,client_pid) {
            Ok(_) => {},
            Err(Error::Existing)=>{
                let (state,prior)=self.ledger.pending(remote_id).map_err(errno)?;
                if prior.client_pid!=client_pid {return Err(13)}
                if state==State::Funded {return self.ledger.receipt(remote_id).map_err(errno)}
                if !matches!(state,State::ChargePrepared|State::ChargeInFlight){
                    return Err(EBUSY)
                }
            }
            Err(err)=>return Err(errno(err)),
        }
        // The only cross-service IPC is between state transitions. There is
        // no borrowed BTreeMap or storage lock in the call itself.
        let request=self.ledger.dispatch_charge(remote_id).map_err(errno)?;
        let ack=control_retry(request)?;
        self.ledger.charged(remote_id,ack).map_err(errno)
    }
    fn ensure_cancel(&mut self,remote_id:u64)->Result<(),i32>{
        let phase=match self.ledger.pending(remote_id){
            Ok((phase,_))=>phase,
            Err(Error::Missing)=>return Ok(()),
            Err(err)=>return Err(errno(err)),
        };
        if matches!(phase,State::ReleasePrepared|State::ReleaseInFlight){
            return self.ensure_release(remote_id)
        }
        let request=self.ledger.begin_cancel(remote_id).map_err(errno)?;
        let request=self.ledger.dispatch_cancel(remote_id).map_err(errno)?;
        let ack=control_retry(request)?;
        self.ledger.cancelled(remote_id,ack).map_err(errno)?;
        let _=rustos_svc_runtime::provider_client::acknowledge_receipt(ack);
        Ok(())
    }
    fn ensure_release(&mut self,remote_id:u64)->Result<(),i32>{
        let state=match self.ledger.pending(remote_id) {
            Ok((state,_))=>state,
            // A replayed legacy handle is not automatically a client debit.
            Err(Error::Missing)=>return Ok(()),
            Err(err)=>return Err(errno(err)),
        };
        if matches!(state,State::ChargePrepared|State::ChargeInFlight|
            State::CancelPrepared|State::CancelInFlight){
            return self.ensure_cancel(remote_id)
        }
        if self.ledger.pending(remote_id).map_err(errno)?.0==State::Funded {
            self.ledger.begin_release(remote_id).map_err(errno)?;
        }
        let request=self.ledger.dispatch_release(remote_id).map_err(errno)?;
        let ack=control_retry(request)?;
        self.ledger.released(remote_id,ack).map_err(errno)?;
        let _=rustos_svc_runtime::provider_client::acknowledge_receipt(ack);
        Ok(())
    }
    fn owns_funded(&self,remote_id:u64,client_pid:u64)->bool{
        self.ledger.pending(remote_id).ok().is_some_and(|(state,frame)|{
            state==State::Funded && frame.client_pid==client_pid
        })
    }
    pub(super) fn live(&self)->usize{self.ledger.live_count()}
    pub(super) fn pending(&self)->usize{self.ledger.pending_count()}
}
impl VfsState {
    /// Lazily construct the provider control epoch after checkpoint replay,
    /// avoiding a rootd call during the VFS endpoint registration phase.
    fn billing_mut(&mut self)->Result<&mut OpenDescriptionBilling,i32>{
        if self.billing.is_none(){
            let pid=unsafe{
                rustos_svc_runtime::syscall::syscall0(linux_abi::SYS_GETPID)
            };
            if pid<=0{return Err(3)}
            let epoch=monotonic_nanos();
            if epoch==0{return Err(EAGAIN)}
            self.billing=Some(OpenDescriptionBilling::new(pid as u64,epoch)?);
        }
        self.billing.as_mut().ok_or(12)
    }
    pub(super) fn bill_open_description(
        &mut self,remote_id:u64,client_pid:u64)->Result<(),i32>{
        self.billing_mut()?.ensure_open(remote_id,client_pid).map(|_|())
    }
    pub(super) fn cancel_unpublished_open_description(
        &mut self,remote_id:u64)->Result<(),i32>{
        match self.billing.as_mut(){
            Some(billing)=>billing.ensure_cancel(remote_id),
            None=>Ok(()),
        }
    }
    pub(super) fn refund_open_description(
        &mut self,remote_id:u64)->Result<(),i32>{
        match self.billing.as_mut(){
            Some(billing)=>billing.ensure_release(remote_id),
            None=>Ok(()),
        }
    }
    /// Snapshot may inform health reporting and resource management,
    /// but unverified counts do not bypass the kernel's Job quota.
    pub(super) fn has_funded_open_description(&self,id:u64,pid:u64)->bool {
        self.billing.as_ref().is_some_and(|b|b.owns_funded(id,pid))
    }
    pub(super) fn billing_snapshot(&self)->(usize,usize){
        self.billing.as_ref().map_or((0,0),|b|(b.live(),b.pending()))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vfs_provider_custody_uses_exact_generational_tickets(){
        let mut book=OpenDescriptionBilling::new(50,7).unwrap();
        let request=book.ledger.begin_charge(100,300).unwrap();
        assert_eq!(request.provider_service,IPC_SERVICE_VFSD);
        assert_eq!(request.client_pid,300);
        assert_eq!(book.ledger.begin_charge(100,999),Err(Error::Existing));
        let same=book.ledger.dispatch_charge(100).unwrap();
        assert_eq!(same,request);
        assert_eq!(book.ledger.pending_count(),1);
    }
}
