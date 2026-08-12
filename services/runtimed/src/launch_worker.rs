//! Off-loop execution of the launch half of a supervisor transaction.
//!
//! - **Owner:** The broker loop owns every byte of `BrokerState`; this module
//!   owns only the blocking service calls a launch makes.
//! - **Boundary:** The worker never sees `BrokerState`. Everything it needs is
//!   copied into the job, and everything it learns comes back as a message.
//! - **Lifecycle:** One launch is in flight at a time: submit, spawn, record,
//!   activate, finish.
//! - **Concurrency:** The worker blocks on the loop's acknowledgement before it
//!   may activate a child, so the ordering the supervisor depends on is a
//!   property of the protocol rather than of who happens to run first.
//! - **Failure:** Every terminal path reports a `Finished` exactly once, so the
//!   loop can never be left holding a launch slot for a worker that has gone.
//!
//! # Why the launch does not run on the broker loop
//! That loop is the console's only receiver. It carries keystroke delivery, the
//! shell's parked reads, and the compositor's parked graph wait, and it visits
//! each of them once per pass. A launch performs four blocking service calls,
//! and running them inline made a pass take 149 ms - measured - during which
//! none of those console callers could be answered, however ready they were.
//!
//! Keeping a receiver always ready is the standing rule for a request broker:
//! QNX states it directly for resource managers, whose thread pool exists to
//! guarantee a thread stays RECEIVE-blocked, and the general remedy for
//! head-of-line blocking is to give long work its own worker rather than to
//! make short work wait behind it. This module is that worker.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

use crate::kvm_smp_qualification::KvmSmpQualificationContract;
use crate::LaunchEntry;

/// How long the worker will hold a spawned-but-unrecorded child while waiting
/// for the loop to acknowledge it.
///
/// # Why this has a bound at all
/// The child exists and is suspended. If the loop were to vanish between the
/// spawn and the record, an unbounded wait here would leave that child
/// suspended forever with nobody owning it. The budget is generous - the loop
/// answers within a pass under any healthy load - so expiring it is evidence of
/// a broker that has stopped, not of a busy one.
const RECORD_ACK_BUDGET: Duration =
    Duration::from_millis(rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS);

/// One launch, with everything the worker needs already copied out of
/// `BrokerState`.
pub(crate) struct LaunchJob {
    pub(crate) entry: LaunchEntry,
    pub(crate) qualification_contract: Option<KvmSmpQualificationContract>,
    pub(crate) session_handle: Option<u64>,
    pub(crate) is_ui_server: bool,
}

/// What the worker tells the loop.
pub(crate) enum LaunchProgress {
    /// The child exists and is start-suspended. The worker is now blocked
    /// waiting for the loop to record it; it cannot activate until then.
    Spawned { pid: i32 },
    /// Terminal. `pid` is present whenever a child was created, so the loop
    /// knows whether it has a record to drop.
    Finished { pid: Option<i32>, result: Result<(), i32> },
}

/// The loop's answer to `Spawned`.
pub(crate) enum RecordAck {
    /// Recorded. The worker may activate.
    Recorded,
    /// Refused - the loop already has this pid. The worker retires the child.
    Duplicate,
}

/// The blocking calls a launch makes, injected so the protocol can be tested
/// without a loader, a console, or ring0.
pub(crate) struct LaunchCalls {
    pub(crate) spawn: fn(&LaunchEntry, u64) -> Result<i32, i32>,
    pub(crate) report_lease: fn(&LaunchEntry, i32) -> Result<(), i32>,
    pub(crate) bind_and_activate:
        fn(i32, Option<KvmSmpQualificationContract>) -> Result<(), (&'static str, i32)>,
    pub(crate) await_endpoint: fn(i32) -> Result<(), i32>,
    pub(crate) retire: fn(i32, &str),
}

pub(crate) struct LaunchWorker {
    jobs: SyncSender<LaunchJob>,
    progress: Receiver<LaunchProgress>,
    record_acks: SyncSender<RecordAck>,
}

impl LaunchWorker {
    /// Hand a launch to the worker. Refuses rather than blocks: the loop must
    /// never wait on the thread whose whole purpose is to wait for it.
    pub(crate) fn submit(&self, job: LaunchJob) -> Result<(), i32> {
        match self.jobs.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(libc::EBUSY),
            Err(TrySendError::Disconnected(_)) => Err(libc::EPIPE),
        }
    }

    /// Take whatever the worker has said, without waiting.
    pub(crate) fn poll(&self) -> Option<LaunchProgress> {
        self.progress.try_recv().ok()
    }

    /// Answer a `Spawned`. The worker is blocked on this.
    pub(crate) fn acknowledge(&self, ack: RecordAck) {
        let _ = self.jobs_ack(ack);
    }

    fn jobs_ack(&self, ack: RecordAck) -> Result<(), i32> {
        match self.record_acks.try_send(ack) {
            Ok(()) => Ok(()),
            Err(_) => Err(libc::EPIPE),
        }
    }
}

/// Start the worker. Returns `None` if the thread cannot be created, which
/// leaves the caller free to run launches inline rather than not at all.
pub(crate) fn start(calls: LaunchCalls) -> Option<LaunchWorker> {
    // One launch in flight, one answer outstanding. A deeper queue would only
    // let the loop run ahead of a supervisor transaction it has to stay in step
    // with.
    let (job_sender, job_receiver) = mpsc::sync_channel::<LaunchJob>(1);
    let (progress_sender, progress_receiver) = mpsc::sync_channel::<LaunchProgress>(2);
    let (ack_sender, ack_receiver) = mpsc::sync_channel::<RecordAck>(1);
    let spawned = thread::Builder::new()
        .name(String::from("runtimed-launch"))
        .spawn(move || run(job_receiver, progress_sender, ack_receiver, calls));
    if spawned.is_err() {
        return None;
    }
    Some(LaunchWorker {
        jobs: job_sender,
        progress: progress_receiver,
        record_acks: ack_sender,
    })
}

fn run(
    jobs: Receiver<LaunchJob>,
    progress: SyncSender<LaunchProgress>,
    acks: Receiver<RecordAck>,
    calls: LaunchCalls,
) {
    while let Ok(job) = jobs.recv() {
        let outcome = execute(&job, &acks, &progress, &calls);
        let _ = progress.send(outcome);
    }
}

/// Run one launch, reporting the terminal outcome.
///
/// The shape mirrors the transaction this replaced exactly: create the child
/// suspended, have it recorded, then activate. What changed is only that the
/// record now happens on the other side of a message, which is what lets the
/// loop keep answering the console while this runs.
fn execute(
    job: &LaunchJob,
    acks: &Receiver<RecordAck>,
    progress: &SyncSender<LaunchProgress>,
    calls: &LaunchCalls,
) -> LaunchProgress {
    let pid = match (calls.spawn)(&job.entry, job.session_handle.unwrap_or(0)) {
        Ok(pid) => pid,
        Err(errno) => {
            return LaunchProgress::Finished {
                pid: None,
                result: Err(errno),
            }
        }
    };

    if job.is_ui_server {
        if let Err(errno) = (calls.report_lease)(&job.entry, pid) {
            (calls.retire)(pid, "rootd-lease-report");
            return LaunchProgress::Finished {
                pid: None,
                result: Err(errno),
            };
        }
    }

    // Nothing below may run until the loop owns this pid.
    if progress.send(LaunchProgress::Spawned { pid }).is_err() {
        (calls.retire)(pid, "record-channel");
        return LaunchProgress::Finished {
            pid: None,
            result: Err(libc::EPIPE),
        };
    }
    match acks.recv_timeout(RECORD_ACK_BUDGET) {
        Ok(RecordAck::Recorded) => {}
        Ok(RecordAck::Duplicate) => {
            (calls.retire)(pid, "duplicate-pid");
            return LaunchProgress::Finished {
                pid: None,
                result: Err(libc::EEXIST),
            };
        }
        Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
            // The child is suspended and unowned. Retiring it is the only
            // outcome that does not leak a task nobody supervises.
            (calls.retire)(pid, "record-ack");
            return LaunchProgress::Finished {
                pid: None,
                result: Err(libc::ETIMEDOUT),
            };
        }
    }

    if let Err((stage, errno)) = (calls.bind_and_activate)(pid, job.qualification_contract) {
        (calls.retire)(pid, stage);
        return LaunchProgress::Finished {
            pid: Some(pid),
            result: Err(errno),
        };
    }
    if job.is_ui_server {
        if let Err(errno) = (calls.await_endpoint)(pid) {
            (calls.retire)(pid, "endpoint-wait");
            return LaunchProgress::Finished {
                pid: Some(pid),
                result: Err(errno),
            };
        }
    }
    LaunchProgress::Finished {
        pid: Some(pid),
        result: Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI32, Ordering};

    static ORDER: AtomicI32 = AtomicI32::new(0);
    static ACTIVATED_AT: AtomicI32 = AtomicI32::new(-1);

    fn entry() -> LaunchEntry {
        LaunchEntry {
            package_id: String::from("pkg"),
            desktop_file_id: String::from("app.desktop"),
            display_name: String::from("App"),
            exec: String::from("app.elf"),
            runtime_deps: Vec::new(),
            restart: false,
            weight_micros: 100,
            logical_admin: false,
            console_hosted: false,
            args: Vec::new(),
            env: Vec::new(),
            private_smp_qualification: None,
        }
    }

    fn calls() -> LaunchCalls {
        LaunchCalls {
            spawn: |_, _| Ok(41),
            report_lease: |_, _| Ok(()),
            bind_and_activate: |_, _| {
                ACTIVATED_AT.store(ORDER.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                Ok(())
            },
            await_endpoint: |_| Ok(()),
            retire: |_, _| {},
        }
    }

    /// A child may not become runnable before the loop has recorded it.
    ///
    /// This is the invariant the whole handshake exists for. Running the launch
    /// inline used to make it true by construction - the same thread did both
    /// steps in order - and moving the launch off the loop would quietly lose it
    /// if activation did not wait for the record. An app that becomes runnable
    /// before runtimed owns its pid is unsupervised: it can exit into a reaper
    /// that has never heard of it.
    #[test]
    fn a_child_is_never_activated_before_the_loop_has_recorded_it() {
        ORDER.store(0, Ordering::SeqCst);
        ACTIVATED_AT.store(-1, Ordering::SeqCst);
        let worker = super::start(calls()).expect("worker starts");
        worker
            .submit(LaunchJob {
                entry: entry(),
                qualification_contract: None,
                session_handle: None,
                is_ui_server: false,
            })
            .expect("the first job is accepted");

        let spawned = wait_for_progress(&worker);
        let LaunchProgress::Spawned { pid } = spawned else {
            panic!("the worker reports the child before activating it");
        };
        assert_eq!(pid, 41);
        assert_eq!(
            ACTIVATED_AT.load(Ordering::SeqCst),
            -1,
            "the worker must still be waiting; activating here would be exactly \
             the unsupervised window the record closes"
        );

        let recorded_at = ORDER.fetch_add(1, Ordering::SeqCst);
        worker.acknowledge(RecordAck::Recorded);

        let finished = wait_for_progress(&worker);
        let LaunchProgress::Finished { pid, result } = finished else {
            panic!("the launch terminates");
        };
        assert_eq!(pid, Some(41));
        assert_eq!(result, Ok(()));
        assert!(
            recorded_at < ACTIVATED_AT.load(Ordering::SeqCst),
            "the record has to be ordered before the activation, not merely near it"
        );
    }

    /// A pid the loop already owns is retired rather than activated.
    #[test]
    fn a_duplicate_pid_is_retired_instead_of_becoming_a_second_owner() {
        static RETIRED: AtomicI32 = AtomicI32::new(0);
        RETIRED.store(0, Ordering::SeqCst);
        let mut calls = calls();
        calls.retire = |_, _| {
            RETIRED.fetch_add(1, Ordering::SeqCst);
        };
        calls.bind_and_activate = |_, _| panic!("a duplicate must never be activated");
        let worker = super::start(calls).expect("worker starts");
        worker
            .submit(LaunchJob {
                entry: entry(),
                qualification_contract: None,
                session_handle: None,
                is_ui_server: false,
            })
            .expect("the first job is accepted");

        assert!(matches!(
            wait_for_progress(&worker),
            LaunchProgress::Spawned { .. }
        ));
        worker.acknowledge(RecordAck::Duplicate);
        let LaunchProgress::Finished { pid, result } = wait_for_progress(&worker) else {
            panic!("the launch terminates");
        };
        assert_eq!(
            pid, None,
            "the loop recorded nothing, so it must be told there is nothing to drop"
        );
        assert_eq!(result, Err(libc::EEXIST));
        assert_eq!(RETIRED.load(Ordering::SeqCst), 1);
    }

    /// The loop is never made to wait on the worker.
    #[test]
    fn a_second_launch_is_refused_rather_than_queued_behind_the_first() {
        let worker = super::start(calls()).expect("worker starts");
        let job = || LaunchJob {
            entry: entry(),
            qualification_contract: None,
            session_handle: None,
            is_ui_server: false,
        };
        worker.submit(job()).expect("the first job is accepted");
        // The first job occupies the worker until it is acknowledged, so the
        // queue slot is the only place a second could go.
        assert!(matches!(
            wait_for_progress(&worker),
            LaunchProgress::Spawned { .. }
        ));
        worker.submit(job()).expect("the queue slot takes one more");
        assert_eq!(
            worker.submit(job()),
            Err(libc::EBUSY),
            "beyond that the loop is told no, rather than blocked"
        );
        worker.acknowledge(RecordAck::Recorded);
    }

    fn wait_for_progress(worker: &LaunchWorker) -> LaunchProgress {
        for _ in 0..2_000 {
            if let Some(progress) = worker.poll() {
                return progress;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("the worker went quiet");
    }
}
