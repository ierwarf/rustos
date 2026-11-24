//! vfsd open-description client Job billing, no_std Ring3 receive owner.
//!
//! Only the VfsState service receiver mutates this ledger. The kernel remains
//! authoritative for client PID -> generational Job ID, session charge, and
//! provider-exit refunds. This module never borrows the VFS storage lock.
//! No application or driver is permitted to supply a trusted Job ID.
//! The engine (charge/release/cancel, deferred batched receipt ACKs) is
//! `vfsd::open_billing`, host-tested in the library target; this file is the
//! `VfsState` glue plus the real rootd transport.
use super::*;
use rustos_component_model::provider_wire::Frame;
use rustos_svc_runtime::provider_client::{acknowledge_batch, call_rootd};
use vfsd::open_billing::RootdLink;

/// Real rootd transport for the billing engine in `vfsd::open_billing`.
pub(super) struct SyscallRootd;
impl RootdLink for SyscallRootd {
    fn control(&mut self, frame: Frame) -> Result<Frame, i32> {
        call_rootd(frame)
    }
    fn ack_batch(&mut self, frames: &[Frame]) -> Result<u64, i32> {
        acknowledge_batch(frames)
    }
}
pub(super) type OpenDescriptionBilling = vfsd::open_billing::OpenDescriptionBilling<SyscallRootd>;
const EAGAIN: i32 = 11;

impl VfsState {
    /// Lazily construct the provider control epoch after checkpoint replay,
    /// avoiding a rootd call during the VFS endpoint registration phase.
    fn billing_mut(&mut self) -> Result<&mut OpenDescriptionBilling, i32> {
        if self.billing.is_none() {
            let pid = unsafe { rustos_svc_runtime::syscall::syscall0(linux_abi::SYS_GETPID) };
            if pid <= 0 {
                return Err(3);
            }
            let epoch = monotonic_nanos();
            if epoch == 0 {
                return Err(EAGAIN);
            }
            self.billing = Some(OpenDescriptionBilling::new(
                pid as u64,
                epoch,
                SyscallRootd,
            )?);
        }
        self.billing.as_mut().ok_or(12)
    }
    pub(super) fn bill_open_description(
        &mut self,
        remote_id: u64,
        client_pid: u64,
    ) -> Result<(), i32> {
        self.billing_mut()?
            .ensure_open(remote_id, client_pid)
            .map(|_| ())
    }
    pub(super) fn cancel_unpublished_open_description(
        &mut self,
        remote_id: u64,
    ) -> Result<(), i32> {
        match self.billing.as_mut() {
            Some(billing) => billing.ensure_cancel(remote_id),
            None => Ok(()),
        }
    }
    pub(super) fn refund_open_description(&mut self, remote_id: u64) -> Result<(), i32> {
        match self.billing.as_mut() {
            Some(billing) => billing.ensure_release(remote_id),
            None => Ok(()),
        }
    }
    /// Snapshot may inform health reporting and resource management,
    /// but unverified counts do not bypass the kernel's Job quota.
    /// Open descriptions are shared across `fork` and `SCM_RIGHTS` fd passing,
    /// so any process holding a kernel-verified descriptor to a `Funded`
    /// description in this `vfsd` incarnation may use it.
    pub(super) fn has_funded_open_description(&self, id: u64) -> bool {
        self.billing.as_ref().is_some_and(|b| b.is_funded(id))
    }
}
