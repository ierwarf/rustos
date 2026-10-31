//! netd slow-path per-socket client Job accounting adapter.
//!
//! This adapter is staged and imported, but the actual socket open/final
//! close dispatch is not yet cut over: socketpair, accept, listen retirement,
//! DVM publication and borrowed refs all need one coordinated call-site
//! migration. It must not take NetState while performing rootd IPC.
use std::sync::{Mutex,OnceLock};
use rustos_svc_runtime::provider_client::{self,ProviderClient,Receipt,State,Error};
use rustos_component_model::provider_wire::Frame;
use rustos_user_abi::syscall::IPC_SERVICE_NETD;
const RETRIES:usize=3;
static CLIENT:OnceLock<Result<Mutex<ProviderClient>,i32>>=OnceLock::new();
fn ledger()->Result<&'static Mutex<ProviderClient>,i32>{
    match CLIENT.get_or_init(||{
        let pid=unsafe{libc::syscall(libc::SYS_getpid)} as i64;
        let epoch=rustos_svc_runtime::syscall::monotonic_nanos();
        if pid<=0 || epoch==0{return Err(libc::ESRCH)}
        ProviderClient::new(IPC_SERVICE_NETD,pid as u64,epoch)
            .map(Mutex::new).map_err(|_|libc::EINVAL)
    }){
        Ok(book)=>Ok(book),Err(err)=>Err(*err),
    }
}
fn errno(e:Error)->i32{
    match e {
        Error::Capacity=>libc::ENOSPC,Error::Missing=>libc::ENOENT,
        Error::Existing=>libc::EEXIST,Error::Phase=>libc::EBUSY,
        Error::Overflow=>libc::EOVERFLOW,Error::WrongAck=>libc::EPROTO,
        Error::Kernel(err)=>err,Error::Uncertain=>libc::EAGAIN,
        Error::Invalid=>libc::EINVAL,
    }
}
fn lock()->Result<std::sync::MutexGuard<'static,ProviderClient>,i32>{
    ledger()?.lock().map_err(|_|libc::EIO)
}
fn rpc_retry(request:Frame)->Result<Frame,i32>{
    let mut last=libc::ETIMEDOUT;
    for _ in 0..RETRIES {
        match provider_client::call_rootd(request){
            Ok(value)=>return Ok(value),
            Err(err) if matches!(err,libc::ETIMEDOUT|libc::EINTR|
                libc::EAGAIN|libc::ENOMEM|libc::EPROTO)=>{
                last=err;
            }
            Err(err)=>return Err(err),
        }
    }
    Err(last)
}
pub(super) fn charge_before_publish(token:u64,client_pid:u64)
    ->Result<Receipt,i32>{
    // Prepare the request before any rootd IPC. The same token must survive
    // an ambiguous ACK. This function is never called under NetState lock.
    let req={
        let mut book=lock()?;
        match book.begin_charge(token,client_pid){
            Ok(_)=>{}
            Err(Error::Existing)=>{}
            Err(err)=>return Err(errno(err)),
        }
        book.dispatch_charge(token).map_err(errno)?
    };
    let ack=rpc_retry(req)?;
    let mut book=lock()?;
    book.charged(token,ack).map_err(errno)
}
pub(super) fn release_after_final_close(token:u64)->Result<(),i32>{
    let req={
        let mut book=lock()?;
        match book.begin_release(token){
            Ok(_)=>{},
            Err(Error::Phase)=>{
                if matches!(book.pending(token),Ok((State::ReleaseInFlight,_))
                  |Ok((State::ReleasePrepared,_))) {
                } else {return Err(libc::EBUSY)}
            }
            Err(err)=>return Err(errno(err)),
        }
        book.dispatch_release(token).map_err(errno)?
    };
    let ack=rpc_retry(req)?;
    let mut book=lock()?;
    book.released(token,ack).map_err(errno)
}
pub(super) fn recheck_ambiguous(token:u64)->Result<(),i32>{
    let (state,_)={lock()?.pending(token).map_err(errno)?};
    match state {
        State::ChargeInFlight=>{
            let req={lock()?.dispatch_charge(token).map_err(errno)?};
            let ack=rpc_retry(req)?;
            lock()?.charged(token,ack).map_err(errno)?;
            Ok(())
        }
        State::ReleaseInFlight|State::ReleasePrepared=>
            release_after_final_close(token),
        State::Funded=>Ok(()),
        State::ChargePrepared=>Err(libc::EAGAIN),
    }
}
pub(super) fn unsettled(mut each:impl FnMut(u64,State,Frame))->Result<(),i32>{
    lock()?.unresolved(|token,state,frame|each(token,state,frame));
    Ok(())
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn unexpected_local_phase_never_claims_refund(){
        assert_eq!(errno(Error::WrongAck),libc::EPROTO);
        assert_eq!(errno(Error::Uncertain),libc::EAGAIN);
    }
}
