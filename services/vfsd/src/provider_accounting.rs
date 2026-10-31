//! Staged vfsd open-description charge adapter (Ring3 only).
//!
//! Not invoked by the legacy VFS request path until open-description
//! ownership, dup/fork and replayed checkpoint closure have one final-ref
//! transfer point. Every IPC call here must be performed outside VfsState
//! and storage locks. Rootd validates the actual client PID/Job mapping.
use rustos_svc_runtime::provider_client::{
    self,ProviderClient,Receipt,Error,State,
};
use rustos_component_model::provider_wire::Frame;
use rustos_user_abi::syscall::IPC_SERVICE_VFSD;
const EINVAL:i32=22;const EBUSY:i32=16;
const EAGAIN:i32=11;const EPROTO:i32=71;const ENOSPC:i32=28;
fn map_error(e:Error)->i32 {
    match e{
        Error::Invalid=>EINVAL,Error::Capacity=>ENOSPC,
        Error::Existing=>17,Error::Missing=>2,
        Error::Phase=>EBUSY,Error::WrongAck=>EPROTO,
        Error::Uncertain=>EAGAIN,Error::Overflow=>75,
        Error::Kernel(errno)=>errno,
    }
}
pub(super) struct OpenDescriptionBilling{
    inner:ProviderClient,
}
impl OpenDescriptionBilling {
    pub(super) fn new(pid:u64,epoch:u64)->Result<Self,i32>{
        Ok(Self{inner:ProviderClient::new(IPC_SERVICE_VFSD,pid,epoch)
            .map_err(map_error)?})
    }
    /// Reserve a billing identity before the corresponding kernel-visible
    /// VFS open description is published. A source checkpoint is NOT a bill.
    pub(super) fn prepare_open(&mut self,remote_id:u64,client_pid:u64)
        ->Result<Frame,i32>{
        self.inner.begin_charge(remote_id,client_pid).map_err(map_error)?;
        self.inner.dispatch_charge(remote_id).map_err(map_error)
    }
    pub(super) fn complete_open(&mut self,remote_id:u64,ack:Frame)
        ->Result<Receipt,i32>{
        self.inner.charged(remote_id,ack).map_err(map_error)
    }
    pub(super) fn retry_open(&mut self,remote_id:u64)->Result<Frame,i32>{
        self.inner.dispatch_charge(remote_id).map_err(map_error)
    }
    pub(super) fn prepare_close(&mut self,remote_id:u64)->Result<Frame,i32>{
        self.inner.begin_release(remote_id).map_err(map_error)?;
        self.inner.dispatch_release(remote_id).map_err(map_error)
    }
    pub(super) fn retry_close(&mut self,remote_id:u64)->Result<Frame,i32>{
        self.inner.dispatch_release(remote_id).map_err(map_error)
    }
    pub(super) fn complete_close(&mut self,remote_id:u64,ack:Frame)
        ->Result<(),i32>{
        self.inner.released(remote_id,ack).map_err(map_error)
    }
    pub(super) fn unresolved(&self,mut visit:impl FnMut(u64,State,Frame)){
        self.inner.unresolved(|id,state,request|visit(id,state,request))
    }
    /// This call must be made after releasing all file metadata and open
    /// description locks. It never guesses refund after transport timeout.
    pub(super) fn call(frame:Frame)->Result<Frame,i32>{
        provider_client::call_rootd(frame)
    }
    pub(super) fn live_count(&self)->usize{self.inner.live_count()}
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn open_description_does_not_double_bill_on_dup(){
        let mut x=OpenDescriptionBilling::new(20,100).unwrap();
        let first=x.prepare_open(9,77).unwrap();
        assert_eq!(first.provider_service,IPC_SERVICE_VFSD);
        assert!(x.prepare_open(9,77).is_err());
        assert_eq!(x.retry_open(9).unwrap(),first);
    }
}
