//! Hand-off state for rootd's dedicated memory-pressure thread.
//!
//! The pressure pass must never queue behind anything that can wait on an IPC
//! peer (loaderd, initd, vfsd, pagerd) or on memory. The supervisor loop and
//! the loader-supervision worker both do exactly that, so the pass runs on its
//! own thread. This module is the allocation-free, lock-free glue:
//!
//! - a try-lock (`busy`) so the thread and the inline fallback never run the
//!   pass concurrently (the governor state is single-writer);
//! - a liveness beat, so the supervisor loop runs the pass inline only while
//!   the thread is absent (clone answers `ENOSYS` before procd) or wedged;
//! - a seqlock-published snapshot of the pids a victim selection must avoid
//!   and of the application realm, so the thread never touches the
//!   supervisor's mutable lease tables.
//!
//! The snapshot is a *hint*, never the whole protection decision: a pid that
//! was spawned after the last publish (the supervisor can be parked on a
//! stalled loaderd) is absent from it. The pass therefore re-derives
//! protection from kernel facts for every candidate (service-endpoint
//! ownership and membership of the application realm), and treats a process
//! whose class cannot be proven an application as a protected service.
//!
//! Supervision pattern adopted: an independent watchdog that shares no
//! blocking dependency with the thing it watches (systemd `WatchdogSec`
//! petting from a separate context, OOM policy decided out of band as with
//! `systemd-oomd`), and a supervisor whose decision input is a published
//! snapshot instead of a call into the supervised peer.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Distinct supervised pids a whole-job kill must avoid.
pub(super) const PROTECTED_PIDS_MAX: usize = 64;
/// A thread that has not beaten for this long no longer excludes the inline pass.
pub(super) const THREAD_STALE_NS: u64 = 1_000_000_000;

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);

pub(super) struct PressureGate {
    busy: AtomicBool,
    thread_started: AtomicBool,
    thread_beat_ns: AtomicU64,
    next_pass_ns: AtomicU64,
    seq: AtomicU64,
    protected_len: AtomicUsize,
    protected_overflow: AtomicBool,
    realm_handle: AtomicU64,
    realm_job: AtomicU64,
    protected: [AtomicU64; PROTECTED_PIDS_MAX],
}

/// The application realm: the root-child job every runtime-launched
/// application lives below. `handle` is rootd's descriptor, `job` the id the
/// kernel reported for it when it was published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Realm {
    pub(super) handle: u64,
    pub(super) job: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Snapshot {
    pub(super) len: usize,
    pub(super) overflow: bool,
    pub(super) realm: Option<Realm>,
}

impl PressureGate {
    pub(super) const fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            thread_started: AtomicBool::new(false),
            thread_beat_ns: AtomicU64::new(0),
            next_pass_ns: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            protected_len: AtomicUsize::new(0),
            protected_overflow: AtomicBool::new(false),
            realm_handle: AtomicU64::new(0),
            realm_job: AtomicU64::new(0),
            protected: [ZERO; PROTECTED_PIDS_MAX],
        }
    }

    /// Publishes the pids a job kill must avoid. Single writer (the supervisor
    /// loop or the loader worker that currently owns the lease tables).
    pub(super) fn publish_protected(&self, pids: impl Iterator<Item = u64>, realm: Option<Realm>) {
        let mut len = 0;
        let mut overflow = false;
        // ORDERING: odd `seq` marks the write window; the Release store of the
        // even value below pairs with the reader's Acquire loads.
        let start = self.seq.load(Ordering::Relaxed) | 1;
        self.seq.store(start, Ordering::Release);
        core::sync::atomic::fence(Ordering::Release);
        for pid in pids {
            if pid == 0 {
                continue;
            }
            if len < PROTECTED_PIDS_MAX {
                self.protected[len].store(pid, Ordering::Relaxed);
                len += 1;
            } else {
                overflow = true;
            }
        }
        self.protected_len.store(len, Ordering::Relaxed);
        self.protected_overflow.store(overflow, Ordering::Relaxed);
        let (handle, job) = realm.map_or((0, 0), |realm| (realm.handle, realm.job));
        self.realm_handle.store(handle, Ordering::Relaxed);
        self.realm_job.store(job, Ordering::Relaxed);
        self.seq.store(start.wrapping_add(1), Ordering::Release);
    }

    /// Copies a consistent snapshot into `out`. `None` means a publish was in
    /// flight on every attempt: skip this pass, never act on a torn list.
    pub(super) fn read_protected(
        &self,
        out: &mut [u64; PROTECTED_PIDS_MAX],
    ) -> Option<Snapshot> {
        for _ in 0..8 {
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let len = self.protected_len.load(Ordering::Relaxed).min(PROTECTED_PIDS_MAX);
            let overflow = self.protected_overflow.load(Ordering::Relaxed);
            let realm_handle = self.realm_handle.load(Ordering::Relaxed);
            let realm_job = self.realm_job.load(Ordering::Relaxed);
            for (slot, value) in out[..len].iter_mut().zip(&self.protected) {
                *slot = value.load(Ordering::Relaxed);
            }
            core::sync::atomic::fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return Some(Snapshot {
                    len,
                    overflow,
                    realm: (realm_job != 0).then_some(Realm {
                        handle: realm_handle,
                        job: realm_job,
                    }),
                });
            }
        }
        None
    }

    pub(super) fn mark_thread_started(&self, now_ns: u64) {
        self.thread_beat_ns.store(now_ns, Ordering::Release);
        self.thread_started.store(true, Ordering::Release);
    }

    pub(super) fn thread_started(&self) -> bool {
        self.thread_started.load(Ordering::Acquire)
    }

    pub(super) fn beat(&self, now_ns: u64) {
        self.thread_beat_ns.store(now_ns, Ordering::Release);
    }

    fn thread_is_serving(&self, now_ns: u64) -> bool {
        self.thread_started.load(Ordering::Acquire)
            && now_ns.saturating_sub(self.thread_beat_ns.load(Ordering::Acquire))
                < THREAD_STALE_NS
    }

    /// Runs `pass` if it is due and nobody else is running it. Returns whether
    /// it ran. Never blocks: a busy gate means another context is mid-pass.
    fn run_due(&self, now_ns: u64, interval_ns: u64, pass: impl FnOnce()) -> bool {
        if now_ns < self.next_pass_ns.load(Ordering::Acquire) {
            return false;
        }
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        self.next_pass_ns
            .store(now_ns.saturating_add(interval_ns), Ordering::Release);
        pass();
        self.busy.store(false, Ordering::Release);
        true
    }

    /// The dedicated thread's iteration: always runs when due.
    pub(super) fn thread_step(&self, now_ns: u64, interval_ns: u64, pass: impl FnOnce()) -> bool {
        self.beat(now_ns);
        self.run_due(now_ns, interval_ns, pass)
    }

    /// The supervisor loop's / loader worker's fallback: runs only while the
    /// thread is absent or stale, so a healthy thread is the sole driver.
    pub(super) fn inline_tick(&self, now_ns: u64, interval_ns: u64, pass: impl FnOnce()) -> bool {
        if self.thread_is_serving(now_ns) {
            return false;
        }
        self.run_due(now_ns, interval_ns, pass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;

    const INTERVAL: u64 = 20_000_000;

    #[test]
    fn inline_pass_runs_only_without_a_serving_thread() {
        let gate = PressureGate::new();
        let runs = AtomicU32::new(0);
        assert!(gate.inline_tick(0, INTERVAL, || {
            runs.fetch_add(1, Ordering::Relaxed);
        }));
        // Thread comes up: the inline path steps aside.
        gate.mark_thread_started(INTERVAL);
        assert!(!gate.inline_tick(2 * INTERVAL, INTERVAL, || panic!("thread serves")));
        // The thread wedges: after the stale window the inline path resumes.
        let stale = INTERVAL + THREAD_STALE_NS;
        assert!(gate.inline_tick(stale, INTERVAL, || {
            runs.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(runs.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn pass_is_cadenced_and_never_reentered() {
        let gate = PressureGate::new();
        assert!(gate.thread_step(0, INTERVAL, || {
            // A concurrent context finds the gate busy and skips.
            assert!(!gate.run_due(u64::MAX / 2, INTERVAL, || panic!("reentered")));
        }));
        assert!(!gate.thread_step(INTERVAL - 1, INTERVAL, || panic!("early")));
        assert!(gate.thread_step(INTERVAL, INTERVAL, || {}));
    }

    #[test]
    fn protected_snapshot_round_trips_and_flags_overflow() {
        let gate = PressureGate::new();
        gate.publish_protected([7, 0, 9].into_iter(), Some(Realm { handle: 5, job: 77 }));
        let mut out = [0; PROTECTED_PIDS_MAX];
        assert_eq!(
            gate.read_protected(&mut out),
            Some(Snapshot { len: 2, overflow: false, realm: Some(Realm { handle: 5, job: 77 }) })
        );
        assert_eq!(&out[..2], &[7, 9]);
        gate.publish_protected((1..=(PROTECTED_PIDS_MAX as u64 + 3)).into_iter(), None);
        assert_eq!(
            gate.read_protected(&mut out),
            Some(Snapshot { len: PROTECTED_PIDS_MAX, overflow: true, realm: None })
        );
    }

    #[test]
    fn torn_snapshot_is_refused() {
        let gate = PressureGate::new();
        gate.seq.store(1, Ordering::Release); // publisher mid-write
        let mut out = [0; PROTECTED_PIDS_MAX];
        assert_eq!(gate.read_protected(&mut out), None);
    }

    /// The scenario from the 2048 MiB boot: the supervisor loop is parked on a
    /// peer (initd/loaderd) that never answers. The pressure thread must keep
    /// passing on its cadence regardless.
    #[test]
    fn pressure_pass_runs_while_the_supervisor_is_blocked_on_an_unresponsive_peer() {
        let gate = Arc::new(PressureGate::new());
        let passes = Arc::new(AtomicU32::new(0));
        let (hold_peer, peer_never_replies) = mpsc::channel::<()>();
        let (parked_tx, parked_rx) = mpsc::channel::<()>();

        let supervisor = {
            let gate = gate.clone();
            thread::spawn(move || {
                gate.publish_protected([1, 2, 3].into_iter(), None);
                parked_tx.send(()).unwrap();
                // Blocked on an IPC peer that is not answering.
                let _ = peer_never_replies.recv_timeout(Duration::from_secs(5));
                // Even after waking it must not have been needed for passes.
                gate.inline_tick(u64::MAX / 2, INTERVAL, || {});
            })
        };
        parked_rx.recv().unwrap();

        gate.mark_thread_started(0);
        let mut now = 0_u64;
        let mut seen = [0_u64; PROTECTED_PIDS_MAX];
        for _ in 0..10 {
            let snapshot = gate.read_protected(&mut seen).expect("snapshot published before the stall");
            assert_eq!(&seen[..snapshot.len], &[1, 2, 3]);
            let passes = passes.clone();
            assert!(gate.thread_step(now, INTERVAL, move || {
                passes.fetch_add(1, Ordering::Relaxed);
            }));
            now += INTERVAL;
        }
        assert_eq!(passes.load(Ordering::Relaxed), 10);
        assert!(!supervisor.is_finished(), "supervisor is still blocked on the peer");
        drop(hold_peer);
        supervisor.join().unwrap();
    }
}
