//! One blocking read, moved off the broker loop.
//!
//! - **Owner:** The loop owns the decision to ask and everything done with the
//!   answer; this module owns only the waiting.
//! - **Boundary:** The worker is handed a plain function and returns a plain
//!   value. It never sees `BrokerState`.
//! - **Lifecycle:** Request, wait, answer; one request outstanding at a time.
//! - **Failure:** A worker that cannot be created is reported as absent, which
//!   leaves the caller free to do the work inline instead of not at all.
//!
//! # Why a broker read cannot run on the broker loop
//! Reads that reach storage are not fast and are not bounded by anything the
//! loop controls. Measured on the boot path, the signed launch catalog took
//! 70 ms and the private qualification contract 104 ms, and for the whole of
//! each the loop answered no console caller - not a keystroke, not the shell's
//! parked read, not the compositor's parked graph wait - because the loop is
//! the only receiver they have.
//!
//! Keeping one receiver always ready is the standing rule for a request broker.
//! The launch transaction has its own worker for the same reason; this is the
//! same separation for the reads that have no transaction to speak of.

use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread;

pub(crate) struct Offload<T> {
    requests: SyncSender<()>,
    results: Receiver<T>,
    /// Whether a request is outstanding. Kept here rather than in the caller so
    /// the two can never disagree about whether the worker is busy.
    outstanding: std::cell::Cell<bool>,
    /// A dead worker can never complete a pending request. Distinguish it
    /// from Empty so its caller can fall back or restart instead of waiting
    /// forever with outstanding=true.
    disconnected: std::cell::Cell<bool>,
}

impl<T: Send + 'static> Offload<T> {
    /// Start the worker. `None` when the thread cannot be created.
    pub(crate) fn start(name: &'static str, work: fn() -> T) -> Option<Self> {
        // One outstanding request. A deeper queue would only let the loop ask
        // again for an answer it has not read yet.
        let (request_sender, request_receiver) = mpsc::sync_channel::<()>(1);
        let (result_sender, result_receiver) = mpsc::sync_channel::<T>(1);
        thread::Builder::new()
            .name(String::from(name))
            .spawn(move || {
                while request_receiver.recv().is_ok() {
                    if result_sender.send(work()).is_err() {
                        return;
                    }
                }
            })
            .ok()?;
        Some(Self {
            requests: request_sender,
            results: result_receiver,
            outstanding: std::cell::Cell::new(false),
            disconnected: std::cell::Cell::new(false),
        })
    }

    /// Ask for the work to be done, unless it already is being. Never blocks:
    /// the loop must not wait on the thread that exists so it does not have to.
    pub(crate) fn request(&self) -> bool {
        if self.outstanding.get() || self.disconnected.get() {
            return false;
        }
        match self.requests.try_send(()) {
            Ok(()) => {
                self.outstanding.set(true);
                true
            }
            Err(TrySendError::Full(())) => false,
            Err(TrySendError::Disconnected(())) => {
                self.disconnected.set(true);
                false
            }
        }
    }

    /// Take the answer if one is ready.
    pub(crate) fn poll(&self) -> Option<T> {
        match self.results.try_recv() {
            Ok(value) => {
                self.outstanding.set(false);
                Some(value)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.disconnected.set(true);
                self.outstanding.set(false);
                None
            }
        }
    }

    /// Returns true after the worker's result channel closes. A caller must
    /// replace this worker or use its existing synchronous failure fallback.
    pub(crate) fn disconnected(&self) -> bool {
        self.disconnected.get()
    }

    /// Whether the worker is still working on a request.
    pub(crate) fn busy(&self) -> bool {
        self.outstanding.get()
    }
}

#[cfg(test)]
mod tests {
    use super::Offload;
    use std::time::Duration;

    /// The loop asks once and is never made to wait.
    ///
    /// Asking again while an answer is outstanding has to be refused rather
    /// than queued: the caller's retry policy is a backoff on a read that has
    /// not finished, and letting it stack requests would turn one slow read
    /// into a queue of them.
    #[test]
    fn a_request_is_refused_while_one_is_outstanding_and_never_blocks_the_caller() {
        let offload = Offload::<u32>::start("runtimed-test-offload", || 7).expect("worker starts");
        assert!(!offload.busy());
        assert!(offload.request(), "the first request is accepted");
        assert!(offload.busy());
        assert!(
            !offload.request(),
            "a second request while one is outstanding is refused, not queued"
        );

        let mut answer = None;
        for _ in 0..2_000 {
            if let Some(value) = offload.poll() {
                answer = Some(value);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(answer, Some(7));
        assert!(!offload.busy(), "the answer clears the request");
        assert!(offload.request(), "and the next one is accepted again");
    }

    /// A dead worker cannot leave a busy flag set forever or cause the
    /// caller to issue requests into a disconnected channel. No timing or
    /// actual thread panic is needed to model both channel endpoints closing.
    #[test]
    fn disconnected_worker_releases_the_inflight_wait_for_recovery() {
        let (requests, request_receiver) = std::sync::mpsc::sync_channel(1);
        drop(request_receiver);
        let (result_sender, results) = std::sync::mpsc::sync_channel::<u32>(1);
        drop(result_sender);
        let offload = Offload {
            requests,
            results,
            outstanding: std::cell::Cell::new(true),
            disconnected: std::cell::Cell::new(false),
        };
        assert_eq!(offload.poll(), None);
        assert!(offload.disconnected());
        assert!(!offload.busy());
        assert!(!offload.request(), "a dead worker cannot queue more work");
    }

    #[test]
    fn disconnected_request_sender_is_remembered() {
        let (requests, request_receiver) = std::sync::mpsc::sync_channel(1);
        drop(request_receiver);
        let (_results_sender, results) = std::sync::mpsc::sync_channel::<u32>(1);
        let offload = Offload {
            requests,
            results,
            outstanding: std::cell::Cell::new(false),
            disconnected: std::cell::Cell::new(false),
        };
        assert!(!offload.request());
        assert!(offload.disconnected());
        assert!(!offload.busy());
    }

    /// Polling an idle worker reports nothing rather than waiting.
    #[test]
    fn polling_without_a_request_reports_nothing() {
        let offload = Offload::<u32>::start("runtimed-test-offload-idle", || 1).expect("starts");
        assert_eq!(offload.poll(), None);
        assert!(!offload.busy());
    }
}
