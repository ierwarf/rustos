//! Loader-owned application job custody, separate from process-supervisor PIDs.
//!
//! A rootd-provisioned realm is the only parent authority. Each launch retains
//! its child descriptor before setting limits or preparing an image. Failed
//! launches remain owned until bounded retirement finishes; a successful spawn
//! is recorded only after inspecting its exact job. No authority goes to apps.
extern crate alloc;

use alloc::vec::Vec;
use rustos_user_abi::syscall::*;

/// Authority delegated for application supervision, never root or policy grants.
pub const REALM_RIGHTS: u32 = LOADER_APPLICATION_REALM_RIGHTS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub processes: u64,
    pub threads: u64,
    pub memory_pages: u64,
}

/// Finite runtime-launch policy. The opt-in proof only tightens the hog's
/// budget; it cannot grant authority or lower the UI/service launch budgets.
pub fn runtime_limits(limit_probe: bool, exec_path: &[u8]) -> Limits {
    Limits {
        processes: 8, threads: 64,
        memory_pages: if limit_probe && exec_path == b"/apps/oomhog/oomhog.elf" {
            LOADER_APPLICATION_LIMIT_PROBE_PAGES
        } else { 524_288 },
    }
}

/// Injectable syscall edge; errors are positive Linux errno values.
pub trait Jobs {
    fn call(&mut self, args: RustosJobArgs) -> Result<RustosJobArgs, i32>;
    fn close(&mut self, handle: u64);
}

/// Production syscall implementation. Host witnesses use `Jobs` fakes instead.
pub struct NativeJobs;

impl Jobs for NativeJobs {
    fn call(&mut self, mut args: RustosJobArgs) -> Result<RustosJobArgs, i32> {
        // SAFETY: the writable argument block is live, aligned and exclusively
        // borrowed for the complete synchronous job syscall.
        let status = unsafe {
            rustos_svc_runtime::syscall::syscall1(SYS_RUSTOS_JOB, &mut args as *mut _ as u64)
        };
        if status < 0 {
            return Err((-status) as i32);
        }
        if args.op == JOB_OP_SET_LIMITS {
            // Runtime application Jobs receive a finite client-funded session
            // ceiling at the same preparation boundary as process and page
            // limits. This is the live enforcement path, not only a model.
            let budget = args.limit_processes.saturating_mul(4).clamp(1, 64);
            let mut sessions = RustosJobSessionArgs {
                abi_version: JOB_SESSION_ABI_VERSION,
                op: JOB_SESSION_OP_SET_LIMIT,
                job_handle: args.job_handle,
                limit: budget,
                ..RustosJobSessionArgs::default()
            };
            let admission = unsafe {
                rustos_svc_runtime::syscall::syscall1(
                    SYS_RUSTOS_JOB_SESSION,
                    &mut sessions as *mut _ as u64,
                )
            };
            if admission < 0 {
                return Err((-admission) as i32);
            }
        }
        // The application Job's resident limit is already enforced by the
        // kernel frame ledger. Its high watermark adds nonblocking pressure
        // telemetry at 80% and no implicit memory.min protection for apps.
        if args.op == JOB_OP_SET_LIMITS && args.limit_memory_pages != JOB_LIMIT_UNSET {
            let mut memory = RustosJobMemoryQosArgs {
                abi_version: JOB_MEMORY_QOS_ABI_VERSION,
                op: JOB_MEMORY_QOS_OP_SET,
                job_handle: args.job_handle,
                high_pages: args.limit_memory_pages.saturating_mul(4) / 5,
                min_pages: 0,
                ..RustosJobMemoryQosArgs::default()
            };
            let qos_status = unsafe {
                rustos_svc_runtime::syscall::syscall1(
                    SYS_RUSTOS_JOB_MEMORY_QOS,
                    &mut memory as *mut _ as u64,
                )
            };
            if qos_status < 0 {
                return Err((-qos_status) as i32);
            }
        }
        Ok(args)
    }
    fn close(&mut self, handle: u64) {
        // SAFETY: close has no user pointers and consumes this owned descriptor.
        let _ = unsafe { rustos_svc_runtime::syscall::syscall1(3, handle) };
    }
}

#[derive(Clone, Copy)]
struct Authority {
    handle: u64,
    id: u64,
}

struct Child {
    authority: Authority,
    leader: Option<u64>,
    retiring: bool,
}

#[derive(Default)]
pub struct ApplicationJobs {
    realm: Option<Authority>,
    children: Vec<Child>,
}

fn request(op: u16, handle: u64) -> RustosJobArgs {
    RustosJobArgs { abi_version: JOB_ABI_VERSION, op, job_handle: handle, ..RustosJobArgs::default() }
}

impl ApplicationJobs {
    /// Called only after live rootd-owner authentication and descriptor-count
    /// validation. The receive loop still owns/closes `candidate`; we duplicate
    /// it rather than stealing receive custody. Repeated provisioning of the
    /// same incarnation is idempotent; replacing a live realm is refused.
    pub fn install(&mut self, candidate: u64, jobs: &mut impl Jobs) -> Result<u64, i32> {
        let info = jobs.call(request(JOB_OP_GET_INFO, candidate))?;
        if info.state != JOB_STATE_LIVE || info.job == JOB_ID_ROOT
            || info.rights != REALM_RIGHTS {
            return Err(1);
        }
        if let Some(realm) = self.realm {
            if realm.id == info.job { return Ok(realm.id); }
            return Err(16);
        }
        let mut duplicate = request(JOB_OP_DUPLICATE_HANDLE, candidate);
        duplicate.rights = REALM_RIGHTS;
        let retained = jobs.call(duplicate)?;
        self.realm = Some(Authority { handle: retained.result_handle, id: info.job });
        Ok(info.job)
    }

    /// Retain before setting limits. In particular a failed SET_LIMITS cannot
    /// strand a newly allocated job, even when its first kill is incomplete.
    pub fn prepare(&mut self, limits: Limits, jobs: &mut impl Jobs) -> Result<u64, i32> {
        // Provisioning/replacement is transient. EACCES would make runtimed
        // permanently abandon a valid launch before rootd can bind the realm.
        let realm = self.realm.ok_or(11)?;
        self.children.try_reserve(1).map_err(|_| 12)?;
        let child = jobs.call(request(JOB_OP_CREATE_CHILD, realm.handle))?;
        let handle = child.result_handle;
        self.children.push(Child {
            authority: Authority { handle, id: child.job }, leader: None, retiring: false,
        });
        let mut set = request(JOB_OP_SET_LIMITS, handle);
        set.limit_mask = JOB_LIMIT_PROCESSES | JOB_LIMIT_THREADS | JOB_LIMIT_MEMORY_PAGES;
        set.limit_processes = limits.processes;
        set.limit_threads = limits.threads;
        set.limit_memory_pages = limits.memory_pages;
        if let Err(errno) = jobs.call(set) {
            self.abort(handle, jobs);
            return Err(errno);
        }
        Ok(handle)
    }

    /// Placement is checked before the launch can be acknowledged. Inspection
    /// returns an identity, not new authority or an allocating process owner.
    pub fn published(&mut self, handle: u64, pid: u64, jobs: &mut impl Jobs) -> Result<u64, i32> {
        let child = self.children.iter_mut().find(|child| child.authority.handle == handle)
            .ok_or(9)?;
        if child.retiring || child.leader.is_some() || pid == 0 { return Err(22); }
        let mut inspect = request(JOB_OP_PROCESS_JOB, handle);
        inspect.pid = pid;
        let info = jobs.call(inspect)?;
        if info.job != child.authority.id { return Err(1); }
        child.leader = Some(pid);
        Ok(child.authority.id)
    }

    /// A failed prepare/commit/reply starts retirement, but does not give up
    /// authority on a partial kill or any non-stale error.
    pub fn abort(&mut self, handle: u64, jobs: &mut impl Jobs) {
        if let Some(child) = self.children.iter_mut().find(|child| child.authority.handle == handle) {
            child.retiring = true;
        }
        self.collect(jobs);
    }

    /// Demand-driven collection: no new boot-time thread or periodic probe.
    /// Once the reaped leader disappears, retire its descendants as a cohort.
    /// Empty jobs are explicitly killed so their slots can be recycled.
    pub fn collect(&mut self, jobs: &mut impl Jobs) {
        self.children.retain_mut(|child| {
            let handle = child.authority.handle;
            if !child.retiring {
                if let Some(pid) = child.leader {
                    let mut inspect = request(JOB_OP_PROCESS_JOB, handle);
                    inspect.pid = pid;
                    match jobs.call(inspect) {
                        Err(3) => child.retiring = true,
                        Err(_) => return true,
                        Ok(_) => {}
                    }
                }
                if !child.retiring {
                    match jobs.call(request(JOB_OP_GET_INFO, handle)) {
                        Ok(info) if info.subtree_processes == 0 && info.children == 0 => {
                            child.retiring = true;
                        }
                        Err(3) => { jobs.close(handle); return false; }
                        _ => return true,
                    }
                }
            }
            match jobs.call(request(JOB_OP_KILL, handle)) {
                Ok(info) if info.remaining == 0 => { jobs.close(handle); false }
                Err(3) => { jobs.close(handle); false }
                _ => true,
            }
        });
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::VecDeque;
    use std::vec;

    struct Fake {
        replies: VecDeque<(u16, Result<RustosJobArgs, i32>)>,
        calls: Vec<RustosJobArgs>,
        closed: Vec<u64>,
    }
    impl Jobs for Fake {
        fn call(&mut self, args: RustosJobArgs) -> Result<RustosJobArgs, i32> {
            let (op, reply) = self.replies.pop_front().expect("unexpected job call");
            assert_eq!(args.op, op);
            self.calls.push(args);
            reply
        }
        fn close(&mut self, handle: u64) { self.closed.push(handle); }
    }
    fn info(id: u64, handle: u64) -> RustosJobArgs {
        RustosJobArgs { job: id, result_handle: handle, state: JOB_STATE_LIVE,
            rights: REALM_RIGHTS, ..RustosJobArgs::default() }
    }
    fn fake(replies: Vec<(u16, Result<RustosJobArgs, i32>)>) -> Fake {
        Fake { replies: replies.into(), calls: Vec::new(), closed: Vec::new() }
    }
    fn installed() -> ApplicationJobs {
        ApplicationJobs { realm: Some(Authority { handle: 7, id: 70 }), children: Vec::new() }
    }
    const LIMITS: Limits = Limits { processes: 8, threads: 64, memory_pages: 32_768 };

    #[test]
    fn limit_proof_only_tightens_the_explicit_hog_and_is_off_by_default() {
        let hog = b"/apps/oomhog/oomhog.elf";
        assert_eq!(runtime_limits(false, hog), Limits { processes: 8, threads: 64, memory_pages: 524_288 });
        assert_eq!(runtime_limits(true, hog), LIMITS);
        for path in [b"/services/uiserver/uiserver.elf".as_slice(), b"/apps/oomhog/oomhog.elf.extra", b""] {
            assert_eq!(runtime_limits(true, path), runtime_limits(false, hog));
        }
    }

    #[test]
    fn realm_install_is_attenuated_idempotent_and_never_replaces_a_live_realm() {
        let mut state = ApplicationJobs::default();
        let mut jobs = fake(vec![
            (JOB_OP_GET_INFO, Ok(info(70, 0))),
            (JOB_OP_DUPLICATE_HANDLE, Ok(info(70, 7))),
            (JOB_OP_GET_INFO, Ok(info(70, 0))),
            (JOB_OP_GET_INFO, Ok(info(80, 0))),
        ]);
        assert_eq!(state.install(3, &mut jobs), Ok(70));
        assert_eq!((jobs.calls[1].job_handle, jobs.calls[1].rights), (3, REALM_RIGHTS));
        assert_eq!(state.install(4, &mut jobs), Ok(70));
        assert_eq!(state.install(5, &mut jobs), Err(16));
        assert_eq!(state.realm.unwrap().handle, 7);
        assert!(jobs.closed.is_empty(), "the receive loop owns incoming descriptors");
        assert!(jobs.replies.is_empty());
    }

    #[test]
    fn root_stale_or_overprivileged_realm_is_refused_without_duplication() {
        for bad in [
            RustosJobArgs { job: JOB_ID_ROOT, ..info(70, 0) },
            RustosJobArgs { state: JOB_STATE_KILLING, ..info(70, 0) },
            RustosJobArgs { rights: REALM_RIGHTS | JOB_RIGHT_SET_POLICY, ..info(70, 0) },
            RustosJobArgs { rights: REALM_RIGHTS & !JOB_RIGHT_KILL, ..info(70, 0) },
        ] {
            let mut state = ApplicationJobs::default();
            let mut jobs = fake(vec![(JOB_OP_GET_INFO, Ok(bad))]);
            assert_eq!(state.install(3, &mut jobs), Err(1));
            assert!(state.realm.is_none());
            assert!(jobs.replies.is_empty());
        }
    }

    #[test]
    fn missing_realm_and_failed_duplication_never_publish_authority() {
        let mut state = ApplicationJobs::default();
        let mut jobs = fake(vec![
            (JOB_OP_GET_INFO, Ok(info(70, 0))),
            (JOB_OP_DUPLICATE_HANDLE, Err(24)),
        ]);
        assert_eq!(state.prepare(LIMITS, &mut jobs), Err(11));
        assert_eq!(state.install(3, &mut jobs), Err(24));
        assert!(state.realm.is_none());
        assert!(state.children.is_empty());
        assert!(jobs.closed.is_empty());
    }

    #[test]
    fn runtime_placement_keeps_authentication_prepare_and_reply_custody_ordered() {
        // Source wiring projection. Real user-copy, broker publication and
        // terminal reply acceptance still require the signed boot gates.
        let ingress = include_str!("application_launch.rs");
        let configure = ingress.split_once("pub(super) fn configure(").unwrap().1;
        assert!(configure.find("authorize_loader_operation(request.op, sender_pid)").unwrap()
            < configure.find("applications.install(descriptors[0] as u64").unwrap());
        let spawn = ingress.split_once("pub(super) fn spawn(").unwrap().1;
        assert!(spawn.contains("validate_service_owner(IPC_SERVICE_SESSIOND, sender_pid)"));
        assert!(spawn.find("if placement.is_some()").unwrap()
            < spawn.find("applications.prepare(limits").unwrap());
        assert!(spawn.find("applications.prepare(limits").unwrap()
            < spawn.find("handle_placed_request(request, Some(handle))").unwrap());
        assert!(spawn.find("applications.published(handle").unwrap()
            < spawn.find("handled.application_job = Some(handle)").unwrap());
        let main = include_str!("main.rs");
        let serve = main.split_once("fn serve(").unwrap().1;
        assert!(serve.contains("if handled.application_job.is_none()"));
        let failure = serve.split_once("if reply < 0 {").unwrap().1;
        assert!(failure.contains("applications.abort(handle"));
        let request = main.split_once("fn handle_request(").unwrap().1;
        assert!(request.find("authorize_loader_operation(request.op, sender_pid)").unwrap()
            < request.find("application_launch::spawn(request, sender_pid").unwrap());
    }

    #[test]
    fn every_child_is_retained_and_all_three_limits_precede_placement() {
        let mut state = installed();
        let mut jobs = fake(vec![
            (JOB_OP_CREATE_CHILD, Ok(info(80, 8))),
            (JOB_OP_SET_LIMITS, Ok(info(80, 0))),
            (JOB_OP_PROCESS_JOB, Ok(info(80, 0))),
        ]);
        assert_eq!(state.prepare(LIMITS, &mut jobs), Ok(8));
        assert_eq!(state.children.len(), 1);
        let set = &jobs.calls[1];
        assert_eq!(set.job_handle, 8);
        assert_eq!(set.limit_mask, JOB_LIMIT_PROCESSES | JOB_LIMIT_THREADS | JOB_LIMIT_MEMORY_PAGES);
        assert_eq!((set.limit_processes, set.limit_threads, set.limit_memory_pages), (8, 64, 32_768));
        assert_eq!(state.published(8, 41, &mut jobs), Ok(80));
        assert_eq!(jobs.calls[2].pid, 41);
        assert_eq!(state.children[0].leader, Some(41));
        assert!(jobs.closed.is_empty());
    }

    #[test]
    fn failed_limits_keep_authority_through_partial_and_denied_retirement() {
        let mut state = installed();
        let mut jobs = fake(vec![
            (JOB_OP_CREATE_CHILD, Ok(info(80, 8))),
            (JOB_OP_SET_LIMITS, Err(22)),
            (JOB_OP_KILL, Ok(RustosJobArgs { remaining: 1, ..info(80, 0) })),
            (JOB_OP_KILL, Err(1)),
            (JOB_OP_KILL, Ok(info(80, 0))),
        ]);
        assert_eq!(state.prepare(LIMITS, &mut jobs), Err(22));
        assert_eq!(state.children.len(), 1);
        state.collect(&mut jobs);
        assert_eq!(state.children.len(), 1);
        assert!(jobs.closed.is_empty());
        state.collect(&mut jobs);
        assert!(state.children.is_empty());
        assert_eq!(jobs.closed, vec![8]);
        state.collect(&mut jobs);
        assert_eq!(jobs.closed, vec![8]);
    }

    #[test]
    fn publication_refuses_a_foreign_job_and_lost_reply_retires_the_whole_job() {
        let mut state = installed();
        let mut jobs = fake(vec![
            (JOB_OP_CREATE_CHILD, Ok(info(80, 8))),
            (JOB_OP_SET_LIMITS, Ok(info(80, 0))),
            (JOB_OP_PROCESS_JOB, Ok(info(90, 0))),
            (JOB_OP_KILL, Err(3)),
        ]);
        let handle = state.prepare(LIMITS, &mut jobs).unwrap();
        assert_eq!(state.published(handle, 41, &mut jobs), Err(1));
        assert_eq!(state.children[0].leader, None);
        state.abort(handle, &mut jobs);
        assert!(state.children.is_empty());
        assert_eq!(jobs.closed, vec![8]);
    }

    #[test]
    fn reaped_leader_retires_descendants_but_transient_inspection_error_does_not() {
        let mut state = installed();
        state.children.push(Child {
            authority: Authority { handle: 8, id: 80 }, leader: Some(41), retiring: false,
        });
        let mut jobs = fake(vec![
            (JOB_OP_PROCESS_JOB, Err(5)),
            (JOB_OP_PROCESS_JOB, Err(3)),
            (JOB_OP_KILL, Ok(RustosJobArgs { remaining: 2, ..info(80, 0) })),
            (JOB_OP_KILL, Ok(info(80, 0))),
        ]);
        state.collect(&mut jobs);
        assert!(!state.children[0].retiring);
        state.collect(&mut jobs);
        assert!(state.children[0].retiring);
        state.collect(&mut jobs);
        assert!(state.children.is_empty());
        assert_eq!(jobs.closed, vec![8]);
    }
}
