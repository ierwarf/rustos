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
        _=>return Err(22),
    };
    if control.operation!=intended||control.phase!=Phase::Request
       ||control.kernel_receipt!=0||control.status!=0{return Err(22)}
    Ok(control)
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
    req.validate_request(sender.pid,req.provider_lease).map_err(|_|13)?;
    let owner=Owner{service,pid:sender.pid,lease:req.provider_lease};
    let token=Token{epoch:req.token_epoch,nonce:req.token_nonce};
    let receipt;
    let (job,gen);
    if req.operation==Operation::Charge {
        let actual=jobs::process_job(req.client_pid).map_err(|_|13i32)?;
        if actual==0{return Err(13)}
        // Kernel Job ID packs slot (upper 32) + immutable slot generation
        // (lower 32). Never accept a caller-supplied guessed Job identity.
        job=actual;
        gen=actual as u32;
        if gen==0{return Err(116)}
        let client=Client{pid:req.client_pid,job,job_generation:gen};
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
    provider_wire::encode_vec(ack).map_err(|_|12)
}
#[cfg(test)]
mod tests {
    use super::*;
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
