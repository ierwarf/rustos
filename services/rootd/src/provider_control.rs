//! Live rootd generic provider-session accounting RPC (slow control only).
//!
//! This endpoint requires the exact kernel-stamped sender identity, positive
//! service grant, effective JobPolicy, and live Job membership. A raw service
//! number or payload is never authority. The old console RPC remains isolated.
//! Kernel JOB_SESSION holds the actual charge; this book only stages/retries.
use super::*;
use alloc::vec::Vec;
use rustos_component_model::provider_wire::{self, Frame, Operation, Phase};
use rustos_component_model::provider_sessions::{Client, Owner, ProviderKind, Ticket, Token};
use rustos_component_model::policy_ops::SlowBrokerOperation;

fn kind(service:u64)->Result<ProviderKind,i32>{
    match service {
        IPC_SERVICE_NETD=>Ok(ProviderKind::Socket),
        IPC_SERVICE_VFSD=>Ok(ProviderKind::OpenDescription),
        _=>Err(13),
    }
}
fn exact_accounting_fields(request:&CommercialMaxProtocolRequest)
    ->Result<Frame,i32>{
    if request.arg0!=0||request.arg1!=0||request.arg2!=0||request.arg3!=0
        ||request.path_len!=0||request.payload_len as usize!=provider_wire::BYTES {
        return Err(22)
    }
    let control=provider_wire::decode(&request.payload[..provider_wire::BYTES])
        .map_err(|_|22)?;
    let intended=match request.header.op {
        COMMERCIAL_MAX_ROOTD_OP_PROVIDER_SESSION_CHARGE=>Operation::Charge,
        COMMERCIAL_MAX_ROOTD_OP_PROVIDER_SESSION_RELEASE=>Operation::Release,
        COMMERCIAL_MAX_ROOTD_OP_PROVIDER_SESSION_CANCEL=>Operation::Cancel,
        COMMERCIAL_MAX_ROOTD_OP_PROVIDER_RECEIPT_ACK=>{
            if !matches!(control.operation,Operation::Release|Operation::Cancel)
                ||control.phase!=Phase::Accepted
                ||control.kernel_receipt!=0||control.status!=0 {
                return Err(22)
            }
            return Ok(control)
        }
        _=>return Err(22),
    };
    if control.operation!=intended||control.phase!=Phase::Request
       ||control.kernel_receipt!=0||control.status!=0{return Err(22)}
    Ok(control)
}
/// Decode and shape-check a RECEIPT_ACK_BATCH payload. Every frame must pass
/// exactly the single-ACK shape check; one bad frame rejects the whole request.
fn decode_ack_batch(request:&CommercialMaxProtocolRequest)->Result<Vec<Frame>,i32>{
    let len=request.payload_len as usize;
    if request.arg0!=0||request.arg1!=0||request.arg2!=0||request.arg3!=0
        ||request.path_len!=0||len==0||len>request.payload.len()
        ||len%provider_wire::BYTES!=0
        ||len/provider_wire::BYTES>PROVIDER_RECEIPT_ACK_BATCH_MAX_FRAMES {
        return Err(22)
    }
    let mut frames=Vec::new();
    frames.try_reserve(len/provider_wire::BYTES).map_err(|_|12)?;
    for chunk in request.payload[..len].chunks_exact(provider_wire::BYTES){
        let f=provider_wire::decode(chunk).map_err(|_|22)?;
        if !matches!(f.operation,Operation::Release|Operation::Cancel)
            ||f.phase!=Phase::Accepted||f.kernel_receipt!=0||f.status!=0 {
            return Err(22)
        }
        frames.push(f);
    }
    Ok(frames)
}
/// Apply already shape-checked ACK frames in order. Ownership of every frame
/// (service, PID, lease/epoch) is checked as for a single ACK and a foreign
/// frame fails the whole request before any compaction. Per-frame rootd
/// refusals (`acknowledge` errors) are reported as clear mask bits, so the
/// provider keeps those receipts queued. Never touches the kernel ledger.
fn apply_ack_batch(
    book:&mut provider_billing::ProviderBilling,frames:&[Frame],
    service:u64,sender_pid:u64,kind:ProviderKind,
)->Result<u64,i32>{
    for req in frames{
        if req.provider_service!=service||req.provider_pid!=sender_pid
            ||req.provider_lease==0||req.token_epoch!=req.provider_lease
            ||!req.valid() {return Err(13)}
    }
    let mut mask=0u64;
    for (i,req) in frames.iter().enumerate(){
        let owner=Owner{service,pid:sender_pid,lease:req.provider_lease};
        let token=Token{epoch:req.token_epoch,nonce:req.token_nonce};
        if book.acknowledge(owner,token,sender_pid,req.client_pid,req.session,
            kind,req.client_job,req.client_job_generation).is_ok(){
            mask|=1<<i;
        }
    }
    Ok(mask)
}
/// Op 31. Same authorization as op 28, once for the whole batch.
/// Returns `(retired mask, frame count)`.
pub(super) fn execute_ack_batch(
    request:&CommercialMaxProtocolRequest,sender:IpcSenderIdentity,
    leases:&[Lease],post_init:&[PostInitLease],
    book:&mut provider_billing::ProviderBilling,
)->Result<(u64,u64),i32>{
    let frames=decode_ack_batch(request)?;
    let service=authorize_sender_slow_grant(
        leases,post_init,sender,SlowBrokerOperation::ChargeClientSession,
    )?;
    let kind=kind(service)?;
    let mask=apply_ack_batch(book,&frames,service,sender.pid,kind)?;
    Ok((mask,frames.len() as u64))
}
pub(super) fn execute(
    request:&CommercialMaxProtocolRequest,sender:IpcSenderIdentity,
    leases:&[Lease],post_init:&[PostInitLease],
    book:&mut provider_billing::ProviderBilling,
)->Result<Vec<u8>,i32>{
    let req=exact_accounting_fields(request)?;
    // This is a slow supervisor RPC, never netd's TCP/IPC read/write path.
    let service=authorize_sender_slow_grant(
        leases,post_init,sender,SlowBrokerOperation::ChargeClientSession,
    )?;
    let kind=kind(service)?;
    if req.provider_service!=service || req.provider_pid!=sender.pid
       ||req.provider_lease==0 || req.token_epoch!=req.provider_lease {
        return Err(13)
    }
    // The asserted provider lease/epoch is not independently a kernel
    // generation: the exact live service owner must already be checked above.
    if request.header.op!=COMMERCIAL_MAX_ROOTD_OP_PROVIDER_RECEIPT_ACK {
        req.validate_request(sender.pid,req.provider_lease).map_err(|_|13)?;
    }
    let owner=Owner{service,pid:sender.pid,lease:req.provider_lease};
    let token=Token{epoch:req.token_epoch,nonce:req.token_nonce};
    let receipt;
    let (job,gen);
    if request.header.op==COMMERCIAL_MAX_ROOTD_OP_PROVIDER_RECEIPT_ACK {
        book.acknowledge(owner,token,sender.pid,req.client_pid,
            req.session,kind,req.client_job,req.client_job_generation)?;
        job=req.client_job;
        gen=req.client_job_generation;
        receipt=0;
    } else if req.operation==Operation::Charge {
        // An in-flight kernel charge may already have succeeded when the
        // client process exits. A retry MUST reuse the originally verified
        // JobId rather than querying the now-stale client PID and treating
        // the lost ACK as permission to invent a new token.
        let client=if let Some((prior,session,prior_kind,_,_))=
            book.find_ticket(owner,token) {
            if prior.pid!=req.client_pid || session!=req.session
                || prior_kind!=kind {return Err(13)}
            prior
        } else {
            let actual=jobs::process_job(req.client_pid).map_err(|_|13i32)?;
            if actual==0 || actual as u32==0{return Err(116)}
            Client{pid:req.client_pid,job:actual,
                job_generation:actual as u32}
        };
        job=client.job;
        gen=client.job_generation;
        let ticket=Ticket{token,provider:owner,client,kind,session:req.session};
        let kernel_charge=|c:Client,pid:u64,t:u64|{
            jobs::charge_client_session(c.pid,c.job,pid,t)
                .map(|_|t).map_err(|e|e as i32)
        };
        receipt=if book.unknown(owner,token) {
            book.reconcile_charge(owner,token,client,req.session,kind,sender.pid,
                kernel_charge)?
        } else {
            book.charge(ticket,sender.pid,true,kernel_charge)?
        };
    } else if req.operation==Operation::Cancel {
        // A failed or ambiguous CHARGE is canceled with expected_job=0;
        // the kernel's per-provider token watermark fences delayed retries.
        book.cancel(owner,token,sender.pid,req.client_pid,req.session,kind,
            |pid,kernel_token|jobs::release_client_session(0,pid,kernel_token)
                .map(|_|()).map_err(|e|e as i32))?;
        job=0;
        gen=0;
        receipt=0;
    } else {
        job=req.client_job;
        gen=req.client_job_generation;
        if job==0 || gen==0 || gen!=job as u32{return Err(22)}
        let client=Client{pid:req.client_pid,job,job_generation:gen};
        book.release(owner,token,sender.pid,client,req.session,kind,
            |j,p,t|jobs::release_client_session(j,p,t)
                .map(|_|()).map_err(|e|e as i32))?;
        receipt=0;
    }
    let ack=Frame {
        operation:req.operation,phase:Phase::Committed,
        client_job:job,client_job_generation:gen,kernel_receipt:receipt,
        ..req
    };
    provider_wire::encode(ack).map_err(|_|12)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn ack_frame(nonce:u64,session:u64)->Frame{
        Frame{operation:Operation::Release,phase:Phase::Accepted,request_id:nonce,
            provider_service:IPC_SERVICE_VFSD,provider_pid:50,provider_lease:7,
            client_pid:300,client_job:0x100000001,client_job_generation:1,
            session,token_epoch:7,token_nonce:nonce,kernel_receipt:0,status:0}
    }
    fn batch_request(frames:&[Frame])->CommercialMaxProtocolRequest{
        let mut r=CommercialMaxProtocolRequest::default();
        r.header.op=COMMERCIAL_MAX_ROOTD_OP_PROVIDER_RECEIPT_ACK_BATCH;
        let mut n=0;
        for f in frames{
            let b=provider_wire::encode(*f).unwrap();
            r.payload[n..n+b.len()].copy_from_slice(&b);n+=b.len();
        }
        r.payload_len=n as u32;r
    }
    fn settled_book(count:u64)->provider_billing::ProviderBilling{
        let owner=Owner{service:IPC_SERVICE_VFSD,pid:50,lease:7};
        let client=Client{pid:300,job:0x100000001,job_generation:1};
        let mut book=provider_billing::ProviderBilling::new();
        for n in 1..=count{
            let t=Ticket{token:Token{epoch:7,nonce:n},provider:owner,client,
                kind:ProviderKind::OpenDescription,session:n+100};
            book.charge(t,50,true,|_,_,k|Ok(k)).unwrap();
            book.release(owner,t.token,50,client,n+100,
                ProviderKind::OpenDescription,|_,_,_|Ok(())).unwrap();
        }
        book
    }
    #[test]
    fn ack_batch_compacts_exactly_the_acknowledged_settled_receipts(){
        let mut book=settled_book(3);
        assert_eq!(book.count(),3);
        let frames=[ack_frame(1,101),ack_frame(2,102),ack_frame(3,103)];
        let decoded=decode_ack_batch(&batch_request(&frames)).unwrap();
        assert_eq!(decoded,frames);
        let mask=apply_ack_batch(&mut book,&decoded,IPC_SERVICE_VFSD,50,
            ProviderKind::OpenDescription).unwrap();
        assert_eq!(mask,0b111);
        assert_eq!(book.count(),0);
        // Replay after a lost reply is idempotent: absent receipts still retire.
        assert_eq!(apply_ack_batch(&mut book,&decoded,IPC_SERVICE_VFSD,50,
            ProviderKind::OpenDescription),Ok(0b111));
    }
    #[test]
    fn ack_batch_refusals_are_per_frame_and_foreign_frames_fail_closed(){
        let mut book=settled_book(2);
        // Wrong session on frame 1: rootd keeps it, the others compact.
        let frames=[ack_frame(1,101),ack_frame(2,999)];
        assert_eq!(apply_ack_batch(&mut book,&frames,IPC_SERVICE_VFSD,50,
            ProviderKind::OpenDescription),Ok(0b01));
        assert_eq!(book.count(),1);
        // A frame naming another provider PID/service/lease rejects all.
        for bad in [
            Frame{provider_pid:51,..ack_frame(2,102)},
            Frame{provider_service:IPC_SERVICE_NETD,..ack_frame(2,102)},
            Frame{token_epoch:6,..ack_frame(2,102)},
        ]{
            assert_eq!(apply_ack_batch(&mut book,&[ack_frame(2,102),bad],
                IPC_SERVICE_VFSD,50,ProviderKind::OpenDescription),Err(13));
        }
        assert_eq!(book.count(),1);
    }
    #[test]
    fn ack_batch_shape_is_exact(){
        let good=[ack_frame(1,101)];
        assert!(decode_ack_batch(&batch_request(&good)).is_ok());
        let mut r=batch_request(&good);r.payload_len-=1;
        assert_eq!(decode_ack_batch(&r),Err(22));
        let mut r=batch_request(&good);r.arg0=1;
        assert_eq!(decode_ack_batch(&r),Err(22));
        let mut r=batch_request(&good);r.payload_len=0;
        assert_eq!(decode_ack_batch(&r),Err(22));
        let mut r=batch_request(&good);r.path_len=1;
        assert_eq!(decode_ack_batch(&r),Err(22));
        // A Request-phase (not Accepted) frame is never an ACK.
        let req=Frame{phase:Phase::Request,..ack_frame(1,101)};
        assert_eq!(decode_ack_batch(&batch_request(&[req])),Err(22));
        let many:Vec<Frame>=(1..=17).map(|n|ack_frame(n,100+n)).collect();
        assert_eq!(decode_ack_batch(&batch_request(&many)),Err(22));
    }
    #[test]
    fn exact_shape_denies_scalar_and_wrong_operation(){
        let mut request=CommercialMaxProtocolRequest::default();
        request.header.op=COMMERCIAL_MAX_ROOTD_OP_PROVIDER_SESSION_CHARGE;
        assert!(exact_accounting_fields(&request).is_err());
        request.arg0=11;
        assert!(exact_accounting_fields(&request).is_err());
        assert_eq!(kind(IPC_SERVICE_NETD),Ok(ProviderKind::Socket));
        assert_eq!(kind(IPC_SERVICE_VFSD),Ok(ProviderKind::OpenDescription));
        assert!(kind(IPC_SERVICE_SESSIOND).is_err());
    }
}
