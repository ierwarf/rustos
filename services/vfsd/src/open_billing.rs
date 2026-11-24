//! vfsd open-description client Job billing engine (transport-generic).
//!
//! Lives in the library target so it can be unit-tested on the host; the
//! binary's `provider_accounting` glue supplies the real rootd transport and
//! the `VfsState` call sites. The ledger is owned by the single VfsState
//! receiver; no storage lock is held over a rootd call. The kernel remains
//! authoritative for client PID -> generational Job ID, session charge and
//! provider-exit refunds, and rootd authorizes every charge/release itself.
//!
//! Rootd calls per description (steady state): open = 1 CHARGE, close = 1
//! RELEASE, plus one batched RECEIPT_ACK_BATCH per 16 closes (see
//! `ACK_LOW_WATER`). Receipt ACKs are compaction only, so they are deferred:
//! rootd keeps a `Settled` tombstone per un-ACKed receipt (at most the 16 below
//! low water plus whatever a rootd outage left queued, capped by
//! `MAX_ACK_OUTBOX` locally and reclaimed under pressure by rootd's own
//! 4096-entry table), the owner nonce watermark keeps every spent token fenced
//! whether or not it was ACKed, a restarted rootd treats an absent token as
//! already settled, and a restarted vfsd's tombstones are dropped when rootd
//! sees the old PID exit. Deferral therefore can neither double-charge nor leak.
use rustos_component_model::provider_wire::Frame;
use rustos_svc_runtime::provider_client::{Error, ProviderClient, Receipt, State};
use rustos_user_abi::syscall::{IPC_SERVICE_VFSD, PROVIDER_RECEIPT_ACK_BATCH_MAX_FRAMES};

const EPROTO: i32 = 71;
const EBUSY: i32 = 16;
const EAGAIN: i32 = 11;
const MAX_RETRIES: usize = 3;
/// Receipt ACKs are sent only once this many are queued, in one batched call.
pub const ACK_LOW_WATER: usize = PROVIDER_RECEIPT_ACK_BATCH_MAX_FRAMES;
/// Batches per billing operation: bounds the extra latency a backlog (for
/// example after a rootd outage) can add to one open/close.
const ACK_MAX_CALLS: usize = 2;

/// Rootd transport. Each method is one synchronous IPC.
pub trait RootdLink {
    /// CHARGE / RELEASE / CANCEL request frame -> committed reply frame.
    fn control(&mut self, frame: Frame) -> Result<Frame, i32>;
    /// Batched receipt ACK; returns the retired bitmask.
    fn ack_batch(&mut self, frames: &[Frame]) -> Result<u64, i32>;
}

fn errno(error: Error) -> i32 {
    match error {
        Error::Invalid => 22,
        Error::Capacity => 28,
        Error::Existing => 17,
        Error::Missing => 2,
        Error::Phase => EBUSY,
        Error::WrongAck => EPROTO,
        Error::Uncertain => EAGAIN,
        Error::Overflow => 75,
        Error::Kernel(code) => code,
    }
}

pub struct OpenDescriptionBilling<T: RootdLink> {
    ledger: ProviderClient,
    link: T,
}

impl<T: RootdLink> OpenDescriptionBilling<T> {
    pub fn new(pid: u64, epoch: u64, link: T) -> Result<Self, i32> {
        Ok(Self {
            ledger: ProviderClient::new(IPC_SERVICE_VFSD, pid, epoch).map_err(errno)?,
            link,
        })
    }
    fn control_retry(&mut self, frame: Frame) -> Result<Frame, i32> {
        let mut latest = EAGAIN;
        for _ in 0..MAX_RETRIES {
            match self.link.control(frame) {
                Ok(ack) => return Ok(ack),
                Err(err) if matches!(err, 11 | 12 | 4 | 71 | 110) => latest = err,
                Err(err) => return Err(err),
            }
        }
        Err(latest)
    }
    pub fn ensure_open(&mut self, remote_id: u64, client_pid: u64) -> Result<Receipt, i32> {
        if remote_id == 0 || client_pid == 0 {
            return Err(22);
        }
        self.flush_acks();
        match self.ledger.begin_charge(remote_id, client_pid) {
            Ok(_) => {}
            Err(Error::Existing) => {
                let (state, prior) = self.ledger.pending(remote_id).map_err(errno)?;
                if prior.client_pid != client_pid {
                    return Err(13);
                }
                if state == State::Funded {
                    return self.ledger.receipt(remote_id).map_err(errno);
                }
                if !matches!(state, State::ChargePrepared | State::ChargeInFlight) {
                    return Err(EBUSY);
                }
            }
            Err(err) => return Err(errno(err)),
        }
        // The only cross-service IPC is between state transitions. There is
        // no borrowed BTreeMap or storage lock in the call itself.
        let request = self.ledger.dispatch_charge(remote_id).map_err(errno)?;
        let ack = self.control_retry(request)?;
        self.ledger.charged(remote_id, ack).map_err(errno)
    }
    /// Deferred, batched receipt-ACK drain: a no-op below `ACK_LOW_WATER`
    /// queued receipts, otherwise at most `ACK_MAX_CALLS` batched IPCs. There
    /// is no background worker in this freestanding service, so a backlog left
    /// by a lost reply is retried by the next billing operation that crosses
    /// the low-water mark.
    pub fn flush_acks(&mut self) {
        let link = &mut self.link;
        self.ledger.flush_ack_batches(
            ACK_LOW_WATER,
            PROVIDER_RECEIPT_ACK_BATCH_MAX_FRAMES,
            ACK_MAX_CALLS,
            |frames| link.ack_batch(frames),
        );
    }
    pub fn ensure_cancel(&mut self, remote_id: u64) -> Result<(), i32> {
        let phase = match self.ledger.pending(remote_id) {
            Ok((phase, _)) => phase,
            Err(Error::Missing) => return Ok(()),
            Err(err) => return Err(errno(err)),
        };
        if matches!(phase, State::ReleasePrepared | State::ReleaseInFlight) {
            return self.ensure_release(remote_id);
        }
        self.ledger.begin_cancel(remote_id).map_err(errno)?;
        let request = self.ledger.dispatch_cancel(remote_id).map_err(errno)?;
        let ack = self.control_retry(request)?;
        self.ledger.cancelled(remote_id, ack).map_err(errno)?;
        self.flush_acks();
        Ok(())
    }
    pub fn ensure_release(&mut self, remote_id: u64) -> Result<(), i32> {
        let state = match self.ledger.pending(remote_id) {
            Ok((state, _)) => state,
            // A replayed legacy handle is not automatically a client debit.
            Err(Error::Missing) => return Ok(()),
            Err(err) => return Err(errno(err)),
        };
        if matches!(
            state,
            State::ChargePrepared
                | State::ChargeInFlight
                | State::CancelPrepared
                | State::CancelInFlight
        ) {
            return self.ensure_cancel(remote_id);
        }
        if self.ledger.pending(remote_id).map_err(errno)?.0 == State::Funded {
            self.ledger.begin_release(remote_id).map_err(errno)?;
        }
        let request = self.ledger.dispatch_release(remote_id).map_err(errno)?;
        let ack = self.control_retry(request)?;
        self.ledger.released(remote_id, ack).map_err(errno)?;
        self.flush_acks();
        Ok(())
    }
    pub fn is_funded(&self, remote_id: u64) -> bool {
        self.ledger
            .pending(remote_id)
            .ok()
            .is_some_and(|(state, _frame)| state == State::Funded)
    }
    #[cfg(test)]
    pub fn pending_acks(&self) -> usize {
        self.ledger.ack_pending()
    }
    #[cfg(test)]
    pub fn link(&self) -> &T {
        &self.link
    }
    #[cfg(test)]
    pub fn link_mut(&mut self) -> &mut T {
        &mut self.link
    }
    /// Test view of the provider-session ledger.
    #[cfg(test)]
    pub fn ledger(&self) -> &ProviderClient {
        &self.ledger
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use rustos_component_model::provider_wire::{Operation, Phase};

    /// Models rootd's accounting for these ops: idempotent by token, absent
    /// receipts ACK successfully. Counts calls by kind.
    #[derive(Default)]
    struct FakeRootd {
        charges: usize,
        releases: usize,
        control_calls: usize,
        ack_calls: usize,
        acked_frames: usize,
        /// Fail the next N `control` calls with these errnos (reply lost
        /// AFTER rootd applied the request).
        fail_control: Vec<i32>,
        fail_ack: bool,
        live: Vec<(u64, u64)>,
        spent: Vec<(u64, u64)>,
        double_charges: usize,
    }
    impl RootdLink for FakeRootd {
        fn control(&mut self, frame: Frame) -> Result<Frame, i32> {
            self.control_calls += 1;
            let key = (frame.token_epoch, frame.token_nonce);
            match frame.operation {
                Operation::Charge => {
                    if self.spent.contains(&key) && !self.live.contains(&key) {
                        self.double_charges += 1;
                    }
                    if !self.live.contains(&key) {
                        self.live.push(key);
                        self.spent.push(key);
                        self.charges += 1;
                    }
                    if !self.fail_control.is_empty() {
                        return Err(self.fail_control.remove(0));
                    }
                    Ok(Frame {
                        phase: Phase::Committed,
                        client_job: 0x100000001,
                        client_job_generation: 1,
                        kernel_receipt: 9,
                        ..frame
                    })
                }
                Operation::Release | Operation::Cancel => {
                    if self.live.contains(&key) && frame.operation == Operation::Release {
                        self.releases += 1;
                    }
                    self.live.retain(|k| *k != key);
                    if !self.fail_control.is_empty() {
                        return Err(self.fail_control.remove(0));
                    }
                    Ok(Frame {
                        phase: Phase::Committed,
                        ..frame
                    })
                }
                Operation::Reconcile => Err(22),
            }
        }
        fn ack_batch(&mut self, frames: &[Frame]) -> Result<u64, i32> {
            self.ack_calls += 1;
            if self.fail_ack {
                return Err(110);
            }
            self.acked_frames += frames.len();
            Ok((1u64 << frames.len()) - 1)
        }
    }
    fn book() -> OpenDescriptionBilling<FakeRootd> {
        OpenDescriptionBilling::new(50, 7, FakeRootd::default()).unwrap()
    }

    #[test]
    fn open_is_one_rootd_call_and_close_is_one_plus_a_batched_ack_per_16() {
        let mut b = book();
        for id in 1..=32u64 {
            b.ensure_open(id, 300).unwrap();
        }
        // Opens never touch the ACK path while nothing is queued.
        assert_eq!((b.link().control_calls, b.link().ack_calls), (32, 0));
        for id in 1..=32u64 {
            b.ensure_release(id).unwrap();
        }
        let l = b.link();
        // 64 control calls + 2 batch calls = 66 rootd IPCs for 32 open/close
        // pairs; the per-receipt ACK design needed 96.
        assert_eq!((l.charges, l.releases, l.control_calls), (32, 32, 64));
        assert_eq!((l.ack_calls, l.acked_frames), (2, 32));
        assert_eq!(b.pending_acks(), 0);
        assert_eq!(b.link().double_charges, 0);
    }
    #[test]
    fn deferred_receipts_stay_below_the_low_water_mark_and_fence_their_nonces() {
        let mut b = book();
        for id in 1..ACK_LOW_WATER as u64 {
            b.ensure_open(id, 300).unwrap();
            b.ensure_release(id).unwrap();
        }
        assert_eq!(b.link().ack_calls, 0);
        assert_eq!(b.pending_acks(), ACK_LOW_WATER - 1);
        // Re-opening a closed id takes a NEW nonce: a spent token is never
        // reissued, ACKed or not.
        b.ensure_open(1, 300).unwrap();
        assert_eq!(b.link().double_charges, 0);
        assert_eq!(b.link().charges, ACK_LOW_WATER);
    }
    #[test]
    fn lost_ack_batch_reply_keeps_receipts_and_is_retried_on_the_next_operation() {
        let mut b = book();
        b.link_mut().fail_ack = true;
        for id in 1..=ACK_LOW_WATER as u64 {
            b.ensure_open(id, 300).unwrap();
            // A transport failure on the ACK batch never fails the close.
            b.ensure_release(id).unwrap();
        }
        assert!(b.link().ack_calls >= 1);
        assert_eq!(b.pending_acks(), ACK_LOW_WATER);
        b.link_mut().fail_ack = false;
        b.ensure_open(99, 300).unwrap();
        assert_eq!(b.pending_acks(), 0);
        assert_eq!(b.link().acked_frames, ACK_LOW_WATER);
    }
    #[test]
    fn lost_charge_and_release_replies_resend_the_same_frame_without_double_charge() {
        let mut b = book();
        // Charge applied at rootd, reply lost: the identical frame is resent.
        b.link_mut().fail_control = alloc::vec![110];
        b.ensure_open(1, 300).unwrap();
        assert_eq!((b.link().charges, b.link().double_charges), (1, 0));
        // Release applied, reply lost twice, then succeeds.
        b.link_mut().fail_control = alloc::vec![110, 110];
        b.ensure_release(1).unwrap();
        assert!(b.link().live.is_empty());
        // Retries exhausted: the error surfaces, the receipt stays resumable
        // and the next close completes it exactly once.
        b.ensure_open(2, 300).unwrap();
        b.link_mut().fail_control = alloc::vec![110, 110, 110];
        assert_eq!(b.ensure_release(2), Err(110));
        assert_eq!(b.pending_acks(), 1);
        b.ensure_release(2).unwrap();
        assert_eq!(b.pending_acks(), 2);
        assert_eq!(b.link().double_charges, 0);
    }
    #[test]
    fn a_restarted_vfsd_uses_a_new_epoch_so_old_tokens_are_never_reissued() {
        let mut a = book();
        a.ensure_open(1, 300).unwrap();
        let first = a.ledger().pending(1).unwrap().1;
        let mut b = OpenDescriptionBilling::new(51, 8, FakeRootd::default()).unwrap();
        b.ensure_open(1, 300).unwrap();
        let second = b.ledger().pending(1).unwrap().1;
        assert_ne!(first.token_epoch, second.token_epoch);
        assert_ne!(first.provider_pid, second.provider_pid);
    }
}
