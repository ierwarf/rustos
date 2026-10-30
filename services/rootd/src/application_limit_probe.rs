//! Opt-in live application-quota proof, piggybacking on rootd's existing census.
//! No new boot thread/timer or default probe. Accounting and the last surviving
//! thread are observations; the host separately requires all four hog workers'
//! vector-14 retirements. Only the existing safe-subtree rule permits cleanup.
use super::*;

const LIMIT: u64 = rustos_user_abi::syscall::LOADER_APPLICATION_LIMIT_PROBE_PAGES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Snapshot {
    job: u64,
    live_processes: u64,
    process_objects: u64,
    children: u64,
    threads: u64,
    pages: u64,
    limit: u64,
    effective: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Target { handle: u64, job: u64, pid: u64 }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Event {
    Observed { job: u64, pid: u64, pages: u64, limit: u64, threads: u64, free: u64, reserve: u64 },
    Drained { job: u64, pid: u64 },
    Rejected { job: u64, errno: i64 },
}

trait Calls {
    fn open(&mut self, job: u64) -> Result<u64, i64>;
    fn info(&mut self, handle: u64) -> Result<Snapshot, i64>;
    fn safe(&mut self, job: u64) -> bool;
    fn kill(&mut self, handle: u64) -> Result<u64, i64>;
    fn close(&mut self, handle: u64);
    fn emit(&mut self, event: Event);
}

#[derive(Default)]
enum State {
    #[default]
    Waiting,
    Killing(Target),
    Draining(Target),
    Rejected(Target),
    Done,
}

#[derive(Default)]
pub(super) struct Probe { state: State }

impl Probe {
    pub(super) fn poll(&mut self, census: &RustosRootdMemoryBrokerArgs, protected: &[u64], overflow: bool) {
        self.poll_with(census, &mut NativeCalls { protected, overflow });
    }

    fn poll_with(&mut self, census: &RustosRootdMemoryBrokerArgs, calls: &mut impl Calls) {
        self.state = match core::mem::replace(&mut self.state, State::Done) {
            State::Waiting => Self::find(census, calls),
            State::Killing(target) => {
                // Service protection is rechecked, not carried across polls.
                if !calls.safe(target.job) {
                    calls.emit(Event::Rejected { job: target.job, errno: 1 });
                    State::Rejected(target)
                } else {
                    match calls.kill(target.handle) {
                        Ok(0) | Err(3) => State::Draining(target),
                        Ok(_) => State::Killing(target),
                        Err(errno) => {
                            calls.emit(Event::Rejected { job: target.job, errno });
                            State::Rejected(target)
                        }
                    }
                }
            }
            State::Draining(target) => {
                match calls.info(target.handle) {
                    Ok(info) if info.job == target.job && info.live_processes == 0
                        && info.process_objects == 0 && info.threads == 0
                        && info.pages == 0 && info.children == 0 => {
                        calls.close(target.handle);
                        calls.emit(Event::Drained { job: target.job, pid: target.pid });
                        State::Done
                    }
                    Err(3) => {
                        // Kernel job-slot reuse requires both no process object
                        // and no remaining page charge, so stale means drained.
                        calls.close(target.handle);
                        calls.emit(Event::Drained { job: target.job, pid: target.pid });
                        State::Done
                    }
                    Ok(_) => State::Draining(target),
                    Err(errno) => {
                        calls.emit(Event::Rejected { job: target.job, errno });
                        State::Rejected(target)
                    }
                }
            }
            State::Rejected(target) => State::Rejected(target), // retain authority on error
            State::Done => State::Done,
        };
    }

    fn find(census: &RustosRootdMemoryBrokerArgs, calls: &mut impl Calls) -> State {
        if census.min_free_pages == 0 || census.free_pages <= census.min_free_pages.saturating_mul(2) {
            return State::Waiting;
        }
        for entry in census.entries.iter().take(census.entry_count as usize) {
            if entry.process_id == 0 || entry.job == 0 || entry.job == rustos_user_abi::syscall::JOB_ID_ROOT {
                continue;
            }
            let Ok(handle) = calls.open(entry.job) else { continue; };
            let eligible = calls.info(handle).ok().filter(|info| {
                info.job == entry.job && info.limit == LIMIT && info.effective == LIMIT
                    && info.live_processes == 1 && info.process_objects == 1
                    && info.children == 0 && info.threads == 1
                    && info.pages >= LIMIT - 512 && info.pages <= LIMIT + 256
            });
            if let Some(info) = eligible.filter(|_| calls.safe(entry.job)) {
                calls.emit(Event::Observed {
                    job: entry.job, pid: entry.process_id, pages: info.pages, limit: info.limit,
                    threads: info.threads, free: census.free_pages, reserve: census.min_free_pages,
                });
                return State::Killing(Target { handle, job: entry.job, pid: entry.process_id });
            }
            calls.close(handle);
        }
        State::Waiting
    }
}

fn snapshot_from_info(info: jobs::Info) -> Snapshot {
    Snapshot {
        job: info.id, live_processes: info.subtree_processes,
        process_objects: info.usage_processes, children: info.children,
        threads: info.usage_threads, pages: info.usage_memory_pages,
        limit: info.limit_memory_pages, effective: info.effective_memory_pages,
    }
}

struct NativeCalls<'a> { protected: &'a [u64], overflow: bool }
impl Calls for NativeCalls<'_> {
    fn open(&mut self, job: u64) -> Result<u64, i64> {
        use rustos_user_abi::syscall::*;
        jobs::open_subtree(JOB_BOOTSTRAP_ROOT_FD, job, JOB_RIGHT_INSPECT | JOB_RIGHT_KILL)
    }
    fn info(&mut self, handle: u64) -> Result<Snapshot, i64> {
        jobs::info(handle).map(snapshot_from_info)
    }
    fn safe(&mut self, job: u64) -> bool { !self.overflow && job_kill_is_safe(job, self.protected) }
    fn kill(&mut self, handle: u64) -> Result<u64, i64> { jobs::kill(handle) }
    fn close(&mut self, handle: u64) { jobs::close(handle); }
    fn emit(&mut self, event: Event) {
        let mut line = [0_u8; 256];
        let (prefix, fields): (&[u8], &[u64]) = match &event {
            Event::Observed { job, pid, pages, limit, threads, free, reserve } => (
                b"rootd: app limit observed job/pid/pages/limit/threads/free/min:",
                &[*job, *pid, *pages, *limit, *threads, *free, *reserve],
            ),
            Event::Drained { job, pid } => (b"rootd: app limit drained job/pid:", &[*job, *pid]),
            Event::Rejected { job, errno } => (b"rootd: app limit rejected job/errno:", &[*job, *errno as u64]),
        };
        let len = memory_governor::format_line(&mut line, prefix, fields);
        debug_line(&line[..len]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, vec, vec::Vec};

    struct Fake {
        infos: VecDeque<Result<Snapshot, i64>>,
        kills: VecDeque<Result<u64, i64>>,
        safe: bool,
        closed: Vec<u64>,
        events: Vec<Event>,
        kill_calls: usize,
    }
    impl Calls for Fake {
        fn open(&mut self, job: u64) -> Result<u64, i64> { assert_eq!(job, 80); Ok(8) }
        fn info(&mut self, handle: u64) -> Result<Snapshot, i64> {
            assert_eq!(handle, 8); self.infos.pop_front().expect("unexpected info")
        }
        fn safe(&mut self, job: u64) -> bool { assert_eq!(job, 80); self.safe }
        fn kill(&mut self, handle: u64) -> Result<u64, i64> {
            assert_eq!(handle, 8); self.kill_calls += 1; self.kills.pop_front().expect("unexpected kill")
        }
        fn close(&mut self, handle: u64) { self.closed.push(handle); }
        fn emit(&mut self, event: Event) { self.events.push(event); }
    }
    fn snapshot() -> Snapshot {
        Snapshot { job: 80, live_processes: 1, process_objects: 1,
            children: 0, threads: 1, pages: LIMIT, limit: LIMIT, effective: LIMIT }
    }
    fn census() -> RustosRootdMemoryBrokerArgs {
        let mut census = RustosRootdMemoryBrokerArgs::default();
        census.free_pages = 100_000;
        census.min_free_pages = 4096;
        census.entry_count = 1;
        census.entries[0].process_id = 41;
        census.entries[0].job = 80;
        census
    }
    fn fake(infos: Vec<Result<Snapshot, i64>>, kills: Vec<Result<u64, i64>>) -> Fake {
        Fake { infos: infos.into(), kills: kills.into(), safe: true,
            closed: Vec::new(), events: Vec::new(), kill_calls: 0 }
    }

    #[test]
    fn observation_requires_the_exact_limit_last_thread_accounting_and_headroom() {
        for bad in [
            Snapshot { job: 81, ..snapshot() }, Snapshot { limit: LIMIT + 1, ..snapshot() },
            Snapshot { effective: LIMIT - 1, ..snapshot() }, Snapshot { live_processes: 2, ..snapshot() },
            Snapshot { process_objects: 2, ..snapshot() }, Snapshot { process_objects: 0, ..snapshot() },
            Snapshot { children: 1, ..snapshot() }, Snapshot { threads: 2, ..snapshot() },
            Snapshot { pages: LIMIT - 513, ..snapshot() }, Snapshot { pages: LIMIT + 257, ..snapshot() },
        ] {
            let mut probe = Probe::default();
            let mut calls = fake(vec![Ok(bad)], vec![]);
            probe.poll_with(&census(), &mut calls);
            assert!(matches!(probe.state, State::Waiting));
            assert!(calls.events.is_empty());
            assert_eq!(calls.closed, [8]);
        }
        let mut low = census(); low.free_pages = low.min_free_pages * 2;
        let mut calls = fake(vec![], vec![]);
        Probe::default().poll_with(&low, &mut calls);
        assert!(calls.events.is_empty());
    }

    #[test]
    fn unreaped_object_without_live_members_retains_drain_custody() {
        let info = jobs::info_from_reply(rustos_user_abi::syscall::RustosJobArgs {
            job: 80, subtree_processes: 0, usage_processes: 1,
            usage_threads: 0, usage_memory_pages: 0,
            ..rustos_user_abi::syscall::RustosJobArgs::default()
        });
        let mut probe = Probe { state: State::Draining(Target { handle: 8, job: 80, pid: 41 }) };
        let mut calls = fake(vec![Ok(snapshot_from_info(info))], vec![]);
        probe.poll_with(&census(), &mut calls);
        if !matches!(probe.state, State::Draining(_)) {
            std::eprintln!("unreaped object lost custody: closed={:?} events={:?}", calls.closed, calls.events);
        }
        assert!(matches!(probe.state, State::Draining(_)));
        assert!(calls.closed.is_empty());
        assert!(calls.events.is_empty());
    }

    #[test]
    fn partial_cleanup_waits_for_both_process_objects_and_charged_pages() {
        let mut probe = Probe::default();
        let mut calls = fake(vec![Ok(snapshot()),
            Ok(Snapshot { live_processes: 0, process_objects: 0, threads: 0, pages: 1, ..snapshot() }),
            Ok(Snapshot { live_processes: 0, process_objects: 1, threads: 0, pages: 0, ..snapshot() }),
            Ok(Snapshot { live_processes: 0, process_objects: 0, threads: 0, pages: 0, ..snapshot() }),
        ], vec![Ok(2), Ok(0)]);
        for _ in 0..5 { probe.poll_with(&census(), &mut calls); }
        assert!(matches!(probe.state, State::Draining(_)));
        assert!(calls.closed.is_empty());
        assert_eq!(calls.events.len(), 1);
        probe.poll_with(&census(), &mut calls);
        assert!(matches!(probe.state, State::Done));
        assert_eq!(calls.closed, [8]);
        assert_eq!(calls.events.last(), Some(&Event::Drained { job: 80, pid: 41 }));
        probe.poll_with(&census(), &mut calls);
        assert_eq!(calls.closed, [8]);
    }

    #[test]
    fn protected_jobs_are_never_observed_and_protection_is_rechecked_before_kill() {
        let mut probe = Probe::default();
        let mut calls = fake(vec![Ok(snapshot()), Ok(snapshot())], vec![]);
        calls.safe = false;
        probe.poll_with(&census(), &mut calls);
        assert!(calls.events.is_empty());
        calls.safe = true;
        probe.poll_with(&census(), &mut calls);
        calls.safe = false;
        probe.poll_with(&census(), &mut calls);
        assert!(matches!(probe.state, State::Rejected(_)));
        assert_eq!(calls.kill_calls, 0);
        assert_eq!(calls.closed, [8]); // only the initially rejected candidate
    }

    #[test]
    fn denied_kill_retains_custody_and_stale_after_cleanup_is_drained() {
        let mut probe = Probe::default();
        let mut calls = fake(vec![Ok(snapshot())], vec![Err(1)]);
        probe.poll_with(&census(), &mut calls);
        probe.poll_with(&census(), &mut calls);
        assert!(matches!(probe.state, State::Rejected(_)));
        assert!(calls.closed.is_empty());
        assert_eq!(calls.events.last(), Some(&Event::Rejected { job: 80, errno: 1 }));
        let mut probe = Probe::default();
        let mut calls = fake(vec![Ok(snapshot()), Err(3)], vec![Ok(0)]);
        for _ in 0..3 { probe.poll_with(&census(), &mut calls); }
        assert!(matches!(probe.state, State::Done));
        assert_eq!(calls.closed, [8]);
    }

    #[test]
    fn probe_is_opt_in_and_reuses_the_existing_census_without_a_boot_thread() {
        let main = include_str!("main.rs");
        let census = main.split_once("fn supervise_memory_pressure(").unwrap().1;
        assert!(census.contains("if option_env!(\"RUSTOS_ROOTD_APP_LIMIT_PROBE\") == Some(\"1\")"));
        assert!(census.contains("application_limit_probe.poll(&census, protected, protected_overflow)"));
        assert!(!include_str!("application_limit_probe.rs").split("#[cfg(test)]").next().unwrap().contains("spawn_loader_worker_thread"));
    }
}
