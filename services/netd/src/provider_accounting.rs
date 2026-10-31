//! netd slow-path per-socket client Job accounting adapter.
//!
//! Socket publication/last-close are now wired. An independent pending
//! refund worker resolves ambiguous ACKs without extending kernel socket
//! lifetime. More fault injection is required at broker publication and
//! queued accept/cancellation boundaries. No NetState lock crosses rootd IPC.
use std::sync::{Mutex,OnceLock};
use std::collections::VecDeque;
use std::thread;
use std::time::Duration;
use rustos_svc_runtime::provider_client::{self,ProviderClient,Receipt,State,Error};
use rustos_component_model::provider_wire::Frame;
use rustos_user_abi::syscall::IPC_SERVICE_NETD;
const RETRIES:usize=3;
static CLIENT:OnceLock<Result<Mutex<ProviderClient>,i32>>=OnceLock::new();
// A close is logically final once the provider has withdrawn the object.
// Ambiguous kernel accounting is retained as an exact token and refunded by
// the control worker, not reported as a retryable socket close that could
// decrement a newly reused file descriptor.
const MAX_PENDING_REFUNDS:usize=4096;
static PENDING_REFUNDS:OnceLock<Mutex<VecDeque<u64>>>=OnceLock::new();
static REFUND_WORKER:OnceLock<()>=OnceLock::new();
fn pending_refunds()->&'static Mutex<VecDeque<u64>> {
    PENDING_REFUNDS.get_or_init(||Mutex::new(VecDeque::new()))
}
fn start_refund_worker() {
    REFUND_WORKER.get_or_init(||{
        thread::Builder::new()
            .name("netd-session-refund".into())
            .spawn(refund_worker)
            .expect("netd: refund worker required for exact session custody");
    });
}
fn enqueue_refund(token:u64)->Result<(),i32>{
    if token==0 {return Err(libc::EINVAL)}
    let mut q=pending_refunds().lock().map_err(|_|libc::EIO)?;
    if q.contains(&token){return Ok(())}
    if q.len()>=MAX_PENDING_REFUNDS{return Err(libc::ENOSPC)}
    q.push_back(token);
    Ok(())
}
fn refund_worker() {
    loop {
        let token=pending_refunds().lock().ok()
            .and_then(|mut q|q.pop_front());
        match token {
            Some(token)=>{
                // A lost CHARGE ACK must be reconciled before releasing
                // the same kernel token. Never use a second charge ID.
                let success=release_if_funded(token).is_ok();
                if !success {
                    // Retry the exact token. Do not invent an errno-success
                    // state merely because the socket itself is already gone.
                    let _=enqueue_refund(token);
                    thread::sleep(Duration::from_millis(10));
                }
            }
            None=>thread::sleep(Duration::from_millis(20)),
        }
    }
}
/// Retire an already withdrawn socket identity. When rootd's control IPC
/// returns an ambiguous result, retain the exact provider token while the
/// local socket close continues to report its independently settled result.
pub(super) fn retire_final(token:u64)->Result<(),i32>{
    match release_if_funded(token){
        Ok(())=>Ok(()),
        Err(_err)=>{
            start_refund_worker();
            enqueue_refund(token)
        }
    }
}

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
    start_refund_worker();
    // Prepare the request before any rootd IPC. The same token must survive
    // an ambiguous ACK. This function is never called under NetState lock.
    let req={
        let mut book=lock()?;
        match book.begin_charge(token,client_pid){
            Ok(_)=>{}
            Err(Error::Existing)=>{
                let (_,prior)=book.pending(token).map_err(errno)?;
                if prior.client_pid!=client_pid{return Err(libc::EACCES)}
            }
            Err(err)=>return Err(errno(err)),
        }
        book.dispatch_charge(token).map_err(errno)?
    };
    let ack=rpc_retry(req)?;
    let mut book=lock()?;
    book.charged(token,ack).map_err(errno)
}
pub(super) fn release_if_funded(token:u64)->Result<(),i32>{
    let phase={
        let book=lock()?;
        match book.pending(token) {
            Ok((state,_))=>state,
            Err(Error::Missing)=>return Ok(()),
            Err(err)=>return Err(errno(err)),
        }
    };
    match phase {
        State::Funded|State::ReleasePrepared|State::ReleaseInFlight=>
            release_after_final_close(token),
        State::ChargePrepared|State::ChargeInFlight|
        State::CancelPrepared|State::CancelInFlight=>
            cancel_unpublished(token),
    }
}
/// An unpublished object cannot go back through CHARGE reconciliation:
/// CANCEL is valid with no client Job receipt and fences a charge that
/// arrived before an ACK was lost.
fn cancel_unpublished(token:u64)->Result<(),i32>{
    let req={
        let mut book=lock()?;
        book.begin_cancel(token).map_err(errno)?;
        book.dispatch_cancel(token).map_err(errno)?
    };
    let ack=rpc_retry(req)?;
    {
        let mut book=lock()?;
        book.cancelled(token,ack).map_err(errno)?;
    }
    let _=provider_client::acknowledge_receipt(ack);
    Ok(())
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
    {
        let mut book=lock()?;
        book.released(token,ack).map_err(errno)?;
    }
    // On ACK loss the rootd tombstone stays, but the kernel refund is final.
    // Tombstone cleanup is best effort, never another socket close.
    let _=provider_client::acknowledge_receipt(ack);
    Ok(())
}
pub(super) fn recheck_ambiguous(token:u64)->Result<(),i32>{
    let (state,_)={lock()?.pending(token).map_err(errno)?};
    match state {
        State::ChargeInFlight|State::ChargePrepared|
        State::CancelInFlight|State::CancelPrepared=>cancel_unpublished(token),
        State::ReleaseInFlight|State::ReleasePrepared=>
            release_after_final_close(token),
        State::Funded=>release_after_final_close(token),
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
