//! Loader-owned application job custody, separate from process-supervisor PIDs.
//!
//! A rootd-provisioned realm is the only parent authority. Each launch retains
//! its child descriptor before setting limits or preparing an image. Failed
//! launches remain owned until bounded retirement finishes; a successful spawn
//! is recorded only after inspecting its exact job. No authority goes to apps.
//!
//! Limits are *granted* here, not guessed: `prepare` observes the machine
//! (`Jobs::observe`), sums what the live Jobs it still holds were granted, and
//! asks `application_grant::grant` to resolve the application's request. A Job
//! that is not retired keeps its grant counted until the kernel confirms it
//! empty and the descriptor is closed (`collect`), so the sum of grants never
//! exceeds the application budget even while a Job is being torn down.
extern crate alloc;

use alloc::vec::Vec;
use crate::application_grant::{
    budget_plan, grant, BudgetPlan, Committed, Grant, Limit, Machine, Refusal,
};
use rustos_user_abi::application_request::ApplicationRequest;
use rustos_user_abi::linux::{LinuxSysinfo, SYS_SYSINFO};
use rustos_user_abi::syscall::*;

/// Authority delegated for application supervision, never root or policy grants.
pub const REALM_RIGHTS: u32 = LOADER_APPLICATION_REALM_RIGHTS;

/// The three counted limits installed on a Job (`SYS_RUSTOS_JOB` SET_LIMITS).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub processes: u64,
    pub threads: u64,
    /// Job-wide page limit (granted processes x pages per process).
    pub memory_pages: u64,
}

/// Why a Job could not be prepared. `refusal` is set when the request could not
/// be granted in full (the limit that refused is named); otherwise `errno` is a
/// syscall or custody failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Failure {
    pub errno: i32,
    pub refusal: Option<Refusal>,
}

impl From<i32> for Failure {
    fn from(errno: i32) -> Self { Self { errno, refusal: None } }
}

impl From<Refusal> for Failure {
    fn from(refusal: Refusal) -> Self { Self { errno: refusal.errno, refusal: Some(refusal) } }
}

/// A prepared Job: the retained handle and what it was granted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Prepared {
    pub handle: u64,
    pub grant: Grant,
    pub limits: Limits,
}

/// Injectable syscall edge; errors are positive Linux errno values.
pub trait Jobs {
    fn call(&mut self, args: RustosJobArgs) -> Result<RustosJobArgs, i32>;
    fn close(&mut self, handle: u64);
    /// Observes the running machine: `SYS_RUSTOS_TASK_CAPACITY` and physical
    /// memory. Taken once per launch, immediately before the grant.
    fn observe(&mut self) -> Result<Machine, i32>;
    /// Installs the session ceiling, `memory.high` threshold and kernel byte
    /// budgets derived from the grant on a Job whose limits were just set.
    fn enforce_budgets(&mut self, handle: u64, plan: &BudgetPlan) -> Result<(), i32>;
}

/// Production syscall implementation. Host witnesses use `Jobs` fakes instead.
pub struct NativeJobs;

fn status_errno(status: i64) -> Result<(), i32> {
    if status < 0 { Err((-status) as i32) } else { Ok(()) }
}

impl Jobs for NativeJobs {
    fn call(&mut self, mut args: RustosJobArgs) -> Result<RustosJobArgs, i32> {
        // SAFETY: the writable argument block is live, aligned and exclusively
        // borrowed for the complete synchronous job syscall.
        let status = unsafe {
            rustos_svc_runtime::syscall::syscall1(SYS_RUSTOS_JOB, &mut args as *mut _ as u64)
        };
        status_errno(status)?;
        Ok(args)
    }

    fn close(&mut self, handle: u64) {
        // SAFETY: close has no user pointers and consumes this owned descriptor.
        let _ = unsafe { rustos_svc_runtime::syscall::syscall1(3, handle) };
    }

    fn observe(&mut self) -> Result<Machine, i32> {
        let mut capacity = RustosTaskCapacityArgs::request();
        // SAFETY: the writable argument block is live, aligned and exclusively
        // borrowed for the complete synchronous read-only syscall.
        let status = unsafe {
            rustos_svc_runtime::syscall::syscall1(
                SYS_RUSTOS_TASK_CAPACITY, &mut capacity as *mut _ as u64,
            )
        };
        status_errno(status)?;
        let mut info = LinuxSysinfo::default();
        // SAFETY: as above; `sysinfo` fills exactly one `LinuxSysinfo`.
        let status = unsafe {
            rustos_svc_runtime::syscall::syscall1(SYS_SYSINFO, &mut info as *mut _ as u64)
        };
        status_errno(status)?;
        let ram_bytes = info.totalram.saturating_mul(u64::from(info.mem_unit.max(1)));
        Machine::observe(&capacity, ram_bytes)
    }

    fn enforce_budgets(&mut self, handle: u64, plan: &BudgetPlan) -> Result<(), i32> {
        // Runtime application Jobs receive a finite client-funded session
        // ceiling at the same preparation boundary as process and page
        // limits. This is the live enforcement path, not only a model.
        let mut sessions = RustosJobSessionArgs {
            abi_version: JOB_SESSION_ABI_VERSION,
            op: JOB_SESSION_OP_SET_LIMIT,
            job_handle: handle,
            limit: plan.sessions,
            ..RustosJobSessionArgs::default()
        };
        // SAFETY: live exclusive argument block for one synchronous syscall.
        status_errno(unsafe {
            rustos_svc_runtime::syscall::syscall1(
                SYS_RUSTOS_JOB_SESSION, &mut sessions as *mut _ as u64,
            )
        })?;
        // The application Job's resident limit is already enforced by the
        // kernel frame ledger. Its high watermark adds nonblocking pressure
        // telemetry and no implicit memory.min protection for apps.
        let mut memory = RustosJobMemoryQosArgs {
            abi_version: JOB_MEMORY_QOS_ABI_VERSION,
            op: JOB_MEMORY_QOS_OP_SET,
            job_handle: handle,
            high_pages: plan.memory_high_pages,
            min_pages: 0,
            ..RustosJobMemoryQosArgs::default()
        };
        // SAFETY: as above.
        status_errno(unsafe {
            rustos_svc_runtime::syscall::syscall1(
                SYS_RUSTOS_JOB_MEMORY_QOS, &mut memory as *mut _ as u64,
            )
        })?;
        // Exact ProcessObject Box quota follows the same Job handle and fails
        // launch on refusal. INSPECT obtains the actual target kernel's object
        // size: no guessed host-compiler struct layout.
        let mut kmem = RustosJobKmemArgs {
            abi_version: JOB_KMEM_ABI_VERSION,
            op: JOB_KMEM_OP_INSPECT,
            job_handle: handle,
            ..RustosJobKmemArgs::default()
        };
        // SAFETY: as above.
        status_errno(unsafe {
            rustos_svc_runtime::syscall::syscall1(SYS_RUSTOS_JOB_KMEM, &mut kmem as *mut _ as u64)
        })?;
        if kmem.object_size_bytes == 0 || kmem.job == 0 {
            return Err(71);
        }
        kmem.op = JOB_KMEM_OP_SET_LIMIT;
        kmem.process_object_max_bytes = plan.processes.saturating_mul(kmem.object_size_bytes);
        kmem.usage_process_object_bytes = 0;
        kmem.object_size_bytes = 0;
        kmem.job = 0;
        // SAFETY: as above.
        status_errno(unsafe {
            rustos_svc_runtime::syscall::syscall1(SYS_RUSTOS_JOB_KMEM, &mut kmem as *mut _ as u64)
        })?;
        // Descriptor-table slots and endpoint queues are their own
        // hierarchical byte classes; each is refunded as its owner dies.
        for (class_id, budget) in plan.classes {
            let mut class = RustosJobKmemArgs {
                abi_version: JOB_KMEM_ABI_VERSION,
                op: JOB_KMEM_OP_SET_CLASS_LIMIT,
                flags: class_id,
                job_handle: handle,
                process_object_max_bytes: budget,
                ..RustosJobKmemArgs::default()
            };
            // SAFETY: as above.
            status_errno(unsafe {
                rustos_svc_runtime::syscall::syscall1(
                    SYS_RUSTOS_JOB_KMEM, &mut class as *mut _ as u64,
                )
            })?;
        }
        Ok(())
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
    /// What this Job was granted; counted against the application budgets until
    /// the Job is reclaimed (the child leaves `children` only then).
    granted_processes: u64,
    granted_threads: u64,
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
    /// Readiness only: prepare still revalidates the live typed realm authority.
    pub fn realm_configured(&self) -> bool { self.realm.is_some() }

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

    /// What the live Jobs still hold. A Job counts in full until the kernel
    /// reports it gone (`ESRCH`) or `collect` drops it, including while it is
    /// being retired; a Job whose state cannot be read counts in full, never as
    /// zero. `unused_threads` is the granted-but-uncreated part.
    fn committed(&self, jobs: &mut impl Jobs) -> Committed {
        let mut held = Committed::default();
        for child in &self.children {
            let used = match jobs.call(request(JOB_OP_GET_INFO, child.authority.handle)) {
                Err(3) => continue,
                Ok(info) => info.usage_threads.min(child.granted_threads),
                Err(_) => 0,
            };
            held.processes = held.processes.saturating_add(child.granted_processes);
            held.threads = held.threads.saturating_add(child.granted_threads);
            held.unused_threads = held.unused_threads
                .saturating_add(child.granted_threads - used);
        }
        held
    }

    /// Grants the application's request against the observed capacity, then
    /// retains the child before setting limits. In particular a failed
    /// SET_LIMITS cannot strand a newly allocated job, even when its first kill
    /// is incomplete. `memory_probe_pages` only tightens the Job's page limit
    /// (the opt-in limit proof).
    ///
    /// A request that cannot be granted in full is refused before any Job is
    /// created (`Failure::refusal` names the limit); nothing is clamped.
    pub fn prepare(
        &mut self, request_for: &ApplicationRequest, memory_probe_pages: Option<u64>,
        jobs: &mut impl Jobs,
    ) -> Result<Prepared, Failure> {
        // Provisioning/replacement is transient. EACCES would make runtimed
        // permanently abandon a valid launch before rootd can bind the realm.
        let realm = self.realm.ok_or(11)?;
        self.children.try_reserve(1).map_err(|_| 12)?;
        let machine = jobs.observe()?;
        let committed = self.committed(jobs);
        let granted = grant(&machine, &committed, request_for)?;
        let limits = Limits {
            processes: granted.processes,
            threads: granted.threads,
            memory_pages: memory_probe_pages
                .map_or(granted.job_memory_pages(), |probe| probe.min(granted.job_memory_pages())),
        };
        let child = jobs.call(request(JOB_OP_CREATE_CHILD, realm.handle))?;
        let handle = child.result_handle;
        self.children.push(Child {
            authority: Authority { handle, id: child.job }, leader: None, retiring: false,
            granted_processes: granted.processes, granted_threads: granted.threads,
        });
        let mut set = request(JOB_OP_SET_LIMITS, handle);
        set.limit_mask = JOB_LIMIT_PROCESSES | JOB_LIMIT_THREADS | JOB_LIMIT_MEMORY_PAGES;
        set.limit_processes = limits.processes;
        set.limit_threads = limits.threads;
        set.limit_memory_pages = limits.memory_pages;
        let in_force = match jobs.call(set) {
            Ok(in_force) => in_force,
            Err(errno) => {
                self.abort(handle, jobs);
                return Err(errno.into());
            }
        };
        // The kernel accepts a child limit above an ancestor's and reports the
        // clamped value as `effective`. A grant the hierarchy silently narrows
        // is not a grant.
        for (requested, effective) in [
            (limits.processes, in_force.effective_processes),
            (limits.threads, in_force.effective_threads),
            (limits.memory_pages, in_force.effective_memory_pages),
        ] {
            if effective < requested {
                self.abort(handle, jobs);
                return Err(Refusal {
                    errno: 11, limit: Limit::AncestorJob, requested, available: effective,
                }.into());
            }
        }
        let plan = budget_plan(limits.processes, limits.threads, limits.memory_pages, &machine);
        if let Err(errno) = jobs.enforce_budgets(handle, &plan) {
            self.abort(handle, jobs);
            return Err(errno.into());
        }
        Ok(Prepared { handle, grant: granted, limits })
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

/// Pages of the opt-in limit proof: only the explicit hog's Job is tightened to
/// this, and only in builds that set `RUSTOS_LOADERD_LIMIT_PROBE=1`.
pub fn memory_probe_pages(limit_probe: bool, exec_path: &[u8]) -> Option<u64> {
    (limit_probe && exec_path == b"/apps/oomhog/oomhog.elf")
        .then_some(LOADER_APPLICATION_LIMIT_PROBE_PAGES)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::application_grant::{PAGE_BYTES, PAGES_PER_TABLE_FRAME};
    use std::collections::VecDeque;
    use std::vec;

    const MIB: u64 = 1024 * 1024;
    const GIB_PAGES: u64 = 1024 * MIB / PAGE_BYTES;

    struct Fake {
        replies: VecDeque<(u16, Result<RustosJobArgs, i32>)>,
        calls: Vec<RustosJobArgs>,
        closed: Vec<u64>,
        machine: Machine,
        observed: usize,
        enforce: Result<(), i32>,
        enforced: Vec<(u64, BudgetPlan)>,
    }
    impl Jobs for Fake {
        fn call(&mut self, args: RustosJobArgs) -> Result<RustosJobArgs, i32> {
            let (op, reply) = self.replies.pop_front().expect("unexpected job call");
            assert_eq!(args.op, op);
            self.calls.push(args);
            reply
        }
        fn close(&mut self, handle: u64) { self.closed.push(handle); }
        fn observe(&mut self) -> Result<Machine, i32> { self.observed += 1; Ok(self.machine) }
        fn enforce_budgets(&mut self, handle: u64, plan: &BudgetPlan) -> Result<(), i32> {
            self.enforced.push((handle, *plan));
            self.enforce
        }
    }
    fn info(id: u64, handle: u64) -> RustosJobArgs {
        RustosJobArgs { job: id, result_handle: handle, state: JOB_STATE_LIVE,
            rights: REALM_RIGHTS, ..RustosJobArgs::default() }
    }
    /// A GET_INFO reply for a Job that has created `usage_threads` threads.
    fn usage(id: u64, usage_threads: u64) -> RustosJobArgs {
        RustosJobArgs { usage_threads, ..info(id, 0) }
    }
    /// The SET_LIMITS reply of a Job whose limits are not narrowed by an ancestor.
    fn in_force(limits: Limits) -> RustosJobArgs {
        RustosJobArgs {
            effective_processes: limits.processes, effective_threads: limits.threads,
            effective_memory_pages: limits.memory_pages, ..info(80, 0)
        }
    }
    fn machine(total: u64, used: u64) -> Machine {
        Machine {
            total_slots: total,
            application_thread_budget: total - total / 4,
            process_thread_ceiling: total / 2,
            root_process_ceiling: total / 4,
            kernel_stack_bytes: 2 * MIB,
            reservable_slots: total - used,
            ram_pages: 4 * GIB_PAGES,
        }
    }
    fn fake(replies: Vec<(u16, Result<RustosJobArgs, i32>)>) -> Fake {
        Fake {
            replies: replies.into(), calls: Vec::new(), closed: Vec::new(),
            machine: machine(256, 56), observed: 0, enforce: Ok(()), enforced: Vec::new(),
        }
    }
    fn installed() -> ApplicationJobs {
        ApplicationJobs { realm: Some(Authority { handle: 7, id: 70 }), children: Vec::new() }
    }
    fn child(handle: u64, id: u64, processes: u64, threads: u64) -> Child {
        Child {
            authority: Authority { handle, id }, leader: None, retiring: false,
            granted_processes: processes, granted_threads: threads,
        }
    }
    const NONE: ApplicationRequest = ApplicationRequest::NONE;
    /// What an application that declares nothing is granted at T = 256, 4 GiB.
    const DEFAULT_LIMITS: Limits = Limits { processes: 12, threads: 48, memory_pages: 12 * 2 * GIB_PAGES };

    #[test]
    fn limit_proof_only_tightens_the_explicit_hog_and_is_off_by_default() {
        let hog = b"/apps/oomhog/oomhog.elf";
        assert_eq!(memory_probe_pages(false, hog), None);
        assert_eq!(memory_probe_pages(true, hog), Some(LOADER_APPLICATION_LIMIT_PROBE_PAGES));
        for path in [b"/services/uiserver/uiserver.elf".as_slice(), b"/apps/oomhog/oomhog.elf.extra", b""] {
            assert_eq!(memory_probe_pages(true, path), None);
        }
        // The proof lowers the page limit only; it never raises it, and the
        // thread and process grants are untouched.
        let mut state = installed();
        let mut jobs = fake(vec![
            (JOB_OP_CREATE_CHILD, Ok(info(80, 8))),
            (JOB_OP_SET_LIMITS, Ok(in_force(Limits { memory_pages: LOADER_APPLICATION_LIMIT_PROBE_PAGES, ..DEFAULT_LIMITS }))),
        ]);
        let prepared = state.prepare(&NONE, Some(LOADER_APPLICATION_LIMIT_PROBE_PAGES), &mut jobs).unwrap();
        assert_eq!(prepared.limits.memory_pages, LOADER_APPLICATION_LIMIT_PROBE_PAGES);
        assert_eq!((prepared.limits.processes, prepared.limits.threads), (12, 48));
        let mut jobs = fake(vec![
            (JOB_OP_GET_INFO, Ok(usage(80, 0))),
            (JOB_OP_CREATE_CHILD, Ok(info(81, 9))),
            (JOB_OP_SET_LIMITS, Ok(in_force(Limits { processes: 2, threads: 2, memory_pages: 2 * 4 }))),
        ]);
        let tiny = ApplicationRequest { processes: 2, threads: 2, memory_pages_per_process: 4 };
        let prepared = state.prepare(&tiny, Some(LOADER_APPLICATION_LIMIT_PROBE_PAGES), &mut jobs).unwrap();
        assert_eq!(prepared.limits.memory_pages, 8, "a probe never raises the limit");
    }

    #[test]
    fn an_undeclared_application_is_granted_a_default_derived_from_the_observation() {
        let mut state = installed();
        let mut jobs = fake(vec![
            (JOB_OP_CREATE_CHILD, Ok(info(80, 8))),
            (JOB_OP_SET_LIMITS, Ok(in_force(DEFAULT_LIMITS))),
        ]);
        let prepared = state.prepare(&NONE, None, &mut jobs).unwrap();
        assert_eq!(prepared.limits, DEFAULT_LIMITS);
        assert_eq!(jobs.observed, 1, "capacity is observed once per launch");
        // A wider table moves the default; nothing is a literal.
        let mut state = installed();
        let mut jobs = fake(vec![
            (JOB_OP_CREATE_CHILD, Ok(info(80, 8))),
            (JOB_OP_SET_LIMITS, Ok(in_force(Limits { processes: 48, threads: 192, memory_pages: 48 * 2 * GIB_PAGES }))),
        ]);
        jobs.machine = Machine { ram_pages: 4 * GIB_PAGES, ..machine(1024, 56) };
        let prepared = state.prepare(&NONE, None, &mut jobs).unwrap();
        assert_eq!((prepared.limits.processes, prepared.limits.threads), (48, 192));
    }

    #[test]
    fn every_child_is_retained_and_all_three_limits_precede_placement() {
        let mut state = installed();
        let request = ApplicationRequest { processes: 16, threads: 128, memory_pages_per_process: 32_768 };
        let limits = Limits { processes: 16, threads: 128, memory_pages: 16 * 32_768 };
        let mut jobs = fake(vec![
            (JOB_OP_CREATE_CHILD, Ok(info(80, 8))),
            (JOB_OP_SET_LIMITS, Ok(in_force(limits))),
            (JOB_OP_PROCESS_JOB, Ok(info(80, 0))),
        ]);
        let prepared = state.prepare(&request, None, &mut jobs).unwrap();
        assert_eq!(prepared.handle, 8);
        assert_eq!(prepared.limits, limits);
        assert_eq!(state.children.len(), 1);
        assert_eq!((state.children[0].granted_processes, state.children[0].granted_threads), (16, 128));
        let set = &jobs.calls[1];
        assert_eq!(set.job_handle, 8);
        assert_eq!(set.limit_mask, JOB_LIMIT_PROCESSES | JOB_LIMIT_THREADS | JOB_LIMIT_MEMORY_PAGES);
        assert_eq!((set.limit_processes, set.limit_threads, set.limit_memory_pages), (16, 128, 16 * 32_768));
        // Budgets follow the grant and the observed kernel stack.
        assert_eq!(jobs.enforced.len(), 1);
        assert_eq!(jobs.enforced[0], (8, budget_plan(16, 128, 16 * 32_768, &machine(256, 56))));
        assert_eq!(state.published(8, 41, &mut jobs), Ok(80));
        assert_eq!(jobs.calls[2].pid, 41);
        assert_eq!(state.children[0].leader, Some(41));
        assert!(jobs.closed.is_empty());
    }

    #[test]
    fn a_request_that_cannot_be_granted_in_full_creates_no_job_and_names_the_limit() {
        let mut state = installed();
        let mut jobs = fake(vec![]);
        // Above the application thread budget (192): structural, EINVAL.
        let failure = state
            .prepare(&ApplicationRequest { threads: 193, ..NONE }, None, &mut jobs)
            .unwrap_err();
        let refusal = failure.refusal.expect("a refusal, not a syscall failure");
        assert_eq!((failure.errno, refusal.limit, refusal.requested, refusal.available),
            (22, Limit::ApplicationThreadBudget, 193, 192));
        assert!(state.children.is_empty(), "no Job was created for a refused request");
        assert!(jobs.calls.is_empty() && jobs.enforced.is_empty());
    }

    #[test]
    fn the_sum_of_live_grants_is_bounded_by_the_application_budget() {
        let mut state = installed();
        // A first Job takes 150 of the 192 budget threads and has created 30.
        state.children.push(child(8, 80, 10, 150));
        let mut jobs = fake(vec![(JOB_OP_GET_INFO, Ok(usage(80, 30)))]);
        let failure = state
            .prepare(&ApplicationRequest { threads: 43, ..NONE }, None, &mut jobs)
            .unwrap_err();
        let refusal = failure.refusal.unwrap();
        assert_eq!((failure.errno, refusal.limit, refusal.requested, refusal.available),
            (11, Limit::ApplicationThreadBudget, 43, 42));
        assert_eq!(state.children.len(), 1);
        // Exactly what is left is grantable; the sum then equals the budget.
        let mut jobs = fake(vec![
            (JOB_OP_GET_INFO, Ok(usage(80, 30))),
            (JOB_OP_CREATE_CHILD, Ok(info(81, 9))),
            (JOB_OP_SET_LIMITS, Ok(in_force(Limits { processes: 12, threads: 42, memory_pages: 12 * 2 * GIB_PAGES }))),
        ]);
        state.prepare(&ApplicationRequest { threads: 42, ..NONE }, None, &mut jobs).unwrap();
        let held: u64 = state.children.iter().map(|child| child.granted_threads).sum();
        assert_eq!(held, 192);
    }

    #[test]
    fn slots_promised_to_live_jobs_are_not_offered_again() {
        // Only 60 slots are free now (boot services hold more than the headroom)
        // and a live Job was granted 150 but has created 100: 50 are promised.
        let mut state = installed();
        state.children.push(child(8, 80, 10, 150));
        let mut jobs = fake(vec![(JOB_OP_GET_INFO, Ok(usage(80, 100)))]);
        jobs.machine = machine(256, 196);
        // 192 - 150 = 42 within the budget, but only 60 - 50 = 10 free slots.
        let failure = state
            .prepare(&ApplicationRequest { threads: 20, ..NONE }, None, &mut jobs)
            .unwrap_err();
        let refusal = failure.refusal.unwrap();
        assert_eq!((failure.errno, refusal.limit, refusal.available), (11, Limit::ReservableTaskSlots, 10));
    }

    #[test]
    fn a_job_the_kernel_no_longer_knows_releases_its_grant_but_an_unreadable_one_does_not() {
        let mut state = installed();
        state.children.push(child(8, 80, 10, 150));
        let mut jobs = fake(vec![
            (JOB_OP_GET_INFO, Err(3)),
            (JOB_OP_CREATE_CHILD, Ok(info(81, 9))),
            (JOB_OP_SET_LIMITS, Ok(in_force(Limits { processes: 12, threads: 192, memory_pages: 12 * 2 * GIB_PAGES }))),
        ]);
        assert!(state.prepare(&ApplicationRequest { threads: 192, ..NONE }, None, &mut jobs).is_ok());
        let mut state = installed();
        state.children.push(child(8, 80, 10, 150));
        let mut jobs = fake(vec![(JOB_OP_GET_INFO, Err(5))]);
        let failure = state.prepare(&ApplicationRequest { threads: 100, ..NONE }, None, &mut jobs).unwrap_err();
        assert_eq!(failure.refusal.unwrap().available, 42, "unreadable counts in full");
    }

    #[test]
    fn a_retiring_job_keeps_counting_until_it_is_reclaimed() {
        let mut state = installed();
        state.children.push(Child { retiring: true, ..child(8, 80, 10, 150) });
        let mut jobs = fake(vec![(JOB_OP_GET_INFO, Ok(usage(80, 150)))]);
        let failure = state.prepare(&NONE, None, &mut jobs).unwrap_err();
        let refusal = failure.refusal.unwrap();
        // The default (48) does not fit beside the retiring Job's 150 of 192.
        assert_eq!((failure.errno, refusal.limit, refusal.requested, refusal.available),
            (11, Limit::ApplicationThreadBudget, 48, 42));
    }

    #[test]
    fn a_parent_job_that_narrows_the_grant_refuses_the_launch_instead_of_clamping() {
        let mut state = installed();
        let clamped = RustosJobArgs { effective_threads: 20, ..in_force(DEFAULT_LIMITS) };
        let mut jobs = fake(vec![
            (JOB_OP_CREATE_CHILD, Ok(info(80, 8))),
            (JOB_OP_SET_LIMITS, Ok(clamped)),
            (JOB_OP_KILL, Ok(info(80, 0))),
        ]);
        let failure = state.prepare(&NONE, None, &mut jobs).unwrap_err();
        let refusal = failure.refusal.unwrap();
        assert_eq!((refusal.limit, refusal.requested, refusal.available), (Limit::AncestorJob, 48, 20));
        assert!(state.children.is_empty(), "the Job was retired");
        assert_eq!(jobs.closed, vec![8]);
        assert!(jobs.enforced.is_empty());
    }

    #[test]
    fn a_refused_byte_budget_retires_the_job() {
        let mut state = installed();
        let mut jobs = fake(vec![
            (JOB_OP_CREATE_CHILD, Ok(info(80, 8))),
            (JOB_OP_SET_LIMITS, Ok(in_force(DEFAULT_LIMITS))),
            (JOB_OP_KILL, Ok(info(80, 0))),
        ]);
        jobs.enforce = Err(12);
        assert_eq!(state.prepare(&NONE, None, &mut jobs), Err(Failure { errno: 12, refusal: None }));
        assert!(state.children.is_empty());
        assert_eq!(jobs.closed, vec![8]);
    }

    #[test]
    fn page_table_budget_follows_resident_pages_and_never_a_fixed_table() {
        let default_frames = 4 * GIB_PAGES / PAGES_PER_TABLE_FRAME;
        let plan = budget_plan(12, 48, 12 * 2 * GIB_PAGES, &machine(256, 0));
        let (_, bytes) = plan.classes.iter().copied().find(|(c, _)| *c == JOB_KMEM_CLASS_PAGE_TABLE).unwrap();
        assert_eq!(bytes, default_frames * 4096 * crate::application_grant::PAGE_TABLE_SPARSITY_FACTOR);
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
        assert_eq!(state.prepare(&NONE, None, &mut jobs), Err(Failure { errno: 11, refusal: None }));
        assert_eq!(jobs.observed, 0, "no realm: nothing is observed or granted");
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
            < spawn.find("applications.prepare(").unwrap());
        assert!(spawn.find("applications.prepare(").unwrap()
            < spawn.find("handle_placed_request(request, Some(handle), snapshot)").unwrap());
        assert!(spawn.find("applications.published(handle").unwrap()
            < spawn.find("handled.application_job = Some(handle)").unwrap());
        // The request reaches the grant from the wire, not from a literal.
        assert!(spawn.contains("&request.application_request"));
        let main = include_str!("main.rs");
        let serve = main.split_once("fn serve(").unwrap().1;
        assert!(serve.contains("if handled.application_job.is_none()"));
        let failure = serve.split_once("if reply < 0 {").unwrap().1;
        assert!(failure.contains("applications.abort(handle"));
        let request = main.split_once("fn handle_request(").unwrap().1;
        assert!(request.find("authorize_loader_operation(request.op, sender_pid)").unwrap()
            < request.find("image_work.begin(request").unwrap());
        let staging = include_str!("image_staging.rs");
        assert!(staging.find("pending.ticket.poll()").unwrap()
            < staging.find("application_launch::spawn(&pending.request").unwrap());
        assert!(staging.find("authorize_loader_operation(pending.request.op").unwrap()
            < staging.find("application_launch::spawn(&pending.request").unwrap());
        let placed = main.split_once("fn handle_placed_request(").unwrap().1;
        let placed = placed.split_once("fn authorize_loader_operation(").unwrap().0;
        assert!(!placed.contains("open_immutable_file_snapshot("));
        assert!(placed.contains("snapshot.take_fd()"));
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
        assert_eq!(state.prepare(&NONE, None, &mut jobs), Err(Failure { errno: 22, refusal: None }));
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
            (JOB_OP_SET_LIMITS, Ok(in_force(DEFAULT_LIMITS))),
            (JOB_OP_PROCESS_JOB, Ok(info(90, 0))),
            (JOB_OP_KILL, Err(3)),
        ]);
        let handle = state.prepare(&NONE, None, &mut jobs).unwrap().handle;
        assert_eq!(state.published(handle, 41, &mut jobs), Err(1));
        assert_eq!(state.children[0].leader, None);
        state.abort(handle, &mut jobs);
        assert!(state.children.is_empty());
        assert_eq!(jobs.closed, vec![8]);
    }

    #[test]
    fn reaped_leader_retires_descendants_but_transient_inspection_error_does_not() {
        let mut state = installed();
        state.children.push(Child { leader: Some(41), ..child(8, 80, 12, 48) });
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
