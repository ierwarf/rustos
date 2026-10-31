//! Rootd-only exact-token client session accounting.
//!
//! The kernel owns hierarchical job counters; only the authenticated live
//! sessiond process can request rootd to mint/retire one session token.
//! Rootd never charges sessiond's own Job for a client allocation.
//!
//! This bounded book is the authority for idempotent operations and exact
//! refunds. It lives as part of rootd's single-threaded supervisor control
//! state, and all mutations pass through the same message receiver.
use alloc::vec::Vec;

const MAX_SESSIONS: usize = 256;
const EINVAL: i32 = 22;
const EACCES: i32 = 13;
const ENOMEM: i32 = 12;
const ENOSPC: i32 = 28;
const ESTALE: i32 = 116;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Accounted {
    token: u64,
    client_pid: u64,
    client_job: u64,
}

/// Bounded recent receipts make a retried release idempotent even after its
/// first kernel refund succeeded but the supervisor response was lost.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Retired {
    token: u64,
    client_pid: u64,
    client_job: u64,
}

pub(crate) struct SessionLedger {
    entries: Vec<Accounted>,
    retired: Vec<Retired>,
    last_token: u64,
    /// Live authenticated sessiond generation, named by exact reporter PID.
    owner_pid: u64,
}

impl SessionLedger {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            retired: Vec::new(),
            last_token: 0,
            owner_pid: 0,
        }
    }

    /// A restarted sessiond may start its local handle counter at one.
    /// Before admitting its first charge, refund the *whole* previous live
    /// generation and retire its token namespace. This is a slow supervisor
    /// transition; it cannot happen on a console read/write hot path.
    pub(crate) fn bind_owner(
        &mut self,
        pid: u64,
        kernel_release: impl FnMut(u64, u64, u64) -> Result<(), i32>,
    ) -> Result<(), i32> {
        if pid == 0 {
            return Err(EINVAL);
        }
        if self.owner_pid != pid {
            self.release_all(kernel_release)?;
            self.owner_pid = pid;
            self.last_token = 0;
            self.retired.clear();
        }
        Ok(())
    }

    /// Flush an exited owner on lifecycle evidence, not only on the next
    /// incoming sessiond instance's first charge. This prevents a crashed
    /// provider from pinning arbitrary client Jobs until it is restarted.
    pub(crate) fn owner_exited(
        &mut self,
        pid: u64,
        kernel_release: impl FnMut(u64, u64, u64) -> Result<(), i32>,
    ) -> Result<(), i32> {
        if pid == 0 || self.owner_pid != pid {
            return Ok(());
        }
        self.release_all(kernel_release)?;
        self.owner_pid = 0;
        self.last_token = 0;
        self.retired.clear();
        Ok(())
    }

    /// No kernel mutation occurs before the inventory is validated and can
    /// hold the new token. Duplicate live requests never double-charge.
    pub(crate) fn charge(
        &mut self,
        token: u64,
        client_pid: u64,
        mut lookup_job: impl FnMut(u64) -> Result<u64, i32>,
        mut kernel_charge: impl FnMut(u64, u64) -> Result<(), i32>,
    ) -> Result<u64, i32> {
        if token == 0 || client_pid == 0 {
            return Err(EINVAL);
        }
        if let Some(entry) = self.entries.iter().find(|e| e.token == token) {
            if entry.client_pid == client_pid {
                return Ok(entry.client_job);
            }
            return Err(EACCES);
        }
        if token <= self.last_token {
            return Err(ESTALE);
        }
        if self.entries.len() >= MAX_SESSIONS {
            return Err(ENOSPC);
        }
        self.entries.try_reserve(1).map_err(|_| ENOMEM)?;
        let job = lookup_job(client_pid)?;
        if job == 0 {
            return Err(EACCES);
        }
        kernel_charge(client_pid, job)?;
        self.entries.push(Accounted { token, client_pid, client_job: job });
        self.last_token = token;
        Ok(job)
    }

    /// Bound the reply-dedup history; never allocate after a kernel refund
    /// has already committed.
    fn reserve_receipt(&mut self) -> Result<(), i32> {
        if self.retired.len() == MAX_SESSIONS {
            self.retired.remove(0);
        } else {
            self.retired.try_reserve(1).map_err(|_| ENOMEM)?;
        }
        Ok(())
    }

    /// The JobId was captured at admission. Never rediscover the Job by a PID
    /// that may have exited or been recycled before this refund.
    pub(crate) fn release(
        &mut self,
        token: u64,
        client_pid: u64,
        mut kernel_release: impl FnMut(u64, u64, u64) -> Result<(), i32>,
    ) -> Result<u64, i32> {
        if token == 0 || client_pid == 0 {
            return Err(EINVAL);
        }
        let Some(index) = self.entries.iter().position(|e| e.token == token) else {
            return match self.retired.iter().find(|e| e.token == token) {
                Some(receipt) if receipt.client_pid == client_pid =>
                    Ok(receipt.client_job),
                Some(_) => Err(EACCES),
                None => Err(ESTALE),
            };
        };
        let entry = self.entries[index];
        if entry.client_pid != client_pid {
            return Err(EACCES);
        }
        self.reserve_receipt()?;
        kernel_release(entry.client_job, self.owner_pid, entry.token)?;
        self.entries.swap_remove(index);
        self.retired.push(Retired {
            token,
            client_pid,
            client_job: entry.client_job,
        });
        Ok(entry.client_job)
    }

    /// Only a currently authenticated SESSIOND principal may request
    /// a token-only retirement. The client PID comes from the original
    /// account, not a caller-supplied mutable input.
    pub(crate) fn release_trusted(
        &mut self,
        token: u64,
        kernel_release: impl FnMut(u64, u64, u64) -> Result<(), i32>,
    ) -> Result<u64, i32> {
        if token == 0 {
            return Err(EINVAL);
        }
        if let Some(entry) = self.entries.iter().find(|entry| entry.token == token) {
            return self.release(token, entry.client_pid, kernel_release);
        }
        if let Some(receipt) = self.retired.iter().find(|entry| entry.token == token) {
            return Ok(receipt.client_job);
        }
        // Lost/late charge acknowledgement: release unknown is a cancellation.
        // Advance the watermark so the corresponding old charge can NEVER
        // arrive later and create a new bill after we acknowledged release.
        self.reserve_receipt()?;
        // Kernel owns a second nonreusable token watermark: a rootd crash
        // after this point cannot permit a delayed CHARGE to revive the bill.
        kernel_release(0, self.owner_pid, token)?;
        self.last_token = self.last_token.max(token);
        self.retired.push(Retired {
            token,
            client_pid: 0,
            client_job: 0,
        });
        Ok(0)
    }

    /// Reclaim every outstanding session after sessiond's authenticated
    /// service lease is retired. No process may borrow sessiond authority
    /// to keep another client's accounting pinned indefinitely.
    pub(crate) fn release_all(
        &mut self,
        mut kernel_release: impl FnMut(u64, u64, u64) -> Result<(), i32>,
    ) -> Result<usize, i32> {
        let mut released = 0;
        while let Some(entry) = self.entries.last().copied() {
            kernel_release(entry.client_job, self.owner_pid, entry.token)?;
            self.entries.pop();
            released += 1;
        }
        self.retired.clear();
        Ok(released)
    }

    pub(crate) fn live_count(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    #[test]
    fn session_charge_is_client_funded_and_exactly_once() {
        let mut book = SessionLedger::new();
        let charges = Cell::new(0u64);
        let releases = Cell::new(0u64);
        assert_eq!(
            book.charge(8, 200, |_| Ok(77), |pid, job| {
                assert_eq!((pid, job), (200, 77));
                charges.set(charges.get() + 1);
                Ok(())
            }),
            Ok(77)
        );
        assert_eq!(book.charge(8, 200, |_| panic!("duplicate lookup"), |_, _| {
            panic!("duplicate bill")
        }), Ok(77));
        assert_eq!(book.charge(8, 201, |_| Ok(99), |_, _| Ok(())), Err(EACCES));
        assert_eq!(charges.get(), 1);
        assert_eq!(book.live_count(), 1);
        assert_eq!(book.release(8, 200, |job, _, _| {
            assert_eq!(job, 77);
            releases.set(releases.get() + 1);
            Ok(())
        }), Ok(77));
        assert_eq!(book.release(8, 200, |_, _, _| panic!("double refund")), Ok(77));
        assert_eq!(book.charge(8, 200, |_| Ok(77), |_, _| Ok(())), Err(ESTALE));
        assert_eq!(releases.get(), 1);
        assert_eq!(book.live_count(), 0);
    }

    #[test]
    fn denied_admission_does_not_advance_last_token() {
        let mut book = SessionLedger::new();
        assert_eq!(book.charge(10, 22, |_| Ok(9), |_, _| Err(ENOSPC)), Err(ENOSPC));
        assert_eq!(book.charge(10, 22, |_| Ok(9), |_, _| Ok(())), Ok(9));
        assert_eq!(book.release(10, 21, |_, _, _| panic!("wrong PID")), Err(EACCES));
        assert_eq!(book.release(10, 22, |_, _, _| Err(16)), Err(16));
        assert_eq!(book.live_count(), 1);
        assert_eq!(book.release_all(|job, _, _| {
            assert_eq!(job, 9);
            Ok(())
        }), Ok(1));
        assert_eq!(book.live_count(), 0);
    }

    #[test]
    fn exited_provider_refunds_every_client_token() {
        let mut book = SessionLedger::new();
        book.bind_owner(901, |_, _, _| Ok(())).unwrap();
        book.charge(1, 20, |_| Ok(2), |_, _| Ok(())).unwrap();
        book.charge(2, 21, |_| Ok(3), |_, _| Ok(())).unwrap();
        let mut retired = alloc::vec::Vec::new();
        book.owner_exited(777, |_, _, _| panic!("foreign exit")).unwrap();
        assert_eq!(book.live_count(), 2);
        book.owner_exited(901, |job, _, _| { retired.push(job); Ok(()) }).unwrap();
        assert_eq!(retired, alloc::vec![3, 2]);
        assert_eq!(book.live_count(), 0);
        book.bind_owner(902, |_, _, _| panic!("already clean")).unwrap();
        assert_eq!(book.charge(1, 22, |_| Ok(4), |_, _| Ok(())), Ok(4));
    }

    #[test]
    fn supervisor_restart_refunds_previous_epoch_and_allows_fresh_tokens() {
        let mut book = SessionLedger::new();
        book.bind_owner(10, |_, _, _| panic!("nothing to refund")).unwrap();
        book.charge(1, 100, |_| Ok(7), |_, _| Ok(())).unwrap();
        book.charge(2, 101, |_| Ok(8), |_, _| Ok(())).unwrap();
        let mut refunded = alloc::vec::Vec::new();
        book.bind_owner(11, |job, _, _| { refunded.push(job); Ok(()) }).unwrap();
        assert_eq!(refunded, alloc::vec![8, 7]);
        assert_eq!(book.live_count(), 0);
        assert_eq!(book.charge(1, 102, |_| Ok(9), |_, _| Ok(())), Ok(9));
        assert_eq!(book.last_token, 1);
        book.bind_owner(11, |_, _, _| panic!("same owner no rollback")).unwrap();
        assert_eq!(book.live_count(), 1);
    }

    #[test]
    fn late_charge_is_invalidated_by_exact_token_cancellation() {
        let mut book = SessionLedger::new();
        book.bind_owner(10, |_, _, _| Ok(())).unwrap();
        assert_eq!(book.release_trusted(23, |_, _, _| panic!("nothing to bill")), Ok(0));
        assert_eq!(book.charge(23, 200, |_| Ok(7), |_, _| panic!("late charge")), Err(ESTALE));
        assert_eq!(book.release_trusted(23, |_, _, _| panic!("already canceled")), Ok(0));
        assert_eq!(book.charge(24, 201, |_| Ok(8), |_, _| Ok(())), Ok(8));
        assert_eq!(book.release_trusted(24, |_, _, _| Ok(())), Ok(8));
        assert_eq!(book.release_trusted(24, |_, _, _| panic!("double refund")), Ok(8));
    }

    #[test]
    fn registry_ceiling_preflights_before_kernel_mutation() {
        let mut book = SessionLedger::new();
        for token in 1..=MAX_SESSIONS as u64 {
            book.charge(token, token, |pid| Ok(pid), |_, _| Ok(())).unwrap();
        }
        assert_eq!(
            book.charge(MAX_SESSIONS as u64 + 1, 99, |_| panic!("full lookup"), |_, _| {
                panic!("full kernel charge")
            }),
            Err(ENOSPC)
        );
        assert_eq!(book.release_all(|_, _, _| Ok(())), Ok(MAX_SESSIONS));
    }
}
