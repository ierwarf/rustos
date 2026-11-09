//! netd slow-path per-socket client Job accounting adapter.
//!
//! Charges run only on the bounded lifecycle pool. Close/failed publication
//! only mark pre-reserved custody for the independent refund worker. No local
//! lock crosses rootd IPC; a token cannot be charged again until refund settles.
//!
//! - **Owner:** `netd` owns the accounting book; rootd owns the Job charge and refund authority.
//! - **Boundary:** rootd replies are untrusted frames, validated by nonce, token and receipt before they settle custody.
//! - **Lifecycle:** reserve custody, CHARGE, publish; close or failed publication marks a refund that one worker settles and ACKs.
//! - **Concurrency:** one bounded custody book behind a mutex that never crosses rootd IPC; charges run only on the lifecycle pool.
//! - **Failure:** a lost, denied or malformed charge cancels by its original nonce; a failed refund rotates and keeps its slot; a full book denies new charges.
//! - **Forbidden:** no lock held across rootd IPC, no second charge identity for one token, no refund dropped on error.
//! - **Evidence:** the accounting cases in `tests/hardening_tests.rs` and `tests/unix_socket_lifecycle_tests.rs`.
use rustos_component_model::provider_wire::Frame;
use rustos_svc_runtime::provider_client::{self, Error, ProviderClient, Receipt, State};
use rustos_user_abi::syscall::IPC_SERVICE_NETD;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;
const RETRIES: usize = 3;
const ACK_BATCH: usize = 8;
static CLIENT: OnceLock<Result<Mutex<Accounting>, i32>> = OnceLock::new();
static REFUND_WORKER: OnceLock<()> = OnceLock::new();

struct Accounting {
    client: ProviderClient,
    // Every charge reserves its eventual refund slot BEFORE the first IPC.
    // Live and in-flight tokens consume capacity too: close can never overflow.
    custody: VecDeque<(u64, bool)>,
}
impl Accounting {
    fn reserve(&mut self, token: u64) -> Result<(), i32> {
        if self.custody.iter().any(|entry| entry.0 == token) {
            return Err(libc::EAGAIN);
        }
        if self.custody.len() >= provider_client::MAX_LOCAL_SESSIONS {
            return Err(libc::ENOSPC);
        }
        self.custody.push_back((token, false));
        Ok(())
    }
    fn prepare_charge(&mut self, token: u64, pid: u64) -> Result<Frame, i32> {
        self.reserve(token)?;
        if let Err(err) = self.client.begin_charge(token, pid) {
            self.custody.retain(|entry| entry.0 != token);
            return Err(errno(err));
        }
        self.client.dispatch_charge(token).map_err(errno)
    }
    fn finish_charge(&mut self, token: u64, reply: Result<Frame, i32>) -> Result<Receipt, i32> {
        let result = reply.and_then(|ack| self.client.charged(token, ack).map_err(errno));
        if result.is_err() {
            self.retire(token)
        }
        result
    }
    fn retire(&mut self, token: u64) {
        if let Some(entry) = self.custody.iter_mut().find(|entry| entry.0 == token) {
            entry.1 = true;
        }
    }
    fn settle(&mut self, token: u64, success: bool) {
        let Some(index) = self
            .custody
            .iter()
            .position(|entry| *entry == (token, true))
        else {
            return;
        };
        let entry = self.custody.remove(index).expect("reserved refund custody");
        if !success {
            self.custody.push_back(entry)
        }
    }
}
fn start_refund_worker() {
    REFUND_WORKER.get_or_init(|| {
        thread::Builder::new()
            .name("netd-session-refund".into())
            .spawn(refund_worker)
            .expect("netd: refund worker required for exact session custody");
    });
}
fn refund_worker() {
    loop {
        // In-flight refunds keep their reserved slot and block token reuse.
        let token = lock()
            .expect("netd accounting lock")
            .custody
            .iter()
            .find(|entry| entry.1)
            .map(|entry| entry.0);
        if let Some(token) = token {
            let success = release_if_funded(token).is_ok();
            lock().expect("netd accounting lock").settle(token, success);
            if !success {
                flush_acks();
                thread::sleep(Duration::from_millis(10));
            }
        } else if flush_acks() == 0 {
            thread::sleep(Duration::from_millis(20));
        }
    }
}
/// Local-only handoff. An unfunded internal endpoint has no custody entry;
/// funded and ambiguous charges already own their bounded refund capacity.
pub(super) fn retire_final(token: u64) -> Result<(), i32> {
    lock()?.retire(token);
    Ok(())
}

fn ledger() -> Result<&'static Mutex<Accounting>, i32> {
    match CLIENT.get_or_init(|| {
        // SAFETY: getpid takes no arguments and cannot fail or touch memory.
        let pid = unsafe { libc::syscall(libc::SYS_getpid) } as i64;
        let epoch = rustos_svc_runtime::syscall::monotonic_nanos();
        if pid <= 0 || epoch == 0 {
            return Err(libc::ESRCH);
        }
        ProviderClient::new(IPC_SERVICE_NETD, pid as u64, epoch)
            .map(|client| {
                Mutex::new(Accounting {
                    client,
                    custody: VecDeque::new(),
                })
            })
            .map_err(|_| libc::EINVAL)
    }) {
        Ok(book) => Ok(book),
        Err(err) => Err(*err),
    }
}
fn errno(e: Error) -> i32 {
    match e {
        Error::Capacity => libc::ENOSPC,
        Error::Missing => libc::ENOENT,
        Error::Existing => libc::EEXIST,
        Error::Phase => libc::EBUSY,
        Error::Overflow => libc::EOVERFLOW,
        Error::WrongAck => libc::EPROTO,
        Error::Kernel(err) => err,
        Error::Uncertain => libc::EAGAIN,
        Error::Invalid => libc::EINVAL,
    }
}
fn lock() -> Result<std::sync::MutexGuard<'static, Accounting>, i32> {
    ledger()?.lock().map_err(|_| libc::EIO)
}
fn rpc_retry(request: Frame) -> Result<Frame, i32> {
    let mut last = libc::ETIMEDOUT;
    for _ in 0..RETRIES {
        match provider_client::call_rootd(request) {
            Ok(value) => return Ok(value),
            Err(err)
                if matches!(
                    err,
                    libc::ETIMEDOUT | libc::EINTR | libc::EAGAIN | libc::ENOMEM | libc::EPROTO
                ) =>
            {
                last = err;
            }
            Err(err) => return Err(err),
        }
    }
    Err(last)
}
pub(super) fn charge_before_publish(token: u64, client_pid: u64) -> Result<Receipt, i32> {
    // A ledger that cannot initialise must fail the charge, not start a refund
    // worker whose `expect` on the same ledger would abort the whole service.
    ledger()?;
    start_refund_worker();
    // Reservation and charge preparation are one local transaction. Existing
    // custody (including a queued failed accept) is NOT a new charge attempt.
    let req = lock()?.prepare_charge(token, client_pid)?;
    let reply = rpc_retry(req);
    // An error marks the already-reserved cancellation slot atomically with
    // the local result. Existing custody never receives a new charge nonce.
    lock()?.finish_charge(token, reply)
}

fn release_if_funded(token: u64) -> Result<(), i32> {
    let phase = {
        let book = lock()?;
        match book.client.pending(token) {
            Ok((state, _)) => state,
            Err(Error::Missing) => return Ok(()),
            Err(err) => return Err(errno(err)),
        }
    };
    match phase {
        State::Funded | State::ReleasePrepared | State::ReleaseInFlight => {
            release_after_final_close(token)
        }
        State::ChargePrepared
        | State::ChargeInFlight
        | State::CancelPrepared
        | State::CancelInFlight => cancel_unpublished(token),
    }
}
/// An unpublished object cannot go back through CHARGE reconciliation:
/// CANCEL is valid with no client Job receipt and fences a charge that
/// arrived before an ACK was lost.
fn cancel_unpublished(token: u64) -> Result<(), i32> {
    let req = {
        let mut book = lock()?;
        book.client.begin_cancel(token).map_err(errno)?;
        book.client.dispatch_cancel(token).map_err(errno)?
    };
    let ack = rpc_retry(req)?;
    {
        let mut book = lock()?;
        book.client.cancelled(token, ack).map_err(errno)?;
    }
    flush_acks();
    Ok(())
}
fn release_after_final_close(token: u64) -> Result<(), i32> {
    let req = {
        let mut book = lock()?;
        match book.client.begin_release(token) {
            Ok(_) => {}
            Err(Error::Phase) => {
                if matches!(
                    book.client.pending(token),
                    Ok((State::ReleaseInFlight, _)) | Ok((State::ReleasePrepared, _))
                ) {
                } else {
                    return Err(libc::EBUSY);
                }
            }
            Err(err) => return Err(errno(err)),
        }
        book.client.dispatch_release(token).map_err(errno)?
    };
    let ack = rpc_retry(req)?;
    {
        let mut book = lock()?;
        book.client.released(token, ack).map_err(errno)?;
    }
    // On ACK loss the rootd tombstone stays, but the kernel refund is final.
    // The receipt ACK stays in the client's bounded outbox and is retried by
    // the control worker; it is never another socket close.
    flush_acks();
    Ok(())
}
/// Drain the receipt-ACK outbox without holding the ledger lock across IPC.
/// Bounded per call; a transport failure leaves the receipt queued.
fn flush_acks() -> usize {
    let mut done = 0;
    while done < ACK_BATCH {
        let frame = {
            match lock() {
                Ok(book) => book.client.next_ack(),
                Err(_) => return done,
            }
        };
        let Some(frame) = frame else { break };
        let Ok(reply) = provider_client::acknowledge_frame(frame) else {
            break;
        };
        let Ok(mut book) = lock() else { break };
        if book.client.ack_done(frame, reply).is_err() {
            break;
        }
        done += 1;
    }
    done
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refund_custody_is_reserved_before_charge_and_survives_failed_retry() {
        let mut book = Accounting {
            client: ProviderClient::new(3, 12, 7).unwrap(),
            custody: VecDeque::new(),
        };
        for token in 1..=provider_client::MAX_LOCAL_SESSIONS as u64 {
            book.reserve(token).unwrap();
        }
        assert_eq!(book.reserve(9000), Err(libc::ENOSPC));
        book.retire(1); // Cannot need another slot even at capacity.
        book.retire(1); // Idempotent, including while IPC is in flight.
        book.settle(1, false);
        assert_eq!(book.custody.len(), provider_client::MAX_LOCAL_SESSIONS);
        assert_eq!(book.reserve(1), Err(libc::EAGAIN));
        assert_eq!(book.custody.back(), Some(&(1, true)));
        book.settle(2, true); // Never settle live custody.
        assert_eq!(book.custody.len(), provider_client::MAX_LOCAL_SESSIONS);
        book.settle(1, true);
        book.reserve(1).unwrap();
        assert_eq!(book.custody.back(), Some(&(1, false)));
        book.retire(0); // Unfunded internal endpoint is a no-op.
    }
    #[test]
    fn delayed_charge_and_rejected_reply_never_leave_an_unowned_socket_or_charge() {
        use crate::{retire_unix_socket, settle_socket_publication_at, NetState, NetdIpcRequest};
        use rustos_component_model::provider_wire::Phase;
        for (now, bind_error) in [(100, libc::EPIPE), (99, libc::EPIPE), (99, 0)] {
            let mut book = Accounting {
                client: ProviderClient::new(3, 12, 7).unwrap(),
                custody: VecDeque::new(),
            };
            let token = 71;
            let charge = book.prepare_charge(token, 20).unwrap();
            // CHARGE commits, but returns only at/near the caller end.
            book.finish_charge(
                token,
                Ok(Frame {
                    phase: Phase::Committed,
                    client_job: 0x100000001,
                    client_job_generation: 1,
                    kernel_receipt: 9,
                    ..charge
                }),
            )
            .unwrap();
            let mut state = NetState::new();
            state
                .sockets
                .insert(token, crate::local_socket_poll_tests::connected_socket(0));
            let binds = std::cell::Cell::new(0);
            let result = settle_socket_publication_at(
                &NetdIpcRequest {
                    deadline_ns: 100,
                    ..NetdIpcRequest::default()
                },
                now,
                token,
                || {
                    binds.set(binds.get() + 1);
                    if bind_error != 0 {
                        Err(bind_error)
                    } else {
                        Ok(())
                    }
                },
                |token| {
                    retire_unix_socket(&mut state, token);
                    book.retire(token);
                },
            );
            assert_eq!(binds.get(), usize::from(now < 100));
            if bind_error == 0 && now < 100 {
                assert_eq!(result, Ok(()));
                // Bound reply then abandoned: kernel owns the initial ref and
                // its deferred transfer-drop CLOSE retires exactly that ref.
                retire_unix_socket(&mut state, token);
                book.retire(token);
            } else {
                assert!(result.is_err())
            }
            assert!(!state.socket_exists(token));
            assert_eq!(book.prepare_charge(token, 20), Err(libc::EAGAIN));
            let release = book.client.begin_release(token).unwrap();
            let dispatched = book.client.dispatch_release(token).unwrap();
            assert_eq!(release.token_nonce, charge.token_nonce);
            assert_eq!(
                book.client.dispatch_release(token).unwrap(),
                dispatched,
                "lost refund ACK must retry exact identity"
            );
            book.settle(token, false);
            book.retire(token); // repeated CLOSE while refund is in flight
            book.client
                .released(
                    token,
                    Frame {
                        phase: Phase::Committed,
                        ..release
                    },
                )
                .unwrap();
            book.settle(token, true);
            book.retire(token); // no second refund obligation
            assert_eq!(book.client.live_count(), 0);
            assert_eq!(book.client.ack_pending(), 1);
            assert!(book.custody.is_empty());
        }
    }
    #[test]
    fn lost_charge_ack_is_cancelled_with_original_nonce_before_accept_token_reuse() {
        use rustos_component_model::provider_wire::{Operation, Phase};
        let mut book = Accounting {
            client: ProviderClient::new(3, 12, 7).unwrap(),
            custody: VecDeque::new(),
        };
        let charge = book.prepare_charge(71, 20).unwrap();
        assert_eq!(
            book.finish_charge(71, Err(libc::ETIMEDOUT)),
            Err(libc::ETIMEDOUT)
        );
        assert_eq!(book.custody.front(),Some(&(71,true)),"unknown outcome must autonomously enter cancellation");
        assert_eq!(book.prepare_charge(71, 21), Err(libc::EAGAIN));
        let cancel = book.client.begin_cancel(71).unwrap();
        assert_eq!(cancel.operation, Operation::Cancel);
        assert_eq!(cancel.token_nonce, charge.token_nonce);
        assert_eq!(cancel.client_job, 0);
        assert_eq!(book.client.dispatch_cancel(71).unwrap(), cancel);
        book.settle(71, false);
        assert_eq!(book.client.dispatch_cancel(71).unwrap(), cancel);
        book.client
            .cancelled(
                71,
                Frame {
                    phase: Phase::Committed,
                    ..cancel
                },
            )
            .unwrap();
        book.settle(71, true);
        let next = book.prepare_charge(71, 21).unwrap();
        assert_ne!(next.token_nonce, charge.token_nonce);
        assert_eq!(book.client.live_count(), 1);
    }
    #[test]
    fn unexpected_local_phase_never_claims_refund() {
        assert_eq!(errno(Error::WrongAck), libc::EPROTO);
        assert_eq!(errno(Error::Uncertain), libc::EAGAIN);
    }
}
