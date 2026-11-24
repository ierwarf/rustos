//! Slow provider-session client: shared by netd/vfsd and future services.
//!
//! Use BEGIN -> drop the local lock -> IPC -> FINISH. In particular no NetState,
//! filesystem object or provider-session bookkeeping mutex may be held over
//! the synchronous rootd call. It must not run from a kernel hot path.
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64,Ordering};
use rustos_component_model::provider_wire::{self, Frame, Operation, Phase};
// Native x86_64 Linux ABI syscall numbers, also valid without optional
// rustos-user-abi linux-compat-state feature (svc-runtime is no_std).
const SYS_GETPID: u64 = 39;
const SYS_GETTID: u64 = 186;
use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest,CommercialMaxProtocolResponse,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_PROVIDER_SESSION_CHARGE,
    COMMERCIAL_MAX_ROOTD_OP_PROVIDER_SESSION_RELEASE,
    COMMERCIAL_MAX_ROOTD_OP_PROVIDER_SESSION_CANCEL,
    COMMERCIAL_MAX_ROOTD_OP_PROVIDER_RECEIPT_ACK,
    COMMERCIAL_MAX_ROOTD_OP_PROVIDER_RECEIPT_ACK_BATCH,
    PROVIDER_RECEIPT_ACK_BATCH_MAX_FRAMES,
    IPC_SERVICE_ROOTD,SYS_RUSTOS_IPC_CALL_BOUNDED,
};
use crate::{ipc,syscall};
const EINVAL:i32=22;const EPROTO:i32=71;const ETIMEDOUT:i32=110;

pub const MAX_LOCAL_SESSIONS:usize=4096;
/// Terminal receipts awaiting rootd compaction. Losing the oldest one only
/// leaves a rootd tombstone (reclaimed under pressure); it never un-spends a
/// token, so overflow evicts instead of blocking a provider.
pub const MAX_ACK_OUTBOX:usize=MAX_LOCAL_SESSIONS;
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum State{ChargePrepared,ChargeInFlight,Funded,
    ReleasePrepared,ReleaseInFlight,CancelPrepared,CancelInFlight}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub struct Receipt{
    pub client_pid:u64,pub client_job:u64,
    pub job_generation:u32,pub kernel_receipt:u64,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
struct Session {
    frame:Frame,state:State,
}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error{
    Invalid,Capacity,Existing,Missing,Phase,Overflow,
    WrongAck,Kernel(i32),Uncertain,
}
pub struct ProviderClient {
    service:u64, epoch:u64,pid:u64,
    next_request_id:u64,next_nonce:u64,
    sessions:BTreeMap<u64,Session>,
    // Committed Release/Cancel frames (Phase::Accepted) keyed by (token epoch,
    // token nonce): `rotate_epoch` restarts nonces, so the nonce alone would let
    // a new epoch overwrite a still-unacknowledged receipt of the previous one.
    ack_outbox:BTreeMap<(u64,u64),Frame>,
}
impl ProviderClient {
    pub fn new(service:u64,pid:u64,epoch:u64)->Result<Self,Error>{
        if service==0||pid==0||epoch==0{return Err(Error::Invalid)}
        Ok(Self{service,epoch,pid,next_request_id:0,
            next_nonce:0,sessions:BTreeMap::new(),
            ack_outbox:BTreeMap::new()})
    }
    pub fn live_count(&self)->usize{self.sessions.len()}
    pub fn epoch(&self)->u64{self.epoch}
    pub fn begin_charge(&mut self,session:u64,client_pid:u64)->Result<Frame,Error>{
        if session==0||client_pid==0{return Err(Error::Invalid)}
        if self.sessions.contains_key(&session){return Err(Error::Existing)}
        if self.sessions.len()>=MAX_LOCAL_SESSIONS{return Err(Error::Capacity)}
        let request=self.next_request_id.checked_add(1).ok_or(Error::Overflow)?;
        let nonce=self.next_nonce.checked_add(1).ok_or(Error::Overflow)?;
        // Prepare memory and request identity before the first RPC. A lost
        // reply cannot cause a second nonce or a different Job charge.
        // BTreeMap has no fallible reservation API. A later cutover can
        // replace it with a fixed-capacity slab for ENOMEM guarantees.
        let frame=Frame{
            operation:Operation::Charge,phase:Phase::Request,
            request_id:request,provider_service:self.service,
            provider_pid:self.pid,provider_lease:self.epoch,
            client_pid,client_job:0,client_job_generation:0,
            session,token_epoch:self.epoch,token_nonce:nonce,
            kernel_receipt:0,status:0,
        };
        if !frame.valid(){return Err(Error::Invalid)}
        self.sessions.insert(session,Session{
            frame,state:State::ChargePrepared
        });
        self.next_request_id=request;self.next_nonce=nonce;
        Ok(frame)
    }
    /// Mark uncertainty BEFORE making the cross-service call. The returned
    /// frame is reused exactly on retries and is never regenerated.
    pub fn dispatch_charge(&mut self,session:u64)->Result<Frame,Error>{
        let e=self.sessions.get_mut(&session).ok_or(Error::Missing)?;
        if !matches!(e.state,State::ChargePrepared|State::ChargeInFlight){
            return Err(Error::Phase)
        }
        e.state=State::ChargeInFlight;
        Ok(e.frame)
    }
    pub fn charged(&mut self,session:u64,ack:Frame)->Result<Receipt,Error>{
        let e=self.sessions.get_mut(&session).ok_or(Error::Missing)?;
        if e.state!=State::ChargeInFlight{return Err(Error::Phase)}
        ack.validate_ack(e.frame).map_err(|_|Error::WrongAck)?;
        if ack.phase!=Phase::Committed||ack.status!=0||
            ack.kernel_receipt==0{return Err(Error::WrongAck)}
        let receipt=Receipt{client_pid:e.frame.client_pid,
            client_job:ack.client_job,job_generation:ack.client_job_generation,
            kernel_receipt:ack.kernel_receipt};
        e.frame.client_job=ack.client_job;
        e.frame.client_job_generation=ack.client_job_generation;
        e.state=State::Funded;
        Ok(receipt)
    }
    /// Convert a failed or ambiguous CHARGE into a jobless CANCEL. The
    /// provider token and original session identity stay unchanged, so rootd
    /// can fence a late charge even if the original reply never arrived.
    pub fn begin_cancel(&mut self,session:u64)->Result<Frame,Error>{
        let next=self.next_request_id.checked_add(1).ok_or(Error::Overflow)?;
        let entry=self.sessions.get_mut(&session).ok_or(Error::Missing)?;
        if matches!(entry.state,State::CancelPrepared|State::CancelInFlight){
            return Ok(entry.frame)
        }
        if !matches!(entry.state,State::ChargePrepared|State::ChargeInFlight|
            State::Funded){return Err(Error::Phase)}
        entry.frame.operation=Operation::Cancel;
        entry.frame.phase=Phase::Request;
        entry.frame.request_id=next;
        entry.frame.client_job=0;
        entry.frame.client_job_generation=0;
        entry.frame.kernel_receipt=0;
        entry.frame.status=0;
        entry.state=State::CancelPrepared;
        self.next_request_id=next;
        Ok(entry.frame)
    }
    pub fn dispatch_cancel(&mut self,session:u64)->Result<Frame,Error>{
        let entry=self.sessions.get_mut(&session).ok_or(Error::Missing)?;
        if !matches!(entry.state,State::CancelPrepared|State::CancelInFlight){
            return Err(Error::Phase)
        }
        entry.state=State::CancelInFlight;
        Ok(entry.frame)
    }
    pub fn cancelled(&mut self,session:u64,ack:Frame)->Result<(),Error>{
        let entry=self.sessions.get(&session).ok_or(Error::Missing)?;
        if entry.state!=State::CancelInFlight{return Err(Error::Phase)}
        ack.validate_ack(entry.frame).map_err(|_|Error::WrongAck)?;
        if ack.phase!=Phase::Committed||ack.status!=0
            ||ack.kernel_receipt!=0{return Err(Error::WrongAck)}
        self.sessions.remove(&session);
        self.queue_ack(ack);
        Ok(())
    }
    pub fn receipt(&self,session:u64)->Result<Receipt,Error>{
        let e=self.sessions.get(&session).ok_or(Error::Missing)?;
        if !matches!(e.state,State::Funded|State::ReleasePrepared|
            State::ReleaseInFlight){return Err(Error::Phase)}
        Ok(Receipt{client_pid:e.frame.client_pid,
            client_job:e.frame.client_job,
            job_generation:e.frame.client_job_generation,
            kernel_receipt:0})
    }
    /// After any uncertain refund, no subsequent charge can reuse this
    /// service-local session ID until terminal kernel custody is proven.
    pub fn begin_release(&mut self,session:u64)->Result<Frame,Error>{
        let next_id=self.next_request_id.checked_add(1).ok_or(Error::Overflow)?;
        let e=self.sessions.get_mut(&session).ok_or(Error::Missing)?;
        if e.state!=State::Funded{return Err(Error::Phase)}
        e.frame.operation=Operation::Release;
        e.frame.phase=Phase::Request;
        e.frame.request_id=next_id;
        e.frame.kernel_receipt=0;e.frame.status=0;
        e.state=State::ReleasePrepared;
        self.next_request_id=next_id;
        Ok(e.frame)
    }
    pub fn dispatch_release(&mut self,session:u64)->Result<Frame,Error>{
        let e=self.sessions.get_mut(&session).ok_or(Error::Missing)?;
        if !matches!(e.state,State::ReleasePrepared|State::ReleaseInFlight){
            return Err(Error::Phase)
        }
        e.state=State::ReleaseInFlight;
        Ok(e.frame)
    }
    pub fn released(&mut self,session:u64,ack:Frame)->Result<(),Error>{
        let e=self.sessions.get(&session).ok_or(Error::Missing)?;
        if e.state!=State::ReleaseInFlight{return Err(Error::Phase)}
        ack.validate_ack(e.frame).map_err(|_|Error::WrongAck)?;
        if ack.phase!=Phase::Committed||ack.status!=0
            ||ack.kernel_receipt!=0{return Err(Error::WrongAck)}
        self.sessions.remove(&session);
        self.queue_ack(ack);
        Ok(())
    }
    fn queue_ack(&mut self,committed:Frame){
        if self.ack_outbox.len()>=MAX_ACK_OUTBOX{
            if let Some(oldest)=self.ack_outbox.keys().next().copied(){
                self.ack_outbox.remove(&oldest);
            }
        }
        let mut frame=committed;
        frame.phase=Phase::Accepted;
        self.ack_outbox.insert((frame.token_epoch,frame.token_nonce),frame);
    }
    /// Oldest terminal receipt not yet acknowledged to rootd. Retried until
    /// `ack_done`; a transport error leaves it queued (idempotent at rootd).
    pub fn next_ack(&self)->Option<Frame>{
        self.ack_outbox.values().next().copied()
    }
    pub fn ack_pending(&self)->usize{self.ack_outbox.len()}
    /// Retire an outbox entry only for rootd's exact Committed reply.
    pub fn ack_done(&mut self,sent:Frame,reply:Frame)->Result<(),Error>{
        let key=(sent.token_epoch,sent.token_nonce);
        let queued=self.ack_outbox.get(&key).copied()
            .ok_or(Error::Missing)?;
        if queued!=sent{return Err(Error::WrongAck)}
        reply.validate_ack(queued).map_err(|_|Error::WrongAck)?;
        if reply.phase!=Phase::Committed||reply.status!=0
            ||reply.kernel_receipt!=0{return Err(Error::WrongAck)}
        self.ack_outbox.remove(&key);
        Ok(())
    }
    /// Bounded drain for owners that hold the client by `&mut` (no shared
    /// lock): at most `limit` attempts, stops at the first transport failure.
    pub fn flush_acks(&mut self,limit:usize,
        mut send:impl FnMut(Frame)->Result<Frame,i32>)->usize{
        let mut done=0;
        while done<limit{
            let Some(frame)=self.next_ack() else {break};
            let Ok(reply)=send(frame) else {break};
            if self.ack_done(frame,reply).is_err(){break}
            done+=1;
        }
        done
    }
    /// Up to `max` oldest terminal receipts for one batched ACK call.
    pub fn next_ack_batch(&self,max:usize)->Vec<Frame>{
        self.ack_outbox.values().take(max).copied().collect()
    }
    /// Retire the frames of `sent` whose bit is set in `retired` (bit `i` =
    /// `sent[i]`). A clear bit, or a frame no longer queued byte-for-byte, is
    /// left alone; a bit beyond `sent.len()` is a protocol violation and
    /// retires nothing. Returns how many entries were removed.
    pub fn ack_batch_done(&mut self,sent:&[Frame],retired:u64)->Result<usize,Error>{
        if sent.len()>64||(sent.len()<64&&retired>>sent.len()!=0){
            return Err(Error::WrongAck)
        }
        let mut removed=0;
        for (i,frame) in sent.iter().enumerate(){
            if retired>>i&1==0{continue}
            let key=(frame.token_epoch,frame.token_nonce);
            if self.ack_outbox.get(&key)==Some(frame){
                self.ack_outbox.remove(&key);removed+=1;
            }
        }
        Ok(removed)
    }
    /// Deferred, batched drain. Receipt ACKs are rootd compaction only (see
    /// `MAX_ACK_OUTBOX`), so nothing needs them promptly: do nothing until at
    /// least `low_water` are queued, then send full batches of `max_batch`
    /// (one IPC each, at most `max_calls` per invocation to bound the time on
    /// the caller's request path). A transport error or a batch that retired
    /// nothing stops the drain and leaves every receipt queued; the next
    /// invocation retries the same frames. Returns receipts retired.
    pub fn flush_ack_batches(&mut self,low_water:usize,max_batch:usize,
        max_calls:usize,mut send:impl FnMut(&[Frame])->Result<u64,i32>)->usize{
        let max_batch=max_batch.clamp(1,PROVIDER_RECEIPT_ACK_BATCH_MAX_FRAMES);
        let mut done=0;
        for _ in 0..max_calls{
            if self.ack_outbox.len()<low_water.max(1){break}
            let batch=self.next_ack_batch(max_batch);
            let Ok(mask)=send(&batch) else {break};
            let Ok(n)=self.ack_batch_done(&batch,mask) else {break};
            if n==0{break}
            done+=n;
        }
        done
    }
    /// A failed or timed-out RPC never deletes a local token. The caller
    /// can retry exactly the same prepared frame, or fail-stop on shutdown.
    pub fn pending(&self,session:u64)->Result<(State,Frame),Error>{
        let e=self.sessions.get(&session).ok_or(Error::Missing)?;
        Ok((e.state,e.frame))
    }
    /// Not yet dispatched: no kernel side effect exists. Never call this
    /// on an in-flight request, even if a local timeout reports failure.
    pub fn abandon_unsent(&mut self,session:u64)->Result<(),Error>{
        let e=self.sessions.get(&session).ok_or(Error::Missing)?;
        if e.state!=State::ChargePrepared{return Err(Error::Uncertain)}
        self.sessions.remove(&session);Ok(())
    }
    /// The provider may rotate its owner epoch only after all kernel-owned
    /// tokens have been settled and every local receipt retired.
    pub fn rotate_epoch(&mut self,new_epoch:u64)->Result<(),Error>{
        if !self.sessions.is_empty(){return Err(Error::Uncertain)}
        if new_epoch<=self.epoch{return Err(Error::Invalid)}
        self.epoch=new_epoch;self.next_nonce=0;
        self.next_request_id=0;Ok(())
    }
    pub fn pending_count(&self)->usize {
        self.sessions.values().filter(|e|matches!(e.state,
            State::ChargeInFlight|State::ReleaseInFlight|
            State::ReleasePrepared|State::CancelPrepared|
            State::CancelInFlight)).count()
    }
    pub fn unresolved(&self,mut f:impl FnMut(u64,State,Frame)){
        for (id,e) in &self.sessions {
            if e.state!=State::Funded {f(*id,e.state,e.frame)}
        }
    }
}
/// Positive errno of a negative syscall result; an out-of-range value is a
/// protocol violation, never a wrapped or panicking negation.
fn errno_of(result:i64)->i32{
    result.checked_neg().and_then(|e|i32::try_from(e).ok()).unwrap_or(EPROTO)
}
/// Send one receipt-ACK outbox frame (Phase::Accepted) and return rootd's
/// reply. Failure merely leaves the frame queued: the rootd tombstone is
/// compaction state only and the nonce watermark already fences re-charging.
pub fn acknowledge_frame(accepted:Frame)->Result<Frame,i32>{
    if !matches!(accepted.operation,Operation::Cancel|Operation::Release)
        || accepted.phase!=Phase::Accepted || accepted.status!=0
        || accepted.kernel_receipt!=0 {return Err(EINVAL)}
    let ack=call_rootd(accepted)?;
    if ack.phase!=Phase::Committed || ack.status!=0 {return Err(EPROTO)}
    Ok(ack)
}
/// Cached rootd supervisor endpoint: a generational kernel handle, so a value
/// left over from a restarted rootd fails with an error instead of reaching
/// another object. Zero = unresolved. Cleared on any transport failure.
static ROOTD_ENDPOINT:AtomicU64=AtomicU64::new(0);
/// This process's PID never changes (service processes never `fork`).
static SELF_PID:AtomicU64=AtomicU64::new(0);
fn rootd_endpoint()->Result<(u64,bool),i32>{
    let cached=ROOTD_ENDPOINT.load(Ordering::Acquire);
    if cached!=0{return Ok((cached,true))}
    let endpoint=ipc::lookup_service_endpoint(IPC_SERVICE_ROOTD);
    if endpoint<=0{return Err(if endpoint==0{EPROTO}else{errno_of(endpoint)})}
    ROOTD_ENDPOINT.store(endpoint as u64,Ordering::Release);
    Ok((endpoint as u64,false))
}
fn self_pid()->Result<u64,i32>{
    let cached=SELF_PID.load(Ordering::Relaxed);
    if cached!=0{return Ok(cached)}
    let pid=unsafe{syscall::syscall0(SYS_GETPID)};
    if pid<=0{return Err(EINVAL)}
    SELF_PID.store(pid as u64,Ordering::Relaxed);
    Ok(pid as u64)
}
/// One synchronous rootd call for `op` with an already encoded payload.
/// `Ok` is a well-formed reply envelope (its `status` may still be non-zero);
/// `Err` is a transport failure after which nothing is known about whether
/// rootd applied the request.
///
/// A transport failure on the CACHED endpoint (typically a restarted rootd,
/// whose old handle is dead) invalidates it and is retried exactly once on a
/// fresh lookup, except for a timeout (rootd is alive but slow: another full
/// wait on the same request would double the caller's stall, and the caller
/// already owns the retry policy). The replay is safe because every op this
/// module sends is idempotent at rootd by `(owner, token)`, and callers
/// already resend the identical frame after any transport error:
///  * CHARGE: `book.charge` returns the prior receipt for an exact repeat of a
///    `Charged` token and `reconcile_charge` reuses the same kernel token for
///    `ChargeUnknown`; a spent nonce is fenced by the owner watermark.
///  * RELEASE / CANCEL: a `Settled` or absent token succeeds without a second
///    refund (`ProviderBilling::release` / `cancel`).
///  * RECEIPT_ACK (single and batch): compaction only; absent is success.
fn rootd_transact(req:&CommercialMaxProtocolRequest)
    ->Result<CommercialMaxProtocolResponse,i32>{
    for attempt in 0..2{
        let (endpoint,cached)=rootd_endpoint()?;
        let mut reply=CommercialMaxProtocolResponse::default();
        let got=unsafe{syscall::syscall6(
            SYS_RUSTOS_IPC_CALL_BOUNDED,endpoint,
            (req as *const CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut reply as *mut CommercialMaxProtocolResponse) as u64,
            size_of::<CommercialMaxProtocolResponse>() as u64,
            IPC_BOOT_CONTROL_HARD_LIMIT_MS,
        )};
        if got>=0&&got as usize==size_of::<CommercialMaxProtocolResponse>()
            &&reply.is_valid_envelope_for(req)&&reply.descriptor_count==0{
            return Ok(reply)
        }
        ROOTD_ENDPOINT.store(0,Ordering::Release);
        if attempt==0&&cached&&got<0&&errno_of(got)!=ETIMEDOUT{continue}
        return Err(if got<0{errno_of(got)}else{EPROTO})
    }
    Err(EPROTO)
}
fn rootd_request(op:u16,payload:&[u8])->Result<CommercialMaxProtocolRequest,i32>{
    let pid=self_pid()?;
    let tid=unsafe{syscall::syscall0(SYS_GETTID)};
    if tid<=0{return Err(EINVAL)}
    let mut req=CommercialMaxProtocolRequest::default();
    if payload.len()>req.payload.len(){return Err(EINVAL)}
    req.header.version=COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    req.header.protocol=COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    req.header.op=op;
    req.header.subject_pid=pid;
    req.header.subject_tid=tid as u64;
    req.payload_len=payload.len() as u32;
    req.payload[..payload.len()].copy_from_slice(payload);
    Ok(req)
}
/// Stateless transport; must be invoked AFTER releasing all local state
/// locks. Exact service identity and the client Job are checked by rootd.
pub fn call_rootd(frame:Frame)->Result<Frame,i32>{
    if !frame.valid(){return Err(EINVAL)}
    let op=match (frame.phase,frame.operation){
        (Phase::Request,Operation::Charge)=>
            COMMERCIAL_MAX_ROOTD_OP_PROVIDER_SESSION_CHARGE,
        (Phase::Request,Operation::Release)=>
            COMMERCIAL_MAX_ROOTD_OP_PROVIDER_SESSION_RELEASE,
        (Phase::Request,Operation::Cancel)=>
            COMMERCIAL_MAX_ROOTD_OP_PROVIDER_SESSION_CANCEL,
        (Phase::Accepted,Operation::Release|Operation::Cancel)=>
            COMMERCIAL_MAX_ROOTD_OP_PROVIDER_RECEIPT_ACK,
        _=>return Err(EINVAL),
    };
    if frame.provider_pid!=self_pid()?{return Err(EINVAL)}
    let bytes=provider_wire::encode(frame).map_err(|_|EINVAL)?;
    let req=rootd_request(op,&bytes)?;
    let reply=rootd_transact(&req)?;
    if reply.status!=0{return Err(reply.status)}
    if reply.value0!=0||reply.value1!=0{return Err(EPROTO)}
    if reply.payload_len as usize!=provider_wire::BYTES{return Err(EPROTO)}
    let ack=provider_wire::decode(&reply.payload[..provider_wire::BYTES])
        .map_err(|_|EPROTO)?;
    ack.validate_ack(frame).map_err(|_|EPROTO)?;
    Ok(ack)
}
/// Acknowledge up to `PROVIDER_RECEIPT_ACK_BATCH_MAX_FRAMES` queued receipts
/// in one rootd call. Returns the retired mask (bit `i` = `frames[i]`); a
/// transport or authorization failure retires nothing and leaves every frame
/// queued.
pub fn acknowledge_batch(frames:&[Frame])->Result<u64,i32>{
    let n=frames.len();
    if n==0||n>PROVIDER_RECEIPT_ACK_BATCH_MAX_FRAMES{return Err(EINVAL)}
    let pid=self_pid()?;
    let mut payload=Vec::with_capacity(n*provider_wire::BYTES);
    for f in frames{
        if !matches!(f.operation,Operation::Cancel|Operation::Release)
            ||f.phase!=Phase::Accepted||f.status!=0||f.kernel_receipt!=0
            ||f.provider_pid!=pid{return Err(EINVAL)}
        payload.extend_from_slice(&provider_wire::encode(*f).map_err(|_|EINVAL)?);
    }
    let req=rootd_request(COMMERCIAL_MAX_ROOTD_OP_PROVIDER_RECEIPT_ACK_BATCH,&payload)?;
    let reply=rootd_transact(&req)?;
    if reply.status!=0{return Err(reply.status)}
    if reply.payload_len!=0||reply.value1!=n as u64||reply.value0>>n!=0{
        return Err(EPROTO)
    }
    Ok(reply.value0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lost_charge_ack_reuses_identity_and_cannot_activate(){
        let mut b=ProviderClient::new(3,12,7).unwrap();
        let original=b.begin_charge(88,101).unwrap();
        assert_eq!(b.dispatch_charge(88),Ok(original));
        assert_eq!(b.dispatch_charge(88),Ok(original));
        assert_eq!(b.begin_release(88),Err(Error::Phase));
        assert_eq!(b.abandon_unsent(88),Err(Error::Uncertain));
        let ack=Frame{phase:Phase::Committed,client_job:0x100000002,
            client_job_generation:2,kernel_receipt:19,..original};
        b.charged(88,ack).unwrap();
        let release=b.begin_release(88).unwrap();
        assert_eq!(b.dispatch_release(88),Ok(release));
        assert_eq!(b.dispatch_release(88),Ok(release));
        let released=Frame{phase:Phase::Committed,..release};
        b.released(88,released).unwrap();
        assert_eq!(b.live_count(),0);
    }
    #[test]
    fn lost_receipt_ack_is_retained_and_retried_until_exact_commit(){
        let mut b=ProviderClient::new(2,88,15).unwrap();
        let charge=b.begin_charge(11,98).unwrap();
        b.dispatch_charge(11).unwrap();
        b.charged(11,Frame{phase:Phase::Committed,client_job:0x200000007,
            client_job_generation:7,kernel_receipt:3,..charge}).unwrap();
        let release=b.begin_release(11).unwrap();
        b.dispatch_release(11).unwrap();
        b.released(11,Frame{phase:Phase::Committed,..release}).unwrap();
        assert_eq!(b.live_count(),0);
        assert_eq!(b.ack_pending(),1);
        let queued=b.next_ack().unwrap();
        assert_eq!(queued.phase,Phase::Accepted);
        // Lost reply: transport error keeps the receipt queued.
        assert_eq!(b.flush_acks(4,|_|Err(110)),0);
        assert_eq!(b.ack_pending(),1);
        // A reply for a different nonce never retires it.
        let mut wrong=Frame{phase:Phase::Committed,..queued};
        wrong.token_nonce+=1;
        assert_eq!(b.flush_acks(4,|_|Ok(wrong)),0);
        assert_eq!(b.ack_pending(),1);
        assert_eq!(b.flush_acks(4,|f|Ok(Frame{phase:Phase::Committed,..f})),1);
        assert_eq!(b.ack_pending(),0);
    }
    #[test]
    fn cancel_receipts_queue_acks_and_overflow_evicts_oldest(){
        let mut b=ProviderClient::new(3,81,7).unwrap();
        for i in 0..(MAX_ACK_OUTBOX as u64+3){
            let s=i+1;
            b.begin_charge(s,70).unwrap();
            b.dispatch_charge(s).unwrap();
            let c=b.begin_cancel(s).unwrap();
            b.dispatch_cancel(s).unwrap();
            b.cancelled(s,Frame{phase:Phase::Committed,..c}).unwrap();
        }
        assert_eq!(b.ack_pending(),MAX_ACK_OUTBOX);
        assert_eq!(b.next_ack().unwrap().token_nonce,4);
    }
    #[test]
    fn epoch_rotation_does_not_overwrite_an_unacknowledged_receipt(){
        let mut b=ProviderClient::new(3,81,7).unwrap();
        let charge=b.begin_charge(1,70).unwrap();
        b.dispatch_charge(1).unwrap();
        let cancel=b.begin_cancel(1).unwrap();
        b.dispatch_cancel(1).unwrap();
        b.cancelled(1,Frame{phase:Phase::Committed,..cancel}).unwrap();
        b.rotate_epoch(8).unwrap();
        // Nonces restart at 1 in the new epoch and must not replace epoch 7's.
        let next=b.begin_charge(2,70).unwrap();
        assert_eq!(next.token_nonce,charge.token_nonce);
        b.dispatch_charge(2).unwrap();
        let cancel=b.begin_cancel(2).unwrap();
        b.dispatch_cancel(2).unwrap();
        b.cancelled(2,Frame{phase:Phase::Committed,..cancel}).unwrap();
        assert_eq!(b.ack_pending(),2);
        assert_eq!(b.next_ack().unwrap().token_epoch,7);
    }
    #[test]
    fn wrong_job_and_nonce_never_settle(){
        let mut b=ProviderClient::new(2,10,5).unwrap();
        let first=b.begin_charge(1,8).unwrap();
        b.dispatch_charge(1).unwrap();
        let bad=Frame{phase:Phase::Committed,client_job:0x100000001,
           client_job_generation:1,kernel_receipt:9,token_nonce:9,..first};
        assert_eq!(b.charged(1,bad),Err(Error::WrongAck));
        assert_eq!(b.rotate_epoch(6),Err(Error::Uncertain));
    }
    #[test]
    fn ambiguous_charge_cancel_preserves_original_provider_nonce(){
        let mut b=ProviderClient::new(3,81,7).unwrap();
        let charge=b.begin_charge(999,70).unwrap();
        b.dispatch_charge(999).unwrap();
        let cancel=b.begin_cancel(999).unwrap();
        assert_eq!(cancel.token_nonce,charge.token_nonce);
        assert_eq!(cancel.operation,Operation::Cancel);
        assert_eq!(cancel.client_job,0);
        assert_eq!(b.dispatch_cancel(999),Ok(cancel));
        assert_eq!(b.dispatch_cancel(999),Ok(cancel));
        let settled=Frame{phase:Phase::Committed,..cancel};
        b.cancelled(999,settled).unwrap();
        assert_eq!(b.live_count(),0);
    }
    #[test]
    fn refund_does_not_confuse_a_new_request_with_a_prior_ack(){
        let mut b=ProviderClient::new(2,88,15).unwrap();
        let charge=b.begin_charge(11,98).unwrap();
        b.dispatch_charge(11).unwrap();
        b.charged(11,Frame{phase:Phase::Committed,
            client_job:0x200000007,client_job_generation:7,
            kernel_receipt:3,..charge}).unwrap();
        let release=b.begin_release(11).unwrap();
        b.dispatch_release(11).unwrap();
        let mut wrong=Frame{phase:Phase::Committed,..release};
        wrong.token_nonce+=1;
        assert_eq!(b.released(11,wrong),Err(Error::WrongAck));
        b.released(11,Frame{phase:Phase::Committed,..release}).unwrap();
        assert_eq!(b.live_count(),0);
    }

    fn settle_one(b:&mut ProviderClient,session:u64){
        let c=b.begin_charge(session,98).unwrap();
        b.dispatch_charge(session).unwrap();
        b.charged(session,Frame{phase:Phase::Committed,client_job:0x200000007,
            client_job_generation:7,kernel_receipt:3,..c}).unwrap();
        let r=b.begin_release(session).unwrap();
        b.dispatch_release(session).unwrap();
        b.released(session,Frame{phase:Phase::Committed,..r}).unwrap();
    }
    #[test]
    fn ack_batches_are_deferred_until_low_water_and_cost_one_call_per_batch(){
        let mut b=ProviderClient::new(2,88,15).unwrap();
        let mut calls=0;
        for s in 1..=40u64{
            settle_one(&mut b,s);
            let n=b.flush_ack_batches(16,16,2,|f|{
                calls+=1;Ok((1u64<<f.len())-1)
            });
            // Nothing is sent below the low-water mark.
            if s<16{assert_eq!((calls,n),(0,0))}
        }
        // 40 receipts, one call per 16: 2 calls retire 32, 8 stay queued.
        assert_eq!(calls,2);
        assert_eq!(b.ack_pending(),8);
    }
    #[test]
    fn lost_ack_batch_reply_keeps_every_receipt_and_retries_identically(){
        let mut b=ProviderClient::new(2,88,15).unwrap();
        for s in 1..=4u64{settle_one(&mut b,s)}
        let first=b.next_ack_batch(16);
        assert_eq!(b.flush_ack_batches(1,16,2,|_|Err(110)),0);
        assert_eq!(b.ack_pending(),4);
        assert_eq!(b.next_ack_batch(16),first);
        // Partial mask: only frames rootd retired leave the outbox.
        assert_eq!(b.flush_ack_batches(1,16,1,|_|Ok(0b0101)),2);
        assert_eq!(b.ack_pending(),2);
        // A bit beyond the batch, or a zero mask, retires nothing and stops.
        assert_eq!(b.flush_ack_batches(1,16,4,|_|Ok(1<<5)),0);
        assert_eq!(b.flush_ack_batches(1,16,4,|_|Ok(0)),0);
        assert_eq!(b.ack_pending(),2);
        assert_eq!(b.flush_ack_batches(1,16,4,|f|Ok((1u64<<f.len())-1)),2);
        assert_eq!(b.ack_pending(),0);
    }
    #[test]
    fn ack_batch_done_ignores_a_frame_that_changed_since_it_was_sent(){
        let mut b=ProviderClient::new(2,88,15).unwrap();
        settle_one(&mut b,1);
        let mut sent=b.next_ack_batch(1);
        sent[0].session+=1;
        assert_eq!(b.ack_batch_done(&sent,1),Ok(0));
        assert_eq!(b.ack_pending(),1);
    }

}
